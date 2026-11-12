//! 后台 Bevy App 的 game-core 装配。
//!
//! runner / 通道 / 生命周期在 game-engine::backend；这里只提供本游戏的
//! 配置、demo / perf / 体素地形组合与 FFI 入口。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use crossbeam_channel::{Receiver, Sender};

use game_engine::backend::{self, EngineConfig, GameModule, ModuleBuildFailure};
use game_engine::input::ResolvedActions;
use game_engine::residency::MemorySoftLimitHandle;

use crate::app::CoreGame;
use crate::dev::demo::DemoPlugin;
use crate::mods::{default_core_mods, CoreMods};
use crate::spec::{ClientBridge, CoreSpec};
use crate::static_data::ObjectTablesHandle;
use crate::voxel::terrain::{TerrainConfig, TerrainMode, VoxelLodHandle, VoxelLodRuntime};
use crate::world::streaming::ObserverDrivenStreamingPlayer;
use crate::world::{
    default_plugin_selection, intern_plugin_selection, SaveRoot, WorldLoadRejectionHandle,
};

// 消息类型由引擎定义，重导出保持 godot 侧路径不变。
pub use game_engine::backend::{BevyControlMsg, BevyLifecycleMsg, FromBevy};
// 内存软上限句柄同理：FFI 只依赖 game-core，不直接依赖 game-engine。
pub use game_engine::residency::{
    MemorySoftLimitHandle as MemoryLimitHandle, MemorySoftLimitOverride,
};

/// 后台 Bevy 实例的配置参数。
#[derive(Debug, Clone)]
pub struct BevyBackendConfig {
    pub runner_hz: f64,
    pub fixed_hz: f64,
    /// 是否挂载演示插件（crate::dev::demo）。
    pub enable_demo: bool,
    /// 挂载性能测试场景并生成该数量的实体（Some 时取代 demo）。
    pub perf_entity_count: Option<usize>,
    /// 体素地形参数（Some 时启用；Godot 体素 demo / perf 用）。地形由 TerrainPlugin 装配：
    /// 配置在 `GameVoxelPlugin` / `TerrainPlugin` 之前插入，体素数据只以组件存在（`VoxVolume` 等）。
    pub terrain: Option<TerrainConfig>,
    /// 体素性能场景：true 时额外挂 VoxelPerfPlugin（移动观察者压表现链路）。
    pub terrain_perf: bool,
    /// 游戏内容测试场景：静态体素平台 + 带碰撞玩家（见 `dev::gameplay_test`）。
    pub gameplay_test: bool,
    /// 体素 demo 观察者：在地形上生成可编辑的玩家 + 相机（见 `dev::voxel_viewer`）。
    pub voxel_viewer: bool,
    /// Godot -> Bevy 的 LOD 运行期句柄（observer / thresholds / max_lod）。
    /// Some 时插入 [VoxelLodHandle]，运行期流式系统读它。
    pub voxel_lod: Option<Arc<Mutex<VoxelLodRuntime>>>,
    /// 地形装配模式：Eager（默认，启动时生成全部块）/ Streaming（只建空容器，
    /// 块由驻留 / 流式管线按请求加载）。
    ///
    /// 选 Streaming 时 CoreGame::build 会额外挂
    /// [crate::world::StreamingWorldPlugin]（驻留 + 世界区块 + 存档）；Eager 下
    /// 一个都不装，所以既有入口的行为完全不变。
    pub terrain_mode: TerrainMode,
    /// 世界存档根目录。Some 时启用存档层：SaveRoot 会让
    /// [crate::world::StreamingWorldPlugin] 追加引擎的 SavePlugin（建 SaveStore、
    /// 起后台存档 IO 线程）与 WorldSavePlugin（手动 SaveNow + 自动存档），
    /// 并在启动时按该目录读档。
    ///
    /// None 时一条存盘路径都不接：ChunkPersistence 保持 NoStore，
    /// 启动不读、运行不写，完全不碰磁盘（headless / 单测）。
    pub save_dir: Option<PathBuf>,
    /// 内存软上限的运行期句柄（Godot 写、Bevy 读）。
    ///
    /// Some 时把 [MemorySoftLimitOverride] 插进 World，覆盖
    /// [game_engine::residency::ResidencyConfig::memory_soft_limit]；
    /// None 时行为与未加该字段前完全一致（沿用插件配置）。
    pub memory_soft_limit: Option<MemorySoftLimitHandle>,
    /// 静态数据表的跨线程句柄（Godot 写重载请求 / 读当前表；Bevy 在 tick 边界应用）。
    ///
    /// Some 时在 CoreGame.build 之前把它插进 World；[StaticDataPlugin]
    /// 会复用这个句柄发布启动表与后续重载，Godot 侧因此能看到同一份数据。
    pub object_tables: Option<ObjectTablesHandle>,
    /// 编译期 native mod 集合。默认 [`default_core_mods`]；Godot 入口无法从
    /// GDScript 构造 `dyn Mod`，需要注入 mod 时由 Rust 侧在启动前写入。
    pub mods: CoreMods,
    /// 参数覆盖（全 key `<mod_id>.<key>`）。在 mod `install` 之前解析校验；
    /// 世界参数随后在 `PostStartup` 冻结。默认空表。
    pub param_overrides: BTreeMap<String, i64>,
    /// 世界插件选择（注册表 id）。None = 默认 profile（default_plugin_selection）。
    /// Some 时空列表表示「一个可选插件都不装」，不是「用默认」；未知 id 在启动阶段
    /// 结构化失败（ModuleBuildFailure + bev_error），不会 panic。
    pub plugins: Option<Vec<String>>,
    /// 读档拒绝信息的跨线程句柄（Bevy 写、Godot 读）。
    ///
    /// Some 时插进 World：[WorldLoadRejectionHandle] 会被
    /// `bootstrap_streaming_world` 写入 `save.*` 拒绝详情，Godot 侧在
    /// [`BevyLifecycleMsg::Error`] 之后、甚至 `bev_stopped` 之后都能读同一份值。
    /// None 时拒绝只留在 World 的 [`crate::world::WorldLoadRejection`] 资源里
    /// （headless / 单测）。
    pub world_load_rejection: Option<WorldLoadRejectionHandle>,
}

