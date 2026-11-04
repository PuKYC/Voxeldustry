//! 组级 diff：把期望块集合换算成 [GroupKey] 分组，与已产出分组比较并增量重建。
//!
//! 判据同时看成员集合与每成员块的面邻居签名；未变化的组完全不动。

use std::collections::{BTreeMap, BTreeSet};

use bevy::prelude::*;
use game_engine::identity::{StableEntityId, StableEntityIndex, StableIdAllocator};
use game_engine::presentation::RenderTransform;
use game_engine::spatial::Size;
use game_engine::voxel::{ChunkKey, VoxVolume};

use crate::presentation::voxel_mesh::VoxChunkGroupRaw;
use crate::presentation::PresentedTransform;
use crate::voxel::terrain::generation::WorldGenerator;

use super::blocks::{
    block_origin_meters, face_neighbors, group_key, group_origin, lod_mismatch_mask, FaceNeighbor,
    GroupKey, TerrainBlock, TerrainPartition,
};
use super::cache::invalidate_chunk_caches;
use super::extract::extract_block_raw;
use super::resources::{
    GroupMembers, TerrainCaches, TerrainGroupMembers, TerrainGroups, TerrainStats,
};

/// 构造一个 mesh 分组实体 bundle（stable id + 逐块 raw/mask + 组原点变换 + Size）。
#[allow(clippy::type_complexity)]
fn group_bundle(
    id: StableEntityId,
    key: GroupKey,
    chunks: BTreeMap<ChunkKey, (Vec<u8>, u8)>,
    voxel_size_m: f32,
) -> (
    StableEntityId,
    VoxChunkGroupRaw,
    Transform,
    Size,
    PresentedTransform,
) {
    // 组原点（米，块基准尺度）= group_origin * 32 * voxel_size；仅影响 culling，
    // VoxelGroupNode 的子节点用各自块的绝对世界坐标，与组原点无关。
    let position = Vec3::from_array(block_origin_meters(group_origin(key), voxel_size_m));
    let mut presented = PresentedTransform::default();
    presented.set_sampled(
        RenderTransform::from_translation(position),
        RenderTransform::from_translation(position),
    );
    (
        id,
        VoxChunkGroupRaw {
            lod: key.lod,
            chunks,
        },
        Transform::from_translation(position),
        Size(None),
        presented,
    )
}

/// 把 desired 块列表归并成 `GroupKey -> 成员块`（BTreeMap，键升序确定）。
fn desired_groups(desired: &[TerrainBlock]) -> BTreeMap<GroupKey, BTreeSet<ChunkKey>> {
    let mut out: BTreeMap<GroupKey, BTreeSet<ChunkKey>> = BTreeMap::new();
    for block in desired {
        out.entry(group_key(block.origin, block.lod))
            .or_default()
            .insert(block.origin);
    }
    out
}

/// 组成员块的 6 面 LOD 感知邻居签名（块原点升序 = 确定）。
///
/// 把「成员集合相同、但邻块 LOD 变化」也纳入重建判据：邻块 LOD 影响本块
/// `extract_raw_halo_with_faces` 的边界采样与 [`skirt_mask`]，只比成员集合会漏重建。
pub(crate) fn group_neighbour_signature(
    blocks: &BTreeSet<ChunkKey>,
    lod: u8,
    partition: &TerrainPartition,
) -> BTreeMap<ChunkKey, ([FaceNeighbor; 6], u8)> {
    blocks
        .iter()
        .map(|origin| {
            let block = TerrainBlock {
                origin: *origin,
                lod,
            };
            // 同 LOD 报告（剔除依据）+ 跨 LOD 面掩码（裙边依据）。
            (
                *origin,
                (
                    face_neighbors(&block, partition),
                    lod_mismatch_mask(&block, partition),
                ),
            )
        })
        .collect()
}

/// 组重建 / 消失时按块失效 C/D 的包装缓存（口径不变：逐块 invalidate_covered）。
fn invalidate_group(blocks: &BTreeSet<ChunkKey>, caches: &mut TerrainCaches<'_>) {
    let TerrainCaches {
        interner,
        wrapped,
        generated,
        neighbors,
    } = caches;
    for origin in blocks {
        invalidate_chunk_caches(interner, *origin, wrapped, generated, neighbors);
    }
}

