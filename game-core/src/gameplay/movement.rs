use bevy::prelude::*;

use game_engine::strategy::{MovementModel, MovementStrategy};

use crate::input::control::LocalPlayer;
use crate::spec::CoreSpec;

pub fn integrate_position(translation: Vec3, velocity: Vec3, dt: f32) -> Vec3 {
    translation + velocity * dt
}

pub fn apply_drag(velocity: Vec3, damping: f32) -> Vec3 {
    velocity * damping
}

/// 阻尼的调参基准：在 `CoreSpec::FIXED_HZ` 下每 tick 保留的速度比例。
///
/// 手感按**每秒**衰减定标（`0.5^60` 每秒）；实际每个 tick 的系数由
/// [`drag_decay_factor`] 按真实步长 `dt` 换算，因此换 `fixed_hz` 时
/// 单位时间的减速不变。
pub const DRAG_BASE_PER_TICK: f32 = 0.5;

/// 把基准每-tick 阻尼系数换算到实际步长 `dt`（秒）。
///
/// `factor = DRAG_BASE_PER_TICK^(dt * FIXED_HZ)`：60Hz 下 ≈ 0.5/tick、
/// 30Hz 下 ≈ 0.25/tick，一个真实秒的总衰减恒为 `0.5^60`。
///
/// 注意：这里引入了 `powf`（超越函数），确定性口径从「IEEE 跨平台逐位一致」
/// 降为「同平台 / 同 libm 一致」；位置积分本身仍是普通 f32 四则运算。
/// 若将来需要跨平台回放，应把阻尼改成定点整数近似。
pub fn drag_decay_factor(dt: f32) -> f32 {
    let nominal_hz = <CoreSpec as game_engine::spec::GameSpec>::FIXED_HZ as f32;
    DRAG_BASE_PER_TICK.powf(dt * nominal_hz)
}

/// 默认位移模型：一阶积分 + 按真实时间衰减的指数阻尼。
///
/// M5：这是可被 mod 用 ModContext::set_strategy 替换的默认策略。
#[derive(Debug, Default)]
pub struct LinearMovementModel;

impl MovementModel for LinearMovementModel {
    fn integrate(&self, translation: Vec3, velocity: Vec3, dt: f32) -> Vec3 {
        integrate_position(translation, velocity, dt)
    }

    fn damp(&self, velocity: Vec3, dt: f32) -> Vec3 {
        apply_drag(velocity, drag_decay_factor(dt))
    }
}

/// 系统：移动（先跑）。位移模型从策略槽读取，mod 可整体替换。
///
/// 逻辑位置直接写在 Bevy 的 `Transform` 上；空间/AOI 读 `GlobalTransform`。
///
/// 本地可控实体（`LocalPlayer`）的位移由其输入控制器独占，通用速度积分器跳过。
pub fn movement_system(
    time: Res<Time<Fixed>>,
    mut query: Query<(&mut Transform, &Velocity), Without<LocalPlayer>>,
    strategy: Res<MovementStrategy<CoreSpec>>,
) {
    // 运行期逻辑步长的唯一真值是 `Time<Fixed>`（由 EngineConfig.fixed_hz 注入）。
    // 用 `timestep()` 而非 `delta_secs()`：直接跑 schedule 的测试里也稳定。
    let dt = time.timestep().as_secs_f32();
    for (mut transform, velocity) in query.iter_mut() {
        transform.translation = strategy.integrate(transform.translation, velocity.linear, dt);
    }
}

/// 系统：速度阻尼（后跑）。顺序由插件里 .chain() 显式声明。
///
/// 本地可控实体（`LocalPlayer`）的速度由其输入控制器独占，通用阻尼器跳过，
/// 避免与输入模型对同一份 `Velocity` 重复积分。
pub fn drag_system(
    time: Res<Time<Fixed>>,
    mut query: Query<&mut Velocity, Without<LocalPlayer>>,
    strategy: Res<MovementStrategy<CoreSpec>>,
) {
    let dt = time.timestep().as_secs_f32();
    for mut velocity in query.iter_mut() {
        velocity.linear = strategy.damp(velocity.linear, dt);
    }
}

pub struct MovementPlugin;

impl Plugin for MovementPlugin {
    fn build(&self, app: &mut App) {
        // 默认策略；mod 可在之后用 set_strategy 覆盖（后插入胜）。
        app.insert_resource(MovementStrategy::<CoreSpec>::new(LinearMovementModel))
            .add_systems(
                FixedUpdate,
                (movement_system, drag_system)
                    .chain()
                    .in_set(super::GameplaySet::Motion),
            );
    }
}

/// 线速度（单位：距离/秒）。
#[derive(Component, Debug, Clone)]
pub struct Velocity {
    pub linear: Vec3,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 替换用策略：完全不动（证明策略真的被系统消费）。
    #[derive(Debug, Default)]
    struct FrozenModel;

    impl MovementModel for FrozenModel {
        fn integrate(&self, translation: Vec3, _velocity: Vec3, _dt: f32) -> Vec3 {
            translation
        }
        fn damp(&self, velocity: Vec3, _dt: f32) -> Vec3 {
            velocity
        }
    }

