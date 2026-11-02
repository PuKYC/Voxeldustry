//! 存档接线：后台 IO 线程 + `ChunkStore` 实现（《存档格式规格》§5.1、§7）。
//!
//! 本模块把 [`super::store`] 的纯文件 IO 接入 Bevy：
//!
//! - [`SaveStore`] 是资源，实现 [`crate::streaming::ChunkStore`]；`lookup` **只查表、
//!   不阻塞、不做 IO**，返回 `Hit` / `Miss` / `Pending`（§7）；
//! - 真正的磁盘读写在**一条** `std::thread` 里（不是 `IoTaskPool`），请求/结果都走
//!   `crossbeam_channel`；`stop` 通道 + `Drop` 里 join，退出时不留 detached 线程；
//! - [`SaveSet::Flush`] 推进 tick、按预算把内存缓冲（写或删除）交给后台；[`SaveSet::Poll`] 按
//!   `(BodyKey, ChunkKey)` 键序应用结果、处理读超时、发 [`SaveFailed`] / [`SaveSucceeded`]。
//!
//! # 读优先级（§7）
//!
//! **内存缓冲 > 墓碑 → `Miss` > `.recovered` > 主文件 > `Miss`**：
//!
//! - 缓冲命中直接 `Hit`，不发 IO（卸载后立刻重载能读到最新内容）；
//! - 没有缓冲、没有在途读 → 登记在途、把读请求塞进队列、返回 `Pending`；
//! - 后台结果回来 → 放进 `ready`，下一次 `lookup` 交付 `Hit`（一次性交付）；
//! - 本会话已知 `Miss` 的块直接 `Miss`，直到该块被 [`SaveStore::write`] 写过（§7）；
//! - 墓碑命中也直接 `Miss`：删除过的块在落盘前就不能再把旧内容交回去；
//! - 超过 `read_timeout_ticks` 还没结果 → `Poll` 里转 `Miss` + `SaveFailed`。
//!
//! # 写（§5.1）
//!
//! [`SaveStore::write`] 只进内存缓冲（同键覆盖、最新赢）；`Flush` 按
//! `write_budget_per_tick` 把脏块交给后台线程，后台做**整 region 读改写** + 原子写。
//! **写成功前缓冲不丢**：写失败保留缓冲并重新标脏，下一次 `Flush` 重试。
//!
//! # 删除（墓碑）
//!
//! [`SaveStore::delete`] 把块标成“不应再有记录”：`lookup` 立即 `Miss`，`Flush` 时
//! 后台读 region、移除该块并把整 region 原子写回；region 因此变空就直接删文件。
//! 删除与写入共用预算、FIFO 队列与失败重试路径，细节见 [`SaveStore`] 的删除语义。
//!
//! # 语义边界
//!
//! - 本层不做实体快照、不写 `world.meta`、不做 `world.lock`、不做 `ChunkWillUnload` 的
//!   “卸载前必须落盘”钩子；
//! - 后台线程是唯一做 IO 的一方（它自己持有 [`SavePaths`]，所以 `.corrupt` / quarantine 的
//!   会话状态也只属于它）；主线程只用路径拼消息里的 `path`；
//! - 读失败的告警由调用方决定：本层把原因放进 [`SaveFailed`]。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bevy::prelude::*;
use crossbeam_channel::{unbounded, Receiver, Sender};

use ::voxel::ChunkKey;

use super::store::{read_region, remove_region, write_region, SavePaths};
use super::RegionFile;
use crate::body_key::BodyKey;
use crate::residency::GameTickSet;
use crate::streaming::{ChunkRecord, ChunkStore, RecordError, StoreLookup};

/// 一个 chunk 的稳定标识（全局唯一：体 + 体局部块坐标）。
type ChunkId = (BodyKey, ChunkKey);

/// 一 tick 内 `Poll` 最多应用多少条后台结果。
///
/// 后台可以一次回很多条（一次 Flush 预算、多个读完成），一 tick 全应用会把帧时间拉尖；
/// 超出部分留到下个 tick，仍然按键序。
pub const POLL_APPLY_BUDGET: usize = 64;

/// 默认读超时（tick）：§7 的 300 tick ≈ 5 s。
pub const DEFAULT_READ_TIMEOUT_TICKS: u64 = 300;
/// 默认每 tick 写/删预算（块数）。
pub const DEFAULT_WRITE_BUDGET_PER_TICK: usize = 8;
/// 默认自动 Flush 周期（tick）：1 = 每 tick 都发。
///
/// 还没有 `ChunkWillUnload` 落盘钩子之前不做批量：周期变大等于把最近一段时间的编辑
/// 只留在内存里，崩溃就丢。省 IO 的批量策略等有了卸载钩子和实测数据再谈。
pub const DEFAULT_FLUSH_INTERVAL_TICKS: u64 = 1;

// ───────────────────────────── 配置与插件 ─────────────────────────────

/// 存档插件配置。
#[derive(Resource, Clone, Debug, PartialEq, Eq)]
pub struct SaveConfig {
    /// 存档根目录（`saves/<world>`，§1）。
    pub root: PathBuf,
    /// 读超时（tick）：超过仍未回结果 → `Miss` + `SaveFailed`。0 = 立即超时。
    pub read_timeout_ticks: u64,
    /// 每 tick 交给后台线程的块数上限（0 = 本 tick 不发）。
    pub write_budget_per_tick: usize,
    /// 自动 Flush 的周期；≤1 表示每 tick；[`SaveStore::flush`] 不受它限制。
    ///
    /// 默认 1（每 tick）：没有 `ChunkWillUnload` 落盘钩子时，周期 >1 意味着最近一段
    /// 时间的编辑只在内存里，崩溃即丢。
    pub flush_interval_ticks: u64,
}

impl Default for SaveConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::from("saves/world"),
            read_timeout_ticks: DEFAULT_READ_TIMEOUT_TICKS,
            write_budget_per_tick: DEFAULT_WRITE_BUDGET_PER_TICK,
            flush_interval_ticks: DEFAULT_FLUSH_INTERVAL_TICKS,
        }
    }
}

/// 存档插件：插 [`SaveConfig`] / [`SaveStore`] 资源，注册两个消息，挂 [`SaveSet`]。
#[derive(Debug, Clone, Default)]
pub struct SavePlugin {
    /// 插件配置（同时作为 `SaveConfig` 资源插入）。
    pub config: SaveConfig,
}

impl Plugin for SavePlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(self.config.clone())
            .insert_resource(SaveStore::new(self.config.clone()))
            .add_message::<SaveFailed>()
            .add_message::<SaveSucceeded>()
            // 只 chain 自己的两个集合，整条链挂在 GameTickSet::Save 内（不 chain 别人的）。
            .configure_sets(
                FixedUpdate,
                (SaveSet::Flush, SaveSet::Poll)
                    .chain()
                    .in_set(GameTickSet::Save),
            )
            .add_systems(
                FixedUpdate,
                (
                    flush_system.in_set(SaveSet::Flush),
                    poll_system.in_set(SaveSet::Poll),
                ),
            );
    }
}

/// 存档插件内部的系统顺序（在每个 tick 内 `chain`）。
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SaveSet {
    /// 推进 tick、按预算把脏块交给后台线程。
    Flush,
    /// 回收后台结果、处理读超时、发消息。
    Poll,
}

// ───────────────────────────── 消息 ─────────────────────────────

/// 一次存档 IO 失败（§5.3 的 `SaveFailed { path, reason }`）。
///
/// 读超时、解码失败、写盘失败都走这里；[`chunk`] 是涉及的块，世界级失败为 `None`。
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct SaveFailed {
    /// 失败的文件路径（region 文件）。
    pub path: PathBuf,
    /// 人类可读原因（带阶段 / 偏移，来自 `SaveError` 的 `Display`）。
    pub reason: String,
    /// 涉及的块；世界级失败为 `None`。
    pub chunk: Option<(BodyKey, ChunkKey)>,
}

/// 一次存档写盘成功（§5.1 第 5 步之后才发）。
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct SaveSucceeded {
    /// 实际写到的文件（可能是 `.recovered`，§5.3）。
    pub path: PathBuf,
    /// 涉及的块；世界级失败为 `None`。
    pub chunk: Option<(BodyKey, ChunkKey)>,
}

// ───────────────────────────── Poll 报告 ─────────────────────────────

/// 一次 `Poll` 的处理结果，供系统和测试使用。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PollReport {
    /// 本 tick 实际应用的键，**按应用顺序**（`(BodyKey, ChunkKey)` 升序）。
    pub applied: Vec<(BodyKey, ChunkKey)>,
    /// 因 `epoch` 不匹配被丢弃的迟到结果条数（§7：迟到的读不能回写状态）。
    pub dropped: usize,
    /// 本 tick 超时转 `Miss` 的读条数。
    pub timed_out: usize,
    /// 要发的失败消息。
    pub failures: Vec<SaveFailed>,
    /// 要发的成功消息。
    pub successes: Vec<SaveSucceeded>,
}

