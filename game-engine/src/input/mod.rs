//! 输入机制层（Godot → 引擎）。
//!
//! 分层：
//!
//! ```text
//! Godot 物理输入
//!   → GDScript 采集（只查 InputMap 名，不做语义判断）
//!   → godot-client-ext 机械转换（Variant → RawInputFrame）
//!   → InputStaging（last-wins + 边沿锁存，跨线程合并）
//!   → 输入系统链（折叠 / 控制关系 / [游戏侧能力过滤] / 历史 / 事件）
//!   → 玩法系统消费 ResolvedInput
//! ```
//!
//! 引擎只提供与具体动作表无关的骨架：动作表由 [`ActionMap`] 注入，
//! 能力求解由游戏侧插入 [`InputSet::Availability`]。

pub mod actions;
pub mod capability;
pub mod control;
pub mod events;
pub mod raw;
pub mod snapshot;
pub mod staging;
pub mod systems;

use std::marker::PhantomData;

use bevy::prelude::*;

pub use actions::{ActionDef, ActionId, ActionMap, ChannelMask, EmptyActions, MAX_INPUT_CHANNELS};
pub use capability::{InputAvailability, InputDisabled};
pub use control::{ControlledBy, ControlsEntity, InputSource, InputSourceId, LocalPlayer};
pub use events::{InputActionPressed, InputActionReleased, InteractRequest, NO_TARGET};
pub use raw::{
    axis_to_fixed, quantize_axis, quantize_look, AxisI16, AxisKind, RawInputFrame, DEFAULT_DEADZONE,
};
pub use snapshot::{InputBuffer, PlayerInput, RawResolved, ResolvedInput};
pub use staging::{InputStaging, TickInput};

/// 当前逻辑 tick（由 `advance_simulation_tick` 每 fixed step 推进一次）。
#[derive(Resource, Clone, Copy, Debug, Default)]
pub struct SimulationTick(pub u32);

/// 输入链的稳定命名阶段。
///
/// 游戏侧（mod）只能相对这些命名集合排序：
/// - `Collect`：采集 / 折叠 / 控制关系（引擎）
/// - `Availability`：能力求解（游戏侧插入）
/// - `Apply`：原始 ∩ 可用 / 历史（引擎）
/// - `Events`：派生瞬时事件（引擎 + 游戏侧）
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InputSet {
    Collect,
    Availability,
    Apply,
    Events,
}

/// 输入机制插件。
///
/// 泛型于具体动作表 `A`：引擎不写死任何动作名。
pub struct InputPlugin<A: ActionMap = EmptyActions>(PhantomData<A>);

impl<A: ActionMap> Default for InputPlugin<A> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<A: ActionMap> Plugin for InputPlugin<A> {
    fn build(&self, app: &mut App) {
        app.init_resource::<InputStaging>()
            .init_resource::<SimulationTick>()
            .init_resource::<systems::FoldedInput>()
            .add_message::<InputActionPressed>()
            .add_message::<InputActionReleased>()
            // 新实体补组件：放在 Update，最迟下一帧生效（避免同帧 Commands 延迟陷阱）。
            .add_systems(Update, systems::ensure_input_components)
            // 确定性顺序：绝不依赖并行调度器的不确定顺序。
            .add_systems(
                FixedPreUpdate,
                (
                    systems::advance_simulation_tick,
                    systems::ingest_raw_input,
                    systems::resolve_controlled_entity,
                )
                    .chain()
                    .in_set(InputSet::Collect),
            )
            .add_systems(
                FixedPreUpdate,
                (systems::write_resolved_input, systems::record_input_history)
                    .chain()
                    .in_set(InputSet::Apply),
            )
            .add_systems(
                FixedPreUpdate,
                systems::derive_input_events::<A>.in_set(InputSet::Events),
            )
            .configure_sets(
                FixedPreUpdate,
                (
                    InputSet::Collect,
                    InputSet::Availability,
                    InputSet::Apply,
                    InputSet::Events,
                )
                    .chain(),
            );
    }
}
