//! 体素地形接入（体素表现唯一通道 = RawVoxels）：Startup 生成一圈地形块，
//! 产出带 [VoxChunkRaw] 的 mesh 块实体。
//!
//! 体素容器 [VoxVolume] **只作为组件**挂在地形根实体上
//! （「一岛 / 一船 / 一结构 = 一个 VoxVolume 组件」），不作为 Resource；
//! 本模块不持有任何全局体素单例。
//!
//! ## 数据流
//!
//! 1. `Startup`：按 [TerrainConfig] 的半径 / max_lod 用 `WorldGenerator` 生成
//!    基础子块（x/z 多生成一圈 margin，让边缘块的 external 邻块有数据）；
//! 2. 逐块走引擎生产管线：`WrappedBlockCache::get_or_wrap` +
//!    `extract_raw_halo` 得到内部 32³ + 一层 halo（共 34³）的方块 id 缓冲；
//! 3. 每个块一个 stable id 实体：`VoxChunkRaw{lod, blocks}` +
//!    `Transform` / `PresentedTransform`（块原点，米）+ `Size(None)`；
//! 4. 体素容器 `VoxVolume` 本地构建、生成完一次性 spawn 成**组件**。
//!
//! 贪婪 meshing 在 Godot 侧完成（调 gdext `mesh_voxel_halo` 现算 39 bit 矩形流）。
//!
//! ## 边界
//!
//! - 块原点（米）= `origin * 32 * voxel_size`，**不乘 2^lod**（见
//!   [blocks::block_origin_meters]）；
//! - 原始体素是块局部、位置无关的数据，LOD 缩放由渲染侧按 payload 的 lod 处理；
//! - 本模块不依赖 godot，也不改 crates/voxel 或 game-engine。
//!
//! ## 运行期流式重划分（observer-relative LOD）
//!
//! 生成仍在 Startup 一次完成（有限区域），但 LOD 分区在运行期按观察者位置重算：
//! 1. 观察者来自 [VoxelLodObserver]；若存在 [VoxelLodHandle] 且其 observer 为
//!    Some，则由 Godot 经 FFI set_voxel_observer 覆写（config 同理）；
//! 2. stream_terrain_lod（Update）用 lod_blocks_for_observer 算出期望分区，
//!    与 [TerrainBlocks] 做 BTreeSet diff；
//! 3. 移除的块 despawn 并从 StableEntityIndex 注销，新增 / 换 LOD 的块重新
//!    extract_raw_halo 后 spawn；统计从 map 重算，不漂移。
//!
//! 体素表现唯一通道仍是 RawVoxels 载荷；本模块只改变「哪些块、什么 LOD」，
//! 不改载荷格式，也不改 crates/voxel 或 game-engine。

pub mod blocks;
pub mod generation;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use game_engine::identity::{StableEntityId, StableEntityIndex, StableIdAllocator};
use game_engine::presentation::voxel::{
    extract_raw_halo, raw_halo_has_solid_interior, WrappedBlockCache, RAW_VOXELS,
};
use game_engine::presentation::RenderTransform;
use game_engine::spatial::Size;
use game_engine::voxel::{ChunkKey, Lod, MeshBlock, VoxTree, VoxVolume, VoxelInterner};

use crate::presentation::voxel_mesh::VoxChunkRaw;
use crate::presentation::PresentedTransform;
use crate::static_data::voxel::{BiomeId, DEFAULT_BIOME, DEFAULT_VOXEL_SIZE};
use crate::voxel::lod::{VoxelLodConfig, MAX_LOD};
use crate::voxel::terrain::generation::WorldGenerator;
use crate::world::WorldSeed;

pub use blocks::{
    block_origin_meters, block_span, chunk_keys_to_generate, lod_blocks, lod_blocks_for_observer,
    nearest_axis_distance_m, nearest_block_distance_m, neighbor_origins, y_band_layers,
    TerrainBlock, TERRAIN_MARGIN_CHUNKS,
};

