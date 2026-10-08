//! game-engine 体素世界层：纯世界数据（VoxVolume / VoxelInterner / 编辑缓冲）。
//!
//! 本模块只做机制与 Bevy 适配：不产出几何、不含游戏语义、不定义游戏数值。
//!
//! - VoxVolume / Attachment / 整数 ChunkKey 寻址；
//! - VoxelInterner（Bevy Resource，包裹 voxel::store::VoxInterner<u8>）；
//! - VoxelEdit / VoxelChangeBuffer 编辑缓冲；
//! - VoxelBox / DirtyChunk / VoxelDirtySet 脏集，及 apply_voxel_changes 应用系统；
//! - LazyChunks 懒生成缓存（生成函数由 game-core 提供）；
//! - VoxelAabb、content_hash、release_body；
//! - VoxelPlugin（由 game-core 注册，装世界层资源、指标与变更系统）。
//!
//! 表现层的网格机制（重建计划 / 打包 / raw halo / 各种缓存）在
//! `crate::presentation::voxel`，世界层不反向依赖它。
//!
//! 边界铁律：crates/voxel 只用整数体素坐标，不知道 voxel_size；
//! 世界换算只在 volume 的 body_voxel / world_meters 里发生，且仅在表现出口把
//! FixedPoint 转 f32。确定性：所有会产字节或决定顺序的容器一律
//! BTreeMap / 排序 Vec，绝不用 HashMap 迭代序。

pub use ::voxel::mesh::{extract_block, MeshBlock, RectBatch, RectInstance};
pub use ::voxel::store::*;

mod aabb;
mod change;
mod dirty;
mod hash;
mod interner;
mod key;
mod lazy;
mod plugin;
mod volume;

