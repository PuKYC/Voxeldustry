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

/// 由于 bevy 关闭默认特性时 为安卓编译时仍然会自带winit 所以必须要选择后端，导致多编译一些窗口相关代码
/// 还有这个函数
/// fuck bevy! 
/// TODO 提交issue
#[cfg(target_os = "android")]
#[unsafe(no_mangle)] // edition 2021 写成 #[no_mangle]
pub extern "C" fn android_main(_app: *mut std::ffi::c_void) {}
