//! 物体提案与全序裁决（纯数据层）。
//!
//! 对应《体素流式_实现文档》§5（trait、优先级、影响集）与 §6（裁决）：
//! 把「世界物体」编译成 [`ProposalRecord`]，再按全序规则淘汰重叠者。
//! 本文件主体是**纯函数与数据**：不接 `stream.rs`、不碰存档。末尾提供
//! [`FeaturePlugin`] / [`WorldFeatureAppExt`] 两个装配入口（不依赖具体物体）。
//! 27 块并集的收集、状态机推进与跨块调度留给后续阶段；本文件的 `realize`
//! 只写调用方指定的一个块。
//!
//! ## 确定性（L3）
//!
//! - 注册表是 `BTreeMap<u16, _>`：遍历顺序只由类型 ID 决定，与注册顺序无关；
//! - 平局键 [`tie_break`] 用 `game_engine::save::splitmix64` 顺序混合，
//!   禁止 `DefaultHasher`，也不自己另抄一份哈希；
//! - 全程整数，没有 `f32/f64`，没有 `HashMap`；裁决只对排序后的副本做贪心；
//! - 提案由 `(seed, owner, seq)` 唯一确定：同一输入永远返回同一输出。
//!
//! ## 为什么裁决必须是全序
//!
//! 「两两相交就淘汰」不是良定义的关系：A 与 B 相交、B 与 C 相交、A 与 C 不
//! 相交时，逐对布尔会按遍历顺序保留 A、C 还是别的组合说不清；两个相邻区块
//! 各自跑一遍还可能得出不同结论，边界上就会出现半棵树。所以先定义与区块
//! **无关**的全序键
//! `(priority 降序, tie_break 升序, origin 升序, feature_type 升序, seq 升序)`，
//! 再按这个顺序贪心保留：每个候选只与**已保留者**比较，比较键不含 owner，
//! 27 块并集在任意一侧都排出同一个序列，同一冲突必然同解。
//!
//! 相交判定用提案的**原始 bounds**，不是裁剪到本块之后的包围盒——裁剪会让
//! 同一棵跨界的树在两侧看到不同形状，结论自然不同。
//!
//! ## 落地（realize）
//!
//! [`ChunkSink`] 是 feature 唯一的写入口：世界坐标先减去块原点，落在
//! `0..32` 之外就静默丢弃。跨界提案在每个相交块里各 `realize` 一次，每次只写
//! 本块那一段，块与块之间没有共享写入，所以裁决全序在块内可复现。
//! [`realize_chunk`] 只处理与本块相交的赢家，并按 `(feature_type, seq)` 升序
//! 调用：同一块内的写入顺序与传入顺序无关。
//!
//! ## 范围外（下一步）
//!
//! - 27 块并集的收集与缓存（[`arbitrate`] 只接收收集好的切片）；
//! - `settle` 状态机接线与 27 块并集的 realize 调度；
//! - 提案落盘 / 读档重算；
//! - 提案密度统计（提案数 / 保留数）。

use std::collections::BTreeMap;

use bevy::math::IVec3;
use game_engine::save::splitmix64;
use game_engine::streaming::{ProposalRecord, MAX_PROPOSAL_EXTENT};
use game_engine::voxel::{chunk_voxels_per_axis, ChunkKey, VoxInterner, VoxOpsWrite, VoxTree};

/// 一条待框架盖章的提案：feature 只给几何与优先级，类型 ID / 序号 / tie_break 由框架填。
///
/// `origin` 必须落在 `[bounds_min, bounds_max)` 内；包围盒允许溢出所属区块
/// （影响集覆盖 3×3×3 邻域），但单轴跨度不得超过 [`MAX_PROPOSAL_EXTENT`]
/// （96 体素）。两条约束都由 [`WorldFeatureRegistry::propose_chunk`] 逐条校验。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProposalSpec {
    /// 提案原点（体素坐标）。
    pub origin: [i32; 3],
    /// 影响集包围盒下界（含，体素坐标）。
    pub bounds_min: [i32; 3],
    /// 影响集包围盒上界（不含，体素坐标）。
    pub bounds_max: [i32; 3],
    /// 数值越大越优先（与 [`ProposalRecord::priority`] 同义）。
    pub priority: i32,
}

/// 一个区块的写入窗口：feature 只能通过它把体素写进当前块。
///
/// 越界写入（world 坐标落在本块 `0..32` 之外）**静默忽略并返回 false**：跨界
/// 提案在每一侧都只写自己那一段，写入永远是局部的——这是裁决全序的前提。
///
/// `tree` / `interner` 必须是同一个区块的树与同一个 interner（调用方责任），
/// 本类型不做检测。
pub struct ChunkSink<'a> {
    tree: &'a mut VoxTree<u8>,
    interner: &'a mut VoxInterner<u8>,
    origin: [i32; 3],
}

impl<'a> ChunkSink<'a> {
    /// tree/interner 必须是本块的树与同一个 interner；origin 是本块的体素原点。
    pub fn new(
        tree: &'a mut VoxTree<u8>,
        interner: &'a mut VoxInterner<u8>,
        origin: [i32; 3],
    ) -> Self {
        Self {
            tree,
            interner,
            origin,
        }
    }

    /// 本块的体素原点（= `ChunkKey × 32`）。
    #[must_use]
    pub fn origin(&self) -> [i32; 3] {
        self.origin
    }

    /// 块内坐标写入；越界忽略返回 false。
    pub fn set_local(&mut self, local: [i32; 3], value: u8) -> bool {
        let extent = chunk_voxels_per_axis();
        if !(0..3).all(|axis| (0..extent).contains(&local[axis])) {
            return false;
        }
        self.tree.set(
            self.interner,
            IVec3::new(local[0], local[1], local[2]),
            value,
        )
    }

    /// 世界（体素）坐标写入；不在本块返回 false。
    pub fn set_world(&mut self, world: [i32; 3], value: u8) -> bool {
        let extent = i64::from(chunk_voxels_per_axis());
        let mut local = [0_i32; 3];
        for axis in 0..3 {
            // 用 i64 做减法：极端坐标下 i32 会溢出。
            let offset = i64::from(world[axis]) - i64::from(self.origin[axis]);
            if offset < 0 || offset >= extent {
                return false;
            }
            local[axis] = offset as i32;
        }
        self.set_local(local, value)
    }
}

