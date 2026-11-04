//! 流式加载的存档数据模型：区块记录、查询结果与 `ChunkStore` 契约。
//!
//! 字段与语义照《存档格式规格》§3（`ChunkRecord`/提案/实体）与 §7（`ChunkStore::lookup`），
//! 设计约束来自《体素流式_实现文档》§6 / §7。
//!
//! - 内存形态的字段顺序 = 存档规格 §3.1：`generator_version`、`content`、`proposals`、`entities`；
//! - **没有**文件格式版本字段：`format_version` 在 `world.meta` 里，块级只有
//!   `generator_version`（§6.2，地形版本，与 `format_version` 完全独立）；
//! - 输出顺序由 `(feature_type, seq)` / `stable_entity_id` 决定，容器用 `Vec`，
//!   写入方负责按规格排序；本模块不用 `HashMap`，判定路径没有浮点。
//!
//! 本文件只有类型，没有 IO：真正的字节编解码在存档层（`game-core/src/save`），
//! 这里只描述“查表该返回什么、记录里有什么”。
//!
//! 流式层只经 `ChunkPersistence` 这个 `Resource` 包装访问 `ChunkStore`，
//! 不直接持有 trait 对象；默认值 `NoStore` 保证脱离存档也能单独运行。

use bevy::prelude::Resource;
use serde::{Deserialize, Serialize};

use crate::body_key::BodyKey;
use ::voxel::ChunkKey;

/// 提案包围盒单轴跨度上限（体素）。
///
/// 《体素流式》§5 第 6 条 / 《存档格式规格》§3.3：允许溢出所属区块，
/// 但单轴跨度不得超过 3 个区块边长（3 × 32 = 96 体素）。
pub const MAX_PROPOSAL_EXTENT: i32 = 96;

/// 一条物体提案的存档形态（对应 §3.3 的 `ProposalRecord` 记录体）。
///
/// 只存 **owner 等于本块** 的提案；26 邻溢出提案加载时按 `(seed, key)` 重算，不落盘。
/// 权重（`priority` / `tie_break`）随提案一起存档：以后改公式，旧区块的相对顺序不变
/// （《体素流式》§5 第 7 条）。
///
/// 线格式里的 `reserved u16` 永远写 0，不是内存模型的字段，故不在这里建模。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalRecord {
    /// 稳定类型 ID（`WorldFeature::type_id`），不是 `type_name`。
    pub feature_type: u16,
    /// 归属区块 `(seed, owner)` 内的提案序号；与 `feature_type` 一起唯一确定提案。
    pub seq: u32,
    /// 裁决优先级：纯函数 `(种子, 提案)` 的结果，数值越大越优先。
    pub priority: i32,
    /// 平局键：`splitmix64` 顺序混合 `(seed, 类型 ID, 原点, 序号)` 的结果，禁止 `DefaultHasher`。
    pub tie_break: u64,
    /// 提案原点（体素坐标，整数）。
    pub origin: [i32; 3],
    /// 影响集包围盒下界（含，体素坐标）。
    pub bounds_min: [i32; 3],
    /// 影响集包围盒上界（不含，体素坐标）。
    pub bounds_max: [i32; 3],
}

impl ProposalRecord {
    /// 包围盒是否合法：逐轴 `min < max`，且单轴跨度不超过 `MAX_PROPOSAL_EXTENT`。
    ///
    /// 返回出错的轴号（`0` / `1` / `2`），全部合法返回 `None`。
    #[must_use]
    pub fn invalid_axis(&self) -> Option<usize> {
        // 用 i64 求跨度：读档数据不可信，`max - min` 在 i32 上会溢出。
        let limit = i64::from(MAX_PROPOSAL_EXTENT);
        for axis in 0..3 {
            let min = i64::from(self.bounds_min[axis]);
            let max = i64::from(self.bounds_max[axis]);
            if min >= max || max - min > limit {
                return Some(axis);
            }
        }
        None
    }
}

