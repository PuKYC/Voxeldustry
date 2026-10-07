//! 跨线程输入暂存区（Godot 线程写 → Bevy 线程读）。
//!
//! **为什么用 `Mutex` 而不是 channel**：Godot 渲染帧率与逻辑 tick 频率无关
//! （可能 144Hz 对 60Hz，也可能卡顿后 30Hz 对 60Hz）。用 channel 排队会让
//! Bevy 越跑越落后且积压越来越多；用共享暂存区则天然**合并**为「最新状态 +
//! 累计边沿」。
//!
//! 折叠规则（见 `TickInput` / `take_for_tick`）：
//!
//! | 类别 | 规则 | 理由 |
//! |---|---|---|
//! | `held` / `move` | last-wins | 状态，旧值无意义 |
//! | `look` | 累加后清零 | 增量，两帧转动都要算 |
//! | `pressed` / `released` | OR 锁存 | 一 tick 内「按下又松开」必须都保留 |

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use bevy::prelude::Resource;

use crate::math::FixedPoint;

use super::control::InputSourceId;
use super::raw::{axis_to_fixed, AxisI16, RawInputFrame};
use super::snapshot::PlayerInput;

/// 视角增量的定点缩放：把累计的 i16 步数换算成弧度量级。
const LOOK_SCALE_DENOM: i32 = 1000;

/// 单个逻辑 tick 取走的输入（原始层，未过滤）。
#[derive(Clone, Copy, Debug, Default)]
pub struct TickInput {
    pub seq: u64,
    pub render_clock_ms: u64,
    pub held: u64,
    pub pressed: u64,
    pub released: u64,
    pub move_axis: (AxisI16, AxisI16),
    pub look_delta: (i32, i32),
    /// 本次取走之前，是否真的收到过新的采集帧。
    pub fresh: bool,
}

impl TickInput {
    pub const IDLE: Self = Self {
        seq: 0,
        render_clock_ms: 0,
        held: 0,
        pressed: 0,
        released: 0,
        move_axis: (AxisI16(0), AxisI16(0)),
        look_delta: (0, 0),
        fresh: false,
    };