pub use aabb::{chunk_aabb, VoxelAabb};
pub use change::{apply_voxel_changes, VoxelChangeBuffer, VoxelEdit, VoxelSet};
pub use dirty::{DirtyChunk, VoxelBox, VoxelDirtySet};
pub use hash::{
    chunk_content_hash, content_hash, count_body_nodes, dec_ref_recursive, release_body,
};
pub use interner::{
    update_voxel_memory_metric, VoxelInterner, VoxelMemoryMetric, DEFAULT_INTERNER_BUDGET_BYTES,
};
pub use key::{
    chunk_key, chunk_voxels_per_axis, floor_div, lod_block_origin, lod_block_span, neighbors6,
};
pub use lazy::{ChunkGenerator, LazyChunks};
pub use plugin::VoxelPlugin;
pub use volume::{body_voxel, voxel_scale, world_meters, world_meters_f32, Attachment, VoxVolume};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::FixedPoint;
    use ::voxel::store::{MaxDepth, VoxOpsBulkWrite, VoxOpsWrite, VoxTree, CHUNK_DEPTH};
    use std::collections::BTreeMap;

    fn test_interner() -> VoxelInterner {
        VoxelInterner::new(4 * 1024 * 1024)
    }

    fn base_value(key: ChunkKey) -> u8 {
        (((key.x + key.y + key.z).rem_euclid(6)) + 1) as u8
    }

    fn build_tree(key: ChunkKey, interner: &mut VoxelInterner) -> VoxTree<u8> {
        let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
        let base = base_value(key);
        tree.fill(interner.inner_mut(), base);
        let _ = tree.set(
            interner.inner_mut(),
            bevy::math::IVec3::new(0, 0, 0),
            base + 1,
        );
        tree
    }

    fn gen_tree(key: ChunkKey, interner: &mut VoxInterner<u8>) -> VoxTree<u8> {
        let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
        let base = base_value(key);
        tree.fill(interner, base);
        let _ = tree.set(interner, bevy::math::IVec3::new(0, 0, 0), base + 1);
        tree
    }

    #[test]
    fn content_hash_is_structural_and_order_independent() {
        let keys = [chunk_key(0, 0, 0), chunk_key(1, 0, 0), chunk_key(0, 2, 1)];

        let mut ia = test_interner();
        let mut va = VoxVolume::new(FixedPoint::from_num(1));
        for &k in keys.iter() {
            let t = build_tree(k, &mut ia);
            va.insert_chunk(k, t);
        }

        // 同一逻辑结构、不同 interner 实例、不同构建顺序 -> 同 hash。
        let mut ib = test_interner();
        let mut vb = VoxVolume::new(FixedPoint::from_num(7));
        for &k in keys.iter().rev() {
            let t = build_tree(k, &mut ib);
            vb.insert_chunk(k, t);
        }
        assert_eq!(content_hash(ia.inner(), &va), content_hash(ib.inner(), &vb));

        // 改一个 chunk 的树 -> hash 变。
        let mut ic = test_interner();
        let mut vc = VoxVolume::new(FixedPoint::from_num(1));
        for &k in keys.iter() {
            let t = build_tree(k, &mut ic);
            vc.insert_chunk(k, t);
        }
        let k0 = keys[0];
        let mut changed = build_tree(k0, &mut ic);
        let _ = changed.set(ic.inner_mut(), bevy::math::IVec3::new(1, 1, 1), 42);
        vc.insert_chunk(k0, changed);
        assert_ne!(content_hash(ia.inner(), &va), content_hash(ic.inner(), &vc));
    }

    #[test]
    fn lazy_generation_equals_eager() {
        let keys = [
            chunk_key(0, 0, 0),
            chunk_key(1, 0, 0),
            chunk_key(0, 2, 1),
            chunk_key(-1, 0, 3),
        ];

        let mut eager_interner = test_interner();
        let mut eager: BTreeMap<ChunkKey, VoxTree<u8>> = BTreeMap::new();
        for &k in keys.iter() {
            let t = build_tree(k, &mut eager_interner);
            eager.insert(k, t);
        }

        let mut lazy_interner = test_interner();
        let mut lazy = LazyChunks::new();
        let mut generator = gen_tree;
        for &k in keys.iter().rev() {
            lazy.get_or_generate(k, lazy_interner.inner_mut(), &mut generator);
        }
        // 重复请求命中缓存，不应改变结果。
        lazy.get_or_generate(keys[0], lazy_interner.inner_mut(), &mut generator);
        assert_eq!(lazy.len(), keys.len());

        for &k in keys.iter() {
            let e = chunk_content_hash(eager_interner.inner(), &eager[&k]);
            let l = chunk_content_hash(lazy_interner.inner(), lazy.get(&k).unwrap());
            assert_eq!(e, l);
        }
    }

    #[test]
    fn release_body_returns_to_baseline() {
        let keys = [chunk_key(0, 0, 0), chunk_key(1, 0, 0), chunk_key(0, 2, 1)];
        let mut interner = test_interner();
        let mut volume = VoxVolume::new(FixedPoint::from_num(1));

        // 基线：空 body 可达节点为 0。
        assert_eq!(count_body_nodes(interner.inner(), &volume), 0);

        for &k in keys.iter() {
            let t = build_tree(k, &mut interner);
            volume.insert_chunk(k, t);
        }
        let before = count_body_nodes(interner.inner(), &volume);
        assert!(before > 0);
        let expected_hash = content_hash(interner.inner(), &volume);

        let released = release_body(interner.inner_mut(), &mut volume);
        assert_eq!(released, keys.len());
        assert!(volume.chunks.is_empty());

        // 节点计数回到基线。
        assert_eq!(count_body_nodes(interner.inner(), &volume), 0);

        // 释放后仍能以相同结构重建，content_hash 不变。
        for &k in keys.iter() {
            let t = build_tree(k, &mut interner);
            volume.insert_chunk(k, t);
        }
        assert_eq!(content_hash(interner.inner(), &volume), expected_hash);
    }

    #[test]
    fn world_conversion_is_fixed_point() {
        // body_voxel = origin*32 + local。
        let origin = chunk_key(1, 0, -1);
        assert_eq!(body_voxel(origin, [0, 0, 0]), [32, 0, -32]);
        assert_eq!(body_voxel(origin, [5, 2, 3]), [37, 2, -29]);

        // world_meters = body_voxel * voxel_size * 2^lod。
        let vs = FixedPoint::from_num(1) / FixedPoint::from_num(2); // 0.5
        let w = world_meters([2, 4, 8], vs, Lod::new(1));
        assert_eq!(w.x, FixedPoint::from_num(2));
        assert_eq!(w.y, FixedPoint::from_num(4));
        assert_eq!(w.z, FixedPoint::from_num(8));
        let f = world_meters_f32([2, 4, 8], vs, Lod::new(1));
        assert_eq!(f, [2.0, 4.0, 8.0]);
    }
}