/// 一条实体记录的存档形态（对应 §4.1 的 `EntityRecord`）。
///
/// `payload` 是线格式里 `payload_len + payload` 的原字节（内含组件表），
/// 未注册的原型也原样保留、原样写回（§4.2），所以这里不解析组件结构。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityRecord {
    /// 稳定实体 ID（升序）。
    pub stable_entity_id: u64,
    /// 稳定原型 ID；`0` 非法。
    pub stable_type_id: u32,
    /// 原型负载原字节（含组件记录），长度即线格式的 `payload_len`。
    pub payload: Vec<u8>,
}

/// 一个区块的存档记录（内存形态），对应 region 文件里的一条记录。
///
/// 字段顺序与《存档格式规格》§3.1 的内存形态一致。
///
/// # `content` 的语义
///
/// `content` 是**整块最终内容**，不是“相对生成结果的差量”：
/// 就是 `voxel::io::export_tree::<u8>(&tree, &interner)` 的原始字节，逐字节原样存取。
/// 差量没有基线就还原不出来，而“有存档就不生成”和“存差量”不能同时成立；
/// 存整块内容之后两条都成立（《体素流式》§6）。`Some` 时生成器零调用。
///
/// # 组合合法性（§3.1）
///
/// - `content = Some` 时 `proposals` 必须为空（已确定块不再存提案）；
/// - `content` 空、提案空、实体空三者同时成立时不得写记录；
/// - 其余组合合法（包括“只有实体”：地形照常按当前版本生成）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRecord {
    /// 生成这个区块的地形生成器版本，**按块**记录（来自 region 索引项，不是文件格式版本）。
    ///
    /// 与 `format_version` 完全独立：它永远不导致“读不了档”，只影响提案能否复用（§6.2）。
    pub generator_version: u32,
    /// 整块体素内容（`export_tree::<u8>` 的原始字节），不是差量；见类型级说明。
    pub content: Option<Vec<u8>>,
    /// owner 等于本块的提案，按 `(feature_type, seq)` 升序；`content = Some` 时必须为空。
    pub proposals: Vec<ProposalRecord>,
    /// 归属本块的实体，按 `stable_entity_id` 升序。
    pub entities: Vec<EntityRecord>,
}

impl ChunkRecord {
    /// 是否有整块内容（`content = Some`）。
    ///
    /// 有内容的记录加载时直接装树、**不跑生成器**（§7）。
    #[must_use]
    pub fn has_content(&self) -> bool {
        self.content.is_some()
    }

    /// 记录是否“三者全空”。这种记录不得写入存档（§3.1）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.content.is_none() && self.proposals.is_empty() && self.entities.is_empty()
    }

    /// 校验组合合法性（§3.1）与提案包围盒（§3.3）。
    ///
    /// 返回 `Err` 表示这份记录不能写入存档；读档失败语义不在这里
    /// （那是存档层的 `SaveFailed`），本方法只判“记录本身是否自洽”。
    pub fn validate(&self) -> Result<(), RecordError> {
        if self.content.is_some() && !self.proposals.is_empty() {
            return Err(RecordError::ProposalsWithContent);
        }
        if self.is_empty() {
            return Err(RecordError::Empty);
        }
        for (index, proposal) in self.proposals.iter().enumerate() {
            if let Some(axis) = proposal.invalid_axis() {
                return Err(RecordError::InvalidBounds { index, axis });
            }
        }
        Ok(())
    }
}

/// `ChunkRecord::validate` 拒绝记录的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordError {
    /// `content` 与 `proposals` 同时存在：已确定块不再存提案。
    ProposalsWithContent,
    /// `content` / `proposals` / `entities` 三者全空：这种记录不得写入。
    Empty,
    /// 第 `index` 条提案的包围盒在第 `axis` 轴上不满足 `min < max` 或跨度超过 96。
    InvalidBounds {
        /// 提案在 `proposals` 里的下标。
        index: usize,
        /// 出错的轴（0/1/2）。
        axis: usize,
    },
}

