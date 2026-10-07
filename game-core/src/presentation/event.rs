//! 一次性表现事件（游戏侧具体事件类型 + 引擎队列别名）。

use serde::{Deserialize, Serialize};

use game_engine::compact_id;
use game_engine::identity::StableEntityId;

pub use game_engine::presentation::event::{
    PresentationEventData, PresentationEventQueue as GenericEventQueue,
    TimedPresentationEvent as GenericTimed, EVENT_QUEUE_CAPACITY,
};

compact_id!(
    /// 音效 ID。
    SoundId
);
compact_id!(
    /// 动画片段 ID。
    AnimId
);
compact_id!(
    /// 特效 ID。
    VfxId
);

/// 逻辑层派生的「事实」。Godot 只决定怎么演，不决定演不演。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PresentationEvent {
    PlaySound {
        id: StableEntityId,
        sound: SoundId,
        position: [f32; 3],
    },
    TriggerAnim {
        id: StableEntityId,
        anim: AnimId,
        speed: f32,
    },
    SpawnVfx {
        id: StableEntityId,
        vfx: VfxId,
        position: [f32; 3],
    },
    DamagePopup {
        id: StableEntityId,
        amount: i32,
    },
}

impl PresentationEvent {
    /// 稳定字符串标签（Godot 侧 match 用）。
    pub fn kind_str(&self) -> &'static str {
        match self {
            PresentationEvent::PlaySound { .. } => "play_sound",
            PresentationEvent::TriggerAnim { .. } => "trigger_anim",
            PresentationEvent::SpawnVfx { .. } => "spawn_vfx",
            PresentationEvent::DamagePopup { .. } => "damage_popup",
        }
    }

    pub fn entity(&self) -> StableEntityId {
        match self {
            PresentationEvent::PlaySound { id, .. }
            | PresentationEvent::TriggerAnim { id, .. }
            | PresentationEvent::SpawnVfx { id, .. }
            | PresentationEvent::DamagePopup { id, .. } => *id,
        }
    }
}

impl PresentationEventData for PresentationEvent {
    fn kind_str(&self) -> &'static str {
        PresentationEvent::kind_str(self)
    }
    fn entity(&self) -> StableEntityId {
        PresentationEvent::entity(self)
    }
}

/// game-core 的事件队列特化。
pub type TimedPresentationEvent = GenericTimed<PresentationEvent>;
/// game-core 的事件队列句柄特化。
pub type PresentationEventQueue = GenericEventQueue<PresentationEvent>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_preserves_order() {
        let queue = PresentationEventQueue::new();
        for amount in 0..5i32 {
            assert!(queue.push(
                0,
                PresentationEvent::DamagePopup {
                    id: StableEntityId(1),
                    amount,
                }
            ));
        }
        let drained = queue.drain();
        let amounts: Vec<i32> = drained
            .iter()
            .map(|e| match &e.event {
                PresentationEvent::DamagePopup { amount, .. } => *amount,
                _ => unreachable!(),
            })
            .collect();
        assert!(drained.iter().all(|e| e.tick == 0));
        assert_eq!(amounts, vec![0, 1, 2, 3, 4]);
        assert!(queue.drain().is_empty(), "drain 之后应为空");
    }

    #[test]
    fn overflow_is_counted_not_silent() {
        let queue = PresentationEventQueue::new();
        for i in 0..(EVENT_QUEUE_CAPACITY + 8) {
            let _ = queue.push(
                0,
                PresentationEvent::DamagePopup {
                    id: StableEntityId(i as u64),
                    amount: 1,
                },
            );
        }
        assert_eq!(queue.dropped(), 8, "溢出必须被计数，不能静默丢");
    }
}
