//! 输入层（业务侧装配）。
//!
//! 机制在 `game_engine::input`；本模块提供本游戏的动作表（`actions`）
//! 与能力通道（`capability`），并用 [`GameInputPlugin`] 把两者装配进引擎骨架。
//!
//! **铁律**：Godot 侧出现的每一个类型都能从本模块（或其重导出）取得；
//! Godot 不做能力过滤，也不把按键映射成玩法语义。

pub mod actions;
pub mod capability;

// 这些子模块现在住在 game-engine；重导出以保持 `crate::input::raw::*` 等旧路径可用。
// 这是**面向 FFI 的有意重导出**：godot-client-ext 只依赖 game-core，需要经
// `game_core::input::*` 取到这些引擎机制类型，请勿当作冗余 import 清理。
pub use game_engine::input::{control, events, raw, snapshot, staging};

// 仅供 crate 内部（dev::demo / dev::perf / presentation 等兄弟模块）使用的顶层别名。
// 不对外公开：外部/未来 Mod 请从 `input::control`、`input::capability` 等
// 子模块路径导入，或直接依赖 `game_engine::input`，不要再依赖这份顶层平铺。
pub use actions::{
    action_of_bit, action_table_snapshot, channel_bit_of, mask_of_name, mask_of_names, name_of_bit,
    CoreActions, ACTION_TABLE,
};
pub use capability::{
    all_actions_mask, availability_mask, compute_input_availability, derive_interact_requests,
    flight_mask, glide_mask, interact_mask, jump_mask, move_mask, rooted_mask, sprint_mask,
    stunned_mask, tool_mask, ChannelSet, FlightChannel, GlideChannel, InteractChannel, JumpChannel,
    MoveChannel, Rooted, SprintChannel, Stunned, ToolUseChannel,
};
pub(crate) use game_engine::input::{InputSet, InteractRequest, SimulationTick};

use bevy::prelude::*;

/// 本游戏的输入装配插件：动作表 + 能力求解 + 交互意图。
pub struct GameInputPlugin;

impl Plugin for GameInputPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(game_engine::input::InputPlugin::<actions::CoreActions>::default());
        app.add_message::<InteractRequest>();
        app.add_systems(
            FixedPreUpdate,
            compute_input_availability.in_set(InputSet::Availability),
        );
        app.add_systems(
            FixedPreUpdate,
            derive_interact_requests.in_set(InputSet::Events),
        );
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    // 机制类型直接从引擎取，不再依赖 mod 内为测试保留的重导出。
    use game_engine::input::control::InputSourceId;
    use game_engine::input::raw::RawInputFrame;
    use game_engine::input::staging::InputStaging;
    use game_engine::math::FixedPoint;

    /// 端到端（不含 Godot）：提交原始帧 → 消费 → 检查折叠结果。
    #[test]
    fn staging_to_player_input_is_deterministic() {
        let staging = InputStaging::new();
        let source = InputSourceId(1);

        let (frame, unknown) = RawInputFrame::from_names::<CoreActions>(
            ["move_forward", "jump"],
            ["jump"],
            [],
            (1.0, 0.0),
            (0.0, 0.0),
            1,
            500,
        );
        assert!(unknown.is_empty());

        staging.submit(source, frame);
        let tick = staging.take_for_tick(source).expect("输入源应已注册");
        let input = tick.to_player_input();

        assert_eq!(
            input.held,
            mask_of_name("move_forward").unwrap() | mask_of_name("jump").unwrap()
        );
        assert_eq!(input.pressed, mask_of_name("jump").unwrap());
        assert_eq!(input.move_x, FixedPoint::from_num(1));
        assert_eq!(input.move_y, FixedPoint::from_num(0));
        assert_eq!(staging.render_clock_ms(), 500);
    }
}