impl Default for BevyBackendConfig {
    fn default() -> Self {
        Self {
            runner_hz: 60.0,
            fixed_hz: 60.0,
            enable_demo: true,
            perf_entity_count: None,
            terrain: None,
            terrain_perf: false,
            gameplay_test: false,
            voxel_viewer: false,
            voxel_lod: None,
            // 两个新开关都取「零变化」的默认值：旧入口不需要知道它们存在。
            terrain_mode: TerrainMode::Eager,
            save_dir: None,
            memory_soft_limit: None,
            object_tables: None,
            mods: default_core_mods(),
            param_overrides: BTreeMap::new(),
            plugins: None,
            world_load_rejection: None,
        }
    }
}

/// CoreGame + 可选 demo / perf / 体素地形的装饰模块。
///
/// 装配顺序：先插入 TerrainConfig / VoxelLodHandle / [TerrainMode] / [SaveRoot]，
/// 再 CoreGame.build_with_mods —— TerrainPlugin 在 build 时读 TerrainMode 决定生成策略，
/// StreamingWorldPlugin 在同一遍 build 里读 SaveRoot 决定是否接存档层；
/// 顺序反了两个资源都还是默认值（Eager / 不落盘），而且不会有任何报错。
struct CoreGameModule {
    enable_demo: bool,
    perf_entity_count: Option<usize>,
    terrain: Option<TerrainConfig>,
    terrain_perf: bool,
    gameplay_test: bool,
    voxel_viewer: bool,
    voxel_lod: Option<Arc<Mutex<VoxelLodRuntime>>>,
    terrain_mode: TerrainMode,
    save_dir: Option<PathBuf>,
    memory_soft_limit: Option<MemorySoftLimitHandle>,
    object_tables: Option<ObjectTablesHandle>,
    mods: CoreMods,
    param_overrides: BTreeMap<String, i64>,
    plugins: Option<Vec<String>>,
    world_load_rejection: Option<WorldLoadRejectionHandle>,
}

