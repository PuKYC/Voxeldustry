//! 生成状态表：内容态（`GenState`）与调度进度（`WorkState`）两条独立的轴。
//!
//! 对应《体素流式_实现文档》§4 / §6：内容态只允许单向推进
//! `Unloaded -> Undetermined -> Determined`，回到 `Unloaded` 必须走
//! `GenTable::cancel` + `GenTable::reset`；调度进度只描述“任务跑到哪一步”，
//! 与内容态互不影响，所以“生成中取消”不会被写成一次非法反向迁移
//! （把任务进度塞进 `GenState` 就写不出来）。
//!
//! 表键是 `(BodyKey, ChunkKey)`，容器是 `BTreeMap`：遍历顺序只由键决定，
//! 与插入顺序、哈希种子无关（L3）。本模块只用整数，没有浮点，也没有 `HashMap`。
//!
//! ## 内容态迁移
//!
//! | from / to | `Unloaded` | `Undetermined` | `Determined` |
//! | --- | --- | --- | --- |
//! | `Unloaded` | 无变化 `Ok(false)` | 合法 `Ok(true)` | 合法 `Ok(true)`（空区块快路径） |
//! | `Undetermined` | 拒绝 `Err` | 无变化 `Ok(false)` | 合法 `Ok(true)` |
//! | `Determined` | 拒绝 `Err` | 拒绝 `Err` | 无变化 `Ok(false)` |
//!
//! “单向推进”按**序**判定（`GenState::rank`），所以允许跳过 `Undetermined`
//! 直接到 `Determined`：§7 裁剪要求“岛外的空区块提案为空，直接标记为已确定”；
//! 分步裁决的调用方仍然可以逐级推进。反向迁移一律拒绝并保持原状，不是 panic。
//!
//! ## 调度迁移
//!
//! | 操作 | `Idle` | `Queued` | `Running` | `Cancelled` |
//! | --- | --- | --- | --- | --- |
//! | `enqueue` | -> `Queued` | 无变化 | 拒绝 | -> `Queued`（重试） |
//! | `begin` | 拒绝 | -> `Running` | 无变化 | 拒绝 |
//! | `finish` | 无变化 | 拒绝 | -> `Idle` | 拒绝 |
//! | `cancel` | 无变化 | -> `Cancelled` | -> `Cancelled` | 无变化 |
//! | `reset` | -> `Idle`（内容态回退） | 拒绝 | 拒绝 | -> `Idle`（内容态回退） |
//!
//! 同状态迁移一律是“无变化”而不是错误：重复入队 / 重复 `begin` / 重复 `finish` 不是非法操作。
//!
//! ## 迟到结果
//!
//! `Slot` 带一个取消计数 `epoch`（见 `GenTable::epoch`）：
//! 每次 `Queued` / `Running -> Cancelled` 加一。后台任务在入队 / 开始时记下 epoch，
//! 回写结果前用 `GenTable::is_stale` 判定——epoch 不一致或该槽已被取消，
//! 结果必须丢弃（§7：`Pending` 期间收到 `ChunkWillUnload` 要“取消读票，迟到结果丢弃”）。
//!
//! 本文件只做类型与状态机，不做系统、不注册 Plugin。

use std::collections::BTreeMap;

use bevy::prelude::Resource;

use crate::body_key::BodyKey;
use ::voxel::ChunkKey;

/// 内容态：区块内容走到哪一步。
///
/// - `Unloaded`：无数据（初始）；
/// - `Undetermined`：只有物体提案，没有体素；
/// - `Determined`：提案已裁决，体素已生成（整块内容定稿）。
///
/// 排序按推进顺序（`Unloaded < Undetermined < Determined`），见 `Self::rank`。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GenState {
    /// 未加载：没有任何数据。
    #[default]
    Unloaded,
    /// 未确定：有提案（类型、原点、范围、权重），体素还没生成。
    Undetermined,
    /// 已确定：裁决完成、体素已生成，内容定稿。
    Determined,
}

impl GenState {
    /// 推进序号（`Unloaded = 0`、`Undetermined = 1`、`Determined = 2`）。
    ///
    /// 序只用于判定“单向推进”，不参与生成判定。
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Unloaded => 0,
            Self::Undetermined => 1,
            Self::Determined => 2,
        }
    }
}

