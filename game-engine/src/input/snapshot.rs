//! 每个逻辑 tick 的确定性输入（`PlayerInput`）与输入历史（`InputBuffer`）。
//!
//! `PlayerInput` 是**唯一**会上报服务端、参与回滚重放的输入类型；
//! 它必须携带「持续状态」（held）与「瞬时边沿」（pressed / released）两部分，
//! 因为一次逻辑 tick 内可能发生「按下又松开」，只留状态会把它吞掉。

use std::collections::VecDeque;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::math::FixedPoint;

/// 经过能力过滤后的、确定性的、单 tick 输入。
///
/// `Default` 即 [`PlayerInput::NONE`]（全零）。
#[derive(Component, Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct PlayerInput {
    pub move_x: FixedPoint,
    pub move_y: FixedPoint,
    /// 本 tick 的视角增量（**不是**绝对朝向；绝对朝向是状态，不是输入）。
    pub look_yaw: FixedPoint,
    pub look_pitch: FixedPoint,
    /// 本 tick 结束时按住的通道位。
    pub held: u64,
    /// 本 tick 内发生的按下边沿（瞬时意图，如「跳跃」）。
    pub pressed: u64,
    /// 本 tick 内发生的松开边沿（如「松开蓄力」）。
    pub released: u64,
}

impl PlayerInput {
    pub const NONE: Self = Self {
        move_x: FixedPoint::from_bits(0),
        move_y: FixedPoint::from_bits(0),
        look_yaw: FixedPoint::from_bits(0),
        look_pitch: FixedPoint::from_bits(0),
        held: 0,
        pressed: 0,
        released: 0,
    };

    /// 用能力掩码过滤：没有对应通道的位直接归零。
    ///
    /// 上层玩法系统因此不需要再判断「这个实体能不能跳」——
    /// 不能跳的实体拿到的 `pressed` 里根本没有 jump 位。
    #[inline]
    pub fn filtered(mut self, allowed: u64) -> Self {
        self.held &= allowed;
        self.pressed &= allowed;
        self.released &= allowed;
        self
    }

    #[inline]
    pub fn is_idle(&self) -> bool {
        self.held == 0
            && self.pressed == 0
            && self.released == 0
            && self.move_x == FixedPoint::from_bits(0)
            && self.move_y == FixedPoint::from_bits(0)
    }
}

/// 折叠后的原始输入（还没经过能力过滤），挂在被控实体上。
///
/// 由 `resolve_controlled_entity` 写入；`write_resolved_input` 读取并与
/// `InputAvailability` 求交后写入 [`ResolvedInput`]。
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct RawResolved(pub PlayerInput);

/// 当前 tick 的过滤后输入（玩法系统只 `Query` 这个）。
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct ResolvedInput(pub PlayerInput);

/// 输入历史环形缓冲。
///
/// 生命周期需覆盖「最近一次已知权威 tick」到「当前 tick」；太短会导致
/// 想回滚却没有对应输入可重放。
///
/// 挂**被控实体**（而不是全局 `Resource`），这样「附身/驾驶/多玩家」天然分离。
#[derive(Component, Debug)]
pub struct InputBuffer {
    entries: VecDeque<(u32, PlayerInput)>,
    max_len: usize,
}

impl InputBuffer {
    pub const DEFAULT_MAX_LEN: usize = 1024;

    pub fn new(max_len: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            max_len: max_len.max(1),
        }
    }

    pub fn push(&mut self, tick: u32, input: PlayerInput) {
        // 同一 tick 重复写入时覆盖（例如回滚重演后重新记录）。
        if let Some((last_tick, last)) = self.entries.back_mut() {
            if *last_tick == tick {
                *last = input;
                return;
            }
        }
        self.entries.push_back((tick, input));
        while self.entries.len() > self.max_len {
            self.entries.pop_front();
        }
    }

    /// 从某个 tick 起（含）的输入序列，用于回滚重演。
    pub fn inputs_from(&self, tick: u32) -> impl Iterator<Item = (u32, PlayerInput)> + '_ {
        self.entries
            .iter()
            .copied()
            .filter(move |(t, _)| *t >= tick)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn last(&self) -> Option<(u32, PlayerInput)> {
        self.entries.back().copied()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

impl Default for InputBuffer {
    fn default() -> Self {
        Self::new(Self::DEFAULT_MAX_LEN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_zeroes_unavailable_channels() {
        let input = PlayerInput {
            pressed: 0b1111,
            held: 0b1111,
            released: 0b1111,
            ..PlayerInput::NONE
        };
        let filtered = input.filtered(0b0101);
        assert_eq!(filtered.pressed, 0b0101);
        assert_eq!(filtered.held, 0b0101);
        assert_eq!(filtered.released, 0b0101);
    }

    #[test]
    fn buffer_keeps_ring_window() {
        let mut buffer = InputBuffer::new(4);
        for tick in 0..10u32 {
            buffer.push(tick, PlayerInput::NONE);
        }
        let ticks: Vec<u32> = buffer.inputs_from(0).map(|(t, _)| t).collect();
        assert_eq!(ticks, vec![6, 7, 8, 9]);
    }

    #[test]
    fn buffer_overwrites_same_tick() {
        let mut buffer = InputBuffer::new(8);
        let pressed = PlayerInput {
            pressed: 1,
            ..PlayerInput::NONE
        };
        buffer.push(3, PlayerInput::NONE);
        buffer.push(3, pressed);
        assert_eq!(buffer.len(), 1);
        assert_eq!(buffer.last().unwrap().1.pressed, 1);
    }
}