/// 地形生成参数（体素层资源）。装配方在 `add_plugins(GameVoxelPlugin)` **之前**
/// `insert_resource` 后，[TerrainPlugin] 就会注册地形生成系统；不插入则不生成。
///
/// 地形由独立插件 [TerrainPlugin] 装配（依赖 [GameVoxelPlugin]）。体素本身只以组件存在——
/// `VoxVolume`（容器）+ `VoxChunkRaw`（每块的 32³ + halo 原始体素）；本结构只是
/// 生成参数，不是体素数据。
///
/// `radius_blocks` 是 x/z 上围绕世界原点的基础子块半径（1 个基础子块 = 32
/// 体素 ≈ 14.4 m）。debug 构建生成很慢（实测约 150 ms/子块），所以默认值刻意小：
/// - [TerrainConfig::default]：radius 2 / max_lod 1（7×7×4 ≈ 196 子块；debug ≈ 30 s，
///   release ≈ 2–4 s）；
/// - [TerrainConfig::demo]：radius 0 / max_lod 0（36 子块；debug ≈ 5 s）。
///
/// LOD 合并要半径 ≥ 5 才会越过 32 m 的第一档阈值，所以想看 LOD 切换要用
/// release 构建并调大半径（FFI：`start_bevy_voxel_perf(fixed_hz, runner_hz, radius, max_lod)`）。
///
/// [GameVoxelPlugin]: crate::voxel::GameVoxelPlugin
#[derive(Resource, Clone, Copy, Debug)]
pub struct TerrainConfig {
    /// 水平方向围绕世界原点的 mesh 块半径（基础子块坐标）。
    pub radius_blocks: i32,
    /// 最高 LOD（0..=3，见 `voxel::MAX_LOD`）。
    pub max_lod: u8,
    /// 世界生成种子。
    pub seed: u64,
    /// 生物群系（决定调色板与高度基线）。
    pub biome: BiomeId,
}

impl TerrainConfig {
    /// 默认实体 demo 用的极小地形：1 个 XZ 单元（36 个子块）、无 LOD。
    ///
    /// debug 构建实测约 150 ms/子块（36 子块 ≈ 5 s），所以"默认挂在地形上的
    /// demo"用最小规模；要看更大范围请走 `start_bevy_voxel_perf(..., radius, lod)`。
    #[must_use]
    pub fn demo() -> Self {
        Self {
            radius_blocks: 0,
            max_lod: 0,
            seed: 0xC0FF_EE,
            biome: DEFAULT_BIOME,
        }
    }
}

impl Default for TerrainConfig {
    fn default() -> Self {
        Self {
            radius_blocks: 2,
            max_lod: 1,
            seed: 0xC0FF_EE,
            biome: DEFAULT_BIOME,
        }
    }
}

/// 地形插件（独立）：依赖 [GameVoxelPlugin]；装配方先插入 [TerrainConfig] 才注册系统。
pub struct TerrainPlugin;

impl Plugin for TerrainPlugin {
    fn build(&self, app: &mut App) {
        assert!(
            app.is_plugin_added::<super::GameVoxelPlugin>(),
            "TerrainPlugin 依赖 GameVoxelPlugin"
        );
        if app.world().get_resource::<TerrainConfig>().is_some() {
            register_systems(app);
        }
    }
}

/// 注册地形生成系统与运行期资源：由 [TerrainPlugin]
/// 在 `TerrainConfig` 资源存在时调用。
///
/// 只做两件事：挂 `Startup` 生成系统、初始化实体索引 / 统计资源；体素数据
/// 本身只以组件存在（生成时把 `VoxVolume` spawn 到实体上）。
pub(crate) fn register_systems(app: &mut App) {
    app.init_resource::<TerrainBlocks>()
        .init_resource::<TerrainNeighborBlocks>()
        // 表现层网格机制不注册插件；WrappedBlockCache 由唯一消费者 core 侧 init。
        .init_resource::<WrappedBlockCache>()
        .init_resource::<TerrainStats>()
        .init_resource::<VoxelLodConfig>()
        .init_resource::<VoxelLodObserver>()
        .init_resource::<TerrainStreamState>()
        // 观察者写入者在 FixedUpdate；流式重划分放 Update 以便同 tick 看到新位置。
        .add_systems(Startup, build_terrain)
        .add_systems(Update, stream_terrain_lod);
}

/// 已产出的 mesh 块实体索引：`(块原点, lod) -> Entity`（确定性升序）。
#[derive(Resource, Default, Debug)]
pub struct TerrainBlocks(BTreeMap<(ChunkKey, u8), Entity>);

