//! 本游戏的语义动作表（业务内容）。
//!
//! 机制（`ActionDef` / `ActionMap` / 通道位工具）在
//! `game_engine::input::actions`；这里只提供 `ACTION_TABLE` 与
//! `CoreActions: ActionMap`，并保留旧的自由函数入口供内部与 FFI 使用。
//!
//! **跨边界契约**：Godot 侧只认 [`ActionDef::name`]（与 Godot `InputMap` 同名）；
//! 名字 → 通道位的映射在此编译期固定，存档 / 协议 / 回滚 / 玩法逻辑
//! 一律看不到物理按键 —— 这就是「改键不影响逻辑」。
//!
//! **交互三动作互相独立**：`attack` / `break_block` / `place_block` 是三个
//! 语义动作、各占独立通道位（不再有聚合的 `tool` 通道，也没有
//! `primary_tool` / `secondary_tool`）：能力 / 禁用状态可以分别门控，
//! 玩法逻辑也就能分别解释意图。

pub use game_engine::input::actions::{
    ActionDef, ActionId, ActionMap, ChannelMask, MAX_INPUT_CHANNELS,
};

macro_rules! action {
    ($name:literal, $id:literal, $bit:literal) => {
        ActionDef {
            name: $name,
            action: ActionId($id),
            channel_bit: $bit,
        }
    };
}

/// 单一真相源：Godot 的 `InputMap` 名 → 通道位
pub const ACTION_TABLE: &[ActionDef] = &[
    action!("move_forward", 1, 0),
    action!("move_back", 2, 1),
    action!("move_left", 3, 2),
    action!("move_right", 4, 3),
    action!("jump", 5, 4),
    action!("glide", 6, 5),
    action!("flight_toggle", 7, 6),
    action!("interact", 8, 7),
    // 三个交互动作各占独立通道位：攻击 / 破坏方块 / 放置方块。
    action!("attack", 9, 8),
    action!("break_block", 10, 9),
    action!("sprint", 11, 10),
    action!("crouch", 12, 11),
    action!("reload", 13, 12),
    action!("drop_item", 14, 13),
    action!("place_block", 15, 14),
    // 1000~9999 留给 mod / 插件；10000+ 用字符串哈希。
];

/// 本游戏的动作表实现（ZST）。
pub struct CoreActions;

impl ActionMap for CoreActions {
    fn table() -> &'static [ActionDef] {
        ACTION_TABLE
    }
}

/// 名字 → 通道位序号。
pub fn channel_bit_of(name: &str) -> Option<u32> {
    CoreActions::channel_bit_of(name)
}

/// 名字 → 单一位掩码。
pub fn mask_of_name(name: &str) -> Option<u64> {
    CoreActions::mask_of_name(name)
}

/// 批量名字 → 掩码（未知名字被忽略）。
pub fn mask_of_names<'a>(names: impl IntoIterator<Item = &'a str>) -> u64 {
    CoreActions::mask_of_names(names)
}

/// 位序号 → 名字。
pub fn name_of_bit(bit: u32) -> Option<&'static str> {
    CoreActions::name_of_bit(bit)
}

/// 位序号 → 语义动作。
pub fn action_of_bit(bit: u32) -> Option<ActionId> {
    CoreActions::action_of_bit(bit)
}

/// 供 Godot 启动时拉取并校验自己的 `InputMap`：返回 `(name, channel_bit)`。
pub fn action_table_snapshot() -> Vec<(&'static str, u32)> {
    CoreActions::snapshot()
}

// ───────────────────────── 运行期全局动作表 ─────────────────────────

use std::sync::{Arc, RwLock};

/// 本次装配的全局动作表（已发布时含 mod 动作）。
///
/// `get_action_table` / `submit_input_frame` 在 Godot 线程上调用，拿不到 Bevy
/// `World`，所以装配成功后把表发布到这里；进程内只有一个 App，不做多世界
/// 隔离。未发布时（纯服务端 / 单测）一律退回核心表，行为与从前一致。
static ACTIVE_ACTIONS: RwLock<Option<Arc<[ActionDef]>>> = RwLock::new(None);

/// 发布本次装配的动作表（含 mod 动作）。
///
/// 由后端在 `build_with_mods` 成功后调用一次；重复调用以后一次为准。
pub fn publish_active_actions(resolved: &game_engine::input::ResolvedActions) {
    let table: Arc<[ActionDef]> = Arc::from(resolved.table());
    match ACTIVE_ACTIONS.write() {
        Ok(mut slot) => *slot = Some(table),
        Err(poisoned) => *poisoned.into_inner() = Some(table),
    }
}

