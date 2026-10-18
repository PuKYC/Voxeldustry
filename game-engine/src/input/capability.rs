//! 输入能力 / 禁用机制的**通用部分**。
//!
//! 具体能力通道组件（`MoveChannel` / `JumpChannel` …）与「组件 − 禁用 = 可用」
//! 的求解属于游戏侧；引擎只提供与具体通道无关的可用性载体与禁用掩码。

use bevy::prelude::*;

use super::control::InputSourceId;

/// 禁用掩码：状态效果 / 环境对通道的削减。
///
/// 用位掩码而不是「每种禁用一个组件」，是为了避免能力组件数量爆炸。
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct InputDisabled(pub u64);

/// 输入能力与可用性载体。
///
/// - `mask`：当前**可用**的动作位（= 能力组件并集经 `InputDisabled` / `Stunned`
///   / `Rooted` 削减后的结果，保持旧名，`write_resolved_input` 继续用它过滤）；
/// - `present`：**能力存在**的动作位并集（只按「有没有挂对应的通道组件」求，
///   不受禁用状态影响，供 UI 区分「没有这个能力」与「有能力但当前不可用」）；
/// - `look`：视角轴是否开放（`LookChannel` 存在）。视角是**独立轴**，不占
///   动作位，也不参与 `mask` 的位运算；`write_resolved_input` 用它决定是否
///   清零 `PlayerInput.look_*`；
/// - `source` / `local`：给表现 / UI 的控制关系信息（`ControlledBy.source`、
///   是否挂 `LocalPlayer`），不参与输入过滤。
///
/// 每 tick 由游戏侧的可用性求解系统重算；`version` 便于排查
/// 「能力为什么没生效」。
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputAvailability {
    pub mask: u64,
    pub present: u64,
    pub source: InputSourceId,
    pub local: bool,
    pub look: bool,
    pub version: u32,
}

/// 「能力通道标记组件」的通用样板生成器。
///
/// 形如 `simple_channel!(/// 文档  Name)`：可选 doc 属性 + 一个标识符，
/// 生成一个 `#[derive(Component, Clone, Copy, Debug, Default)]` 的单元 struct。
///
/// 引擎只提供这个与具体通道无关的骨架；到底需要哪些通道（移动 / 跳跃 /
/// 工具 ……）以及它们的名字，全部由游戏侧用本宏声明。
#[macro_export]
macro_rules! simple_channel {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Component, Clone, Copy, Debug, Default)]
        pub struct $name;
    };
}