/// 一种世界物体（树、石、建筑……）的纯提案函数。
///
/// 契约：纯函数——只读 `(seed, owner)`，不读邻居、不读世界、不写任何状态；
/// 同一输入必须永远返回同一输出（确定性 L3）。trait 方法拿不到 `World` / `Res`，
/// 从签名上就隔离了不该读的东西。
///
/// `TYPE_ID` 是**关联常量**而不是实例方法：它只与类型绑定，存档和哈希都只认它。
pub trait WorldFeature: Send + Sync + 'static {
    /// 稳定类型 ID，写进存档；0 保留，注册时拒绝。
    const TYPE_ID: u16;

    /// 本区块拥有的提案（可以溢出到 3×3×3 邻域）。
    ///
    /// 返回 Vec 的下标就是提案的 `seq`，所以同一份返回值里顺序即身份：
    /// 不要用非确定顺序（例如遍历 `HashMap`）来生成它。
    fn propose(&self, seed: u64, owner: ChunkKey) -> Vec<ProposalSpec>;

    /// 把裁决后保留的一条提案写进当前块；只写落在 sink 内（本块）的体素。
    ///
    /// 同一提案会在它覆盖的每个块里各调用一次，每次只写本块那一段。
    /// 越界写入由 [`ChunkSink`] 丢弃，feature 不需要自己裁剪。
    ///
    /// 没有默认实现：注册一个新类型却忘了落地，应该在编译期就暴露，而不是
    /// 静默地什么都不写。
    fn realize(&self, seed: u64, proposal: &ProposalRecord, sink: &mut ChunkSink<'_>);
}

/// 类型擦除后的提案函数。
///
/// 带关联常量的 trait **不能**做 trait 对象（E0038），所以注册表不直接存
/// `Box<dyn WorldFeature>`，而是存这个 object-safe 的适配层；类型 ID 由
/// [`WorldFeatureRegistry`] 在注册时单独记下。对调用方没有区别。
trait ErasedFeature: Send + Sync + 'static {
    /// 与 [`WorldFeature::propose`] 完全同义，只是去掉了关联常量。
    fn propose(&self, seed: u64, owner: ChunkKey) -> Vec<ProposalSpec>;

    /// 与 [`WorldFeature::realize`] 完全同义，只是去掉了关联常量。
    fn realize(&self, seed: u64, proposal: &ProposalRecord, sink: &mut ChunkSink<'_>);
}

impl<F: WorldFeature> ErasedFeature for F {
    fn propose(&self, seed: u64, owner: ChunkKey) -> Vec<ProposalSpec> {
        WorldFeature::propose(self, seed, owner)
    }

    fn realize(&self, seed: u64, proposal: &ProposalRecord, sink: &mut ChunkSink<'_>) {
        WorldFeature::realize(self, seed, proposal, sink)
    }
}

/// 已注册的物体类型：类型 ID -> 提案函数。
///
/// 键是 [`WorldFeature::TYPE_ID`]，遍历顺序只由 ID 升序决定：Mod 加载顺序、
/// 注册顺序都不影响结果。
///
/// 作为 Bevy 资源插入世界（`insert_resource(WorldFeatureRegistry::default())`）：
/// 没有它时流式层把「世界里没有物体」处理成提案恒为空。
#[derive(Default, bevy::prelude::Resource)]
pub struct WorldFeatureRegistry {
    features: BTreeMap<u16, Box<dyn ErasedFeature>>,
}

impl WorldFeatureRegistry {
    /// 按类型注册一个物体；`F` 由 `Default` 构造（提案函数本应无状态）。
    ///
    /// 拒绝：类型 ID 为 0（[`FeatureError::ZeroTypeId`]）、类型 ID 已注册
    /// （[`FeatureError::DuplicateTypeId`]，**不覆盖**已有实例）。
    ///
    /// 注册顺序不影响 [`Self::propose_chunk`] 的输出。
    pub fn register<F>(&mut self) -> Result<(), FeatureError>
    where
        F: WorldFeature + Default,
    {
        let type_id = F::TYPE_ID;
        if type_id == 0 {
            return Err(FeatureError::ZeroTypeId);
        }
        if self.features.contains_key(&type_id) {
            return Err(FeatureError::DuplicateTypeId(type_id));
        }
        self.features.insert(type_id, Box::new(F::default()));
        Ok(())
    }

    /// 已注册的类型数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.features.len()
    }

    /// 是否没有任何已注册类型。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.features.is_empty()
    }

    /// 已注册的类型 ID，升序。
    #[must_use]
    pub fn type_ids(&self) -> Vec<u16> {
        self.features.keys().copied().collect()
    }

    /// 某个类型 ID 是否已注册。
    #[must_use]
    pub fn contains(&self, feature_type: u16) -> bool {
        self.features.contains_key(&feature_type)
    }

    /// 生成 `owner` 拥有的全部提案，并盖上类型 ID / 序号 / 平局键。
    ///
    /// 遍历注册表按类型 ID 升序，结果再按 `(feature_type, seq)` 显式排序一次：
    /// 注册顺序不影响输出。`seq` = 该 feature 返回 Vec 的下标
    /// （`u32` 转换溢出报 [`FeatureError::TooManyProposals`]）。
    ///
    /// 逐轴校验 `min < max`、跨度不超过 [`MAX_PROPOSAL_EXTENT`]、
    /// `origin` 落在 `[min, max)` 内；第一条不合法的提案立即返回
    /// [`FeatureError::InvalidProposal`]，不产出部分结果。
    pub fn propose_chunk(
        &self,
        seed: u64,
        owner: ChunkKey,
    ) -> Result<Vec<ProposalRecord>, FeatureError> {
        let mut records = Vec::new();
        for (&feature_type, feature) in &self.features {
            for (index, spec) in feature.propose(seed, owner).into_iter().enumerate() {
                let seq = u32::try_from(index)
                    .map_err(|_| FeatureError::TooManyProposals { feature_type })?;
                validate_spec(feature_type, seq, &spec)?;
                records.push(ProposalRecord {
                    feature_type,
                    seq,
                    priority: spec.priority,
                    tie_break: tie_break(seed, feature_type, spec.origin, seq),
                    origin: spec.origin,
                    bounds_min: spec.bounds_min,
                    bounds_max: spec.bounds_max,
                });
            }
        }
        // BTreeMap 遍历已经保证 ID 升序；显式排一次，防止以后换容器时静默破坏契约。
        records.sort_by_key(|record| (record.feature_type, record.seq));
        Ok(records)
    }
}

