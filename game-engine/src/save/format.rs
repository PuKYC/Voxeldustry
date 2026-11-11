//! 存档容器的线格式常量、内存类型与 §1.1 摘要算法。
//!
//! 这里只有“数据长什么样”，没有字节读写；编解码在 codec 模块。
//! 字段/偏移/宽度逐字节照《存档格式规格》§2（world.meta）与 §3（region 文件）。

use std::collections::BTreeMap;

use ::voxel::ChunkKey;

use crate::body_key::BodyKey;
use crate::modding::ParamRecord;

pub use crate::streaming::{ChunkRecord, EntityRecord};

/// world.meta 魔数：b"VXWORLD\0"（§2 偏移 0）。
pub const WORLD_MAGIC: [u8; 8] = *b"VXWORLD\0";
/// region 文件魔数：b"VXREGION"（§3 偏移 0）。
pub const REGION_MAGIC: [u8; 8] = *b"VXREGION";
/// 当前世界格式版本；未来只升不降（§2 偏移 8 / §6.1）。
///
/// v2 在 body 表之后追加「mod 清单 + 世界参数 + 动作表 + 插件清单」段；
/// v1 存档仍可读（扩展段解为空）；v2 缺任一扩展段按坏档拒绝。
pub const FORMAT_VERSION: u16 = 2;
/// 仍可读取的最低格式版本（§6.1 支持区间 `MIN_SUPPORTED..=CURRENT`）。
pub const MIN_SUPPORTED_FORMAT_VERSION: u16 = 1;
/// 组件负载编码；v1 = 1 = postcard 1.1.3（§2 偏移 12 / §4.3）。
pub const CODEC_ID: u16 = 1;
/// world.meta 头长（§2 偏移 10）。
pub const WORLD_HEADER_LEN: u16 = 48;
/// region 头长（§3 偏移 10）。
pub const REGION_HEADER_LEN: u16 = 32;
/// 一条 body 记录的字节数（§2）。
pub const BODY_RECORD_LEN: usize = 54;
/// 一条 region 索引项的字节数（§3）。
pub const REGION_INDEX_ENTRY_LEN: usize = 32;
/// 一条提案记录的字节数（§3.3）。
pub const PROPOSAL_RECORD_LEN: usize = 56;
/// 文件尾摘要长度（§0 / §1.1）。
pub const DIGEST_LEN: usize = 8;
/// body 表上限（§2 偏移 40）。
pub const MAX_BODY_COUNT: u32 = 65_536;
/// v2 mod 清单条数上限。
pub const MAX_MOD_COUNT: u32 = 4_096;
/// v2 世界参数条数上限。
pub const MAX_PARAM_COUNT: u32 = 4_096;
/// mod id 的 UTF-8 字节长度上限。
pub const MAX_MOD_ID_LEN: u16 = 255;
/// 参数全 key 的 UTF-8 字节长度上限。
pub const MAX_PARAM_KEY_LEN: u16 = 255;
/// v2 动作表条数上限。
pub const MAX_ACTION_COUNT: u32 = 4_096;
/// 动作名的 UTF-8 字节长度上限。
pub const MAX_ACTION_NAME_LEN: u16 = 64;
/// v2 插件清单条数上限（等于注册表规模上限，留足 mod 插件余量）。
pub const MAX_PLUGIN_COUNT: u32 = 64;
/// 插件 id 的 UTF-8 字节长度上限。
pub const MAX_PLUGIN_ID_LEN: u16 = 64;
/// 单 region 区块数上限（§3 偏移 12）。
pub const MAX_CHUNK_COUNT: u32 = 64;
/// 单条记录体长度上限（§0）。
pub const MAX_RECORD_LEN: u32 = 16 * 1024 * 1024;
/// region 文件长度上限（§0）。
pub const MAX_REGION_FILE_LEN: u64 = 64 * 1024 * 1024;
/// 单个组件负载长度上限（§4.1：payload_len ≤ 1 MiB）。
pub const MAX_COMPONENT_PAYLOAD_LEN: u32 = 1024 * 1024;
/// region 的边长（块数）：x/z 各 4 块，y 带 4 块（§1）。
pub const REGION_SIZE: i32 = 4;

