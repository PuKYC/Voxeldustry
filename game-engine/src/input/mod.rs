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

pub use actions::{
    ActionDef, ActionId, ActionMap, ActionResolveError, ChannelMask, EmptyActions, ResolvedActions,
    ACTION_NAME_MAX_BYTES, MAX_INPUT_CHANNELS, MOD_ACTION_ID_BASE, MOD_ACTION_ID_END,
};
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

/// 只读会话标记：装配前插入，冻结模拟时钟与整条输入链路。
///
/// 只读模式仍然照常跑 `FixedUpdate`（流式加载 / 存档 tick 依赖它），
/// 因此**不能**用 `Time<Virtual>` 暂停或后端 Pause 来实现。
///
/// 必须在 `InputPlugin::build` 之前插入（即 `add_plugins` 之前）；
/// 装配后再插不生效——系统已经装上了。
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadOnlySession;

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

        // 只读会话：装配前插入该资源时，输入链路整体不装。
        // 效果是一次性冻结「时钟推进 / 折叠 / 控制关系 / ResolvedInput / 历史 / 事件」，
        // 而 FixedUpdate 本身照常跑（流式加载、存档 tick 依赖它，参见 SaveSet / GameTickSet）。
        // 必须用资源而不是给插件加字段：InputPlugin 是无字段 PhantomData，
        // 且装配方（CoreGameModule）只能在 add_plugins 之前 insert_resource。
        if app.world().contains_resource::<ReadOnlySession>() {
            return;
        }

        // 确定性顺序：绝不依赖并行调度器的不确定顺序。
        app.add_systems(
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
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::input::control::InputSourceId;
    use crate::input::raw::RawInputFrame;
    use crate::input::staging::InputStaging;

    fn app_with(read_only: bool) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        // 真实装配里由 StableId 插件提供；输入链只读它做 StableId → Entity 解析。
        app.init_resource::<crate::identity::StableEntityIndex>();
        if read_only {
            app.insert_resource(ReadOnlySession);
        }
        app.add_plugins(InputPlugin::<EmptyActions>::default());
        app
    }

    #[test]
    fn readonly_session_freezes_tick_and_skips_input_chain() {
        let mut app = app_with(true);
        app.insert_resource(SimulationTick(7));
        app.world().resource::<InputStaging>().submit(
            InputSourceId::default(),
            RawInputFrame {
                held: 0b1,
                pressed_latch: 0b1,
                ..Default::default()
            },
        );
        // 放一个输入源实体，确保「是否消费暂存区」不是因为没源可查。
        app.world_mut().spawn(InputSource {
            id: InputSourceId::default(),
        });

        for _ in 0..3 {
            app.world_mut().run_schedule(FixedPreUpdate);
        }

        assert_eq!(
            app.world().resource::<SimulationTick>().0,
            7,
            "只读会话不推进逻辑 tick"
        );
        assert!(
            app.world()
                .resource::<systems::FoldedInput>()
                .by_source
                .is_empty(),
            "只读会话不折叠输入"
        );
        let leaked = app
            .world()
            .resource::<InputStaging>()
            .take_for_tick(InputSourceId::default())
            .expect("输入源仍在暂存区");
        assert_eq!(leaked.pressed, 0b1, "只读会话不消费边沿，避免静默丢弃");
    }

    #[test]
    fn normal_session_advances_tick_and_consumes_staging() {
        let mut app = app_with(false);
        app.world().resource::<InputStaging>().submit(
            InputSourceId::default(),
            RawInputFrame {
                held: 0b1,
                pressed_latch: 0b1,
                ..Default::default()
            },
        );
        // 放一个输入源实体，确保「是否消费暂存区」不是因为没源可查。
        app.world_mut().spawn(InputSource {
            id: InputSourceId::default(),
        });

        for _ in 0..3 {
            app.world_mut().run_schedule(FixedPreUpdate);
        }

        assert_eq!(app.world().resource::<SimulationTick>().0, 3);
        assert_eq!(
            app.world()
                .resource::<systems::FoldedInput>()
                .by_source
                .len(),
            1,
            "正常会话折叠输入"
        );
        let consumed = app
            .world()
            .resource::<InputStaging>()
            .take_for_tick(InputSourceId::default())
            .expect("输入源仍在暂存区");
        assert_eq!(consumed.pressed, 0, "正常会话已消费边沿");
    }
}