    /// 折叠成确定性的 [`PlayerInput`]。
    pub fn to_player_input(self) -> PlayerInput {
        let scale = FixedPoint::from_num(LOOK_SCALE_DENOM);
        PlayerInput {
            move_x: axis_to_fixed(self.move_axis.0),
            move_y: axis_to_fixed(self.move_axis.1),
            look_yaw: FixedPoint::from_num(self.look_delta.0) / scale,
            look_pitch: FixedPoint::from_num(self.look_delta.1) / scale,
            held: self.held,
            pressed: self.pressed,
            released: self.released,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct SourceStaging {
    seq: u64,
    render_clock_ms: u64,
    held: u64,
    pressed_latch: u64,
    released_latch: u64,
    move_axis: (AxisI16, AxisI16),
    look_accum: (i32, i32),
    has_new_sample: bool,
}

/// 被各通道共享的内部状态。
#[derive(Debug, Default)]
struct StagingInner {
    sources: HashMap<InputSourceId, SourceStaging>,
    suspended: bool,
    /// 最近一次上报的渲染时钟（即使该源已被取走也保留）。
    render_clock_ms: u64,
}

/// 输入暂存区句柄。
///
/// `Arc` 内部共享 + `Clone`，因此既能在 `game-core` 里当 `Resource`，
/// 又能在 `godot-client-ext` 里被 Godot 主线程持有。
#[derive(Resource, Clone, Default)]
pub struct InputStaging {
    inner: Arc<Mutex<StagingInner>>,
}

impl InputStaging {
    pub fn new() -> Self {
        Self::default()
    }

    /// 锁住内部状态；对 poisoning 采取「继续用」而不是 panic
    /// （输入丢失不应该让整个游戏崩掉）。
    fn lock(&self) -> MutexGuard<'_, StagingInner> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Godot 侧调用（每次采集 flush 一次）。
    ///
    /// - 边沿位做 **OR 锁存**（不丢快速点按）；
    /// - 轴值 **last-wins**；
    /// - 视角增量 **累加**；
    /// - 暂停期间丢弃边沿与增量，只保留按住状态（见 `set_suspended`）。
    pub fn submit(&self, source: InputSourceId, frame: RawInputFrame) {
        let mut inner = self.lock();
        let suspended = inner.suspended;
        inner.render_clock_ms = frame.render_clock_ms;

        let entry = inner.sources.entry(source).or_default();
        entry.seq = frame.seq;
        entry.render_clock_ms = frame.render_clock_ms;
        entry.held = frame.held;
        entry.move_axis = frame.move_axis;
        entry.has_new_sample = true;

        if suspended {
            // 暂停期间不累计任何会被「一次性释放」的东西。
            entry.pressed_latch = 0;
            entry.released_latch = 0;
            entry.look_accum = (0, 0);
        } else {
            entry.pressed_latch |= frame.pressed_latch;
            entry.released_latch |= frame.released_latch;
            let (look_x, look_y) = frame.look_delta;
            entry.look_accum.0 = entry.look_accum.0.saturating_add(look_x);
            entry.look_accum.1 = entry.look_accum.1.saturating_add(look_y);
        }
    }

    /// Bevy 侧调用（每逻辑 tick 一次）。
    ///
    /// 取走并清零边沿 / 视角增量，**保留**按住状态与轴值（它们是状态，应该粘住）。
    /// 源不存在时返回 `None`。
    pub fn take_for_tick(&self, source: InputSourceId) -> Option<TickInput> {
        let mut inner = self.lock();
        let entry = inner.sources.get_mut(&source)?;

        let tick = TickInput {
            seq: entry.seq,
            render_clock_ms: entry.render_clock_ms,
            held: entry.held,
            pressed: entry.pressed_latch,
            released: entry.released_latch,
            move_axis: entry.move_axis,
            look_delta: entry.look_accum,
            fresh: entry.has_new_sample,
        };

        entry.pressed_latch = 0;
        entry.released_latch = 0;
        entry.look_accum = (0, 0);
        entry.has_new_sample = false;

        Some(tick)
    }

    /// 暂停 / 恢复。
    ///
    /// 恢复时**清空边沿锁存**：否则暂停期间按的键会在恢复帧集中触发
    /// （「一堆跳跃同时发生」）。按住状态与轴值保留为最新值。
    pub fn set_suspended(&self, suspended: bool) {
        let mut inner = self.lock();
        inner.suspended = suspended;
        if !suspended {
            for entry in inner.sources.values_mut() {
                entry.pressed_latch = 0;
                entry.released_latch = 0;
                entry.look_accum = (0, 0);
            }
        }
    }

    pub fn is_suspended(&self) -> bool {
        self.lock().suspended
    }

    /// 最近一次上报的渲染时钟（毫秒）。表现层插值用。
    pub fn render_clock_ms(&self) -> u64 {
        self.lock().render_clock_ms
    }

    /// 已注册的输入源数量（诊断用）。
    pub fn source_count(&self) -> usize {
        self.lock().sources.len()
    }

    /// 丢弃所有暂存状态（重启会话时用）。
    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.sources.clear();
        inner.render_clock_ms = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::super::control::InputSourceId;
    use super::super::raw::{quantize_axis, AxisI16, RawInputFrame};
    use super::InputStaging;

    fn frame(held: u64, pressed: u64, released: u64, move_x: f32) -> RawInputFrame {
        RawInputFrame {
            seq: 1,
            render_clock_ms: 100,
            held,
            pressed_latch: pressed,
            released_latch: released,
            move_axis: (quantize_axis(move_x, 0.0), AxisI16(0)),
            look_delta: (100, 0),
        }
    }

    #[test]
    fn edges_are_latched_not_lost() {
        let staging = InputStaging::new();
        let src = InputSourceId(1);

        // 同一个 tick 内：按下又松开。
        staging.submit(src, frame(0, 1, 0, 0.0));
        staging.submit(src, frame(0, 0, 1, 0.0));

        let tick = staging.take_for_tick(src).unwrap();
        assert_eq!(tick.pressed, 1, "按下边沿必须保留");
        assert_eq!(tick.released, 1, "松开边沿必须保留");
        assert_eq!(tick.held, 0);
    }

    #[test]
    fn held_and_move_are_last_wins_but_look_accumulates() {
        let staging = InputStaging::new();
        let src = InputSourceId(1);

        staging.submit(src, frame(0b1, 0, 0, 1.0));
        staging.submit(src, frame(0b10, 0, 0, -1.0));

        let tick = staging.take_for_tick(src).unwrap();
        assert_eq!(tick.held, 0b10, "held 应取最新值");
        assert_eq!(
            tick.move_axis.0,
            AxisI16(-i16::MAX),
            "move 应取最新值（第二次是 -1.0）"
        );
        assert_eq!(tick.look_delta.0, 200, "视角增量应累加");
    }

    #[test]
    fn take_clears_edges_but_keeps_held() {
        let staging = InputStaging::new();
        let src = InputSourceId(1);

        staging.submit(src, frame(0b1, 0b1, 0, 0.0));
        let first = staging.take_for_tick(src).unwrap();
        assert_eq!(first.pressed, 0b1);

        // 没有再 submit，第二个 tick 仍应看到「按住」状态，但没有边沿。
        let second = staging.take_for_tick(src).unwrap();
        assert_eq!(second.pressed, 0, "边沿已被消费");
        assert_eq!(second.held, 0b1, "按住状态是粘性的");
        assert!(!second.fresh);
    }

    #[test]
    fn unknown_source_returns_none() {
        let staging = InputStaging::new();
        assert!(staging.take_for_tick(InputSourceId(9)).is_none());
    }

    #[test]
    fn suspend_discards_edges_but_keeps_held() {
        let staging = InputStaging::new();
        let src = InputSourceId(1);

        staging.set_suspended(true);
        staging.submit(src, frame(0b1, 0b1000, 0b1000, 0.5));

        let tick = staging.take_for_tick(src).unwrap();
        assert_eq!(tick.pressed, 0, "暂停期间的按下边沿必须丢弃");
        assert_eq!(tick.released, 0, "暂停期间的松开边沿必须丢弃");
        assert_eq!(tick.look_delta, (0, 0), "暂停期间的视角增量必须丢弃");
        assert_eq!(tick.held, 0b1, "按住状态保留");
        assert_eq!(tick.move_axis.0, quantize_axis(0.5, 0.0));
    }

    #[test]
    fn resume_clears_pending_edges() {
        let staging = InputStaging::new();
        let src = InputSourceId(1);

        staging.submit(src, frame(0b1, 0b1, 0, 0.0));
        staging.set_suspended(true);
        staging.set_suspended(false);

        let tick = staging.take_for_tick(src).unwrap();
        assert_eq!(tick.pressed, 0, "恢复时清空残留边沿，避免集中触发");
    }
}
