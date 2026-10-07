//! 预测 / 插值共用的回滚与渲染采样机制（引擎层，泛型，无业务）。
//!
//! 引擎只认识「能 step、能 sample、能 lerp」的状态契约与「按 tick 的历史」，
//! 不认识任何具体组件 / 玩法数值 / 字符串。具体状态（core 的 Snapshot）、
//! 输入应用与策略分配全部留在 game-core。
//!
//! 三层概念：
//!
//! - StateHistory<S>：某实体按 tick 的状态历史（环形缓冲，可回滚截断）；
//! - RenderPolicy：该实体按预测还是按固定延迟插值被采样；
//! - sample_render_states：按策略从历史取两端点，写入既有表现端点
//!   PresentedTransform，不改变 GPF1 载荷布局与 PayloadKind code。
//!
//! ## 与旧 advance_render_transforms 的关系
//!
//! 旧的 advance_render_transforms 每个逻辑 tick 把 Transform 推成
//! (prev = state(t-1), curr = state(t))。预测实体的采样器取「最近两个历史
//! 端点」，得到同样的两端点；RenderPolicy::Predicted 因此与该路径等价，
//! 只是数据源换成了 StateHistory（等待回滚重写时不被瞬时值污染）。
//!
//! 采样器与旧推进系统都会写 PresentedTransform，因此在调度上必须
//! RenderSampleSystems.after(advance_render_transforms)（见 RollbackPlugin），
//! 由采样器覆盖挂 RenderHistory 的实体，其余实体保持旧行为。

use std::collections::VecDeque;

use bevy::prelude::*;

use crate::input::SimulationTick;
use crate::presentation::interp::{
    advance_render_transforms, record_tick_clock, PresentedTransform, RenderClockState,
    RenderTransform,
};
use crate::sim::EngineSet;

/// 回滚相关系统的稳定命名阶段（FixedUpdate 内，逻辑推进之后）。
///
/// game-core 把「每 tick 抽取本地实体状态写入 history」的系统挂进本集合。
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RollbackSystems;

/// 渲染采样系统的稳定命名阶段（FixedPostUpdate 内）。
///
/// game-core 把 sample_render_states::<Snapshot> 挂进本集合。
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RenderSampleSystems;

/// 参与预测 / 回滚 / 插值的一帧状态契约。
///
/// step / lerp 必须是纯函数（无时钟、无线程、无全局态），延续确定性约束。
/// 引擎只认识本契约，不认识 S 的内容。
pub trait RollbackState: Clone + Send + Sync + 'static {
    /// 驱动输入（由输入历史提供）。
    type Input: Clone + Send + Sync + 'static;
    /// 表现采样输出（例如 RenderTransform）。
    type Sample: Clone + Send + Sync + 'static;

    /// 纯函数推进一个 tick。
    fn step(&self, input: &Self::Input, dt: f32) -> Self;

    /// 转成表现采样。
    fn sample(&self) -> Self::Sample;

    /// 两端点混合（alpha 已由渲染时钟算好）。
    fn lerp(a: &Self::Sample, b: &Self::Sample, alpha: f32) -> Self::Sample;
}

/// 某实体按 tick 的状态历史（环形缓冲，tick 单调递增）。
///
/// 同一 tick 重复写入表示覆盖（回滚重演后重写）。容量满时丢弃最旧一条。
#[derive(Clone, Debug)]
pub struct StateHistory<S> {
    entries: VecDeque<(u32, S)>,
    capacity: usize,
}

impl<S> StateHistory<S> {
    /// 新建环形缓冲；容量至少为 1。
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    /// 环形缓冲容量。
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 插入 / 覆盖某 tick。
    pub fn insert(&mut self, tick: u32, state: S) {
        match self.entries.binary_search_by_key(&tick, |(t, _)| *t) {
            Ok(index) => self.entries[index].1 = state,
            Err(index) => {
                self.entries.insert(index, (tick, state));
                while self.entries.len() > self.capacity {
                    self.entries.pop_front();
                }
            }
        }
    }

    /// 取某 tick 的状态。
    pub fn get(&self, tick: u32) -> Option<&S> {
        self.entries
            .binary_search_by_key(&tick, |(t, _)| *t)
            .ok()
            .map(|index| &self.entries[index].1)
    }

