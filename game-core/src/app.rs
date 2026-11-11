//! CoreGame：game-core 的默认装配（GameModule<CoreSpec>）。
//!
//! 局域网 / 客户端 / 服务端是不同的 GameModule 组合；mod 可在其之上追加。

use std::collections::BTreeMap;

use bevy::prelude::*;

use game_engine::backend::GameModule;
use game_engine::modding::{DataRegistry, ModError, ModParamsPlugin, ParamTable};

use crate::gameplay::{BlockEditPlugin, CameraPlugin, DeathPlugin, GameplayPlugin, MovementPlugin};
use crate::input::GameInputPlugin;
use crate::mods::{default_core_mods, CoreMods};
use crate::prediction::PredictionPlugin;
use crate::presentation::voxel_mesh::VoxelMeshRefreshPlugin;
use crate::presentation::{register_synced_components, PresentationPlugin};
use crate::spec::CoreSpec;
use crate::static_data::StaticDataPlugin;
use crate::voxel::terrain::{TerrainMode, TerrainPlugin};
use crate::voxel::GameVoxelPlugin;
use crate::world::WorldPlugin;
use game_engine::aoi::AoIPlugin;
use game_engine::identity::StableIdPlugin;
use game_engine::spatial::{Size, SpatialPlugin};

/// 本游戏的默认插件组合。
pub struct CoreGame;

impl CoreGame {
    /// 装配默认插件，并应用 mod 集合与参数覆盖。
    ///
    /// 参数表在 mod `install` 之前插入，因此 mod 可以读取自己的参数；
    /// 世界参数在 `PostStartup` 冻结。返回 mod_set_hash。
    pub fn build_with_mods(
        &self,
        app: &mut App,
        mods: CoreMods,
        overrides: &BTreeMap<String, i64>,
    ) -> Result<u64, ModError> {
        app.add_plugins(StaticDataPlugin);

        let manifests = mods.manifests();
        let params = ParamTable::resolve(&manifests, overrides)?;
        drop(manifests);
        app.insert_resource(params);
        app.add_plugins(ModParamsPlugin);

        let mut data = DataRegistry::new();
        let mod_set_hash = mods.apply(app, &mut data)?;
        app.insert_resource(data);

        add_default_plugins(app);
        Ok(mod_set_hash)
    }
}

/// 默认插件列表（不含 [StaticDataPlugin] 与 mod 应用）。
fn add_default_plugins(app: &mut App) {
    app.add_plugins(GameInputPlugin);
    app.add_plugins(PresentationPlugin);
    // 逻辑层基础：稳定 ID / 空间索引 / AOI。生产同样需要。
    app.add_plugins((
        StableIdPlugin,
        // 空间/AOI 一律用世界位置（GlobalTransform），逻辑位置写在 Transform 上。
        SpatialPlugin::<GlobalTransform, Size>::new(32.0),
        AoIPlugin::<GlobalTransform>::default(),
    ));
    // 玩法层装配。
    app.add_plugins((
        GameplayPlugin,
        MovementPlugin,
        DeathPlugin,
        CameraPlugin,
        BlockEditPlugin,
    ));
    // 体素世界层：游戏语义 / 数值 / 生成。v1 无执行。
    app.add_plugins(WorldPlugin);
    app.add_plugins(GameVoxelPlugin);
    app.add_plugins(TerrainPlugin);
    // 流式地形：只有装配方显式选了 TerrainMode::Streaming 才接整条管线。
    // Eager（默认）什么都不加，旧路径行为零变化（测试也直接调 CoreGame::build）。
    if app.world().get_resource::<TerrainMode>().copied() == Some(TerrainMode::Streaming) {
        app.add_plugins(crate::world::StreamingWorldPlugin);
    }
    // 脏块 -> RawVoxelGroups：编辑后同一渲染帧更新可见几何。
    app.add_plugins(VoxelMeshRefreshPlugin);
    // 预测 / 回滚绑定（单人：全部 Predicted、delay 0、无重演）。
    app.add_plugins(PredictionPlugin);
    register_synced_components(app);
}

impl GameModule<CoreSpec> for CoreGame {
    fn build(&self, app: &mut App) {
        self.build_with_mods(app, default_core_mods(), &BTreeMap::new())
            .expect("默认 mod 集合与参数表必须有效");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gameplay::BlockEditConfig;
    use game_engine::modding::{
        Mod, ModManifest, ParamError, ParamScope, ParamSpec, ParamType, WorldParamsFrozen,
    };
    use game_engine::voxel::{VoxelDirtySet, VoxelInterner};
    use std::time::Duration;

    /// 默认装配必须能完整跑 Update / FixedUpdate：编辑插件与脏块刷新插件都
    /// 依赖体素资源，缺失时会在系统参数校验阶段 panic。
    #[test]
    fn core_game_builds_and_ticks() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(Time::<Fixed>::from_hz(60.0));
        app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_millis(20),
        ));
        CoreGame.build(&mut app);
        app.finish();
        app.cleanup();

        assert!(app.world().get_resource::<VoxelInterner>().is_some());
        assert!(app.world().get_resource::<VoxelDirtySet>().is_some());
        assert!(app.world().get_resource::<BlockEditConfig>().is_some());

        for _ in 0..3 {
            app.update();
        }
    }

    static PARAM_MANIFEST: ModManifest = ModManifest {
        id: "test:param",
        version: 1,
        engine_api: 1,
        load_after: &[],
        load_after_optional: &[],
        conflicts: &[],
        exclusive_groups: &[],
        params: &[ParamSpec {
            key: "width",
            ty: ParamType::Int {
                min: 1,
                max: 9,
                step: 1,
            },
            default: 3,
            scope: ParamScope::World,
            group: "test",
            visible_if: None,
        }],
        actions: &[],
    };

    struct ParamMod;

    impl Mod<CoreSpec> for ParamMod {
        fn manifest(&self) -> &'static ModManifest {
            &PARAM_MANIFEST
        }
    }

    #[test]
    fn core_game_applies_mod_params_and_freezes_world_scope() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(Time::<Fixed>::from_hz(60.0));
        app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_millis(20),
        ));

        let mut overrides = BTreeMap::new();
        overrides.insert("test:param.width".to_string(), 7);
        let hash = CoreGame
            .build_with_mods(&mut app, CoreMods::new().push(ParamMod), &overrides)
            .expect("mod 装配应成功");
        assert_ne!(hash, 0, "mod_set_hash 必须非零");
        assert_eq!(
            app.world()
                .resource::<ParamTable>()
                .get_mod("test:param", "width")
                .unwrap()
                .value,
            7
        );

        app.finish();
        app.cleanup();
        app.update();

        assert!(app.world().resource::<WorldParamsFrozen>().0);
        let error = app
            .world_mut()
            .resource_mut::<ParamTable>()
            .set("test:param.width", 8, true)
            .unwrap_err();
        assert_eq!(
            error,
            ParamError::Frozen {
                key: "test:param.width".to_string()
            }
        );
    }
}
