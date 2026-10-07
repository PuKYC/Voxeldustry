//! 表现命令与帧。
//!
//! **语义必须严格区分**（否则会出现「玩家走远了怪物就被销毁」的灾难性问题）：
//!
//! | 命令 | 触发 | Godot 动作 | 逻辑实体 |
//! |---|---|---|---|
//! | [`PresentationCommand::Attach`] | 首次进入表现层 | 建立数据记录（**不建节点**） | 保留 |
//! | [`PresentationCommand::Add`] / `Update` | 组件首次 / 增量 | 写组件 | 保留 |
//! | [`PresentationCommand::Remove`] | 组件不可见 / 被移除 | 清掉对应表现数据 | 保留 |
//! | [`PresentationCommand::Detach`] | 离开 AOI / 本地视野 | **回收节点** | **保留** |
//! | [`PresentationCommand::Despawn`] | 实体真的消失 | 永久移除 | 消失 |

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use bevy::prelude::Resource;

use crate::identity::StableEntityId;

use super::packed::{FrameStreams, PackedHeader};
use super::payload::{PayloadKindTrait, PresentationPayload};

/// 一条表现命令。
#[derive(Clone, Debug, PartialEq)]
pub enum PresentationCommand<P: PresentationPayload> {
    /// 实体首次进入本地表现层：只让 Godot **建立数据记录**。
    ///
    /// 不携带原型、也不建节点——原型现在是一个普通整数组件（payload
    /// `Prototype`），和位置一样走 `Add`/`Update` 通道。Godot 在帧末按
    /// 「有位置 且 有原型」的谓词统一物化节点，见客户端 `EntityData`。
    Attach { id: StableEntityId },
    /// 组件首次下发。
    Add { id: StableEntityId, payload: P },
    /// 组件增量（Component 级 diff，对齐）。
    Update { id: StableEntityId, payload: P },
    /// 组件不可见 / 被移除：Godot 必须清掉对应表现数据，不能残留。
    Remove { id: StableEntityId, kind: P::Kind },
    /// 离开 AOI / 本地视野：回收节点，**逻辑实体保留**。
    Detach { id: StableEntityId },
    /// 实体真的消失（逻辑 despawn）：永久移除 + 清基线。
    Despawn { id: StableEntityId },
}

impl<P: PresentationPayload> PresentationCommand<P> {
    /// 下发顺序权重（同一帧内按此稳定排序）。
    pub fn rank(&self) -> u8 {
        match self {
            PresentationCommand::Attach { .. } => 0,
            PresentationCommand::Add { .. } => 1,
            PresentationCommand::Update { .. } => 2,
            PresentationCommand::Remove { .. } => 3,
            PresentationCommand::Detach { .. } => 4,
            PresentationCommand::Despawn { .. } => 5,
        }
    }

    /// 确定性排序键：rank，其次实体 ID，再次载荷 kind code。
    ///
    /// 同 rank 内仅靠稳定排序会保留 HashMap/调度顺序，导致 GPF1 字节不可复现。
    fn sort_key(&self) -> (u8, u64, u8) {
        let kind = match self {
            PresentationCommand::Attach { .. } => 0,
            PresentationCommand::Add { payload, .. }
            | PresentationCommand::Update { payload, .. } => payload.kind().code(),
            PresentationCommand::Remove { kind, .. } => kind.code(),
            PresentationCommand::Detach { .. } | PresentationCommand::Despawn { .. } => 0,
        };
        (self.rank(), self.target().0, kind)
    }

    pub fn target(&self) -> StableEntityId {
        match self {
            PresentationCommand::Attach { id, .. }
            | PresentationCommand::Add { id, .. }
            | PresentationCommand::Update { id, .. }
            | PresentationCommand::Remove { id, .. }
            | PresentationCommand::Detach { id }
            | PresentationCommand::Despawn { id } => *id,
        }
    }

    /// 是否是「实体终结」类命令（同帧要抵消掉该 id 的其它命令）。
    fn is_terminal(&self) -> bool {
        matches!(
            self,
            PresentationCommand::Detach { .. } | PresentationCommand::Despawn { .. }
        )
    }

