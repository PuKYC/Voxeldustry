//! 驻留（Residency）：统一回答「哪些体素块需要在内存里，以什么形式存在」。
//!
//! 本插件是体素流式（`StreamPlugin`）与世界级区块管理（`WorldChunkPlugin`）共同依赖的
//! 底层：两边都只通过本模块的**消息**和**资源**决定加载 / 卸载，不各自维护一套表。
//!
//! ## 用法
//!
//! - 需要真实模拟的实体挂 [`Real`] 标签：驻留为「锚点所在块 ∪ `WorkSites` ∪ `PathCorridor`」
//!   记 [`Residency::Data`] 请求；
//! - 玩家 / 相机挂 [`Observer`]：以锚点为中心、半径 `radius` 个体素块记 [`Residency::Render`] 请求；
//! - 其它系统通过 [`PinRequests`] 钉住区块，通过 [`UnsavedEditMark`] / [`UnsavedEditClear`]
//!   钉住「编辑过但还没存档」的区块；
//! - 表的变化只以 [`ResidencyChanged`] 消息对外发布，流式加载器据此推进生成 / 卸载。
//!
//! ## 硬约束（确定性 L3）
//!
//! - 表一律 `BTreeMap` / `BTreeSet`，**禁止** `HashMap` 参与任何决定结果的遍历；
//! - 所有来自 `Query` 的变化先收集成集合、按键排序，再应用到表上；
//! - [`ResidencyChanged`] 的发射顺序由 `(BodyKey, ChunkKey)` 键序决定；
//! - 块归属只用定点整数（[`chunk_of`]），**禁止 f32 参与**。
//!
//! ## 门控（[`AwaitingChunks`]）
//!
//! `Gate` 集合只读 [`ResidencySlot::reached`]，不读同 tick 的流式消息。流式在
//! `GameTickSet::Streaming`（排在 `GameTickSet::Residency` 之后）才写 `ChunkReached`，
//! 所以门控天然滞后一个 tick。所有推进 `Real` 实体状态的系统都必须过滤 [`RealActive`]。
//!
//! ## 位置来源（过渡）
//!
//! 仓库目前没有权威位姿组件（`Body` 只有 kind，`Island` 只有 drift），因此本模块临时定义
//! [`LogicPos`] 作为定点逻辑坐标来源。**将来 `BodyPose` 落地后替换**：`update_anchors`
//! 改为从 `BodyPose` 取坐标即可，本模块其余部分不依赖位置来源。
//!
//! 坐标约定：`LogicPos` 是**体（岛）局部**的定点体素坐标，单位是体素（`Vec3F` 的原始位），
//! 与 `ChunkKey` 的岛局部语义一致；浮岛的世界坐标换算由上游负责，本模块不做。

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use bevy::prelude::*;

use crate::body_key::BodyKey;
use crate::identity::StableEntityId;
use crate::math::Vec3F;
use crate::voxel::ChunkKey;

mod plugin;
#[cfg(test)]
mod tests;

pub use plugin::ResidencyPlugin;

// ───────────────────────────── 块归属（整数、无 f32） ─────────────────────────────

/// 一个体素的定点边长原始位：`round(0.45 * 2^24) = 7_549_747`。
///
/// `game-core/src/static_data/voxel.rs` 有同名数值的 `VOXEL_SIZE_BITS`，但本 crate
/// 不能依赖 `game-core`（分层：engine 在下层），所以这里保留一份常量。
pub const VOXEL_SIZE_RAW: i64 = 7_549_747;

/// 一个体素块每轴的体素数（`CHUNK_DEPTH` = 32）。
pub const CHUNK_VOXELS_PER_AXIS: i64 = 32;

/// 一个体素块每轴的定点原始位：`VOXEL_SIZE_RAW * 32 = 241_591_904`。
pub const CHUNK_EDGE_RAW: i64 = VOXEL_SIZE_RAW * CHUNK_VOXELS_PER_AXIS;