    /// 取「<= tick 的最近一条」与「>= tick 的最近一条」。
    ///
    /// tick 恰好命中时两端为同一条；越界时取最近端点。空历史返回 None。
    pub fn bracket(&self, tick: u32) -> Option<(&(u32, S), &(u32, S))> {
        if self.entries.is_empty() {
            return None;
        }
        let ge = self.entries.partition_point(|(t, _)| *t < tick);
        let hi = ge.min(self.entries.len() - 1);
        let le = self.entries.partition_point(|(t, _)| *t <= tick);
        let lo = le.saturating_sub(1);
        Some((&self.entries[lo], &self.entries[hi]))
    }

    /// 回滚：丢弃所有 tick > tick 的历史，准备重演重写。
    pub fn truncate_after(&mut self, tick: u32) {
        while self.entries.back().is_some_and(|(t, _)| *t > tick) {
            self.entries.pop_back();
        }
    }

    /// 最新（最大）tick。
    pub fn latest_tick(&self) -> Option<u32> {
        self.entries.back().map(|(tick, _)| *tick)
    }

    /// 最新一条状态。
    pub fn latest(&self) -> Option<&S> {
        self.entries.back().map(|(_, state)| state)
    }

    /// 最近两个端点 (prev, curr)；只有一条时二者指向同一条。
    pub fn latest_two(&self) -> Option<(&S, &S)> {
        match self.entries.len() {
            0 => None,
            1 => {
                let state = &self.entries[0].1;
                Some((state, state))
            }
            len => Some((&self.entries[len - 2].1, &self.entries[len - 1].1)),
        }
    }

    /// 历史条数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 历史是否为空。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 渲染策略：本地预测还是带固定延迟的远端插值。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RenderPolicy {
    /// 本地预测：取最近两个已记录 tick 作为端点（render 落后一个 tick）。
    #[default]
    Predicted,
    /// 远端插值：以 clock.tick - delay_ticks 为目标 tick，取包住它的两端点。
    Interpolated { delay_ticks: u32 },
}

/// 挂在实体上的渲染历史 + 策略。
#[derive(Component)]
pub struct RenderHistory<S: RollbackState> {
    pub history: StateHistory<S>,
    pub policy: RenderPolicy,
}

impl<S: RollbackState> RenderHistory<S> {
    /// 指定容量与策略。
    pub fn new(capacity: usize, policy: RenderPolicy) -> Self {
        Self {
            history: StateHistory::new(capacity),
            policy,
        }
    }

    /// 本地预测（单人默认）。
    pub fn predicted(capacity: usize) -> Self {
        Self::new(capacity, RenderPolicy::Predicted)
    }

    /// 远端插值（固定延迟）。
    pub fn interpolated(capacity: usize, delay_ticks: u32) -> Self {
        Self::new(capacity, RenderPolicy::Interpolated { delay_ticks })
    }
}

/// 渲染时钟：把逻辑 tick、tick 起始时刻与 fixed 步长交给采样器。
#[derive(Resource, Debug, Default)]
pub struct RenderClock {
    /// 最新已完成的逻辑 tick。
    pub tick: u32,
    /// 该 tick 发生时的进程单调时刻（复用现有 RenderClockState）。
    pub tick_clock_ms: Option<u64>,
    /// 逻辑定步时长（秒）。
    pub fixed_dt: f32,
    /// 预测实体在本 tick 内的推进量（Time<Fixed>::overstep_fraction）。
    pub overstep: f32,
}

/// 按策略从历史取两端点（纯函数，便于单测）。
///
/// - Predicted：取最近两个 tick 的状态；
/// - Interpolated { delay_ticks }：以 clock.tick - delay_ticks 为目标 tick，
///   取包住它的相邻两条。
pub fn sample_endpoints<'a, S: RollbackState>(
    history: &'a StateHistory<S>,
    policy: RenderPolicy,
    clock: &RenderClock,
) -> Option<(&'a S, &'a S)> {
    match policy {
        RenderPolicy::Predicted => history.latest_two(),
        RenderPolicy::Interpolated { delay_ticks } => {
            let target = clock.tick.saturating_sub(delay_ticks);
            let (lo, hi) = history.bracket(target)?;
            Some((&lo.1, &hi.1))
        }
    }
}

