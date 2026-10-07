//! 原始输入帧与量化。
//!
//! Godot 只传**原始**信息（动作名 + f32 轴值）；「量化成定点、死区、折叠成
//! 单 tick」全部在 `game-core` 完成，否则同一份原始浮点在不同平台上可能落到
//! 不同定点值，破坏确定性。

use serde::{Deserialize, Serialize};

use crate::math::FixedPoint;

use super::actions::ActionMap;

/// 量化后的轴值：`i16` 表示 `[-1, 1]`。
///
/// 用整数而不是 `f32` 作为跨边界表示，保证位级可复现。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct AxisI16(pub i16);

/// 默认死区（|raw| <= deadzone 视为 0）。
pub const DEFAULT_DEADZONE: f32 = 0.15;

/// Godot 传来的 f32 轴值 → 确定性 `i16`。
///
/// 非线性曲线 / 更精细的死区策略以后可以在这里扩展；但**必须留在这个函数里**，
/// 不能挪到 Godot 侧。
pub fn quantize_axis(raw: f32, deadzone: f32) -> AxisI16 {
    if !raw.is_finite() {
        return AxisI16(0);
    }
    let clamped = raw.clamp(-1.0, 1.0);
    let deadzone = deadzone.clamp(0.0, 0.99);

    let magnitude = clamped.abs();
    if magnitude <= deadzone {
        return AxisI16(0);
    }

    // 把 [deadzone, 1] 线性拉伸回 [0, 1]，避免死区边缘出现跳变。
    let normalized = (magnitude - deadzone) / (1.0 - deadzone);
    let scaled = (normalized * f32::from(i16::MAX)).round() as i32;
    let scaled = scaled.clamp(0, i32::from(i16::MAX));

    // 正负都用 i16::MAX 上限，保持对称（避免 -32768 造成的不对称）。
    AxisI16(if clamped < 0.0 {
        -(scaled as i16)
    } else {
        scaled as i16
    })
}

/// 单帧鼠标位移上限（防止异常值把 i32 累加撑爆）。
pub const MAX_LOOK_DELTA: i32 = 1_000_000;

/// Godot 传来的鼠标像素位移 → 确定性 `i32` 步数。
///
/// **不能复用 [`quantize_axis`]**：那是给 [-1, 1] 的摇杆轴用的，会把鼠标位移
/// 夹到 ±1，导致视角输入饱和、灵敏度失控。这里保留原始计数（非有限值归零，
/// 超限饱和），累加与缩放（`staging::LOOK_SCALE_DENOM`）在输入折叠时完成。
pub fn quantize_look(raw: f32) -> i32 {
    if !raw.is_finite() {
        return 0;
    }
    raw.round()
        .clamp(-(MAX_LOOK_DELTA as f32), MAX_LOOK_DELTA as f32) as i32
}

/// `i16` → 定点数（落在 `[-1, 1]`）。
pub fn axis_to_fixed(axis: AxisI16) -> FixedPoint {
    FixedPoint::from_num(axis.0) / FixedPoint::from_num(i16::MAX)
}

/// 一个采集周期的原始输入（Godot 侧产出，`game-core` 消费）。
///
/// 注意：这是「原始」层，**还没经过能力过滤**。
#[derive(Clone, Copy, Debug, Default)]
pub struct RawInputFrame {
    /// 采集序号，单调递增（Godot 每次 flush +1），用于诊断与丢帧统计。
    pub seq: u64,
    /// Godot 渲染时钟（毫秒，自启动）。表现层插值用。
    pub render_clock_ms: u64,
    /// 当前按住的通道位。
    pub held: u64,
    /// 自上次消费以来累计的「按下」边沿（OR 锁存，不可丢）。
    pub pressed_latch: u64,
    /// 自上次消费以来累计的「松开」边沿（OR 锁存，不可丢）。
    pub released_latch: u64,
    /// 最新轴值（last-wins）。
    ///
    /// 字段名不能叫 `move`（Rust 关键字），所以是 `move_axis`。
    pub move_axis: (AxisI16, AxisI16),
    /// 自上次消费以来累计的视角增量（累加，消费后清零）。
    ///
    /// 鼠标像素位移是**增量**，用 `i32` 保留原始计数；进入逻辑时再除以
    /// `staging::LOOK_SCALE_DENOM` 换算成弧度。
    pub look_delta: (i32, i32),
}

