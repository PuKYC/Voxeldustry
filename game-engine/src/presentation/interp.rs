//! 渲染插值。
//!
//! 固定 tick（逻辑）与渲染帧率无关，直接把最新逻辑状态赋给节点会抖。
//! 这里实现标准的「渲染落后一个 tick + 按 alpha 插值」：
//!
//!   T_{t-1}            T_t                T_{t+1}
//!     │                 │                   │
//!     ├── prev=state(t-1) ── curr=state(t) ──┤
//!               ▲
//!          render_time = now
//!     alpha = (now - T_t) / dt  ∈ [0, 1]
//!
//! ## 本版修正（真机会出 bug 的旧实现）
//!
//! 旧实现有两个结构性问题：
//!
//! 1. **prev/curr 在 Update 每个 runner 帧都推一次**：没有 fixed tick 的帧
//!    prev = curr 会冻住，一帧跑两个 tick 的帧会跳过一格。现在改由
//!    FixedPostUpdate 的 advance_render_transforms **每个逻辑 tick 推一次**。
//! 2. **alpha 用 Godot 上报的整数毫秒算**：Godot 帧率与 runner 一致时，T_t
//!    和 now 都被量化到 Godot 帧边沿，alpha 基本只有 0 或 1，插值等于没做；
//!    而且 alpha 在 Bevy 线程算完要等下一次 _process 才用，又多滞后一帧。
//!    现在 Bevy 只下发 (prev, curr) + 本 tick 的**进程级单调时钟** T_t，
//!    由 Godot 在真正的渲染时刻调用纯函数 sample_alpha 采样。
//!
//! sample_alpha 与插值的决策全部留在 game-core（铁律 B2），Godot 只是按返回
//! 的 alpha 机械 lerp。

use std::sync::OnceLock;
use std::time::Instant;

use bevy::prelude::*;

use serde::{Deserialize, Serialize};

/// 渲染用变换（f32）。
///
/// 逻辑位置已是 f32，Godot 直接赋值即可。
#[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct RenderTransform {
    pub position: [f32; 3],
    pub yaw: f32,
    /// 权威位置与渲染位置的偏差（Reconciliation 后）。
    pub correction: [f32; 3],
}

impl RenderTransform {
    pub fn from_translation(translation: Vec3) -> Self {
        Self {
            position: translation.to_array(),
            yaw: 0.0,
            correction: [0.0, 0.0, 0.0],
        }
    }

    pub fn with_yaw(mut self, yaw: f32) -> Self {
        self.yaw = yaw;
        self
    }

    /// 在两个渲染变换之间插值（alpha 由 game-core 按渲染时钟算出）。
    pub fn lerp(prev: &Self, curr: &Self, alpha: f32) -> Self {
        let a = alpha.clamp(0.0, 1.0);
        let lerp = |from: f32, to: f32| from + (to - from) * a;
        Self {
            position: [
                lerp(prev.position[0], curr.position[0]),
                lerp(prev.position[1], curr.position[1]),
                lerp(prev.position[2], curr.position[2]),
            ],
            yaw: curr.yaw,
            correction: curr.correction,
        }
    }
}

/// 表现侧插值采样：前后两个 tick 的渲染变换。
#[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct RenderTransformSample {
    pub prev: RenderTransform,
    pub curr: RenderTransform,
}

/// 插值 alpha 的来源（诊断用）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AlphaSource {
    /// 用客户端渲染时钟算出来的。
    ClientClock,
    /// 退回 Time<Fixed>::overstep_fraction()。
    #[default]
    OverstepFallback,
}

/// 纯函数：算渲染插值 alpha，返回 (alpha, 来源)。
///
/// 时钟缺失、时钟回退、或跨度明显异常（暂停 / 长卡顿）时退回 overstep_fraction，
/// **绝不外插**（外插会让静止物体在卡顿后「弹一下」）。
pub fn compute_alpha(
    client_now_ms: Option<u64>,
    tick_clock_ms: Option<u64>,
    timestep_ms: f32,
    overstep_fraction: f32,
) -> (f32, AlphaSource) {
    let fallback = (
        if overstep_fraction.is_finite() {
            overstep_fraction.clamp(0.0, 1.0)
        } else {
            0.0
        },
        AlphaSource::OverstepFallback,
    );

    let (Some(now), Some(tick)) = (client_now_ms, tick_clock_ms) else {
        return fallback;
    };
    if timestep_ms <= 0.0 || !timestep_ms.is_finite() {
        return fallback;
    }

    let Some(elapsed_ms) = now.checked_sub(tick) else {
        // 时钟回退（暂停恢复 / 计时器重置）。
        return fallback;
    };

    let raw = elapsed_ms as f32 / timestep_ms;
    if !raw.is_finite() || raw > 2.0 {
        // 跨度超过两个 tick：说明期间没有在正常推进，别外插。
        return fallback;
    }

    (raw.clamp(0.0, 1.0), AlphaSource::ClientClock)
}