/// 组重建的只读输入：体素容器、生成器与体素边长（米）。
pub(crate) struct GroupDiffWorld<'a> {
    /// 地形体素容器（只读：本系统只包装 / 生成块树）。
    pub(crate) volume: &'a VoxVolume,
    /// 世界生成器（粗 LOD 直接按 LOD 生成时用）。
    pub(crate) generator: &'a WorldGenerator,
    /// 体素边长（米，f32），用于组原点的 Transform。
    pub(crate) voxel_size_f32: f32,
}

/// 组重建的可变状态：分组索引、成员快照、统计与 stable id 分配。
pub(crate) struct GroupDiffState<'a> {
    /// GroupKey -> 实体 索引。
    pub(crate) groups: &'a mut TerrainGroups,
    /// 上次构建的组成员快照。
    pub(crate) members: &'a mut TerrainGroupMembers,
    /// 地形统计（构建后从成员快照重算）。
    pub(crate) stats: &'a mut TerrainStats,
    /// stable id 分配器（逐组分配）。
    pub(crate) allocator: &'a mut StableIdAllocator,
    /// stable id -> 实体索引（spawn / despawn 时同步）。
    pub(crate) index: &'a mut StableEntityIndex,
}

/// 一次 diff 的可变上下文：diff 期间所有借用都收在这里，便于按阶段拆函数。
struct GroupDiffCtx<'a, 'b, 'w, 's> {
    commands: &'a mut Commands<'w, 's>,
    volume: &'a VoxVolume,
    generator: &'a WorldGenerator,
    voxel_size_f32: f32,
    partition: &'a TerrainPartition,
    caches: &'a mut TerrainCaches<'b>,
    groups: &'a mut TerrainGroups,
    members: &'a mut TerrainGroupMembers,
    stats: &'a mut TerrainStats,
    allocator: &'a mut StableIdAllocator,
    index: &'a mut StableEntityIndex,
}

/// 组级 diff + 重建：Startup 与运行期流式共用，避免两条路径分叉。
///
/// 硬约束：只有**新增组 / 成员块集合变化的组**才对该组内每块跑 extract_block_raw；
/// 未变化的组完全不动（不 re-extract、不 spawn）。组内全空则不 spawn，但成员快照
/// 仍记录，避免下次重算分区时把全空组当成新组反复重建。
/// stable id 逐组分配、按 GroupKey 升序，分配顺序确定。
pub(crate) fn apply_group_diff(
    commands: &mut Commands,
    world: &mut GroupDiffWorld<'_>,
    caches: &mut TerrainCaches<'_>,
    state: &mut GroupDiffState<'_>,
    desired: &[TerrainBlock],
    partition: &TerrainPartition,
) {
    let mut ctx = GroupDiffCtx {
        commands,
        volume: world.volume,
        generator: world.generator,
        voxel_size_f32: world.voxel_size_f32,
        partition,
        caches,
        groups: state.groups,
        members: state.members,
        stats: state.stats,
        allocator: state.allocator,
        index: state.index,
    };
    let desired_by_group = desired_groups(desired);

    // ── 1. 消失的组（快照里有、期望里没有）：despawn + 逐块失效 ──
    despawn_gone_groups(&mut ctx, &desired_by_group);

    // ── 2. 新增 / 成员集变化的组：重建；未变化的组直接跳过 ──
    for (key, desired_blocks) in &desired_by_group {
        rebuild_group(&mut ctx, *key, desired_blocks);
    }

    // ── 3. 统计从成员快照重算，避免漂移（chunks 是生成期口径，不动）──
    recompute_stats(ctx.groups, ctx.members, ctx.stats);
}

