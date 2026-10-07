//! game-core：确定性 ECS 模拟核心（无渲染）。
//!
//! - 单人模式：本地权威直跑（无网络）。
//! - 局域网模式（规划中）：权威服务器 + 客户端预测副本，复用同一套系统。
//!
//! **边界铁律**：本 crate 不依赖 `godot` crate。跨边界类型要么在本 crate
//! 定义（业务内容 / 载荷实例 / 组件），要么从 `game_engine` 取出（机制）；
//! `godot-client-ext` 只做 `Variant ↔ game_core 类型` 的机械搬运。
//!
//! # 模块地图
//!
//! | 模块 | 职责 |
//! | --- | --- |
//! | [`app`] / [`spec`] / [`mods`] | 引擎绑定：`CoreGame` 装配 / `CoreSpec` + [`spec::ClientBridge`] / mod 集合 |
//! | [`bevy_backend`] | 后台 Bevy 线程入口（FFI） |
//! | `gameplay` | 玩法层（阶段集 + movement / death） |
//! | [`input`] | 动作表与能力通道 |
//! | [`privacy`] | 隐私组件与 `CorePrivacyScope` |
//! | `prediction` | 预测 / 回滚绑定（单人路径） |
//! | [`presentation`] | 表现通道（payload / synced_components / sync / semantics / event） |
//! | [`static_data`] | 静态数据（prototype / item） |
//! | `net`（`feature = "net"`） | replicon 可见性适配 |
//! | `dev` | 仅开发 / 演示场景（生产可整体删除） |

pub mod app;
pub mod bevy_backend;
mod dev;
mod gameplay;
pub mod input;
/// 中立扩展字段袋（逻辑侧容器，见模块文档的分层说明）。
pub mod logic_ext;
pub mod mods;
mod prediction;
pub mod presentation;
pub mod privacy;
pub mod spec;
pub mod static_data;
/// 游戏世界语义与数值：BodyKind / 生成 / LOD 策略 / 停靠类型。
pub mod world;

/// 网络适配（replicon VisibilityFilter / AOI 桥）；仅 feature = "net" 编译。
#[cfg(feature = "net")]
pub mod net;