/// 定点原始位 → 块坐标（**向下取整**，负数也正确）。
///
/// 用 `i64::div_euclid` 而不是 `/`：`/` 向零截断，负坐标会算错一个块。
#[inline]
#[must_use]
pub fn chunk_coord(raw: i64) -> i32 {
    let coord = raw.div_euclid(CHUNK_EDGE_RAW);
    // 位置远超 i32 块范围属于数据错误；这里饱和而不是回绕，保证确定性。
    coord.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// 定点逻辑坐标 → 体素块坐标（整数 floor 除法，**全程不经过 f32**）。
#[inline]
#[must_use]
pub fn chunk_of(pos: Vec3F) -> ChunkKey {
    ChunkKey::new(
        chunk_coord(pos.x.to_bits()),
        chunk_coord(pos.y.to_bits()),
        chunk_coord(pos.z.to_bits()),
    )
}

/// `Observer` 半径为 `radius` 个块时覆盖的块（以锚点为中心的三维球，含中心）。
///
/// 半径按欧氏距离判定（整数平方比较），不用立方体：立方体的角会伸到 `radius * √3`
/// 个块外，对「显示半径」没有意义。
#[must_use]
pub fn observer_chunks(anchor: ChunkKey, radius: u8) -> Vec<ChunkKey> {
    let r = i32::from(radius);
    let limit = i64::from(r) * i64::from(r);
    let mut out = Vec::new();
    for dx in -r..=r {
        for dy in -r..=r {
            for dz in -r..=r {
                let ddx = i64::from(dx);
                let ddy = i64::from(dy);
                let ddz = i64::from(dz);
                if ddx * ddx + ddy * ddy + ddz * ddz <= limit {
                    out.push(ChunkKey::new(anchor.x + dx, anchor.y + dy, anchor.z + dz));
                }
            }
        }
    }
    out
}

// ───────────────────────────── 标签组件 ─────────────────────────────

/// 定点逻辑坐标（体 / 岛局部，单位：体素）。
///
/// **过渡组件**：仓库还没有权威位姿组件，先用它推算 [`ChunkAnchor`]。
/// 将来 `BodyPose` 落地后替换本组件，`update_anchors` 改读 `BodyPose`，
/// 其余代码不受影响。**禁止**用 `Transform`（f32）参与块归属。
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogicPos(pub Vec3F);

impl Default for LogicPos {
    fn default() -> Self {
        Self(Vec3F::ZERO)
    }
}

/// 标签：该实体必须被真实模拟，**永不进账本**。请求档位 [`Residency::Data`]。
///
/// 请求范围 = 锚点所在块 ∪ [`WorkSites`] ∪ [`PathCorridor`]。
///
/// 实体必须带 [`StableEntityId`]（`#[require]` 无法自动补：它没有 `Default`，
/// 且 0 是保留的无效 ID），生成方需用 `spawn_stable` 或显式插入。
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[require(ChunkAnchor)]
pub struct Real;

/// 标签：观察者（玩家相机）。请求档位 [`Residency::Render`]，范围是显示半径。
///
/// `radius` 以体素块为单位，覆盖以锚点为中心的三维球（见 [`observer_chunks`]）。
/// 同样需要 [`StableEntityId`]（原因见 [`Real`]）。
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
#[require(ChunkAnchor)]
pub struct Observer {
    /// 显示半径，单位：体素块。
    pub radius: u8,
}

impl Default for Observer {
    fn default() -> Self {
        Self { radius: 0 }
    }
}

/// 实体当前所在的体素块。由 `update_anchors` 从 [`LogicPos`] 推算，业务代码不写它。
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkAnchor {
    /// 实体所属的体（岛 / 船 / 结构）。[`BodyKey::INVALID`] 表示还没赋值。
    pub body: BodyKey,
    /// 体局部体素块坐标。
    pub chunk: ChunkKey,
}

impl Default for ChunkAnchor {
    fn default() -> Self {
        Self {
            body: BodyKey::INVALID,
            chunk: ChunkKey::ZERO,
        }
    }
}

impl ChunkAnchor {
    /// 构造。
    #[must_use]
    pub const fn new(body: BodyKey, chunk: ChunkKey) -> Self {
        Self { body, chunk }
    }

    /// 两个字段都可用时才为 `true`（[`BodyKey::INVALID`] 视为未锚定）。
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.body.is_valid()
    }
}

/// 可选：实体正在作业的区块（通常一两个）。AI 作业时由 AI 系统写入。
///
/// 每项自带 [`BodyKey`]：作业对象可能在别的体（岛 / 船）上。
#[derive(Component, Debug, Default, Clone, PartialEq, Eq)]
pub struct WorkSites(pub Vec<(BodyKey, ChunkKey)>);

