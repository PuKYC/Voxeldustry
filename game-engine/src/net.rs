//! 网络适配的 crate 根入口（feature = "net"）。
//!
//! 具体机制在 `perception::net`；这里仅做再导出，便于 `game_engine::net::*` 访问。
//! 引擎默认不依赖 bevy_replicon。

pub use crate::perception::net::*;