/// 调度进度：任务跑到哪一步，与内容态是两条独立的轴。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkState {
    /// 空闲：没有任务在排队或在跑。
    #[default]
    Idle,
    /// 已入队，等待预算。
    Queued,
    /// 正在执行（分步 realize 可能跨多个 tick）。
    Running,
    /// 已取消：在跑的任务结果作废，等待调用方决定回到 `Idle` 还是重新入队。
    Cancelled,
}

/// 状态机拒绝一次迁移的原因；拒绝时状态保持原样。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GenError {
    /// 内容态只允许单向推进；反向迁移必须走 `GenTable::cancel` + `GenTable::reset`。
    BackwardTransition {
        /// 迁移前的状态。
        from: GenState,
        /// 请求迁移到的状态（比 `from` 靠前）。
        to: GenState,
    },
    /// 调度进度不允许这次迁移（例如 `Running -> Queued`）。
    IllegalWorkTransition {
        /// 迁移前的状态。
        from: WorkState,
        /// 请求迁移到的状态。
        to: WorkState,
    },
    /// 还有排队 / 在跑的任务，必须先 `GenTable::cancel` 才能 `GenTable::reset`。
    PendingWork {
        /// 迁移前的调度状态（`Queued` 或 `Running`）。
        work: WorkState,
    },
}

/// 一个区块的状态槽。
///
/// 字段私有：所有迁移都必须经过 `GenTable` 的方法，避免绕过状态机直接改字段。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Slot {
    gen: GenState,
    work: WorkState,
    epoch: u64,
}

impl Slot {
    /// 内容态。
    #[must_use]
    pub const fn gen(self) -> GenState {
        self.gen
    }

    /// 调度进度。
    #[must_use]
    pub const fn work(self) -> WorkState {
        self.work
    }

    /// 取消计数（迟到结果判据的来源）。
    #[must_use]
    pub const fn epoch(self) -> u64 {
        self.epoch
    }
}

/// 生成状态表：`(BodyKey, ChunkKey) -> Slot`。
///
/// 用 `BTreeMap`：遍历顺序由键序决定，与插入顺序无关。
/// 未插入的键按 `Slot::default()`（`Unloaded` / `Idle` / epoch 0）读取。
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct GenTable {
    slots: BTreeMap<(BodyKey, ChunkKey), Slot>,
}