/// §1.1 的 splitmix64。**唯一允许的哈希实现**，不与别的哈希混用。
#[must_use]
pub const fn splitmix64(x: u64) -> u64 {
    let mut x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// §1.1 的文件尾摘要：覆盖它之前的**全部字节**，按 8 字节小端分块。
///
/// 只做纠错，不防篡改（64 位非密码学）。
#[must_use]
pub fn digest(bytes: &[u8]) -> u64 {
    let mut h = 0x9E37_79B9_7F4A_7C15u64;
    for chunk in bytes.chunks(8) {
        let mut word = [0u8; 8];
        word[..chunk.len()].copy_from_slice(chunk);
        h = splitmix64(h ^ u64::from_le_bytes(word));
    }
    splitmix64(h ^ bytes.len() as u64)
}

/// region 键：逐轴 div_euclid(REGION_SIZE)，负坐标不是截断除法（§1）。
#[must_use]
pub fn region_of(key: ChunkKey) -> ChunkKey {
    ChunkKey::new(
        key.x.div_euclid(REGION_SIZE),
        key.y.div_euclid(REGION_SIZE),
        key.z.div_euclid(REGION_SIZE),
    )
}

/// body 的 kind_tag（§2：0 岛 / 1 船 / 2 结构 / 3 自动机）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum BodyKind {
    /// 岛：用 anchored + biome，通常按 Miss 生成。
    Island = 0,
    /// 船。
    Ship = 1,
    /// 结构。§2 说 attach_tag 非 2 只有结构可用；本层只校验枚举值与
    /// attach_tag = 1 的 attach_to 引用，不强制 kind 与 attach 的组合。
    Structure = 2,
    /// 自动机。
    Automaton = 3,
}

impl BodyKind {
    /// 线格式的 kind_tag。
    #[must_use]
    pub const fn tag(self) -> u8 {
        self as u8
    }

    /// 从 kind_tag 还原；未知值返回 None（调用方必须拒绝，不能跳过 body）。
    #[must_use]
    pub const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Island),
            1 => Some(Self::Ship),
            2 => Some(Self::Structure),
            3 => Some(Self::Automaton),
            _ => None,
        }
    }
}

/// body 的 attach_tag（§2：0 世界 / 1 body / 2 自由）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum AttachTag {
    /// 锚在世界。
    World = 0,
    /// 挂在另一个 body 上；此时 attach_to 必须非 0 且是表中的另一个 body。
    Body = 1,
    /// 自由。
    Free = 2,
}

impl AttachTag {
    /// 线格式的 attach_tag。
    #[must_use]
    pub const fn tag(self) -> u8 {
        self as u8
    }

    /// 从 attach_tag 还原；未知值返回 None（调用方必须拒绝）。
    #[must_use]
    pub const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::World),
            1 => Some(Self::Body),
            2 => Some(Self::Free),
            _ => None,
        }
    }
}

/// 一条 54 B 的 body 记录（§2）。
///
/// 线格式末尾的 reserved u16 不是内存字段，编码恒写 0、解码要求 0。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyRecord {
    /// body 的稳定 ID（= 表键）。
    pub id: BodyKey,
    /// kind_tag。
    pub kind: BodyKind,
    /// attach_tag。
    pub attach: AttachTag,
    /// 岛用；其余 kind 应为 0（§2 的“置 0”是写方归一化，本层不据此判损坏）。
    pub biome: u8,
    /// 岛用（0/1）；其余 kind 应为 0（同上，本层不做归一化）。
    pub anchored: u8,
    /// I40F24 位模式（to_bits），必须 > 0。
    pub voxel_size: i64,
    /// 漂移 I40F24 位模式（x/y/z）；非岛应为 0（同上，本层不做归一化）。
    pub drift: [i64; 3],
    /// 仅 attach = Body 时非 0，且必须是表中另一个 body（允许前向引用）。
    pub attach_to: BodyKey,
}

