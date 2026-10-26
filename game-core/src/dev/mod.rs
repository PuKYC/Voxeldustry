//! 仅开发 / 演示用场景与装配样板；生产项目可整体删除。
//!
//! - [`demo`]：端到端演示（Godot 输入 -> 模拟 -> 表现同步）；
//! - [`perf`]：性能测试场景（大量实体 + 每 tick 全量变化）；
//! - [`gameplay_test`]：游戏内容测试场景（带碰撞的体素平台 + 带碰撞的玩家）。

pub mod demo;
#[cfg(feature = "physics")]
pub mod gameplay_test;
pub mod perf;
mod spawn;
#[cfg(feature = "physics")]
pub mod voxel_viewer;
pub(crate) use spawn::spawn_local_player;
