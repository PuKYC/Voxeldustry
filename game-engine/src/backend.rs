//! 游戏模块装配接缝（M6）与 headless runner。
//!
//! Bevy 调度不支持运行期替换已注册系统，所以「换一整块玩法」的正确做法是
//! 组合一个新的 GameModule，而不是从调度里抠系统。
//!
//! runner 与 Godot 完全解耦：只通过 ClientBridge 的纯数据通道交换信息。

use std::marker::PhantomData;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use bevy::app::AppExit;
use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;
use crossbeam_channel::{Receiver, Sender};

use crate::bridge::ClientBridge;
use crate::presentation::pipeline::PresentationRuntime;
use crate::spec::GameSpec;

/// 一个游戏的默认装配单元。
pub trait GameModule<S: GameSpec>: Send + Sync + 'static {
    fn build(&self, app: &mut App);
}

/// 把 GameModule 当成普通插件挂载。
pub struct GameModulePlugin<S: GameSpec, M: GameModule<S>> {
    module: M,
    _spec: PhantomData<S>,
}

impl<S: GameSpec, M: GameModule<S>> GameModulePlugin<S, M> {
    pub fn new(module: M) -> Self {
        Self {
            module,
            _spec: PhantomData,
        }
    }
}

impl<S: GameSpec, M: GameModule<S>> Plugin for GameModulePlugin<S, M> {
    fn build(&self, app: &mut App) {
        self.module.build(app);
    }
}

/// Godot -> Bevy 的控制指令。
#[derive(Debug, Clone)]
pub enum BevyControlMsg {
    Pause,
    Resume,
    Shutdown,
    Custom(String),
}

/// Bevy -> Godot 的生命周期与状态反馈。
#[derive(Debug, Clone)]
pub enum BevyLifecycleMsg {
    Started,
    Paused,
    Resumed,
    Error(String),
    Custom(String),
    Stopped(i64),
}

/// 跨线程传递的消息载体。
#[derive(Debug, Clone)]
pub struct FromBevy {
    pub session: u64,
    pub payload: BevyLifecycleMsg,
}

/// runner 配置（与具体游戏无关）。
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub runner_hz: f64,
    pub fixed_hz: f64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            runner_hz: 60.0,
            fixed_hz: 60.0,
        }
    }
}

/// 注入到 Bevy World 中的通道资源。
#[derive(Resource)]
pub struct BackendChannels<S: GameSpec> {
    pub ctrl_rx: Receiver<BevyControlMsg>,
    pub life_tx: Sender<FromBevy>,
    pub session: u64,
    _spec: PhantomData<S>,
}

impl<S: GameSpec> Clone for BackendChannels<S> {
    fn clone(&self) -> Self {
        Self {
            ctrl_rx: self.ctrl_rx.clone(),
            life_tx: self.life_tx.clone(),
            session: self.session,
            _spec: PhantomData,
        }
    }
}

/// 后台 Bevy App 的通用主入口。
#[allow(clippy::too_many_arguments)]
pub fn run_headless<S: GameSpec, M: GameModule<S>>(
    config: EngineConfig,
    module: M,
    bridge: ClientBridge<S>,
    ctrl_rx: Receiver<BevyControlMsg>,
    life_tx: Sender<FromBevy>,
    session: u64,
) -> ExitCode {
    let mut app = App::new();

    // 1. 先注入跨边界句柄（必须在 add_plugins 之前，init_resource 不覆盖已存在的）。
    app.insert_resource(bridge.input.clone());
    app.insert_resource(bridge.slot.clone());
    app.insert_resource(bridge.events.clone());
    app.insert_resource(bridge.semantics.clone());
    app.insert_resource(PresentationRuntime::with_session(session));

    // 2. 基础插件。
    app.add_plugins(MinimalPlugins);

    // 3. 游戏装配。
    app.add_plugins(GameModulePlugin::<S, M>::new(module));

    // 4. 时间与物理步长。
    app.insert_resource(Time::<Fixed>::from_hz(config.fixed_hz));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::ZERO));

    // 5. 控制通道。
    app.insert_resource(BackendChannels::<S> {
        ctrl_rx,
        life_tx: life_tx.clone(),
        session,
        _spec: PhantomData,
    });

    let _ = life_tx.send(FromBevy {
        session,
        payload: BevyLifecycleMsg::Started,
    });

    // 6. 自定义 Runner。
    let runner_bridge = bridge.clone();
    app.set_runner(move |mut app: App| {
        let channels = app.world().resource::<BackendChannels<S>>().clone();
        let target_frame_time = Duration::from_secs_f64(1.0 / config.runner_hz.max(1.0));

        let mut paused = false;
        let mut exit_requested = false;
        let mut last_update = Instant::now();

        app.finish();
        app.cleanup();

        loop {
            while let Ok(msg) = channels.ctrl_rx.try_recv() {
                match msg {
                    BevyControlMsg::Shutdown => exit_requested = true,
                    BevyControlMsg::Pause => {
                        paused = true;
                        runner_bridge.set_input_suspended(true);
                        let _ = channels.life_tx.send(FromBevy {
                            session: channels.session,
                            payload: BevyLifecycleMsg::Paused,
                        });
                    }
                    BevyControlMsg::Resume => {
                        paused = false;
                        runner_bridge.set_input_suspended(false);
                        let _ = channels.life_tx.send(FromBevy {
                            session: channels.session,
                            payload: BevyLifecycleMsg::Resumed,
                        });
                        last_update = Instant::now();
                    }
                    BevyControlMsg::Custom(cmd) => {
                        let _ = channels.life_tx.send(FromBevy {
                            session: channels.session,
                            payload: BevyLifecycleMsg::Custom(cmd),
                        });
                    }
                }
            }

            if exit_requested {
                break;
            }

            if paused {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }

            let now = Instant::now();
            let delta = now - last_update;
            last_update = now;

            {
                let mut strategy = app.world_mut().resource_mut::<TimeUpdateStrategy>();
                *strategy = TimeUpdateStrategy::ManualDuration(delta);
            }

            app.update();

            if app.should_exit().is_some() {
                break;
            }

            let elapsed = Instant::now() - now;
            if elapsed < target_frame_time {
                std::thread::sleep(target_frame_time - elapsed);
            }
        }

        AppExit::Success
    });

    // 7. 运行并捕获 Panic（GDExtension 多线程环境下必须经由 Channel 报错）。
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        app.run();
    }));

    match result {
        Ok(_) => {
            let _ = life_tx.send(FromBevy {
                session,
                payload: BevyLifecycleMsg::Stopped(0),
            });
            ExitCode::SUCCESS
        }
        Err(payload) => {
            let err_msg = if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else {
                "Unknown panic in Bevy backend".to_string()
            };

            let _ = life_tx.send(FromBevy {
                session,
                payload: BevyLifecycleMsg::Error(err_msg),
            });
            let _ = life_tx.send(FromBevy {
                session,
                payload: BevyLifecycleMsg::Stopped(1),
            });
            ExitCode::FAILURE
        }
    }
}
