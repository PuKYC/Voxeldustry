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
use super::control::ControlledBy;
use super::events::{InteractRequest, NO_TARGET};
use super::snapshot::ResolvedInput;
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
    /// 可使用工具。
    ToolUseChannel
);
simple_channel!(
    /// 可冲刺。
    SprintChannel
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
    /// 眩晕：禁用移动 / 跳跃 / 工具。
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
pub const TOOL_ACTIONS: &[&str] = &["primary_tool", "secondary_tool"];
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
pub fn tool_mask() -> u64 {
    mask_of(TOOL_ACTIONS)
}
pub fn sprint_mask() -> u64 {
    mask_of(SPRINT_ACTIONS)
}

/// 眩晕削减的通道。
pub fn stunned_mask() -> u64 {
    move_mask() | jump_mask() | tool_mask()
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
    pub can_use_tool: bool,
    pub can_sprint: bool,
}

/// 纯函数：`能力组件 − 禁用状态 = 可用通道`。
///
/// **这里是「有什么组件就有什么输入」的唯一实现**。
pub fn availability_mask(set: ChannelSet, disabled: u64, stunned: bool, rooted: bool) -> u64 {
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
    if set.can_use_tool {
        mask |= tool_mask();
    }
    if set.can_sprint {
        mask |= sprint_mask();
    }
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
#[allow(clippy::type_complexity)]
pub fn compute_input_availability(
    mut query: Query<
        (
            &mut InputAvailability,
            Option<&MoveChannel>,
            Option<&JumpChannel>,
            Option<&GlideChannel>,
            Option<&FlightChannel>,
            Option<&InteractChannel>,
            Option<&ToolUseChannel>,
            Option<&SprintChannel>,
            Option<&InputDisabled>,
            Option<&Stunned>,
            Option<&Rooted>,
        ),
        With<ControlledBy>,
    >,
) {
    for (
        mut availability,
        can_move,
        can_jump,
        can_glide,
        can_fly,
        can_interact,
        can_use_tool,
        can_sprint,
        disabled,
        stunned,
        rooted,
    ) in &mut query
    {
        let set = ChannelSet {
            can_move: can_move.is_some(),
            can_jump: can_jump.is_some(),
            can_glide: can_glide.is_some(),
            can_fly: can_fly.is_some(),
            can_interact: can_interact.is_some(),
            can_use_tool: can_use_tool.is_some(),
            can_sprint: can_sprint.is_some(),
        };
        let mask = availability_mask(
            set,
            disabled.map(|d| d.0).unwrap_or(0),
            stunned.is_some(),
            rooted.is_some(),
        );

        if availability.mask != mask {
            availability.mask = mask;
            availability.version = availability.version.wrapping_add(1);
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
    fn stunning_removes_move_jump_tool_but_keeps_interact() {
        let set = ChannelSet {
            can_move: true,
            can_jump: true,
            can_use_tool: true,
            can_interact: true,
            ..ChannelSet::default()
        };
        let mask = availability_mask(set, 0, true, false);

        assert_eq!(mask & move_mask(), 0);
        assert_eq!(mask & jump_mask(), 0);
        assert_eq!(mask & tool_mask(), 0);
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
}
