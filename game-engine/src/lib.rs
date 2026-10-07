//! game-engine：通用确定性 ECS 引擎（无业务、无 godot、默认无网络）。
//!
//! 分层：`godot-client-ext` → `game-core` → `game-engine`。
//! 本 crate 只拥有「怎么做」的机制，不包含任何本游戏的字符串 / 数值 / 策略。
//!
//! ## 稳定 API（mod / 下游依赖的承诺面，只增不改）
//!
//! - 类型绑定：GameSpec、DefaultSpec；
//! - 调度锚点：EngineSet、InputSet；
//! - 装配：InputPlugin、EnginePresentationPlugin、GameModule / GameModulePlugin / run_headless；
//! - Mod：Mod / ModContext / Mods / ModManifest / Registry / EngineSet；
//! - 表现：PayloadKindTrait / PresentationPayload / define_payloads!、
//!   PresentationCommand / PresentationFrame / PresentationSlot、ClientBridge；
//! - 数据：ids 分区工具、identity、math、spatial、aoi、perception。
//!
//! 其余模块可在标记为 `#[non_exhaustive]` 后自由演进。

pub mod aoi;
pub mod backend;
pub mod bridge;
pub mod identity;
pub mod ids;
pub mod input;
pub mod math;
pub mod modding;
/// 网络适配的 crate 根入口；具体机制在 `perception::net`。仅 feature = "net" 编译。
#[cfg(feature = "net")]
pub mod net;
pub mod perception;
pub mod presentation;
pub mod rng;
pub mod rollback;
pub mod sim;
pub mod spatial;
pub mod spec;
pub mod strategy;
/// 体素世界层；仅 feature = "voxel" 编译。
#[cfg(feature = "voxel")]
pub mod voxel;
