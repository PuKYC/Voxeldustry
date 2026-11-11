//! CoreGame：game-core 的默认装配（GameModule<CoreSpec>）。
//!
//! 局域网 / 客户端 / 服务端是不同的 GameModule 组合；mod 可在其之上追加。

use std::collections::BTreeMap;

use bevy::prelude::*;

use game_engine::backend::GameModule;
use game_engine::modding::{DataRegistry, ModError, ModParamsPlugin, ParamTable};

use crate::mods::{default_core_mods, CoreMods};
use crate::spec::CoreSpec;
use crate::static_data::StaticDataPlugin;
use crate::world::{default_plugin_selection, resolve_plugins, PluginEntry, ProfileError};

/// 本游戏的默认插件组合。
pub struct CoreGame;

/// 世界装配失败：mod 应用失败，或 profile 插件解析失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorldBuildError {
    Mod(ModError),
    Profile(ProfileError),
}

impl WorldBuildError {
    /// 稳定错误码，供 Godot 侧按 code 分支处理。
    pub fn code(&self) -> &'static str {
        match self {
            WorldBuildError::Mod(error) => mod_error_code(error),
            WorldBuildError::Profile(error) => error.code(),
        }
    }
}

fn mod_error_code(error: &ModError) -> &'static str {
    match error {
        ModError::MissingDependency { .. } => "mod.missing_dependency",
        ModError::CircularDependency { .. } => "mod.circular_dependency",
        ModError::Conflict { .. } => "mod.conflict",
        ModError::ExclusiveGroup { .. } => "mod.exclusive_group",
        ModError::DuplicateId { .. } => "mod.duplicate_id",
        ModError::Param(_) => "mod.param",
        ModError::Actions(_) => "mod.actions",
    }
}

impl std::fmt::Display for WorldBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorldBuildError::Mod(error) => write!(f, "mod 装配失败：{error}"),
            WorldBuildError::Profile(error) => write!(f, "世界档插件解析失败：{error}"),
        }
    }
}

impl std::error::Error for WorldBuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WorldBuildError::Mod(error) => Some(error),
            WorldBuildError::Profile(error) => Some(error),
        }
    }
}

impl From<ModError> for WorldBuildError {
    fn from(error: ModError) -> Self {
        WorldBuildError::Mod(error)
    }
}

impl From<ProfileError> for WorldBuildError {
    fn from(error: ProfileError) -> Self {
        WorldBuildError::Profile(error)
    }
}

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
        let mod_set_hash = self.apply_mods(app, mods, overrides)?;
        add_default_plugins(app);
        Ok(mod_set_hash)
    }

    /// 按显式 profile 装配：先解析插件 id（依赖自动补齐），再应用 mod，
    /// 最后按拓扑序逐个装配插件。
    ///
    /// profile 解析在任何资源写入之前完成，因此未知 id / 缺依赖 / 环不会
    /// 留下半装配的 App。
    pub fn build_with_profile(
        &self,
        app: &mut App,
        mods: CoreMods,
        overrides: &BTreeMap<String, i64>,
        selection: &[&str],
    ) -> Result<u64, WorldBuildError> {
        let entries = resolve_plugins(selection)?;
        let mod_set_hash = self.apply_mods(app, mods, overrides)?;
        add_resolved_plugins(app, &entries);
        Ok(mod_set_hash)
    }

    /// 原有装配顺序：静态数据 → 参数表 → mod apply → 插件。
    /// 插件装配由调用方负责，保证「插件必须最后」不被破坏。
    fn apply_mods(
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

        Ok(mod_set_hash)
    }
}

/// 默认插件列表（不含 [StaticDataPlugin] 与 mod 应用）。
fn add_default_plugins(app: &mut App) {
    let entries = resolve_plugins(&default_plugin_selection())
        .expect("内置插件注册表必须满足依赖与默认 profile");
    add_resolved_plugins(app, &entries);
}

fn add_resolved_plugins(app: &mut App, entries: &[&PluginEntry]) {
    for entry in entries {
        (entry.build)(app);
    }
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
    fn build_with_profile_rejects_unknown_plugin_before_writing_anything() {
        let mut app = App::new();
        let error = CoreGame
            .build_with_profile(&mut app, CoreMods::new(), &BTreeMap::new(), &["nope"])
            .expect_err("未知插件 id 必须被拒绝");
        assert_eq!(error.code(), "profile.unknown_plugin");
        assert_eq!(
            error,
            WorldBuildError::Profile(ProfileError::UnknownPlugin {
                id: "nope".to_string()
            })
        );
        assert!(app.world().get_resource::<ParamTable>().is_none());
    }

    #[test]
    fn build_with_profile_only_installs_selected_plugins() {
        let mut app = App::new();
        CoreGame
            .build_with_profile(&mut app, CoreMods::new(), &BTreeMap::new(), &["world"])
            .expect("最小 profile 应能装配");
        assert!(app
            .world()
            .get_resource::<crate::world::IslandRegistry>()
            .is_some());
        assert!(app
            .world()
            .get_resource::<game_engine::voxel::VoxelInterner>()
            .is_none());
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