/// `ChunkStore::lookup` 的查询结果（§7）。
///
/// 语义：
///
/// - `Hit` 且 `content = Some`：装树，直接标记 `Determined`，生成器零调用；
/// - `Hit` 但 `content = None`：照常提案/裁决/生成，再恢复实体；
/// - `Miss`：正常生成，没有存档实体；
/// - `Pending`：后台 IO 还没读完，留在队列下一 tick 再问，**不阻塞**其他区块、
///   不启动生成；超时转 `Miss`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreLookup {
    /// 命中：以存档为准（内容或实体）。
    Hit(ChunkRecord),
    /// 没有存档记录（或记录不可用），按生成器走。
    Miss,
    /// 还在后台读取；调用方下一 tick 再问。
    Pending,
}

impl StoreLookup {
    /// 是否还在等后台 IO。
    #[must_use]
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }

    /// 命中时借出记录，其余返回 `None`。
    #[must_use]
    pub fn record(&self) -> Option<&ChunkRecord> {
        match self {
            Self::Hit(record) => Some(record),
            Self::Miss | Self::Pending => None,
        }
    }
}

/// 读档回调：流式加载器在入队时问一次存档层（§7 / 《存档格式规格》§7）。
///
/// 实现方约束：`lookup` **只查表、不阻塞、不做 IO**，可以在生成循环里同步调用。
/// 后台读的结果通过内存缓冲返回；这一层只回答“现在有没有”。
pub trait ChunkStore: Send + Sync {
    /// 查询一个区块的存档记录。
    ///
    /// 优先级：内存未落盘缓冲 > `.recovered` > 主文件 > `Miss`；同一块的 `Miss`
    /// 在本会话内缓存到该块被写入为止。
    fn lookup(&self, body: BodyKey, key: ChunkKey) -> StoreLookup;
}

/// 默认的空实现：永远 `Miss`（流式可以脱离存档单独运行）。
#[derive(Clone, Copy, Debug, Default)]
pub struct NoStore;

impl ChunkStore for NoStore {
    fn lookup(&self, _body: BodyKey, _key: ChunkKey) -> StoreLookup {
        StoreLookup::Miss
    }
}

/// 读档查询的 Resource 包装：流式层只经它访问 `ChunkStore`。
#[derive(Resource)]
pub struct ChunkPersistence(pub Box<dyn ChunkStore>);

impl Default for ChunkPersistence {
    fn default() -> Self {
        Self(Box::new(NoStore))
    }
}