/// v2 mod 清单里的一条记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModRecord {
    /// mod 的稳定 id。
    pub id: String,
    /// mod 版本号；用于诊断，是否兼容由加载方决定。
    pub version: u32,
}

/// v2 动作表里的一条记录：动作名 → 动作 ID / 通道位的冻结映射。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionRecord {
    /// 动作名（全局唯一，按 name 严格升序写盘/读盘）。
    pub name: String,
    /// 动作 ID；核心为 1..=15，mod 为 1000..=9999。
    pub action: u32,
    /// 通道位（< MAX_INPUT_CHANNELS）。
    pub channel_bit: u32,
}

/// world.meta 的内存形态（§2）。
///
/// bodies 用 BTreeMap 保证按 BodyKey 严格升序写盘/读盘，不看任何 HashMap 顺序（§8）。
/// v2 的 mods / params 在加载时已按 id / key 排序，编码时仍会复查。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct WorldMeta {
    /// 世界种子（§2 偏移 16）。
    pub seed: u64,
    /// 快照时的游戏累计 tick（§2 偏移 24）。
    pub world_tick: u64,
    /// StableIdAllocator 的下一个 ID（§2 偏移 32）。
    pub next_stable_entity_id: u64,
    /// body 表：键必须等于 BodyRecord::id；最多 MAX_BODY_COUNT 条。
    pub bodies: BTreeMap<BodyKey, BodyRecord>,
    /// v2：本存档加载的 mod 清单，按 id 严格升序；v1 解码结果为空。
    pub mods: Vec<ModRecord>,
    /// v2：随存档保存的世界参数，按全 key 严格升序；v1 解码结果为空。
    pub params: Vec<ParamRecord>,
    /// v2：随存档冻结的动作表（含 mod 动作），按 name 严格升序；
    /// v1 解码结果为空，v2 缺该段即坏档。
    pub actions: Vec<ActionRecord>,
    /// v2：本世界装配过的插件 id，按**装配顺序**排列（不是字典序）。
    ///
    /// 顺序本身是数据：它是注册表确定性拓扑排序的结果，读档要逐项比对，
    /// 不能事后重算（重算会把「注册表变了」变成静默漂移）。v1 为空，
    /// v2 缺该段即坏档。
    pub plugins: Vec<String>,
}

impl WorldMeta {
    /// body 表条数（线格式 body_count）。
    #[must_use]
    pub fn body_count(&self) -> u32 {
        u32::try_from(self.bodies.len()).unwrap_or(u32::MAX)
    }
}

/// 一个 region 文件的内存形态（§3）。
///
/// 索引项里的 generator_version 存在每条 ChunkRecord 里；record_offset/record_len、
/// index_offset、max_stable_entity_id 都是编码时现算、解码时校验的派生值，不重复存字段。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct RegionFile {
    /// 本 region 的区块记录，按 ChunkKey 升序（BTreeMap 天然保证）。
    pub chunks: BTreeMap<ChunkKey, ChunkRecord>,
}

impl RegionFile {
    /// 区块数（线格式 chunk_count）。
    #[must_use]
    pub fn chunk_count(&self) -> u32 {
        u32::try_from(self.chunks.len()).unwrap_or(u32::MAX)
    }

    /// 本 region 实体最大 ID；无实体为 0（§3 偏移 24）。
    #[must_use]
    pub fn max_stable_entity_id(&self) -> u64 {
        self.chunks
            .values()
            .flat_map(|record| record.entities.iter())
            .map(|entity| entity.stable_entity_id)
            .max()
            .unwrap_or(0)
    }

    /// 是否没有区块记录（合法：chunk_count = 0 的空文件）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }
}

/// 一条组件记录（§4.1 的 ComponentRecord，不含外层 envelope）。
///
/// payload 是 codec_id = 1 的编码结果，本模块当**不透明字节**原样往返。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ComponentRecord {
    /// 稳定类型 ID；0 非法，升序且同实体不重复。
    pub stable_type_id: u32,
    /// postcard 负载（本轮不解析），长度 ≤ MAX_COMPONENT_PAYLOAD_LEN。
    pub payload: Vec<u8>,
}
