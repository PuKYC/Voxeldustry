//! 游戏模块装配接缝 与 headless runner。
//!
//! 本模块位于 game-engine（通用确定性 ECS 机制层），向上只暴露
//! [`GameModule`] 与 [`run_headless`]，向下依赖 Bevy、[`ClientBridge`]
//! 与表现管线，不依赖 game-core 或 Godot。
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
///
/// 实现者负责把该游戏的插件与系统装进 [`App`]；runner 只调用一次
/// [`GameModule::build`]，不关心内部注册了哪些系统。
pub trait GameModule<S: GameSpec>: Send + Sync + 'static {
    /// 把该游戏的插件与系统装配进 `app`。
    ///
    /// 调用时 `app` 已注入 [`ClientBridge`] 句柄与基础插件。
    fn build(&self, app: &mut App);
}

/// 把 [`GameModule`] 当成普通插件挂载。
///
/// 泛型参数 `S` 用于匹配 [`GameSpec`]，`M` 是具体玩法模块。
pub struct GameModulePlugin<S: GameSpec, M: GameModule<S>> {
    module: M,
    _spec: PhantomData<S>,
}

impl<S: GameSpec, M: GameModule<S>> GameModulePlugin<S, M> {
    /// 用给定模块构造插件。
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
    /// 暂停模拟；runner 会挂起输入并回送 [`BevyLifecycleMsg::Paused`]。
    Pause,
    /// 恢复模拟；runner 会恢复输入、回送 `Resumed` 并重置帧时钟。
    Resume,
    /// 请求退出；runner 补跑最后一帧（插入 [`BackendShutdown`]）后停止。
    Shutdown,
    /// 透传给具体游戏的自定义指令；runner 原样回送为 `Custom` 生命周期消息。
    Custom(String),
}

/// 退出存档标记：runner 收到 [`BevyControlMsg::Shutdown`] 后插入本资源，
/// 再跑**最后一帧** `App::update()`，让世界层把「已装块」落盘。
///
/// runner 自己不存盘：存盘属于世界语义，由 game-core 的 WorldSavePlugin 在
/// `Last` 里消费本资源并移除它（一次性触发，见 `save_on_shutdown`）。
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct BackendShutdown;

/// Bevy -> Godot 的生命周期与状态反馈。
#[derive(Debug, Clone)]
pub enum BevyLifecycleMsg {
    /// runner 已完成装配，开始主循环。
    Started,
    /// 模拟已暂停。
    Paused,
    /// 模拟已恢复。
    Resumed,
    /// runner 捕获到 panic，附带错误文本。
    Error(String),
    /// 透传自定义指令。
    Custom(String),
    /// runner 已停止；`0` 表示正常退出，非零表示异常退出。
    Stopped(i64),
}

/// 跨线程传递的消息载体。
#[derive(Debug, Clone)]
pub struct FromBevy {
    /// 创建该 runner 的会话 id，用于丢弃过期会话的消息。
    pub session: u64,
    /// 生命周期或状态载荷。
    pub payload: BevyLifecycleMsg,
}

/// runner 配置（与具体游戏无关）。
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// runner 主循环目标频率（Hz）；非正值按 1 Hz 处理。
    pub runner_hz: f64,
    /// Bevy `Fixed` 调度的固定步长频率（Hz）。
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
    /// 控制指令接收端。
    pub ctrl_rx: Receiver<BevyControlMsg>,
    /// 生命周期消息发送端。
    pub life_tx: Sender<FromBevy>,
    /// 当前会话 id。
    pub session: u64,
    /// 绑定 `S` 的编译期标记。
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
///
/// 装配 `app`、启动主循环并阻塞到退出。正常退出返回 [`ExitCode::SUCCESS`]，
/// panic 或异常退出返回 [`ExitCode::FAILURE`]。
///
/// 生命周期消息通过 `life_tx` 回送；runner 自身不存盘，只在退出前补跑一帧，
/// 由世界层消费 [`BackendShutdown`] 落盘。
///
/// # 参数
/// - `config`: 主循环与固定步长频率。
/// - `module`: 具体玩法装配单元。
/// - `bridge`: 与 Godot 交换的纯数据句柄。
/// - `ctrl_rx`: 控制指令接收端。
/// - `life_tx`: 生命周期消息发送端。
/// - `session`: 会话 id，随每条消息回传。
pub fn run_headless<S: GameSpec, M: GameModule<S>>(
    config: EngineConfig,
    module: M,
    bridge: ClientBridge<S>,
    ctrl_rx: Receiver<BevyControlMsg>,
    life_tx: Sender<FromBevy>,
    session: u64,
) -> ExitCode {
    let mut app = build_app(&config, module, &bridge, ctrl_rx, life_tx.clone(), session);

    let _ = life_tx.send(FromBevy {
        session,
        payload: BevyLifecycleMsg::Started,
    });

    // 自定义 Runner。bridge 在装配后不再使用，直接移入闭包，省一次 clone。
    app.set_runner(move |app| run_app_loop(app, &config, &bridge));

    // 运行并捕获 Panic（GDExtension 多线程环境下必须经由 Channel 报错）。
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| app.run()));
    finish_run(result, session, life_tx)
}