impl GameModule<CoreSpec> for CoreGameModule {
    fn build(&self, app: &mut App) {
        // 地形参数必须在 CoreGame（进而 GameVoxelPlugin / TerrainPlugin）之前插入才会被注册。
        let terrain = match (self.terrain, self.terrain_perf) {
            (Some(config), _) => Some(config),
            (None, true) => Some(TerrainConfig::default()),
            // 纯 demo 默认给极小地形（radius 0 / lod 0），debug 生成也快。
            (None, false) if self.enable_demo => Some(TerrainConfig::demo()),
            (None, false) => None,
        };
        if let Some(config) = terrain {
            app.insert_resource(config);
        }
        // LOD 句柄必须在 CoreGame.build 之前插入，terrain 运行期系统才会读到它。
        if let Some(handle) = &self.voxel_lod {
            app.insert_resource(VoxelLodHandle(handle.clone()));
        }
        // 装配顺序是硬约束：TerrainPlugin 在 build 时读 TerrainMode 决定注册哪套
        // 生成系统，StreamingWorldPlugin 在同一遍 build 里读 SaveRoot 决定是否接
        // 存档层；两者都由 CoreGame.build 装配，所以这两个资源必须**先**插入。
        // 顺序反了不会报错，只会被静默当成默认值（Eager / 不落盘）。
        app.insert_resource(self.terrain_mode);
        if let Some(dir) = &self.save_dir {
            app.insert_resource(SaveRoot(dir.clone()));
        }
        // 内存软上限覆盖：与 VoxelLodHandle 同一模式（Godot 主线程写共享句柄）。
        if let Some(handle) = &self.memory_soft_limit {
            app.insert_resource(MemorySoftLimitOverride::from_handle(handle.clone()));
        }
        // 静态表句柄必须早于 CoreGame.build：StaticDataPlugin 的 init_resource
        // 不会覆盖已有资源，启动表会直接发布进 Godot 持有的这份句柄。
        if let Some(handle) = &self.object_tables {
            app.insert_resource(handle.clone());
        }
        // 拒绝句柄必须早于 CoreGame.build：bootstrap_streaming_world 在 Startup
        // 里就要写它，晚于 build 插入就赶不上这一帧。
        if let Some(handle) = &self.world_load_rejection {
            app.insert_resource(handle.clone());
        }
        // dev 观察者入口（流式 + voxel_viewer，即 start_bevy_streaming）没有输入
        // 位移系统：玩家出生后只会受重力，驻留锚点固定在出生点，世界流式永远
        // 不卸载块。这条件装配一条「相机 / 观察者驱动玩家」的 dev 通道，让 Godot
        // 侧推 set_voxel_observer 就能驱动驻留。真实玩法入口（demo / gameplay_test）
        // 有自己的控制器，不能开：否则相机一推玩家就被瞬移。
        if self.terrain_mode == TerrainMode::Streaming
            && self.voxel_viewer
            && !self.enable_demo
            && !self.gameplay_test
        {
            app.insert_resource(ObserverDrivenStreamingPlayer);
        }
        // None 走默认 profile；Some 先归一 id（未知 id 结构化失败），再交给 build_with_profile。
        let selection = match self.plugins.as_deref() {
            None => default_plugin_selection(),
            Some(ids) => match intern_plugin_selection(ids) {
                Ok(selection) => selection,
                Err(error) => {
                    app.insert_resource(ModuleBuildFailure {
                        code: error.code().to_string(),
                        message: error.to_string(),
                    });
                    return;
                }
            },
        };
        // 装配失败不 panic：把稳定 code + 中文说明放进资源，由 runner 在
        // 发 Started 之前拦截并回传 Godot，然后以失败退出。
        if let Err(error) =
            CoreGame.build_with_profile(app, self.mods.clone(), &self.param_overrides, &selection)
        {
            app.insert_resource(ModuleBuildFailure {
                code: error.code().to_string(),
                message: error.to_string(),
            });
            return;
        }
        // 发布全局动作表：Godot 线程拿不到 `World`，动作名翻译与动作表查询
        // 都从这里取，否则 mod 动作在客户端永远「查无此名」。
        if let Some(resolved) = app.world().get_resource::<ResolvedActions>() {
            crate::input::publish_active_actions(resolved);
        }
        if self.terrain_perf {
            let config = *app.world().resource::<TerrainConfig>();
            app.add_plugins(crate::dev::perf::VoxelPerfPlugin::new(config));
        }
        if self.voxel_viewer {
            // 体素 demo：地形已由 TerrainPlugin 生成，这里只补一个落在地表上的
            // 玩家 + 相机，让方块编辑系统有射线来源。
            #[cfg(feature = "physics")]
            app.add_plugins(crate::dev::voxel_viewer::VoxelViewerPlugin);
        } else if self.gameplay_test {
            // 测试场景自带平台 / 玩家 / 相机，不再叠加 demo 或性能实体。
            #[cfg(feature = "physics")]
            app.add_plugins(crate::dev::gameplay_test::GameplayTestPlugin);
        } else if let Some(perf_entity_count) = self.perf_entity_count {
            app.add_plugins(crate::dev::perf::PerfDemoPlugin {
                entity_count: perf_entity_count,
            });
        } else if self.enable_demo {
            app.add_plugins(DemoPlugin);
        }
    }
}

