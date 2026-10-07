//! external 面掩码缓存。
//!
//! ExternalMaskCache 把「邻居 wrapped 树 + 面 + 采样深度」映射到该面 n 个
//! u64 的外部占用掩码（n = 1 << max_depth，当前恒为 32，即 256 B）。
//!
//! **内容寻址**：key 用 u64::from(BlockId)。interner 做 hash-cons，内容相同的
//! 子树得到相同 BlockId，内容变则 id 变，因此**不需要显式失效**；旧条目自然
//! 成为垃圾，由容量上限统一清掉。
//!
//! **不持有 interner 引用**：值只有纯位数据（Box<[u64]>），没有 VoxTree，
//! 所以永远不需要 dec_ref_recursive，不存在 的引用泄漏问题。
//!
//! **确定性（L3）**：masks 用 BTreeMap；遍历顺序确定。
//!
//! **容量上限（v1 不做 LRU）**：条目数达到 EXTERNAL_MASK_CACHE_CAPACITY 时，
//! 在插入新条目之前直接 clear() 整个缓存。4096 条 × 256 B ≈ 1 MiB。
//! 第一版不做 LRU。

use std::collections::BTreeMap;

use bevy::prelude::*;
use voxel::mesh::{ExternalPlaneKind, OccupancyDataBuilder};
use voxel::store::{MaxDepth, VoxTree};

use super::interner::VoxelInterner;

/// external 掩码缓存条目上限（v1 不做 LRU，满了整体 clear()）。
pub const EXTERNAL_MASK_CACHE_CAPACITY: usize = 4096;

/// external 面掩码缓存。key = (邻居 wrapped root BlockId, ExternalPlane, max_depth)。
///
/// 值 = n 个 u64（n = 1 << max_depth，当前恒为 32，即 256 B），是纯位数据，
/// 不持有 interner 引用，因此 clear() 不需要释放任何节点。
#[derive(Resource, Default)]
pub struct ExternalMaskCache {
    masks: BTreeMap<(u64, u8, u8), Box<[u64]>>,
}

impl ExternalMaskCache {
    /// 当前缓存的掩码条目数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.masks.len()
    }

    /// 缓存是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.masks.is_empty()
    }

    /// 清空全部条目。值不持有 interner 引用，因此无需释放任何节点。
    pub fn clear(&mut self) {
        self.masks.clear();
    }

    /// 取 neighbour 在 plane 上、采样深度 max_depth 的 external 掩码。
    ///
    /// 命中直接返回缓存切片；未命中时用临时 OccupancyDataBuilder +
    /// voxel::mesh::generate_external_occupancy_mask 现算（offset =
    /// UVec2::ZERO），复制该面的 n 个 u64 后插入。
    ///
    /// 空 root（BlockId::EMPTY）不缓存，返回空切片；调用方应把空切片当作
    /// 「该面全零、无外部遮挡」处理，绝不 panic。
    ///
    /// # Panics
    ///
    /// 与 OccupancyDataBuilder::new 一致：1 << max_depth 超出 64 时 panic。
    /// 当前调用点 max_depth 恒为 5（n = 32）。
    pub fn get_or_generate(
        &mut self,
        interner: &VoxelInterner,
        neighbour: &VoxTree<u8>,
        plane: ExternalPlaneKind,
        max_depth: MaxDepth,
    ) -> &[u64] {
        self.get_or_generate_raw(interner.inner(), neighbour, plane, max_depth)
    }

    /// Like get_or_generate but takes the raw store interner, for engine entry
    /// points that already hold a &VoxInterner<u8> (the T6 incremental mesher).
    /// Same key, same value.
    pub fn get_or_generate_raw(
        &mut self,
        interner: &voxel::store::VoxInterner<u8>,
        neighbour: &VoxTree<u8>,
        plane: ExternalPlaneKind,
        max_depth: MaxDepth,
    ) -> &[u64] {
        let root = neighbour.get_root_id();
        // 空邻居没有任何外部占用；跳过缓存并优雅返回空切片。
        if root.is_empty() {
            return &[];
        }

        let depth = max_depth.max();
        let key = (u64::from(root), plane as u8, depth);

        // v1 无 LRU：满了先把整个缓存清掉，再插入新条目。命中时不清理。
        if self.masks.len() >= EXTERNAL_MASK_CACHE_CAPACITY && !self.masks.contains_key(&key) {
            self.masks.clear();
        }

        let stored = self.masks.entry(key).or_insert_with(|| {
            let n = 1usize << depth;
            let mut builder = OccupancyDataBuilder::new(n as u32);
            voxel::mesh::generate_external_occupancy_mask(
                interner,
                &mut builder,
                &root,
                max_depth,
                plane,
                bevy::math::UVec2::ZERO,
            );
            let start = plane as usize * n;
            builder.external[start..start + n]
                .to_vec()
                .into_boxed_slice()
        });
        stored
    }
}