/// 进程级单调时钟（毫秒，自本进程首次调用起）。
///
/// Bevy 后台线程与 Godot 主线程在**同一进程**里，所以这是一把两边都能读的
/// 共享时钟 —— 不需要 Godot 上报毫秒，也就没有「上报值量化到 Godot 帧边沿」
/// 的问题。
pub fn monotonic_now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// 纯函数：Godot 在真正的渲染时刻采样插值 alpha。
///
/// - now_ms 与 tick_clock_ms 必须来自同一把时钟（monotonic_now_ms）；
/// - 缺失 / 非法 timestep 直接返回 1.0（贴到 curr，不抖）；
/// - 时钟回退或跨度 > 2 tick 一律夹到 [0, 1]，**绝不外插**。
pub fn sample_alpha(now_ms: u64, tick_clock_ms: u64, timestep_ms: f32) -> f32 {
    if timestep_ms <= 0.0 || !timestep_ms.is_finite() {
        return 1.0;
    }
    let Some(elapsed_ms) = now_ms.checked_sub(tick_clock_ms) else {
        return 1.0;
    };
    let raw = elapsed_ms as f32 / timestep_ms;
    if !raw.is_finite() {
        return 1.0;
    }
    raw.clamp(0.0, 1.0)
}

/// 表现层专用、**已插值**的变换 —— 这才是注册进表现管线的组件。
///
/// 只带 prev / curr 两个端点，真正的插值采样交给 Godot 在渲染时刻做。
/// 与逻辑侧 Bevy `Transform` 物理分离：回滚重放不会污染插值状态。
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct PresentedTransform {
    /// 上一个 tick 的渲染变换。
    pub prev: RenderTransform,
    /// 当前 tick 的渲染变换。
    pub curr: RenderTransform,
    /// 是否已经历过首个 tick（避免首帧从原点滑入）。
    initialized: bool,
}

impl PresentedTransform {
    /// 引擎采样器写入两端点（`rollback::sample_render_states`）。
    ///
    /// 值不变时不写，避免每 tick 触发 `Changed`；首次写入直接采用真实历史
    /// 端点（不是默认原点），因此不会从原点滑入。`initialized` 仅用于兼容
    /// 旧的 `advance_render_transforms` 首帧语义。
    pub fn set_sampled(&mut self, prev: RenderTransform, curr: RenderTransform) {
        if self.prev != prev || self.curr != curr {
            self.prev = prev;
            self.curr = curr;
        }
        self.initialized = true;
    }
}

/// 渲染时钟：最新一个 fixed tick 发生时的进程单调时刻，即 T_t。
#[derive(Resource, Debug, Default)]
pub struct RenderClockState {
    pub tick_clock_ms: Option<u64>,
}

/// 每个 fixed step 记录一次 T_t（Godot 采样 alpha 用）。
pub fn record_tick_clock(mut clock: ResMut<RenderClockState>) {
    clock.tick_clock_ms = Some(monotonic_now_ms());
}

