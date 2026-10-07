//! 仅开发 / 演示用场景与装配样板；生产项目可整体删除。
//!
//! - [`demo`]：端到端演示（Godot 输入 -> 模拟 -> 表现同步）；
//! - [`perf`]：性能测试场景（大量实体 + 每 tick 全量变化）。

pub mod demo;
pub mod perf;
mod spawn;
pub(crate) use spawn::spawn_local_player;