/// 当前全局动作表：已发布则含 mod 动作，否则退回核心表。
pub fn active_actions() -> Arc<[ActionDef]> {
    let published = match ACTIVE_ACTIONS.read() {
        Ok(slot) => (*slot).clone(),
        Err(poisoned) => (*poisoned.into_inner()).clone(),
    };
    published.unwrap_or_else(|| Arc::from(ACTION_TABLE))
}

/// 同 [`action_table_snapshot`]，但已发布的全局表优先（含 mod 动作）。
pub fn action_table_snapshot_active() -> Vec<(&'static str, u32)> {
    active_actions()
        .iter()
        .map(|def| (def.name, def.channel_bit))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 发布后的全局表必须带上 mod 动作，且查询接口与之一致。
    ///
    /// 只读写本模块的全局，不影响其它用例（它们都直接走 `CoreActions`）。
    #[test]
    fn published_table_includes_mod_actions() {
        let mods: &[(&'static str, &'static [&'static str])] =
            &[("demo", &["demo_action", "demo_other"])];
        let resolved =
            game_engine::input::ResolvedActions::resolve(CoreActions::table(), mods)
                .expect("演示动作表必须可解析");
        publish_active_actions(&resolved);

        let active = active_actions();
        assert_eq!(active.len(), ACTION_TABLE.len() + 2);
        assert!(active.iter().any(|def| def.name == "demo_action"));
        assert_eq!(
            action_table_snapshot_active().len(),
            ACTION_TABLE.len() + 2,
            "Godot 拉取的表必须与全局表同长"
        );
    }

    /// 不带 mod 时，运行期解析表必须与静态 ACTION_TABLE 逐位相同。
    #[test]
    fn resolved_without_mods_matches_static_table() {
        let resolved = game_engine::input::ResolvedActions::resolve(CoreActions::table(), &[])
            .expect("core 表必须可解析");
        assert_eq!(resolved.table(), ACTION_TABLE);
        for def in ACTION_TABLE {
            assert_eq!(resolved.lookup(def.name), Some(*def));
        }
        assert_eq!(resolved.snapshot(), action_table_snapshot());
    }

    #[test]
    fn table_is_consistent() {
        let mut bits: Vec<u32> = ACTION_TABLE.iter().map(|d| d.channel_bit).collect();
        bits.sort_unstable();
        let before = bits.len();
        bits.dedup();
        assert_eq!(before, bits.len(), "通道位重复");

        let mut ids: Vec<u32> = ACTION_TABLE.iter().map(|d| d.action.0).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "ActionId 重复");

        assert!(ACTION_TABLE
            .iter()
            .all(|d| d.channel_bit < MAX_INPUT_CHANNELS));
    }

    #[test]
    fn unknown_name_is_none_not_panic() {
        assert!(channel_bit_of("no_such_action_at_all").is_none());
        assert_eq!(channel_bit_of("jump"), Some(4));
    }

    #[test]
    fn mask_roundtrip() {
        let mask = mask_of_name("jump").unwrap();
        assert_eq!(mask, 1 << 4);
        assert_eq!(name_of_bit(4), Some("jump"));
        assert_eq!(action_of_bit(4), Some(ActionId(5)));
    }

    /// 三个交互动作是独立语义：各自的 id / 通道位不得复用，
    /// 旧的聚合 `tool` 与 `primary_tool` / `secondary_tool` 必须已移除。
    #[test]
    fn combat_actions_are_independent_semantics() {
        assert_eq!(channel_bit_of("attack"), Some(8));
        assert_eq!(channel_bit_of("break_block"), Some(9));
        assert_eq!(channel_bit_of("place_block"), Some(14));
        assert_eq!(action_of_bit(8), Some(ActionId(9)));
        assert_eq!(action_of_bit(9), Some(ActionId(10)));
        assert_eq!(action_of_bit(14), Some(ActionId(15)));

        for removed in ["tool", "primary_tool", "secondary_tool"] {
            assert!(
                channel_bit_of(removed).is_none(),
                "{removed} 必须已从 ACTION_TABLE 移除"
            );
        }

        // jump 的通道位是玩法 / 测试依赖的契约（bit 4），不要顺手改掉。
        assert_eq!(channel_bit_of("jump"), Some(4));
    }
}
