//! 表现语义注册表（**mod 的基石**）。
//!
//! 逻辑/表现两侧只交换 `u32` ID；名字、分区、哈希规则全部收在这里，
//! 与 `crate::input::actions::ACTION_TABLE` 使用同一套 ID 分区约定：
//!
//! | 区间 | 用途 | 谁分配 |
//! |---|---|---|
//! | `0` | 保留（无 / none） | 核心 |
//! | `1..=999` | 核心状态 / 资源名 | 核心 |
//! | `1000..=9999` | mod 显式分配 | mod |
//! | `>= 10000` | mod 名字哈希（[`mod_hash_id`]） | mod |
//!
//! 名字只用于表现层查表（状态展示、动画 / 音效 / 特效资源映射）；
//! 逻辑、存档与网络协议永远只看 ID。
//!
//! 扩展方式：
//! - 编译期：核心表是 `const`，只增不改；
//! - 原生 mod：`ResMut<SemanticRegistry>` 里 [`SemanticRegistry::register`]；
//! - GDScript mod：`BevyAppManager.register_semantic(...)` /
//!   `register_semantic_named("mymod:xxx")`。

/// 表现语义表项。
///
/// 与 [`game_engine::ids::IdEntry`] 形状相同，这里统一为别名，避免两套结构漂移。
pub use game_engine::ids::IdEntry as SemanticEntry;

// ── 分区工具：真相源在中立模块 `game_engine::ids` ──
//
// 物品定义（逻辑层）与表现语义（表现层）共同依赖它，放在这里会造成
// 「逻辑 -> 表现」的依赖倒挂，因此提升到 `ids.rs` 后在此重导出，
// 保持既有调用点（`presentation_bridge.rs` 等）零改动。
pub use game_engine::ids::{
    fnv1a32, mod_hash_id, partition_name, partition_of, IdPartition, CORE_ID_MAX, HASH_ID_MIN,
    MOD_ID_MAX, MOD_ID_MIN,
};

mod registry;
mod tables;

pub use registry::SemanticRegistry;
pub use tables::{
    action_state_name, anim_name, domain_table, is_core_domain, locomotion_name, overlay_tag_name,
    sound_name, table_snapshot, vfx_name, ACTION_STATE_TABLE, ANIM_TABLE, LOCOMOTION_TABLE,
    OVERLAY_TAG_TABLE, SEMANTIC_DOMAINS, SOUND_TABLE, VFX_TABLE,
};