/// [`SaveStore`] 的只读快照，用于诊断与测试。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SaveStats {
    /// 当前 tick（由 `Flush` 系统推进）。
    pub tick: u64,
    /// 内存缓冲里的块数（未确认写盘）。
    pub buffered: usize,
    /// 待交给后台线程的块数。
    pub dirty: usize,
    /// 已交给后台、还没回结果的写条数。
    pub in_flight_writes: usize,
    /// 已发出、还没回结果的读条数。
    pub in_flight_reads: usize,
    /// 本会话已知 `Miss` 的块数。
    pub known_misses: usize,
    /// 后台已回、还没被 `lookup` 取走的命中数。
    pub ready_hits: usize,
    /// 已标记删除、还没确认落盘的块数（墓碑）。
    pub tombstones: usize,
}

// ───────────────────────────── 屏障错误 ─────────────────────────────

/// [`SaveStore::quiesce`] 失败的原因。
///
/// 与 [`RecordError`] 分开放：那个是“这条记录本身非法”，这个是“屏障等不到
/// 一个可以安全落盘的时刻”，调用方对两者的处置不同（前者拒绝写，后者放弃
/// 本次全量存档并报告）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuiesceError {
    /// 没有后台线程：永远不会确认，屏障无意义。
    NoWorker,
    /// 后台线程已退出，仍有未完成的写。
    WorkerDead,
    /// 超时：后台一直没把在途写做完。
    Timeout,
}

impl fmt::Display for QuiesceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoWorker => write!(f, "存档屏障无意义：没有后台 IO 线程"),
            Self::WorkerDead => write!(f, "存档后台线程已退出，仍有未完成的写"),
            Self::Timeout => write!(f, "存档屏障超时：后台未在限定时间内完成在途写"),
        }
    }
}

impl std::error::Error for QuiesceError {}

// ───────────────────────────── SaveStore ─────────────────────────────

/// 存档资源：内存缓冲 + 在途状态 + 后台 IO 线程句柄。
///
/// 作为 `Resource` 插入；[`ChunkStore`] 的 `lookup` 可以给流式层直接同步调用。
///
/// # 删除语义（墓碑）
///
/// [`SaveStore::delete`] 表示“这个块不应再有记录”（例如区块卸载时内容与实体都空）。
/// 它只改内存：从缓冲移除、记一条墓碑、标脏，`lookup` 立即返回 `Miss`；磁盘删除
/// 与其他写入一样**只在 Flush 时**发生（同一条后台队列、同一个预算），因此不阻塞
/// 卸载 tick，也天然按 FIFO 排在此前对该块的写之后。
///
/// 后台删除会读旧 region 并移除该块：
///
/// - 块本来就不在 region（或 region 文件不存在）→ 什么都不写，避免为每个未编辑
///   块制造一次写放大；
/// - region 因此变空 → 删除 region 文件（主文件与 `.recovered` 都删，否则读路径
///   会从 `.recovered` 把旧内容读回来）；
/// - 其余情况 → 原子重写该 region。
///
/// 读失败或删除文件失败时墓碑保留、块重新标脏，下一次 Flush 重试；成功（含
/// no-op）才清墓碑。删除成功同样发 [`SaveSucceeded`]（`chunk` 指向该块）。
#[derive(Resource)]
pub struct SaveStore {
    config: SaveConfig,
    /// 主线程只用它拼路径（真正的 IO 在后台线程，那边有自己的一份）。
    paths: SavePaths,
    state: Mutex<StoreState>,
    worker: Option<WorkerThread>,
    results: Option<Receiver<WorkerResult>>,
}

impl SaveStore {
    /// 按配置启动后台 IO 线程。
    ///
    /// 线程启动失败（OS 资源耗尽）直接 panic：没有后台 IO 的存档等于永远 `Miss`，
    /// 静默降级比崩溃更难查。
    #[must_use]
    pub fn new(config: SaveConfig) -> Self {
        let (worker, results) = WorkerThread::spawn(config.root.clone());
        Self {
            paths: SavePaths::new(config.root.clone()),
            config,
            state: Mutex::new(StoreState::default()),
            worker: Some(worker),
            results: Some(results),
        }
    }

    /// 不启动后台线程的构造（测试用）：读请求没人取 → 走超时路径。
    #[cfg(test)]
    fn offline(config: SaveConfig) -> Self {
        Self {
            paths: SavePaths::new(config.root.clone()),
            config,
            state: Mutex::new(StoreState::default()),
            worker: None,
            results: None,
        }
    }

    /// 带结果通道、但没有后台线程的构造（测试用）：把结果塞进通道就能驱动 `poll`。
    #[cfg(test)]
    fn with_results_channel(config: SaveConfig) -> (Self, Sender<WorkerResult>) {
        let (sender, receiver) = unbounded();
        (
            Self {
                paths: SavePaths::new(config.root.clone()),
                config,
                state: Mutex::new(StoreState::default()),
                worker: None,
                results: Some(receiver),
            },
            sender,
        )
    }

    /// 插件配置。
    #[must_use]
    pub fn config(&self) -> &SaveConfig {
        &self.config
    }

    /// 当前 tick（`Flush` 系统推进）。
    #[must_use]
    pub fn tick(&self) -> u64 {
        self.state().tick
    }

    /// 该块**已确认落盘**的最高版本号；从未成功写过或删过返回 `None`。
    ///
    /// “确认”指后台线程回报 `WriteOutcome::Ok`（原子写 + fsync 已完成），不是
    /// 指记录进了内存缓冲。调用方（流式层的卸载释放）用它决定数据是否安全落地。
    #[must_use]
    pub fn committed_version(&self, body: BodyKey, key: ChunkKey) -> Option<u64> {
        self.state().committed.get(&(body, key)).copied()
    }

    /// 版本 `version` 是否已经落盘（`committed_version >= version`）。
    ///
    /// 版本号全局单调，因此“更新的版本已落盘”也算这一版安全：那块的最新内容
    /// 已经在磁盘上，不会因为释放内存而丢数据。
    #[must_use]
    pub fn is_committed(&self, body: BodyKey, key: ChunkKey, version: u64) -> bool {
        self.committed_version(body, key)
            .is_some_and(|done| done >= version)
    }

    /// 是否有后台 IO 线程。没有线程的 store（测试替身）永远不会回报落盘确认，
    /// 调用方不能无限等它。
    #[must_use]
    pub fn has_worker(&self) -> bool {
        self.worker.is_some()
    }

    /// 只读快照。
    #[must_use]
    pub fn stats(&self) -> SaveStats {
        let state = self.state();
        SaveStats {
            tick: state.tick,
            buffered: state.buffer.len(),
            dirty: state.dirty.len(),
            in_flight_writes: state.in_flight_writes.len(),
            in_flight_reads: state.in_flight_reads.len(),
            known_misses: state.misses.len(),
            ready_hits: state.ready.len(),
            tombstones: state.tombstones.len(),
        }
    }

    /// 还没交给后台的脏块数（写或删除）。
    ///
    /// [`SaveStore::quiesce`] 返回 `Ok` 后仍可能非 0：写失败会被
    /// [`apply_write`] 重新标脏，留到下一 tick 的 `Flush` 重试。SaveNow 用它
    /// 决定是否提示“本次快照已写入，但还有块没落盘”。
    #[must_use]
    pub fn dirty_count(&self) -> usize {
        self.state().dirty.len()
    }

    /// 后台线程是否还活着（诊断用）。
    #[must_use]
    pub fn worker_running(&self) -> bool {
        self.worker
            .as_ref()
            .is_some_and(|worker| worker.alive.load(Ordering::Acquire))
    }

    /// 停掉后台线程并 join（`Drop` 也会做；重复调用安全）。
    pub fn shutdown_worker(&mut self) {
        // take() 之后立刻 drop → WorkerThread::drop 发 stop 并 join。
        self.worker.take();
    }

    /// 把一个块写进内存缓冲（同键覆盖、最新赢），等 `Flush` 交给后台。
    ///
    /// 组合非法的记录（§3.1：三者全空、`content` 与提案并存）直接拒绝，不进缓冲。
    ///
    /// 返回这一版写的**版本号**：全局单调递增（不按块复位），调用方可以拿它配
    /// [`SaveStore::is_committed`] 判断这次写是否真的落了盘。同键覆盖时旧版本的
    /// 迟到结果不会清掉更新的意图（版本比较），但“旧版本内容确实写过盘”这件事
    /// 仍会计入 committed——版本号单调，所以不会把它误当成新版本已落盘。
    pub fn write(
        &self,
        body: BodyKey,
        key: ChunkKey,
        record: ChunkRecord,
    ) -> Result<u64, RecordError> {
        record.validate()?;
        let id = (body, key);
        let mut state = self.state();
        let version = state.next_version();
        state.versions.insert(id, version);
        state.buffer.insert(id, record);
        state.tombstones.remove(&id);
        state.dirty.insert(id);
        // 最新写入压过在途读：旧磁盘内容不能再回来（迟到的结果靠 epoch 丢弃）。
        state.in_flight_reads.remove(&id);
        state.ready.remove(&id);
        state.misses.remove(&id);
        Ok(version)
    }