/// 引擎采样器：按策略把历史两端点写进既有表现端点 PresentedTransform。
///
/// Sample = RenderTransform 是「低风险路径」：不泛型化载荷、不改 GPF1。
/// 只处理挂 RenderHistory 的实体；其余实体仍由旧的
/// advance_render_transforms 推进。
pub fn sample_render_states<S>(
    clock: Res<RenderClock>,
    mut query: Query<(&RenderHistory<S>, &mut PresentedTransform)>,
) where
    S: RollbackState<Sample = RenderTransform>,
{
    for (render_history, mut presented) in &mut query {
        let Some((prev, curr)) =
            sample_endpoints(&render_history.history, render_history.policy, &clock)
        else {
            continue;
        };
        presented.set_sampled(prev.sample(), curr.sample());
    }
}

/// 每个 fixed tick 刷新 RenderClock（采样器的时间基准）。
///
/// tick_clock_ms 直接取 record_tick_clock 写入的同一把时钟，保证与
/// 下发给 Godot 的帧头一致。
pub fn record_render_clock(
    simulation_tick: Res<SimulationTick>,
    fixed: Res<Time<Fixed>>,
    render_clock_state: Res<RenderClockState>,
    mut clock: ResMut<RenderClock>,
) {
    clock.tick = simulation_tick.0;
    clock.tick_clock_ms = render_clock_state.tick_clock_ms;
    clock.fixed_dt = fixed.timestep().as_secs_f32();
    clock.overstep = fixed.overstep_fraction();
}

/// 回滚 / 采样机制插件：初始化时钟与命名阶段，注册引擎侧时钟系统。
///
/// 它不注册任何具体 S 的系统——那由 game-core 往命名阶段里挂。
pub struct RollbackPlugin;