/// 重建单个组：判据 = 成员集合 + 每成员块的 6 面 LOD 邻居签名。
///
/// 邻块 LOD 变化时成员集合可能完全不变，但本块 halo 采样与 skirt_mask 会变，
/// 必须一并重建；未变化的组完全不动（不 re-extract、不 spawn）。
fn rebuild_group(
    ctx: &mut GroupDiffCtx<'_, '_, '_, '_>,
    key: GroupKey,
    desired_blocks: &BTreeSet<ChunkKey>,
) {
    let neighbours = group_neighbour_signature(desired_blocks, key.lod, ctx.partition);
    let unchanged = ctx
        .members
        .0
        .get(&key)
        .is_some_and(|old| &old.blocks == desired_blocks && old.neighbours == neighbours);
    if unchanged {
        return;
    }
    // 旧实体换新（stable id 重分配），旧 id 先注销。
    if let Some(entity) = ctx.groups.0.remove(&key) {
        ctx.index.remove_by_entity(entity);
        ctx.commands.entity(entity).despawn();
    }
    // 失效口径仍按块：旧成员 + 新成员逐个 invalidate_covered。
    if let Some(old) = ctx.members.0.get(&key) {
        invalidate_group(&old.blocks, ctx.caches);
    }
    invalidate_group(desired_blocks, ctx.caches);

    let chunks = extract_group_chunks(
        ctx.volume,
        ctx.generator,
        ctx.partition,
        key,
        desired_blocks,
        ctx.caches,
    );
    let solid = chunks.len();
    spawn_group(
        ctx.commands,
        ctx.groups,
        ctx.allocator,
        ctx.index,
        key,
        chunks,
        ctx.voxel_size_f32,
    );
    ctx.members.0.insert(
        key,
        GroupMembers {
            blocks: desired_blocks.clone(),
            solid,
            neighbours,
        },
    );
}

/// 期望里已不存在的组：despawn 实体、注销 stable id、逐块失效缓存并清快照。
fn despawn_gone_groups(
    ctx: &mut GroupDiffCtx<'_, '_, '_, '_>,
    desired: &BTreeMap<GroupKey, BTreeSet<ChunkKey>>,
) {
    let gone: Vec<GroupKey> = ctx
        .members
        .0
        .keys()
        .filter(|key| !desired.contains_key(*key))
        .copied()
        .collect();
    for key in gone {
        if let Some(entity) = ctx.groups.0.remove(&key) {
            ctx.index.remove_by_entity(entity);
            ctx.commands.entity(entity).despawn();
        }
        if let Some(old) = ctx.members.0.remove(&key) {
            invalidate_group(&old.blocks, ctx.caches);
        }
    }
}

/// 组内逐块提取原始 halo；返回 块原点 -> (halo 字节, skirt_mask)（键升序确定）。
fn extract_group_chunks(
    volume: &VoxVolume,
    generator: &WorldGenerator,
    partition: &TerrainPartition,
    key: GroupKey,
    blocks: &BTreeSet<ChunkKey>,
    caches: &mut TerrainCaches<'_>,
) -> BTreeMap<ChunkKey, (Vec<u8>, u8)> {
    let mut chunks = BTreeMap::new();
    for origin in blocks {
        let block = TerrainBlock {
            origin: *origin,
            lod: key.lod,
        };
        if let Some((raw, mask)) = extract_block_raw(block, volume, generator, partition, caches) {
            chunks.insert(*origin, (raw, mask));
        }
    }
    chunks
}

/// spawn 一个分组实体并登记 stable id；全空组不 spawn（成员快照仍由调用方记录）。
fn spawn_group(
    commands: &mut Commands,
    groups: &mut TerrainGroups,
    allocator: &mut StableIdAllocator,
    index: &mut StableEntityIndex,
    key: GroupKey,
    chunks: BTreeMap<ChunkKey, (Vec<u8>, u8)>,
    voxel_size_f32: f32,
) {
    if chunks.is_empty() {
        return;
    }
    let id = allocator.allocate();
    let entity = commands
        .spawn(group_bundle(id, key, chunks, voxel_size_f32))
        .id();
    index.insert(id, entity);
    groups.0.insert(key, entity);
}

/// 统计从成员快照重算，避免漂移（chunks 是生成期口径，不动）。
fn recompute_stats(
    groups: &TerrainGroups,
    members: &TerrainGroupMembers,
    stats: &mut TerrainStats,
) {
    stats.groups = groups.0.len();
    stats.blocks = 0;
    stats.skipped_empty = 0;
    stats.lod_blocks = [0; 4];
    stats.lod_groups = [0; 4];
    for (key, group) in &members.0 {
        let lod = usize::from(key.lod);
        stats.blocks += group.solid;
        stats.skipped_empty += group.blocks.len() - group.solid;
        stats.lod_blocks[lod] += group.solid;
        if groups.0.contains_key(key) {
            stats.lod_groups[lod] += 1;
        }
    }
}
