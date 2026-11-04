//! terrain 的 bevy 系统：启动生成、地形体挂载与运行期 LOD 重划分。

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use game_engine::identity::{StableEntityId, StableEntityIndex, StableIdAllocator};
use game_engine::math::Vec3F;
use game_engine::presentation::voxel::WrappedBlockCache;
use game_engine::voxel::{VoxVolume, VoxelInterner};

use crate::static_data::voxel::{BiomeId, DEFAULT_VOXEL_SIZE};
use crate::voxel::body::attach_island_entity;
use crate::voxel::lod::{VoxelLodConfig, MAX_LOD};
use crate::voxel::terrain::generation::WorldGenerator;
use crate::world::WorldSeed;

use super::blocks::{
    chunk_keys_to_generate, lod_blocks, lod_blocks_for_observer, TerrainBlock, TerrainPartition,
};
use super::group_diff::{apply_group_diff, GroupDiffState, GroupDiffWorld};
use super::plugin::TerrainConfig;
use super::resources::{
    TerrainCaches, TerrainGeneratedBlocks, TerrainGroupMembers, TerrainGroups,
    TerrainNeighborBlocks, TerrainStats, TerrainStreamState, VoxelLodHandle, VoxelLodObserver,
};

/// 一次地形重建需要的四份块缓存（[SystemParam] 聚合，避免系统参数超过 clippy 阈值）。
#[derive(SystemParam)]
pub(crate) struct TerrainCacheParams<'w> {
    /// 节点 interner。
    pub(crate) interner: ResMut<'w, VoxelInterner>,
    /// LOD0 包装缓存（本块）。
    pub(crate) wrapped: ResMut<'w, WrappedBlockCache>,
    /// 按 LOD 生成的块树缓存（own + neighbors）。
    pub(crate) generated: ResMut<'w, TerrainGeneratedBlocks>,
    /// 26 邻的 LOD0 包装缓存。
    pub(crate) neighbors: ResMut<'w, TerrainNeighborBlocks>,
}

/// 组级 diff 的可变状态（分组索引 / 成员快照 / 统计 / stable id）。
#[derive(SystemParam)]
pub(crate) struct TerrainGroupParams<'w> {
    /// GroupKey 到实体的索引。
    pub(crate) groups: ResMut<'w, TerrainGroups>,
    /// 上次构建的组成员快照。
    pub(crate) members: ResMut<'w, TerrainGroupMembers>,
    /// 地形统计。
    pub(crate) stats: ResMut<'w, TerrainStats>,
    /// stable id 分配器。
    pub(crate) allocator: ResMut<'w, StableIdAllocator>,
    /// stable id 到实体的索引。
    pub(crate) index: ResMut<'w, StableEntityIndex>,
}

/// 运行期 LOD 的输入（FFI 句柄 + 配置 / 观察者 / 上次输入）。
#[derive(SystemParam)]
pub(crate) struct LodInputParams<'w> {
    /// Godot 侧 FFI 句柄（可选）。
    pub(crate) lod_handle: Option<Res<'w, VoxelLodHandle>>,
    /// LOD 配置（可能被 FFI 覆写）。
    pub(crate) lod_config: ResMut<'w, VoxelLodConfig>,
    /// 观察者位置（可能被 FFI 覆写）。
    pub(crate) observer: ResMut<'w, VoxelLodObserver>,
    /// 上一次推进的输入（early-out 用）。
    pub(crate) stream: ResMut<'w, TerrainStreamState>,
}

/// Setup 用体素边长（米，f64 镜像）。
#[must_use]
pub fn voxel_size_meters() -> f64 {
    DEFAULT_VOXEL_SIZE.to_num::<f64>()
}

