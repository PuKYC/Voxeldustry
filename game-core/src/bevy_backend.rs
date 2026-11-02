//! 后台 Bevy App 的 game-core 装配。
//!
//! runner / 通道 / 生命周期在 game-engine::backend；这里只提供本游戏的
//! 配置、demo / perf / 体素地形组合与 FFI 入口。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use crossbeam_channel::{Receiver, Sender};

use game_engine::backend::{self, EngineConfig, GameModule};

use crate::app::CoreGame;
use crate::dev::demo::DemoPlugin;
use crate::spec::{ClientBridge, CoreSpec};
use crate::voxel::terrain::{TerrainConfig, TerrainMode, VoxelLodHandle, VoxelLodRuntime};
use crate::world::SaveRoot;

// 消息类型由引擎定义，重导出保持 godot 侧路径不变。
pub use game_engine::backend::{BevyControlMsg, BevyLifecycleMsg, FromBevy};

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
        }
    }
}

/// CoreGame + 可选 demo / perf / 体素地形的装饰模块。
///
/// 装配顺序：先插入 TerrainConfig / VoxelLodHandle / [TerrainMode] / [SaveRoot]，
/// 再 CoreGame.build —— TerrainPlugin 在 build 时读 TerrainMode 决定生成策略，
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
        CoreGame.build(app);
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
        }
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
}
