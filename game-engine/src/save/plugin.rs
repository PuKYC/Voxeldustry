//! 存档接线：后台 IO 线程 + `ChunkStore` 实现（《存档格式规格》§5.1、§7）。
//!
//! 本模块把 [`super::store`] 的纯文件 IO 接入 Bevy：
//!
//! - [`SaveStore`] 是资源，实现 [`crate::streaming::ChunkStore`]；`lookup` **只查表、
//!   不阻塞、不做 IO**，返回 `Hit` / `Miss` / `Pending`（§7）；
//! - 真正的磁盘读写在**一条** `std::thread` 里（不是 `IoTaskPool`），请求/结果都走
//!   `crossbeam_channel`；`stop` 通道 + `Drop` 里 join，退出时不留 detached 线程；
//! - [`SaveSet::Flush`] 推进 tick、按预算把内存缓冲交给后台；[`SaveSet::Poll`] 按
//!   `(BodyKey, ChunkKey)` 键序应用结果、处理读超时、发 [`SaveFailed`] / [`SaveSucceeded`]。
//!
//! # 读优先级（§7）
//!
//! **内存缓冲 > `.recovered` > 主文件 > `Miss`**：
//!
//! - 缓冲命中直接 `Hit`，不发 IO（卸载后立刻重载能读到最新内容）；
//! - 没有缓冲、没有在途读 → 登记在途、把读请求塞进队列、返回 `Pending`；
//! - 后台结果回来 → 放进 `ready`，下一次 `lookup` 交付 `Hit`（一次性交付）；
//! - 本会话已知 `Miss` 的块直接 `Miss`，直到该块被 [`SaveStore::write`] 写过（§7）；
//! - 超过 `read_timeout_ticks` 还没结果 → `Poll` 里转 `Miss` + `SaveFailed`。
//!
//! # 写（§5.1）
//!
//! [`SaveStore::write`] 只进内存缓冲（同键覆盖、最新赢）；`Flush` 按
//! `write_budget_per_tick` 把脏块交给后台线程，后台做**整 region 读改写** + 原子写。
//! **写成功前缓冲不丢**：写失败保留缓冲并重新标脏，下一次 `Flush` 重试。
//!
//! # 语义边界
//!
//! - 本层不做实体快照、不写 `world.meta`、不做 `world.lock`、不做 `ChunkWillUnload` 的
//!   “卸载前必须落盘”钩子；
//! - 后台线程是唯一做 IO 的一方（它自己持有 [`SavePaths`]，所以 `.corrupt` / quarantine 的
//!   会话状态也只属于它）；主线程只用路径拼消息里的 `path`；
//! - 读失败的告警由调用方决定：本层把原因放进 [`SaveFailed`]。

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use bevy::prelude::*;
use crossbeam_channel::{unbounded, Receiver, Sender};

use ::voxel::ChunkKey;

use super::store::{read_region, write_region, SavePaths};
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
/// 默认每 tick 写预算（块数）。
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
}

// ───────────────────────────── SaveStore ─────────────────────────────

/// 存档资源：内存缓冲 + 在途状态 + 后台 IO 线程句柄。
///
/// 作为 `Resource` 插入；[`ChunkStore`] 的 `lookup` 可以给流式层直接同步调用。
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
        }
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
    pub fn write(
        &self,
        body: BodyKey,
        key: ChunkKey,
        record: ChunkRecord,
    ) -> Result<(), RecordError> {
        record.validate()?;
        let id = (body, key);
        let mut state = self.state();
        let version = state
            .versions
            .get(&id)
            .copied()
            .unwrap_or(0)
            .wrapping_add(1);
        state.versions.insert(id, version);
        state.buffer.insert(id, record);
        state.dirty.insert(id);
        // 最新写入压过在途读：旧磁盘内容不能再回来（迟到的结果靠 epoch 丢弃）。
        state.in_flight_reads.remove(&id);
        state.ready.remove(&id);
        state.misses.remove(&id);
        Ok(())
    }

    /// `ChunkStore::lookup` 的实现（§7）。
    ///
    /// 只查内存、不阻塞：缓冲 → 已回结果 → 已知 Miss → 在途 → 新请求。
    pub fn lookup(&self, body: BodyKey, key: ChunkKey) -> StoreLookup {
        let id = (body, key);
        let mut state = self.state();

        if let Some(record) = state.buffer.get(&id) {
            return StoreLookup::Hit(record.clone());
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

    /// 立刻按 `write_budget_per_tick` 把脏块交给后台线程（不受 Flush 周期限制）。
    ///
    /// 返回实际发出的块数。
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
            let Some(record) = state.buffer.get(&id).cloned() else {
                state.dirty.remove(&id);
                continue;
            };
            let version = state.versions.get(&id).copied().unwrap_or(0);
            let job = WorkerJob::Write(WriteRequest {
                body: id.0,
                key: id.1,
                record,
                version,
            });
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

#[derive(Debug, Default)]
struct StoreState {
    tick: u64,
    next_epoch: u64,
    /// 每个块的最新版本（同键覆盖时 +1）。
    versions: BTreeMap<ChunkId, u64>,
    /// 内存缓冲：最新内容，写盘确认前一直在。
    buffer: BTreeMap<ChunkId, ChunkRecord>,
    /// 待交给后台的块。
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

/// 写结果：成功才丢缓冲；失败保留最新内容并重新标脏，下一次 Flush 重试。
fn apply_write(state: &mut StoreState, result: WriteResult, report: &mut PollReport) {
    let id = (result.body, result.key);
    if state.in_flight_writes.get(&id) == Some(&result.version) {
        state.in_flight_writes.remove(&id);
    }
    match result.outcome {
        WriteOutcome::Ok { path } => {
            // 期间没有更新的写 → 缓冲已落盘，可以丢；否则保留最新内容。
            if state.versions.get(&id).copied() == Some(result.version) {
                state.buffer.remove(&id);
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

/// 后台线程主体：一条线程顺序处理读、写、stop。
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

// ───────────────────────────── 消息类型（线程间） ─────────────────────────────

enum WorkerJob {
    Read(ReadRequest),
    Write(WriteRequest),
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

enum WorkerResult {
    Read(ReadResult),
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
}
