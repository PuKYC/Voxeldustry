//! godot-client-ext：Godot 与 `game-core` 之间的**唯一**桥接 crate。
//!
//! 边界铁律：
//! 本 crate 里允许出现的代码只有两类 ——
//!
//! 1. `Variant / Dictionary / Array / PackedStringArray ↔ game_core 类型` 的转换；
//! 2. `godot::prelude` 的节点 / 信号样板。
//!
//! **出现任何业务 `match` 或常量表就是违规**：语义动作表在
//! `game_core::input::actions`，表现载荷在 `game_core::presentation::payload`。

mod bevy_runtime_node;
mod presentation_bridge;

use godot::prelude::*;

struct MyExtension;

#[gdextension]
unsafe impl ExtensionLibrary for MyExtension {}