/// 校验一条未盖章的提案，出错时带上是哪条（类型 ID、序号）与原因。
fn validate_spec(feature_type: u16, seq: u32, spec: &ProposalSpec) -> Result<(), FeatureError> {
    let limit = i64::from(MAX_PROPOSAL_EXTENT);
    for axis in 0..3 {
        // 用 i64 做减法：i32 的 `max - min` 在极端坐标上会溢出。
        let min = i64::from(spec.bounds_min[axis]);
        let max = i64::from(spec.bounds_max[axis]);
        let origin = i64::from(spec.origin[axis]);
        if min >= max {
            return Err(FeatureError::InvalidProposal {
                feature_type,
                seq,
                reason: "包围盒下界必须小于上界（逐轴 min < max）",
            });
        }
        if max - min > limit {
            return Err(FeatureError::InvalidProposal {
                feature_type,
                seq,
                reason: "包围盒单轴跨度超过 MAX_PROPOSAL_EXTENT（96 体素）",
            });
        }
        if origin < min || origin >= max {
            return Err(FeatureError::InvalidProposal {
                feature_type,
                seq,
                reason: "提案原点必须落在包围盒内（min <= origin < max）",
            });
        }
    }
    Ok(())
}

/// 平局键：顺序混合 `(seed, 类型 ID, 原点, 序号)`。
///
/// 顺序混合，禁止一次 XOR 拼接（一次 XOR 会把「换轴」和「换值」搅成同一个
/// 结果）。公式固定：
///
/// ```text
/// h0 = splitmix64(seed)
/// h1 = splitmix64(h0 ^ feature_type)
/// h2..h4 = 逐轴 splitmix64(h ^ (coord as i64 as u64))
/// tie_break = splitmix64(h4 ^ seq)
/// ```
///
/// 用 [`splitmix64`]（唯一允许的哈希实现），全程整数、与平台字节序无关；
/// 同一条提案在任何机器 / 任何调用顺序下都是同一个键。
#[must_use]
pub fn tie_break(seed: u64, feature_type: u16, origin: [i32; 3], seq: u32) -> u64 {
    let mut hash = splitmix64(seed);
    hash = splitmix64(hash ^ u64::from(feature_type));
    for axis in origin {
        hash = splitmix64(hash ^ i64::from(axis) as u64);
    }
    splitmix64(hash ^ u64::from(seq))
}

/// 27 块提案并集 → 保留下来的提案（按裁决顺序，确定性）。
///
/// 排序键：`priority` 降序 → `tie_break` 升序 → `origin` 逐轴升序 →
/// `feature_type` 升序 → `seq` 升序。依次取用：与任一**已保留**提案的
/// 原始 bounds 相交（逐轴 `a.min < b.max && a.max > b.min`）即淘汰，
/// 不相交就保留。
///
/// 相交判定用**原始 bounds**（不是裁剪到本块后的），否则跨界提案两侧会得出
/// 不同结论。
///
/// 不在入参顺序上做任何假设：内部先 `sort_by` 一个副本，空输入返回空 Vec。
/// 同一条提案重复出现时，后来的那条会与自己相交，自然只保留一条。
#[must_use]
pub fn arbitrate(proposals: &[ProposalRecord]) -> Vec<ProposalRecord> {
    let mut ordered = proposals.to_vec();
    ordered.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| a.tie_break.cmp(&b.tie_break))
            .then_with(|| a.origin.cmp(&b.origin))
            .then_with(|| a.feature_type.cmp(&b.feature_type))
            .then_with(|| a.seq.cmp(&b.seq))
    });

    let mut kept: Vec<ProposalRecord> = Vec::new();
    for proposal in ordered {
        if kept.iter().any(|winner| bounds_overlap(winner, &proposal)) {
            continue;
        }
        kept.push(proposal);
    }
    kept
}

/// 两个包围盒是否相交：逐轴 `a.min < b.max && a.max > b.min`（上界不含）。
fn bounds_overlap(a: &ProposalRecord, b: &ProposalRecord) -> bool {
    (0..3).all(|axis| {
        a.bounds_min[axis] < b.bounds_max[axis] && a.bounds_max[axis] > b.bounds_min[axis]
    })
}

/// 把裁决后的赢家写进一个区块；返回真正落地的提案数。
///
/// - 只处理 `bounds` 与本块相交的提案（相交判定与 [`arbitrate`] 一致，用原始
///   bounds）；
/// - 按 `(feature_type, seq)` 升序调用，保证同一块内写入顺序确定；
/// - 类型 ID 未注册 → `Err(FeatureError::UnknownType(id))`（不跳过、不猜）；
/// - `tree` 必须是本块的树（由调用方保证）。
///
/// 跨界提案会在它覆盖的每个块里各被调用一次，每次只写落进本块的那一段。
pub fn realize_chunk(
    registry: &WorldFeatureRegistry,
    seed: u64,
    chunk: ChunkKey,
    winners: &[ProposalRecord],
    tree: &mut VoxTree<u8>,
    interner: &mut VoxInterner<u8>,
) -> Result<usize, FeatureError> {
    let extent = i64::from(chunk_voxels_per_axis());
    let mut origin = [0_i32; 3];
    for (axis, coordinate) in [chunk.x, chunk.y, chunk.z].into_iter().enumerate() {
        // 先用 i64 乘：i32 直接乘 32 会在区块坐标较大时溢出。
        let world = i64::from(coordinate) * extent;
        origin[axis] =
            i32::try_from(world).map_err(|_| FeatureError::ChunkOriginOverflow { chunk })?;
    }

    // 本块覆盖的世界坐标区间是半开的 [min, max)。
    let mut chunk_min = [0_i64; 3];
    let mut chunk_max = [0_i64; 3];
    for axis in 0..3 {
        chunk_min[axis] = i64::from(origin[axis]);
        chunk_max[axis] = chunk_min[axis] + extent;
    }

    let mut pending: Vec<&ProposalRecord> = winners
        .iter()
        .filter(|proposal| {
            (0..3).all(|axis| {
                i64::from(proposal.bounds_min[axis]) < chunk_max[axis]
                    && i64::from(proposal.bounds_max[axis]) > chunk_min[axis]
            })
        })
        .collect();
    pending.sort_by_key(|proposal| (proposal.feature_type, proposal.seq));

    let mut sink = ChunkSink::new(tree, interner, origin);
    let mut realized = 0;
    for proposal in pending {
        let feature = registry
            .features
            .get(&proposal.feature_type)
            .ok_or(FeatureError::UnknownType(proposal.feature_type))?;
        feature.realize(seed, proposal, &mut sink);
        realized += 1;
    }
    Ok(realized)
}

