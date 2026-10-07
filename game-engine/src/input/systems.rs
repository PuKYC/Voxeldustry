//! 输入系统链骨架。
//!
//! 调度顺序是**显式**的（`.chain()` + 命名 [`super::InputSet`]），因为确定性
//! 边界要求「不依赖并行调度器的不确定顺序」。引擎提供与具体能力无关的
//! 骨架；游戏侧把「能力 − 禁用 = 可用」的求解插进 `InputSet::Availability`。
//!
//! 1. `advance_simulation_tick` —— 推进逻辑 tick
//! 2. `ingest_raw_input` —— 把暂存区折叠成本 tick 的原始输入
//! 3. `resolve_controlled_entity` —— 输入源 → 被控实体
//! 4. （游戏侧）`compute_input_availability` —— 能力组件 − 禁用状态
//! 5. `write_resolved_input` —— 原始 ∩ 可用 = `ResolvedInput`
//! 6. `record_input_history` —— 每 tick 一条进 `InputBuffer`
//! 7. `derive_input_events` —— 派生瞬时事件（按下 / 松开）

use std::collections::HashMap;

use bevy::prelude::*;

use crate::identity::{StableEntityId, StableEntityIndex};

use super::actions::{ActionMap, MAX_INPUT_CHANNELS};
use super::capability::InputAvailability;
use super::control::{ControlledBy, ControlsEntity, InputSource, InputSourceId};
use super::events::{InputActionPressed, InputActionReleased};
use super::snapshot::{InputBuffer, RawResolved, ResolvedInput};
use super::staging::{InputStaging, TickInput};
use super::SimulationTick;

/// 本 tick 折叠后的原始输入，按输入源索引。
#[derive(Resource, Default, Debug)]
pub struct FoldedInput {
    pub by_source: HashMap<InputSourceId, TickInput>,
}

impl FoldedInput {
    pub fn get(&self, id: InputSourceId) -> Option<&TickInput> {
        self.by_source.get(&id)
    }
}

/// 1. 推进逻辑 tick。
pub fn advance_simulation_tick(mut tick: ResMut<SimulationTick>) {
    tick.0 = tick.0.wrapping_add(1);
}

/// 2. 把暂存区折叠成本 tick 的原始输入（每个输入源一份）。
pub fn ingest_raw_input(
    staging: Res<InputStaging>,
    sources: Query<&InputSource>,
    mut folded: ResMut<FoldedInput>,
) {
    folded.by_source.clear();
    for source in &sources {
        if let Some(tick_input) = staging.take_for_tick(source.id) {
            folded.by_source.insert(source.id, tick_input);
        }
    }
}

/// 3. 输入源 → 被控实体。
///
/// 通过 [`StableEntityIndex`] 做 `StableEntityId → Entity` 解析，
/// 因此「附身傀儡」只要改 `ControlsEntity.target` 即可。
pub fn resolve_controlled_entity(
    folded: Res<FoldedInput>,
    sources: Query<(&InputSource, &ControlsEntity)>,
    index: Res<StableEntityIndex>,
    mut targets: Query<&mut RawResolved>,
) {
    for (source, controls) in &sources {
        let Some(tick_input) = folded.get(source.id) else {
            continue;
        };
        let Some(entity) = index.entity(controls.target) else {
            continue;
        };
        let Ok(mut raw) = targets.get_mut(entity) else {
            continue;
        };
        raw.0 = tick_input.to_player_input();
    }
}

/// 5. 原始 ∩ 可用 = `ResolvedInput`。
///
/// 可用掩码由游戏侧的求解系统写入 [`InputAvailability`]。
pub fn write_resolved_input(
    mut query: Query<(&RawResolved, &InputAvailability, &mut ResolvedInput)>,
) {
    for (raw, availability, mut resolved) in &mut query {
        resolved.0 = raw.0.filtered(availability.mask);
    }
}

/// 6. 每 tick 记录一条输入历史（供回滚重放）。
pub fn record_input_history(
    tick: Res<SimulationTick>,
    mut query: Query<(&ResolvedInput, &mut InputBuffer)>,
) {
    for (resolved, mut buffer) in &mut query {
        buffer.push(tick.0, resolved.0);
    }
}

/// 7. 派生瞬时事件（按下 / 松开）。
///
/// 具体游戏若有额外语义事件（如交互请求），在游戏侧向
/// [`super::InputSet::Events`] 追加自己的系统。
pub fn derive_input_events<A: ActionMap>(
    tick: Res<SimulationTick>,
    query: Query<(&StableEntityId, &ResolvedInput)>,
    mut pressed_writer: MessageWriter<InputActionPressed>,
    mut released_writer: MessageWriter<InputActionReleased>,
) {
    let mut pressed_msgs = Vec::new();
    let mut released_msgs = Vec::new();

    for (id, resolved) in &query {
        for bit in 0..MAX_INPUT_CHANNELS {
            let bit_mask = 1u64 << bit;
            let Some(action) = A::action_of_bit(bit) else {
                continue;
            };

            if resolved.0.pressed & bit_mask != 0 {
                pressed_msgs.push(InputActionPressed {
                    entity: *id,
                    action,
                    tick: tick.0,
                });
            }

            if resolved.0.released & bit_mask != 0 {
                released_msgs.push(InputActionReleased {
                    entity: *id,
                    action,
                    tick: tick.0,
                });
            }
        }
    }

    if !pressed_msgs.is_empty() {
        pressed_writer.write_batch(pressed_msgs);
    }
    if !released_msgs.is_empty() {
        released_writer.write_batch(released_msgs);
    }
}

/// 确保被控实体带着输入管线需要的组件。
///
/// 放在 `Update`（不是 `FixedPreUpdate`）：新生成的实体最迟下一帧就能拿到组件，
/// 而 `FixedPreUpdate` 里的系统可以全部使用 `Query<&mut T>` 而不需要 `Commands`，
/// 避免「本帧插入、本帧查不到」的延迟陷阱。
pub fn ensure_input_components(
    mut commands: Commands,
    query: Query<
        (
            Entity,
            Option<&RawResolved>,
            Option<&ResolvedInput>,
            Option<&InputAvailability>,
            Option<&InputBuffer>,
        ),
        With<ControlledBy>,
    >,
) {
    for (entity, raw, resolved, availability, buffer) in &query {
        if raw.is_none() {
            commands.entity(entity).insert(RawResolved::default());
        }
        if resolved.is_none() {
            commands.entity(entity).insert(ResolvedInput::default());
        }
        if availability.is_none() {
            commands.entity(entity).insert(InputAvailability::default());
        }
        if buffer.is_none() {
            commands.entity(entity).insert(InputBuffer::default());
        }
    }
}
