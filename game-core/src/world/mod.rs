//! 游戏世界语义（极简）。
//!
//! 分层：本模块属于 game-core，依赖 game-engine。体素机制（Body / 地形 / LOD）
//! 已迁到 crate::voxel；本模块只保留「一个世界必须有的确定性状态」：
//! IslandRegistry / WorldSeed / WorldRng。
//!
//! 数值表在 crate::static_data::voxel（WorldTables）；体素插件在 crate::voxel。

pub mod island;
pub mod rng;
pub mod seed;

pub use island::{Biome, Island, IslandId, IslandRegistry};
pub use rng::WorldRng;
pub use seed::WorldSeed;
// BiomeId 定义在 static_data（游戏数值），这里重导出以满足 world 语义入口。
pub use crate::static_data::voxel::BiomeId;

use bevy::prelude::*;

/// 世界层插件（极简）：只注册 IslandRegistry / WorldSeed / WorldRng。
///
/// 体素机制与地形分别由 crate::voxel::GameVoxelPlugin / terrain::TerrainPlugin 装配。
pub struct WorldPlugin;

impl Plugin for WorldPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<IslandRegistry>()
            .init_resource::<WorldSeed>()
            .init_resource::<WorldRng>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_plugin_registers_resources() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(WorldPlugin);

        assert!(app.world().get_resource::<IslandRegistry>().is_some());
        assert!(app.world().get_resource::<WorldSeed>().is_some());
        assert!(app.world().get_resource::<WorldRng>().is_some());
        assert_eq!(app.world().resource::<WorldSeed>().0, 0);
    }

    #[test]
    fn world_rng_is_deterministic() {
        let mut a = WorldRng::from_seed(42);
        let mut b = WorldRng::from_seed(42);
        for _ in 0..64 {
            assert_eq!(a.next_u64(), b.next_u64());
        }

        let mut c = WorldRng::from_seed(1);
        let mut d = WorldRng::from_seed(2);
        assert_ne!(c.next_u64(), d.next_u64());

        let mut e = WorldRng::from_seed(7);
        let fixed = e.next_fixed();
        assert!(fixed >= game_engine::math::FixedPoint::from_num(0));
        assert!(fixed < game_engine::math::FixedPoint::from_num(1));
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
