//! 跨边界装配点：引擎与 FFI 适配层之间唯一共享的句柄集合。
//!
//! 只包含纯数据通道，不包含 World / App / 任何 Godot 类型。四个字段都是
//! Arc 内部的 Clone 句柄，因此可以廉价克隆给后台线程。
//!
//! 泛型于 GameSpec：载荷 / 事件 / 语义注册表由游戏侧绑定。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::input::control::InputSourceId;
use crate::input::raw::RawInputFrame;
use crate::input::staging::InputStaging;
use crate::presentation::command::{PresentationFrame, PresentationSlot};
use crate::presentation::event::{PresentationEventQueue, TimedPresentationEvent};
use crate::presentation::packed::{FrameStreams, PackedHeader};
use crate::spec::GameSpec;

/// 客户端与核心之间的桥。
pub struct ClientBridge<S: GameSpec> {
    /// 输入暂存区（Godot 写、Bevy 读）。
    pub input: InputStaging,
    /// 表现帧单槽（Bevy 写、Godot 取）。
    pub slot: PresentationSlot<S::Payload>,
    /// 表现事件队列（Bevy 推、Godot drain）。
    pub events: PresentationEventQueue<S::Event>,
    /// 运行时语义注册表（Godot / mod 注册，Bevy 作为 Resource 读取）。
    pub semantics: S::Semantics,
    session: u64,
    submitted_frames: Arc<AtomicU64>,
    unknown_actions: Arc<AtomicU64>,
}

impl<S: GameSpec> Clone for ClientBridge<S> {
    fn clone(&self) -> Self {
        Self {
            input: self.input.clone(),
            slot: self.slot.clone(),
            events: self.events.clone(),
            semantics: self.semantics.clone(),
            session: self.session,
            submitted_frames: Arc::clone(&self.submitted_frames),
            unknown_actions: Arc::clone(&self.unknown_actions),
        }
    }
}

impl<S: GameSpec> ClientBridge<S>
where
    S::Semantics: Default,
{
    /// 新建一个会话的桥。
    pub fn new(session: u64) -> Self {
        Self::with_semantics(session, S::Semantics::default())
    }
}

impl<S: GameSpec> ClientBridge<S> {
    /// 复用一份已存在的语义注册表。
    pub fn with_semantics(session: u64, semantics: S::Semantics) -> Self {
        Self {
            input: InputStaging::new(),
            slot: PresentationSlot::new(),
            events: PresentationEventQueue::new(),
            semantics,
            session,
            submitted_frames: Arc::new(AtomicU64::new(0)),
            unknown_actions: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn session(&self) -> u64 {
        self.session
    }

    /// Godot 侧：提交一帧原始输入（含渲染时钟）。
    pub fn submit_input_frame(
        &self,
        source: InputSourceId,
        frame: RawInputFrame,
        unknown_actions: usize,
    ) {
        if unknown_actions > 0 {
            self.unknown_actions
                .fetch_add(unknown_actions as u64, Ordering::Relaxed);
        }
        self.submitted_frames.fetch_add(1, Ordering::Relaxed);
        self.input.submit(source, frame);
    }

    /// Godot 侧：暂停 / 恢复输入采集。
    pub fn set_input_suspended(&self, suspended: bool) {
        self.input.set_suspended(suspended);
    }

    /// Godot 侧：取走最新表现帧（没有则 None）。
    pub fn take_presentation_frame(&self) -> Option<Arc<PresentationFrame<S::Payload>>> {
        let frame = self.slot.take()?;
        if frame.session != self.session {
            return None;
        }
        Some(frame)
    }

    /// Godot 侧：取走最新表现帧的 GPF1 打包字节（兼容通道，惰性编码）。
    pub fn take_packed_presentation_frame(&self) -> Option<Arc<[u8]>> {
        let entry = self.slot.take_entry()?;
        if entry.frame.session != self.session {
            return None;
        }
        let bytes = crate::presentation::packed::encode_frame::<S::Payload>(&entry.frame);
        Some(Arc::from(bytes.into_boxed_slice()))
    }

    /// Godot 侧：取走最新表现帧的 SoA 流 + 帧头（快通道，零解码）。
    pub fn take_streams(&self) -> Option<(PackedHeader, FrameStreams)> {
        let (header, streams) = self.slot.take_streams()?;
        if header.session != self.session {
            return None;
        }
        Some((header, streams))
    }

    /// Godot 侧：取走全部待处理表现事件（保序，带 tick）。
    pub fn drain_presentation_events(&self) -> Vec<TimedPresentationEvent<S::Event>> {
        self.events.drain()
    }

    /// Bevy 侧：推入一条表现事件。队列满返回 false。
    pub fn push_presentation_event(&self, tick: u32, event: S::Event) -> bool {
        self.events.push(tick, event)
    }

    pub fn dropped_events(&self) -> u64 {
        self.events.dropped()
    }

    pub fn submitted_frames(&self) -> u64 {
        self.submitted_frames.load(Ordering::Relaxed)
    }

    pub fn unknown_actions(&self) -> u64 {
        self.unknown_actions.load(Ordering::Relaxed)
    }

    /// 会话切换时清理残留（重启 Bevy 前调用）。语义注册表保留。
    pub fn reset(&self, session: u64) -> Self {
        Self {
            input: InputStaging::new(),
            slot: PresentationSlot::new(),
            events: PresentationEventQueue::new(),
            semantics: self.semantics.clone(),
            session,
            submitted_frames: Arc::new(AtomicU64::new(0)),
            unknown_actions: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl<S: GameSpec> Default for ClientBridge<S>
where
    S::Semantics: Default,
{
    fn default() -> Self {
        Self::new(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::StableEntityId;
    use crate::input::actions::EmptyActions;
    use crate::presentation::command::PresentationCommand;
    use crate::spec::DefaultSpec;

    #[test]
    fn stale_session_frames_are_dropped() {
        let bridge = ClientBridge::<DefaultSpec>::new(1);
        bridge.slot.publish(Arc::new(PresentationFrame {
            session: 0,
            seq: 1,
            tick: 1,
            render_clock_ms: 0,
            timestep_ms: 0.0,
            commands: vec![PresentationCommand::Despawn {
                id: StableEntityId(1),
            }]
            .into(),
        }));
        assert!(bridge.take_presentation_frame().is_none());
    }

    #[test]
    fn current_session_frame_is_returned() {
        let bridge = ClientBridge::<DefaultSpec>::new(3);
        bridge.slot.publish(Arc::new(PresentationFrame {
            session: 3,
            seq: 2,
            tick: 9,
            render_clock_ms: 77,
            timestep_ms: 16.6,
            commands: Vec::new().into(),
        }));
        let frame = bridge
            .take_presentation_frame()
            .expect("本 session 的帧应可取走");
        assert_eq!(frame.seq, 2);
        assert_eq!(frame.tick, 9);
    }

    #[test]
    fn submitting_unknown_actions_is_counted_not_fatal() {
        let bridge = ClientBridge::<DefaultSpec>::new(1);
        let (frame, unknown) = RawInputFrame::from_names::<EmptyActions>(
            ["jump", "some_future_action"],
            [],
            [],
            (0.0, 0.0),
            (0.0, 0.0),
            1,
            0,
        );
        bridge.submit_input_frame(InputSourceId(1), frame, unknown.len());
        assert_eq!(bridge.unknown_actions(), 2);
        assert_eq!(bridge.submitted_frames(), 1);
    }
}