impl GenTable {
    /// 空表。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: BTreeMap::new(),
        }
    }

    /// 推进内容态。
    ///
    /// 合法条件：`to.rank() >= 当前.rank()`（单向推进，允许跳过 `Undetermined`
    /// 直接到 `Determined`，供 §7 的空区块快路径使用）。
    ///
    /// 返回 `Ok(true)` 表示状态变化，`Ok(false)` 表示已经是目标状态（幂等）；
    /// 反向迁移返回 `GenError::BackwardTransition` 且**不改变**状态。
    pub fn advance(&mut self, key: (BodyKey, ChunkKey), to: GenState) -> Result<bool, GenError> {
        let slot = self.slots.entry(key).or_default();
        let from = slot.gen;
        if to.rank() < from.rank() {
            return Err(GenError::BackwardTransition { from, to });
        }
        if to == from {
            return Ok(false);
        }
        slot.gen = to;
        Ok(true)
    }

    /// 入队：`Idle` / `Cancelled` -> `Queued`。
    ///
    /// `Cancelled` 之后重新入队是一次合法重试（epoch 已在 `Self::cancel` 时加一，
    /// 旧任务的迟到结果仍然会被 `Self::is_stale` 判为过期）。
    /// `Queued` 重复入队返回 `Ok(false)`；`Running` 入队返回
    /// `GenError::IllegalWorkTransition`。
    pub fn enqueue(&mut self, key: (BodyKey, ChunkKey)) -> Result<bool, GenError> {
        self.transition_work(
            key,
            &[WorkState::Idle, WorkState::Cancelled],
            WorkState::Queued,
        )
    }

    /// 开始执行：`Queued` -> `Running`。
    ///
    /// 其余来源一律 `GenError::IllegalWorkTransition`，`Running` 重复开始返回 `Ok(false)`。
    pub fn begin(&mut self, key: (BodyKey, ChunkKey)) -> Result<bool, GenError> {
        self.transition_work(key, &[WorkState::Queued], WorkState::Running)
    }

    /// 执行完成：`Running` -> `Idle`。
    ///
    /// `Queued` / `Cancelled` 一律 `GenError::IllegalWorkTransition`；
    /// 已经是 `Idle` 时返回 `Ok(false)`（同态迁移是幂等，不是错误）。
    pub fn finish(&mut self, key: (BodyKey, ChunkKey)) -> Result<bool, GenError> {
        self.transition_work(key, &[WorkState::Running], WorkState::Idle)
    }

    /// 取消：把 `Queued` / `Running` 转成 `Cancelled`，并把 epoch 加一。
    ///
    /// 返回是否发生变化：`Idle`（没有可取消的任务）与已 `Cancelled` 都返回 `false`，
    /// 且不递增 epoch（没有在途任务需要作废）。
    ///
    /// 本方法**不**改内容态：回到 `Idle` / `Unloaded` 由调用方决定，见 `Self::reset`。
    /// §8 的“分步 realize 中途卸载”必须先走这条，否则半成品会写进 `VoxVolume`。
    pub fn cancel(&mut self, key: (BodyKey, ChunkKey)) -> bool {
        let slot = self.slots.entry(key).or_default();
        match slot.work {
            WorkState::Queued | WorkState::Running => {
                slot.work = WorkState::Cancelled;
                slot.epoch = slot.epoch.wrapping_add(1);
                true
            }
            WorkState::Idle | WorkState::Cancelled => false,
        }
    }

    /// 取消后收敛：`Cancelled` / `Idle` -> `Idle`，并把内容态从 `Undetermined` 退回 `Unloaded`。
    ///
    /// 文档只规定“取消时允许 `Undetermined -> Unloaded`”，没有给函数名；本方法是这条
    /// 回退的**唯一入口**（`advance` 永远拒绝反向迁移）。
    ///
    /// - `Determined` **不**回退：体素已经定稿、可能已经存档，内容不能再当半成品丢掉；
    /// - `Queued` / `Running` 返回 `GenError::PendingWork`：必须先 `Self::cancel`；
    /// - `Idle` 且内容态没有可回退的步骤时返回 `Ok(false)`。
    pub fn reset(&mut self, key: (BodyKey, ChunkKey)) -> Result<bool, GenError> {
        let slot = self.slots.entry(key).or_default();
        match slot.work {
            WorkState::Queued | WorkState::Running => {
                return Err(GenError::PendingWork { work: slot.work });
            }
            WorkState::Idle | WorkState::Cancelled => {}
        }
        let mut changed = false;
        if slot.work == WorkState::Cancelled {
            slot.work = WorkState::Idle;
            changed = true;
        }
        if slot.gen == GenState::Undetermined {
            slot.gen = GenState::Unloaded;
            changed = true;
        }
        Ok(changed)
    }

    /// 调度轴迁移的共用实现：同状态是 `Ok(false)`，不在 `allowed_from` 里则拒绝。
    fn transition_work(
        &mut self,
        key: (BodyKey, ChunkKey),
        allowed_from: &[WorkState],
        to: WorkState,
    ) -> Result<bool, GenError> {
        let slot = self.slots.entry(key).or_default();
        let from = slot.work;
        if from == to {
            return Ok(false);
        }
        if !allowed_from.contains(&from) {
            return Err(GenError::IllegalWorkTransition { from, to });
        }
        slot.work = to;
        Ok(true)
    }

    /// 读一个槽；未插入的键返回 `None`。
    #[must_use]
    pub fn slot(&self, key: (BodyKey, ChunkKey)) -> Option<Slot> {
        self.slots.get(&key).copied()
    }

    /// 内容态；未插入的键按 `Unloaded` 读。
    #[must_use]
    pub fn gen_state(&self, key: (BodyKey, ChunkKey)) -> GenState {
        self.slots
            .get(&key)
            .map_or(GenState::Unloaded, |slot| slot.gen)
    }

    /// 调度进度；未插入的键按 `Idle` 读。
    #[must_use]
    pub fn work_state(&self, key: (BodyKey, ChunkKey)) -> WorkState {
        self.slots
            .get(&key)
            .map_or(WorkState::Idle, |slot| slot.work)
    }

    /// 当前取消计数；未插入的键返回 0。
    ///
    /// 后台任务在入队 / 开始时记下这个值；回写前用它做 `Self::is_stale` 判定。
    #[must_use]
    pub fn epoch(&self, key: (BodyKey, ChunkKey)) -> u64 {
        self.slots.get(&key).map_or(0, |slot| slot.epoch)
    }

    /// 迟到结果判据：`epoch` 与表中不一致、该槽已被取消、或键不存在，都返回 `true`。
    ///
    /// 返回 `true` 时调用方必须丢弃结果（不装树、不 spawn、不触发快照）。
    #[must_use]
    pub fn is_stale(&self, key: (BodyKey, ChunkKey), epoch: u64) -> bool {
        match self.slots.get(&key) {
            Some(slot) => slot.epoch != epoch || slot.work == WorkState::Cancelled,
            None => true,
        }
    }

    /// 表里的槽数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// 表是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// 按 `(BodyKey, ChunkKey)` 升序遍历：`(键, 槽)`。顺序只由键决定。
    pub fn iter(&self) -> impl Iterator<Item = (&(BodyKey, ChunkKey), Slot)> + '_ {
        self.slots.iter().map(|(key, slot)| (key, *slot))
    }

    /// 按 `(BodyKey, ChunkKey)` 升序遍历键。
    pub fn keys(&self) -> impl Iterator<Item = (BodyKey, ChunkKey)> + '_ {
        self.slots.keys().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::StableEntityId;

    /// 测试用的表键：body 稳定 ID + 子块坐标。
    fn chunk(body: u64, x: i32, y: i32, z: i32) -> (BodyKey, ChunkKey) {
        (BodyKey::new(StableEntityId(body)), ChunkKey::new(x, y, z))
    }

    const GEN_STATES: [GenState; 3] = [
        GenState::Unloaded,
        GenState::Undetermined,
        GenState::Determined,
    ];

    const WORK_STATES: [WorkState; 4] = [
        WorkState::Idle,
        WorkState::Queued,
        WorkState::Running,
        WorkState::Cancelled,
    ];

    /// 把一个新表驱动到指定的调度状态（只走合法路径）。
    fn table_in(key: (BodyKey, ChunkKey), work: WorkState) -> GenTable {
        let mut table = GenTable::new();
        match work {
            WorkState::Idle => {}
            WorkState::Queued => assert_eq!(table.enqueue(key), Ok(true)),
            WorkState::Running => {
                assert_eq!(table.enqueue(key), Ok(true));
                assert_eq!(table.begin(key), Ok(true));
            }
            WorkState::Cancelled => {
                assert_eq!(table.enqueue(key), Ok(true));
                assert_eq!(table.begin(key), Ok(true));
                assert!(table.cancel(key));
            }
        }
        assert_eq!(table.work_state(key), work);
        table
    }

    /// 内容态全迁移矩阵：向前合法、同态幂等、反向拒绝且保持原状。
    #[test]
    fn gen_transition_matrix_is_one_way() {
        let key = chunk(1, 0, 0, 0);
        for from in GEN_STATES {
            for to in GEN_STATES {
                let mut table = GenTable::new();
                if from != GenState::Unloaded {
                    assert_eq!(table.advance(key, from), Ok(true));
                }
                assert_eq!(table.gen_state(key), from);

                let result = table.advance(key, to);
                if to.rank() > from.rank() {
                    assert_eq!(result, Ok(true), "{from:?} -> {to:?} 应合法");
                    assert_eq!(table.gen_state(key), to);
                } else if to.rank() == from.rank() {
                    assert_eq!(result, Ok(false), "{from:?} -> {to:?} 应幂等");
                    assert_eq!(table.gen_state(key), from);
                } else {
                    assert_eq!(
                        result,
                        Err(GenError::BackwardTransition { from, to }),
                        "{from:?} -> {to:?} 必须拒绝"
                    );
                    assert_eq!(table.gen_state(key), from, "拒绝后必须保持原状");
                }
            }
        }
    }

    /// 反向迁移不能通过 `advance` 绕过；只能 `cancel` + `reset`。
    #[test]
    fn backward_advance_does_not_touch_state() {
        let key = chunk(2, 3, 0, -1);
        let mut table = GenTable::new();
        assert_eq!(table.advance(key, GenState::Determined), Ok(true));
        assert_eq!(
            table.advance(key, GenState::Undetermined),
            Err(GenError::BackwardTransition {
                from: GenState::Determined,
                to: GenState::Undetermined,
            })
        );
        assert_eq!(
            table.advance(key, GenState::Unloaded),
            Err(GenError::BackwardTransition {
                from: GenState::Determined,
                to: GenState::Unloaded,
            })
        );
        assert_eq!(table.gen_state(key), GenState::Determined);
    }

    /// 调度轴全迁移矩阵：每个来源状态 × 五个操作。
    #[test]
    fn work_transition_matrix_matches_documented_edges() {
        let key = chunk(3, 0, 0, 0);
        for from in WORK_STATES {
            // enqueue：Idle / Cancelled -> Queued；Queued 幂等；Running 拒绝。
            let mut table = table_in(key, from);
            let result = table.enqueue(key);
            match from {
                WorkState::Idle | WorkState::Cancelled => assert_eq!(result, Ok(true)),
                WorkState::Queued => assert_eq!(result, Ok(false)),
                WorkState::Running => assert_eq!(
                    result,
                    Err(GenError::IllegalWorkTransition {
                        from,
                        to: WorkState::Queued,
                    })
                ),
            }
            let expected = if from == WorkState::Running {
                WorkState::Running
            } else {
                WorkState::Queued
            };
            assert_eq!(table.work_state(key), expected);

            // begin：Queued -> Running；其余（Running 幂等）都拒绝。
            let mut table = table_in(key, from);
            let result = table.begin(key);
            match from {
                WorkState::Queued => assert_eq!(result, Ok(true)),
                WorkState::Running => assert_eq!(result, Ok(false)),
                WorkState::Idle | WorkState::Cancelled => assert_eq!(
                    result,
                    Err(GenError::IllegalWorkTransition {
                        from,
                        to: WorkState::Running,
                    })
                ),
            }
            let expected = if from == WorkState::Queued {
                WorkState::Running
            } else {
                from
            };
            assert_eq!(table.work_state(key), expected);

            // finish：Running -> Idle；Idle 幂等；Queued / Cancelled 拒绝。
            let mut table = table_in(key, from);
            let result = table.finish(key);
            match from {
                WorkState::Running => {
                    assert_eq!(result, Ok(true));
                    assert_eq!(table.work_state(key), WorkState::Idle);
                }
                WorkState::Idle => {
                    assert_eq!(result, Ok(false));
                    assert_eq!(table.work_state(key), WorkState::Idle);
                }
                WorkState::Queued | WorkState::Cancelled => {
                    assert_eq!(
                        result,
                        Err(GenError::IllegalWorkTransition {
                            from,
                            to: WorkState::Idle,
                        })
                    );
                    assert_eq!(table.work_state(key), from);
                }
            }

            // cancel：Queued / Running -> Cancelled 并递增 epoch；其余无变化。
            let mut table = table_in(key, from);
            let epoch_before = table.epoch(key);
            let changed = table.cancel(key);
            match from {
                WorkState::Queued | WorkState::Running => {
                    assert!(changed);
                    assert_eq!(table.work_state(key), WorkState::Cancelled);
                    assert_eq!(table.epoch(key), epoch_before.wrapping_add(1));
                }
                WorkState::Idle | WorkState::Cancelled => {
                    assert!(!changed);
                    assert_eq!(table.work_state(key), from);
                    assert_eq!(table.epoch(key), epoch_before);
                }
            }

            // reset：Idle / Cancelled -> Idle；Queued / Running 拒绝。
            let mut table = table_in(key, from);
            let result = table.reset(key);
            match from {
                WorkState::Queued | WorkState::Running => {
                    assert_eq!(result, Err(GenError::PendingWork { work: from }));
                    assert_eq!(table.work_state(key), from);
                }
                WorkState::Idle | WorkState::Cancelled => {
                    assert_eq!(result, Ok(from == WorkState::Cancelled));
                    assert_eq!(table.work_state(key), WorkState::Idle);
                }
            }
        }
    }

    /// 取消后迟到的结果必须能被判为过期并丢弃。
    #[test]
    fn cancelled_work_makes_late_result_stale() {
        let key = chunk(4, 1, 0, 1);
        let mut table = GenTable::new();
        assert_eq!(table.advance(key, GenState::Undetermined), Ok(true));
        assert_eq!(table.enqueue(key), Ok(true));
        let ticket = table.epoch(key);
        assert!(!table.is_stale(key, ticket), "入队后的票应该有效");

        assert_eq!(table.begin(key), Ok(true));
        assert!(table.cancel(key), "Running -> Cancelled");
        assert_ne!(table.epoch(key), ticket);
        assert!(table.is_stale(key, ticket), "取消后的迟到结果必须丢弃");

        // 取消后重试：新任务拿新 epoch，旧票仍然过期。
        assert_eq!(table.enqueue(key), Ok(true), "Cancelled 可以重新入队");
        let retry = table.epoch(key);
        assert_ne!(retry, ticket);
        assert!(!table.is_stale(key, retry));

        // 结果回来时槽已经被再次取消。
        assert!(table.cancel(key));
        assert!(table.is_stale(key, retry));

        // 键不存在时任何票都过期。
        assert!(table.is_stale(chunk(255, 0, 0, 0), 0));

        // 收敛：Cancelled -> Idle，内容态 Undetermined -> Unloaded。
        assert_eq!(table.reset(key), Ok(true));
        assert_eq!(table.work_state(key), WorkState::Idle);
        assert_eq!(table.gen_state(key), GenState::Unloaded);
        assert!(table.is_stale(key, retry), "旧票在 reset 后依旧过期");
    }

    /// 内容态回退只发生在 `Undetermined`，且必须 `reset`。
    #[test]
    fn reset_rolls_back_only_undetermined() {
        let key = chunk(5, 0, 0, 0);
        let mut table = GenTable::new();

        // 只有提案：取消后回退到未加载。
        assert_eq!(table.advance(key, GenState::Undetermined), Ok(true));
        assert_eq!(table.enqueue(key), Ok(true));
        assert_eq!(table.begin(key), Ok(true));
        assert_eq!(
            table.reset(key),
            Err(GenError::PendingWork {
                work: WorkState::Running,
            }),
            "有在跑的任务时不能直接回退"
        );
        assert_eq!(table.gen_state(key), GenState::Undetermined);
        assert!(table.cancel(key));
        assert_eq!(table.reset(key), Ok(true));
        assert_eq!(table.gen_state(key), GenState::Unloaded);
        assert_eq!(table.work_state(key), WorkState::Idle);

        // 已定稿：不因 reset 回退。
        assert_eq!(table.advance(key, GenState::Determined), Ok(true));
        assert_eq!(table.reset(key), Ok(false));
        assert_eq!(table.gen_state(key), GenState::Determined);

        assert_eq!(table.enqueue(key), Ok(true));
        assert!(table.cancel(key));
        assert_eq!(table.reset(key), Ok(true));
        assert_eq!(
            table.gen_state(key),
            GenState::Determined,
            "已确定的内容不能被当成半成品丢掉"
        );
    }

    /// 未插入的键按默认槽读取，不改变表。
    #[test]
    fn missing_key_reads_default_slot() {
        let key = chunk(6, 9, 9, 9);
        let table = GenTable::new();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert_eq!(table.gen_state(key), GenState::Unloaded);
        assert_eq!(table.work_state(key), WorkState::Idle);
        assert_eq!(table.epoch(key), 0);
        assert_eq!(table.slot(key), None);
    }

    /// 遍历顺序只由键序决定：乱序插入得到相同序列。
    #[test]
    fn iteration_order_is_key_order_regardless_of_insertion() {
        let keys = [
            chunk(1, 0, 0, 0),
            chunk(1, 0, 0, 1),
            chunk(1, 0, 1, 0),
            chunk(1, -1, 0, 0),
            chunk(2, 0, 0, 0),
            chunk(2, -1, -2, -3),
            chunk(10, 0, 0, 0),
            chunk(10, 5, -5, 5),
        ];

        let mut shuffled = GenTable::new();
        for index in [5, 0, 7, 2, 6, 1, 4, 3] {
            assert_eq!(shuffled.enqueue(keys[index]), Ok(true));
        }
        let mut ordered = GenTable::new();
        for key in keys {
            assert_eq!(ordered.enqueue(key), Ok(true));
        }

        let iterated: Vec<_> = shuffled.keys().collect();
        let mut sorted = iterated.clone();
        sorted.sort();
        assert_eq!(iterated, sorted, "BTreeMap 升序遍历");
        assert_eq!(iterated, ordered.keys().collect::<Vec<_>>());
        assert_eq!(shuffled, ordered, "表内容与插入顺序无关");
        assert_eq!(
            shuffled.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
            iterated
        );
        assert_eq!(shuffled.len(), keys.len());
        assert!(!shuffled.is_empty());
    }
}
