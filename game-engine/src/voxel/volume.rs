//! 通用体素容器与体素<->世界换算。

use std::collections::BTreeMap;

use bevy::prelude::*;
use voxel::store::{ChunkKey, Lod, VoxInterner, VoxTree};

use crate::identity::StableEntityId;
use crate::math::{FixedPoint, Vec3F};

use super::key::chunk_voxels_per_axis;

/// 一岛 / 一船 / 一结构 = 一个 VoxVolume 组件。
///
/// - chunks 用 BTreeMap 而不是 HashMap：遍历 / 序列化 / 哈希全部按 ChunkKey
/// 升序 -> 天然确定（L3）。
/// - voxel_size 是运行期字段，数值由 game-core 提供；引擎不知道 0.45 m。
#[derive(Component)]
pub struct VoxVolume {
    pub chunks: BTreeMap<ChunkKey, VoxTree<u8>>,
    pub voxel_size: FixedPoint,
}

impl VoxVolume {
    pub fn new(voxel_size: FixedPoint) -> Self {
        Self {
            chunks: BTreeMap::new(),
            voxel_size,
        }
    }

    /// with_capacity 风格构造器。
    ///
    /// BTreeMap 是节点式结构、没有容量概念，因此 chunk_capacity 仅作调用点的
    /// 语义提示，不预分配。
    pub fn with_capacity(voxel_size: FixedPoint, chunk_capacity: usize) -> Self {
        let _ = chunk_capacity;
        Self::new(voxel_size)
    }

    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    pub fn contains_chunk(&self, key: &ChunkKey) -> bool {
        self.chunks.contains_key(key)
    }

    pub fn get_chunk(&self, key: &ChunkKey) -> Option<&VoxTree<u8>> {
        self.chunks.get(key)
    }

    pub fn get_chunk_mut(&mut self, key: &ChunkKey) -> Option<&mut VoxTree<u8>> {
        self.chunks.get_mut(key)
    }

    /// 插入一个子块。
    ///
    /// 覆盖已有 key 是危险的：旧树会被返回，调用方一旦丢弃就静默泄漏
    /// （`VoxTree` 没有 `Drop`）。debug 构建下直接拦截；要替换请先
    /// `take_chunk_raw` 并显式 `dec_ref_recursive` 旧 root。
    pub fn insert_chunk(&mut self, key: ChunkKey, tree: VoxTree<u8>) -> Option<VoxTree<u8>> {
        debug_assert!(
            !self.chunks.contains_key(&key),
            "insert_chunk 覆盖已有 key 会丢弃旧树（VoxTree 无 Drop）；覆盖请先 take_chunk_raw 再 dec_ref_recursive"
        );
        self.chunks.insert(key, tree)
    }

    /// 低层取树：**只供唯一的卸载入口 `unload_chunk` 使用**
    /// （`game_core::voxel::terrain::unload_chunk`）。
    ///
    /// `VoxTree` 没有 `Drop`：返回值被直接析构时 root 的引用计数不会归还，
    /// 节点永久驻留 interner（不 panic，只涨内存）。业务代码必须走
    /// `unload_chunk`：它先失效缓存、再取树、最后显式 `dec_ref_recursive`。
    ///
    /// 名字里的 `raw` 就是这个意思：拿到树之后，释放引用的责任在调用方。
    /// 整卷释放用 `release_body`；terrain 四份缓存用 `release_terrain_body`；
    /// 只有一份 `WrappedBlockCache` 时用 `release_body_with_cache`。
    pub fn take_chunk_raw(&mut self, key: &ChunkKey) -> Option<VoxTree<u8>> {
        self.chunks.remove(key)
    }

    /// 清空容器，并把每个 chunk root 的引用归还 interner。
    ///
    /// `VoxTree` 没有 `Drop`：直接 `chunks.clear()` 会把 root 引用永久留在
    /// interner 里（静默泄漏，不 panic）。返回归还的非空 root 数量。
    ///
    /// 等价于 [`crate::voxel::release_body`] 的清空部分；整卷释放优先用
    /// `release_body` / terrain 的 `release_terrain_body`。
    pub fn clear(&mut self, interner: &mut VoxInterner<u8>) -> usize {
        let mut released = 0;
        for (_, tree) in std::mem::take(&mut self.chunks) {
            let root = tree.get_root_id();
            if !root.is_empty() {
                interner.dec_ref_recursive(&root);
                released += 1;
            }
        }
        released
    }

    pub fn iter(&self) -> impl Iterator<Item = (&ChunkKey, &VoxTree<u8>)> {
        self.chunks.iter()
    }
}

impl Default for VoxVolume {
    fn default() -> Self {
        Self::new(FixedPoint::from_bits(0))
    }
}

impl std::fmt::Debug for VoxVolume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoxVolume")
            .field("chunks", &self.chunks.len())
            .field("voxel_size", &self.voxel_size)
            .finish()
    }
}

/// 停靠 / 锚定归属。只依赖 StableEntityId；DockRecord 在 core。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Attachment {
    World,
    Body(StableEntityId),
    Free,
}

/// body_voxel = origin * 32 + local（纯整数）。
#[inline]
pub fn body_voxel(origin: ChunkKey, local: [i32; 3]) -> [i32; 3] {
    let c = chunk_voxels_per_axis();
    [
        origin.x * c + local[0],
        origin.y * c + local[1],
        origin.z * c + local[2],
    ]
}

/// 该 LOD 下单个体素的定点边长 = voxel_size * 2^lod。
#[inline]
pub fn voxel_scale(voxel_size: FixedPoint, lod: Lod) -> FixedPoint {
    voxel_size * FixedPoint::from_num(1i32 << lod.lod())
}

/// world_meters = body_voxel * voxel_size * 2^lod（定点）。
#[inline]
pub fn world_meters(body_voxel: [i32; 3], voxel_size: FixedPoint, lod: Lod) -> Vec3F {
    let s = voxel_scale(voxel_size, lod);
    Vec3F::new(
        FixedPoint::from_num(body_voxel[0]) * s,
        FixedPoint::from_num(body_voxel[1]) * s,
        FixedPoint::from_num(body_voxel[2]) * s,
    )
}

/// 世界换算的表现边界：仅在渲染出口把定点转 f32。
#[inline]
pub fn world_meters_f32(body_voxel: [i32; 3], voxel_size: FixedPoint, lod: Lod) -> [f32; 3] {
    let v = world_meters(body_voxel, voxel_size, lod);
    [
        v.x.to_num::<f32>(),
        v.y.to_num::<f32>(),
        v.z.to_num::<f32>(),
    ]
}
