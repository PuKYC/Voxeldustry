//! game-core 给引擎的类型绑定：`CoreSpec` 与客户端桥特化。
//!
//! 跨边界装配点的 game-core 特化。机制（输入暂存 / 表现单槽 / 事件队列 /
//! 语义注册表句柄）在 game-engine 的 `ClientBridge<S>`；这里把游戏绑定为
//! [`CoreSpec`]，并给出 [`ClientBridge`] 别名。

use game_engine::bridge::ClientBridge as GenericClientBridge;
use game_engine::spec::GameSpec;

use crate::input::actions::CoreActions;
use crate::presentation::semantics::SemanticRegistry;
use crate::presentation::{PresentationEvent, SyncPayload};

/// 本游戏的 GameSpec 实例。
pub struct CoreSpec;

impl GameSpec for CoreSpec {
    type Actions = CoreActions;
    type Payload = SyncPayload;
    type Event = PresentationEvent;
    type Semantics = SemanticRegistry;
    const FIXED_HZ: u32 = 60;
}

/// game-core 的客户端桥。
pub type ClientBridge = GenericClientBridge<CoreSpec>;