#[cfg(test)]
mod tests {
    use super::{ExternalMaskCache, EXTERNAL_MASK_CACHE_CAPACITY};
    use crate::voxel::VoxelInterner;
    use bevy::math::IVec3;
    use voxel::mesh::{generate_external_occupancy_mask, ExternalPlaneKind, OccupancyDataBuilder};
    use voxel::store::{MaxDepth, VoxOpsWrite, VoxTree, CHUNK_DEPTH};

    /// Deterministic, non-trivial neighbour tree.
    fn build_neighbour(interner: &mut VoxelInterner) -> VoxTree<u8> {
        let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
        for z in 0..32i32 {
            for y in 0..32i32 {
                for x in 0..32i32 {
                    let v = if (x * 3 + y * 5 + z * 7).rem_euclid(11) < 4 {
                        2u8
                    } else {
                        0
                    };
                    if v != 0 {
                        let _ = tree.set(interner.inner_mut(), IVec3::new(x, y, z), v);
                    }
                }
            }
        }
        tree
    }

    /// Direct call to the public extractor, i.e. the uncached reference.
    fn reference_mask(
        interner: &VoxelInterner,
        tree: &VoxTree<u8>,
        plane: ExternalPlaneKind,
        depth: MaxDepth,
    ) -> Vec<u64> {
        let n = 1usize << depth.max();
        let mut builder = OccupancyDataBuilder::new(n as u32);
        generate_external_occupancy_mask(
            interner.inner(),
            &mut builder,
            &tree.get_root_id(),
            depth,
            plane,
            bevy::math::UVec2::ZERO,
        );
        let start = plane as usize * n;
        builder.external[start..start + n].to_vec()
    }

    #[test]
    fn generated_mask_matches_direct_call_and_hits_cache() {
        let mut interner = VoxelInterner::new(4 * 1024 * 1024);
        let tree = build_neighbour(&mut interner);
        let depth = MaxDepth::new(CHUNK_DEPTH);
        let mut cache = ExternalMaskCache::default();

        for (i, plane) in ExternalPlaneKind::ALL.iter().copied().enumerate() {
            let expected = reference_mask(&interner, &tree, plane, depth);
            let first = cache.get_or_generate(&interner, &tree, plane, depth);
            assert_eq!(
                first,
                expected.as_slice(),
                "cached plane {plane:?} must equal the direct call"
            );
            let first_ptr = first.as_ptr();

            // A second call must reuse the exact stored allocation (cache hit)
            // and return identical bits.
            let second = cache.get_or_generate(&interner, &tree, plane, depth);
            assert_eq!(second, expected.as_slice());
            assert_eq!(second.as_ptr(), first_ptr, "second call must hit the cache");
            assert_eq!(cache.len(), i + 1, "one entry per plane");
        }
    }

    #[test]
    fn clear_empties_the_cache() {
        let mut interner = VoxelInterner::new(4 * 1024 * 1024);
        let tree = build_neighbour(&mut interner);
        let depth = MaxDepth::new(CHUNK_DEPTH);
        let mut cache = ExternalMaskCache::default();

        let _ = cache.get_or_generate(&interner, &tree, ExternalPlaneKind::YZPos, depth);
        assert!(!cache.is_empty());
        assert_eq!(cache.len(), 1);

        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn capacity_cap_clears_before_insert() {
        let mut interner = VoxelInterner::new(4 * 1024 * 1024);
        let tree = build_neighbour(&mut interner);
        let depth = MaxDepth::new(CHUNK_DEPTH);
        let plane = ExternalPlaneKind::YZNeg;
        let mut cache = ExternalMaskCache::default();

        // Fill to the cap with synthetic keys. Depth 7 can never collide with
        // the real key, whose depth is 5.
        for i in 0..EXTERNAL_MASK_CACHE_CAPACITY {
            cache
                .masks
                .insert((i as u64, 0, 7), vec![0u64].into_boxed_slice());
        }
        assert_eq!(cache.len(), EXTERNAL_MASK_CACHE_CAPACITY);

        let expected = reference_mask(&interner, &tree, plane, depth);
        let got = cache.get_or_generate(&interner, &tree, plane, depth);
        assert_eq!(got, expected.as_slice());

        // Full cache was cleared, so exactly the freshly computed entry remains.
        assert_eq!(cache.len(), 1, "cap must clear the cache before inserting");
        let key = (u64::from(tree.get_root_id()), plane as u8, depth.max());
        assert!(cache.masks.contains_key(&key));
    }

    #[test]
    fn empty_root_is_skipped_without_panicking() {
        let interner = VoxelInterner::new(1024 * 1024);
        let empty = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
        assert!(empty.get_root_id().is_empty());

        let mut cache = ExternalMaskCache::default();
        let got = cache.get_or_generate(
            &interner,
            &empty,
            ExternalPlaneKind::XYNeg,
            MaxDepth::new(CHUNK_DEPTH),
        );
        assert!(got.is_empty(), "empty root yields an empty slice");
        assert!(cache.is_empty(), "empty root must not be cached");
    }
}
