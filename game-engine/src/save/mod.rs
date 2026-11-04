//! 存档容器编解码（纯函数，无 IO）。
//!
//! 权威文档：docs/待办/实现文档/存档格式规格.md（2026-11-01，format_version = 1）。
//!
//! 本模块只做**字节容器**：
//!
//! - world.meta：48 B 头 + body_count × 54 B body 记录 + 8 B 尾摘要（§2）；
//! - <body>/region_x_y_z：32 B 头 + 记录区 + chunk_count × 32 B 索引 + 8 B 尾摘要（§3）；
//! - 记录体：ChunkRecord（复用 crate::streaming::ChunkRecord）的线格式（§3.1–§3.3、§4）；
//! - 摘要：§1.1 的 splitmix64 / digest，逐字节照抄，不用 DefaultHasher。
//!
//! 文件 IO / 原子写 / fsync 在 `store`（§1 目录命名、§5 崩溃一致性），本模块只管字节。
//!
//! **范围外**（下一步）：postcard 组件负载编解码（codec_id = 1 只做外层框）、
//! SavePlugin、ChunkStore 实现、与 streaming::ChunkStore 的接线。
//! 组件负载在这里是**不透明字节**，只做 u32 len + bytes 原样往返。
//!
//! # 确定性（§8）
//!
//! 一切都是字节函数：encode(...) -> Result<Vec<u8>, SaveError> 只由入参决定输出，
//! 与调用顺序 / 哈希种子 / 线程数无关。body 用 BTreeMap<BodyKey, _>，chunk 用
//! BTreeMap<ChunkKey, _>，提案与实体在编码前按规格键重新排序（重复即报错），
//! 所以同一组记录无论以什么 Vec 顺序传入，字节都相同。
//!
//! # 校验失败
//!
//! 所有 §2/§3/§4 的“必须 / 拒绝 / 判损坏”都返回带路径与字节偏移的 SaveError：
//! 未知 kind_tag / attach_tag / codec_id、非 0 保留字段、计数超限、id 非升序/重复、
//! attach_to 悬空、voxel_size <= 0、版本/头长不符、长度不足/多余、摘要不符、
//! 索引越界/重叠/不连续，**未知一律拒绝、不跳过**。
//!
//! 解码顺序是“先结构、后摘要”：字段级损坏会先报到具体字段；只有结构完全合法但
//! 摘要不符时才报 digest。这样破坏性测试能指到被改的字段。

mod codec;
mod content;
mod format;
mod fs_atomic;
pub mod persist;
pub mod plugin;
mod snapshot;
pub mod store;

pub use codec::{
    decode_chunk_record, decode_component_table, decode_region, decode_world_meta,
    encode_chunk_record, encode_component_table, encode_region, encode_world_meta,
};
pub use content::{export_chunk, install_chunk};
pub use format::{
    digest, region_of, splitmix64, AttachTag, BodyKind, BodyRecord, ComponentRecord, RegionFile,
    WorldMeta, BODY_RECORD_LEN, CODEC_ID, DIGEST_LEN, FORMAT_VERSION, MAX_BODY_COUNT,
    MAX_CHUNK_COUNT, MAX_COMPONENT_PAYLOAD_LEN, MAX_RECORD_LEN, MAX_REGION_FILE_LEN,
    PROPOSAL_RECORD_LEN, REGION_HEADER_LEN, REGION_INDEX_ENTRY_LEN, REGION_MAGIC, REGION_SIZE,
    WORLD_HEADER_LEN, WORLD_MAGIC,
};

/// ChunkRecord 与 EntityRecord 直接复用流式模块的定义，不在这里重新定义（§3.1/§4.1）。
pub use crate::streaming::{ChunkRecord, EntityRecord};

/// 存档接线：后台 IO 线程 + `ChunkStore` 实现 + 每 tick 系统。
pub use plugin::{
    PollReport, QuiesceError, SaveConfig, SaveFailed, SavePlugin, SaveSet, SaveStats, SaveStore,
    SaveSucceeded, POLL_APPLY_BUDGET,
};

/// 组件负载编解码与类型注册表（§4）：postcard 1.1.3 + `StableTypeId`。
pub use persist::{
    ApplyReport, OpaqueComponents, PersistComponent, PersistPlugin, PersistRegistry, StableTypeId,
};

pub use snapshot::{capture, EntitySnapshot, Persist};

/// 文件 IO 层（§1 目录命名、§5 原子写与崩溃一致性）。
pub use store::{
    orphan_bodies, read_region, read_world_meta, reclaim_body, reclaim_orphans, region_origin,
    regions_on_disk, write_atomic, write_region, write_world_meta, ReclaimStats, SavePaths,
};

use std::fmt;

/// 容器编解码失败，带**路径**（字段定位）与**字节偏移**。
///
/// 例：world.meta body[0].kind_tag at byte 56: invalid kind_tag 9。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaveError {
    path: String,
    offset: usize,
    kind: SaveErrorKind,
}

impl SaveError {
    /// 构造一个错误；path 是字段全路径（如 region.index[2].record_len）。
    pub(crate) fn new(path: impl Into<String>, offset: usize, kind: SaveErrorKind) -> Self {
        Self {
            path: path.into(),
            offset,
            kind,
        }
    }

    /// 构造一个**语义层**错误：不落在具体字节偏移上（offset 记 0）。
    ///
    /// 采集 / 落盘编排（如 game-core 的 `world::save`）拿不到私有字段，也不该
    /// 伪造字节偏移；统一用本入口报告「世界里缺了什么 / 哪里自相矛盾」。
    /// 路径用字段全路径或逻辑位置（如 `capture.body[4v1]`）。
    #[must_use]
    pub fn semantic(path: impl Into<String>, kind: SaveErrorKind) -> Self {
        Self::new(path, 0, kind)
    }