    /// 标记一个块“不应再有记录”（卸载空块时调用）。
    ///
    /// 只改内存状态：从缓冲移除、记墓碑、标脏，`lookup` 从此返回 `Miss`。
    /// 真正的磁盘删除发生在 [`SaveStore::flush`] / `Flush` 系统里，与写入共用
    /// 队列和预算；失败时墓碑保留，下一次 Flush 重试（见类型级文档的删除语义）。
    ///
    /// 返回 `Result` 与 [`SaveStore::write`] 对称，调用方可以对两者用同一套
    /// `Err` → warn 处理；目前没有会失败的校验。成功时返回本次删除的**版本号**
    ///（与 `write` 同一条全局递增序列，可以交给 [`SaveStore::is_committed`]）。
    pub fn delete(&self, body: BodyKey, key: ChunkKey) -> Result<u64, RecordError> {
        let id = (body, key);
        let mut state = self.state();
        let version = state.next_version();
        // 换一个新版本号：在途的旧写/旧读因此作废，迟到的成功不会清掉墓碑。
        state.versions.insert(id, version);
        state.buffer.remove(&id);
        state.tombstones.insert(id);
        state.dirty.insert(id);
        state.in_flight_reads.remove(&id);
        state.ready.remove(&id);
        state.misses.insert(id);
        Ok(version)
    }

    /// `ChunkStore::lookup` 的实现（§7）。
    ///
    /// 只查内存、不阻塞：缓冲 → 墓碑 → 已回结果 → 已知 Miss → 在途 → 新请求。
    pub fn lookup(&self, body: BodyKey, key: ChunkKey) -> StoreLookup {
        let id = (body, key);
        let mut state = self.state();

        if let Some(record) = state.buffer.get(&id) {
            return StoreLookup::Hit(record.clone());
        }
        // 墓碑优先于任何缓存：删除过、还没落盘的块也不能把旧内容交回去。
        if state.tombstones.contains(&id) {
            return StoreLookup::Miss;
        }
        // 磁盘命中是一次性交付：记录交给调用方，不在内存里留副本。
        if let Some(record) = state.ready.remove(&id) {
            return StoreLookup::Hit(record);
        }
        if state.misses.contains(&id) {
            return StoreLookup::Miss;
        }
        if state.in_flight_reads.contains_key(&id) {
            return StoreLookup::Pending;
        }

        let epoch = state.next_epoch;
        let issued_tick = state.tick;
        state.next_epoch = state.next_epoch.wrapping_add(1);
        state
            .in_flight_reads
            .insert(id, ReadTicket { epoch, issued_tick });
        if let Some(worker) = &self.worker {
            // 发送失败（线程已退出）不算错：请求留在在途表里，走超时 → Miss。
            let _ = worker
                .jobs
                .send(WorkerJob::Read(ReadRequest { body, key, epoch }));
        }
        StoreLookup::Pending
    }

    /// `Flush` 系统每 tick 调一次：推进 tick，到周期就按预算发一批。
    ///
    /// 返回本 tick 发给后台的块数。
    pub fn on_tick(&self) -> usize {
        let due = {
            let mut state = self.state();
            state.tick = state.tick.wrapping_add(1);
            flush_due(&self.config, &state)
        };
        if due {
            self.flush()
        } else {
            0
        }
    }

    /// 立刻按 `write_budget_per_tick` 把脏块（写或删除）交给后台线程
    ///（不受 Flush 周期限制）。
    ///
    /// 返回实际发出的块数（删除也算一条）。
    pub fn flush(&self) -> usize {
        let Some(worker) = &self.worker else {
            return 0;
        };
        let mut state = self.state();
        let ids: Vec<ChunkId> = state
            .dirty
            .iter()
            .copied()
            .take(self.config.write_budget_per_tick)
            .collect();
        let mut sent = 0;
        for id in ids {
            let version = state.versions.get(&id).copied().unwrap_or(0);
            let job = if let Some(record) = state.buffer.get(&id).cloned() {
                WorkerJob::Write(WriteRequest {
                    body: id.0,
                    key: id.1,
                    record,
                    version,
                })
            } else if state.tombstones.contains(&id) {
                WorkerJob::Delete(DeleteRequest {
                    body: id.0,
                    key: id.1,
                    version,
                })
            } else {
                // 既没有内容也没有墓碑：脏标记是残留，清掉即可。
                state.dirty.remove(&id);
                continue;
            };
            if worker.jobs.send(job).is_err() {
                // 后台已退出：保留 dirty，等下次（或调用方重新建 store）。
                break;
            }
            state.dirty.remove(&id);
            state.in_flight_writes.insert(id, version);
            sent += 1;
        }
        sent
    }