/// 提案 / 注册阶段拒绝的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeatureError {
    /// 类型 ID 0 是保留值，不能注册。
    ZeroTypeId,
    /// 类型 ID 重复；已注册的实例保持不变，不覆盖。
    DuplicateTypeId(u16),
    /// 一个 feature 返回的提案数超过 `u32::MAX`，`seq` 装不下。
    TooManyProposals {
        /// 出错的类型 ID。
        feature_type: u16,
    },
    /// 提案几何不合法；`reason` 是给人看的具体原因（见 [`validate_spec`]）。
    InvalidProposal {
        /// 出错的类型 ID。
        feature_type: u16,
        /// 该 feature 返回 Vec 里的下标。
        seq: u32,
        /// 具体原因（中文，可直接展示）。
        reason: &'static str,
    },
    /// 裁决结果里出现了一个没有注册的类型 ID。
    UnknownType(u16),
    /// 区块坐标换算成 i32 体素原点时溢出。
    ChunkOriginOverflow {
        /// 出错的区块。
        chunk: ChunkKey,
    },
}

impl std::fmt::Display for FeatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroTypeId => write!(f, "物体类型 ID 0 是保留值，不能注册"),
            Self::DuplicateTypeId(id) => write!(f, "物体类型 ID {id} 已注册，拒绝覆盖"),
            Self::TooManyProposals { feature_type } => {
                write!(f, "物体类型 {feature_type} 的提案数超过 u32 上限")
            }
            Self::InvalidProposal {
                feature_type,
                seq,
                reason,
            } => write!(
                f,
                "提案包围盒非法（类型 {feature_type}，序号 {seq}）：{reason}"
            ),
            Self::UnknownType(id) => write!(f, "裁决结果里的类型 ID 未注册：{id}"),
            Self::ChunkOriginOverflow { chunk } => write!(
                f,
                "区块 ({}, {}, {}) 的体素原点超出 i32 范围",
                chunk.x, chunk.y, chunk.z
            ),
        }
    }
}

impl std::error::Error for FeatureError {}

/// 世界物体注册表插件：以空资源插入 [`WorldFeatureRegistry`]。
///
/// 只做资源初始化；具体物体由 [`WorldFeatureAppExt::register_world_feature`]
/// 注册。重复 add 同一个插件由 Bevy 拒绝，这里不再额外判重。
pub struct FeaturePlugin;

impl bevy::prelude::Plugin for FeaturePlugin {
    fn build(&self, app: &mut bevy::prelude::App) {
        app.init_resource::<WorldFeatureRegistry>();
    }
}

/// `App` 上的世界物体注册扩展。
pub trait WorldFeatureAppExt {
    /// 注册一种世界物体。
    ///
    /// 类型 ID 为 0 或与已注册类型重复时直接 `panic!`（消息带
    /// [`FeatureError`] 的 Display）：注册表是配置，配错应当当场暴露，
    /// 而不是静默丢掉这个类型。
    fn register_world_feature<F: WorldFeature + Default>(&mut self) -> &mut Self;
}