    /// 字段全路径（容器 + 记录下标 + 字段名）。
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// 出错字段在文件里的起始字节偏移。
    ///
    /// 编码路径下是“这个字段本来会写到的偏移”；语义校验（如 attach_to 悬空）
    /// 可能报记录起始偏移。
    #[must_use]
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// 失败类别。
    #[must_use]
    pub fn kind(&self) -> &SaveErrorKind {
        &self.kind
    }
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}: {}", self.path, self.offset, self.kind)
    }
}

impl std::error::Error for SaveError {}

/// SaveError 的失败类别。所有变体都只含整数/静态串，方便断言与日志。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveErrorKind {
    /// 字节不够：需要 need，实际只剩 have。
    Truncated { need: usize, have: usize },
    /// 文件尾有多余字节。
    TrailingBytes { extra: usize },
    /// 魔数不符。
    BadMagic {
        expected: &'static str,
        found: [u8; 8],
    },
    /// 版本 / 未知枚举 / 未知标志位等“不认识就拒绝”的值。
    Unsupported { what: &'static str, value: u64 },
    /// 保留字段必须为 0。
    NonZeroReserved { value: u64 },
    /// 字段值非法（voxel_size <= 0、0 类型 ID、悬空坐标等）。
    InvalidValue { what: &'static str, value: i64 },
    /// 计数 / 长度超过上限。
    TooLarge {
        what: &'static str,
        value: u64,
        max: u64,
    },
    /// 要求严格升序的键不升序（或重复）。
    NotStrictlyIncreasing {
        what: &'static str,
        previous: u64,
        current: u64,
    },
    /// 重复键。
    Duplicate { what: &'static str, id: u64 },
    /// region 索引里的 ChunkKey 重复或非升序（§3：按 ChunkKey 升序）。
    ChunkKeyOrder {
        previous: [i32; 3],
        current: [i32; 3],
    },
    /// 引用不存在（如 attach_to 不在 body 表）。
    Missing { what: &'static str, id: u64 },
    /// 两个本应相等的值不一致（header_len、index_offset、max_stable_entity_id 等）。
    Inconsistent {
        what: &'static str,
        expected: u64,
        found: u64,
    },
    /// 记录组合非法（§3.1：content 与提案互斥、三者全空不得写）。
    InvalidCombination { what: &'static str },
    /// 提案包围盒非法（§3.3）。
    InvalidBounds { index: usize, axis: usize },
    /// 尾摘要不符（只做纠错，不防篡改）。
    DigestMismatch { stored: u64, computed: u64 },
    /// body 表 map 键与记录内的 id 不一致（编码侧防呆）。
    BodyKeyMismatch { map: u64, record: u64 },
    /// 文件 IO 失败（store 层）：阶段 + 错误类别 + 原始 errno。
    ///
    /// 路径在 [`Self::path`] 里；阶段是 `open` / `read` / `write` / `sync` / `rename` /
    /// `copy` / `mkdir` / `remove` / `quarantine`（§5 失败矩阵的落点）。
    /// 保留 `ErrorKind` 与 errno，磁盘满（ENOSPC）之类的失败不会被抹平。
    Io {
        stage: &'static str,
        kind: std::io::ErrorKind,
        raw: Option<i32>,
    },
    /// 组件负载（codec_id = 1 的 postcard）编解码失败（§4.3）。
    ///
    /// `what` 是阶段（postcard encode / decode），`detail` 是原始错误文本。
    Payload { what: &'static str, detail: String },
}

impl fmt::Display for SaveErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { need, have } => {
                write!(f, "truncated: need {need} bytes, have {have}")
            }
            Self::TrailingBytes { extra } => write!(f, "{extra} trailing byte(s)"),
            Self::BadMagic { expected, found } => {
                write!(f, "bad magic {found:02X?}, expected {expected:?}")
            }
            Self::Unsupported { what, value } => write!(f, "unsupported {what} {value}"),
            Self::NonZeroReserved { value } => write!(f, "reserved must be 0, found {value}"),
            Self::InvalidValue { what, value } => write!(f, "invalid {what} {value}"),
            Self::TooLarge { what, value, max } => {
                write!(f, "{what} {value} exceeds limit {max}")
            }
            Self::NotStrictlyIncreasing {
                what,
                previous,
                current,
            } => write!(
                f,
                "{what} not strictly increasing: {previous} then {current}"
            ),
            Self::Duplicate { what, id } => write!(f, "duplicate {what} {id}"),
            Self::ChunkKeyOrder { previous, current } => {
                write!(
                    f,
                    "ChunkKey not strictly increasing: {previous:?} then {current:?}"
                )
            }
            Self::Missing { what, id } => write!(f, "missing {what} {id}"),
            Self::Inconsistent {
                what,
                expected,
                found,
            } => write!(f, "{what} mismatch: expected {expected}, found {found}"),
            Self::InvalidCombination { what } => {
                write!(f, "invalid record combination: {what}")
            }
            Self::InvalidBounds { index, axis } => {
                write!(f, "proposal {index} has invalid bounds on axis {axis}")
            }
            Self::DigestMismatch { stored, computed } => {
                write!(
                    f,
                    "digest mismatch: stored {stored:016X}, computed {computed:016X}"
                )
            }
            Self::Payload { what, detail } => write!(f, "{what} failed: {detail}"),
            Self::BodyKeyMismatch { map, record } => {
                write!(f, "body map key {map} != record id {record}")
            }
            Self::Io { stage, kind, raw } => match raw {
                Some(code) => write!(f, "io {stage} failed: {kind} (os error {code})"),
                None => write!(f, "io {stage} failed: {kind}"),
            },
        }
    }
}
