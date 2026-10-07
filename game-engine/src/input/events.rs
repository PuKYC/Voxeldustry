//! 由输入系统**确定性派生**的瞬时事件。
//!
//! 这些事件只用于「本逻辑帧的通知」，**不能替代 `InputBuffer`**：
//! 事件是「发生一次、下一帧就消失」的语义，无法按历史 tick 查询、无法重放
//! （L9）。

use bevy::prelude::*;

use crate::identity::StableEntityId;

use super::actions::ActionId;

/// 某个动作在本 tick 内被按下。
#[derive(Message, Clone, Copy, Debug)]
pub struct InputActionPressed {
    pub entity: StableEntityId,
    pub action: ActionId,
    pub tick: u32,
}

/// 某个动作在本 tick 内被松开。
#[derive(Message, Clone, Copy, Debug)]
pub struct InputActionReleased {
    pub entity: StableEntityId,
    pub action: ActionId,
    pub tick: u32,
}

/// 交互请求。
///
/// 注意：这只是「意图」，**不代表交互成功**。是否有电、是否堵塞、
/// 是否被元素浓度干扰，由玩法系统判断（输入层不决定玩法结果）。
#[derive(Message, Clone, Copy, Debug)]
pub struct InteractRequest {
    pub actor: StableEntityId,
    /// 交互目标；`0` 表示「未指定目标，由玩法系统做视线/范围查询」。
    pub target: StableEntityId,
    pub action: ActionId,
    pub tick: u32,
}

/// 交互目标的占位值。
pub const NO_TARGET: StableEntityId = StableEntityId(0);