/// 可选：寻路 AI 的前视走廊。寻路系统在提交新路径时写入，
/// 内容是路径接下来 N 个体素块（按路径先后排序），只保留还没走到的。
///
/// 没有它，寻路 AI 只请求锚点所在块，走到块边界就会被「目标块未加载」挡回来，
/// 表现为 AI 撞在无形的墙上。按「走完这段要多久」裁剪，AI 停下就清空。
#[derive(Component, Debug, Default, Clone, PartialEq, Eq)]
pub struct PathCorridor(pub Vec<(BodyKey, ChunkKey)>);

/// 门控标记：[`Real`] 实体所需区块还没全部达到 [`Residency::Data`]。
///
/// 本插件自动增删，业务代码只读不写。所有推进 [`Real`] 实体状态的系统都必须
/// 用 [`RealActive`] 过滤掉挂着它的实体（时间暂停）。
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AwaitingChunks;

/// 放行条件过滤器：[`Real`] 实体所需区块全部就绪（未挂 [`AwaitingChunks`]）。
pub type RealActive = Without<AwaitingChunks>;

// ───────────────────────────── 档位与请求来源 ─────────────────────────────

/// 驻留档位，顺序必须 `None < Data < Render`（`Ord` 被汇总逻辑直接使用）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Residency {
    /// 仅生成状态数据（提案，或存档里的内容）：内存里没有体素 / 实体 / 网格。
    #[default]
    None,
    /// 体素 + 实体，不建网格、不建碰撞。
    Data,
    /// `Data` 的全部 + 网格 + 碰撞。
    Render,
}

/// 请求来源。用稳定 ID 而不是 `Entity`：存档读回后 `Entity` 会变。
///
/// 变体顺序即排序顺序（`Observer < Agent < System < UnsavedEdit`），
/// 决定 [`ResidencyTable`] 里 `BTreeMap` 的遍历序，因此**只增不改**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestSource {
    /// 带 [`Observer`] 标签的实体（玩家 / 相机）。
    Observer(StableEntityId),
    /// 带 [`Real`] 标签的实体（AI、需要真实模拟的机器）。
    Agent(StableEntityId),
    /// 系统钉住。带命名空间，避免两个系统撞 ID。
    System {
        /// 命名空间（每个系统 / 模块固定一个）。
        namespace: u32,
        /// 命名空间内的 ID。
        id: u64,
    },
    /// 被编辑过且还没存档的区块（存档落地前不能卸载）。
    UnsavedEdit,
}

impl RequestSource {
    /// 变体排序秩，用于手写 `Ord`。
    const fn rank(self) -> u8 {
        match self {
            Self::Observer(_) => 0,
            Self::Agent(_) => 1,
            Self::System { .. } => 2,
            Self::UnsavedEdit => 3,
        }
    }
}

// `StableEntityId` 没有实现 `Ord`（不归本模块所有），所以手写比较：
// 先比变体秩，再比底层 u64。
impl PartialOrd for RequestSource {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RequestSource {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        match (*self, *other) {
            (Self::Observer(a), Self::Observer(b)) => a.0.cmp(&b.0),
            (Self::Agent(a), Self::Agent(b)) => a.0.cmp(&b.0),
            (
                Self::System {
                    namespace: na,
                    id: ia,
                },
                Self::System {
                    namespace: nb,
                    id: ib,
                },
            ) => (na, ia).cmp(&(nb, ib)),
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

// ───────────────────────────── 请求表 ─────────────────────────────

/// 一个 `(体, 块)` 的驻留槽。
///
/// 字段私有：外部只能读；改档位只能通过本插件的系统、[`PinRequests`]、[`UnsavedEdit*`]
/// 消息，从根上杜绝「谁都能改档位」。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ResidencySlot {
    requests: BTreeMap<RequestSource, Residency>,
    target: Residency,
    reached: Residency,
    last_requested_tick: u64,
}

impl ResidencySlot {
    /// 当前全部来源的请求（键序确定）。同一来源同一区块只有一条。
    #[must_use]
    pub fn requests(&self) -> &BTreeMap<RequestSource, Residency> {
        &self.requests
    }

    /// 汇总并经过防抖后的目标档位；[`ResidencyChanged`] 就是它的变化。
    #[must_use]
    pub fn target(&self) -> Residency {
        self.target
    }

    /// 流式汇报的实际到达档位（[`ChunkReached`] 更新）。
    #[must_use]
    pub fn reached(&self) -> Residency {
        self.reached
    }

