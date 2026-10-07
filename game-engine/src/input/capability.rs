//! 输入能力 / 禁用机制的**通用部分**。
//!
//! 具体能力通道组件（`MoveChannel` / `JumpChannel` …）与「组件 − 禁用 = 可用」
//! 的求解属于游戏侧；引擎只提供与具体通道无关的可用性载体与禁用掩码。

use bevy::prelude::*;

/// 禁用掩码：状态效果 / 环境对通道的削减。
///
/// 用位掩码而不是「每种禁用一个组件」，是为了避免能力组件数量爆炸。
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct InputDisabled(pub u64);

/// 本 tick 该实体真正可用的通道集合。
///
/// 每 tick 由游戏侧的可用性求解系统重算；`version` 便于排查
/// 「能力为什么没生效」。
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct InputAvailability {
    pub mask: u64,
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