impl TerrainBlocks {
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn get(&self, origin: ChunkKey, lod: u8) -> Option<Entity> {
        self.0.get(&(origin, lod)).copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = ((ChunkKey, u8), Entity)> + '_ {
        self.0.iter().map(|(key, entity)| (*key, *entity))
    }
}

/// 邻块包装缓存：`extract_raw_halo` 的 `external` 只读借用与它自己的
/// `&mut WrappedBlockCache` 不能来自同一个变量（借用冲突），所以邻块单独一份。
#[derive(Resource, Default)]
pub struct TerrainNeighborBlocks(pub WrappedBlockCache);

/// 地形统计（demo / perf / 测试断言用；不进协议）。
#[derive(Resource, Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerrainStats {
    /// 生成的基础子块数。
    pub chunks: usize,
    /// 产出的 mesh 块实体数。
    pub blocks: usize,
    /// 每个 LOD 的块数（下标 = lod，0..=3）。
    pub lod_blocks: [usize; 4],
    /// 因网格为空（全空气）而跳过的块数。
    pub skipped_empty: usize,
}

/// 观察者位置（米，世界坐标）。运行期流式系统按它重划分 LOD。
///
/// 默认 [0.0, 0.0, 0.0]；由 crate::dev::perf::move_voxel_observer 或
/// godot-client-ext 经 [VoxelLodHandle] 的 observer 覆写驱动。
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq)]
pub struct VoxelLodObserver {
    pub position: [f32; 3],
}

/// 跨线程（Godot -> Bevy）的 LOD 运行期输入。
///
/// None 表示不覆写：observer 覆写 [VoxelLodObserver]，config 覆写 [VoxelLodConfig]。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct VoxelLodRuntime {
    pub observer: Option<[f32; 3]>,
    pub config: Option<VoxelLodConfig>,
}

/// FFI 句柄：Godot 主线程写，Bevy 后台线程读，内部 Arc<Mutex<..>>。
#[derive(Resource, Clone, Default)]
pub struct VoxelLodHandle(pub Arc<Mutex<VoxelLodRuntime>>);