    /// 同步屏障：把所有脏块交给后台，并阻塞到所有在途写都有结果（成功或失败）。
    ///
    /// 返回 `Ok` 只保证**此刻没有在途 region 写**、且每个脏块至少投递过一次。
    /// 不保证磁盘与内存完全一致：写失败会被 [`apply_write`] 重新标脏，残留的
    /// 脏块留给下一 tick 的 `Flush` 重试。等待循环里刻意不再 `flush()`——磁盘
    /// 满、坏块这类永久失败如果每轮重投，会让屏障一直重试到超时。
    ///
    /// 为什么需要：SaveNow 会整文件重写 region（见 game-core 的
    /// `write_capture` 合并写）。后台写是“读旧 region -> 插入本块 -> 整文件
    /// 覆盖”，若与本函数之后的 SaveNow 重叠，后台那条写可能在 SaveNow 之后
    /// 落地、用旧内容覆盖它。屏障把两条写路径在时间上分开。
    ///
    /// 不持锁睡眠：每轮只短暂加锁读一次进度，睡眠前 guard 已经释放。
    pub fn quiesce(&self, timeout: Duration) -> Result<(), QuiesceError> {
        // flush() 在 worker 为 None 时直接返回 0，所以“没有线程”必须先判，
        // 否则会在下面的循环里空转到超时。
        if !self.worker_running() {
            // 线程已经没了：先把通道里已经回来的结果收掉（shutdown 前完成的写
            // 可能还排在结果通道里），再判断有没有永远等不到确认的写。
            while !self.poll().applied.is_empty() {}
            if self.dirty_count() > 0 || self.in_flight_write_count() > 0 {
                return Err(QuiesceError::WorkerDead);
            }
            // 从未有过后台线程（离线 store / 测试替身）：屏障没有意义，是否
            // 继续由调用方决定（SaveNow 在无 worker 时直接跳过屏障）。
            if !self.has_worker() {
                return Err(QuiesceError::NoWorker);
            }
        }

        let started = Instant::now();
        // flush 受 write_budget_per_tick 限制，要循环到 dirty 排空（每次发送
        // 都会把块从 dirty 移进 in_flight_writes）。
        while self.flush() > 0 {}

        // 发送阶段就可能发现线程已经退出（jobs 通道关闭）：留下未投递的脏块
        // 不能报 Ok，否则 SaveNow 会以为屏障成功。
        if !self.worker_running() && self.dirty_count() > 0 {
            return Err(QuiesceError::WorkerDead);
        }

        loop {
            // poll 一次最多应用 POLL_APPLY_BUDGET 条；结果多时要多轮，直到没有
            // 可应用的结果，否则会在结果还没收完时白睡 1ms。
            while !self.poll().applied.is_empty() {}
            if self.in_flight_write_count() == 0 {
                return Ok(());
            }
            if !self.worker_running() {
                return Err(QuiesceError::WorkerDead);
            }
            if started.elapsed() >= timeout {
                return Err(QuiesceError::Timeout);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// 在途写数量（屏障内部用，避免每轮构造整个 [`SaveStats`]）。
    fn in_flight_write_count(&self) -> usize {
        self.state().in_flight_writes.len()
    }

    /// 回收后台结果 + 处理读超时，返回本 tick 要发的消息。
    ///
    /// 结果先收集、按 `(BodyKey, ChunkKey)` 键序应用（同键保持到达顺序），
    /// 一 tick 最多 [`POLL_APPLY_BUDGET`] 条。
    pub fn poll(&self) -> PollReport {
        let mut report = PollReport::default();
        let mut batch = Vec::new();
        if let Some(results) = &self.results {
            while batch.len() < POLL_APPLY_BUDGET {
                match results.try_recv() {
                    Ok(result) => batch.push(result),
                    Err(_) => break,
                }
            }
        }
        batch.sort_by_key(WorkerResult::sort_key);

        let mut state = self.state();
        for result in batch {
            apply_result(&mut state, result, &mut report);
        }
        sweep_read_timeouts(&self.paths, &self.config, &mut state, &mut report);
        report
    }

    fn state(&self) -> MutexGuard<'_, StoreState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl ChunkStore for SaveStore {
    fn lookup(&self, body: BodyKey, key: ChunkKey) -> StoreLookup {
        SaveStore::lookup(self, body, key)
    }
}

// ───────────────────────────── 每 tick 系统 ─────────────────────────────

/// `SaveSet::Flush`：推进 tick、按预算把脏块交给后台线程。
fn flush_system(store: Res<SaveStore>) {
    store.on_tick();
}

/// `SaveSet::Poll`：回收结果、处理超时、发消息。
fn poll_system(
    store: Res<SaveStore>,
    mut failed: MessageWriter<SaveFailed>,
    mut succeeded: MessageWriter<SaveSucceeded>,
) {
    let report = store.poll();
    for failure in report.failures {
        failed.write(failure);
    }
    for success in report.successes {
        succeeded.write(success);
    }
}

// ───────────────────────────── 内存状态机 ─────────────────────────────

/// 在途读的登记项。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReadTicket {
    /// 发起时的单调序号；与结果里的 epoch 不等就是迟到结果。
    epoch: u64,
    /// 发起时的 tick，用于超时。
    issued_tick: u64,
}

impl StoreState {
    /// 取一个全新的版本号（全局递增，写/删共用一条序列）。
    fn next_version(&mut self) -> u64 {
        self.next_version = self.next_version.saturating_add(1);
        self.next_version
    }
}

#[derive(Debug, Default)]
struct StoreState {
    tick: u64,
    next_epoch: u64,
    /// 全局递增的版本发号器：写/删都取一个新号，永不复用。
    next_version: u64,
    /// 每个块**当前意图**的版本（同键覆盖时换成新号，落盘成功后移除）。
    versions: BTreeMap<ChunkId, u64>,
    /// 每个块**已经确认落盘**的最高版本；只增不减。写入成功、删除成功（含
    /// “本来就没有”的 no-op）都记一笔，供调用方判断某次写是否已经落地。
    committed: BTreeMap<ChunkId, u64>,
    /// 内存缓冲：最新内容，写盘确认前一直在。
    buffer: BTreeMap<ChunkId, ChunkRecord>,
    /// 墓碑：该块不应再有记录，等 Flush 把删除落到磁盘（与 buffer 互斥）。
    tombstones: BTreeSet<ChunkId>,
    /// 待交给后台的块（写或删）。
    dirty: BTreeSet<ChunkId>,
    /// 已交给后台的写：块 → 发送时的版本。
    in_flight_writes: BTreeMap<ChunkId, u64>,
    /// 已发出的读：块 → 登记项。
    in_flight_reads: BTreeMap<ChunkId, ReadTicket>,
    /// 本会话已知 Miss 的块（被 `write` 清除）。
    misses: BTreeSet<ChunkId>,
    /// 后台已回、还没被 lookup 取走的命中。
    ready: BTreeMap<ChunkId, ChunkRecord>,
}

fn flush_due(config: &SaveConfig, state: &StoreState) -> bool {
    let interval = config.flush_interval_ticks;
    interval <= 1 || state.tick % interval == 0
}

/// 应用一条后台结果（⇔ `Poll` 的单条动作）。
fn apply_result(state: &mut StoreState, result: WorkerResult, report: &mut PollReport) {
    match result {
        WorkerResult::Read(result) => apply_read(state, result, report),
        WorkerResult::Write(result) => apply_write(state, result, report),
    }
}

/// 读结果：epoch 不匹配（超时后迟到 / 被写覆盖）直接丢。
fn apply_read(state: &mut StoreState, result: ReadResult, report: &mut PollReport) {
    let id = (result.body, result.key);
    match state.in_flight_reads.get(&id) {
        Some(ticket) if ticket.epoch == result.epoch => {
            state.in_flight_reads.remove(&id);
        }
        _ => {
            report.dropped += 1;
            return;
        }
    }
    match result.outcome {
        ReadOutcome::Hit(record) => {
            state.ready.insert(id, record);
        }
        ReadOutcome::Miss => {
            state.misses.insert(id);
        }
        ReadOutcome::Failed { path, reason } => {
            // 读失败 = 这一块按 Miss 处理，但原因要带到调用方（§5.3 / §7）。
            state.misses.insert(id);
            report.failures.push(SaveFailed {
                path,
                reason,
                chunk: Some(id),
            });
        }
    }
    report.applied.push(id);
}

/// 写/删除结果：成功才丢缓冲或墓碑；失败保留最新意图并重新标脏，下次 Flush 重试。
///
/// 删除复用 `WriteResult`：`Ok` 表示磁盘已不需要这条记录（含“本来就没有”的
/// no-op），`Failed` 表示没读成或没删成，墓碑必须留着。
fn apply_write(state: &mut StoreState, result: WriteResult, report: &mut PollReport) {
    let id = (result.body, result.key);
    if state.in_flight_writes.get(&id) == Some(&result.version) {
        state.in_flight_writes.remove(&id);
    }
    match result.outcome {
        WriteOutcome::Ok { path } => {
            // 落盘确认只增不减：即使这条结果被更新的意图压过，它代表的内容
            // 也确实在磁盘上了（版本号全局单调，后写的号更大）。
            state
                .committed
                .entry(id)
                .and_modify(|done| *done = (*done).max(result.version))
                .or_insert(result.version);
            // 期间没有更新的写/删 → 目标状态已落盘，可以丢；否则保留最新意图。
            if state.versions.get(&id).copied() == Some(result.version) {
                state.buffer.remove(&id);
                state.tombstones.remove(&id);
                state.versions.remove(&id);
                state.dirty.remove(&id);
            }
            report.successes.push(SaveSucceeded {
                path,
                chunk: Some(id),
            });
        }
        WriteOutcome::Failed { path, reason } => {
            state.dirty.insert(id);
            report.failures.push(SaveFailed {
                path,
                reason,
                chunk: Some(id),
            });
        }
    }
    report.applied.push(id);
}

/// 读超时：转 `Miss` + `SaveFailed`，并清掉在途登记（迟到结果会被 epoch 丢掉）。
fn sweep_read_timeouts(
    paths: &SavePaths,
    config: &SaveConfig,
    state: &mut StoreState,
    report: &mut PollReport,
) {
    let timeout = config.read_timeout_ticks;
    let now = state.tick;
    let expired: Vec<ChunkId> = state
        .in_flight_reads
        .iter()
        .filter(|(_, ticket)| now.saturating_sub(ticket.issued_tick) >= timeout)
        .map(|(id, _)| *id)
        .collect();
    for id in expired {
        state.in_flight_reads.remove(&id);
        state.misses.insert(id);
        report.timed_out += 1;
        report.applied.push(id);
        report.failures.push(SaveFailed {
            path: paths.region(id.0, id.1),
            reason: format!("read timeout after {timeout} tick(s)"),
            chunk: Some(id),
        });
    }
}

// ───────────────────────────── 后台线程 ─────────────────────────────

/// 后台线程句柄：停止通道 + join。
struct WorkerThread {
    jobs: Sender<WorkerJob>,
    stop: Sender<()>,
    handle: Mutex<Option<JoinHandle<()>>>,
    alive: Arc<AtomicBool>,
}

impl WorkerThread {
    fn spawn(root: PathBuf) -> (Self, Receiver<WorkerResult>) {
        let (jobs, job_rx) = unbounded::<WorkerJob>();
        let (stop, stop_rx) = unbounded::<()>();
        let (result_tx, result_rx) = unbounded::<WorkerResult>();
        let alive = Arc::new(AtomicBool::new(true));
        let thread_alive = Arc::clone(&alive);
        let handle = std::thread::Builder::new()
            .name("game-engine-save-io".to_string())
            .spawn(move || worker_loop(root, job_rx, stop_rx, result_tx, thread_alive))
            .expect("启动存档后台 IO 线程失败");
        (
            Self {
                jobs,
                stop,
                handle: Mutex::new(Some(handle)),
                alive,
            },
            result_rx,
        )
    }
}

impl Drop for WorkerThread {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        let handle = self
            .handle
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(handle) = handle {
            // 线程正常退出（或 panic）后 join；不检测 panic：存档层不该把别的线程的 panic 再抛一次。
            let _ = handle.join();
        }
    }
}

/// 后台线程主体：一条线程顺序处理读、写、删、stop。
fn worker_loop(
    root: PathBuf,
    jobs: Receiver<WorkerJob>,
    stop: Receiver<()>,
    results: Sender<WorkerResult>,
    alive: Arc<AtomicBool>,
) {
    struct ExitGuard(Arc<AtomicBool>);
    impl Drop for ExitGuard {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }
    // 无论是 break 还是 panic 展开，退出前都把存活标志置 false（Drop 里 join 后可见）。
    let _guard = ExitGuard(alive);

    let paths = SavePaths::new(root);
    loop {
        let should_stop = crossbeam_channel::select! {
            recv(stop) -> _ => true,
            recv(jobs) -> message => match message {
                Ok(WorkerJob::Read(request)) => {
                    !deliver(&results, WorkerResult::Read(run_read(&paths, request)))
                }
                Ok(WorkerJob::Write(request)) => {
                    !deliver(&results, WorkerResult::Write(run_write(&paths, request)))
                }
                Ok(WorkerJob::Delete(request)) => {
                    !deliver(&results, WorkerResult::Write(run_delete(&paths, request)))
                }
                // 主线程 drop 了发送端：退出。
                Err(_) => true,
            },
        };
        if should_stop {
            break;
        }
    }
}

fn deliver(results: &Sender<WorkerResult>, result: WorkerResult) -> bool {
    results.send(result).is_ok()
}

/// 后台读：整 region 读 + 解码，只取本块（§7 的 `lookup` 数据源）。
fn run_read(paths: &SavePaths, request: ReadRequest) -> ReadResult {
    let path = paths.region(request.body, request.key);
    let outcome = match read_region(paths, request.body, request.key) {
        Ok(Some(mut region)) => match region.chunks.remove(&request.key) {
            Some(record) => ReadOutcome::Hit(record),
            // region 里没有这一块 = 空块（§1：空块不占字节）。
            None => ReadOutcome::Miss,
        },
        Ok(None) => ReadOutcome::Miss,
        Err(error) => ReadOutcome::Failed {
            path,
            reason: error.to_string(),
        },
    };
    ReadResult {
        body: request.body,
        key: request.key,
        epoch: request.epoch,
        outcome,
    }
}

/// 后台写：整 region 读改写 + 原子写（§9：v1 不做增量写）。
///
/// 旧档读不出来（主文件与 `.recovered` 都坏）时按空基线写：那些字节本来就读不出来，
/// 写目标由 §5.3 的粘性规则决定（`.corrupt` 已生成 → 写 `.recovered`，主文件不动）；
/// 读失败本身已由 `lookup` 那条路径报过 `SaveFailed`。
fn run_write(paths: &SavePaths, request: WriteRequest) -> WriteResult {
    let path = paths.region(request.body, request.key);
    let mut region = match read_region(paths, request.body, request.key) {
        Ok(Some(region)) => region,
        Ok(None) | Err(_) => RegionFile::default(),
    };
    region.chunks.insert(request.key, request.record);
    let outcome = match write_region(paths, request.body, request.key, &region) {
        Ok(path) => WriteOutcome::Ok { path },
        Err(error) => WriteOutcome::Failed {
            path,
            reason: error.to_string(),
        },
    };
    WriteResult {
        body: request.body,
        key: request.key,
        version: request.version,
        outcome,
    }
}

/// 后台删除：整 region 读改写；该块本来就不在文件里时**不写盘**（避免写放大）。
///
/// 读失败不能当成“块不在”：那会让墓碑假成功、旧记录在下次读时复活。失败原样
/// 带回，主线程保留墓碑与 dirty，下一次 Flush 重试。
///
/// 删除后 region 变空 → 删掉 region 文件（[remove_region] 连 `.recovered` 一起删）；
/// 否则把移除该块后的整 region 原子写回（§9：v1 不做增量写）。
fn run_delete(paths: &SavePaths, request: DeleteRequest) -> WriteResult {
    let path = paths.region(request.body, request.key);
    let mut region = match read_region(paths, request.body, request.key) {
        Ok(Some(region)) => region,
        // region 文件不存在 = 磁盘上本来就没这条记录，删除是 no-op。
        Ok(None) => {
            return WriteResult {
                body: request.body,
                key: request.key,
                version: request.version,
                outcome: WriteOutcome::Ok { path },
            };
        }
        Err(error) => {
            return WriteResult {
                body: request.body,
                key: request.key,
                version: request.version,
                outcome: WriteOutcome::Failed {
                    path,
                    reason: error.to_string(),
                },
            };
        }
    };
    if region.chunks.remove(&request.key).is_none() {
        return WriteResult {
            body: request.body,
            key: request.key,
            version: request.version,
            outcome: WriteOutcome::Ok { path },
        };
    }
    let outcome = if region.chunks.is_empty() {
        match remove_region(paths, request.body, request.key) {
            Ok(written) => WriteOutcome::Ok { path: written },
            Err(error) => WriteOutcome::Failed {
                path,
                reason: error.to_string(),
            },
        }
    } else {
        match write_region(paths, request.body, request.key, &region) {
            Ok(written) => WriteOutcome::Ok { path: written },
            Err(error) => WriteOutcome::Failed {
                path,
                reason: error.to_string(),
            },
        }
    };
    WriteResult {
        body: request.body,
        key: request.key,
        version: request.version,
        outcome,
    }
}

// ───────────────────────────── 消息类型（线程间） ─────────────────────────────

enum WorkerJob {
    Read(ReadRequest),
    Write(WriteRequest),
    Delete(DeleteRequest),
}

struct ReadRequest {
    body: BodyKey,
    key: ChunkKey,
    epoch: u64,
}

struct WriteRequest {
    body: BodyKey,
    key: ChunkKey,
    record: ChunkRecord,
    version: u64,
}

/// 删除请求：只带定位与版本；磁盘上的旧记录由后台读出来再删。
struct DeleteRequest {
    body: BodyKey,
    key: ChunkKey,
    version: u64,
}

enum WorkerResult {
    Read(ReadResult),
    /// 写与删除共用：都是“把这个块的目标状态落到磁盘”。
    Write(WriteResult),
}

impl WorkerResult {
    fn sort_key(&self) -> (BodyKey, ChunkKey) {
        match self {
            Self::Read(result) => (result.body, result.key),
            Self::Write(result) => (result.body, result.key),
        }
    }
}

struct ReadResult {
    body: BodyKey,
    key: ChunkKey,
    epoch: u64,
    outcome: ReadOutcome,
}

enum ReadOutcome {
    Hit(ChunkRecord),
    Miss,
    Failed { path: PathBuf, reason: String },
}

struct WriteResult {
    body: BodyKey,
    key: ChunkKey,
    version: u64,
    outcome: WriteOutcome,
}

enum WriteOutcome {
    Ok { path: PathBuf },
    Failed { path: PathBuf, reason: String },
}
#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::identity::StableEntityId;
    use crate::save::store::write_region as write_region_to_disk;
    use crate::save::{RegionFile, SavePaths};
    use crate::streaming::StoreLookup;