    /// 防抖计时锚点：最后一次「当前目标档位仍被请求」的 tick。
    ///
    /// 降级成功后会被重置为当 tick，实现「每一档重新计时」。
    #[must_use]
    pub fn last_requested_tick(&self) -> u64 {
        self.last_requested_tick
    }
}

/// 驻留总表：所有 `(体, 块)` 的目标档位、请求与到达状态。
///
/// 唯一的写入方是 `GameTickSet::Residency` 里的系统与标签生命周期 observer；
/// 外部只读（[`ResidencyChanged`] 是唯一的变化出口）。
#[derive(Resource, Debug, Default)]
pub struct ResidencyTable {
    slots: BTreeMap<(BodyKey, ChunkKey), ResidencySlot>,
    /// 反查：来源 → 它请求的区块。来源消失时按它清理，不必再去查实体。
    by_source: BTreeMap<RequestSource, Vec<(BodyKey, ChunkKey)>>,
    /// 自上次 `Resolve` 以来请求集合被改动的槽位（键序即发射顺序）。
    dirty: BTreeSet<(BodyKey, ChunkKey)>,
    /// `Resolve` 判定出的档位变化，等 `Emit` 按序写出。
    pending_changes: Vec<ResidencyChanged>,
    /// 本插件自己的 tick 计数（`FixedUpdate` 每跑一次 +1，不依赖墙钟）。
    tick: u64,
}

impl ResidencyTable {
    /// 本插件 tick 计数（每跑一次 `FixedUpdate` +1）。
    #[must_use]
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// 某个区块的槽位（只读）。
    #[must_use]
    pub fn slot(&self, body: BodyKey, chunk: ChunkKey) -> Option<&ResidencySlot> {
        self.slots.get(&(body, chunk))
    }

    /// 全部槽位，按 `(BodyKey, ChunkKey)` 升序。
    pub fn iter(&self) -> impl Iterator<Item = (&(BodyKey, ChunkKey), &ResidencySlot)> + '_ {
        self.slots.iter()
    }

    /// 槽位数量（含缓降级中的槽位）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// 是否没有任何槽位。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// 某个来源当前请求的全部区块（键序确定，空则空切片）。
    #[must_use]
    pub fn requests_of(&self, source: RequestSource) -> &[(BodyKey, ChunkKey)] {
        self.by_source.get(&source).map(Vec::as_slice).unwrap_or(&[])
    }

    /// 某区块的目标档位；没有槽位时视为 [`Residency::None`]。
    #[must_use]
    pub fn target(&self, body: BodyKey, chunk: ChunkKey) -> Residency {
        self.slot(body, chunk).map_or(Residency::None, ResidencySlot::target)
    }

    /// 某区块的到达档位；没有槽位时视为 [`Residency::None`]。
    #[must_use]
    pub fn reached(&self, body: BodyKey, chunk: ChunkKey) -> Residency {
        self.slot(body, chunk).map_or(Residency::None, ResidencySlot::reached)
    }

    /// 是否该区块仍被任何来源请求（不管档位）。
    #[must_use]
    pub fn is_requested(&self, body: BodyKey, chunk: ChunkKey) -> bool {
        self.slot(body, chunk).is_some_and(|s| !s.requests.is_empty())
    }

    /// 推进 tick，返回新 tick。由 `Collect` 每 tick 调一次。
    pub(crate) fn begin_tick(&mut self) -> u64 {
        self.tick = self.tick.saturating_add(1);
        self.tick
    }

    /// 整体替换一个来源的请求集合（空集合 = 清空该来源）。
    ///
    /// 语义与文档一致：消失的来源整体消失，不产生「先清空再逐条加」的虚假卸载倒计时。
    pub(crate) fn apply_snapshot(
        &mut self,
        source: RequestSource,
        snapshot: BTreeMap<(BodyKey, ChunkKey), Residency>,
        tick: u64,
    ) {
        let old = self.by_source.remove(&source).unwrap_or_default();

        for key in &old {
            let Some(slot) = self.slots.get_mut(key) else {
                continue;
            };
            slot.requests.remove(&source);
            if slot.requests.is_empty() {
                // 最后一次有请求的 tick 就是当前 tick：降级从这一刻开始计时。
                slot.last_requested_tick = tick;
            }
            self.dirty.insert(*key);
        }

        if snapshot.is_empty() {
            return;
        }

        let mut keys = Vec::with_capacity(snapshot.len());
        for (key, level) in snapshot {
            debug_assert!(
                level != Residency::None,
                "请求集合里不应出现 Residency::None（None 用移除表示）"
            );
            if level == Residency::None {
                continue;
            }
            let slot = self.slots.entry(key).or_default();
            slot.requests.insert(source, level);
            slot.last_requested_tick = tick;
            self.dirty.insert(key);
            keys.push(key);
        }

        if !keys.is_empty() {
            // `snapshot` 来自 `BTreeMap` 遍历，`keys` 天然升序。
            self.by_source.insert(source, keys);
        }
    }

