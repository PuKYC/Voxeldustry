//! 能力通道组件与可用性求解（业务内容）。
//!
//! 核心原则：**有什么输入能力组件，就开放什么输入通道**；
//! 而不是 `if entity.is_player() { ... }`。
//!
//! 通道集合每 tick 由 [`compute_input_availability`] 重算：`能力组件 − 禁用状态`。
//! 引擎提供通用的 [`InputAvailability`] / [`InputDisabled`] 载体与
//! `InputSet::Availability` 阶段，具体通道由本文件定义。

use bevy::prelude::*;
use game_engine::input::actions::ActionMap;

use game_engine::identity::StableEntityId;
use game_engine::math::FixedPoint;
use game_engine::simple_channel;

use super::actions::{self, CoreActions};
use super::control::{ControlledBy, LocalPlayer};
use super::events::{InteractRequest, NO_TARGET};
use super::snapshot::{PlayerInput, RawResolved, ResolvedInput};
use super::SimulationTick;

pub(crate) use game_engine::input::capability::{InputAvailability, InputDisabled};

simple_channel!(
    /// 可接受移动输入。
    MoveChannel
);
simple_channel!(
    /// 可接受跳跃输入。
    JumpChannel
);
simple_channel!(
    /// 可接受滑翔输入。
    GlideChannel
);
simple_channel!(
    /// 可接受飞行输入。
    FlightChannel
);
simple_channel!(
    /// 可攻击（`attack`）。
    AttackChannel
);
simple_channel!(
    /// 可破坏方块（`break_block`）。
    BreakBlockChannel
);
simple_channel!(
    /// 可放置方块（`place_block`）。
    PlaceBlockChannel
);
simple_channel!(
    /// 可冲刺。
    SprintChannel
);
simple_channel!(
    /// 可接受视角输入（鼠标 / 触摸拖拽）。
    ///
    /// 视角是独立轴：不占动作位、不进 `channel_mask`，由
    /// `InputAvailability.look` 单独门控；没有它的实体仍可移动 / 跳跃。
    LookChannel
);

/// 可交互，并声明交互半径。
#[derive(Component, Clone, Copy, Debug)]
pub struct InteractChannel {
    pub range: FixedPoint,
}

impl Default for InteractChannel {
    fn default() -> Self {
        Self {
            range: FixedPoint::from_num(3),
        }
    }
}

simple_channel!(
    /// 眩晕：禁用移动 / 跳跃 / 攻击 / 破坏方块 / 放置方块（交互不受影响）。
    Stunned
);
simple_channel!(
    /// 定身：禁用移动。
    Rooted
);

// ───────────────────────── 通道 → 动作名集合 ─────────────────────────
//
// 唯一真相源仍然是 `actions::ACTION_TABLE`：这里只写名字，
// 掩码一律通过 `actions::mask_of_names` 求得，避免出现第二张位表。

pub const MOVE_ACTIONS: &[&str] = &["move_forward", "move_back", "move_left", "move_right"];
pub const JUMP_ACTIONS: &[&str] = &["jump"];
pub const GLIDE_ACTIONS: &[&str] = &["glide"];
pub const FLIGHT_ACTIONS: &[&str] = &["flight_toggle"];
pub const INTERACT_ACTIONS: &[&str] = &["interact"];
/// 攻击是独立语义动作（旧 `tool` 聚合通道已整体移除）。
pub const ATTACK_ACTIONS: &[&str] = &["attack"];
/// 破坏方块是独立语义动作。
pub const BREAK_BLOCK_ACTIONS: &[&str] = &["break_block"];
/// 放置方块是独立语义动作。
pub const PLACE_BLOCK_ACTIONS: &[&str] = &["place_block"];
pub const SPRINT_ACTIONS: &[&str] = &["sprint"];

fn mask_of(actions: &[&str]) -> u64 {
    actions::mask_of_names(actions.iter().copied())
}

