//! 一次性表现事件的通用保序队列（不可丢）。
//!
//! 与状态载荷的区别：状态可覆盖、可丢帧，走单槽；一次性事实不幂等，丢了就是丢了，
//! 走有界 + try_send + 丢弃计数的保序队列。具体事件类型由游戏侧实现
//! [PresentationEventData]。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use bevy::prelude::Resource;
use crossbeam_channel::{bounded, Receiver, Sender};

use crate::identity::StableEntityId;

/// 事件队列容量（有界，宁可可观测地丢，也不无界增长）。
pub const EVENT_QUEUE_CAPACITY: usize = 4096;

/// 逻辑层派生的一次性事实。
pub trait PresentationEventData: Clone + core::fmt::Debug + Send + Sync + 'static {
    /// 稳定字符串标签（Godot 侧 match 用）。
    fn kind_str(&self) -> &'static str;
    /// 事件所属的稳定实体 ID。
    fn entity(&self) -> StableEntityId;
}

/// 带逻辑 tick 的表现事件（tick 用于与表现帧建立相对顺序）。
#[derive(Clone, Debug)]
pub struct TimedPresentationEvent<E> {
    pub tick: u32,
    pub event: E,
}

struct QueueInner<E> {
    tx: Sender<TimedPresentationEvent<E>>,
    rx: Receiver<TimedPresentationEvent<E>>,
    dropped: AtomicU64,
}

impl<E> Default for QueueInner<E> {
    fn default() -> Self {
        let (tx, rx) = bounded(EVENT_QUEUE_CAPACITY);
        Self {
            tx,
            rx,
            dropped: AtomicU64::new(0),
        }
    }
}

/// 表现事件队列句柄（Bevy 线程推、Godot 主线程 drain）。
#[derive(Resource)]
pub struct PresentationEventQueue<E: PresentationEventData> {
    inner: Arc<QueueInner<E>>,
}

impl<E: PresentationEventData> Default for PresentationEventQueue<E> {
    fn default() -> Self {
        Self {
            inner: Arc::new(QueueInner::<E>::default()),
        }
    }
}

impl<E: PresentationEventData> Clone for PresentationEventQueue<E> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<E: PresentationEventData> PresentationEventQueue<E> {
    pub fn new() -> Self {
        Self::default()
    }

    /// 推入一条事件（带产生它的逻辑 tick）。
    ///
    /// 返回 false 表示队列已满、该事件被丢弃（调用方应打 error! 并监控）。
    pub fn push(&self, tick: u32, event: E) -> bool {
        match self
            .inner
            .tx
            .try_send(TimedPresentationEvent { tick, event })
        {
            Ok(()) => true,
            Err(_) => {
                self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// 取走当前所有事件（保序，带 tick）。
    pub fn drain(&self) -> Vec<TimedPresentationEvent<E>> {
        self.inner.rx.try_iter().collect()
    }

    /// 累计被丢弃的事件数。
    pub fn dropped(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    pub fn len(&self) -> usize {
        self.inner.rx.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.rx.is_empty()
    }
}

/// 「紧凑 ID」的通用样板生成器。
///
/// 形如 `compact_id!(/// 文档 \n Name)`：可选 doc 属性 + 一个标识符，
/// 生成 `pub struct Name(pub u32)`，并派生
/// `Clone, Copy, PartialEq, Eq, Hash, Debug, Default` 与 serde 的
/// `Serialize, Deserialize`。
///
/// derive 使用完整路径 `serde::Serialize` / `serde::Deserialize`，
/// 不依赖调用方作用域里是否 import 了 serde 名字。具体 ID 命名空间
/// （音效 / 动画 / 特效 ……）由游戏侧用本宏声明。
#[macro_export]
macro_rules! compact_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Clone, Copy, PartialEq, Eq, Hash, Debug, Default, serde::Serialize, serde::Deserialize,
        )]
        pub struct $name(pub u32);
    };
}