    /// 稳定字符串标签（Godot 侧 `match` 用）。
    pub fn kind_str(&self) -> &'static str {
        match self {
            PresentationCommand::Attach { .. } => "attach",
            PresentationCommand::Add { .. } => "add",
            PresentationCommand::Update { .. } => "update",
            PresentationCommand::Remove { .. } => "remove",
            PresentationCommand::Detach { .. } => "detach",
            PresentationCommand::Despawn { .. } => "despawn",
        }
    }
}

/// 整理命令序列。
///
/// 1. **同帧抵消**：某 id 本帧终结（Detach/Despawn）时，丢掉它同帧的
///    Attach/Add/Update/Remove —— 否则 Godot 会先建节点再销毁，白做一遍；
/// 2. **稳定排序**：`Attach → Add → Update → Remove → Detach → Despawn`，
///    保证「先有节点才有组件」「先移除组件再回收节点」。
pub fn compact_commands<P: PresentationPayload>(commands: &mut Vec<PresentationCommand<P>>) {
    let mut terminal: Vec<StableEntityId> = commands
        .iter()
        .filter(|c| c.is_terminal())
        .map(|c| c.target())
        .collect();

    if !terminal.is_empty() {
        terminal.sort_unstable_by_key(|id| id.0);
        terminal.dedup();
        commands.retain(|command| match command {
            PresentationCommand::Attach { .. }
            | PresentationCommand::Add { .. }
            | PresentationCommand::Update { .. }
            | PresentationCommand::Remove { .. } => terminal
                .binary_search_by_key(&command.target().0, |id| id.0)
                .is_err(),
            _ => true,
        });
    }

    // 确定性：rank 之外再按 (实体 ID, 载荷 kind code) 二级排序。
    commands.sort_by_key(|command| command.sort_key());
}

/// 把一个实体的「每载荷种类最后一次写」插入 / 覆盖到 `kinds`。
fn upsert_kind<P: PresentationPayload>(
    kinds: &mut Vec<(P::Kind, usize, PresentationCommand<P>)>,
    kind: P::Kind,
    index: usize,
    command: PresentationCommand<P>,
) {
    if let Some(slot) = kinds.iter_mut().find(|(k, _, _)| *k == kind) {
        // 后写的覆盖先写的（Add/Update/Remove 都是幂等 set/clear）。
        slot.1 = index;
        slot.2 = command;
    } else {
        kinds.push((kind, index, command));
    }
}

