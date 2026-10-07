//! 游戏世界语义与数值。
//!
//! 分层：本模块属于 game-core，依赖 game-engine（含 voxel feature），
//! **不依赖 godot**。职责边界：
//! - 游戏语义：Body / BodyKind / 标签 / Island / DockRecord（本模块）；
//! - 游戏数值：static_data::voxel（体素边长 / 调色板 / 材质 / 生物群系 / LOD 阈值）；
//! - 生成：定点整数噪声 + 按 ChunkKey 惰性生成（generation）；
//! - LOD 策略：距离 -> Lod（lod）；
//! - 地形接入：`Startup` 生成一圈地形块并产出 `VoxelMeshBlock` 实体（terrain，路径 B）；
//! - WorldPlugin：把注册表 / 策略 / 静态表挂成 Bevy 资源。
//!
//! v1 不执行 AI、不做功能方块；VoxVolume 等引擎机制由 game-engine 提供。
//! 地形生成不是独立插件：装配方在 `WorldPlugin` 之前插入 `TerrainConfig` 即启用；
//! 体素数据只以组件存在（`VoxVolume` 容器 / `VoxelMeshBlock` 每块矩形流）。

pub mod body;
pub mod dock;
pub mod generation;
pub mod island;
pub mod lod;
pub mod terrain;

pub use body::{AutomatonTag, Body, BodyKind, IslandTag, ShipTag, StructureTag};
pub use dock::DockRecord;
pub use generation::{
    content_hash, generate_chunk, generate_island, generate_island_lazy, new_interner,
    WorldGenerator, WorldSeed, CHUNK_SIZE, DEFAULT_INTERNER_BUDGET_BYTES,
};
pub use island::{Biome, Island, IslandId, IslandRegistry};
pub use lod::{lod_for_distance, LodPolicy, MAX_LOD};
pub use terrain::{blocks, TerrainBlocks, TerrainConfig, TerrainNeighborBlocks, TerrainStats};
// BiomeId 定义在 static_data（游戏数值），这里重导出以满足 world 语义入口。
pub use crate::static_data::voxel::BiomeId;

use bevy::prelude::*;
use game_engine::math::FixedPoint;
use game_engine::voxel::VoxelPlugin;

use crate::static_data::voxel;

/// 世界静态表资源句柄：只读访问 static_data::voxel 中的游戏数值。
///
/// 把静态表做成 Resource 是为了让系统通过注入拿到一致入口，
/// 也方便将来在测试 / mod 中替换数值（v1 仍读 const 表）。
#[derive(Resource, Default, Clone, Copy, Debug)]
pub struct WorldTables;

impl WorldTables {
    pub fn block(&self, id: u8) -> Option<&'static voxel::BlockDef> {
        voxel::block_def(id)
    }

    pub fn material(&self, id: voxel::MaterialId) -> Option<&'static voxel::MaterialDef> {
        voxel::material_def(id)
    }

    pub fn biome(&self, id: BiomeId) -> Option<&'static voxel::BiomeDef> {
        voxel::biome_def(id)
    }

    pub fn block_is_solid(&self, id: u8) -> bool {
        voxel::is_solid_block(id)
    }

    /// 默认体素边长（写进 VoxVolume.voxel_size）。
    pub fn voxel_size(&self) -> FixedPoint {
        voxel::DEFAULT_VOXEL_SIZE
    }

    /// LOD 距离阈值（米，定点）。
    pub fn lod_thresholds(&self) -> &'static [FixedPoint; 3] {
        &voxel::LOD_DISTANCE_THRESHOLDS
    }
}

/// 世界层插件：注册 IslandRegistry / WorldSeed / WorldTables / LodPolicy，
/// 以及（可选）体素地形生成。
///
/// 体素地形**不是独立插件**：装配方在 `add_plugins(WorldPlugin)` 之前
/// `insert_resource(TerrainConfig { .. })`，本插件才注册生成系统；体素数据
/// 只以组件存在（`VoxVolume` 容器 + `VoxelMeshBlock` 每块矩形流）。
///
/// 保持最小：v1 不注册 AI / 生成调度系统；生成由调用方按 ChunkKey 惰性触发。
pub struct WorldPlugin;

impl Plugin for WorldPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<IslandRegistry>()
            .init_resource::<WorldSeed>()
            .init_resource::<WorldTables>()
            .init_resource::<LodPolicy>();
        // 引擎侧体素机制：共享 interner / 脏标记 / 内存指标。
        app.add_plugins(VoxelPlugin::default());
        // 地形生成：装配方给了 TerrainConfig 才启用（生产 / demo / perf 各自选半径）。
        if app.world().get_resource::<TerrainConfig>().is_some() {
            terrain::register_systems(app);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::static_data::voxel::DEFAULT_BIOME;
    use game_engine::voxel::Attachment;

    #[test]
    fn world_plugin_registers_resources() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(WorldPlugin);

        assert!(app.world().get_resource::<IslandRegistry>().is_some());
        assert!(app.world().get_resource::<WorldSeed>().is_some());
        assert!(app.world().get_resource::<WorldTables>().is_some());
        assert!(app.world().get_resource::<LodPolicy>().is_some());

        assert_eq!(app.world().resource::<WorldSeed>().0, 0);
        assert_eq!(
            app.world().resource::<WorldTables>().voxel_size(),
            crate::static_data::voxel::DEFAULT_VOXEL_SIZE
        );
    }

    #[test]
    fn body_spawn_integration() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(WorldPlugin);

        app.world_mut()
            .spawn((Body::island(true, DEFAULT_BIOME), IslandTag));
        app.world_mut().spawn((Body::ship(), ShipTag));
        app.world_mut()
            .spawn((Body::structure(Attachment::World), StructureTag));
        app.world_mut().spawn((Body::automaton(), AutomatonTag));

        let mut bodies = app.world_mut().query::<&Body>();
        assert_eq!(bodies.iter(app.world()).count(), 4);

        let mut islands = app.world_mut().query::<(&Body, &IslandTag)>();
        assert_eq!(islands.iter(app.world()).count(), 1);

        let mut automatons = app.world_mut().query::<(&Body, &AutomatonTag)>();
        assert_eq!(automatons.iter(app.world()).count(), 1);

        let policy = *app.world().resource::<LodPolicy>();
        assert_eq!(policy.lod_for_distance(FixedPoint::from_bits(0)).lod(), 0);
    }

    #[test]
    fn island_registry_resource_tracks_entries() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(WorldPlugin);

        app.world_mut()
            .resource_mut::<IslandRegistry>()
            .insert(IslandId(1), Island::default());

        assert_eq!(app.world().resource::<IslandRegistry>().len(), 1);
    }
}