impl ChunkPersistence {
    /// 转发给内部 `ChunkStore`；语义与 `ChunkStore::lookup` 完全一致。
    #[must_use]
    pub fn lookup(&self, body: BodyKey, key: ChunkKey) -> StoreLookup {
        self.0.lookup(body, key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::StableEntityId;

    fn proposal(feature_type: u16, seq: u32, origin: [i32; 3]) -> ProposalRecord {
        ProposalRecord {
            feature_type,
            seq,
            priority: 100 - seq as i32,
            tie_break: 0x1234_5678_9ABC_DEF0,
            origin,
            bounds_min: [origin[0] - 2, origin[1], origin[2] - 2],
            bounds_max: [origin[0] + 2, origin[1] + 5, origin[2] + 2],
        }
    }

    fn entity(stable_entity_id: u64) -> EntityRecord {
        EntityRecord {
            stable_entity_id,
            stable_type_id: 3,
            payload: vec![2, 0, 0, 0, 7, 0, 0, 0, 0xAB],
        }
    }

    fn content_record() -> ChunkRecord {
        ChunkRecord {
            generator_version: 7,
            content: Some(b"VOXELIO\0\x01\x00\x02".to_vec()),
            proposals: Vec::new(),
            entities: vec![entity(42)],
        }
    }

    fn body() -> BodyKey {
        BodyKey::new(StableEntityId(7))
    }

    fn chunk() -> ChunkKey {
        ChunkKey::new(-1, 2, 3)
    }

    /// 整块内容（`content`）必须逐字节往返，不做任何重编码。
    #[test]
    fn chunk_record_serde_roundtrip_with_content() {
        let record = content_record();
        assert!(record.has_content());
        assert!(!record.is_empty());

        let json = serde_json::to_string(&record).expect("ChunkRecord 必须可序列化");
        let back: ChunkRecord = serde_json::from_str(&json).expect("ChunkRecord 必须可反序列化");
        assert_eq!(back, record);
        assert_eq!(back.content.as_deref(), record.content.as_deref());
        assert!(back.proposals.is_empty());
        assert_eq!(back.generator_version, 7);
    }

    /// 只有提案 / 只有实体的记录也要能往返（`content = None` 时照常生成地形）。
    #[test]
    fn chunk_record_serde_roundtrip_with_proposals_and_entities() {
        let record = ChunkRecord {
            generator_version: 11,
            content: None,
            proposals: vec![proposal(2, 0, [10, 20, 30]), proposal(10, 3, [-5, 0, 7])],
            entities: vec![entity(1), entity(9)],
        };
        assert!(!record.has_content());
        assert!(!record.is_empty());
        assert_eq!(record.validate(), Ok(()));

        let json = serde_json::to_string(&record).expect("ChunkRecord 必须可序列化");
        let back: ChunkRecord = serde_json::from_str(&json).expect("ChunkRecord 必须可反序列化");
        assert_eq!(back, record);
        assert_eq!(back.proposals[0], proposal(2, 0, [10, 20, 30]));
        assert_eq!(back.entities[1].stable_entity_id, 9);
    }

    /// 字段顺序与《存档格式规格》§3.1 的内存形态一致（serde 按声明顺序输出）。
    #[test]
    fn chunk_record_json_field_order_matches_spec() {
        let json = serde_json::to_string(&content_record()).expect("必须可序列化");
        let names = [
            "\"generator_version\"",
            "\"content\"",
            "\"proposals\"",
            "\"entities\"",
        ];
        let positions: Vec<usize> = names
            .iter()
            .map(|name| {
                json.find(name)
                    .unwrap_or_else(|| panic!("缺少字段 {name}: {json}"))
            })
            .collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "字段顺序必须是 generator_version/content/proposals/entities: {json}"
        );
        assert!(json.starts_with("{\"generator_version\":"));

        // 提案字段顺序同样照 §3.3。
        let record = ChunkRecord {
            generator_version: 1,
            content: None,
            proposals: vec![proposal(2, 0, [0, 0, 0])],
            entities: Vec::new(),
        };
        let json = serde_json::to_string(&record).expect("必须可序列化");
        let names = [
            "\"feature_type\"",
            "\"seq\"",
            "\"priority\"",
            "\"tie_break\"",
            "\"origin\"",
            "\"bounds_min\"",
            "\"bounds_max\"",
        ];
        let positions: Vec<usize> = names
            .iter()
            .map(|name| {
                json.find(name)
                    .unwrap_or_else(|| panic!("缺少字段 {name}: {json}"))
            })
            .collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "提案字段顺序不符: {json}"
        );
    }

    /// 其余组合合法：只有内容 / 只有提案 / 只有实体。
    #[test]
    fn validate_accepts_legal_combinations() {
        let content_only = ChunkRecord {
            generator_version: 1,
            content: Some(vec![1, 2, 3]),
            proposals: Vec::new(),
            entities: Vec::new(),
        };
        assert_eq!(content_only.validate(), Ok(()));

        let proposals_only = ChunkRecord {
            generator_version: 1,
            content: None,
            proposals: vec![proposal(1, 0, [0, 0, 0])],
            entities: Vec::new(),
        };
        assert_eq!(proposals_only.validate(), Ok(()));

        let entities_only = ChunkRecord {
            generator_version: 1,
            content: None,
            proposals: Vec::new(),
            entities: vec![entity(5)],
        };
        assert_eq!(entities_only.validate(), Ok(()));
    }

    /// `content = Some` 时不得再存提案（已确定块不再存提案）。
    #[test]
    fn validate_rejects_proposals_with_content() {
        let record = ChunkRecord {
            generator_version: 1,
            content: Some(vec![1]),
            proposals: vec![proposal(1, 0, [0, 0, 0])],
            entities: Vec::new(),
        };
        assert_eq!(record.validate(), Err(RecordError::ProposalsWithContent));
    }

    /// 三者全空的记录不得写入。
    #[test]
    fn validate_rejects_empty_record() {
        let record = ChunkRecord {
            generator_version: 1,
            content: None,
            proposals: Vec::new(),
            entities: Vec::new(),
        };
        assert!(record.is_empty());
        assert_eq!(record.validate(), Err(RecordError::Empty));
    }

