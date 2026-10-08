//! 后台 Bevy App 的 game-core 装配。
//!
//! runner / 通道 / 生命周期在 game-engine::backend；这里只提供本游戏的
//! 配置、demo / perf / 体素地形组合与 FFI 入口。

use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use crossbeam_channel::{Receiver, Sender};

use game_engine::backend::{self, EngineConfig, GameModule};

use crate::app::CoreGame;
use crate::dev::demo::DemoPlugin;
use crate::spec::{ClientBridge, CoreSpec};
use crate::voxel::terrain::{TerrainConfig, VoxelLodHandle, VoxelLodRuntime};

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
    /// Godot -> Bevy 的 LOD 运行期句柄（observer / thresholds / max_lod）。
    /// Some 时插入 [VoxelLodHandle]，运行期流式系统读它。
    pub voxel_lod: Option<Arc<Mutex<VoxelLodRuntime>>>,
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
            voxel_lod: None,
        }
    }
}

/// CoreGame + 可选 demo / perf / 体素地形的装饰模块。
struct CoreGameModule {
    enable_demo: bool,
    perf_entity_count: Option<usize>,
    terrain: Option<TerrainConfig>,
    terrain_perf: bool,
    voxel_lod: Option<Arc<Mutex<VoxelLodRuntime>>>,
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
        CoreGame.build(app);
        if self.terrain_perf {
            let config = *app.world().resource::<TerrainConfig>();
            app.add_plugins(crate::dev::perf::VoxelPerfPlugin::new(config));
        }
        if let Some(perf_entity_count) = self.perf_entity_count {
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
            voxel_lod: config.voxel_lod,
        },
        bridge,
        ctrl_rx,
        life_tx,
        session,
    );
}