/// 后台 Bevy App 的主入口函数，由 godot-client-ext/src/bevy_runtime_node.rs 在新线程中调用。
pub fn run_bevy_backend(
    ctrl_rx: Receiver<BevyControlMsg>,
    life_tx: Sender<FromBevy>,
    session: u64,
    config: BevyBackendConfig,
    bridge: ClientBridge,
) {
    let _ = backend::run_headless::<CoreSpec, _>(
        EngineConfig {
            runner_hz: config.runner_hz,
            fixed_hz: config.fixed_hz,
        },
        CoreGameModule {
            enable_demo: config.enable_demo,
            perf_entity_count: config.perf_entity_count,
            terrain: config.terrain,
            terrain_perf: config.terrain_perf,
            gameplay_test: config.gameplay_test,
            voxel_viewer: config.voxel_viewer,
            voxel_lod: config.voxel_lod,
            terrain_mode: config.terrain_mode,
            save_dir: config.save_dir,
            memory_soft_limit: config.memory_soft_limit,
            object_tables: config.object_tables,
            mods: config.mods,
            param_overrides: config.param_overrides,
            plugins: config.plugins,
            world_load_rejection: config.world_load_rejection,
        },
        bridge,
        ctrl_rx,
        life_tx,
        session,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{resolve_plugins, ActivePlugins};
    use game_engine::modding::{Mod, ModManifest, ParamScope, ParamSpec, ParamTable, ParamType};
    use std::time::Duration;

    /// 只关心装配顺序：其它装饰字段全关，避免把 demo / perf / 观察者一起拉起来。
    fn module(terrain_mode: TerrainMode, save_dir: Option<PathBuf>) -> CoreGameModule {
        CoreGameModule {
            enable_demo: false,
            perf_entity_count: None,
            terrain: Some(TerrainConfig::default()),
            terrain_perf: false,
            gameplay_test: false,
            voxel_viewer: false,
            voxel_lod: None,
            terrain_mode,
            save_dir,
            memory_soft_limit: None,
            object_tables: None,
            mods: default_core_mods(),
            param_overrides: BTreeMap::new(),
            plugins: None,
            world_load_rejection: None,
        }
    }

    static BACKEND_PARAM: ModManifest = ModManifest {
        id: "test:backend",
        version: 1,
        engine_api: 1,
        load_after: &[],
        load_after_optional: &[],
        conflicts: &[],
        exclusive_groups: &[],
        params: &[ParamSpec {
            key: "radius",
            ty: ParamType::Int {
                min: 1,
                max: 16,
                step: 1,
            },
            default: 4,
            scope: ParamScope::World,
            group: "test",
            visible_if: None,
        }],
        actions: &[],
    };

    struct BackendParamMod;

    impl Mod<CoreSpec> for BackendParamMod {
        fn manifest(&self) -> &'static ModManifest {
            &BACKEND_PARAM
        }
    }

    /// 后端配置必须真的把 mod 集合与参数覆盖送进 CoreGame 装配，
    /// 而不是只把字段存下来。
    #[test]
    fn backend_module_applies_mods_and_param_overrides() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(Time::<Fixed>::from_hz(60.0));
        app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_millis(20),
        ));

        let mut module = module(TerrainMode::Eager, None);
        module.mods = CoreMods::new().push(BackendParamMod);
        module
            .param_overrides
            .insert("test:backend.radius".to_string(), 9);
        module.build(&mut app);

        assert_eq!(
            app.world()
                .resource::<ParamTable>()
                .get_mod("test:backend", "radius")
                .unwrap()
                .value,
            9,
            "BevyBackendConfig 的 mods / param_overrides 必须进入 ParamTable"
        );
    }

    /// 静态表句柄复用：Godot 在起线程前持有 ObjectTablesHandle，
    /// CoreGameModule 必须先把它塞进 App，StaticDataPlugin 的 init_resource
    /// 才会沿用它；否则 Godot 的 current() 永远是 None。
    #[test]
    fn object_tables_handle_is_reused_by_static_data_plugin() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(Time::<Fixed>::from_hz(60.0));
        app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_millis(20),
        ));

        let handle = crate::static_data::ObjectTablesHandle::default();
        let mut module = module(TerrainMode::Eager, None);
        module.object_tables = Some(handle.clone());
        module.build(&mut app);
        app.world_mut().run_schedule(Startup);

        let published = handle.current().expect("启动后句柄必须已发布静态表");
        assert_eq!(published.generation(), 1);
        assert_eq!(
            app.world()
                .resource::<crate::static_data::ObjectTables>()
                .generation(),
            1,
            "StaticDataPlugin 必须写入同一个 App 资源"
        );
    }

    /// 装配顺序回归：TerrainMode / SaveRoot 必须赶在 CoreGame.build 之前进 App，
    /// 否则 TerrainPlugin 注册 Eager 那套系统、StreamingWorldPlugin 保持 NoStore，
    /// 而且全程静默 —— 只能靠断言资源把这类「装错了但不报错」挡住。
    ///
    /// 只走装配（[CoreGameModule::build]），不调 run_bevy_backend，因此没有
    /// runner 线程；Streaming + SaveRoot 会由 StreamingWorldPlugin 建出 SaveStore，
    /// 那个 IO 线程全程空闲、随 App 析构时 join。
    #[test]
    fn streaming_config_installs_save_root_and_mode() {
        let save_dir = std::env::temp_dir().join("game-core-streaming-assembly");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(Time::<Fixed>::from_hz(60.0));
        app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_millis(20),
        ));

        module(TerrainMode::Streaming, Some(save_dir.clone())).build(&mut app);

        assert_eq!(
            app.world().get_resource::<TerrainMode>(),
            Some(&TerrainMode::Streaming),
            "TerrainMode 必须在 CoreGame.build 之前插入，否则地形按 Eager 装配"
        );
        let root = app
            .world()
            .get_resource::<SaveRoot>()
            .expect("save_dir 为 Some 时必须插入 SaveRoot，否则存档层不会被启用");
        assert_eq!(root.0, save_dir);
    }

    /// 只做装配、不跑调度：插件选择测试只需要看 App 里留下了什么资源。
    fn assembly_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(Time::<Fixed>::from_hz(60.0));
        app
    }

    /// plugins = None 必须与旧行为逐项一致：默认 profile 的解析结果（装配顺序）。
    ///
    /// `ActivePlugins` 记的是 resolve 之后的序列，不是注册表顺序——例如
    /// `block_edit` 会被排到它依赖的 world / game_voxel / camera / gameplay 之后。
    #[test]
    fn plugins_none_uses_default_profile() {
        let mut app = assembly_app();
        let mut module = module(TerrainMode::Eager, None);
        module.plugins = None;
        module.build(&mut app);

        let expected: Vec<&str> = resolve_plugins(&default_plugin_selection())
            .expect("默认 profile 必须可解析")
            .iter()
            .map(|entry| entry.id)
            .collect();
        assert_eq!(
            app.world().resource::<ActivePlugins>().ids(),
            &expected[..],
            "None 必须装配默认 profile（default_plugin_selection 的解析结果）"
        );
    }

    /// 显式选择只装选中的插件（及其依赖）；顺序以 resolve 结果为准，不写死。
    #[test]
    fn plugins_some_overrides_selection() {
        let mut app = assembly_app();
        let mut module = module(TerrainMode::Eager, None);
        module.plugins = Some(vec!["input".to_string(), "presentation".to_string()]);
        module.build(&mut app);

        let active = app.world().resource::<ActivePlugins>();
        assert_eq!(active.ids().len(), 2, "只应装配选中的两个插件");
        assert!(active.ids().contains(&"input"));
        assert!(active.ids().contains(&"presentation"));
        assert!(
            app.world().get_resource::<ModuleBuildFailure>().is_none(),
            "合法选择不应留下 ModuleBuildFailure"
        );
    }

    /// 选择叶子插件时，requires 必须自动补齐（block_edit -> world/game_voxel/camera/gameplay）。
    #[test]
    fn plugins_some_auto_adds_requires() {
        let mut app = assembly_app();
        let mut module = module(TerrainMode::Eager, None);
        module.plugins = Some(vec!["block_edit".to_string()]);
        module.build(&mut app);

        let active = app.world().resource::<ActivePlugins>();
        for required in ["block_edit", "world", "game_voxel", "camera", "gameplay"] {
            assert!(
                active.ids().contains(&required),
                "REQUIRES_BLOCK_EDIT 要求装配 {required}，实际：{:?}",
                active.ids()
            );
        }
    }

    /// 未知 id 结构化失败：只留 ModuleBuildFailure，不 panic，也不写入 ActivePlugins。
    #[test]
    fn unknown_plugin_fails_structurally() {
        let mut app = assembly_app();
        let mut module = module(TerrainMode::Eager, None);
        module.plugins = Some(vec!["no_such_plugin".to_string()]);
        module.build(&mut app);

        let failure = app
            .world()
            .get_resource::<ModuleBuildFailure>()
            .expect("未知插件必须留下 ModuleBuildFailure");
        assert_eq!(failure.code, "profile.unknown_plugin");
        assert!(
            app.world().get_resource::<ActivePlugins>().is_none(),
            "解析失败必须零写入（不能有 ActivePlugins）"
        );
    }
}
