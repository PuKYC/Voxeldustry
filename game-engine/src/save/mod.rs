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
    digest, region_of, splitmix64, ActionRecord, AttachTag, BodyKind, BodyRecord, ComponentRecord,
    ModRecord, RegionFile, SavedWith, WorldMeta, BODY_RECORD_LEN, CODEC_ID, DIGEST_LEN,
    FORMAT_VERSION, MAX_ACTION_COUNT, MAX_ACTION_NAME_LEN, MAX_BODY_COUNT, MAX_CHUNK_COUNT,
    MAX_COMPONENT_PAYLOAD_LEN, MAX_GAME_VERSION_LEN, MAX_RECORD_LEN, MAX_REGION_FILE_LEN,
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

/// SaveError 的失败类别。除诊断文本（Payload / Param）外只含整数/静态串，方便断言与日志。
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
    /// 字符串字段不是合法 UTF-8。
    InvalidString { what: &'static str },
    /// 插件参数与当前 schema 不一致（加载时由 ParamTable 校验）。
    Param { detail: String },
    /// 存档加载过的 mod 在当前装配里缺失。
    MissingMod { id: String },
    /// 当前装配里有存档没记录过的 mod。
    UnexpectedMod { id: String },
    /// 同名 mod 的版本号与存档不一致。
    ModVersionMismatch {
        id: String,
        saved: u32,
        current: u32,
    },
    /// 动作表某条记录（name / action / channel_bit）与存档冻结的表不一致。
    ActionTableMismatch { name: String },
    /// 动作表条数与存档不一致。
    ActionCountMismatch { expected: usize, found: usize },
    /// 存档记录过该插件，当前注册表里没有（删名/改名）。
    MissingPlugin { id: String },
    /// 当前装配里有存档没记录过的插件（新增 default_on 插件）。
    UnexpectedPlugin { id: String },
    /// 插件集合一致，但装配顺序与存档不同（tie-break 变了）。
    PluginOrderMismatch { saved: String, current: String },
    /// 存档里带有该段数据，但世界缺少对应的运行期资源。
    MissingResource { what: &'static str },
}

impl SaveErrorKind {
    /// 稳定错误码（对 Godot / 日志 / 测试的公开契约）。
    ///
    /// **文案可改，code 不可改**：客户端按 code 分支（比如 `save.*` 弹差异面板），
    /// 用户能看到的说明在 [`fmt::Display`] 里。每个变体一个类别，不返回
    /// `save.unknown` —— 否则客户端没法区分「坏字节」与「世界身份不符」。
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Truncated { .. } => "save.truncated",
            Self::TrailingBytes { .. } => "save.trailing_bytes",
            Self::BadMagic { .. } => "save.bad_magic",
            Self::Unsupported { .. } => "save.unsupported",
            Self::NonZeroReserved { .. } => "save.non_zero_reserved",
            Self::InvalidValue { .. } => "save.invalid_value",
            Self::TooLarge { .. } => "save.too_large",
            Self::NotStrictlyIncreasing { .. } => "save.not_strictly_increasing",
            Self::Duplicate { .. } => "save.duplicate",
            Self::ChunkKeyOrder { .. } => "save.chunk_key_order",
            Self::Missing { .. } => "save.missing",
            Self::Inconsistent { .. } => "save.inconsistent",
            Self::InvalidCombination { .. } => "save.invalid_combination",
            Self::InvalidBounds { .. } => "save.invalid_bounds",
            Self::DigestMismatch { .. } => "save.digest_mismatch",
            Self::BodyKeyMismatch { .. } => "save.body_key_mismatch",
            Self::Io { .. } => "save.io",
            Self::Payload { .. } => "save.payload",
            Self::InvalidString { .. } => "save.invalid_string",
            Self::Param { .. } => "save.param_mismatch",
            Self::MissingMod { .. } => "save.missing_mod",
            Self::UnexpectedMod { .. } => "save.unexpected_mod",
            Self::ModVersionMismatch { .. } => "save.mod_version_mismatch",
            Self::ActionTableMismatch { .. } => "save.action_table_mismatch",
            Self::ActionCountMismatch { .. } => "save.action_count_mismatch",
            Self::MissingPlugin { .. } => "save.missing_plugin",
            Self::UnexpectedPlugin { .. } => "save.unexpected_plugin",
            Self::PluginOrderMismatch { .. } => "save.plugin_order_mismatch",
            Self::MissingResource { .. } => "save.missing_resource",
        }
    }

    /// 这是不是「世界身份不一致」导致的拒绝（mod / 插件 / 动作表 / 参数 / 版本）。
    ///
    /// 为 `true` 时**不能降级建新世界**：档本身是好的，只是与当前装配不符，
    /// 建新世界再自动存档会直接把玩家的档覆盖掉。为 `false`（损坏 / 截断 /
    /// 多字节 / 坏魔数 / IO / 摘要 / 版本号不认识）时保持原有降级路径 ——
    /// 这类档玩家只能靠删档恢复，当成身份不符只会让他永远进不去。
    #[must_use]
    pub fn is_world_rejection(&self) -> bool {
        matches!(
            self,
            Self::MissingMod { .. }
                | Self::UnexpectedMod { .. }
                | Self::ModVersionMismatch { .. }
                | Self::ActionTableMismatch { .. }
                | Self::ActionCountMismatch { .. }
                | Self::MissingPlugin { .. }
                | Self::UnexpectedPlugin { .. }
                | Self::PluginOrderMismatch { .. }
                | Self::MissingResource { .. }
                | Self::Param { .. }
                | Self::Unsupported { .. }
        )
    }
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
            Self::InvalidString { what } => write!(f, "invalid UTF-8 in {what}"),
            Self::Param { detail } => write!(f, "plugin parameter mismatch: {detail}"),
            Self::MissingMod { id } => {
                write!(f, "存档加载过 mod \"{id}\"，当前装配里没有")
            }
            Self::UnexpectedMod { id } => {
                write!(f, "当前装配里有 mod \"{id}\"，存档没有记录")
            }
            Self::ModVersionMismatch { id, saved, current } => {
                write!(f, "mod \"{id}\" 版本不一致：存档 {saved}，当前 {current}")
            }
            Self::ActionTableMismatch { name } => {
                write!(f, "动作 \"{name}\" 的动作表记录与存档不一致")
            }
            Self::ActionCountMismatch { expected, found } => {
                write!(f, "动作表条数不一致：存档 {expected}，当前 {found}")
            }
            Self::MissingPlugin { id } => {
                write!(f, "存档装配过插件 \"{id}\"，当前注册表里没有")
            }
            Self::UnexpectedPlugin { id } => {
                write!(f, "当前装配里有插件 \"{id}\"，存档没有记录")
            }
            Self::PluginOrderMismatch { saved, current } => {
                write!(
                    f,
                    "插件装配顺序与存档不一致：存档 \"{saved}\"，当前 \"{current}\""
                )
            }
            Self::MissingResource { what } => {
                write!(f, "世界缺少运行期资源 {what}（存档带有对应数据）")
            }
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