/// 流式系统的上一次输入（early-out 用）。
#[derive(Resource, Default)]
struct TerrainStreamState {
    last_observer: [f32; 3],
    last_config: Option<VoxelLodConfig>,
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
/// 提取一个块的原始体素 halo（34³）；内部无实心体素返回 None。
///
/// 共享给 Startup 生成与运行期流式重划分，避免两条路径分叉。邻块缓存与本块
/// 包装缓存必须是两个资源（external 只读借用与 &mut 本块缓存冲突）。
fn extract_block_raw(
    block: TerrainBlock,
    volume: &VoxVolume,
    _voxel_size_m: f32,
    interner: &mut VoxelInterner,
    wrapped: &mut WrappedBlockCache,
    neighbors: &mut TerrainNeighborBlocks,
) -> Option<Vec<u8>> {
    let lod = Lod::new(block.lod);
    let mesh_block = MeshBlock::new(block.origin, lod);
    let neighbors6 = neighbor_origins(mesh_block.origin, block.lod);

    // 预热同 LOD 的 6 邻（邻块缓存），随后只读取回。
    for neighbor in neighbors6.iter() {
        neighbors.0.get_or_wrap(
            &volume.chunks,
            interner.inner_mut(),
            MeshBlock::new(*neighbor, lod),
        );
    }
    let external: [Option<&VoxTree<u8>>; 6] = {
        let neighbor_cache = &neighbors.0;
        let mut out: [Option<&VoxTree<u8>>; 6] = [None; 6];
        for (face, neighbor) in neighbors6.iter().enumerate() {
            out[face] = neighbor_cache.get(*neighbor, lod);
        }
        out
    };

    // 用已 wrap 的块树 + 6 邻树提取原始体素（内部 32³ + 一层 halo）。
    let tree = wrapped.get_or_wrap(&volume.chunks, interner.inner_mut(), mesh_block);
    let raw = extract_raw_halo(tree, interner.inner(), mesh_block, external);
    debug_assert_eq!(raw.len(), RAW_VOXELS, "extract_raw_halo 必须产出 34³");
    if raw_halo_has_solid_interior(&raw) {
        Some(raw)
    } else {
        None
    }
}

/// 构造一个 mesh 块实体 bundle（stable id + 原始体素 + 块原点变换 + Size）。
fn block_bundle(
    id: StableEntityId,
    block: TerrainBlock,
    raw: Vec<u8>,
    voxel_size_m: f32,
) -> (
    StableEntityId,
    VoxChunkRaw,
    Transform,
    Size,
    PresentedTransform,
) {
    let position = Vec3::from_array(block_origin_meters(block.origin, voxel_size_m));
    let mut presented = PresentedTransform::default();
    presented.set_sampled(
        RenderTransform::from_translation(position),
        RenderTransform::from_translation(position),
    );
    (
        id,
        VoxChunkRaw {
            lod: block.lod,
            blocks: raw,
        },
        Transform::from_translation(position),
        Size(None),
        presented,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_terrain(
    mut commands: Commands,
    config: Res<TerrainConfig>,
    mut interner: ResMut<VoxelInterner>,
    mut wrapped: ResMut<WrappedBlockCache>,
    mut neighbors: ResMut<TerrainNeighborBlocks>,
    mut blocks: ResMut<TerrainBlocks>,
    mut stats: ResMut<TerrainStats>,
    mut allocator: ResMut<StableIdAllocator>,
    mut index: ResMut<StableEntityIndex>,
) {
    let voxel_size_m = voxel_size_meters();
    let voxel_size_f32 = DEFAULT_VOXEL_SIZE.to_num::<f32>();
    let radius_blocks = config.radius_blocks.max(0);
    let max_lod = config.max_lod.min(MAX_LOD);

    // ── 1. 生成基础子块（本地 VoxVolume，最后作为组件挂到地形根实体）──
    let mut volume = VoxVolume::new(DEFAULT_VOXEL_SIZE);
    let generator = WorldGenerator::new(WorldSeed(config.seed), config.biome);
    for key in chunk_keys_to_generate(radius_blocks, max_lod) {
        let tree = generator.generate_chunk(interner.inner_mut(), key);
        volume.insert_chunk(key, tree);
    }
    stats.chunks = volume.chunk_count();

    // ── 2. 逐块网格化 + 产出实体（分区 = 静态观察者）──
    for block in lod_blocks(radius_blocks, max_lod, voxel_size_m) {
        let Some(raw) = extract_block_raw(
            block,
            &volume,
            voxel_size_f32,
            &mut interner,
            &mut wrapped,
            &mut neighbors,
        ) else {
            // 全空气块：不建实体，避免下发无意义 RAWVOXELS 载荷。
            stats.skipped_empty += 1;
            continue;
        };
        let id = allocator.allocate();
        let entity = commands
            .spawn(block_bundle(id, block, raw, voxel_size_f32))
            .id();
        index.insert(id, entity);
        blocks.0.insert((block.origin, block.lod), entity);

        stats.blocks += 1;
        stats.lod_blocks[usize::from(block.lod)] += 1;
    }

    // 地形根实体：体素容器以组件形式挂载
    //（「一岛 / 一船 / 一结构 = 一个 VoxVolume 组件」）；生成完成后一次性 attach。
    commands.spawn(volume);

    info!(
        "terrain: chunks={} blocks={} lod={:?} seed={:#x}",
        stats.chunks, stats.blocks, stats.lod_blocks, config.seed
    );
}

/// 运行期流式重划分：按观察者把 LOD 分区 diff 到 [TerrainBlocks]，增删块实体。
///
/// 输入优先级：若存在 [VoxelLodHandle] 且 observer / config 为 Some，则覆写
/// [VoxelLodObserver] / [VoxelLodConfig]（Godot 经 FFI 驱动）；否则用 Rust 侧资源
/// （crate::dev::perf 的移动观察者）。
///
/// 确定性：desired / current 都用 BTreeSet，增删按排序顺序处理，因此
/// StableIdAllocator 的分配顺序可复现。观察者只动一点时分区往往不变，diff 为空；
/// 只有 (observer, config) 变化才重算分区。
#[allow(clippy::too_many_arguments)]
fn stream_terrain_lod(
    mut commands: Commands,
    config: Res<TerrainConfig>,
    lod_handle: Option<Res<VoxelLodHandle>>,
    mut lod_config: ResMut<VoxelLodConfig>,
    mut observer: ResMut<VoxelLodObserver>,
    mut stream: ResMut<TerrainStreamState>,
    mut interner: ResMut<VoxelInterner>,
    mut wrapped: ResMut<WrappedBlockCache>,
    mut neighbors: ResMut<TerrainNeighborBlocks>,
    mut blocks: ResMut<TerrainBlocks>,
    mut stats: ResMut<TerrainStats>,
    mut allocator: ResMut<StableIdAllocator>,
    mut index: ResMut<StableEntityIndex>,
    volume_query: Query<&VoxVolume>,
) {
    // ── 1. 解析 FFI 覆写，并把配置夹到合法域 ──
    if let Some(handle) = lod_handle.as_deref() {
        if let Ok(runtime) = handle.0.lock() {
            if let Some(position) = runtime.observer {
                observer.position = position;
            }
            if let Some(runtime_config) = runtime.config {
                let sanitized = runtime_config.sanitized();
                if *lod_config != sanitized {
                    *lod_config = sanitized;
                }
            }
        }
    }
    let effective = lod_config.sanitized();
    if effective != *lod_config {
        *lod_config = effective;
    }
    let observer_position = observer.position;

    // ── 2. early-out：观察者与配置都没变则不动 ──
    if stream.last_config == Some(effective) && stream.last_observer == observer_position {
        return;
    }

    let Some(volume) = volume_query.iter().next() else {
        // 体素容器尚未生成：不推进状态，下一 tick 重试。
        return;
    };
    stream.last_config = Some(effective);
    stream.last_observer = observer_position;

    let voxel_size_m = voxel_size_meters();
    let voxel_size_f32 = DEFAULT_VOXEL_SIZE.to_num::<f32>();
    let radius_blocks = config.radius_blocks.max(0);
    let max_lod = config.max_lod.min(MAX_LOD);

    // ── 3. 期望分区（与 Startup 相同的 base cell 集合，只改 LOD 合并基准）──
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
    let desired_keys: BTreeSet<(ChunkKey, u8)> = desired
        .iter()
        .map(|block| (block.origin, block.lod))
        .collect();

    // ── 4. diff（BTreeSet 升序 = 确定性分配顺序）──
    let current_keys: BTreeSet<(ChunkKey, u8)> = blocks.0.keys().copied().collect();
    let removed: Vec<(ChunkKey, u8)> = current_keys.difference(&desired_keys).copied().collect();
    let added: Vec<(ChunkKey, u8)> = desired_keys.difference(&current_keys).copied().collect();

    for (origin, lod) in removed {
        if let Some(entity) = blocks.0.remove(&(origin, lod)) {
            index.remove_by_entity(entity);
            commands.entity(entity).despawn();
        }
        wrapped.invalidate_covered(interner.inner_mut(), origin);
        neighbors.0.invalidate_covered(interner.inner_mut(), origin);
    }

    for (origin, lod) in added {
        let block = TerrainBlock { origin, lod };
        let Some(raw) = extract_block_raw(
            block,
            volume,
            voxel_size_f32,
            &mut interner,
            &mut wrapped,
            &mut neighbors,
        ) else {
            continue;
        };
        let id = allocator.allocate();
        let entity = commands
            .spawn(block_bundle(id, block, raw, voxel_size_f32))
            .id();
        index.insert(id, entity);
        blocks.0.insert((origin, lod), entity);
    }

    // ── 5. 统计从 map 重算，避免漂移（chunks / skipped_empty 是生成期口径）──
    stats.blocks = blocks.0.len();
    stats.lod_blocks = [0; 4];
    for (_, lod) in blocks.0.keys() {
        stats.lod_blocks[usize::from(*lod)] += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use game_engine::identity::{StableEntityId, StableIdPlugin};
    use game_engine::voxel::{
        chunk_key, count_body_nodes, release_body, MaxDepth, VoxOpsBulkWrite, CHUNK_DEPTH,
    };

    /// 半径 0（1 个 XZ 单元）：debug 下生成最便宜，单元测试够用。
    ///
    /// 装配方插入 [TerrainConfig] 后由 [TerrainPlugin] 注册生成系统。
    fn terrain_app(radius_blocks: i32, max_lod: u8) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StableIdPlugin);
        app.insert_resource(TerrainConfig {
            radius_blocks,
            max_lod,
            ..TerrainConfig::default()
        });
        app.add_plugins(crate::voxel::GameVoxelPlugin);
        app.add_plugins(TerrainPlugin);
        app
    }

    /// 地形根实体：体素容器必须是**组件**，不是 Resource。
    fn volume_entity(world: &mut World) -> Entity {
        let mut query = world.query_filtered::<Entity, With<VoxVolume>>();
        query
            .iter(world)
            .next()
            .expect("地形根实体必须带 VoxVolume 组件")
    }

    /// 锁定边界：体素容器只以组件存在，不能退回全局 Resource。
    #[test]
    fn terrain_volume_is_a_component_not_a_resource() {
        let mut app = terrain_app(0, 0);
        app.update();
        let entity = volume_entity(app.world_mut());
        assert!(
            app.world().entity(entity).get::<VoxVolume>().is_some(),
            "体素容器必须是实体上的 VoxVolume 组件"
        );
    }

    #[test]
    fn build_terrain_produces_block_entities() {
        let mut app = terrain_app(0, 0);
        app.update();

        let stats = *app.world().resource::<TerrainStats>();
        let layers = y_band_layers(0) as usize;
        // 半径 0 → 1 个 XZ 单元 × y band；天空层（全空气）被跳过。
        assert_eq!(stats.chunks, 3 * 3 * layers, "半径 0 + margin 1");
        assert_eq!(stats.blocks + stats.skipped_empty, layers);
        assert!(stats.blocks > 0, "地表块必须产出几何");
        assert!(stats.skipped_empty > 0, "天空层必须被跳过");
        assert_eq!(stats.lod_blocks[0], stats.blocks);

        let blocks = app.world().resource::<TerrainBlocks>();
        assert_eq!(blocks.len(), stats.blocks);

        for ((origin, lod), entity) in blocks.iter() {
            assert_eq!(lod, 0);
            let entity_ref = app.world().entity(entity);
            assert!(entity_ref.get::<StableEntityId>().is_some());
            assert!(entity_ref.get::<Size>().is_some());
            assert!(entity_ref.get::<PresentedTransform>().is_some());

            let raw = entity_ref
                .get::<VoxChunkRaw>()
                .expect("mesh 块实体必须带 VoxChunkRaw");
            assert_eq!(raw.lod, 0);
            assert_eq!(raw.blocks.len(), RAW_VOXELS);
            assert!(raw_halo_has_solid_interior(&raw.blocks));

            // Transform 米坐标 = origin * 32 * voxel_size（不乘 2^lod）。
            let transform = entity_ref
                .get::<Transform>()
                .expect("块实体必须带 Transform");
            let want = block_origin_meters(origin, DEFAULT_VOXEL_SIZE.to_num::<f32>());
            assert!((transform.translation.x - want[0]).abs() < 1e-3);
            assert!((transform.translation.y - want[1]).abs() < 1e-3);
            assert!((transform.translation.z - want[2]).abs() < 1e-3);
        }
    }

    #[test]
    fn build_terrain_is_deterministic() {
        let mut first = terrain_app(0, 0);
        first.update();
        let mut second = terrain_app(0, 0);
        second.update();

        let raw_blocks = |app: &App| -> Vec<Vec<u8>> {
            let blocks = app.world().resource::<TerrainBlocks>();
            let mut out = Vec::new();
            for (_, entity) in blocks.iter() {
                let raw = app
                    .world()
                    .entity(entity)
                    .get::<VoxChunkRaw>()
                    .expect("VoxChunkRaw");
                out.push(raw.blocks.clone());
            }
            out
        };
        assert_eq!(raw_blocks(&first), raw_blocks(&second));
        assert_eq!(
            *first.world().resource::<TerrainStats>(),
            *second.world().resource::<TerrainStats>()
        );
    }

    #[test]
    fn release_terrain_returns_interner_to_baseline() {
        let mut app = terrain_app(0, 0);
        // 基线：GameVoxelPlugin 刚建好 interner、还没生成任何子块
        //（空哨兵本身可能算一个 node，所以不能断言 0）。
        let baseline = app
            .world()
            .resource::<VoxelInterner>()
            .inner()
            .alive_nodes();

        app.update();

        let alive_before = app
            .world()
            .resource::<VoxelInterner>()
            .inner()
            .alive_nodes();
        assert!(alive_before > baseline, "生成后必须有可达节点");
        let entity = volume_entity(app.world_mut());
        let volume = app
            .world()
            .entity(entity)
            .get::<VoxVolume>()
            .expect("地形根实体必须带 VoxVolume 组件");
        let volume_nodes =
            count_body_nodes(app.world().resource::<VoxelInterner>().inner(), volume);
        assert!(volume_nodes > 0);

        // 先清两个包装缓存（否则 root 引用把节点钉住），再 release_body。
        let world = app.world_mut();
        world.resource_scope::<VoxelInterner, _>(|world, mut interner| {
            {
                let mut wrapped = world.resource_mut::<WrappedBlockCache>();
                wrapped.clear(interner.inner_mut());
            }
            {
                let mut neighbors = world.resource_mut::<TerrainNeighborBlocks>();
                neighbors.0.clear(interner.inner_mut());
            }
            let entity = volume_entity(world);
            let mut entity_mut = world.entity_mut(entity);
            let mut volume = entity_mut
                .get_mut::<VoxVolume>()
                .expect("地形根实体必须带 VoxVolume 组件");
            let released = release_body(interner.inner_mut(), &mut volume);
            assert!(released > 0);
            assert_eq!(count_body_nodes(interner.inner(), &volume), 0);
            assert_eq!(
                interner.inner().alive_nodes(),
                baseline,
                "包装缓存与 body 释放后可达节点必须回到基线"
            );
        });
    }

    /// 大半径 LOD 场景：debug 下生成太慢，只在需要时手动跑。
    #[test]
    #[ignore = "半径 8 / LOD3 的生成在 debug 下很慢；release 手动运行"]
    fn terrain_with_lods_builds_coarse_blocks() {
        let mut app = terrain_app(8, 3);
        app.update();
        let stats = *app.world().resource::<TerrainStats>();
        assert!(stats.lod_blocks[1] > 0 || stats.lod_blocks[2] > 0);
    }

    // ─────────────── 运行期流式重划分（合成体素，不跑 worldgen）───────────────

    /// 合成实心体素 + 只挂 key 资源的 App：跳过 worldgen，测试快且完全确定。
    ///
    /// 直接调用私有系统 stream_terrain_lod，等于把流式逻辑从生成里隔离出来验证。
    fn streaming_app(radius: i32, max_lod: u8) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StableIdPlugin);
        app.add_plugins(crate::voxel::GameVoxelPlugin);
        app.init_resource::<WrappedBlockCache>()
            .init_resource::<TerrainNeighborBlocks>()
            .init_resource::<TerrainBlocks>()
            .init_resource::<TerrainStats>()
            .init_resource::<VoxelLodObserver>()
            .init_resource::<VoxelLodConfig>()
            .init_resource::<TerrainStreamState>();
        app.insert_resource(TerrainConfig {
            radius_blocks: radius,
            max_lod,
            ..TerrainConfig::default()
        });
        app.add_systems(Update, stream_terrain_lod);

        let world = app.world_mut();
        let mut volume = VoxVolume::new(DEFAULT_VOXEL_SIZE);
        world.resource_scope::<VoxelInterner, _>(|world, mut interner| {
            for key in chunk_keys_to_generate(radius, max_lod) {
                let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH as u8));
                tree.fill(interner.inner_mut(), 1);
                volume.insert_chunk(key, tree);
            }
            world.spawn(volume);
        });
        app
    }

    fn set_stream_observer(app: &mut App, position: [f32; 3]) {
        app.world_mut().resource_mut::<VoxelLodObserver>().position = position;
    }

    fn set_stream_config(app: &mut App, config: VoxelLodConfig) {
        *app.world_mut().resource_mut::<VoxelLodConfig>() = config;
    }

    fn stream_block_keys(app: &App) -> Vec<(ChunkKey, u8)> {
        app.world()
            .resource::<TerrainBlocks>()
            .iter()
            .map(|((origin, lod), _)| (origin, lod))
            .collect()
    }

    #[test]
    fn stream_terrain_lod_restreams_around_observer() {
        let mut app = streaming_app(1, 1);
        set_stream_config(
            &mut app,
            VoxelLodConfig {
                max_lod: 1,
                thresholds_m: [1.0, 2.0, 3.0],
            },
        );

        // 第一帧：观察者在原点 -> 母块 AABB 都含原点，距离 0，全部 LOD0。
        set_stream_observer(&mut app, [0.0, 0.0, 0.0]);
        app.update();
        let before = *app.world().resource::<TerrainStats>();
        assert!(before.blocks > 0);
        assert_eq!(before.lod_blocks[0], before.blocks, "近观察者必须全 LOD0");
        let stale = app
            .world()
            .resource::<TerrainBlocks>()
            .get(chunk_key(0, 0, 0), 0)
            .expect("(0,0,0) LOD0 块必须存在");

        // 观察者移到 +X 极远 -> 唯一完整母块 (0,0) 合并成 LOD1。
        set_stream_observer(&mut app, [10_000.0, 0.0, 0.0]);
        app.update();

        let stats = *app.world().resource::<TerrainStats>();
        assert!(
            stats.lod_blocks[1] > 0,
            "远端必须出现 LOD1：{:?}",
            stats.lod_blocks
        );
        assert_eq!(stats.blocks, app.world().resource::<TerrainBlocks>().len());
        assert_eq!(
            app.world().resource::<StableEntityIndex>().len(),
            stats.blocks,
            "实体索引必须与 TerrainBlocks 一致"
        );
        assert!(
            app.world().get_entity(stale).is_err(),
            "离开分区的旧块实体必须被 despawn"
        );
        for ((origin, lod), entity) in app.world().resource::<TerrainBlocks>().iter() {
            let entity_ref = app
                .world()
                .get_entity(entity)
                .unwrap_or_else(|_| panic!("块实体 {origin:?}/lod{lod} 必须存在"));
            assert!(entity_ref.get::<VoxChunkRaw>().is_some());
        }
    }

    fn run_observer_sequence() -> (Vec<(ChunkKey, u8)>, TerrainStats) {
        let mut app = streaming_app(1, 1);
        set_stream_config(
            &mut app,
            VoxelLodConfig {
                max_lod: 1,
                thresholds_m: [1.0, 2.0, 3.0],
            },
        );
        for position in [
            [0.0, 0.0, 0.0],
            [100.0, 0.0, 0.0],
            [100.0, 0.0, 200.0],
            [-100.0, 0.0, -50.0],
        ] {
            set_stream_observer(&mut app, position);
            app.update();
        }
        (
            stream_block_keys(&app),
            *app.world().resource::<TerrainStats>(),
        )
    }

    #[test]
    fn stream_terrain_lod_is_deterministic() {
        let first = run_observer_sequence();
        let second = run_observer_sequence();
        assert_eq!(first.0, second.0, "同一观察者序列必须产生相同分区键");
        assert_eq!(first.1, second.1, "统计必须一致");
    }

    #[test]
    fn stream_terrain_lod_keeps_registered_state_consistent() {
        // 走真实 TerrainPlugin 注册路径（radius 0 的 worldgen 便宜）。
        let mut app = terrain_app(0, 0);
        app.update();
        let before = *app.world().resource::<TerrainStats>();

        // radius 0 没有可合并的母块：换阈值 + 远观察者也不改分区。
        *app.world_mut().resource_mut::<VoxelLodConfig>() = VoxelLodConfig {
            max_lod: 0,
            thresholds_m: [1.0, 2.0, 3.0],
        };
        app.world_mut().resource_mut::<VoxelLodObserver>().position = [5_000.0, 0.0, 0.0];
        app.update();

        let after = *app.world().resource::<TerrainStats>();
        assert_eq!(before.blocks, after.blocks);
        assert_eq!(after.blocks, app.world().resource::<TerrainBlocks>().len());
        assert_eq!(
            app.world().resource::<StableEntityIndex>().len(),
            after.blocks
        );
    }
}