impl WorldFeatureAppExt for bevy::prelude::App {
    fn register_world_feature<F: WorldFeature + Default>(&mut self) -> &mut Self {
        self.init_resource::<WorldFeatureRegistry>();
        self.world_mut()
            .resource_mut::<WorldFeatureRegistry>()
            .register::<F>()
            .unwrap_or_else(|error| {
                panic!("注册世界物体失败（TYPE_ID = {}）：{error}", F::TYPE_ID)
            });
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::prelude::App;
    use game_engine::voxel::{chunk_content_hash, MaxDepth, VoxOpsRead, CHUNK_DEPTH};

    use crate::voxel::features::TreeFeature;

    /// 与 `CHUNK` 对齐的区块边长（crates/voxel 的 `CHUNK_DEPTH`，测试只用来算假原点）。
    const CHUNK: i32 = 32;

    fn chunk_origin(owner: ChunkKey) -> [i32; 3] {
        [owner.x * CHUNK, owner.y * CHUNK, owner.z * CHUNK]
    }

    /// 原点在包围盒下界、三轴跨度相同的提案。
    fn spec(origin: [i32; 3], span: i32, priority: i32) -> ProposalSpec {
        ProposalSpec {
            origin,
            bounds_min: origin,
            bounds_max: [origin[0] + span, origin[1] + span, origin[2] + span],
            priority,
        }
    }

    /// 直接拼一条已盖章的记录（裁决测试不经过注册表）。
    fn record(
        feature_type: u16,
        seq: u32,
        priority: i32,
        tie_break: u64,
        min: [i32; 3],
        max: [i32; 3],
    ) -> ProposalRecord {
        ProposalRecord {
            feature_type,
            seq,
            priority,
            tie_break,
            origin: min,
            bounds_min: min,
            bounds_max: max,
        }
    }

    /// 注册一个假 feature 并立刻提案，供非法几何测试使用。
    fn propose_with<F: WorldFeature + Default>(
        seed: u64,
        owner: ChunkKey,
    ) -> Result<Vec<ProposalRecord>, FeatureError> {
        let mut registry = WorldFeatureRegistry::default();
        registry.register::<F>()?;
        registry.propose_chunk(seed, owner)
    }

    /// 假 feature：每个区块两条树提案（块内、包围盒小）。
    #[derive(Default)]
    struct Tree;

    impl WorldFeature for Tree {
        const TYPE_ID: u16 = 7;

        fn propose(&self, _seed: u64, owner: ChunkKey) -> Vec<ProposalSpec> {
            let base = chunk_origin(owner);
            vec![
                spec([base[0] + 4, base[1] + 4, base[2] + 4], 3, 100),
                spec([base[0] + 20, base[1] + 4, base[2] + 4], 2, 90),
            ]
        }

        fn realize(&self, _seed: u64, _proposal: &ProposalRecord, _sink: &mut ChunkSink<'_>) {}
    }

    /// 假 feature：每个区块一条高优先级石头提案。
    #[derive(Default)]
    struct Rock;

    impl WorldFeature for Rock {
        const TYPE_ID: u16 = 9;

        fn propose(&self, _seed: u64, owner: ChunkKey) -> Vec<ProposalSpec> {
            let base = chunk_origin(owner);
            vec![spec([base[0] + 16, base[1] + 8, base[2] + 8], 4, 200)]
        }

        fn realize(&self, _seed: u64, _proposal: &ProposalRecord, _sink: &mut ChunkSink<'_>) {}
    }

    /// 非法：类型 ID 0。
    #[derive(Default)]
    struct ZeroId;

    impl WorldFeature for ZeroId {
        const TYPE_ID: u16 = 0;

        fn propose(&self, _seed: u64, _owner: ChunkKey) -> Vec<ProposalSpec> {
            Vec::new()
        }

        fn realize(&self, _seed: u64, _proposal: &ProposalRecord, _sink: &mut ChunkSink<'_>) {}
    }

    /// 非法：单轴 `min == max`（空包围盒）。
    #[derive(Default)]
    struct FlatBox;

    impl WorldFeature for FlatBox {
        const TYPE_ID: u16 = 11;

        fn propose(&self, _seed: u64, _owner: ChunkKey) -> Vec<ProposalSpec> {
            vec![ProposalSpec {
                origin: [0, 0, 0],
                bounds_min: [0, 0, 0],
                bounds_max: [0, 4, 4],
                priority: 0,
            }]
        }

        fn realize(&self, _seed: u64, _proposal: &ProposalRecord, _sink: &mut ChunkSink<'_>) {}
    }

    /// 非法：单轴 `min > max`。
    #[derive(Default)]
    struct InvertedBox;

    impl WorldFeature for InvertedBox {
        const TYPE_ID: u16 = 12;

        fn propose(&self, _seed: u64, _owner: ChunkKey) -> Vec<ProposalSpec> {
            vec![ProposalSpec {
                origin: [-1, 0, 0],
                bounds_min: [0, 0, 0],
                bounds_max: [-2, 4, 4],
                priority: 0,
            }]
        }

        fn realize(&self, _seed: u64, _proposal: &ProposalRecord, _sink: &mut ChunkSink<'_>) {}
    }

    /// 非法：跨度 97，超过 `MAX_PROPOSAL_EXTENT`。
    #[derive(Default)]
    struct WideBox;

    impl WorldFeature for WideBox {
        const TYPE_ID: u16 = 13;

        fn propose(&self, _seed: u64, _owner: ChunkKey) -> Vec<ProposalSpec> {
            vec![spec([0, 0, 0], 97, 0)]
        }

        fn realize(&self, _seed: u64, _proposal: &ProposalRecord, _sink: &mut ChunkSink<'_>) {}
    }

    /// 合法上界：跨度正好 96。
    #[derive(Default)]
    struct ExactBox;

    impl WorldFeature for ExactBox {
        const TYPE_ID: u16 = 14;

        fn propose(&self, _seed: u64, _owner: ChunkKey) -> Vec<ProposalSpec> {
            vec![spec([0, 0, 0], MAX_PROPOSAL_EXTENT, 0)]
        }

        fn realize(&self, _seed: u64, _proposal: &ProposalRecord, _sink: &mut ChunkSink<'_>) {}
    }

    /// 非法：原点正好等于上界。
    #[derive(Default)]
    struct OriginAtMax;

    impl WorldFeature for OriginAtMax {
        const TYPE_ID: u16 = 15;

        fn propose(&self, _seed: u64, _owner: ChunkKey) -> Vec<ProposalSpec> {
            vec![ProposalSpec {
                origin: [4, 0, 0],
                bounds_min: [0, 0, 0],
                bounds_max: [4, 10, 10],
                priority: 0,
            }]
        }

        fn realize(&self, _seed: u64, _proposal: &ProposalRecord, _sink: &mut ChunkSink<'_>) {}
    }

    /// 非法：原点小于下界。
    #[derive(Default)]
    struct OriginBelowMin;

    impl WorldFeature for OriginBelowMin {
        const TYPE_ID: u16 = 16;

        fn propose(&self, _seed: u64, _owner: ChunkKey) -> Vec<ProposalSpec> {
            vec![ProposalSpec {
                origin: [-1, 0, 0],
                bounds_min: [0, 0, 0],
                bounds_max: [4, 10, 10],
                priority: 0,
            }]
        }

        fn realize(&self, _seed: u64, _proposal: &ProposalRecord, _sink: &mut ChunkSink<'_>) {}
    }

    #[test]
    fn zero_type_id_rejected() {
        let mut registry = WorldFeatureRegistry::default();
        assert_eq!(registry.register::<ZeroId>(), Err(FeatureError::ZeroTypeId));
        assert!(registry.is_empty());
        assert!(registry.type_ids().is_empty());
        assert!(!registry.contains(0));
    }

    #[test]
    fn duplicate_type_id_rejected() {
        let mut registry = WorldFeatureRegistry::default();
        assert_eq!(registry.register::<Tree>(), Ok(()));
        assert_eq!(
            registry.register::<Tree>(),
            Err(FeatureError::DuplicateTypeId(Tree::TYPE_ID))
        );
        // 不覆盖：仍是一个类型、仍只有 Tree 的两条提案。
        assert_eq!(registry.len(), 1);
        assert!(registry.contains(Tree::TYPE_ID));
        assert_eq!(registry.type_ids(), vec![Tree::TYPE_ID]);
        assert_eq!(
            registry
                .propose_chunk(1, ChunkKey::new(0, 0, 0))
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn proposals_are_stamped_with_type_seq_and_tie_break() {
        let owner = ChunkKey::new(1, -1, 2);
        let records = propose_with::<Tree>(42, owner).unwrap();
        let base = chunk_origin(owner);
        let expected = [
            (0_u32, [base[0] + 4, base[1] + 4, base[2] + 4], 3, 100),
            (1_u32, [base[0] + 20, base[1] + 4, base[2] + 4], 2, 90),
        ];
        assert_eq!(records.len(), expected.len());
        for (record, (seq, origin, span, priority)) in records.iter().zip(expected) {
            assert_eq!(record.feature_type, Tree::TYPE_ID);
            assert_eq!(record.seq, seq);
            assert_eq!(record.priority, priority);
            assert_eq!(record.origin, origin);
            assert_eq!(record.bounds_min, origin);
            assert_eq!(
                record.bounds_max,
                [origin[0] + span, origin[1] + span, origin[2] + span]
            );
            assert_eq!(record.tie_break, tie_break(42, Tree::TYPE_ID, origin, seq));
        }
    }

    #[test]
    fn registration_order_does_not_change_proposals() {
        let owner = ChunkKey::new(-2, 3, 0);

        let mut tree_first = WorldFeatureRegistry::default();
        tree_first.register::<Tree>().unwrap();
        tree_first.register::<Rock>().unwrap();

        let mut rock_first = WorldFeatureRegistry::default();
        rock_first.register::<Rock>().unwrap();
        rock_first.register::<Tree>().unwrap();

        assert_eq!(tree_first.type_ids(), vec![Tree::TYPE_ID, Rock::TYPE_ID]);
        assert_eq!(rock_first.type_ids(), tree_first.type_ids());
        assert_eq!(
            tree_first.propose_chunk(7, owner).unwrap(),
            rock_first.propose_chunk(7, owner).unwrap()
        );

        // 显式排序：类型 ID 升序，注册顺序无关。
        let records = rock_first.propose_chunk(7, owner).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].feature_type, Tree::TYPE_ID);
        assert_eq!(records[1].feature_type, Tree::TYPE_ID);
        assert_eq!(records[2].feature_type, Rock::TYPE_ID);
    }

    #[test]
    fn tie_break_is_deterministic_and_input_sensitive() {
        let base = tie_break(42, 7, [1, 2, 3], 0);
        assert_eq!(base, tie_break(42, 7, [1, 2, 3], 0));
        assert_ne!(base, tie_break(43, 7, [1, 2, 3], 0));
        assert_ne!(base, tie_break(42, 8, [1, 2, 3], 0));
        assert_ne!(base, tie_break(42, 7, [2, 2, 3], 0));
        assert_ne!(base, tie_break(42, 7, [1, 2, 4], 0));
        assert_ne!(base, tie_break(42, 7, [1, 2, 3], 1));
        // 负坐标也要稳定（`as i64 as u64` 是符号扩展，不是平台相关的重解释）。
        let negative = tie_break(42, 7, [-1, -2, -3], 0);
        assert_eq!(negative, tie_break(42, 7, [-1, -2, -3], 0));
        assert_ne!(negative, base);
    }

    #[test]
    fn propose_rejects_inverted_bounds() {
        let owner = ChunkKey::new(0, 0, 0);

        let flat = propose_with::<FlatBox>(0, owner).unwrap_err();
        assert!(matches!(
            flat,
            FeatureError::InvalidProposal {
                feature_type: 11,
                seq: 0,
                ..
            }
        ));
        assert!(flat.to_string().contains("提案包围盒非法"));

        let inverted = propose_with::<InvertedBox>(0, owner).unwrap_err();
        assert!(matches!(
            inverted,
            FeatureError::InvalidProposal {
                feature_type: 12,
                seq: 0,
                ..
            }
        ));
    }

    #[test]
    fn propose_rejects_oversized_bounds() {
        let owner = ChunkKey::new(0, 0, 0);

        let oversized = propose_with::<WideBox>(0, owner).unwrap_err();
        assert!(matches!(
            oversized,
            FeatureError::InvalidProposal {
                feature_type: 13,
                seq: 0,
                ..
            }
        ));

        // 上限本身合法：96 必须通过。
        let exact = propose_with::<ExactBox>(0, owner).unwrap();
        assert_eq!(exact.len(), 1);
        assert_eq!(
            exact[0].bounds_max[0] - exact[0].bounds_min[0],
            MAX_PROPOSAL_EXTENT
        );
    }

    #[test]
    fn propose_rejects_origin_outside_bounds() {
        let owner = ChunkKey::new(0, 0, 0);

        let at_max = propose_with::<OriginAtMax>(0, owner).unwrap_err();
        assert!(matches!(
            at_max,
            FeatureError::InvalidProposal {
                feature_type: 15,
                ..
            }
        ));

        let below_min = propose_with::<OriginBelowMin>(0, owner).unwrap_err();
        assert!(matches!(
            below_min,
            FeatureError::InvalidProposal {
                feature_type: 16,
                ..
            }
        ));
    }

    #[test]
    fn arbitrate_keeps_disjoint_proposals() {
        let a = record(7, 0, 10, 5, [0, 0, 0], [4, 4, 4]);
        let b = record(9, 0, 20, 1, [10, 0, 0], [14, 4, 4]);

        assert!(arbitrate(&[]).is_empty());
        // 输出按裁决顺序（priority 降序），不是入参顺序。
        assert_eq!(arbitrate(&[a, b]), vec![b, a]);
        assert_eq!(arbitrate(&[b, a]), vec![b, a]);
    }

    #[test]
    fn arbitrate_drops_lower_priority_overlap() {
        let high = record(7, 0, 100, 9, [0, 0, 0], [4, 4, 4]);
        let low = record(9, 0, 50, 1, [2, 0, 0], [6, 4, 4]);
        assert_eq!(arbitrate(&[low, high]), vec![high]);

        // priority 相同 → tie_break 小者胜（升序）。
        let early_tie = record(7, 0, 7, 10, [0, 0, 0], [4, 4, 4]);
        let late_tie = record(9, 0, 7, 20, [2, 0, 0], [6, 4, 4]);
        assert_eq!(arbitrate(&[late_tie, early_tie]), vec![early_tie]);
    }

    #[test]
    fn arbitrate_is_order_independent() {
        let input = vec![
            record(7, 0, 100, 30, [0, 0, 0], [6, 10, 10]),
            record(7, 1, 90, 10, [4, 0, 0], [10, 10, 10]),
            record(9, 0, 80, 20, [9, 0, 0], [15, 10, 10]),
            record(3, 0, 90, 30, [100, 0, 0], [104, 4, 4]),
            record(3, 1, 90, 31, [102, 0, 0], [106, 4, 4]),
        ];
        let expected = arbitrate(&input);
        assert!(!expected.is_empty());

        let mut reversed = input.clone();
        reversed.reverse();
        assert_eq!(arbitrate(&reversed), expected);

        let shuffled = vec![input[3], input[0], input[4], input[1], input[2]];
        assert_eq!(arbitrate(&shuffled), expected);
    }

    #[test]
    fn arbitrate_chain_overlap_keeps_first_winner() {
        // 一维链：A∩B、B∩C，但 A 与 C 不相交（y/z 范围相同）。
        let a = record(7, 0, 300, 30, [0, 0, 0], [4, 10, 10]);
        let b = record(7, 1, 200, 20, [2, 0, 0], [6, 10, 10]);
        let c = record(7, 2, 100, 10, [5, 0, 0], [9, 10, 10]);

        assert_eq!(arbitrate(&[c, b, a]), vec![a, c]);
    }

    #[test]
    fn arbitrate_same_result_for_two_owners() {
        // 跨界的一对提案：owner A 的视角先看自己的，owner B 的视角先看邻居的。
        let own_a = record(7, 0, 100, 5, [30, 0, 0], [34, 8, 8]);
        let own_b = record(9, 0, 90, 1, [32, 0, 0], [36, 8, 8]);

        let from_a = arbitrate(&[own_a, own_b]);
        let from_b = arbitrate(&[own_b, own_a]);

        assert_eq!(from_a, from_b);
        assert_eq!(from_a.len(), 1);
        assert_eq!(from_a[0].feature_type, 7);
    }

    // ---- 落地（realize） ----

    /// 把 proposal 的包围盒逐个体素写进 sink；越界由 sink 丢弃。
    fn fill_bounds(proposal: &ProposalRecord, sink: &mut ChunkSink<'_>, value: u8) {
        for x in proposal.bounds_min[0]..proposal.bounds_max[0] {
            for y in proposal.bounds_min[1]..proposal.bounds_max[1] {
                for z in proposal.bounds_min[2]..proposal.bounds_max[2] {
                    sink.set_world([x, y, z], value);
                }
            }
        }
    }

    /// 假 feature：把自己包围盒内的体素全部写成 7。
    #[derive(Default)]
    struct BlobFeature;

    impl WorldFeature for BlobFeature {
        const TYPE_ID: u16 = 21;

        fn propose(&self, _seed: u64, _owner: ChunkKey) -> Vec<ProposalSpec> {
            Vec::new()
        }

        fn realize(&self, _seed: u64, proposal: &ProposalRecord, sink: &mut ChunkSink<'_>) {
            fill_bounds(proposal, sink, 7);
        }
    }

    /// 假 feature：写 `1 + seq`，用来验证 seq 顺序。
    #[derive(Default)]
    struct SeqValueFeature;

    impl WorldFeature for SeqValueFeature {
        const TYPE_ID: u16 = 22;

        fn propose(&self, _seed: u64, _owner: ChunkKey) -> Vec<ProposalSpec> {
            Vec::new()
        }

        fn realize(&self, _seed: u64, proposal: &ProposalRecord, sink: &mut ChunkSink<'_>) {
            fill_bounds(proposal, sink, 1 + (proposal.seq as u8));
        }
    }

    /// 假 feature：把自己包围盒内的体素全部写成 9。
    #[derive(Default)]
    struct OverlayFeature;

    impl WorldFeature for OverlayFeature {
        const TYPE_ID: u16 = 30;

        fn propose(&self, _seed: u64, _owner: ChunkKey) -> Vec<ProposalSpec> {
            Vec::new()
        }

        fn realize(&self, _seed: u64, proposal: &ProposalRecord, sink: &mut ChunkSink<'_>) {
            fill_bounds(proposal, sink, 9);
        }
    }

    fn test_registry() -> WorldFeatureRegistry {
        let mut registry = WorldFeatureRegistry::default();
        registry.register::<BlobFeature>().unwrap();
        registry.register::<SeqValueFeature>().unwrap();
        registry.register::<OverlayFeature>().unwrap();
        registry
    }

    fn empty_tree_and_interner() -> (VoxTree<u8>, VoxInterner<u8>) {
        (
            VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH)),
            VoxInterner::<u8>::with_memory_budget(2 * 1024 * 1024),
        )
    }

    #[test]
    fn sink_ignores_out_of_chunk_writes() {
        let (mut tree, mut interner) = empty_tree_and_interner();
        let before = chunk_content_hash(&interner, &tree);

        {
            let mut sink = ChunkSink::new(&mut tree, &mut interner, [0, 0, 0]);
            assert_eq!(sink.origin(), [0, 0, 0]);
            assert!(!sink.set_local([32, 0, 0], 7));
            assert!(!sink.set_local([0, 32, 0], 7));
            assert!(!sink.set_local([0, 0, 32], 7));
            assert!(!sink.set_local([-1, 0, 0], 7));
            assert!(!sink.set_world([32, 0, 0], 7));
            assert!(!sink.set_world([-1, 0, 0], 7));
        }

        assert_eq!(chunk_content_hash(&interner, &tree), before);
        assert_eq!(tree.get(&interner, IVec3::new(0, 0, 0)).unwrap_or(0), 0);

        {
            let mut sink = ChunkSink::new(&mut tree, &mut interner, [0, 0, 0]);
            assert!(sink.set_local([1, 2, 3], 7));
            assert!(sink.set_world([4, 5, 6], 8));
        }
        assert_eq!(tree.get(&interner, IVec3::new(1, 2, 3)), Some(7));
        assert_eq!(tree.get(&interner, IVec3::new(4, 5, 6)), Some(8));
    }

    #[test]
    fn realize_chunk_writes_only_intersecting_part() {
        let registry = test_registry();
        // x 方向跨界：左块写 28..32，右块写 32..36（块内 0..4）。
        let winner = record(BlobFeature::TYPE_ID, 0, 10, 0, [28, 0, 0], [36, 4, 4]);

        let (mut left, mut left_interner) = empty_tree_and_interner();
        assert_eq!(
            realize_chunk(
                &registry,
                1,
                ChunkKey::new(0, 0, 0),
                &[winner.clone()],
                &mut left,
                &mut left_interner,
            ),
            Ok(1)
        );
        for x in 28..32 {
            for y in 0..4 {
                for z in 0..4 {
                    assert_eq!(left.get(&left_interner, IVec3::new(x, y, z)), Some(7));
                }
            }
        }
        for x in 0..4 {
            for y in 0..4 {
                for z in 0..4 {
                    assert_eq!(
                        left.get(&left_interner, IVec3::new(x, y, z)).unwrap_or(0),
                        0
                    );
                }
            }
        }

        let (mut right, mut right_interner) = empty_tree_and_interner();
        assert_eq!(
            realize_chunk(
                &registry,
                1,
                ChunkKey::new(1, 0, 0),
                &[winner],
                &mut right,
                &mut right_interner,
            ),
            Ok(1)
        );
        for x in 0..4 {
            for y in 0..4 {
                for z in 0..4 {
                    assert_eq!(right.get(&right_interner, IVec3::new(x, y, z)), Some(7));
                }
            }
        }
        for x in 4..8 {
            for y in 0..4 {
                for z in 0..4 {
                    assert_eq!(
                        right.get(&right_interner, IVec3::new(x, y, z)).unwrap_or(0),
                        0
                    );
                }
            }
        }
    }

    #[test]
    fn realize_chunk_skips_non_intersecting_winners() {
        let registry = test_registry();
        let (mut tree, mut interner) = empty_tree_and_interner();
        assert!(tree.set(&mut interner, IVec3::new(5, 5, 5), 3));
        let before = chunk_content_hash(&interner, &tree);

        // 完全落在 x=64 之后，与本块（0..32）不相交。
        let winner = record(BlobFeature::TYPE_ID, 0, 10, 0, [64, 0, 0], [68, 4, 4]);
        assert_eq!(
            realize_chunk(
                &registry,
                1,
                ChunkKey::new(0, 0, 0),
                &[winner],
                &mut tree,
                &mut interner,
            ),
            Ok(0)
        );
        assert_eq!(chunk_content_hash(&interner, &tree), before);
        assert_eq!(tree.get(&interner, IVec3::new(5, 5, 5)), Some(3));
    }

    #[test]
    fn realize_chunk_rejects_unknown_type() {
        let registry = test_registry();
        let (mut tree, mut interner) = empty_tree_and_interner();
        let before = chunk_content_hash(&interner, &tree);

        let winner = record(999, 0, 10, 0, [0, 0, 0], [2, 2, 2]);
        assert_eq!(
            realize_chunk(
                &registry,
                1,
                ChunkKey::new(0, 0, 0),
                &[winner],
                &mut tree,
                &mut interner,
            ),
            Err(FeatureError::UnknownType(999))
        );
        assert_eq!(chunk_content_hash(&interner, &tree), before);
        assert_eq!(
            FeatureError::UnknownType(999).to_string(),
            "裁决结果里的类型 ID 未注册：999"
        );
    }

    #[test]
    fn realize_chunk_is_deterministic() {
        let registry = test_registry();
        let winners = vec![
            record(BlobFeature::TYPE_ID, 0, 10, 0, [4, 4, 4], [6, 6, 6]),
            record(SeqValueFeature::TYPE_ID, 0, 10, 0, [8, 8, 8], [10, 10, 10]),
        ];

        let (mut tree, mut interner) = empty_tree_and_interner();
        assert_eq!(
            realize_chunk(
                &registry,
                7,
                ChunkKey::new(0, 0, 0),
                &winners,
                &mut tree,
                &mut interner
            ),
            Ok(2)
        );
        let first = chunk_content_hash(&interner, &tree);

        assert_eq!(
            realize_chunk(
                &registry,
                7,
                ChunkKey::new(0, 0, 0),
                &winners,
                &mut tree,
                &mut interner
            ),
            Ok(2)
        );
        assert_eq!(chunk_content_hash(&interner, &tree), first);

        let (mut fresh, mut fresh_interner) = empty_tree_and_interner();
        assert_eq!(
            realize_chunk(
                &registry,
                7,
                ChunkKey::new(0, 0, 0),
                &winners,
                &mut fresh,
                &mut fresh_interner
            ),
            Ok(2)
        );
        assert_eq!(chunk_content_hash(&fresh_interner, &fresh), first);
    }

    #[test]
    fn realize_chunk_orders_by_type_and_seq() {
        let registry = test_registry();

        // 类型顺序：21 先于 30，所以 30 的 9 覆盖 21 的 7。
        let blob = record(BlobFeature::TYPE_ID, 0, 10, 0, [0, 0, 0], [4, 4, 4]);
        let overlay = record(OverlayFeature::TYPE_ID, 0, 10, 0, [0, 0, 0], [4, 4, 4]);

        let (mut tree, mut interner) = empty_tree_and_interner();
        assert_eq!(
            realize_chunk(
                &registry,
                1,
                ChunkKey::new(0, 0, 0),
                &[overlay.clone(), blob.clone()],
                &mut tree,
                &mut interner,
            ),
            Ok(2)
        );
        for x in 0..4 {
            for y in 0..4 {
                for z in 0..4 {
                    assert_eq!(tree.get(&interner, IVec3::new(x, y, z)), Some(9));
                }
            }
        }
        let from_reversed = chunk_content_hash(&interner, &tree);

        let (mut other, mut other_interner) = empty_tree_and_interner();
        assert_eq!(
            realize_chunk(
                &registry,
                1,
                ChunkKey::new(0, 0, 0),
                &[blob, overlay],
                &mut other,
                &mut other_interner,
            ),
            Ok(2)
        );
        assert_eq!(chunk_content_hash(&other_interner, &other), from_reversed);

        // seq 顺序：同一个类型里 seq 大的后写，seq=1 的 2 覆盖 seq=0 的 1。
        let seq0 = record(SeqValueFeature::TYPE_ID, 0, 10, 0, [0, 0, 0], [2, 2, 2]);
        let seq1 = record(SeqValueFeature::TYPE_ID, 1, 10, 0, [0, 0, 0], [2, 2, 2]);

        let (mut seq_tree, mut seq_interner) = empty_tree_and_interner();
        assert_eq!(
            realize_chunk(
                &registry,
                1,
                ChunkKey::new(0, 0, 0),
                &[seq1, seq0],
                &mut seq_tree,
                &mut seq_interner,
            ),
            Ok(2)
        );
        for x in 0..2 {
            for y in 0..2 {
                for z in 0..2 {
                    assert_eq!(seq_tree.get(&seq_interner, IVec3::new(x, y, z)), Some(2));
                }
            }
        }
    }

    #[test]
    fn feature_plugin_init_resource() {
        let mut app = App::new();
        app.add_plugins(FeaturePlugin);

        let registry = app
            .world()
            .get_resource::<WorldFeatureRegistry>()
            .expect("FeaturePlugin 必须初始化 WorldFeatureRegistry");
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(registry.type_ids().is_empty());
    }

    #[test]
    fn register_world_feature_extension() {
        let mut app = App::new();
        app.register_world_feature::<TreeFeature>();
        {
            let registry = app.world().resource::<WorldFeatureRegistry>();
            assert_eq!(registry.len(), 1);
            assert!(registry.contains(TreeFeature::TYPE_ID));
            assert_eq!(registry.type_ids(), vec![TreeFeature::TYPE_ID]);
        }

        // 重复注册是配置错误：直接 panic，且不覆盖已注册的实例。
        let duplicate = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            app.register_world_feature::<TreeFeature>();
        }));
        assert!(duplicate.is_err(), "重复注册同一个 feature 必须 panic");
        assert_eq!(
            app.world().resource::<WorldFeatureRegistry>().len(),
            1,
            "panic 后注册表不能被破坏"
        );
    }
}
