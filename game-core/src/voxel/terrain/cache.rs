//! terrain 的块缓存失效与释放入口。
//!
//! 编辑、组重建、卸载共用同一套「四份块缓存」失效口径：LOD0 的
//! [WrappedBlockCache]、邻块缓存 [TerrainNeighborBlocks]、按 LOD 生成的
//! [TerrainGeneratedBlocks] 的 own / neighbors。[VoxTree] 没有 Drop，缓存里残留的
//! root 引用会把节点钉在 interner 里，所以失效与释放必须成对调用。

use bevy::prelude::*;
use game_engine::presentation::voxel::WrappedBlockCache;
use game_engine::voxel::{
    dec_ref_recursive, release_body, ChunkKey, VoxVolume, VoxelDirtySet, VoxelInterner,
};

use super::generated::GeneratedBlockCache;
use super::resources::{TerrainGeneratedBlocks, TerrainNeighborBlocks};

/// 把 `key` 在 terrain 四份块缓存里覆盖的条目全部失效（所有 LOD）。
///
/// 四份实例缺一不可：`WrappedBlockCache`、`TerrainNeighborBlocks`、
/// `TerrainGeneratedBlocks::own`、`TerrainGeneratedBlocks::neighbors`。
/// 编辑与组重建复用本函数，避免只清 wrapper 的旧口径。
///
/// 注意：粗 LOD 的 `GeneratedBlockCache` 重生成仍只由 `(origin, lod)` 决定、
/// 不读 `VoxVolume`（阶段 3 / doc 12 §9 的债），所以“失效”只保证缓存不陈旧，
/// 不代表粗 LOD 内容会跟随编辑。
pub(crate) fn invalidate_chunk_caches(
    interner: &mut VoxelInterner,
    key: ChunkKey,
    wrapped: &mut WrappedBlockCache,
    generated: &mut TerrainGeneratedBlocks,
    neighbors: &mut TerrainNeighborBlocks,
) {
    wrapped.invalidate_covered(interner.inner_mut(), key);
    neighbors.0.invalidate_covered(interner.inner_mut(), key);
    generated.own.invalidate_covered(interner.inner_mut(), key);
    generated
        .neighbors
        .invalidate_covered(interner.inner_mut(), key);
}

/// 编辑后失效本块在**四份块缓存**里的所有 LOD 条目。
///
/// 粗 LOD 目前不读编辑（见 [`invalidate_chunk_caches`] 的债务说明），
/// 但缓存必须清：否则后续重新提取会拿到编辑前的旧树。
pub(crate) fn invalidate_stale_terrain_caches(
    mut interner: ResMut<VoxelInterner>,
    dirty: Res<VoxelDirtySet>,
    mut wrapped: ResMut<WrappedBlockCache>,
    mut generated: ResMut<TerrainGeneratedBlocks>,
    mut neighbors: ResMut<TerrainNeighborBlocks>,
) {
    for edit in dirty.peek_edits() {
        invalidate_chunk_caches(
            &mut interner,
            edit.key,
            &mut wrapped,
            &mut generated,
            &mut neighbors,
        );
    }
}

/// 唯一卸载入口：把一个基础子块从 [`VoxVolume`] 与给定缓存里彻底移除。
///
/// `VoxTree` 没有 `Drop`，所以顺序固定、不能交换：
///
/// 1. **先失效本块在所有 LOD 的缓存**：[`WrappedBlockCache::invalidate_covered`]
///    与 [`GeneratedBlockCache::invalidate_covered`] 都对
///    `lod == 0..=MAX_LOD` 用 `lod_block_origin(key, lod)` 反推缓存键，删掉
///    「覆盖该基础子块」的条目（粗 LOD 条目可能同时覆盖别的子块，这里按保守
///    口径整条失效）并 `dec_ref_recursive` 其 root；
/// 2. [`VoxVolume::take_chunk_raw`] 把树取出来（**不要**裸调它并让返回值析构，
///    那是静默泄漏；它叫 `raw` 就是因为释放引用的责任在调用方）；
/// 3. 对取回树的 root 显式 `dec_ref_recursive`。
///
/// 返回 `false` 表示 `VoxVolume` 里本来就没有这个块（缓存仍会被失效），
/// 因此对同一块重复调用是安全的、幂等的。
///
/// **邻块缓存要一起清**：[`TerrainNeighborBlocks`] 与
/// [`TerrainGeneratedBlocks::neighbors`] 是独立的缓存实例，`extract_block_raw`
/// 会把 26 邻（含本块）预热进去。若它们也持有该块/其覆盖块，必须对每一份缓存
/// 各调用一次本函数（只有真正从 `VoxVolume` 取回树的那次返回 `true`）；
/// 否则残留的 root 引用会把节点钉在 interner 里，`alive_nodes` 回不到基线。
/// 按《体素流式_实现文档》第 8 节，`Render → Data` 降档时就应先清掉这些
/// 表现层缓存，`Data → None` 再走本入口。
pub fn unload_chunk(
    interner: &mut VoxelInterner,
    volume: &mut VoxVolume,
    wrapped: &mut WrappedBlockCache,
    generated: &mut GeneratedBlockCache,
    key: ChunkKey,
) -> bool {
    wrapped.invalidate_covered(interner.inner_mut(), key);
    generated.invalidate_covered(interner.inner_mut(), key);

    let Some(tree) = volume.take_chunk_raw(&key) else {
        return false;
    };
    let root = tree.get_root_id();
    dec_ref_recursive(interner.inner_mut(), &root);
    true
}

/// 地形整卷释放的唯一入口：先清**四份块缓存**，再 `release_body`。
///
/// 这是 terrain 场景对 `game_engine::presentation::voxel::release_body_with_cache`
/// 的替代品：后者只清 `WrappedBlockCache`，`max_lod > 0` 时
/// [`TerrainGeneratedBlocks`]（own + neighbors）与 [`TerrainNeighborBlocks`]
/// 里的 root 引用会把粗 LOD 节点钉在 interner 里（不 panic，只涨内存）。
///
/// 调用方把 terrain 的四份缓存一起传进来，不要自己拼清缓存顺序。
/// 返回释放的非空 chunk root 数量。
pub fn release_terrain_body(
    interner: &mut VoxelInterner,
    volume: &mut VoxVolume,
    wrapped: &mut WrappedBlockCache,
    generated: &mut TerrainGeneratedBlocks,
    neighbors: &mut TerrainNeighborBlocks,
) -> usize {
    wrapped.clear(interner.inner_mut());
    neighbors.0.clear(interner.inner_mut());
    generated.own.clear(interner.inner_mut());
    generated.neighbors.clear(interner.inner_mut());
    release_body(interner.inner_mut(), volume)
}
