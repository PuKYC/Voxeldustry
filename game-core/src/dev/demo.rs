//! 端到端演示：让「Godot 输入 → 模拟 → 表现同步」整条管线在一个场景里跑起来。
//!
//! **生产项目应当替换/删除本模块**（它只是把管线接通、便于肉眼验证）。
//!
//! 演示内容：
//! - 1 个本地玩家（挂 `LocalPlayer` / `AoiObserver` / 各种能力通道）
//! - 1 个输入源实体（通过 `ControlsEntity` 指向玩家）
//! - 24 个游荡实体，其中一部分带**隐私组件** `ExactHealth`：
//!   - `CoreRequiredPerception::PUBLIC` → 血量会下发
//!   - `CoreRequiredPerception::new(0b0100)` 而玩家只有 `0b0001` → 血量**永不下发**
//!     （验证「可见性过滤真值只有一份」）
//! - 每隔一段时间派生一条 `PresentationEvent`（验证事件通道不丢）

use bevy::prelude::*;

use super::spawn_local_player;
use crate::gameplay::{GameplaySet, Velocity};
use crate::input::control::{InputSourceId, LocalPlayer};
use crate::input::snapshot::ResolvedInput;
use crate::input::SimulationTick;
use crate::prediction::{apply_input, ACCEL, MAX_SPEED};
use crate::presentation::event::{PresentationEvent, SoundId};
use crate::presentation::payload::PresentationState;
use crate::presentation::{PresentationEventQueue, PresentedTransform};
use crate::privacy::{CoreRequiredPerception, ExactHealth};
use crate::static_data::prototype::Prototype;
use game_engine::identity::{StableEntityId, StableIdWorldExt};
use game_engine::rng::RngState;
use game_engine::spatial::Size;

/// 本地输入源 ID。
pub const LOCAL_SOURCE: InputSourceId = InputSourceId(1);

/// 演示用游荡参数（位置只由 tick 决定，不用随机数）。
#[derive(Component, Clone, Copy, Debug)]
pub struct DemoDrifter {
    pub base: f32,
    pub speed: f32,
    pub span: f32,
}

/// 演示插件（纯实体演示）。
///
/// 地形不在演示插件里：它属于体素层，由装配方在 `GameVoxelPlugin` 之前
/// `insert_resource(TerrainConfig)` 启用（见 `BevyBackendConfig::terrain`）。
#[derive(Clone, Copy, Debug, Default)]
pub struct DemoPlugin;

impl Plugin for DemoPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_demo_world)
            // 位移挂在 Motion 阶段，保证回滚历史每 tick 记录到推进后的 Transform。
            .add_systems(
                FixedUpdate,
                (demo_move_player, demo_move_drifters).in_set(GameplaySet::Motion),
            )
            .add_systems(FixedUpdate, demo_emit_events.in_set(GameplaySet::Reaction));
    }
}

/// 建场景。用 exclusive system（`&mut World`）以便使用 `spawn_stable`，
/// 让 `StableEntityIndex` 在**当帧**就建立好映射，不留一帧输入真空。
fn spawn_demo_world(world: &mut World) {
    // ── 本地玩家 ──
    //
    // 组件清单与两次 insert 的样板收在 spawn_local_player! 里（Bevy 元组 Bundle
    // 上限 15 个元素）；输入管线需要的组件即使不全，也会被 ensure_input_components
    // 补上（最迟下一帧生效）。
    let (_player_entity, _player_id) = spawn_local_player!(
        world,
        position = Vec3::ZERO,
        source = LOCAL_SOURCE,
        observer = 70.0,
        extra = (),
    );

    // ── 游荡实体 ──
    let mut rng = RngState::from_seed(0x5EED);
    for index in 0..24u32 {
        let grid_x = (rng.next_u32() % 9) as i32 - 4;
        let grid_z = (rng.next_u32() % 9) as i32 - 4;

        let base_x = (grid_x * 38) as f32;
        let base_z = (grid_z * 38) as f32;

        // 三种隐私配置，用于肉眼验证可见性过滤：
        //   0 → 公开血量
        //   1 → 需要玩家没有的感知位（血量永不下发）
        //   2 → 根本没有血量组件
        let privacy = index % 3;
        let prototype = if privacy == 2 { 2 } else { 3 };

        let speed = 1.0 + (index % 4) as f32;
        let span = (30 + (index % 5) * 10) as f32;

        let (entity, _id) = world.spawn_stable((
            Transform::from_xyz(base_x, 0.0, base_z),
            // 没有 `Size` 就进不了空间索引，会永远落在玩家 AOI 之外。
            Size(None),
            Prototype::new(prototype),
            PresentationState::idle().with_locomotion(1),
            DemoDrifter {
                base: base_x,
                speed,
                span,
            },
            CoreRequiredPerception::PUBLIC,
            PresentedTransform::default(),
        ));

        match privacy {
            0 => {
                let mut entity_mut = world.entity_mut(entity);
                entity_mut.insert((
                    ExactHealth::full(40 + index as i32),
                    CoreRequiredPerception::PUBLIC,
                ));
            }
            1 => {
                let mut entity_mut = world.entity_mut(entity);
                entity_mut.insert((
                    ExactHealth::full(40 + index as i32),
                    // 玩家只有 0b0001，这里b0100 → 永远不可见。
                    CoreRequiredPerception::new(0b0100),
                ));
            }
            _ => {}
        }
    }
}