pub fn move_mask() -> u64 {
    mask_of(MOVE_ACTIONS)
}
pub fn jump_mask() -> u64 {
    mask_of(JUMP_ACTIONS)
}
pub fn glide_mask() -> u64 {
    mask_of(GLIDE_ACTIONS)
}
pub fn flight_mask() -> u64 {
    mask_of(FLIGHT_ACTIONS)
}
pub fn interact_mask() -> u64 {
    mask_of(INTERACT_ACTIONS)
}
pub fn attack_mask() -> u64 {
    mask_of(ATTACK_ACTIONS)
}
pub fn break_block_mask() -> u64 {
    mask_of(BREAK_BLOCK_ACTIONS)
}
pub fn place_block_mask() -> u64 {
    mask_of(PLACE_BLOCK_ACTIONS)
}
pub fn sprint_mask() -> u64 {
    mask_of(SPRINT_ACTIONS)
}

/// 眩晕削减的通道：移动 / 跳跃 / 攻击 / 破坏 / 放置。
///
/// 交互（`interact`）不受眩晕影响。
pub fn stunned_mask() -> u64 {
    move_mask() | jump_mask() | attack_mask() | break_block_mask() | place_block_mask()
}

/// 定身削减的通道。
pub fn rooted_mask() -> u64 {
    move_mask()
}

/// 全部动作位（调试 / 无能力限制时使用）。
pub fn all_actions_mask() -> u64 {
    actions::ACTION_TABLE.iter().fold(0u64, |acc, def| {
        acc | (1u64 << (def.channel_bit % actions::MAX_INPUT_CHANNELS))
    })
}

// ───────────────────────── 输入通道表（UI / 表现侧元数据） ─────────────────────────

/// 一个输入通道的静态描述。
///
/// 只描述「通道由哪些动作组成、该用哪种控件呈现」，不含物理按键
/// （按键是 Godot InputMap 的职责）。顺序固定，禁止用哈希表影响输出。
///
/// **视角（`LookChannel`）不在本表里**：它是独立轴、没有动作位；UI 用
/// `InputAvailability.look` / caps 载荷的 `look` 单独判断。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputChannelDef {
    /// 通道稳定键（Godot 侧信号 / UI 用它）。
    pub key: &'static str,
    /// 组成该通道的动作名（与 `ACTION_TABLE` 同名）。
    pub actions: &'static [&'static str],
    /// 建议控件形态：`axis2d` / `button` / `hold` / `toggle`。
    pub mode: &'static str,
}

/// 通道表（顺序即 Godot 侧遍历顺序，确定性要求）。
pub const INPUT_CHANNEL_TABLE: &[InputChannelDef] = &[
    InputChannelDef {
        key: "move",
        actions: MOVE_ACTIONS,
        mode: "axis2d",
    },
    InputChannelDef {
        key: "jump",
        actions: JUMP_ACTIONS,
        mode: "button",
    },
    InputChannelDef {
        key: "glide",
        actions: GLIDE_ACTIONS,
        mode: "hold",
    },
    InputChannelDef {
        key: "flight",
        actions: FLIGHT_ACTIONS,
        mode: "toggle",
    },
    InputChannelDef {
        key: "interact",
        actions: INTERACT_ACTIONS,
        mode: "button",
    },
    InputChannelDef {
        key: "attack",
        actions: ATTACK_ACTIONS,
        mode: "button",
    },
    InputChannelDef {
        key: "break_block",
        actions: BREAK_BLOCK_ACTIONS,
        mode: "button",
    },
    InputChannelDef {
        key: "place_block",
        actions: PLACE_BLOCK_ACTIONS,
        mode: "button",
    },
    InputChannelDef {
        key: "sprint",
        actions: SPRINT_ACTIONS,
        mode: "hold",
    },
];

/// 全部输入通道（固定顺序）。
pub fn input_channels() -> &'static [InputChannelDef] {
    INPUT_CHANNEL_TABLE
}

