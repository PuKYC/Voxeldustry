//! 体素世界层（游戏内容）：Body / 停靠 / LOD / 地形。
//!
//! 世界层机制在 game_engine::voxel，网格机制是在 game_engine::presentation::voxel
//! 的纯函数库（不注册插件）；本模块提供 GameVoxelPlugin（注册 LodPolicy + 引擎体素
//! 插件）并重导出引擎类型给 FFI（godot-client-ext 经 game_core::voxel::* 取用）。
//! 地形生成是独立插件 terrain::TerrainPlugin，依赖 GameVoxelPlugin。
//!
//! LOD 分两部分：[VoxelLodConfig] 是运行期可调阈值（Godot 经 FFI 写），
//! [lod_for_distance] / [LodPolicy] 是只读的默认映射；terrain 的运行期流式系统
//! 按观察者位置用前者重划分 mesh 块（表现唯一通道仍是 RawVoxels 载荷）。

pub mod body;
pub mod dock;
pub mod lod;
pub mod terrain;

pub use body::{AutomatonTag, Body, BodyKind, IslandTag, ShipTag, StructureTag};
pub use dock::DockRecord;
pub use lod::{lod_for_distance, LodPolicy, VoxelLodConfig, MAX_LOD};

// 面向 FFI 的引擎体素类型重导出。godot-client-ext 只依赖 game-core，
// 需要经 game_core::voxel::* 取到这些机制类型。
pub use game_engine::voxel::{Attachment, VoxVolume, VoxelChangeBuffer, VoxelEdit, VoxelPlugin};

use bevy::prelude::*;

/// 本游戏的体素装配插件：注册 LOD 策略资源与引擎体素机制。
pub struct GameVoxelPlugin;

impl Plugin for GameVoxelPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<LodPolicy>()
            .add_plugins(game_engine::voxel::VoxelPlugin::default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::static_data::voxel::DEFAULT_BIOME;
    use game_engine::math::FixedPoint;

    #[test]
    fn game_voxel_plugin_spawns_bodies_and_applies_lod_policy() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(GameVoxelPlugin);

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
        let mut ships = app.world_mut().query::<(&Body, &ShipTag)>();
        assert_eq!(ships.iter(app.world()).count(), 1);
        let mut structures = app.world_mut().query::<(&Body, &StructureTag)>();
        assert_eq!(structures.iter(app.world()).count(), 1);
        let mut automata = app.world_mut().query::<(&Body, &AutomatonTag)>();
        assert_eq!(automata.iter(app.world()).count(), 1);

        let policy = *app.world().resource::<LodPolicy>();
        assert_eq!(policy.lod_for_distance(FixedPoint::from_bits(0)).lod(), 0);
        assert_eq!(
            policy.lod_for_distance(FixedPoint::from_num(10_000)).lod(),
            MAX_LOD
        );
    }
}
