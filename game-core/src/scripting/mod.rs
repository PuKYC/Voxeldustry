//! BMS 脚本层（feature = "scripting"）：Lua 脚本作为 ECS 附件。
//!
//! 设计边界（《Voxeldustry 插件化架构方案》脚本层）：
//! - 脚本只做「声明组件 / 写系统体」，热路径仍在 Rust；
//! - 脚本来源由 mod 包提供（`LoadedPackage.scripts`），启动冷路径一次性转成
//!   [`ScriptAsset`] 并挂到同一个实体上；
//! - BMS 的 `ScriptValue` 目前没有网络 / 存档编码路径，脚本组件不参与同步与存档，
//!   跨端一致性仍由 Rust 侧组件承担（见 docs/项目架构/05-表现管线.md）。
use bevy::app::{App, Plugin, Startup};
use bevy::asset::{AssetPlugin, Assets};
use bevy::ecs::prelude::*;
use bevy::log::debug;
use bevy_mod_scripting::prelude::*;

/// 一个待加载的脚本源（文本已在内存中，来源是 mod 包或调用方）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptSource {
    /// 稳定名字，仅用于日志与诊断（例如 `"example/bobber.lua"`）。
    pub name: String,
    /// Lua 源码。
    pub source: String,
}

/// 脚本源集合：由装配方在 `build_with_profile` 之前插入。
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct ScriptSources {
    /// 按包拓扑顺序排列的脚本。
    pub scripts: Vec<ScriptSource>,
}

impl ScriptSources {
    /// 追加一个脚本。
    pub fn push(&mut self, name: impl Into<String>, source: impl Into<String>) {
        self.scripts.push(ScriptSource {
            name: name.into(),
            source: source.into(),
        });
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.scripts.is_empty()
    }
}

/// BMS 脚本层插件：所有脚本在 Startup 时载入并 spawn 成 [`ScriptComponent`]。
#[derive(Default)]
pub struct ScriptingPlugin;

impl Plugin for ScriptingPlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<AssetPlugin>() {
            app.add_plugins(AssetPlugin::default());
        }
        app.add_plugins(BMSPlugin);
        app.init_resource::<ScriptSources>();
        app.add_systems(Startup, spawn_sources);
    }
}

/// 把所有脚本源转成资产，挂到同一个实体上。
///
/// 实体只承载脚本；脚本需要操作世界时通过 BMS 的 ECS 绑定拿 `world`。
fn spawn_sources(
    mut commands: Commands,
    mut assets: ResMut<Assets<ScriptAsset>>,
    sources: Res<ScriptSources>,
) {
    if sources.scripts.is_empty() {
        return;
    }
    let mut handles = Vec::with_capacity(sources.scripts.len());
    for script in &sources.scripts {
        let asset = ScriptAsset::new(script.source.clone()).with_language(Language::Lua);
        let handle = assets.add(asset);
        debug!("scripting: 载入脚本 {}", script.name);
        handles.push(handle);
    }
    commands.spawn(ScriptComponent::new(handles));
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::MinimalPlugins;

    fn test_app(sources: ScriptSources) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(sources);
        app.add_plugins(ScriptingPlugin);
        app
    }

    #[test]
    fn sources_become_script_assets_and_one_component() {
        let mut sources = ScriptSources::default();
        sources.push("demo/bobber.lua", "function on_update() end");
        sources.push("demo/second.lua", "function on_update() end");
        let mut app = test_app(sources);
        app.update();

        assert_eq!(app.world().resource::<Assets<ScriptAsset>>().len(), 2);
        let mut query = app.world_mut().query::<&ScriptComponent>();
        let components: Vec<_> = query.iter(app.world()).collect();
        assert_eq!(components.len(), 1, "所有脚本挂在同一个实体上");
        assert_eq!(components[0].len(), 2);
    }

    #[test]
    fn empty_sources_spawn_nothing() {
        let mut app = test_app(ScriptSources::default());
        app.update();
        let mut query = app.world_mut().query::<&ScriptComponent>();
        assert_eq!(query.iter(app.world()).count(), 0);
        assert_eq!(app.world().resource::<Assets<ScriptAsset>>().len(), 0);
    }
}