/// Startup：生成子块 -> 逐块网格化 -> 产出 stable id 实体。
///
/// 体素容器在本地构建，生成完成后作为 `VoxVolume` **组件** spawn 到地形
/// 根实体上，不写进任何 Resource。
///
/// 用普通 system（多 `ResMut`）而不是 exclusive system，是为了让
/// interner / wrapped / 邻块缓存 / 增量缓存的借用天然分离；实体 id 由
/// `StableIdAllocator` 立即分配并登记进 `StableEntityIndex`（等价
/// `spawn_stable` 的当帧语义）。
pub(crate) fn build_terrain(
    mut commands: Commands,
    config: Res<TerrainConfig>,
    mut caches: TerrainCacheParams,
    mut group: TerrainGroupParams,
) {
    let voxel_size_m = voxel_size_meters();
    let radius_blocks = config.radius_blocks.max(0);
    let max_lod = config.max_lod.min(MAX_LOD);

    // ── 1. 生成基础子块（本地 VoxVolume，最后作为组件挂到地形根实体）──
    let (volume, generator) = generate_terrain_volume(
        &mut caches.interner,
        radius_blocks,
        max_lod,
        config.seed,
        config.biome,
    );
    group.stats.chunks = volume.chunk_count();

    // ── 2. 混合分组 + 组内逐块网格化 + 产出实体（分区 = 静态观察者）──
    let planned_blocks = lod_blocks(radius_blocks, max_lod, voxel_size_m);
    let partition = TerrainPartition::new(&planned_blocks);
    // 借用包只覆盖 diff 本身，之后还要读 stats / 用 volume 生成根实体。
    apply_startup_diff(
        &mut commands,
        &mut caches,
        &mut group,
        &volume,
        &generator,
        &planned_blocks,
        &partition,
    );

    // 地形根实体：体素容器以组件形式挂载
    //（「一岛 / 一船 / 一结构 = 一个 VoxVolume 组件」）；生成完成后一次性 attach。
    // physics：同时挂 VoxelCollider，让地形派生 avian 碰撞体（Transform 由
    // #[require(Transform)] 补为原点，正好等于 body 局部原点）。
    #[cfg(feature = "physics")]
    commands.spawn((volume, game_engine::voxel::VoxelCollider));
    #[cfg(not(feature = "physics"))]
    commands.spawn(volume);

    info!(
        "terrain: chunks={} blocks={} lod={:?} seed={:#x}",
        group.stats.chunks, group.stats.blocks, group.stats.lod_blocks, config.seed
    );
}

/// Startup 的组级 diff：构造一次缓存借用包 / 只读输入 / 可变状态并跑 diff。
fn apply_startup_diff(
    commands: &mut Commands,
    caches: &mut TerrainCacheParams,
    group: &mut TerrainGroupParams,
    volume: &VoxVolume,
    generator: &WorldGenerator,
    planned_blocks: &[TerrainBlock],
    partition: &TerrainPartition,
) {
    let voxel_size_f32 = DEFAULT_VOXEL_SIZE.to_num::<f32>();
    let mut cache_bundle = TerrainCaches {
        interner: &mut caches.interner,
        wrapped: &mut caches.wrapped,
        generated: &mut caches.generated,
        neighbors: &mut caches.neighbors,
    };
    let mut world = GroupDiffWorld {
        volume,
        generator,
        voxel_size_f32,
    };
    let mut state = GroupDiffState {
        groups: &mut group.groups,
        members: &mut group.members,
        stats: &mut group.stats,
        allocator: &mut group.allocator,
        index: &mut group.index,
    };
    apply_group_diff(
        commands,
        &mut world,
        &mut cache_bundle,
        &mut state,
        planned_blocks,
        partition,
    );
}

/// 生成本地地形体素容器（x/z 多生成一圈 margin）与配套的世界生成器。
fn generate_terrain_volume(
    interner: &mut VoxelInterner,
    radius_blocks: i32,
    max_lod: u8,
    seed: u64,
    biome: BiomeId,
) -> (VoxVolume, WorldGenerator) {
    let mut volume = VoxVolume::new(DEFAULT_VOXEL_SIZE);
    let generator = WorldGenerator::new(WorldSeed(seed), biome);
    for key in chunk_keys_to_generate(radius_blocks, max_lod) {
        let tree = generator.generate_chunk(interner.inner_mut(), key);
        volume.insert_chunk(key, tree);
    }
    (volume, generator)
}

/// 把地形根实体升级成正式 island body：挂 `StableEntityId` + 写 [IslandRegistry]。
///
/// 两种调用方都把它 chain 在「容器出现」之后：
/// - Eager：`build_terrain` 用 `Commands` spawn，executor 在运行 exclusive system
///   前会 flush 命令队列；
/// - Streaming：`bootstrap_streaming_world` 要么读档恢复 body（已带稳定 ID，被
///   `Without<StableEntityId>` 排除），要么直接 `world.spawn` 空容器。
///
/// 所以这里必须是 **exclusive system**（`&mut World`）。Eager 路径由
/// `terrain_root_entity_is_a_registered_island_body` 实测（一次 `update()` 后体素
/// 实体必须已带稳定 ID 且注册表有对应条目）。
///
/// 地形是锚定岛：`drift = ZERO`、`anchored = true`、`biome = TerrainConfig.biome`。
/// 幂等：已带 `StableEntityId` 的实体被 `Without<StableEntityId>` 排除，重复运行
/// 不会二次分配；即使被再次 attach，[`attach_island_entity`] 也复用原 ID。
pub(crate) fn attach_terrain_body(world: &mut World) {
    let biome = world.resource::<TerrainConfig>().biome;
    // 先收集成 Vec 结束查询对 world 的借用，再逐实体 attach（需要 &mut World）。
    let entities: Vec<Entity> = {
        let mut query =
            world.query_filtered::<Entity, (With<VoxVolume>, Without<StableEntityId>)>();
        query.iter(world).collect()
    };
    for entity in entities {
        attach_island_entity(world, entity, Vec3F::ZERO, true, biome);
    }
}