    /// 清空一个来源的全部请求（标签移除 / 实体销毁时用）。
    pub(crate) fn clear_source(&mut self, source: RequestSource) {
        let old = self.by_source.remove(&source).unwrap_or_default();

        for key in old {
            let Some(slot) = self.slots.get_mut(&key) else {
                continue;
            };
            slot.requests.remove(&source);
            if slot.requests.is_empty() {
                slot.last_requested_tick = self.tick;
            }
            self.dirty.insert(key);
        }
    }

    /// 流式汇报的实际档位。
    pub(crate) fn set_reached(&mut self, body: BodyKey, chunk: ChunkKey, level: Residency) {
        if !body.is_valid() {
            debug_assert!(false, "ChunkReached 带 BodyKey::INVALID");
            warn!("residency: ChunkReached 带无效 BodyKey，已忽略（chunk={chunk:?}）");
            return;
        }
        self.slots.entry((body, chunk)).or_default().reached = level;
    }
}

// ───────────────────────────── 系统钉住 ─────────────────────────────

/// 系统钉住表：`pin` / `unpin_all`，带命名空间避免两个系统撞 ID。
///
/// 与其它来源一样，钉住只记请求；撤销后仍走 `demote_grace_ticks` 防抖。
#[derive(Resource, Debug, Default)]
pub struct PinRequests {
    pins: BTreeMap<(u32, u64), BTreeMap<(BodyKey, ChunkKey), Residency>>,
    /// 自上次 `Collect` 以来被改动的来源（键序确定）。
    touched: BTreeSet<(u32, u64)>,
}

impl PinRequests {
    /// 钉住一个区块到指定档位。
    ///
    /// `level == None` 等价于只撤销这一条（`None` 不是请求，不能占着防抖计时）。
    /// `(namespace, id)` 相同的多次调用覆盖同一来源；不同 namespace 互不影响。
    pub fn pin(&mut self, namespace: u32, id: u64, body: BodyKey, key: ChunkKey, level: Residency) {
        let source = (namespace, id);
        self.touched.insert(source);
        if level == Residency::None {
            if let Some(map) = self.pins.get_mut(&source) {
                map.remove(&(body, key));
            }
        } else {
            self.pins
                .entry(source)
                .or_default()
                .insert((body, key), level);
        }
    }

    /// 撤销某个来源的全部钉住。
    pub fn unpin_all(&mut self, namespace: u32, id: u64) {
        self.touched.insert((namespace, id));
        self.pins.remove(&(namespace, id));
    }

    /// 查询某来源对某区块的钉住档位。
    #[must_use]
    pub fn pinned(&self, namespace: u32, id: u64, body: BodyKey, key: ChunkKey) -> Option<Residency> {
        self.pins
            .get(&(namespace, id))
            .and_then(|map| map.get(&(body, key)))
            .copied()
    }

    /// 某来源当前钉住的条数。
    #[must_use]
    pub fn source_len(&self, namespace: u32, id: u64) -> usize {
        self.pins.get(&(namespace, id)).map_or(0, BTreeMap::len)
    }

    pub(crate) fn take_touched(&mut self) -> BTreeSet<(u32, u64)> {
        std::mem::take(&mut self.touched)
    }

