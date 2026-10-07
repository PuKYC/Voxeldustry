//! 预测 / 回滚绑定。
//!
//! 单人预测 / 回滚链路已实现：
//!
//! - [`Predicted`]：标记参与预测 / 回滚的组件（纯表现副作用不挂，避免重演时重复触发）；
//! - [`Snapshot`]：回滚基线；[`apply_input`] / [`resimulate`]：可脱离 Bevy 单测的纯函数；
//! - [`record_local_history`] / [`PredictionPlugin`]：每逻辑 tick 写历史并挂引擎回滚时钟。
//!
//! 本地位移模型参数 [`MAX_SPEED`] / [`ACCEL`] / [`HISTORY_CAPACITY`] 在此定义，
//! 并与 `dev` 场景共用；输入类型的唯一真值在 [`crate::input::snapshot`]。
//!
//! 局域网 reconciliation 尚未实现（服务端权威 + 客户端重演，本期不做）。

use bevy::prelude::*;

use game_engine::presentation::RenderTransform;
use game_engine::rollback::{
    sample_render_states, RenderHistory, RenderSampleSystems, RollbackPlugin, RollbackState,
    RollbackSystems,
};

use crate::gameplay::Velocity;
use crate::input::snapshot::PlayerInput;
use crate::input::SimulationTick;

/// 参与预测/回滚的状态标记。
///
/// 受击震屏、粒子特效等纯表现副作用**不挂**此标记，否则回滚重演时会
/// 重复触发（例如死亡音效播放两次）。
#[derive(Component, Debug, Default)]
pub struct Predicted;

/// 预测快照：回滚的基线（参与预测的组件状态）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Snapshot {
    pub translation: Vec3,
    pub velocity: Vec3,
}

/// 纯函数：把输入应用到状态（带动量的一阶移动模型）。
///
/// `velocity` 朝输入决定的目标速度靠拢（带动量），再积分到 `translation`。
/// 抽成纯函数，便于脱离 Bevy 做回滚单测。
///
/// 注意：`look_yaw` / `look_pitch` 由朝向系统消费，**不回灌位移模型**。
pub fn apply_input(
    translation: Vec3,
    velocity: Vec3,
    input: PlayerInput,
    max_speed: f32,
    accel: f32,
    dt: f32,
) -> (Vec3, Vec3) {
    let target = Vec3::new(
        input.move_x.to_num::<f32>() * max_speed,
        input.move_y.to_num::<f32>() * max_speed,
        0.0,
    );
    let velocity = velocity + (target - velocity) * (accel * dt);
    let translation = translation + velocity * dt;
    (translation, velocity)
}

/// 从快照 + 输入历史重新模拟（回滚重演，L9 Reconciliation 的核心）。
pub fn resimulate(
    start: Snapshot,
    inputs: impl Iterator<Item = (u32, PlayerInput)>,
    max_speed: f32,
    accel: f32,
    dt: f32,
) -> Snapshot {
    let mut translation = start.translation;
    let mut velocity = start.velocity;
    for (_tick, input) in inputs {
        let (t, v) = apply_input(translation, velocity, input, max_speed, accel, dt);
        translation = t;
        velocity = v;
    }
    Snapshot {
        translation,
        velocity,
    }
}

/// 回滚历史容量（tick 数）。业务容量常量，属 game-core。
pub const HISTORY_CAPACITY: usize = 256;

/// 位移模型最大速度（与 demo 的本地玩家一致）。
pub const MAX_SPEED: f32 = 14.0;
/// 位移模型加速度。
pub const ACCEL: f32 = 8.0;

impl RollbackState for Snapshot {
    type Input = PlayerInput;
    type Sample = RenderTransform;

    /// 复用 [apply_input](crate::prediction::apply_input)：位置 f32，同平台确定。
    fn step(&self, input: &Self::Input, dt: f32) -> Self {
        let (translation, velocity) = apply_input(
            self.translation,
            self.velocity,
            *input,
            MAX_SPEED,
            ACCEL,
            dt,
        );
        Self {
            translation,
            velocity,
        }
    }

    fn sample(&self) -> Self::Sample {
        RenderTransform::from_translation(self.translation)
    }

    fn lerp(a: &Self::Sample, b: &Self::Sample, alpha: f32) -> Self::Sample {
        RenderTransform::lerp(a, b, alpha)
    }
}

/// 每个逻辑 tick：从 Transform / Velocity 抽取本地实体的 Snapshot，
/// 写入其 RenderHistory。
///
/// 单人下所有实体都是本地实体，因此不做额外权限过滤；局域网里由本 crate 的
/// net 路径决定哪些实体写 history 并触发重演（P3/P4，本期不做）。
pub fn record_local_history(
    tick: Res<SimulationTick>,
    mut query: Query<(&Transform, &Velocity, &mut RenderHistory<Snapshot>)>,
) {
    for (transform, velocity, mut history) in &mut query {
        history.history.insert(
            tick.0,
            Snapshot {
                translation: transform.translation,
                velocity: velocity.linear,
            },
        );
    }
}