/// 动作名所在通道；未知动作返回 `None`。
pub fn channel_of_action(name: &str) -> Option<&'static InputChannelDef> {
    INPUT_CHANNEL_TABLE
        .iter()
        .find(|channel| channel.actions.contains(&name))
}

// ───────────────────────── 可用性求解 ─────────────────────────

/// 实体声明的通道集合（`compute_input_availability` 的纯数据输入）。
///
/// 抽成纯数据是为了能脱离 `App` 做确定性单测。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChannelSet {
    pub can_move: bool,
    pub can_jump: bool,
    pub can_glide: bool,
    pub can_fly: bool,
    pub can_interact: bool,
    pub can_attack: bool,
    pub can_break_block: bool,
    pub can_place_block: bool,
    pub can_sprint: bool,
    /// 视角轴（`LookChannel`）；不参与动作位掩码，单独走 `InputAvailability.look`。
    pub can_look: bool,
}

/// 纯函数：只按「能力组件是否存在」生成位掩码（`present`）。
///
/// 不扣除禁用状态：这是「有没有这个能力」的声明，供表现 / UI 与
/// [`availability_mask`] 的可用位对比。
///
/// **视角不在本掩码里**：`LookChannel` 是独立轴，用 [`look_allowed`] /
/// `InputAvailability.look` 表达；动作位掩码只描述可按 / 可松开的动作。
pub fn channel_mask(set: ChannelSet) -> u64 {
    let mut mask = 0u64;
    if set.can_move {
        mask |= move_mask();
    }
    if set.can_jump {
        mask |= jump_mask();
    }
    if set.can_glide {
        mask |= glide_mask();
    }
    if set.can_fly {
        mask |= flight_mask();
    }
    if set.can_interact {
        mask |= interact_mask();
    }
    if set.can_attack {
        mask |= attack_mask();
    }
    if set.can_break_block {
        mask |= break_block_mask();
    }
    if set.can_place_block {
        mask |= place_block_mask();
    }
    if set.can_sprint {
        mask |= sprint_mask();
    }
    mask
}

/// 视角轴是否开放（`LookChannel` 存在）。
///
/// 视角不占动作位，也不受 `Stunned` / `Rooted` / `InputDisabled` 影响；
/// 如以后需要「禁止转视角」，应扩展这里而不是往动作表塞假动作。
pub fn look_allowed(set: ChannelSet) -> bool {
    set.can_look
}

/// 纯函数：`能力组件 − 禁用状态 = 可用通道`。
///
/// **这里是「有什么组件就有什么输入」的唯一实现**。
pub fn availability_mask(set: ChannelSet, disabled: u64, stunned: bool, rooted: bool) -> u64 {
    let mut mask = channel_mask(set);
    if stunned {
        mask &= !stunned_mask();
    }
    if rooted {
        mask &= !rooted_mask();
    }
    mask &= !disabled;
    mask
}