    /// 提案包围盒：逐轴 `min < max`，单轴跨度 ≤ 96。
    #[test]
    fn validate_rejects_bad_proposal_bounds() {
        // min == max（第 1 轴）。
        let mut bad = proposal(1, 0, [0, 0, 0]);
        bad.bounds_max[1] = bad.bounds_min[1];
        assert_eq!(bad.invalid_axis(), Some(1));

        // 跨度为 0 的另一侧：min > max（第 0 轴）。
        let mut inverted = proposal(1, 0, [0, 0, 0]);
        inverted.bounds_min[0] = inverted.bounds_max[0] + 1;
        assert_eq!(inverted.invalid_axis(), Some(0));

        // 跨度超过 96（第 2 轴）。
        let mut too_wide = proposal(1, 0, [0, 0, 0]);
        too_wide.bounds_max[2] = too_wide.bounds_min[2] + MAX_PROPOSAL_EXTENT + 1;
        assert_eq!(too_wide.invalid_axis(), Some(2));

        // 恰好 96 是合法的。
        let mut at_limit = proposal(1, 0, [0, 0, 0]);
        at_limit.bounds_max[0] = at_limit.bounds_min[0] + MAX_PROPOSAL_EXTENT;
        assert_eq!(at_limit.invalid_axis(), None);

        // 极端值不能溢出（读档数据不可信）。
        let mut extreme = proposal(1, 0, [0, 0, 0]);
        extreme.bounds_min[1] = i32::MIN;
        extreme.bounds_max[1] = i32::MAX;
        assert_eq!(extreme.invalid_axis(), Some(1));

        let record = ChunkRecord {
            generator_version: 1,
            content: None,
            proposals: vec![proposal(1, 0, [0, 0, 0]), inverted],
            entities: Vec::new(),
        };
        assert_eq!(
            record.validate(),
            Err(RecordError::InvalidBounds { index: 1, axis: 0 })
        );
    }

    /// 默认空实现永远 `Miss`；`StoreLookup` 的判据只对 `Hit` / `Pending` 成立。
    #[test]
    fn no_store_always_misses() {
        assert_eq!(NoStore.lookup(body(), chunk()), StoreLookup::Miss);

        let mut lookup = StoreLookup::Miss;
        assert!(!lookup.is_pending());
        assert!(lookup.record().is_none());

        lookup = StoreLookup::Pending;
        assert!(lookup.is_pending());
        assert!(lookup.record().is_none());

        lookup = StoreLookup::Hit(content_record());
        assert!(!lookup.is_pending());
        assert_eq!(lookup.record(), Some(&content_record()));
    }

    /// 默认 `ChunkPersistence` 包装 `NoStore`，所以永远查不到存档。
    #[test]
    fn chunk_persistence_defaults_to_miss() {
        let persistence = ChunkPersistence::default();
        assert_eq!(persistence.lookup(body(), chunk()), StoreLookup::Miss);
        assert_eq!(
            persistence.lookup(body(), ChunkKey::new(0, 0, 0)),
            StoreLookup::Miss
        );
    }

    /// 包装层必须原样转发给内部 store，记录的每个字段都要一致。
    #[test]
    fn chunk_persistence_forwards_to_inner_store() {
        /// 测试用 store：不论查询哪个块都返回同一条记录。
        struct FixedStore(ChunkRecord);

        impl ChunkStore for FixedStore {
            fn lookup(&self, _body: BodyKey, _key: ChunkKey) -> StoreLookup {
                StoreLookup::Hit(self.0.clone())
            }
        }

        let record = content_record();
        let persistence = ChunkPersistence(Box::new(FixedStore(record.clone())));
        let found = persistence.lookup(body(), chunk());
        assert_eq!(found, StoreLookup::Hit(record.clone()));
        let found = found.record().expect("命中时必须有记录");
        assert_eq!(found.generator_version, record.generator_version);
        assert_eq!(found.content, record.content);
        assert_eq!(found.proposals, record.proposals);
        assert_eq!(found.entities, record.entities);
    }
}