#[cfg(test)]
mod tests {
    use super::SaveErrorKind;

    /// 每个变体一个可区分类别；`save.unknown` 会让客户端没法分支。
    #[test]
    fn every_kind_has_a_distinct_code() {
        let kinds = [
            SaveErrorKind::Truncated { need: 1, have: 0 },
            SaveErrorKind::TrailingBytes { extra: 1 },
            SaveErrorKind::BadMagic {
                expected: "ECS2",
                found: *b"BADMAGIC",
            },
            SaveErrorKind::Unsupported {
                what: "version",
                value: 3,
            },
            SaveErrorKind::NonZeroReserved { value: 1 },
            SaveErrorKind::InvalidValue {
                what: "voxel_size",
                value: 0,
            },
            SaveErrorKind::TooLarge {
                what: "count",
                value: 9,
                max: 1,
            },
            SaveErrorKind::NotStrictlyIncreasing {
                what: "body",
                previous: 2,
                current: 1,
            },
            SaveErrorKind::Duplicate {
                what: "body",
                id: 7,
            },
            SaveErrorKind::ChunkKeyOrder {
                previous: [1, 0, 0],
                current: [0, 0, 0],
            },
            SaveErrorKind::Missing {
                what: "body",
                id: 3,
            },
            SaveErrorKind::Inconsistent {
                what: "header_len",
                expected: 1,
                found: 2,
            },
            SaveErrorKind::InvalidCombination {
                what: "content+proposal",
            },
            SaveErrorKind::InvalidBounds { index: 0, axis: 1 },
            SaveErrorKind::DigestMismatch {
                stored: 1,
                computed: 2,
            },
            SaveErrorKind::BodyKeyMismatch { map: 1, record: 2 },
            SaveErrorKind::Io {
                stage: "read",
                kind: std::io::ErrorKind::NotFound,
                raw: Some(2),
            },
            SaveErrorKind::Payload {
                what: "decode",
                detail: "bad".to_string(),
            },
            SaveErrorKind::InvalidString { what: "plugin id" },
            SaveErrorKind::Param {
                detail: "radius".to_string(),
            },
            SaveErrorKind::MissingMod {
                id: "core".to_string(),
            },
            SaveErrorKind::UnexpectedMod {
                id: "core".to_string(),
            },
            SaveErrorKind::ModVersionMismatch {
                id: "core".to_string(),
                saved: 1,
                current: 2,
            },
            SaveErrorKind::ActionTableMismatch {
                name: "jump".to_string(),
            },
            SaveErrorKind::ActionCountMismatch {
                expected: 1,
                found: 2,
            },
            SaveErrorKind::MissingPlugin {
                id: "p".to_string(),
            },
            SaveErrorKind::UnexpectedPlugin {
                id: "p".to_string(),
            },
            SaveErrorKind::PluginOrderMismatch {
                saved: "a,b".to_string(),
                current: "b,a".to_string(),
            },
            SaveErrorKind::MissingResource { what: "ParamTable" },
        ];

        let mut codes: Vec<&'static str> = kinds.iter().map(SaveErrorKind::code).collect();
        codes.sort_unstable();
        let unique = {
            let mut deduped = codes.clone();
            deduped.dedup();
            deduped
        };
        assert_eq!(codes.len(), unique.len(), "错误码必须互不重复：{codes:?}");
        assert!(
            codes.iter().all(|code| code.starts_with("save.")),
            "错误码统一用 save. 前缀：{codes:?}"
        );
        assert!(
            codes.iter().all(|code| *code != "save.unknown"),
            "不允许 save.unknown：类别必须能区分"
        );
    }