/// 每 tick 重算可用通道（插进引擎的 `InputSet::Availability`）。
///
/// 注意：Bevy 的 Query 元组上限是 15 个元素。三个交互通道各自独立查询后
/// 正好 15 项，因此 `&ControlledBy` 拆成独立的只读查询（本系统只从中取
/// `source`），主查询仍以 `With<ControlledBy>` 过滤，语义不变。
#[allow(clippy::type_complexity)]
pub fn compute_input_availability(
    controlled_query: Query<(Entity, &ControlledBy)>,
    mut query: Query<
        (
            &mut InputAvailability,
            Option<&LocalPlayer>,
            Option<&MoveChannel>,
            Option<&JumpChannel>,
            Option<&GlideChannel>,
            Option<&FlightChannel>,
            Option<&InteractChannel>,
            Option<&AttackChannel>,
            Option<&BreakBlockChannel>,
            Option<&PlaceBlockChannel>,
            Option<&SprintChannel>,
            Option<&LookChannel>,
            Option<&InputDisabled>,
            Option<&Stunned>,
            Option<&Rooted>,
        ),
        With<ControlledBy>,
    >,
) {
    for (entity, controlled) in &controlled_query {
        let Ok((
            mut availability,
            local_player,
            can_move,
            can_jump,
            can_glide,
            can_fly,
            can_interact,
            can_attack,
            can_break_block,
            can_place_block,
            can_sprint,
            can_look,
            disabled,
            stunned,
            rooted,
        )) = query.get_mut(entity)
        else {
            continue;
        };
        let set = ChannelSet {
            can_move: can_move.is_some(),
            can_jump: can_jump.is_some(),
            can_glide: can_glide.is_some(),
            can_fly: can_fly.is_some(),
            can_interact: can_interact.is_some(),
            can_attack: can_attack.is_some(),
            can_break_block: can_break_block.is_some(),
            can_place_block: can_place_block.is_some(),
            can_sprint: can_sprint.is_some(),
            can_look: can_look.is_some(),
        };
        let present = channel_mask(set);
        let available = availability_mask(
            set,
            disabled.map(|d| d.0).unwrap_or(0),
            stunned.is_some(),
            rooted.is_some(),
        );
        let next = InputAvailability {
            mask: available,
            present,
            source: controlled.source,
            local: local_player.is_some(),
            look: look_allowed(set),
            version: availability.version,
        };

        // 全字段比较后再写：只改 mask 的旧实现会让 Bevy 的 Changed 每 tick
        // 误报（尤其是 present/source/local 变化），导致 INPUTCAPS 载荷每 tick 重发。
        if *availability != next {
            *availability = InputAvailability {
                version: availability.version.wrapping_add(1),
                ..next
            };
        }
    }
}

/// 失去控制后清零输入能力与输入快照。
///
/// `write_resolved_input` 不检查 `ControlledBy`：实体被解除控制后
/// `RawResolved` / `ResolvedInput` 会保留最后一份输入，角色会继续按最后的
/// 输入走。这里对「没有 `ControlledBy`」的实体把三者清零；只在非默认值时写，
/// 避免每 tick 触发 `Changed`。
///
/// 与 `resolve_controlled_entity`（`With<ControlledBy>`）的查询过滤条件不相交，
/// 调度器可以安全并行。
pub fn clear_uncontrolled_input(
    mut query: Query<
        (
            &mut InputAvailability,
            Option<&mut RawResolved>,
            Option<&mut ResolvedInput>,
        ),
        Without<ControlledBy>,
    >,
) {
    for (mut availability, raw, resolved) in &mut query {
        if *availability != InputAvailability::default() {
            *availability = InputAvailability::default();
        }
        if let Some(mut raw) = raw {
            if raw.0 != PlayerInput::NONE {
                raw.0 = PlayerInput::NONE;
            }
        }
        if let Some(mut resolved) = resolved {
            if resolved.0 != PlayerInput::NONE {
                resolved.0 = PlayerInput::NONE;
            }
        }
    }
}