/// 预测 / 回滚绑定的 core 装配。
///
/// 挂上引擎 RollbackPlugin（时钟 + 命名阶段），并把 Snapshot 的采样器与本地
/// 历史写入系统放进命名阶段。单人路径：全部 Predicted、delay 0、无重演。
pub struct PredictionPlugin;

impl Plugin for PredictionPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(RollbackPlugin)
            .add_systems(FixedUpdate, record_local_history.in_set(RollbackSystems))
            .add_systems(
                FixedPostUpdate,
                sample_render_states::<Snapshot>.in_set(RenderSampleSystems),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::snapshot::InputBuffer;
    use game_engine::math::FixedPoint;
    use game_engine::rng::RngState;

    fn sim(start: Snapshot, inputs: impl Iterator<Item = (u32, PlayerInput)>) -> Snapshot {
        let dt = 1.0 / 60.0;
        let max_speed = 10.0;
        let accel = 5.0;
        resimulate(start, inputs, max_speed, accel, dt)
    }

    #[test]
    fn rollback_resimulation_is_deterministic() {
        let dt = 1.0 / 60.0;
        let max_speed = 10.0;
        let accel = 5.0;

        // 用确定性 RNG 生成 100 tick 的输入序列。
        let mut rng = RngState::from_seed(123);
        let mut buffer = InputBuffer::new(256);
        let mut inputs = Vec::new();
        for tick in 0..100u32 {
            let input = PlayerInput {
                move_x: if rng.next_u32() % 2 == 0 {
                    FixedPoint::from_num(1)
                } else {
                    FixedPoint::from_num(-1)
                },
                ..PlayerInput::NONE
            };
            buffer.push(tick, input);
            inputs.push((tick, input));
        }

        // 正向模拟 0..100。
        let start = Snapshot {
            translation: Vec3::ZERO,
            velocity: Vec3::ZERO,
        };
        let forward = sim(start, inputs.iter().copied());

        // 快照到 tick=50，再回滚重演 50..100。
        let state50 = sim(start, inputs.iter().copied().take(50));
        let rolled_back = resimulate(state50, buffer.inputs_from(50), max_speed, accel, dt);

        assert_eq!(forward, rolled_back, "回滚重演必须与正向模拟逐位一致");
    }

    #[test]
    fn input_buffer_keeps_ring_window() {
        let mut buffer = InputBuffer::new(4);
        for tick in 0..10u32 {
            buffer.push(tick, PlayerInput::NONE);
        }
        // 只保留最近 4 个 tick：6..=9
        let ticks: Vec<u32> = buffer.inputs_from(0).map(|(t, _)| t).collect();
        assert_eq!(ticks, vec![6, 7, 8, 9]);
    }

    #[test]
    fn look_delta_does_not_affect_translation() {
        let dt = 1.0 / 60.0;
        let max_speed = 10.0;
        let accel = 5.0;
        let start = Vec3::ZERO;
        let velocity = Vec3::ZERO;

        let without_look = apply_input(start, velocity, PlayerInput::NONE, max_speed, accel, dt);
        let with_look = apply_input(
            start,
            velocity,
            PlayerInput {
                look_yaw: FixedPoint::from_num(1),
                look_pitch: FixedPoint::from_num(1),
                ..PlayerInput::NONE
            },
            max_speed,
            accel,
            dt,
        );
        assert_eq!(without_look, with_look, "视角输入不得影响位移模型");
    }

    #[test]
    fn single_player_history_and_sampler_close_the_loop() {
        use crate::presentation::PresentedTransform;
        use game_engine::rollback::RenderClock;

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(SimulationTick(1));
        app.insert_resource(RenderClock {
            tick: 1,
            ..Default::default()
        });
        app.add_systems(FixedUpdate, record_local_history);
        app.add_systems(FixedPostUpdate, sample_render_states::<Snapshot>);

        let entity = app
            .world_mut()
            .spawn((
                Transform::from_xyz(3.0, 0.0, 0.0),
                Velocity {
                    linear: Vec3::new(1.0, 0.0, 0.0),
                },
                RenderHistory::<Snapshot>::predicted(HISTORY_CAPACITY),
                PresentedTransform::default(),
            ))
            .id();

        app.finish();
        app.cleanup();
        app.world_mut().run_schedule(FixedUpdate);
        app.world_mut().run_schedule(FixedPostUpdate);

        let history = app
            .world()
            .entity(entity)
            .get::<RenderHistory<Snapshot>>()
            .unwrap();
        assert_eq!(
            history.history.get(1).map(|s| s.translation),
            Some(Vec3::new(3.0, 0.0, 0.0)),
            "本地实体每个逻辑 tick 必须写一条 Snapshot"
        );

        let presented = app
            .world()
            .entity(entity)
            .get::<PresentedTransform>()
            .unwrap();
        assert_eq!(presented.curr.position, [3.0, 0.0, 0.0]);
        assert_eq!(presented.prev.position, [3.0, 0.0, 0.0]);
    }
}