/// 合并「尚未被 Godot 取走」的旧帧与新帧，输出一张**从 Godot 当前状态
/// 一步到位**的净效果命令表。
///
/// 为什么不能直接拼接：`compact_commands` 会按 `rank()` 重排，若旧帧是
/// `Remove(kind)`、新帧是同一实体的 `Add(kind)`（权限被剥夺后又恢复），
/// 拼接后 `Add` 会排到 `Remove` 前面，Godot 最终把组件又清掉了；`Detach`
/// 后重新 `Attach` 同理。本函数按时间顺序归约：
///
/// - 最后一次 `Attach` 之后的终结命令才生效；`Attach` 会清掉更早的终结/载荷；
/// - 每个 `(id, kind)` 只保留最后一次写（Add/Update/Remove）；
/// - 最终节点不存在时只发最后一条终结命令，存在时发 `Attach` + 各载荷终值。
///
/// 该归约不会丢命令（只合并同一 net 效果的重复），因此可反复与后续帧继续合并。
pub fn coalesce_commands<P: PresentationPayload>(
    older: &[PresentationCommand<P>],
    newer: &[PresentationCommand<P>],
) -> Vec<PresentationCommand<P>> {
    struct EntityState<P: PresentationPayload> {
        attach: Option<(usize, PresentationCommand<P>)>,
        terminal: Option<(usize, PresentationCommand<P>)>,
        kinds: Vec<(P::Kind, usize, PresentationCommand<P>)>,
    }

    impl<P: PresentationPayload> Default for EntityState<P> {
        fn default() -> Self {
            Self {
                attach: None,
                terminal: None,
                kinds: Vec::new(),
            }
        }
    }

    let mut states: HashMap<u64, EntityState<P>> = HashMap::new();
    let mut order: Vec<u64> = Vec::new();

    for (index, command) in older.iter().chain(newer.iter()).enumerate() {
        let key = command.target().0;
        let state = states.entry(key).or_insert_with(|| {
            order.push(key);
            EntityState::default()
        });

        match command {
            PresentationCommand::Attach { .. } => {
                // 重新 Attach 等价于从一个干净节点开始：更早的终结与载荷都作废。
                state.attach = Some((index, command.clone()));
                state.terminal = None;
                state.kinds.clear();
            }
            PresentationCommand::Detach { .. } | PresentationCommand::Despawn { .. } => {
                state.terminal = Some((index, command.clone()));
            }
            PresentationCommand::Add { payload, .. }
            | PresentationCommand::Update { payload, .. } => {
                upsert_kind(&mut state.kinds, payload.kind(), index, command.clone());
            }
            PresentationCommand::Remove { kind, .. } => {
                upsert_kind(&mut state.kinds, *kind, index, command.clone());
            }
        }
    }

    let mut out = Vec::new();
    for key in order {
        let Some(state) = states.get(&key) else {
            continue;
        };

        let attach_index = state.attach.as_ref().map(|(i, _)| *i);
        let terminal_index = state.terminal.as_ref().map(|(i, _)| *i);

        // 最终节点是否存在：不存在则只保留最后一条终结命令。
        let node_exists = match (attach_index, terminal_index) {
            (Some(_), None) => true,
            (Some(a), Some(t)) => a > t,
            // 没有 Attach：节点是旧帧之前就建好的，载荷写仍然要发。
            (None, Some(_)) => false,
            (None, None) => true,
        };

        if node_exists {
            if let Some((_, attach)) = state.attach.as_ref() {
                out.push(attach.clone());
            }
            for (_, _, command) in state.kinds.iter() {
                out.push(command.clone());
            }
        } else if let Some((_, terminal)) = state.terminal.as_ref() {
            out.push(terminal.clone());
        }
    }

    compact_commands(&mut out);
    out
}

/// 一帧表现增量。
#[derive(Debug)]
pub struct PresentationFrame<P: PresentationPayload> {
    /// 复用 `session`，让 Godot 丢弃重启前的残留帧。
    pub session: u64,
    /// 帧序号，Godot 可据此检测丢帧（状态帧允许丢）。
    pub seq: u64,
    /// 逻辑 tick。
    pub tick: u32,
    /// 这一帧对应的 fixed tick 时刻 `T_t`（进程级单调时钟，毫秒）。
    /// Godot 用它 + 自身渲染时刻采样插值 alpha（见 `presentation::interp`）。
    pub render_clock_ms: u64,
    /// 逻辑 timestep（毫秒），Godot 采样 alpha 用。
    pub timestep_ms: f32,
    /// 命令列表。`Arc` 共享，Godot 取走后无需拷贝整帧。
    pub commands: Arc<[PresentationCommand<P>]>,
}