/// 把 `interact` 动作的按下边沿转成 [`InteractRequest`]。
///
/// 交互的玩法判定不在输入层；这里只声明「意图」。
pub fn derive_interact_requests(
    tick: Res<SimulationTick>,
    query: Query<(&StableEntityId, &ResolvedInput)>,
    mut writer: MessageWriter<InteractRequest>,
) {
    let Some(action) = CoreActions::lookup("interact").map(|def| def.action) else {
        return;
    };
    let mask = CoreActions::mask_of_name("interact").unwrap_or(0);

    let mut msgs = Vec::new();
    for (id, resolved) in &query {
        if resolved.0.pressed & mask != 0 {
            msgs.push(InteractRequest {
                actor: *id,
                target: NO_TARGET,
                action,
                tick: tick.0,
            });
        }
    }

    if !msgs.is_empty() {
        writer.write_batch(msgs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_capability_means_no_input() {
        assert_eq!(
            availability_mask(ChannelSet::default(), 0, false, false),
            0,
            "没有任何能力组件时必须一个输入位都不开放"
        );
    }

    #[test]
    fn glide_requires_glide_channel() {
        let without = ChannelSet {
            can_jump: true,
            ..ChannelSet::default()
        };
        let with_glide = ChannelSet {
            can_jump: true,
            can_glide: true,
            ..ChannelSet::default()
        };
        let glide_bit = actions::mask_of_name("glide").unwrap();

        assert_eq!(
            availability_mask(without, 0, false, false) & glide_bit,
            0,
            "没有 GlideChannel 就不能收到滑翔输入"
        );
        assert_ne!(
            availability_mask(with_glide, 0, false, false) & glide_bit,
            0,
            "有 GlideChannel 就必须开放滑翔输入"
        );
    }

    #[test]
    fn stunning_removes_move_jump_combat_but_keeps_interact() {
        let set = ChannelSet {
            can_move: true,
            can_jump: true,
            can_attack: true,
            can_break_block: true,
            can_place_block: true,
            can_interact: true,
            ..ChannelSet::default()
        };
        let mask = availability_mask(set, 0, true, false);

        assert_eq!(mask & move_mask(), 0);
        assert_eq!(mask & jump_mask(), 0);
        assert_eq!(mask & attack_mask(), 0);
        assert_eq!(mask & break_block_mask(), 0);
        assert_eq!(mask & place_block_mask(), 0);
        assert_ne!(mask & interact_mask(), 0, "眩晕不应该禁用交互");
    }

    #[test]
    fn input_disabled_mask_wins_over_capability() {
        let set = ChannelSet {
            can_move: true,
            ..ChannelSet::default()
        };
        let jump_bit = actions::mask_of_name("jump").unwrap();
        assert_eq!(availability_mask(set, jump_bit, false, false) & jump_bit, 0);
    }

    #[test]
    fn rooted_only_removes_movement() {
        let set = ChannelSet {
            can_move: true,
            can_jump: true,
            ..ChannelSet::default()
        };
        let mask = availability_mask(set, 0, false, true);
        assert_eq!(mask & move_mask(), 0);
        assert_ne!(mask & jump_mask(), 0, "定身不应该禁用跳跃");
    }

    #[test]
    fn resulting_mask_is_produced_deterministically() {
        let set = ChannelSet {
            can_move: true,
            can_jump: true,
            can_interact: true,
            ..ChannelSet::default()
        };
        let a = availability_mask(set, 0, false, false);
        let b = availability_mask(set, 0, false, false);
        assert_eq!(a, b);
        assert_eq!(a, move_mask() | jump_mask() | interact_mask());
    }

    #[test]
    fn masks_come_from_action_table() {
        assert_eq!(
            move_mask(),
            actions::mask_of_name("move_forward").unwrap()
                | actions::mask_of_name("move_back").unwrap()
                | actions::mask_of_name("move_left").unwrap()
                | actions::mask_of_name("move_right").unwrap()
        );
        assert_eq!(jump_mask(), actions::mask_of_name("jump").unwrap());
        assert_eq!(
            jump_mask() & move_mask(),
            0,
            "jump 与 move 应占用不同通道位"
        );
        assert_eq!(jump_mask() & !all_actions_mask(), 0);
        assert_eq!(move_mask() & !all_actions_mask(), 0);
    }

    #[test]
    fn stunned_is_subset_of_all() {
        assert_eq!(stunned_mask() & !all_actions_mask(), 0);
        assert_eq!(rooted_mask() & !all_actions_mask(), 0);
    }

    #[test]
    fn channel_mask_is_zero_without_components() {
        assert_eq!(
            channel_mask(ChannelSet::default()),
            0,
            "没有任何能力组件时 present 必须为 0"
        );
    }

    #[test]
    fn channel_mask_is_presence_only() {
        let set = ChannelSet {
            can_move: true,
            can_jump: true,
            ..ChannelSet::default()
        };
        assert_eq!(channel_mask(set), move_mask() | jump_mask());
    }

    /// 眩晕削减 available，但 present（能力存在）不变：UI 才能区分
    /// 「没有这个能力」与「有能力但当前不可用」。
    #[test]
    fn stunning_reduces_available_but_not_present() {
        let set = ChannelSet {
            can_move: true,
            can_jump: true,
            can_attack: true,
            can_break_block: true,
            can_place_block: true,
            can_interact: true,
            ..ChannelSet::default()
        };
        let present = channel_mask(set);
        let available = availability_mask(set, 0, true, false);

        assert_ne!(present & move_mask(), 0);
        assert_ne!(present & jump_mask(), 0);
        assert_ne!(present & attack_mask(), 0);
        assert_ne!(present & break_block_mask(), 0);
        assert_ne!(present & place_block_mask(), 0);
        assert_eq!(available & move_mask(), 0);
        assert_eq!(available & jump_mask(), 0);
        assert_eq!(available & attack_mask(), 0);
        assert_eq!(available & break_block_mask(), 0);
        assert_eq!(available & place_block_mask(), 0);
        assert_ne!(available & interact_mask(), 0, "眩晕不应该禁用交互");
        assert_eq!(
            present,
            channel_mask(set),
            "禁用状态不得改变 present（能力组件并集）"
        );
    }

    /// 旧的聚合 `tool` 通道与 `primary_tool` / `secondary_tool` 已整体移除：
    /// 动作表里不得再存在这些名字（否则能力 / 存档语义会悄悄漂移）。
    #[test]
    fn action_table_has_no_tool_aggregates() {
        for removed in ["tool", "primary_tool", "secondary_tool"] {
            assert!(
                CoreActions::lookup(removed).is_none(),
                "ACTION_TABLE 里不得再存在 {removed}"
            );
            assert!(actions::channel_bit_of(removed).is_none());
            assert!(channel_of_action(removed).is_none());
        }
    }

    /// attack / break_block / place_block 是三个独立通道：位不同、掩码不相交，
    /// 且各自只含一个动作。
    #[test]
    fn combat_actions_occupy_distinct_channels() {
        let cases = [
            ("attack", attack_mask(), ATTACK_ACTIONS, 8u32),
            ("break_block", break_block_mask(), BREAK_BLOCK_ACTIONS, 9),
            ("place_block", place_block_mask(), PLACE_BLOCK_ACTIONS, 14),
        ];
        for (name, mask, action_names, bit) in cases {
            assert_eq!(mask.count_ones(), 1, "{name} 必须只占一个通道位");
            assert_eq!(mask, actions::mask_of_name(name).unwrap());
            assert_eq!(action_names.len(), 1);
            assert_eq!(action_names[0], name);
            assert_eq!(actions::channel_bit_of(name), Some(bit));
            let channel = channel_of_action(name).expect("动作必须挂在通道表上");
            assert_eq!(channel.key, name);
            assert_eq!(channel.mode, "button");
            assert_eq!(channel.actions, action_names);
        }
        assert_eq!(attack_mask() & break_block_mask(), 0);
        assert_eq!(attack_mask() & place_block_mask(), 0);
        assert_eq!(break_block_mask() & place_block_mask(), 0);
    }

    /// 眩晕同时削减三个动作的 available，但 present 保持能力声明不变。
    #[test]
    fn stunning_trims_all_combat_actions_but_keeps_present() {
        let set = ChannelSet {
            can_attack: true,
            can_break_block: true,
            can_place_block: true,
            ..ChannelSet::default()
        };
        let present = channel_mask(set);
        let available = availability_mask(set, 0, true, false);

        for mask in [attack_mask(), break_block_mask(), place_block_mask()] {
            assert_ne!(present & mask, 0, "present 必须保留能力位");
            assert_eq!(available & mask, 0, "available 必须被眩晕削减");
        }
        assert_eq!(present, channel_mask(set));
    }

    /// 交互不受眩晕削减（交互由 InteractChannel 单独门控）。
    #[test]
    fn interact_survives_stunned() {
        let set = ChannelSet {
            can_interact: true,
            ..ChannelSet::default()
        };
        let available = availability_mask(set, 0, true, false);
        assert_ne!(available & interact_mask(), 0);
        assert_eq!(stunned_mask() & interact_mask(), 0);
        assert_eq!(stunned_mask() & sprint_mask(), 0, "眩晕不影响冲刺能力位");
    }

    /// 通道表里的每个动作都能在 ACTION_TABLE 查到，且通道位合法。
    #[test]
    fn every_channel_action_exists_in_action_table_with_valid_bit() {
        let mut keys: Vec<&str> = INPUT_CHANNEL_TABLE.iter().map(|c| c.key).collect();
        keys.sort_unstable();
        let before = keys.len();
        keys.dedup();
        assert_eq!(before, keys.len(), "通道 key 必须唯一");

        for channel in INPUT_CHANNEL_TABLE {
            assert!(!channel.actions.is_empty(), "{} 通道不得为空", channel.key);
            for action in channel.actions {
                let def = CoreActions::lookup(action).unwrap_or_else(|| {
                    panic!("通道 {} 的动作 {action} 不在 ACTION_TABLE", channel.key)
                });
                assert!(def.channel_bit < actions::MAX_INPUT_CHANNELS);
                assert_eq!(channel_of_action(action).map(|c| c.key), Some(channel.key));
            }
        }
    }

    /// 视角是独立轴：不占动作位，也不被眩晕 / 定身削减。
    #[test]
    fn look_is_a_separate_axis() {
        let set = ChannelSet {
            can_look: true,
            ..ChannelSet::default()
        };
        assert!(look_allowed(set));
        assert_eq!(channel_mask(set), 0, "look 不得占用动作位");
        assert_eq!(
            availability_mask(set, 0, true, true),
            0,
            "视角不进动作位掩码，眩晕 / 定身不改变它"
        );
    }

    /// 失去控制：能力与输入快照都必须清零；仍被控制的实体不受影响。
    #[test]
    fn uncontrolled_entities_are_cleared() {
        use crate::input::control::{ControlledBy, InputSourceId};
        use crate::input::snapshot::{PlayerInput, RawResolved, ResolvedInput};
        use bevy::prelude::*;

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_systems(Update, clear_uncontrolled_input);

        let dirty = InputAvailability {
            mask: move_mask() | jump_mask(),
            present: move_mask() | jump_mask(),
            source: InputSourceId(3),
            local: true,
            look: true,
            version: 9,
        };
        let input = PlayerInput {
            held: move_mask(),
            pressed: jump_mask(),
            ..PlayerInput::NONE
        };
        let lost = app
            .world_mut()
            .spawn((dirty, RawResolved(input), ResolvedInput(input)))
            .id();
        let controlled = app
            .world_mut()
            .spawn((
                dirty,
                ControlledBy {
                    source: InputSourceId(3),
                },
                RawResolved(input),
                ResolvedInput(input),
            ))
            .id();

        app.update();

        let world = app.world();
        assert_eq!(
            world.entity(lost).get::<InputAvailability>(),
            Some(&InputAvailability::default()),
            "无 ControlledBy 的实体能力必须清零"
        );
        assert_eq!(
            world.entity(lost).get::<RawResolved>().unwrap().0,
            PlayerInput::NONE
        );
        assert_eq!(
            world.entity(lost).get::<ResolvedInput>().unwrap().0,
            PlayerInput::NONE
        );
        // 仍被控制的实体必须保持原样。
        assert_eq!(
            world.entity(controlled).get::<InputAvailability>(),
            Some(&dirty)
        );
        assert_eq!(
            world.entity(controlled).get::<RawResolved>().unwrap().0,
            input
        );
    }
}