    /// 世界身份不一致 = 拒绝；坏字节 / IO / 摘要 = 仍可降级建新世界。
    #[test]
    fn only_identity_mismatch_is_a_world_rejection() {
        let rejection = [
            SaveErrorKind::MissingMod {
                id: "m".to_string(),
            },
            SaveErrorKind::UnexpectedMod {
                id: "m".to_string(),
            },
            SaveErrorKind::ModVersionMismatch {
                id: "m".to_string(),
                saved: 1,
                current: 2,
            },
            SaveErrorKind::ActionTableMismatch {
                name: "jump".to_string(),
            },
            SaveErrorKind::ActionCountMismatch {
                expected: 1,
                found: 2,
            },
            SaveErrorKind::MissingPlugin {
                id: "p".to_string(),
            },
            SaveErrorKind::UnexpectedPlugin {
                id: "p".to_string(),
            },
            SaveErrorKind::PluginOrderMismatch {
                saved: "a".to_string(),
                current: "b".to_string(),
            },
            SaveErrorKind::MissingResource { what: "ParamTable" },
            SaveErrorKind::Param {
                detail: "radius".to_string(),
            },
            SaveErrorKind::Unsupported {
                what: "format_version",
                value: 3,
            },
        ];
        for kind in &rejection {
            assert!(kind.is_world_rejection(), "{kind:?} 应判定为世界拒绝");
        }

        let degradation = [
            SaveErrorKind::Truncated { need: 8, have: 3 },
            SaveErrorKind::TrailingBytes { extra: 1 },
            SaveErrorKind::BadMagic {
                expected: "ECS2",
                found: *b"BADMAGIC",
            },
            SaveErrorKind::DigestMismatch {
                stored: 1,
                computed: 2,
            },
            SaveErrorKind::Io {
                stage: "open",
                kind: std::io::ErrorKind::PermissionDenied,
                raw: Some(13),
            },
            SaveErrorKind::Payload {
                what: "decode",
                detail: "bad".to_string(),
            },
            SaveErrorKind::InvalidString { what: "plugin id" },
        ];
        for kind in &degradation {
            assert!(!kind.is_world_rejection(), "{kind:?} 不应判定为世界拒绝");
        }
    }

    /// Display 是玩家可见文案，code 是机器契约：两者必须同时可用。
    #[test]
    fn display_and_code_are_independent() {
        let error = super::SaveError::semantic(
            "load.meta.mods",
            SaveErrorKind::MissingMod {
                id: "core".to_string(),
            },
        );
        assert_eq!(error.path(), "load.meta.mods");
        assert_eq!(error.kind().code(), "save.missing_mod");
        assert!(
            error.to_string().contains("存档加载过 mod"),
            "Display 的中文原因不能被 code 取代：{error}"
        );
    }
}