impl<P: PresentationPayload> PresentationFrame<P> {
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

/// 表现帧单槽。
///
/// Bevy 写、Godot 取（`take`）。**不能简单覆盖旧帧**：增量命令（Attach /
/// Remove / Detach / Despawn）一旦丢失不会重发（基线已经记账），会造成永久
/// 缺节点、幽灵节点或隐私数据残留。因此覆盖时用 `coalesce_commands` 把旧帧
/// 尚未取走的命令合并进新帧；只有等价的状态快照会被真正合并掉。
/// 槽内一帧：原始表现帧 + 预生成的 SoA 流。
///
/// SoA 在**发布时（Bevy 线程）**算一次，Godot 取用时零解码开销；
/// GPF1 字节改为 `take_packed` 惰性编码（兼容通道用）。
#[derive(Debug)]
pub struct SlotFrame<P: PresentationPayload> {
    pub frame: Arc<PresentationFrame<P>>,
    /// 预生成的 SoA 流，见 [crate::presentation::packed]。
    pub streams: FrameStreams,
}

/// 表现帧单槽。
///
/// Bevy 写、Godot 取（`take`）。**不能简单覆盖旧帧**：增量命令（Attach /
/// Remove / Detach / Despawn）一旦丢失不会重发（基线已经记账），会造成永久
/// 缺节点、幽灵节点或隐私数据残留。因此覆盖时用 `coalesce_commands` 把旧帧
/// 尚未取走的命令合并进新帧；只有等价的状态快照会被真正合并掉。
///
/// 同一帧同时提供三条消费路径：`take`（Dictionary 兼容通道）、
/// `take_streams`（SoA 快通道，零解码）与 `take_packed`（GPF1 字节，惰性编码）。
/// 三者**互斥消费**——一帧只能被取走一次。
#[derive(Resource, Clone)]
pub struct PresentationSlot<P: PresentationPayload> {
    inner: Arc<Mutex<Option<SlotFrame<P>>>>,
}

impl<P: PresentationPayload> Default for PresentationSlot<P> {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
        }
    }
}

impl<P: PresentationPayload> PresentationSlot<P> {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Option<SlotFrame<P>>> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// 合并发布。返回 `true` 表示**覆盖掉了一帧还没被 Godot 取走的帧**
    /// （说明 Godot 掉帧/卡住了，可用于诊断）。
    pub fn publish(&self, frame: Arc<PresentationFrame<P>>) -> bool {
        let mut guard = self.lock();
        let overwritten = guard.is_some();

        let merged = match guard.take() {
            Some(previous) => {
                let commands = coalesce_commands(&previous.frame.commands, &frame.commands);
                Arc::new(PresentationFrame {
                    session: frame.session,
                    seq: frame.seq,
                    tick: frame.tick,
                    render_clock_ms: frame.render_clock_ms,
                    timestep_ms: frame.timestep_ms,
                    commands: commands.into(),
                })
            }
            None => frame,
        };

        let streams = crate::presentation::packed::frame_to_streams::<P>(&merged);
        *guard = Some(SlotFrame {
            frame: merged,
            streams,
        });

        overwritten
    }

    /// 取走最新帧（Dictionary 兼容通道）；没有则返回 `None`。
    pub fn take(&self) -> Option<Arc<PresentationFrame<P>>> {
        self.lock().take().map(|entry| entry.frame)
    }

    /// 取走最新帧的原始 `SlotFrame`（含预生成 SoA 流）；没有则返回 `None`。
    pub fn take_entry(&self) -> Option<SlotFrame<P>> {
        self.lock().take()
    }

    /// 取走最新帧的 GPF1 打包字节（兼容通道，**惰性编码**）；没有则返回 `None`。
    ///
    /// 快通道请优先用 [`PresentationSlot::take_streams`]，避免这次编码。
    pub fn take_packed(&self) -> Option<Arc<[u8]>> {
        let entry = self.lock().take()?;
        let bytes = crate::presentation::packed::encode_frame::<P>(&entry.frame);
        Some(Arc::from(bytes.into_boxed_slice()))
    }

    /// 取走最新帧的 SoA 流 + 重建的帧头（快通道，零解码）；没有则返回 `None`。
    pub fn take_streams(&self) -> Option<(PackedHeader, FrameStreams)> {
        let entry = self.lock().take()?;
        let header = PackedHeader {
            session: entry.frame.session,
            seq: entry.frame.seq,
            tick: entry.frame.tick,
            render_clock_ms: entry.frame.render_clock_ms,
            timestep_ms: entry.frame.timestep_ms,
            command_count: entry.frame.commands.len() as u32,
        };
        Some((header, entry.streams))
    }

    pub fn has_pending(&self) -> bool {
        self.lock().is_some()
    }
}