/// 运行期流式重划分：按观察者把 LOD 分区 diff 到 [TerrainGroups]，增删分组实体。
///
/// 输入优先级：若存在 [VoxelLodHandle] 且 observer / config 为 Some，则覆写
/// [VoxelLodObserver] / [VoxelLodConfig]（Godot 经 FFI 驱动）；否则用 Rust 侧资源
/// （crate::dev::perf 的移动观察者）。
///
/// 确定性：desired / current 都用 BTreeSet，增删按排序顺序处理，因此
/// StableIdAllocator 的分配顺序可复现。观察者只动一点时分区往往不变，diff 为空；
/// 只有 (observer, config) 变化才重算分区。
pub(crate) fn stream_terrain_lod(
    mut commands: Commands,
    config: Res<TerrainConfig>,
    mut lod: LodInputParams,
    mut caches: TerrainCacheParams,
    mut group: TerrainGroupParams,
    volume_query: Query<&VoxVolume>,
) {
    // ── 1. 解析 FFI 覆写 / 夹取配置；输入没变则本 tick 不动 ──
    let Some((effective, observer_position)) = resolve_lod_inputs(&mut lod) else {
        return;
    };
    let Some(volume) = volume_query.iter().next() else {
        // 体素容器尚未生成：不推进状态，下一 tick 重试。
        return;
    };
    lod.stream.last_config = Some(effective);
    lod.stream.last_observer = observer_position;

    let voxel_size_m = voxel_size_meters();
    let radius_blocks = config.radius_blocks.max(0);
    let max_lod = config.max_lod.min(MAX_LOD);

    // ── 2. 期望分区（与 Startup 相同的 base cell 集合，只改 LOD 合并基准）──
    let desired = lod_blocks_for_observer(
        [
            f64::from(observer_position[0]),
            f64::from(observer_position[2]),
        ],
        radius_blocks,
        max_lod,
        voxel_size_m,
        &effective,
    );
    let partition = TerrainPartition::new(&desired);

    // ── 3. 组级 diff + 重建（BTreeMap 键升序 = 确定性分配顺序）──
    apply_stream_diff(
        &mut commands,
        &mut caches,
        &mut group,
        volume,
        &config,
        &desired,
        &partition,
    );
}

/// 流式重划分的组级 diff：构造生成器 / 缓存借用包 / 只读输入 / 可变状态并跑 diff。
fn apply_stream_diff(
    commands: &mut Commands,
    caches: &mut TerrainCacheParams,
    group: &mut TerrainGroupParams,
    volume: &VoxVolume,
    config: &TerrainConfig,
    desired: &[TerrainBlock],
    partition: &TerrainPartition,
) {
    let generator = WorldGenerator::new(WorldSeed(config.seed), config.biome);
    let voxel_size_f32 = DEFAULT_VOXEL_SIZE.to_num::<f32>();
    let mut cache_bundle = TerrainCaches {
        interner: &mut caches.interner,
        wrapped: &mut caches.wrapped,
        generated: &mut caches.generated,
        neighbors: &mut caches.neighbors,
    };
    let mut world = GroupDiffWorld {
        volume,
        generator: &generator,
        voxel_size_f32,
    };
    let mut state = GroupDiffState {
        groups: &mut group.groups,
        members: &mut group.members,
        stats: &mut group.stats,
        allocator: &mut group.allocator,
        index: &mut group.index,
    };
    apply_group_diff(
        commands,
        &mut world,
        &mut cache_bundle,
        &mut state,
        desired,
        partition,
    );
}

/// 解析 FFI 覆写、把配置夹到合法域，并判断输入是否变化。
///
/// 返回 Some((生效配置, 观察者位置)) 表示输入变化、需要重划分；None 表示
/// 观察者与配置都没变，本 tick 直接跳过（不推进 [TerrainStreamState]）。
fn resolve_lod_inputs(lod: &mut LodInputParams<'_>) -> Option<(VoxelLodConfig, [f32; 3])> {
    if let Some(handle) = lod.lod_handle.as_deref() {
        if let Ok(runtime) = handle.0.lock() {
            if let Some(position) = runtime.observer {
                lod.observer.position = position;
            }
            if let Some(runtime_config) = runtime.config {
                let sanitized = runtime_config.sanitized();
                if *lod.lod_config != sanitized {
                    *lod.lod_config = sanitized;
                }
            }
        }
    }
    let effective = lod.lod_config.sanitized();
    if effective != *lod.lod_config {
        *lod.lod_config = effective;
    }
    let observer_position = lod.observer.position;

    // ── 2. early-out：观察者与配置都没变则不动 ──
    if lod.stream.last_config == Some(effective) && lod.stream.last_observer == observer_position {
        return None;
    }
    Some((effective, observer_position))
}