    pub(crate) fn snapshot(
        &self,
        namespace: u32,
        id: u64,
    ) -> BTreeMap<(BodyKey, ChunkKey), Residency> {
        self.pins
            .get(&(namespace, id))
            .cloned()
            .unwrap_or_default()
    }
}

// ───────────────────────────── 配置与统计 ─────────────────────────────

/// 驻留配置。
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct ResidencyConfig {
    /// 请求消失后延迟多少 tick 才降一级（防抖）。每降一级重新计时。
    pub demote_grace_ticks: u64,
    /// 内存软上限（已用容量百分比 0–100；`None` = 不限制）。
    ///
    /// 预留：等流式插件落地 `MemoryPressure`（见实现文档 `10）后接入
    /// `Resolve` 的「只记录、不兑现」逻辑；当前没有压力来源，不会扣下升级。
    pub memory_soft_limit: Option<u32>,
}

impl Default for ResidencyConfig {
    fn default() -> Self {
        Self {
            // 实测前先给保守值：0.5 秒（60 Hz）。
            demote_grace_ticks: 30,
            memory_soft_limit: None,
        }
    }
}

/// 驻留统计，供流式统计面板使用。每 tick 由 `Resolve` / `Gate` 刷新。
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ResidencyStats {
    /// 目标档位为 `None` 的区块数。
    pub none_chunks: usize,
    /// 目标档位为 `Data` 的区块数。
    pub data_chunks: usize,
    /// 目标档位为 `Render` 的区块数。
    pub render_chunks: usize,
    /// 因内存压力被扣下的升级次数（累计；当前没有压力来源，恒为 0）。
    pub deferred_upgrades: u64,
    /// 当前挂着 [`AwaitingChunks`] 的 `Real` 实体数。
    pub awaiting_entities: usize,
}

// ───────────────────────────── 消息 ─────────────────────────────

/// 区块目标档位变化（本插件唯一的对外变化出口）。
///
/// 发射顺序由 `(body, key)` 键序决定，与 `Query` 遍历顺序无关（确定性）。
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResidencyChanged {
    /// 体。
    pub body: BodyKey,
    /// 体局部块坐标。
    pub key: ChunkKey,
    /// 变化前档位。
    pub from: Residency,
    /// 变化后档位。
    pub to: Residency,
}

/// 流式汇报的实际到达档位（输入消息）。
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkReached {
    /// 体。
    pub body: BodyKey,
    /// 体局部块坐标。
    pub key: ChunkKey,
    /// 实际到达的档位。
    pub level: Residency,
}

/// 标记「编辑过但还没存档」的区块：记一条 [`RequestSource::UnsavedEdit`] 请求。
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsavedEditMark {
    /// 体。
    pub body: BodyKey,
    /// 体局部块坐标。
    pub key: ChunkKey,
}

/// 撤销 [`UnsavedEditMark`]。存档落地后由存档层发送。
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsavedEditClear {
    /// 体。
    pub body: BodyKey,
    /// 体局部块坐标。
    pub key: ChunkKey,
}

// ───────────────────────────── 系统集 ─────────────────────────────

/// 全局 tick 调度约定（驻留 / 流式 / 世界区块三个插件共用）。
///
/// 只在 `FixedUpdate` 内使用；渲染相关（网格上传、材质）放 `Update`，不进本表。
/// 本插件只声明 [`GameTickSet::Residency`] 内部顺序，不 chain 别人的集合。
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GameTickSet {
    /// 驻留：收集请求、汇总、发消息、门控。
    Residency,
    /// 流式：消费 [`ResidencyChanged`]，推进生成、网格、卸载。
    Streaming,
    /// 区块管理：实体归属迁移、世界区块加载状态。
    WorldChunk,
    /// 游戏逻辑（AI、机器、漂移等，必须过滤 [`AwaitingChunks`]）。
    Gameplay,
    /// 存档：脏标记、后台 IO 轮询。
    Save,
}

/// 驻留插件内部的系统顺序（在每个 tick 内 `chain`）。
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResidencySet {
    /// 从位置重算锚点。
    Anchor,
    /// 收集请求（标签变化、钉住、未存档编辑、到达档位）。
    Collect,
    /// 汇总成目标档位（升级立即、降级防抖）。
    Resolve,
    /// 按键序发出 [`ResidencyChanged`]。
    Emit,
    /// 按 `reached` 门控 `Real` 实体。
    Gate,
}

// ───────────────────────────── 内部小工具 ─────────────────────────────

/// 从实体读稳定 ID。缺失时 debug 断言、release 告警并忽略该实体（文档 `5 约束）。
#[track_caller]
pub(crate) fn body_key_for(entity: Entity, id: Option<&StableEntityId>) -> Option<BodyKey> {
    match id {
        Some(id) => Some(BodyKey(*id)),
        None => {
            debug_assert!(
                false,
                "residency: 实体 {entity:?} 挂载了 Real/Observer 但没有 StableEntityId"
            );
            warn!(
                "residency: entity {entity:?} 挂载了 Real/Observer 但没有 StableEntityId，已忽略"
            );
            None
        }
    }
}