    use super::*;

    /// `std::env::temp_dir()` 下的唯一目录；`Drop` 时整棵删掉（不加 tempfile 依赖）。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            use std::sync::atomic::AtomicU64;
            use std::time::{SystemTime, UNIX_EPOCH};

            static NEXT: AtomicU64 = AtomicU64::new(0);
            let seq = NEXT.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |delta| delta.as_nanos());
            let path = std::env::temp_dir().join(format!(
                "game-engine-save-plugin-{}-{nanos}-{seq}-{label}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("创建测试临时目录");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn config(dir: &TempDir) -> SaveConfig {
        SaveConfig {
            root: dir.path().to_path_buf(),
            read_timeout_ticks: DEFAULT_READ_TIMEOUT_TICKS,
            write_budget_per_tick: 4,
            flush_interval_ticks: 1,
        }
    }

    fn body(id: u64) -> BodyKey {
        BodyKey::new(StableEntityId(id))
    }

    fn key(x: i32, y: i32, z: i32) -> ChunkKey {
        ChunkKey::new(x, y, z)
    }

    /// 带一块 content 的记录；`marker` 让不同版本可区分。
    fn record(marker: u8) -> ChunkRecord {
        ChunkRecord {
            generator_version: 3,
            content: Some(vec![marker; 4]),
            proposals: Vec::new(),
            entities: Vec::new(),
        }
    }

    /// 把一条后台结果直接喂给状态机（与 `poll` 走同一条路径）。
    fn apply(store: &SaveStore, result: WorkerResult) -> PollReport {
        let mut state = store.state();
        let mut report = PollReport::default();
        apply_result(&mut state, result, &mut report);
        report
    }

    /// 等后台线程回一条结果并应用（超时 10 s 只防死等，不做 sleep 轮询）。
    fn pump(store: &SaveStore) -> PollReport {
        let results = store.results.as_ref().expect("该 store 必须有结果通道");
        let result = results
            .recv_timeout(Duration::from_secs(10))
            .expect("后台线程必须在 10 s 内回结果");
        apply(store, result)
    }

    #[test]
    fn lookup_pending_then_hit_from_disk() {
        let dir = TempDir::new("lookup-hit");
        let paths = SavePaths::new(dir.path());
        let b = body(1);
        let k = key(0, 0, 0);
        let mut region = RegionFile::default();
        region.chunks.insert(k, record(7));
        write_region_to_disk(&paths, b, k, &region).expect("预写 region 文件");

        let store = SaveStore::new(config(&dir));

        // 第一次问：还没读，必须 Pending（lookup 不做 IO）。
        assert_eq!(store.lookup(b, k), StoreLookup::Pending);
        // 同 tick 再问：仍 Pending，不重复发请求。
        assert_eq!(store.lookup(b, k), StoreLookup::Pending);
        assert_eq!(store.stats().in_flight_reads, 1);

        let report = pump(&store);
        assert_eq!(report.applied, vec![(b, k)]);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(store.stats().in_flight_reads, 0);

        assert_eq!(store.lookup(b, k), StoreLookup::Hit(record(7)));
    }

    #[test]
    fn lookup_pending_then_miss_is_cached() {
        let dir = TempDir::new("lookup-miss");
        let store = SaveStore::new(config(&dir));
        let b = body(1);
        let k = key(0, 0, 0);

        assert_eq!(store.lookup(b, k), StoreLookup::Pending);
        let report = pump(&store);
        assert_eq!(report.applied, vec![(b, k)]);
        assert!(report.failures.is_empty(), "{:?}", report.failures);

        // Miss 在本会话内缓存，不再发 IO（§7）。
        assert_eq!(store.lookup(b, k), StoreLookup::Miss);
        assert_eq!(store.stats().known_misses, 1);
        assert_eq!(store.stats().in_flight_reads, 0);

        // 组合非法的记录不进缓冲。
        let empty = ChunkRecord {
            generator_version: 1,
            content: None,
            proposals: Vec::new(),
            entities: Vec::new(),
        };
        assert_eq!(store.write(b, k, empty), Err(RecordError::Empty));
        assert_eq!(store.stats().buffered, 0);

        // 写入同一块后 Miss 缓存失效，缓冲直接命中。
        store.write(b, k, record(3)).expect("写缓冲");
        assert_eq!(store.lookup(b, k), StoreLookup::Hit(record(3)));
    }

    #[test]
    fn buffer_wins_over_disk() {
        let dir = TempDir::new("buffer-first");
        let b = body(1);
        let k = key(0, 0, 0);
        let mut region = RegionFile::default();
        region.chunks.insert(k, record(1));
        write_region_to_disk(&SavePaths::new(dir.path()), b, k, &region).expect("预写旧档");

        let store = SaveStore::new(config(&dir));
        store.write(b, k, record(2)).expect("写缓冲");

        // 缓冲命中：直接 Hit，不发 IO（旧档还在磁盘上）。
        assert_eq!(store.lookup(b, k), StoreLookup::Hit(record(2)));
        let on_disk = read_region(&SavePaths::new(dir.path()), b, k)
            .expect("读磁盘")
            .expect("旧档存在");
        assert_eq!(on_disk.chunks[&k], record(1));

        // Flush 之后才落盘，落盘成功缓冲才丢。
        assert_eq!(store.flush(), 1);
        let report = pump(&store);
        assert_eq!(report.successes.len(), 1);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(store.stats().buffered, 0);

        // 现在从磁盘读回新内容。
        assert_eq!(store.lookup(b, k), StoreLookup::Pending);
        pump(&store);
        assert_eq!(store.lookup(b, k), StoreLookup::Hit(record(2)));
    }

    #[test]
    fn flush_respects_write_budget() {
        let dir = TempDir::new("budget");
        let store = SaveStore::new(SaveConfig {
            write_budget_per_tick: 2,
            ..config(&dir)
        });
        let b = body(1);
        for x in 0..3 {
            store
                .write(b, key(x, 0, 0), record(x as u8))
                .expect("写缓冲");
        }
        assert_eq!(store.stats().dirty, 3);

        assert_eq!(store.flush(), 2, "一 tick 只发 budget 个");
        assert_eq!(store.stats().dirty, 1);
        assert_eq!(store.flush(), 1);
        assert_eq!(store.stats().dirty, 0);
    }

    #[test]
    fn quiesce_flushes_dirty_records() {
        let dir = TempDir::new("quiesce-flush");
        let paths = SavePaths::new(dir.path());
        let b = body(1);
        let keys = [key(0, 0, 0), key(1, 0, 0), key(2, 0, 0)];
        let store = SaveStore::new(config(&dir));
        for (index, k) in keys.iter().enumerate() {
            store
                .write(b, *k, record(index as u8 + 1))
                .expect("写缓冲");
        }

        store.quiesce(Duration::from_secs(5)).expect("屏障应成功");

        assert_eq!(store.stats().dirty, 0);
        assert_eq!(store.stats().in_flight_writes, 0);
        let region = read_region(&paths, b, keys[0])
            .expect("读 region")
            .expect("屏障返回后 region 必须已落盘");
        for (index, k) in keys.iter().enumerate() {
            assert_eq!(
                region.chunks.get(k),
                Some(&record(index as u8 + 1)),
                "第 {index} 块必须原样落盘"
            );
            assert!(
                store.committed_version(b, *k).is_some(),
                "第 {index} 块必须有落盘确认"
            );
        }
    }

    #[test]
    fn quiesce_is_noop_when_idle() {
        let dir = TempDir::new("quiesce-idle");
        let store = SaveStore::new(config(&dir));

        let started = Instant::now();
        store.quiesce(Duration::from_secs(5)).expect("空闲屏障应成功");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "没有在途写时必须立即返回，不能等超时"
        );
    }

    #[test]
    fn quiesce_without_worker_is_an_error() {
        let dir = TempDir::new("quiesce-offline");
        let store = SaveStore::offline(config(&dir));

        assert_eq!(
            store.quiesce(Duration::from_millis(200)),
            Err(QuiesceError::NoWorker),
            "离线 store 没有后台线程，屏障没有意义"
        );
    }

    #[test]
    fn quiesce_drains_more_than_one_poll_batch() {
        let dir = TempDir::new("quiesce-poll-batch");
        let b = body(2);
        let total = POLL_APPLY_BUDGET + 5;
        let store = SaveStore::new(SaveConfig {
            write_budget_per_tick: 4,
            ..config(&dir)
        });

        // 每块落在不同 region，避免 69 次整文件读改写相互放大；同时验证
        // poll 一次只应用 64 条时，屏障会继续轮询直到最后一个结果。
        let mut versions = BTreeMap::new();
        for index in 0..total {
            let k = key(index as i32 * 4, 0, 0);
            let version = store
                .write(b, k, record((index % 251) as u8))
                .expect("写缓冲");
            versions.insert(k, version);
        }

        store
            .quiesce(Duration::from_secs(10))
            .expect("超过一批结果的屏障也必须成功");

        assert_eq!(store.stats().dirty, 0);
        assert_eq!(store.stats().in_flight_writes, 0);
        for (k, version) in &versions {
            assert!(
                store.is_committed(b, *k, *version),
                "块 {k:?} 的结果在第二批里，也必须被 quiesce 收完"
            );
        }
    }

    #[test]
    fn quiesce_detects_dead_worker() {
        let dir = TempDir::new("quiesce-dead");
        let b = body(3);
        let k = key(0, 0, 0);
        let mut store = SaveStore::new(config(&dir));
        store.shutdown_worker();
        store.write(b, k, record(1)).expect("写缓冲");

        let started = Instant::now();
        let error = store.quiesce(Duration::from_millis(200)).expect_err("必须失败");
        assert_eq!(error, QuiesceError::WorkerDead, "线程已退出且还有脏块");
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "线程已死时必须立刻判定，不能等满超时"
        );
    }

    #[test]
    fn quiesce_does_not_retry_failed_writes() {
        let dir = TempDir::new("quiesce-write-fail");
        let b = body(4);
        let k = key(0, 0, 0);
        let paths = SavePaths::new(dir.path());
        // 目标位置被目录占住：后台写必失败，apply_write 会把块重新标脏。
        fs::create_dir_all(paths.region(b, k)).expect("占位目录");
        let store = SaveStore::new(config(&dir));
        store.write(b, k, record(5)).expect("写缓冲");

        let started = Instant::now();
        store
            .quiesce(Duration::from_secs(2))
            .expect("写失败不应让屏障失败");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "等待循环不能重投失败的写，否则永久失败会拖到超时"
        );
        assert_eq!(store.stats().in_flight_writes, 0);
        assert_eq!(store.stats().dirty, 1, "失败写留到下一 tick 重试");
        assert_eq!(store.committed_version(b, k), None);
    }

    #[test]
    fn read_timeout_turns_miss_and_reports_failure() {
        let dir = TempDir::new("timeout");
        let store = SaveStore::offline(SaveConfig {
            read_timeout_ticks: 3,
            ..config(&dir)
        });
        let b = body(1);
        let k = key(0, 0, 0);

        assert_eq!(store.lookup(b, k), StoreLookup::Pending);
        for _ in 0..3 {
            store.on_tick();
        }
        let report = store.poll();

        assert_eq!(report.timed_out, 1);
        assert_eq!(report.applied, vec![(b, k)]);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].chunk, Some((b, k)));
        assert_eq!(
            report.failures[0].path,
            SavePaths::new(dir.path()).region(b, k),
            "超时要报 region 文件路径"
        );
        assert!(report.failures[0].reason.contains("timeout"));
        // 超时后按 Miss 处理，且在途登记已清掉。
        assert_eq!(store.lookup(b, k), StoreLookup::Miss);
        assert_eq!(store.stats().in_flight_reads, 0);
    }

    #[test]
    fn late_read_result_is_dropped_by_epoch() {
        let dir = TempDir::new("epoch");
        let (store, sender) = SaveStore::with_results_channel(SaveConfig {
            read_timeout_ticks: 1,
            ..config(&dir)
        });
        let b = body(1);
        let k = key(0, 0, 0);

        assert_eq!(store.lookup(b, k), StoreLookup::Pending);
        store.on_tick();
        assert_eq!(store.poll().timed_out, 1, "先超时，清掉在途登记");

        // 迟到的结果带着旧 epoch（0）：必须直接丢，不能把 Miss 翻回 Hit。
        sender
            .send(WorkerResult::Read(ReadResult {
                body: b,
                key: k,
                epoch: 0,
                outcome: ReadOutcome::Hit(record(9)),
            }))
            .expect("结果通道还开着");
        let report = store.poll();
        assert_eq!(report.dropped, 1);
        assert!(report.applied.is_empty());
        assert_eq!(store.lookup(b, k), StoreLookup::Miss);
        assert_eq!(store.stats().ready_hits, 0);
    }

    #[test]
    fn poll_applies_results_in_key_order() {
        let dir = TempDir::new("order");
        let (store, sender) = SaveStore::with_results_channel(config(&dir));
        let b = body(1);
        let ordered = [key(0, 0, 1), key(0, 0, 2), key(0, 0, 3)];

        // 请求顺序与键序相反。
        for k in [key(0, 0, 3), key(0, 0, 1), key(0, 0, 2)] {
            assert_eq!(store.lookup(b, k), StoreLookup::Pending);
        }
        // 结果到达顺序也打乱（epoch 与请求顺序对应）。
        for (epoch, k) in [(2u64, key(0, 0, 2)), (0, key(0, 0, 3)), (1, key(0, 0, 1))] {
            sender
                .send(WorkerResult::Read(ReadResult {
                    body: b,
                    key: k,
                    epoch,
                    outcome: ReadOutcome::Miss,
                }))
                .expect("结果通道还开着");
        }

        let report = store.poll();
        assert_eq!(
            report.applied,
            ordered.iter().map(|k| (b, *k)).collect::<Vec<_>>(),
            "必须按 (BodyKey, ChunkKey) 键序应用"
        );
        assert_eq!(report.dropped, 0);
    }

    #[test]
    fn poll_apply_budget_caps_one_tick() {
        let dir = TempDir::new("apply-budget");
        let (store, sender) = SaveStore::with_results_channel(config(&dir));
        let b = body(1);
        let total = POLL_APPLY_BUDGET + 3;

        for index in 0..total {
            assert_eq!(
                store.lookup(b, key(0, 0, index as i32)),
                StoreLookup::Pending
            );
        }
        for epoch in 0..total {
            sender
                .send(WorkerResult::Read(ReadResult {
                    body: b,
                    key: key(0, 0, epoch as i32),
                    epoch: epoch as u64,
                    outcome: ReadOutcome::Miss,
                }))
                .expect("结果通道还开着");
        }

        let first = store.poll();
        assert_eq!(first.applied.len(), POLL_APPLY_BUDGET, "一 tick 应用有上限");
        let second = store.poll();
        assert_eq!(second.applied.len(), 3, "剩下的下个 tick 再应用");
        assert_eq!(store.stats().in_flight_reads, 0);
    }

    #[test]
    fn flush_then_restart_reads_back() {
        let dir = TempDir::new("restart");
        let b = body(2);
        let first_key = key(0, 0, 0);
        let second_key = key(1, 0, 0); // 同一 region：整文件读改写要能合并两块
        let config = SaveConfig {
            write_budget_per_tick: 4,
            ..config(&dir)
        };

        {
            let store = SaveStore::new(config.clone());
            store.write(b, first_key, record(1)).expect("写缓冲");
            store.write(b, second_key, record(2)).expect("写缓冲");
            assert_eq!(store.flush(), 2);

            let mut successes = 0;
            while successes < 2 {
                successes += pump(&store).successes.len();
            }
            assert_eq!(store.stats().buffered, 0, "写成功才丢缓冲");
            assert!(store.worker_running());
        } // Drop 里 stop + join

        // 重启：同一目录上新的 SaveStore 必须读回刚写的两块。
        let store = SaveStore::new(config);
        assert_eq!(store.lookup(b, first_key), StoreLookup::Pending);
        pump(&store);
        assert_eq!(store.lookup(b, first_key), StoreLookup::Hit(record(1)));

        assert_eq!(store.lookup(b, second_key), StoreLookup::Pending);
        pump(&store);
        assert_eq!(store.lookup(b, second_key), StoreLookup::Hit(record(2)));
    }

    #[test]
    fn delete_removes_buffered_record_before_flush() {
        let dir = TempDir::new("delete-buffer");
        let b = body(6);
        let k = key(0, 0, 0);
        let store = SaveStore::new(config(&dir));

        store.write(b, k, record(1)).expect("写缓冲");
        assert_eq!(store.lookup(b, k), StoreLookup::Hit(record(1)));

        store.delete(b, k).expect("标删除");
        assert_eq!(store.stats().buffered, 0, "delete 必须把记录移出缓冲");
        assert_eq!(store.stats().tombstones, 1);
        assert_eq!(store.stats().dirty, 1, "删除要等 Flush 才落盘");
        assert_eq!(
            store.lookup(b, k),
            StoreLookup::Miss,
            "删除落盘前也不许交回旧内容"
        );

        // 磁盘上本来就没有这块：这次删除是 no-op，不得产生文件。
        assert_eq!(store.flush(), 1);
        let report = pump(&store);
        assert_eq!(report.successes.len(), 1);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(store.stats().tombstones, 0, "成功后才清墓碑");
        assert_eq!(store.lookup(b, k), StoreLookup::Miss);
        assert!(
            !SavePaths::new(dir.path()).region(b, k).exists(),
            "no-op 删除不得创建 region 文件"
        );
    }

    #[test]
    fn delete_missing_region_is_noop() {
        let dir = TempDir::new("delete-noop");
        let b = body(7);
        let k = key(1, 2, 3);
        let paths = SavePaths::new(dir.path());
        let store = SaveStore::new(config(&dir));

        store.delete(b, k).expect("标删除");
        assert_eq!(store.lookup(b, k), StoreLookup::Miss);
        assert_eq!(store.flush(), 1);
        let report = pump(&store);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(store.stats().tombstones, 0);
        assert_eq!(read_region(&paths, b, k).expect("读 region"), None);
        assert!(
            !paths.body_dir(b).exists(),
            "no-op 删除不应创建 body 目录"
        );
    }

    #[test]
    fn delete_after_flush_removes_from_disk() {
        let dir = TempDir::new("delete-disk");
        let b = body(8);
        let k = key(0, 0, 0);
        let paths = SavePaths::new(dir.path());
        let config = config(&dir);

        {
            let store = SaveStore::new(config.clone());
            store.write(b, k, record(4)).expect("写缓冲");
            assert_eq!(store.flush(), 1);
            let report = pump(&store);
            assert!(report.failures.is_empty(), "{:?}", report.failures);
            assert!(paths.region(b, k).is_file(), "旧记录必须先在磁盘上");
        }

        {
            let store = SaveStore::new(config.clone());
            store.delete(b, k).expect("标删除");
            assert_eq!(store.lookup(b, k), StoreLookup::Miss);
            assert_eq!(store.flush(), 1);
            let report = pump(&store);
            assert!(report.failures.is_empty(), "{:?}", report.failures);
            assert_eq!(store.stats().tombstones, 0);
            assert!(!paths.region(b, k).exists(), "region 变空必须删文件");
        }

        // 重启 store：新的会话不能再从磁盘读回旧记录。
        let store = SaveStore::new(config);
        assert_eq!(store.lookup(b, k), StoreLookup::Pending, "重启后要真的读盘");
        let report = pump(&store);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(store.lookup(b, k), StoreLookup::Miss, "旧记录不得复活");
    }

    #[test]
    fn tombstone_survives_failed_region_read() {
        let dir = TempDir::new("delete-read-fail");
        let b = body(9);
        let k = key(0, 0, 0);
        let paths = SavePaths::new(dir.path());
        let store = SaveStore::new(config(&dir));

        store.write(b, k, record(6)).expect("写缓冲");
        assert_eq!(store.flush(), 1);
        let report = pump(&store);
        assert!(report.failures.is_empty(), "{:?}", report.failures);

        // 弄坏主文件：下一轮删除读不出 region，必须失败而不是假成功。
        let path = paths.region(b, k);
        let mut bytes = fs::read(&path).expect("读主文件");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        fs::write(&path, &bytes).expect("写坏主文件");

        store.delete(b, k).expect("标删除");
        assert_eq!(store.lookup(b, k), StoreLookup::Miss);
        assert_eq!(store.flush(), 1);
        let report = pump(&store);
        assert_eq!(report.failures.len(), 1, "读失败必须报 SaveFailed");
        assert_eq!(report.failures[0].chunk, Some((b, k)));
        assert_eq!(report.failures[0].path, path);
        assert_eq!(store.stats().tombstones, 1, "失败后墓碑必须留着");
        assert_eq!(store.stats().dirty, 1, "失败后重新标脏等重试");
        assert_eq!(
            store.lookup(b, k),
            StoreLookup::Miss,
            "读失败期间也不许交回旧内容"
        );
        assert!(path.is_file(), "删除没成功，文件不能被删");

        // 手动清掉坏文件后重试：磁盘上已经没有这块，no-op 成功、清墓碑。
        fs::remove_file(&path).expect("删掉坏文件");
        assert_eq!(store.flush(), 1);
        let report = pump(&store);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(store.stats().tombstones, 0, "重试成功后才清墓碑");
        assert!(!path.exists());
    }

    #[test]
    fn delete_keeps_other_chunks_in_region() {
        let dir = TempDir::new("delete-keep");
        let b = body(10);
        let keep = key(0, 0, 0);
        let drop = key(1, 0, 0); // 同一 region：删除走整文件读改写
        let paths = SavePaths::new(dir.path());
        let config = config(&dir);

        {
            let store = SaveStore::new(config.clone());
            store.write(b, keep, record(1)).expect("写 keep");
            store.write(b, drop, record(2)).expect("写 drop");
            assert_eq!(store.flush(), 2);
            let mut successes = 0;
            while successes < 2 {
                successes += pump(&store).successes.len();
            }
            assert!(paths.region(b, keep).is_file());
        }

        {
            let store = SaveStore::new(config.clone());
            store.delete(b, drop).expect("标删除");
            assert_eq!(store.flush(), 1);
            let report = pump(&store);
            assert!(report.failures.is_empty(), "{:?}", report.failures);
            assert_eq!(store.stats().tombstones, 0);
        }

        // region 里还有别的块：文件必须保留，且只少了被删的那块。
        let region = read_region(&paths, b, keep)
            .expect("读 region")
            .expect("region 还在");
        assert_eq!(region.chunk_count(), 1);
        assert_eq!(region.chunks.get(&keep), Some(&record(1)));
        assert!(!region.chunks.contains_key(&drop));

        // 重启后 keep 仍在、drop 已 Miss。
        let store = SaveStore::new(config);
        assert_eq!(store.lookup(b, keep), StoreLookup::Pending);
        pump(&store);
        assert_eq!(store.lookup(b, keep), StoreLookup::Hit(record(1)));
        assert_eq!(store.lookup(b, drop), StoreLookup::Pending);
        pump(&store);
        assert_eq!(store.lookup(b, drop), StoreLookup::Miss);
    }

    #[test]
    fn write_failure_keeps_buffer_until_retry_succeeds() {
        let dir = TempDir::new("write-fail");
        let b = body(3);
        let k = key(0, 0, 0);
        let paths = SavePaths::new(dir.path());
        // 目标位置被目录占住：rename(tmp, final) 必失败（模拟写盘失败）。
        fs::create_dir_all(paths.region(b, k)).expect("占位目录");

        let store = SaveStore::new(config(&dir));
        store.write(b, k, record(5)).expect("写缓冲");
        assert_eq!(store.flush(), 1);

        let report = pump(&store);
        assert_eq!(report.failures.len(), 1, "写失败必须报 SaveFailed");
        assert_eq!(report.failures[0].chunk, Some((b, k)));
        assert_eq!(store.stats().buffered, 1, "写失败不能丢缓冲");
        assert_eq!(store.stats().dirty, 1, "写失败要重新标脏，等重试");
        assert_eq!(
            store.lookup(b, k),
            StoreLookup::Hit(record(5)),
            "写失败期间读仍拿到最新内容"
        );

        // 清掉障碍后重试成功，缓冲才丢。
        fs::remove_dir_all(paths.region(b, k)).expect("清掉障碍");
        assert_eq!(store.flush(), 1);
        let report = pump(&store);
        assert_eq!(report.successes.len(), 1);
        assert_eq!(store.stats().buffered, 0);
    }

    #[test]
    fn worker_thread_exits_when_store_is_dropped() {
        let dir = TempDir::new("drop");
        let store = SaveStore::new(config(&dir));
        let alive = Arc::clone(&store.worker.as_ref().expect("默认就有后台线程").alive);
        assert!(store.worker_running());
        assert!(alive.load(Ordering::Acquire));

        drop(store); // Drop → stop 通道 → join

        // join 已经返回：线程一定在退出前把标志置 false（无 sleep 等待）。
        assert!(!alive.load(Ordering::Acquire), "Drop 后线程必须已退出");
    }

    #[test]
    fn shutdown_worker_stops_reads_and_they_time_out() {
        let dir = TempDir::new("shutdown");
        let b = body(4);
        let k = key(0, 0, 0);
        let mut store = SaveStore::new(SaveConfig {
            read_timeout_ticks: 2,
            ..config(&dir)
        });
        let alive = Arc::clone(&store.worker.as_ref().expect("默认就有后台线程").alive);

        store.shutdown_worker();
        assert!(!alive.load(Ordering::Acquire), "shutdown_worker 必须 join");
        assert!(!store.worker_running());

        // 没有后台线程：读请求没人取 → 到点转 Miss。
        assert_eq!(store.lookup(b, k), StoreLookup::Pending);
        store.on_tick();
        store.on_tick();
        let report = store.poll();
        assert_eq!(report.timed_out, 1);
        assert_eq!(store.lookup(b, k), StoreLookup::Miss);
    }

    #[test]
    fn plugin_registers_resources_and_poll_writes_messages() {
        let dir = TempDir::new("plugin");
        let b = body(5);
        let k = key(0, 0, 0);
        // 0 = 立即超时：Poll 在同一 tick 就能把在途读转成 Miss。
        let config = SaveConfig {
            read_timeout_ticks: 0,
            ..config(&dir)
        };

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(SavePlugin {
            config: config.clone(),
        });

        assert_eq!(app.world().resource::<SaveConfig>(), &config);
        assert!(app.world().contains_resource::<SaveStore>());
        assert!(app.world().contains_resource::<Messages<SaveFailed>>());
        assert!(app.world().contains_resource::<Messages<SaveSucceeded>>());

        // 换成离线 store：读一定走超时，保证本测试与后台线程速度无关。
        app.insert_resource(SaveStore::offline(config));
        {
            let store = app.world().resource::<SaveStore>();
            assert_eq!(store.lookup(b, k), StoreLookup::Pending);
        }

        app.world_mut().run_schedule(FixedUpdate);

        let failed = app.world().resource::<Messages<SaveFailed>>();
        assert_eq!(failed.len(), 1, "Poll 必须把超时发成 SaveFailed");
        let succeeded = app.world().resource::<Messages<SaveSucceeded>>();
        assert_eq!(succeeded.len(), 0);
    }

    #[test]
    fn version_numbers_are_globally_monotonic() {
        let dir = TempDir::new("version-monotonic");
        let store = SaveStore::new(config(&dir));
        let b = body(9);
        let first = store.write(b, key(0, 0, 0), record(1)).expect("写缓冲");
        let second = store.write(b, key(1, 0, 0), record(2)).expect("写缓冲");
        let third = store.write(b, key(0, 0, 0), record(3)).expect("写缓冲");
        assert!(second > first, "不同块也必须共用一条递增序列：{first} -> {second}");
        assert!(third > second, "同块覆盖要换新号：{second} -> {third}");
        // 删除也取同一条序列的号。
        let deleted = store.delete(b, key(1, 0, 0)).expect("标删除");
        assert!(deleted > third, "删除也要取新号：{third} -> {deleted}");
    }

    #[test]
    fn committed_version_is_only_recorded_after_flush() {
        let dir = TempDir::new("committed-write");
        let b = body(10);
        let k = key(0, 0, 0);
        let store = SaveStore::new(config(&dir));
        let version = store.write(b, k, record(1)).expect("写缓冲");
        assert_eq!(store.committed_version(b, k), None, "进缓冲不算落盘");
        assert!(!store.is_committed(b, k, version));

        assert_eq!(store.flush(), 1);
        let report = pump(&store);
        assert_eq!(report.successes.len(), 1);
        assert_eq!(store.committed_version(b, k), Some(version));
        assert!(store.is_committed(b, k, version));
    }

    #[test]
    fn committed_version_is_recorded_after_delete() {
        let dir = TempDir::new("committed-delete");
        let b = body(11);
        let k = key(0, 0, 0);
        let store = SaveStore::new(config(&dir));
        store.write(b, k, record(1)).expect("写缓冲");
        let written = store.flush();
        assert_eq!(written, 1);
        pump(&store);
        let before_delete = store.committed_version(b, k).expect("写成功必须确认");

        let version = store.delete(b, k).expect("标删除");
        assert_eq!(store.committed_version(b, k), Some(before_delete), "删除没落盘前不确认");
        assert!(!store.is_committed(b, k, version), "删除没落盘前不能算确认");
        store.flush();
        pump(&store);
        assert!(store.is_committed(b, k, version), "删除成功也要记版本");
    }

    #[test]
    fn a_newer_write_invalidates_committed_check() {
        let dir = TempDir::new("committed-newer");
        let b = body(12);
        let k = key(0, 0, 0);
        let store = SaveStore::new(config(&dir));
        let first = store.write(b, k, record(1)).expect("写缓冲");
        store.flush();
        pump(&store);
        assert!(store.is_committed(b, k, first));

        // 第一次已落盘，但第二次是更新的内容：在它自己落盘前不能算安全。
        let second = store.write(b, k, record(2)).expect("写缓冲");
        assert!(store.is_committed(b, k, first), "旧版本已落地，事实不变");
        assert!(!store.is_committed(b, k, second));
        store.flush();
        pump(&store);
        assert!(store.is_committed(b, k, second));
    }

    #[test]
    fn failed_write_never_gets_committed() {
        let dir = TempDir::new("committed-fail");
        let b = body(13);
        let k = key(0, 0, 0);
        let paths = SavePaths::new(dir.path());
        // 目标路径被目录占住 → rename 必失败。
        fs::create_dir_all(paths.region(b, k)).expect("占位目录");
        let store = SaveStore::new(config(&dir));
        let version = store.write(b, k, record(1)).expect("写缓冲");
        store.flush();
        let report = pump(&store);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(store.committed_version(b, k), None, "写失败绝不能确认");
        assert!(!store.is_committed(b, k, version));

        // 障碍清掉后重试成功，同一版本号才算落地。
        fs::remove_dir_all(paths.region(b, k)).expect("清掉障碍");
        store.flush();
        pump(&store);
        assert!(store.is_committed(b, k, version));
    }

    #[test]
    fn has_worker_reports_background_io() {
        let dir = TempDir::new("has-worker");
        let online = SaveStore::new(config(&dir));
        assert!(online.has_worker());
        let offline = SaveStore::offline(config(&dir));
        assert!(!offline.has_worker(), "离线 store 永远不会回报落盘确认");
    }
}