impl Plugin for RollbackPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<RenderClock>()
            .init_resource::<RenderClockState>()
            .configure_sets(FixedUpdate, RollbackSystems.after(EngineSet::Cleanup))
            .configure_sets(
                FixedPostUpdate,
                RenderSampleSystems.after(advance_render_transforms),
            )
            .add_systems(
                FixedPostUpdate,
                record_render_clock
                    .after(record_tick_clock)
                    .before(RenderSampleSystems),
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq)]
    struct TestState {
        x: f32,
    }

    impl RollbackState for TestState {
        type Input = f32;
        type Sample = f32;

        fn step(&self, input: &f32, dt: f32) -> Self {
            Self {
                x: self.x + input * dt,
            }
        }

        fn sample(&self) -> f32 {
            self.x
        }

        fn lerp(a: &f32, b: &f32, alpha: f32) -> f32 {
            a + (b - a) * alpha
        }
    }

    fn state(x: f32) -> TestState {
        TestState { x }
    }

    #[test]
    fn insert_get_and_overwrite_same_tick() {
        let mut history = StateHistory::new(8);
        history.insert(1, state(1.0));
        history.insert(2, state(2.0));
        history.insert(2, state(20.0));

        assert_eq!(history.len(), 2);
        assert_eq!(history.get(1), Some(&state(1.0)));
        assert_eq!(history.get(2), Some(&state(20.0)));
        assert_eq!(history.get(3), None);
        assert_eq!(history.latest_tick(), Some(2));
    }

    #[test]
    fn insert_out_of_order_keeps_sorted() {
        let mut history = StateHistory::new(8);
        history.insert(5, state(5.0));
        history.insert(1, state(1.0));
        history.insert(3, state(3.0));

        assert_eq!(history.latest_tick(), Some(5));
        let (lo, hi) = history.bracket(2).expect("应有包围端点");
        assert_eq!((lo.0, hi.0), (1, 3));
    }

    #[test]
    fn ring_buffer_drops_oldest() {
        let mut history = StateHistory::new(3);
        for tick in 0..5u32 {
            history.insert(tick, state(tick as f32));
        }
        assert_eq!(history.len(), 3);
        assert_eq!(history.get(0), None);
        assert_eq!(history.get(2).map(|s| s.x), Some(2.0));
        assert_eq!(history.latest_tick(), Some(4));
    }

    #[test]
    fn bracket_returns_enclosing_pair() {
        let mut history = StateHistory::new(8);
        history.insert(0, state(0.0));
        history.insert(5, state(5.0));
        history.insert(10, state(10.0));

        let (lo, hi) = history.bracket(5).unwrap();
        assert_eq!((lo.0, hi.0), (5, 5), "命中 tick 时两端相同");

        let (lo, hi) = history.bracket(7).unwrap();
        assert_eq!((lo.0, hi.0), (5, 10));

        let (lo, hi) = history.bracket(0).unwrap();
        assert_eq!((lo.0, hi.0), (0, 0));

        let (lo, hi) = history.bracket(99).unwrap();
        assert_eq!((lo.0, hi.0), (10, 10));

        assert!(StateHistory::<TestState>::new(4).bracket(1).is_none());
    }

    #[test]
    fn truncate_after_discards_newer_ticks() {
        let mut history = StateHistory::new(16);
        for tick in 0..10u32 {
            history.insert(tick, state(tick as f32));
        }
        history.truncate_after(6);
        assert_eq!(history.latest_tick(), Some(6));
        assert_eq!(history.len(), 7);
        assert!(history.get(7).is_none());

        // 重演后再写回。
        history.insert(7, state(70.0));
        assert_eq!(history.get(7).map(|s| s.x), Some(70.0));
    }

    #[test]
    fn predicted_endpoints_use_latest_two() {
        let mut history = StateHistory::new(8);
        history.insert(1, state(1.0));
        history.insert(2, state(2.0));
        let clock = RenderClock {
            tick: 2,
            ..Default::default()
        };
        let (prev, curr) = sample_endpoints(&history, RenderPolicy::Predicted, &clock).unwrap();
        assert_eq!(prev.sample(), 1.0);
        assert_eq!(curr.sample(), 2.0);
    }

    #[test]
    fn single_entry_predicted_repeats_endpoint() {
        let mut history = StateHistory::new(8);
        history.insert(1, state(1.0));
        let clock = RenderClock::default();
        let (prev, curr) = sample_endpoints(&history, RenderPolicy::Predicted, &clock).unwrap();
        assert_eq!(prev.sample(), curr.sample());
    }

    #[test]
    fn interpolated_endpoints_bracket_delayed_tick() {
        let mut history = StateHistory::new(8);
        for tick in 0..6u32 {
            history.insert(tick, state(tick as f32));
        }
        let clock = RenderClock {
            tick: 5,
            ..Default::default()
        };

        let (prev, curr) = sample_endpoints(
            &history,
            RenderPolicy::Interpolated { delay_ticks: 2 },
            &clock,
        )
        .unwrap();
        assert_eq!((prev.sample(), curr.sample()), (3.0, 3.0));

        let (prev, curr) = sample_endpoints(
            &history,
            RenderPolicy::Interpolated { delay_ticks: 1 },
            &clock,
        )
        .unwrap();
        assert_eq!((prev.sample(), curr.sample()), (4.0, 4.0));
    }

    #[test]
    fn sampler_writes_presented_transform_from_history() {
        use crate::presentation::interp::{PresentedTransform, RenderTransform};

        #[derive(Clone, Copy, Debug, PartialEq)]
        struct RtState(RenderTransform);

        impl RollbackState for RtState {
            type Input = ();
            type Sample = RenderTransform;

            fn step(&self, _input: &(), _dt: f32) -> Self {
                *self
            }

            fn sample(&self) -> RenderTransform {
                self.0
            }

            fn lerp(a: &RenderTransform, b: &RenderTransform, alpha: f32) -> RenderTransform {
                RenderTransform::lerp(a, b, alpha)
            }
        }

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(RenderClock {
            tick: 2,
            ..Default::default()
        });
        app.add_systems(FixedPostUpdate, sample_render_states::<RtState>);

        let mut history = StateHistory::new(8);
        history.insert(
            1,
            RtState(RenderTransform::from_translation(Vec3::new(1.0, 0.0, 0.0))),
        );
        history.insert(
            2,
            RtState(RenderTransform::from_translation(Vec3::new(2.0, 0.0, 0.0))),
        );

        let entity = app
            .world_mut()
            .spawn((
                RenderHistory {
                    history,
                    policy: RenderPolicy::Predicted,
                },
                PresentedTransform::default(),
            ))
            .id();

        app.finish();
        app.cleanup();
        app.world_mut().run_schedule(FixedPostUpdate);

        let presented = app
            .world()
            .entity(entity)
            .get::<PresentedTransform>()
            .unwrap();
        assert_eq!(presented.prev.position[0], 1.0);
        assert_eq!(presented.curr.position[0], 2.0);
    }
}