/// 玩家：消费 `ResolvedInput`（**已经过能力过滤**），推进。
fn demo_move_player(
    time: Res<Time<Fixed>>,
    // 直接读写真实速度组件（与 record_local_history 读的是同一份状态），
    // 回滚重演因此不会丢动量。
    mut query: Query<(&mut Transform, &mut Velocity, &ResolvedInput), With<LocalPlayer>>,
) {
    let dt = time.delta_secs();

    for (mut transform, mut velocity, input) in &mut query {
        let (translation, new_velocity) = apply_input(
            transform.translation,
            velocity.linear,
            input.0,
            MAX_SPEED,
            ACCEL,
            dt,
        );
        transform.translation = translation;
        velocity.linear = new_velocity;
    }
}

/// 游荡实体：位置**只由 tick 决定**（可回放），沿 X 轴来回。
fn demo_move_drifters(
    tick: Res<SimulationTick>,
    mut query: Query<(&mut Transform, &DemoDrifter), Without<LocalPlayer>>,
) {
    let now = tick.0 as f32;
    for (mut transform, drifter) in &mut query {
        let x = wrap_span(drifter.base + now * drifter.speed, drifter.span);
        let z = transform.translation.z;
        transform.translation = Vec3::new(x, 0.0, z);
    }
}

/// 把 `value` 折叠到 `[-span/2, span/2)`。
fn wrap_span(value: f32, span: f32) -> f32 {
    let half = span / 2.0;
    let shifted = value + half;
    let periods = (shifted / span).floor();
    shifted - periods * span - half
}

/// 定期派生表现事件（验证「事件通道不丢、保序」）。
fn demo_emit_events(
    tick: Res<SimulationTick>,
    queue: Res<PresentationEventQueue>,
    players: Query<&StableEntityId, With<LocalPlayer>>,
) {
    // 每 3 秒（60Hz * 3）一次。
    if tick.0 == 0 || tick.0 % 180 != 0 {
        return;
    }

    for id in &players {
        if !queue.push(
            tick.0,
            PresentationEvent::PlaySound {
                id: *id,
                sound: SoundId(1),
                position: [0.0, 0.0, 0.0],
            },
        ) {
            error!(
                "表现事件队列已满，事件被丢弃（已丢弃 {} 条）",
                queue.dropped()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_span_keeps_value_inside_range() {
        let span = 100.0;
        for step in -50..50 {
            let value = (step * 7) as f32;
            let wrapped = wrap_span(value, span);
            assert!(
                wrapped >= -50.0 && wrapped < 50.0,
                "wrap 结果越界: {wrapped:?}"
            );
        }
    }

    #[test]
    fn wrap_span_is_periodic() {
        let span = 10.0;
        let value = 3.0;
        assert_eq!(wrap_span(value, span), wrap_span(value + span * 4.0, span));
    }

    /// Bug 1 回归：本地玩家移动后，写进回滚历史的 `Snapshot.velocity` 必须非零。
    ///
    /// 曾经玩家由 `DemoVelocity` 驱动、而 `record_local_history` 读的是恒为
    /// 零的 `Velocity`，P3 的 step/resimulate 因此会丢动量。
    #[test]
    fn local_player_snapshot_velocity_is_nonzero_after_moving() {
        use crate::input::snapshot::PlayerInput;
        use crate::prediction::{record_local_history, Snapshot, HISTORY_CAPACITY};
        use game_engine::math::FixedPoint;
        use game_engine::rollback::RenderHistory;
        use std::time::Duration;

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(Time::<Fixed>::from_hz(60.0));
        app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(
            Duration::from_millis(20),
        ));
        app.insert_resource(SimulationTick(1));
        app.add_systems(
            FixedUpdate,
            (demo_move_player, record_local_history).chain(),
        );

        let entity = app
            .world_mut()
            .spawn((
                LocalPlayer,
                Transform::from_translation(Vec3::ZERO),
                Velocity { linear: Vec3::ZERO },
                ResolvedInput(PlayerInput {
                    move_x: FixedPoint::from_num(1),
                    ..PlayerInput::NONE
                }),
                RenderHistory::<Snapshot>::predicted(HISTORY_CAPACITY),
            ))
            .id();

        // fixed 累积可能让某次 update 不产生逻辑步，多跑几次直到有记录。
        let mut latest_velocity = None;
        for _ in 0..8 {
            app.update();
            latest_velocity = app
                .world()
                .entity(entity)
                .get::<RenderHistory<Snapshot>>()
                .unwrap()
                .history
                .latest()
                .map(|snapshot| snapshot.velocity);
            if latest_velocity.is_some() {
                break;
            }
        }

        let velocity = latest_velocity.expect("玩家至少应记录一条历史快照");
        assert_ne!(
            velocity,
            Vec3::ZERO,
            "本地玩家移动后 Snapshot.velocity 必须非零"
        );
    }
}