    fn build(with_override: bool) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        // 显式固定为 CoreSpec 的名义频率，避免落到 Bevy 默认的 64Hz。
        app.insert_resource(Time::<Fixed>::from_hz(60.0));
        app.add_plugins(MovementPlugin);
        if with_override {
            app.insert_resource(MovementStrategy::<CoreSpec>::new(FrozenModel));
        }
        for i in 0..8i32 {
            app.world_mut().spawn((
                Transform::from_translation(Vec3::ZERO),
                Velocity {
                    linear: Vec3::new(i as f32 + 1.0, 0.0, 0.0),
                },
            ));
        }
        app.finish();
        app.cleanup();
        app
    }

    fn tick(app: &mut App) {
        app.world_mut().run_schedule(FixedUpdate);
    }

    fn positions(app: &mut App) -> Vec<u32> {
        let world = app.world_mut();
        let mut query = world.query::<&Transform>();
        let mut xs: Vec<u32> = query
            .iter(world)
            .map(|t| t.translation.x.to_bits())
            .collect();
        xs.sort_unstable();
        xs
    }

    #[test]
    fn replaced_strategy_changes_movement_and_stays_deterministic() {
        let mut a = build(false);
        let mut b = build(false);
        for _ in 0..10 {
            tick(&mut a);
            tick(&mut b);
        }
        assert_eq!(
            positions(&mut a),
            positions(&mut b),
            "默认策略两次运行必须逐位一致"
        );
        assert!(
            positions(&mut a).iter().any(|&x| x != 0),
            "默认策略应产生位移"
        );

        let mut c = build(true);
        let mut d = build(true);
        for _ in 0..10 {
            tick(&mut c);
            tick(&mut d);
        }
        assert_eq!(
            positions(&mut c),
            positions(&mut d),
            "替换策略两次运行必须逐位一致"
        );
        assert!(
            positions(&mut c).iter().all(|&x| x == 0),
            "FrozenModel 不应产生位移"
        );
        assert_ne!(
            positions(&mut a),
            positions(&mut c),
            "替换策略必须真正改变结果"
        );
    }

    /// 积分正常、阻尼不动：把「位移」从「减速」里隔离出来单独测。
    #[derive(Debug, Default)]
    struct NoDampModel;

    impl MovementModel for NoDampModel {
        fn integrate(&self, translation: Vec3, velocity: Vec3, dt: f32) -> Vec3 {
            integrate_position(translation, velocity, dt)
        }
        fn damp(&self, velocity: Vec3, _dt: f32) -> Vec3 {
            velocity
        }
    }

    /// 以 `hz` 逻辑频率跑 `ticks` 个 tick，返回实体最终 x。
    fn distance_after(hz: f64, ticks: u32) -> f32 {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(Time::<Fixed>::from_hz(hz));
        app.add_plugins(MovementPlugin);
        app.insert_resource(MovementStrategy::<CoreSpec>::new(NoDampModel));
        let entity = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::ZERO),
                Velocity {
                    linear: Vec3::new(6.0, 0.0, 0.0),
                },
            ))
            .id();
        app.finish();
        app.cleanup();
        for _ in 0..ticks {
            app.world_mut().run_schedule(FixedUpdate);
        }
        app.world()
            .entity(entity)
            .get::<Transform>()
            .unwrap()
            .translation
            .x
    }

    /// 核心回归：同样 0.5 秒真实时间，60Hz×30 tick 与 30Hz×15 tick 位移一致。
    #[test]
    fn forward_distance_is_invariant_to_fixed_hz() {
        let fast = distance_after(60.0, 30);
        let slow = distance_after(30.0, 15);

        assert!((fast - 3.0).abs() < 1e-4, "60Hz 位移应 ≈ 3.0，得到 {fast}");
        assert!((slow - 3.0).abs() < 1e-4, "30Hz 位移应 ≈ 3.0，得到 {slow}");
        assert!(
            (fast - slow).abs() < 1e-4,
            "换 fixed_hz 后单位时间位移必须不变：{fast} vs {slow}"
        );
    }

    /// 阻尼同样按真实时间：60Hz 基准手感不变，其余频率按 dt 换算。
    #[test]
    fn drag_decay_is_time_based() {
        let at_60 = drag_decay_factor(1.0 / 60.0);
        let at_30 = drag_decay_factor(1.0 / 30.0);
        let at_120 = drag_decay_factor(1.0 / 120.0);

        assert!(
            (at_60 - 0.5).abs() < 1e-5,
            "60Hz 基准应 ≈ 0.5，得到 {at_60}"
        );
        assert!(
            (at_30 - at_60 * at_60).abs() < 1e-6,
            "30Hz 一 tick 应等于 60Hz 两 tick"
        );
        assert!(
            (at_120 - at_60.sqrt()).abs() < 1e-6,
            "120Hz 一 tick 应等于 60Hz 半 tick"
        );

        // 同样 0.1 秒真实时间，速度衰减一致。
        let mut fast = Vec3::new(10.0, 0.0, 0.0);
        for _ in 0..6 {
            fast = apply_drag(fast, at_60);
        }
        let mut slow = Vec3::new(10.0, 0.0, 0.0);
        for _ in 0..3 {
            slow = apply_drag(slow, at_30);
        }
        assert!(
            (fast.x - slow.x).abs() < 1e-3,
            "0.1 秒后速度应一致：{fast:?} vs {slow:?}"
        );
    }
}