/// FixedPostUpdate：每个逻辑 tick 推进一次 prev / curr。
///
/// **必须每 tick 推一次**（而不是每个渲染帧），否则：
/// - 没有 tick 的渲染帧会把 prev 覆盖成 curr -> 冻住；
/// - 一帧跑两个 tick 会跳过一格。
///
/// 静止实体值不变时跳过写入，因此不会每帧触发 Changed<PresentedTransform>。
pub fn advance_render_transforms(mut query: Query<(&Transform, &mut PresentedTransform)>) {
    for (transform, mut presented) in &mut query {
        let curr = RenderTransform::from_translation(transform.translation);

        if !presented.initialized {
            // 首帧：prev 与 curr 都设成当前值，避免从原点滑入。
            presented.prev = curr;
            presented.curr = curr;
            presented.initialized = true;
        } else if presented.curr != curr {
            presented.prev = presented.curr;
            presented.curr = curr;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn translation(x: i32) -> Transform {
        Transform::from_xyz(x as f32, 0.0, 0.0)
    }

    #[test]
    fn without_client_clock_it_falls_back_to_overstep() {
        let (alpha, source) = compute_alpha(None, None, 16.6, 0.25);
        assert_eq!(source, AlphaSource::OverstepFallback);
        assert!((alpha - 0.25).abs() < f32::EPSILON);
    }

    #[test]
    fn with_client_clock_it_uses_elapsed_over_timestep() {
        let (alpha, source) = compute_alpha(Some(1016), Some(1000), 16.0, 0.0);
        assert_eq!(source, AlphaSource::ClientClock);
        assert!((alpha - 1.0).abs() < f32::EPSILON);

        let (half, _) = compute_alpha(Some(1008), Some(1000), 16.0, 0.0);
        assert!((half - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn never_extrapolates_on_large_gaps() {
        let (alpha, source) = compute_alpha(Some(9000), Some(1000), 16.0, 0.75);
        assert_eq!(source, AlphaSource::OverstepFallback);
        assert!((alpha - 0.75).abs() < f32::EPSILON);
    }

    #[test]
    fn clock_going_backwards_falls_back() {
        let (alpha, source) = compute_alpha(Some(500), Some(1000), 16.0, 0.5);
        assert_eq!(source, AlphaSource::OverstepFallback);
        assert!((alpha - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn alpha_is_always_clamped_into_unit_range() {
        for overstep in [-3.0f32, 0.0, 0.5, 1.0, 9.0, f32::NAN] {
            let (alpha, _) = compute_alpha(None, None, 16.0, overstep);
            assert!(
                (0.0..=1.0).contains(&alpha),
                "alpha 越界: {alpha} (overstep = {overstep})"
            );
        }
        let (alpha, source) = compute_alpha(Some(1000), Some(1000), 0.0, 0.3);
        assert_eq!(source, AlphaSource::OverstepFallback);
        assert!((alpha - 0.3).abs() < f32::EPSILON);
    }

    #[test]
    fn stationary_entity_produces_a_stable_render_transform() {
        let still = RenderTransform {
            position: [3.0, 0.0, -2.0],
            yaw: 0.0,
            correction: [0.0; 3],
        };
        for alpha in [0.0f32, 0.33, 0.5, 0.99, 1.0] {
            assert_eq!(RenderTransform::lerp(&still, &still, alpha), still);
        }
    }

    #[test]
    fn moving_entity_interpolates_between_the_two_ticks() {
        let prev = RenderTransform {
            position: [0.0, 0.0, 0.0],
            ..Default::default()
        };
        let curr = RenderTransform {
            position: [10.0, 0.0, 0.0],
            ..Default::default()
        };
        assert_eq!(RenderTransform::lerp(&prev, &curr, 0.0).position[0], 0.0);
        assert_eq!(RenderTransform::lerp(&prev, &curr, 0.5).position[0], 5.0);
        assert_eq!(RenderTransform::lerp(&prev, &curr, 1.0).position[0], 10.0);
    }

    // ───────────────────── 新增：Godot 侧采样 ─────────────────────

    #[test]
    fn sample_alpha_is_continuous_across_the_tick() {
        assert!((sample_alpha(1000, 1000, 16.0) - 0.0).abs() < f32::EPSILON);
        assert!((sample_alpha(1008, 1000, 16.0) - 0.5).abs() < f32::EPSILON);
        assert!((sample_alpha(1016, 1000, 16.0) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn sample_alpha_never_extrapolates_or_goes_backwards() {
        assert_eq!(sample_alpha(9999, 1000, 16.0), 1.0);
        assert_eq!(sample_alpha(500, 1000, 16.0), 1.0);
        assert_eq!(sample_alpha(1000, 1000, 0.0), 1.0);
        assert_eq!(sample_alpha(1000, 1000, f32::NAN), 1.0);
    }

    #[test]
    fn monotonic_clock_does_not_go_backwards() {
        let a = monotonic_now_ms();
        let b = monotonic_now_ms();
        assert!(b >= a, "单调时钟不得回退");
    }

    // ───────────────────── 新增：按 tick 推进 prev/curr ─────────────────────

    #[test]
    fn first_tick_does_not_slide_from_origin() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_systems(Update, advance_render_transforms);
        let entity = app
            .world_mut()
            .spawn((translation(5), PresentedTransform::default()))
            .id();

        app.update();

        let presented = app
            .world()
            .entity(entity)
            .get::<PresentedTransform>()
            .unwrap();
        assert_eq!(presented.prev.position[0], 5.0);
        assert_eq!(presented.curr.position[0], 5.0);
    }

    #[test]
    fn prev_curr_advance_once_per_tick_not_per_render_frame() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_systems(Update, advance_render_transforms);
        let entity = app
            .world_mut()
            .spawn((translation(0), PresentedTransform::default()))
            .id();

        app.update();
        // 第二个渲染帧、逻辑位置没变（没有 tick）：prev 必须保持 0，不能冻成 curr。
        app.update();
        {
            let presented = app
                .world()
                .entity(entity)
                .get::<PresentedTransform>()
                .unwrap();
            assert_eq!(presented.prev.position[0], 0.0);
            assert_eq!(presented.curr.position[0], 0.0);
        }

        // 一个 tick 移动到 10：prev 变成上一个 tick 的 0，curr 是 10。
        app.world_mut().entity_mut(entity).insert(translation(10));
        app.update();
        let presented = app
            .world()
            .entity(entity)
            .get::<PresentedTransform>()
            .unwrap();
        assert_eq!(presented.prev.position[0], 0.0);
        assert_eq!(presented.curr.position[0], 10.0);
    }
}
