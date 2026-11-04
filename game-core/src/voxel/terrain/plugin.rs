//! 地形插件的装配面：生成参数 [TerrainConfig]、模式 [TerrainMode] 与系统注册。
//!
//! [TerrainPlugin] 只在装配方**先**插入 [TerrainConfig] 时注册系统；缺省模式是
//! [TerrainMode::Eager]（与引入 TerrainMode 之前的行为逐位一致）。

use bevy::prelude::*;
use game_engine::presentation::voxel::WrappedBlockCache;

use crate::static_data::voxel::{BiomeId, DEFAULT_BIOME};
use crate::voxel::lod::VoxelLodConfig;
use crate::world::IslandRegistry;

use super::cache::invalidate_stale_terrain_caches;
use super::resources::{
    TerrainGeneratedBlocks, TerrainGroupMembers, TerrainGroups, TerrainNeighborBlocks,
    TerrainStats, TerrainStreamState, VoxelLodObserver,
};
use super::systems::{attach_terrain_body, build_terrain, stream_terrain_lod};

/// 地形生成参数（体素层资源）。装配方在 `add_plugins(GameVoxelPlugin)` **之前**
/// `insert_resource` 后，[TerrainPlugin] 就会注册地形生成系统；不插入则不生成。
///
/// 地形由独立插件 [TerrainPlugin] 装配（依赖 [GameVoxelPlugin](crate::voxel::GameVoxelPlugin)）。
/// 体素本身只以组件存在——`VoxVolume`（容器）+ `VoxChunkGroupRaw`（分组内每块的
/// 32³ + halo 原始体素）；本结构只是生成参数，不是体素数据。
///
/// `radius_blocks` 是 x/z 上围绕世界原点的基础子块半径（1 个基础子块 = 32
/// 体素 ≈ 14.4 m）。debug 构建生成很慢（实测约 150 ms/子块），所以默认值刻意小：
/// - [TerrainConfig::default]：radius 2 / max_lod 1（6×6×4 = 144 子块；debug ≈ 30 s，
///   release ≈ 2–4 s）；
/// - [TerrainConfig::demo]：radius 0 / max_lod 0（36 子块；debug ≈ 5 s）。
///
/// LOD 合并要半径 ≥ 5 才会越过 32 m 的第一档阈值，所以想看 LOD 切换要用
/// release 构建并调大半径（FFI：`start_bevy_voxel_perf(fixed_hz, runner_hz, radius, max_lod)`）。
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

/// 地形装配模式。缺省 [TerrainMode::Eager]（保持现有行为）。
///
/// 由装配方在 add_plugins(TerrainPlugin) **之前** 插入；不插入等价于 Eager，
/// 所以既有装配（含直接调用 CoreGame::build 的测试）行为零变化。
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum TerrainMode {
    /// 急切：Startup 生成全部块 + 全部渲染组（旧路径）。
    #[default]
    Eager,
    /// 流式：地形体由 [StreamingWorldPlugin](crate::world::StreamingWorldPlugin) 的
    /// 启动引导创建（有档走 load_world 恢复，无档/坏档才建**空**容器）；本插件
    /// 不再注册 Startup，块与渲染组交给驻留/流式管线。
    Streaming,
}

/// 地形插件（独立）：依赖 [GameVoxelPlugin](crate::voxel::GameVoxelPlugin)；
/// 装配方先插入 [TerrainConfig] 才注册系统。
pub struct TerrainPlugin;

impl Plugin for TerrainPlugin {
    fn build(&self, app: &mut App) {
        assert!(
            app.is_plugin_added::<super::super::GameVoxelPlugin>(),
            "TerrainPlugin 依赖 GameVoxelPlugin"
        );
        if app.world().get_resource::<TerrainConfig>().is_some() {
            // 缺省 Eager：旧装配不需要知道 TerrainMode 的存在，行为与以前完全一致。
            let mode = app
                .world()
                .get_resource::<TerrainMode>()
                .copied()
                .unwrap_or_default();
            register_systems(app, mode);
        }
    }
}

/// 注册地形生成系统与运行期资源：由 [TerrainPlugin]
/// 在 `TerrainConfig` 资源存在时调用。
///
/// 两条路径共享全部资源初始化，只在 Startup 生成系统与 Update 流式系统上分流：
/// - [TerrainMode::Eager]：Startup 生成全部块 + 全部组，Update 继续跑旧 LOD 重划分；
/// - [TerrainMode::Streaming]：本插件**不注册 Startup**：地形体由
///   [StreamingWorldPlugin](crate::world::StreamingWorldPlugin) 的引导系统创建
///   （有档读档、无档/坏档建空容器），块由驻留/流式管线装。这里也**不**注册
///   stream_terrain_lod——它和流式层会同时读写同一批块缓存，造成「旧系统把
///   流式装进来的块又覆盖/回滚」的竞态。
pub(crate) fn register_systems(app: &mut App, mode: TerrainMode) {
    app.init_resource::<TerrainGroups>()
        // 地形根实体要成为正式 island body（StableEntityId + IslandRegistry 条目），
        // 所以本插件保证注册表存在；WorldPlugin 已经 init 过时这里是 no-op。
        .init_resource::<IslandRegistry>()
        .init_resource::<TerrainGroupMembers>()
        .init_resource::<TerrainNeighborBlocks>()
        // 表现层网格机制不注册插件；WrappedBlockCache 由唯一消费者 core 侧 init。
        .init_resource::<WrappedBlockCache>()
        .init_resource::<TerrainGeneratedBlocks>()
        .init_resource::<TerrainStats>()
        .init_resource::<VoxelLodConfig>()
        .init_resource::<VoxelLodObserver>()
        .init_resource::<TerrainStreamState>();

    // 编辑后包装缓存必须失效：否则下一次 LOD 流式重划分（或流式层重提取）
    // 会用旧树重新 extract，把玩家刚改的方块「回滚」掉。脏集由 mesh 刷新
    // 系统消费，所以这里只 peek，并且必须排在那之前。
    // 每个模式**只注册一次**（重复注册会让同一系统每帧跑两遍）。
    match mode {
        TerrainMode::Eager => {
            // 观察者写入者在 FixedUpdate；流式重划分放 Update 以便同 tick 看到新位置。
            // build_terrain 用 Commands 生成地形根实体；紧随其后的 exclusive system
            // 会在运行前 flush 这些命令，因此能拿到那个实体（见 attach_terrain_body）。
            app.add_systems(Startup, (build_terrain, attach_terrain_body).chain())
                .add_systems(
                    Update,
                    invalidate_stale_terrain_caches
                        .before(crate::presentation::voxel_mesh::refresh_dirty_voxel_meshes)
                        .before(stream_terrain_lod),
                )
                .add_systems(Update, stream_terrain_lod);
        }
        TerrainMode::Streaming => {
            // Startup 不在这里注册：地形体由 StreamingWorldPlugin 的引导系统创建
            // （有档 load_world 恢复、无档/坏档建空容器）。两边都注册会在读档成功
            // 后多出一个空地形体，违背「有档不生成」。
            //
            // 也不注册 stream_terrain_lod：它和流式层会同时读写同一批块缓存，
            // 造成「旧系统把流式装进来的块又覆盖/回滚」的竞态。
            app.add_systems(
                Update,
                invalidate_stale_terrain_caches
                    .before(crate::presentation::voxel_mesh::refresh_dirty_voxel_meshes),
            );
        }
    }
}