/// 装配后台 App：注入跨边界句柄、基础插件、玩法模块、时间步长与控制通道。
///
/// 所有资源都必须在 `add_plugins` 之前注入，因为 `init_resource` 不会覆盖
/// 已存在的资源。返回的 `App` 尚未安装 runner。
fn build_app<S: GameSpec, M: GameModule<S>>(
    config: &EngineConfig,
    module: M,
    bridge: &ClientBridge<S>,
    ctrl_rx: Receiver<BevyControlMsg>,
    life_tx: Sender<FromBevy>,
    session: u64,
) -> App {
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
        life_tx,
        session,
        _spec: PhantomData,
    });

    app
}

/// 后台 App 的主循环：处理控制指令、按目标帧率推进 `App::update`。
///
/// 收到 [`BevyControlMsg::Shutdown`] 时插入 [`BackendShutdown`] 并补跑最后一帧，
/// 然后返回 [`AppExit::Success`]；`App` 自身请求退出时同样正常返回。
fn run_app_loop<S: GameSpec>(
    mut app: App,
    config: &EngineConfig,
    bridge: &ClientBridge<S>,
) -> AppExit {
    let channels = app.world().resource::<BackendChannels<S>>().clone();
    let target_frame_time = Duration::from_secs_f64(1.0 / config.runner_hz.max(1.0));

    let mut paused = false;
    let mut exit_requested = false;
    let mut last_update = Instant::now();
    app.finish();
    app.cleanup();

    loop {
        drain_control_messages(
            &channels,
            bridge,
            &mut paused,
            &mut exit_requested,
            &mut last_update,
        );

        if exit_requested {
            // 退出前补跑一帧：世界层（game-core 的 WorldSavePlugin）只有在这一帧
            // 的 Last 里才有机会把「已装块」落盘。FixedUpdate 依赖累计时间，
            // 这一帧可能一步都不走，所以退出保存不能挂在 FixedUpdate 上。
            app.insert_resource(BackendShutdown);
            app.update();
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
}

/// 排空控制通道并更新暂停/退出状态，同时回送对应生命周期消息。
///
/// 恢复模拟时会重置 `last_update`，避免把暂停时长算进下一帧的步长。
fn drain_control_messages<S: GameSpec>(
    channels: &BackendChannels<S>,
    bridge: &ClientBridge<S>,
    paused: &mut bool,
    exit_requested: &mut bool,
    last_update: &mut Instant,
) {
    while let Ok(msg) = channels.ctrl_rx.try_recv() {
        match msg {
            BevyControlMsg::Shutdown => *exit_requested = true,
            BevyControlMsg::Pause => {
                *paused = true;
                bridge.set_input_suspended(true);
                let _ = channels.life_tx.send(FromBevy {
                    session: channels.session,
                    payload: BevyLifecycleMsg::Paused,
                });
            }
            BevyControlMsg::Resume => {
                *paused = false;
                bridge.set_input_suspended(false);
                let _ = channels.life_tx.send(FromBevy {
                    session: channels.session,
                    payload: BevyLifecycleMsg::Resumed,
                });
                *last_update = Instant::now();
            }
            BevyControlMsg::Custom(cmd) => {
                let _ = channels.life_tx.send(FromBevy {
                    session: channels.session,
                    payload: BevyLifecycleMsg::Custom(cmd),
                });
            }
        }
    }
}

/// 把 `App::run` 的结果翻译为进程退出码，并回送停止/错误生命周期消息。
///
/// panic 载荷优先按 `String` / `&str` 提取文本，其他类型统一回退为
/// `"Unknown panic in Bevy backend"`。
fn finish_run(
    result: std::thread::Result<AppExit>,
    session: u64,
    life_tx: Sender<FromBevy>,
) -> ExitCode {
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