impl RawInputFrame {
    /// 从 Godot 侧的「动作名列表 + 原始 f32 轴值」构造。
    ///
    /// **这里是唯一做名字查表的地方**：`godot-client-ext` 只负责把
    /// `PackedStringArray` 变成 `&str` 并调用本函数，不做任何判断。
    ///
    /// 返回 `(帧, 未知动作名列表)`；未知名字由调用方打 warning 后忽略
    /// （前向兼容：老客户端遇到新动作名不崩）。
    pub fn from_names<'a, A: ActionMap>(
        held: impl IntoIterator<Item = &'a str>,
        pressed: impl IntoIterator<Item = &'a str>,
        released: impl IntoIterator<Item = &'a str>,
        move_raw: (f32, f32),
        look_raw: (f32, f32),
        seq: u64,
        render_clock_ms: u64,
    ) -> (Self, Vec<String>) {
        let mut unknown = Vec::new();

        let collect = |names: Vec<&'a str>, unknown: &mut Vec<String>| -> u64 {
            let mut mask = 0u64;
            for name in names {
                match A::mask_of_name(name) {
                    Some(bit) => mask |= bit,
                    None => unknown.push(name.to_string()),
                }
            }
            mask
        };

        let held_mask = collect(held.into_iter().collect(), &mut unknown);
        let pressed_mask = collect(pressed.into_iter().collect(), &mut unknown);
        let released_mask = collect(released.into_iter().collect(), &mut unknown);

        let frame = Self {
            seq,
            render_clock_ms,
            held: held_mask,
            pressed_latch: pressed_mask,
            released_latch: released_mask,
            move_axis: (
                quantize_axis(move_raw.0, DEFAULT_DEADZONE),
                quantize_axis(move_raw.1, DEFAULT_DEADZONE),
            ),
            look_delta: (quantize_look(look_raw.0), quantize_look(look_raw.1)),
        };

        (frame, unknown)
    }
}

/// 请求的轴（预留给多轴扩展；目前只用 `Move` / `Look`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum AxisKind {
    Move,
    Look,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestActions;

    impl ActionMap for TestActions {
        fn table() -> &'static [super::super::actions::ActionDef] {
            use super::super::actions::{ActionDef, ActionId};
            &[ActionDef {
                name: "jump",
                action: ActionId(5),
                channel_bit: 4,
            }]
        }
    }

    #[test]
    fn deadzone_maps_to_zero() {
        assert_eq!(quantize_axis(0.0, DEFAULT_DEADZONE), AxisI16(0));
        assert_eq!(quantize_axis(0.1, DEFAULT_DEADZONE), AxisI16(0));
        assert_eq!(quantize_axis(-0.1, DEFAULT_DEADZONE), AxisI16(0));
    }

    #[test]
    fn full_deflection_is_symmetric_and_saturating() {
        assert_eq!(quantize_axis(1.0, DEFAULT_DEADZONE), AxisI16(i16::MAX));
        assert_eq!(quantize_axis(-1.0, DEFAULT_DEADZONE), AxisI16(-i16::MAX));
        // 超出范围必须饱和而不是回绕。
        assert_eq!(quantize_axis(9.0, DEFAULT_DEADZONE), AxisI16(i16::MAX));
        assert_eq!(quantize_axis(-9.0, DEFAULT_DEADZONE), AxisI16(-i16::MAX));
    }

    #[test]
    fn non_finite_is_zero() {
        assert_eq!(quantize_axis(f32::NAN, DEFAULT_DEADZONE), AxisI16(0));
        assert_eq!(quantize_axis(f32::INFINITY, DEFAULT_DEADZONE), AxisI16(0));
    }

    #[test]
    fn deterministic_for_same_input() {
        for step in 0..200 {
            let raw = (step as f32) / 199.0 * 2.0 - 1.0;
            assert_eq!(
                quantize_axis(raw, DEFAULT_DEADZONE),
                quantize_axis(raw, DEFAULT_DEADZONE)
            );
        }
    }

    #[test]
    fn axis_to_fixed_bounds() {
        assert_eq!(axis_to_fixed(AxisI16(0)), FixedPoint::from_num(0));
        assert_eq!(axis_to_fixed(AxisI16(i16::MAX)), FixedPoint::from_num(1));
        assert_eq!(axis_to_fixed(AxisI16(-i16::MAX)), FixedPoint::from_num(-1));
    }

    #[test]
    fn from_names_reports_unknown_without_panicking() {
        let (frame, unknown) = RawInputFrame::from_names::<TestActions>(
            ["jump", "totally_unknown"],
            ["jump"],
            [],
            (0.0, 0.0),
            (0.0, 0.0),
            7,
            1234,
        );
        assert_eq!(frame.held, 1 << 4);
        assert_eq!(frame.pressed_latch, 1 << 4);
        assert_eq!(frame.seq, 7);
        assert_eq!(frame.render_clock_ms, 1234);
        assert_eq!(unknown, vec!["totally_unknown".to_string()]);
    }

    #[test]
    fn look_delta_is_not_clamped_to_unit_axis() {
        // 鼠标位移是像素数，不能像摇杆轴那样夹到 ±1。
        assert_eq!(quantize_look(5.0), 5);
        assert_eq!(quantize_look(-12.3), -12);
        assert_eq!(quantize_look(0.0), 0);
        assert_eq!(quantize_look(f32::NAN), 0);
        assert_eq!(quantize_look(f32::INFINITY), 0);
        assert_eq!(quantize_look(1.0e12), MAX_LOOK_DELTA);
        assert_eq!(quantize_look(-1.0e12), -MAX_LOOK_DELTA);
    }

    #[test]
    fn from_names_keeps_multipixel_look_delta() {
        let (frame, unknown) =
            RawInputFrame::from_names::<TestActions>([], [], [], (0.0, 0.0), (123.0, -45.0), 1, 0);
        assert!(unknown.is_empty());
        assert_eq!(frame.look_delta, (123, -45), "像素位移必须原样保留");
    }
}
