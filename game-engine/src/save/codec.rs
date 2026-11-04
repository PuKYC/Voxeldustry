//! world.meta / region 文件的字节编解码（纯函数，无 IO）。
//!
//! 线格式与校验规则逐条对应《存档格式规格》§2（world.meta）、§3（region 文件与记录体）、
//! §4（实体/组件外层框）、§1.1（摘要）。所有 encode 都是纯函数：同样入参字节相同，
//! 提案/实体在写之前按规格键重排（BTreeMap），重复键直接报错而不是静默覆盖。

use std::collections::{BTreeMap, BTreeSet};

use ::voxel::ChunkKey;

use crate::body_key::BodyKey;
use crate::identity::StableEntityId;
use crate::save::format::{
    digest, AttachTag, BodyKind, BodyRecord, ChunkRecord, ComponentRecord, EntityRecord,
    RegionFile, WorldMeta, BODY_RECORD_LEN, CODEC_ID, DIGEST_LEN, FORMAT_VERSION, MAX_BODY_COUNT,
    MAX_CHUNK_COUNT, MAX_COMPONENT_PAYLOAD_LEN, MAX_RECORD_LEN, MAX_REGION_FILE_LEN,
    PROPOSAL_RECORD_LEN, REGION_HEADER_LEN, REGION_INDEX_ENTRY_LEN, REGION_MAGIC, WORLD_HEADER_LEN,
    WORLD_MAGIC,
};
use crate::save::{SaveError, SaveErrorKind};
use crate::streaming::ProposalRecord;

// ---------------------------------------------------------------------------
// 线格式偏移常量（§2 / §3）：既用于报错定位，也让编码顺序与规格表一一对应。
// ---------------------------------------------------------------------------

const W_FORMAT_VERSION: usize = 8;
const W_HEADER_LEN: usize = 10;
const W_CODEC_ID: usize = 12;
const W_FLAGS: usize = 14;
const W_BODY_COUNT: usize = 40;
const W_RESERVED: usize = 44;

const B_ID: usize = 0;
const B_ATTACH: usize = 9;
const B_VOXEL_SIZE: usize = 14;
const B_ATTACH_TO: usize = 46;

const R_FORMAT_VERSION: usize = 8;
const R_HEADER_LEN: usize = 10;
const R_CHUNK_COUNT: usize = 12;
const R_INDEX_OFFSET: usize = 16;
const R_MAX_STABLE_ENTITY_ID: usize = 24;

const I_RECORD_OFFSET: usize = 16;

// ---------------------------------------------------------------------------
// 小端读写器
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn new() -> Self {
        Self::default()
    }

    fn len(&self) -> usize {
        self.buf.len()
    }

    fn u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_le_bytes());
    }

    fn i32(&mut self, value: i32) {
        self.u32(value as u32);
    }

    fn i64(&mut self, value: i64) {
        self.u64(value as u64);
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

/// 拼接字段路径：`{path}.{field}`。
///
/// 解码与校验一律经此构造错误路径，避免调用点手写 `format!` 造成文案漂移。
fn field_path(path: &str, field: &str) -> String {
    format!("{path}.{field}")
}

/// 拼接带下标的字段路径：`{path}.{field}[{index}]`。
fn indexed_path(path: &str, field: &str, index: usize) -> String {
    format!("{path}.{field}[{index}]")
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    /// 该切片在文件里的绝对起始偏移，用于把错误偏移报到文件坐标。
    base: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8], base: usize) -> Self {
        Self {
            bytes,
            pos: 0,
            base,
        }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    fn offset(&self) -> usize {
        self.base + self.pos
    }

    fn take(&mut self, len: usize, path: &str) -> Result<&'a [u8], SaveError> {
        if self.remaining() < len {
            return Err(SaveError::new(
                path,
                self.offset(),
                SaveErrorKind::Truncated {
                    need: len,
                    have: self.remaining(),
                },
            ));
        }
        let start = self.pos;
        self.pos += len;
        Ok(&self.bytes[start..self.pos])
    }

    /// 按「分区路径 + 字段名」读取，内部统一拼接错误路径。
    fn take_at(&mut self, path: &str, field: &str, len: usize) -> Result<&'a [u8], SaveError> {
        self.take(len, &field_path(path, field))
    }

    fn read_u8_at(&mut self, path: &str, field: &str) -> Result<u8, SaveError> {
        self.u8(&field_path(path, field))
    }

    fn read_u16_at(&mut self, path: &str, field: &str) -> Result<u16, SaveError> {
        self.u16(&field_path(path, field))
    }

    fn read_u32_at(&mut self, path: &str, field: &str) -> Result<u32, SaveError> {
        self.u32(&field_path(path, field))
    }

    fn read_u64_at(&mut self, path: &str, field: &str) -> Result<u64, SaveError> {
        self.u64(&field_path(path, field))
    }

    fn read_i32_at(&mut self, path: &str, field: &str) -> Result<i32, SaveError> {
        self.i32(&field_path(path, field))
    }

    fn read_i64_at(&mut self, path: &str, field: &str) -> Result<i64, SaveError> {
        self.i64(&field_path(path, field))
    }

    fn u8(&mut self, path: &str) -> Result<u8, SaveError> {
        Ok(self.take(1, path)?[0])
    }

    fn u16(&mut self, path: &str) -> Result<u16, SaveError> {
        let bytes = self.take(2, path)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self, path: &str) -> Result<u32, SaveError> {
        let bytes = self.take(4, path)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self, path: &str) -> Result<u64, SaveError> {
        let bytes = self.take(8, path)?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn i32(&mut self, path: &str) -> Result<i32, SaveError> {
        self.u32(path).map(|value| value as i32)
    }

    fn i64(&mut self, path: &str) -> Result<i64, SaveError> {
        self.u64(path).map(|value| value as i64)
    }
}

fn read_u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

fn u32_len(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

// ---------------------------------------------------------------------------
// 校验 helper（统一构造 SaveError）
// ---------------------------------------------------------------------------

/// 校验两个值相等；失败报 Inconsistent（头长度、索引偏移、最大实体 id 等）。
fn expect_eq(
    path: &str,
    field: &str,
    offset: usize,
    what: &'static str,
    expected: u64,
    found: u64,
) -> Result<(), SaveError> {
    if expected != found {
        return Err(SaveError::new(
            field_path(path, field),
            offset,
            SaveErrorKind::Inconsistent {
                what,
                expected,
                found,
            },
        ));
    }
    Ok(())
}

/// 校验 `value` 没有置起 `allowed` 之外的标志位；失败报 Unsupported。
fn expect_flags_mask(
    path: &str,
    field: &str,
    offset: usize,
    what: &'static str,
    value: u64,
    allowed: u64,
) -> Result<(), SaveError> {
    if value & !allowed != 0 {
        return Err(SaveError::new(
            field_path(path, field),
            offset,
            SaveErrorKind::Unsupported { what, value },
        ));
    }
    Ok(())
}

/// 校验 `value <= max`；失败报 TooLarge。
fn expect_max(
    path: &str,
    field: &str,
    offset: usize,
    what: &'static str,
    value: u64,
    max: u64,
) -> Result<(), SaveError> {
    if value > max {
        return Err(SaveError::new(
            field_path(path, field),
            offset,
            SaveErrorKind::TooLarge { what, value, max },
        ));
    }
    Ok(())
}

/// 校验保留字段为 0；失败报 NonZeroReserved。
fn expect_reserved_zero(
    path: &str,
    field: &str,
    offset: usize,
    value: u64,
) -> Result<(), SaveError> {
    if value != 0 {
        return Err(SaveError::new(
            field_path(path, field),
            offset,
            SaveErrorKind::NonZeroReserved { value },
        ));
    }
    Ok(())
}

/// 校验 `value != 0`；失败报 InvalidValue。
fn expect_nonzero(
    path: &str,
    field: &str,
    offset: usize,
    what: &'static str,
    value: i64,
) -> Result<(), SaveError> {
    if value == 0 {
        return Err(SaveError::new(
            field_path(path, field),
            offset,
            SaveErrorKind::InvalidValue { what, value },
        ));
    }
    Ok(())
}

/// 校验版本 / 枚举取值必须等于 `expected`；失败报 Unsupported。
fn expect_supported(
    path: &str,
    field: &str,
    offset: usize,
    what: &'static str,
    expected: u64,
    found: u64,
) -> Result<(), SaveError> {
    if found != expected {
        return Err(SaveError::new(
            field_path(path, field),
            offset,
            SaveErrorKind::Unsupported { what, value: found },
        ));
    }
    Ok(())
}

/// 校验键严格升序；重复报 Duplicate，逆序报 NotStrictlyIncreasing。
fn expect_strictly_increasing(
    path: &str,
    field: &str,
    offset: usize,
    what: &'static str,
    previous: u64,
    current: u64,
) -> Result<(), SaveError> {
    if current <= previous {
        let kind = if current == previous {
            SaveErrorKind::Duplicate { what, id: current }
        } else {
            SaveErrorKind::NotStrictlyIncreasing {
                what,
                previous,
                current,
            }
        };
        return Err(SaveError::new(field_path(path, field), offset, kind));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// world.meta（§2）
// ---------------------------------------------------------------------------

/// 编码 world.meta：48 B 头 + 54 B × body_count + 8 B 摘要（§2）。
///
/// 编码侧同时校验：body 键与记录 id 一致、voxel_size > 0、attach_to 只在
/// attach_tag = 1 时非 0 且必须指向表中另一个 body、body_count ≤ 65536；
/// 并且 **只有 Structure 可以 attach_tag != Free**（§2），其余 kind 直接拒绝。
///
/// **读侧宽容、写侧归一化**：解码保留线格式字节（不因 kind 与 biome/anchored/drift
/// 的组合判损坏）；编码则把非岛 kind 的 biome/anchored/drift 一律写 0，并在归零前
/// debug_assert 它们本来就是 0，避免写方传脏值后被静默丢弃。
pub fn encode_world_meta(meta: &WorldMeta) -> Result<Vec<u8>, SaveError> {
    let body_count = meta.body_count();
    if body_count > MAX_BODY_COUNT {
        return Err(SaveError::new(
            "world.meta.body_count",
            W_BODY_COUNT,
            SaveErrorKind::TooLarge {
                what: "body_count",
                value: u64::from(body_count),
                max: u64::from(MAX_BODY_COUNT),
            },
        ));
    }

    let mut writer = Writer::new();
    writer.bytes(&WORLD_MAGIC);
    writer.u16(FORMAT_VERSION);
    writer.u16(WORLD_HEADER_LEN);
    writer.u16(CODEC_ID);
    writer.u16(0);
    writer.u64(meta.seed);
    writer.u64(meta.world_tick);
    writer.u64(meta.next_stable_entity_id);
    writer.u32(body_count);
    writer.u32(0);

    for (index, (key, body)) in meta.bodies.iter().enumerate() {
        let base = usize::from(WORLD_HEADER_LEN) + index * BODY_RECORD_LEN;
        let path = |field: &str| field_path(&indexed_path("world.meta", "body", index), field);

        if body.id != *key {
            return Err(SaveError::new(
                path("id"),
                base + B_ID,
                SaveErrorKind::BodyKeyMismatch {
                    map: key.as_u64(),
                    record: body.id.as_u64(),
                },
            ));
        }
        if body.voxel_size <= 0 {
            return Err(SaveError::new(
                path("voxel_size"),
                base + B_VOXEL_SIZE,
                SaveErrorKind::InvalidValue {
                    what: "voxel_size",
                    value: body.voxel_size,
                },
            ));
        }

        // §2：只有 Structure 可以 attach_tag != Free。读侧宽容，写侧拒绝非法组合。
        if body.attach != AttachTag::Free && body.kind != BodyKind::Structure {
            return Err(SaveError::new(
                path("attach_tag"),
                base + B_ATTACH,
                SaveErrorKind::Unsupported {
                    what: "attach_tag on non-Structure body, body id",
                    value: body.id.as_u64(),
                },
            ));
        }

        match body.attach {
            AttachTag::Body => {
                let target = body.attach_to.as_u64();
                if target == 0 {
                    return Err(SaveError::new(
                        path("attach_to"),
                        base + B_ATTACH_TO,
                        SaveErrorKind::InvalidValue {
                            what: "attach_to (must be non-zero for attach_tag = body)",
                            value: 0,
                        },
                    ));
                }
                if target == key.as_u64() {
                    return Err(SaveError::new(
                        path("attach_to"),
                        base + B_ATTACH_TO,
                        SaveErrorKind::InvalidValue {
                            what: "attach_to (must reference another body)",
                            value: target as i64,
                        },
                    ));
                }
                if !meta.bodies.contains_key(&body.attach_to) {
                    return Err(SaveError::new(
                        path("attach_to"),
                        base + B_ATTACH_TO,
                        SaveErrorKind::Missing {
                            what: "attach_to body",
                            id: target,
                        },
                    ));
                }
            }
            AttachTag::World | AttachTag::Free => {
                if body.attach_to.as_u64() != 0 {
                    return Err(SaveError::new(
                        path("attach_to"),
                        base + B_ATTACH_TO,
                        SaveErrorKind::Inconsistent {
                            what: "attach_to for non-body attach_tag",
                            expected: 0,
                            found: body.attach_to.as_u64(),
                        },
                    ));
                }
            }
        }

        // 写侧归一化（§2）：非岛 kind 的 biome/anchored/drift 一律写 0。
        // debug 下先断言写方已经归零：脏值属于写方 bug，不能静默丢失。
        let (biome, anchored, drift) = if body.kind == BodyKind::Island {
            (body.biome, body.anchored, body.drift)
        } else {
            debug_assert!(
                body.biome == 0 && body.anchored == 0 && body.drift == [0; 3],
                "non-island body {} must be normalized by the writer: biome/anchored/drift must already be 0",
                body.id.as_u64()
            );
            (0, 0, [0; 3])
        };

        writer.u64(key.as_u64());
        writer.u8(body.kind.tag());
        writer.u8(body.attach.tag());
        writer.u8(biome);
        writer.u8(anchored);
        writer.u16(0);
        writer.i64(body.voxel_size);
        writer.i64(drift[0]);
        writer.i64(drift[1]);
        writer.i64(drift[2]);
        writer.u64(body.attach_to.as_u64());
    }

    let digest_value = digest(&writer.buf);
    writer.u64(digest_value);
    Ok(writer.into_vec())
}

/// 解码 world.meta（§2）。
///
/// 未知 kind_tag / attach_tag、非 0 flags / reserved、版本/头长不符、body 表非严格升序、
/// voxel_size <= 0、attach_to 悬空、长度不符、摘要不符都返回 Err；不跳过任何 body。
pub fn decode_world_meta(bytes: &[u8]) -> Result<WorldMeta, SaveError> {
    let header = usize::from(WORLD_HEADER_LEN);
    let min_len = header + DIGEST_LEN;
    if bytes.len() < min_len {
        return Err(SaveError::new(
            "world.meta",
            bytes.len(),
            SaveErrorKind::Truncated {
                need: min_len,
                have: bytes.len(),
            },
        ));
    }

    let mut reader = Reader::new(bytes, 0);

    let magic = reader.take(8, "world.meta.magic")?;
    if magic != WORLD_MAGIC {
        let mut found = [0u8; 8];
        found.copy_from_slice(magic);
        return Err(SaveError::new(
            "world.meta.magic",
            0,
            SaveErrorKind::BadMagic {
                expected: "VXWORLD\0",
                found,
            },
        ));
    }

    let format_version = reader.u16("world.meta.format_version")?;
    if format_version != FORMAT_VERSION {
        return Err(SaveError::new(
            "world.meta.format_version",
            W_FORMAT_VERSION,
            SaveErrorKind::Unsupported {
                what: "format_version",
                value: u64::from(format_version),
            },
        ));
    }

    let header_len = reader.u16("world.meta.header_len")?;
    if header_len != WORLD_HEADER_LEN {
        return Err(SaveError::new(
            "world.meta.header_len",
            W_HEADER_LEN,
            SaveErrorKind::Inconsistent {
                what: "header_len",
                expected: u64::from(WORLD_HEADER_LEN),
                found: u64::from(header_len),
            },
        ));
    }

    let codec_id = reader.u16("world.meta.codec_id")?;
    if codec_id != CODEC_ID {
        return Err(SaveError::new(
            "world.meta.codec_id",
            W_CODEC_ID,
            SaveErrorKind::Unsupported {
                what: "codec_id",
                value: u64::from(codec_id),
            },
        ));
    }

    let flags = reader.u16("world.meta.flags")?;
    if flags != 0 {
        return Err(SaveError::new(
            "world.meta.flags",
            W_FLAGS,
            SaveErrorKind::Unsupported {
                what: "flags",
                value: u64::from(flags),
            },
        ));
    }

    let seed = reader.u64("world.meta.seed")?;
    let world_tick = reader.u64("world.meta.world_tick")?;
    let next_stable_entity_id = reader.u64("world.meta.next_stable_entity_id")?;

    let body_count = reader.u32("world.meta.body_count")?;
    if body_count > MAX_BODY_COUNT {
        return Err(SaveError::new(
            "world.meta.body_count",
            W_BODY_COUNT,
            SaveErrorKind::TooLarge {
                what: "body_count",
                value: u64::from(body_count),
                max: u64::from(MAX_BODY_COUNT),
            },
        ));
    }

    let reserved = reader.u32("world.meta.reserved")?;
    if reserved != 0 {
        return Err(SaveError::new(
            "world.meta.reserved",
            W_RESERVED,
            SaveErrorKind::NonZeroReserved {
                value: u64::from(reserved),
            },
        ));
    }

    let expected_len = header + u32_len(body_count) * BODY_RECORD_LEN + DIGEST_LEN;
    if bytes.len() != expected_len {
        return Err(SaveError::new(
            "world.meta.file_len",
            bytes.len().min(expected_len),
            SaveErrorKind::Inconsistent {
                what: "file length for body_count",
                expected: expected_len as u64,
                found: bytes.len() as u64,
            },
        ));
    }

    let mut bodies: BTreeMap<BodyKey, BodyRecord> = BTreeMap::new();
    let mut previous_id: Option<u64> = None;

    for index in 0..u32_len(body_count) {
        let path = |field: &str| field_path(&indexed_path("world.meta", "body", index), field);

        let id_offset = reader.offset();
        let id = reader.u64(&path("id"))?;
        if let Some(previous) = previous_id {
            if id <= previous {
                let kind = if id == previous {
                    SaveErrorKind::Duplicate {
                        what: "body id",
                        id,
                    }
                } else {
                    SaveErrorKind::NotStrictlyIncreasing {
                        what: "body id",
                        previous,
                        current: id,
                    }
                };
                return Err(SaveError::new(path("id"), id_offset, kind));
            }
        }
        previous_id = Some(id);

        let kind_offset = reader.offset();
        let kind_tag = reader.u8(&path("kind_tag"))?;
        let Some(kind) = BodyKind::from_tag(kind_tag) else {
            return Err(SaveError::new(
                path("kind_tag"),
                kind_offset,
                SaveErrorKind::Unsupported {
                    what: "kind_tag",
                    value: u64::from(kind_tag),
                },
            ));
        };

        let attach_offset = reader.offset();
        let attach_tag = reader.u8(&path("attach_tag"))?;
        let Some(attach) = AttachTag::from_tag(attach_tag) else {
            return Err(SaveError::new(
                path("attach_tag"),
                attach_offset,
                SaveErrorKind::Unsupported {
                    what: "attach_tag",
                    value: u64::from(attach_tag),
                },
            ));
        };

        let biome = reader.u8(&path("biome"))?;
        let anchored = reader.u8(&path("anchored"))?;

        let body_reserved_offset = reader.offset();
        let body_reserved = reader.u16(&path("reserved"))?;
        if body_reserved != 0 {
            return Err(SaveError::new(
                path("reserved"),
                body_reserved_offset,
                SaveErrorKind::NonZeroReserved {
                    value: u64::from(body_reserved),
                },
            ));
        }

        let voxel_offset = reader.offset();
        let voxel_size = reader.i64(&path("voxel_size"))?;
        if voxel_size <= 0 {
            return Err(SaveError::new(
                path("voxel_size"),
                voxel_offset,
                SaveErrorKind::InvalidValue {
                    what: "voxel_size",
                    value: voxel_size,
                },
            ));
        }

        let drift_x = reader.i64(&path("drift_x"))?;
        let drift_y = reader.i64(&path("drift_y"))?;
        let drift_z = reader.i64(&path("drift_z"))?;

        let attach_to_offset = reader.offset();
        let attach_to_raw = reader.u64(&path("attach_to"))?;

        match attach {
            AttachTag::Body => {
                if attach_to_raw == 0 || attach_to_raw == id {
                    return Err(SaveError::new(
                        path("attach_to"),
                        attach_to_offset,
                        SaveErrorKind::InvalidValue {
                            what: "attach_to for attach_tag = body",
                            value: attach_to_raw as i64,
                        },
                    ));
                }
            }
            AttachTag::World | AttachTag::Free => {
                if attach_to_raw != 0 {
                    return Err(SaveError::new(
                        path("attach_to"),
                        attach_to_offset,
                        SaveErrorKind::Inconsistent {
                            what: "attach_to for non-body attach_tag",
                            expected: 0,
                            found: attach_to_raw,
                        },
                    ));
                }
            }
        }

        let key = BodyKey::new(StableEntityId(id));
        bodies.insert(
            key,
            BodyRecord {
                id: key,
                kind,
                attach,
                biome,
                anchored,
                voxel_size,
                drift: [drift_x, drift_y, drift_z],
                attach_to: BodyKey::new(StableEntityId(attach_to_raw)),
            },
        );
    }

    // attach_to 允许前向引用，整表读完后解析。
    for (index, body) in bodies.values().enumerate() {
        if body.attach == AttachTag::Body && !bodies.contains_key(&body.attach_to) {
            return Err(SaveError::new(
                field_path(&indexed_path("world.meta", "body", index), "attach_to"),
                header + index * BODY_RECORD_LEN + B_ATTACH_TO,
                SaveErrorKind::Missing {
                    what: "attach_to body",
                    id: body.attach_to.as_u64(),
                },
            ));
        }
    }

    let digest_offset = header + u32_len(body_count) * BODY_RECORD_LEN;
    let stored = read_u64_at(bytes, digest_offset);
    let computed = digest(&bytes[..digest_offset]);
    if stored != computed {
        return Err(SaveError::new(
            "world.meta.digest",
            digest_offset,
            SaveErrorKind::DigestMismatch { stored, computed },
        ));
    }

    Ok(WorldMeta {
        seed,
        world_tick,
        next_stable_entity_id,
        bodies,
    })
}

// ---------------------------------------------------------------------------
// 组件外层框（§4.1；payload 是不透明字节）
// ---------------------------------------------------------------------------

/// 编码实体 payload（§4.1：u16 component_count | u16 reserved = 0 | ComponentRecord[]）。
///
/// 组件按 stable_type_id 升序写出（BTreeMap），重复 ID / 0 ID / 负载超过 1 MiB 报错。
/// 组件 payload 原样拼接，不做 postcard 编解码。
pub fn encode_component_table(components: &[ComponentRecord]) -> Result<Vec<u8>, SaveError> {
    if components.len() > usize::from(u16::MAX) {
        return Err(SaveError::new(
            "component_table.component_count",
            0,
            SaveErrorKind::TooLarge {
                what: "component_count",
                value: components.len() as u64,
                max: u64::from(u16::MAX),
            },
        ));
    }

    let mut sorted: BTreeMap<u32, &ComponentRecord> = BTreeMap::new();
    for (index, component) in components.iter().enumerate() {
        let path = indexed_path("component_table", "component", index);
        if component.stable_type_id == 0 {
            return Err(SaveError::new(
                field_path(path, "stable_type_id"),
                0,
                SaveErrorKind::InvalidValue {
                    what: "stable_type_id",
                    value: 0,
                },
            ));
        }
        if component.payload.len() > u32_len(MAX_COMPONENT_PAYLOAD_LEN) {
            return Err(SaveError::new(
                field_path(path, "payload_len"),
                0,
                SaveErrorKind::TooLarge {
                    what: "component payload_len",
                    value: component.payload.len() as u64,
                    max: u64::from(MAX_COMPONENT_PAYLOAD_LEN),
                },
            ));
        }
        if sorted.insert(component.stable_type_id, component).is_some() {
            return Err(SaveError::new(
                field_path(path, "stable_type_id"),
                0,
                SaveErrorKind::Duplicate {
                    what: "component stable_type_id",
                    id: u64::from(component.stable_type_id),
                },
            ));
        }
    }

    let mut writer = Writer::new();
    writer.u16(u16::try_from(sorted.len()).unwrap_or(u16::MAX));
    writer.u16(0);
    for (stable_type_id, component) in sorted {
        writer.u32(stable_type_id);
        writer.u32(u32::try_from(component.payload.len()).unwrap_or(u32::MAX));
        writer.bytes(&component.payload);
    }
    Ok(writer.into_vec())
}

/// 解码实体 payload，返回组件表（§4.1）。
///
/// 校验：外层 reserved = 0、组件 ID 非 0 且严格升序、负载 ≤ 1 MiB、
/// 恰好消费完全部字节（多一字节也算损坏）。组件 payload 原样返回。
pub fn decode_component_table(bytes: &[u8]) -> Result<Vec<ComponentRecord>, SaveError> {
    decode_component_table_at(bytes, "component_table", 0)
}

fn decode_component_table_at(
    bytes: &[u8],
    path: &str,
    base: usize,
) -> Result<Vec<ComponentRecord>, SaveError> {
    if bytes.len() < 4 {
        return Err(SaveError::new(
            path,
            base,
            SaveErrorKind::Truncated {
                need: 4,
                have: bytes.len(),
            },
        ));
    }

    let mut reader = Reader::new(bytes, base);

    let count = usize::from(reader.read_u16_at(path, "component_count")?);

    let reserved_offset = reader.offset();
    let reserved = reader.read_u16_at(path, "reserved")?;
    if reserved != 0 {
        return Err(SaveError::new(
            field_path(path, "reserved"),
            reserved_offset,
            SaveErrorKind::NonZeroReserved {
                value: u64::from(reserved),
            },
        ));
    }

    // 每条组件至少 8 B（ID + 长度），先按上限判断，避免按不可信 count 预分配。
    if count > reader.remaining() / 8 {
        return Err(SaveError::new(
            field_path(path, "component_count"),
            reader.offset(),
            SaveErrorKind::Truncated {
                need: count.saturating_mul(8),
                have: reader.remaining(),
            },
        ));
    }

    let mut components: Vec<ComponentRecord> = Vec::with_capacity(count);
    for index in 0..count {
        let cpath = indexed_path(path, "component", index);

        let id_offset = reader.offset();
        let stable_type_id = reader.read_u32_at(&cpath, "stable_type_id")?;
        if stable_type_id == 0 {
            return Err(SaveError::new(
                field_path(&cpath, "stable_type_id"),
                id_offset,
                SaveErrorKind::InvalidValue {
                    what: "stable_type_id",
                    value: 0,
                },
            ));
        }

        let len_offset = reader.offset();
        let payload_len = reader.read_u32_at(&cpath, "payload_len")?;
        if payload_len > MAX_COMPONENT_PAYLOAD_LEN {
            return Err(SaveError::new(
                field_path(&cpath, "payload_len"),
                len_offset,
                SaveErrorKind::TooLarge {
                    what: "component payload_len",
                    value: u64::from(payload_len),
                    max: u64::from(MAX_COMPONENT_PAYLOAD_LEN),
                },
            ));
        }
        let payload = reader
            .take_at(&cpath, "payload", u32_len(payload_len))?
            .to_vec();

        if let Some(previous) = components.last().map(|component| component.stable_type_id) {
            if stable_type_id <= previous {
                let kind = if stable_type_id == previous {
                    SaveErrorKind::Duplicate {
                        what: "component stable_type_id",
                        id: u64::from(stable_type_id),
                    }
                } else {
                    SaveErrorKind::NotStrictlyIncreasing {
                        what: "component stable_type_id",
                        previous: u64::from(previous),
                        current: u64::from(stable_type_id),
                    }
                };
                return Err(SaveError::new(
                    field_path(&cpath, "stable_type_id"),
                    id_offset,
                    kind,
                ));
            }
        }

        components.push(ComponentRecord {
            stable_type_id,
            payload,
        });
    }

    if reader.remaining() != 0 {
        return Err(SaveError::new(
            field_path(path, "trailing"),
            reader.offset(),
            SaveErrorKind::TrailingBytes {
                extra: reader.remaining(),
            },
        ));
    }

    Ok(components)
}

// ---------------------------------------------------------------------------
// region 记录体（§3.1–§3.3、§4.1）
// ---------------------------------------------------------------------------

fn canonical_proposals(
    record: &ChunkRecord,
    path: &str,
    start: usize,
) -> Result<BTreeMap<(u16, u32), ProposalRecord>, SaveError> {
    let mut proposals: BTreeMap<(u16, u32), ProposalRecord> = BTreeMap::new();
    for (index, proposal) in record.proposals.iter().enumerate() {
        let ppath = indexed_path(path, "proposal", index);
        if let Some(axis) = proposal.invalid_axis() {
            return Err(SaveError::new(
                field_path(&ppath, "bounds"),
                start,
                SaveErrorKind::InvalidBounds { index, axis },
            ));
        }
        let key = (proposal.feature_type, proposal.seq);
        if proposals.insert(key, *proposal).is_some() {
            return Err(SaveError::new(
                field_path(&ppath, "seq"),
                start,
                SaveErrorKind::Duplicate {
                    what: "proposal (feature_type, seq)",
                    id: (u64::from(key.0) << 32) | u64::from(key.1),
                },
            ));
        }
    }
    Ok(proposals)
}

fn canonical_entities<'a>(
    record: &'a ChunkRecord,
    path: &str,
    start: usize,
    seen_entities: &mut BTreeSet<u64>,
) -> Result<BTreeMap<u64, &'a EntityRecord>, SaveError> {
    let mut entities: BTreeMap<u64, &EntityRecord> = BTreeMap::new();
    for (index, entity) in record.entities.iter().enumerate() {
        let epath = indexed_path(path, "entity", index);
        if entity.stable_type_id == 0 {
            return Err(SaveError::new(
                field_path(&epath, "stable_type_id"),
                start,
                SaveErrorKind::InvalidValue {
                    what: "stable_type_id",
                    value: 0,
                },
            ));
        }
        if entity.payload.len() > u32::MAX as usize {
            return Err(SaveError::new(
                field_path(&epath, "payload_len"),
                start,
                SaveErrorKind::TooLarge {
                    what: "entity payload_len",
                    value: entity.payload.len() as u64,
                    max: u64::from(u32::MAX),
                },
            ));
        }
        // payload 原字节写回，但外层框必须自洽（§4.1）。校验结果丢弃。
        decode_component_table_at(&entity.payload, &field_path(&epath, "payload"), start)?;

        if entities.insert(entity.stable_entity_id, entity).is_some() {
            return Err(SaveError::new(
                field_path(&epath, "stable_entity_id"),
                start,
                SaveErrorKind::Duplicate {
                    what: "entity id in chunk",
                    id: entity.stable_entity_id,
                },
            ));
        }
        if !seen_entities.insert(entity.stable_entity_id) {
            return Err(SaveError::new(
                field_path(&epath, "stable_entity_id"),
                start,
                SaveErrorKind::Duplicate {
                    what: "entity id in region",
                    id: entity.stable_entity_id,
                },
            ));
        }
    }
    Ok(entities)
}

fn encode_record(
    record: &ChunkRecord,
    path: &str,
    start: usize,
    seen_entities: &mut BTreeSet<u64>,
) -> Result<Vec<u8>, SaveError> {
    if record.is_empty() {
        return Err(SaveError::new(
            path,
            start,
            SaveErrorKind::InvalidCombination {
                what: "empty record must not be written",
            },
        ));
    }
    if record.content.is_some() && !record.proposals.is_empty() {
        return Err(SaveError::new(
            field_path(path, "proposal_count"),
            start,
            SaveErrorKind::InvalidCombination {
                what: "content with proposals",
            },
        ));
    }

    let proposals = canonical_proposals(record, path, start)?;
    let entities = canonical_entities(record, path, start, seen_entities)?;

    let mut writer = Writer::new();
    writer.u16(1); // record_version
    let flags: u16 = if record.content.is_some() { 1 } else { 0 };
    writer.u16(flags);

    if let Some(content) = &record.content {
        let content_len = u32::try_from(content.len()).map_err(|_| {
            SaveError::new(
                field_path(path, "content_len"),
                start,
                SaveErrorKind::TooLarge {
                    what: "content_len",
                    value: content.len() as u64,
                    max: u64::from(MAX_RECORD_LEN),
                },
            )
        })?;
        writer.u32(content_len);
        writer.bytes(content);
    }

    writer.u32(u32::try_from(proposals.len()).unwrap_or(u32::MAX));
    for proposal in proposals.values() {
        writer.u16(proposal.feature_type);
        writer.u16(0);
        writer.u32(proposal.seq);
        writer.i32(proposal.priority);
        writer.u64(proposal.tie_break);
        writer.i32(proposal.origin[0]);
        writer.i32(proposal.origin[1]);
        writer.i32(proposal.origin[2]);
        writer.i32(proposal.bounds_min[0]);
        writer.i32(proposal.bounds_min[1]);
        writer.i32(proposal.bounds_min[2]);
        writer.i32(proposal.bounds_max[0]);
        writer.i32(proposal.bounds_max[1]);
        writer.i32(proposal.bounds_max[2]);
    }

    writer.u32(u32::try_from(entities.len()).unwrap_or(u32::MAX));
    for entity in entities.values() {
        writer.u64(entity.stable_entity_id);
        writer.u32(entity.stable_type_id);
        writer.u32(u32::try_from(entity.payload.len()).unwrap_or(u32::MAX));
        writer.bytes(&entity.payload);
    }

    if writer.len() > u32_len(MAX_RECORD_LEN) {
        return Err(SaveError::new(
            path,
            start,
            SaveErrorKind::TooLarge {
                what: "record_len",
                value: writer.len() as u64,
                max: u64::from(MAX_RECORD_LEN),
            },
        ));
    }

    Ok(writer.into_vec())
}

fn decode_record(
    bytes: &[u8],
    path: &str,
    base: usize,
    generator_version: u32,
    seen_entities: &mut BTreeSet<u64>,
) -> Result<ChunkRecord, SaveError> {
    let mut reader = Reader::new(bytes, base);

    let version_offset = reader.offset();
    let record_version = reader.read_u16_at(path, "record_version")?;
    expect_supported(
        path,
        "record_version",
        version_offset,
        "record_version",
        1,
        u64::from(record_version),
    )?;

    let flags_offset = reader.offset();
    let flags = reader.read_u16_at(path, "flags")?;
    expect_flags_mask(
        path,
        "flags",
        flags_offset,
        "record flags",
        u64::from(flags),
        1,
    )?;

    let content = if flags & 1 != 0 {
        let len_offset = reader.offset();
        let content_len = reader.read_u32_at(path, "content_len")?;
        expect_max(
            path,
            "content_len",
            len_offset,
            "content_len",
            u64::from(content_len),
            u64::from(MAX_RECORD_LEN),
        )?;
        Some(
            reader
                .take_at(&path, "content", u32_len(content_len))?
                .to_vec(),
        )
    } else {
        None
    };

    let proposal_count = u32_len(reader.read_u32_at(path, "proposal_count")?);
    if proposal_count > reader.remaining() / PROPOSAL_RECORD_LEN {
        return Err(SaveError::new(
            field_path(path, "proposal_count"),
            reader.offset(),
            SaveErrorKind::Truncated {
                need: proposal_count.saturating_mul(PROPOSAL_RECORD_LEN),
                have: reader.remaining(),
            },
        ));
    }

    let mut proposals: Vec<ProposalRecord> = Vec::with_capacity(proposal_count);
    for index in 0..proposal_count {
        let ppath = indexed_path(path, "proposal", index);

        let feature_type = reader.read_u16_at(&ppath, "feature_type")?;

        let reserved_offset = reader.offset();
        let reserved = reader.read_u16_at(&ppath, "reserved")?;
        expect_reserved_zero(&ppath, "reserved", reserved_offset, u64::from(reserved))?;

        let seq = reader.read_u32_at(&ppath, "seq")?;
        let priority = reader.read_i32_at(&ppath, "priority")?;
        let tie_break = reader.read_u64_at(&ppath, "tie_break")?;
        let origin = [
            reader.read_i32_at(&ppath, "origin.x")?,
            reader.read_i32_at(&ppath, "origin.y")?,
            reader.read_i32_at(&ppath, "origin.z")?,
        ];
        let bounds_min = [
            reader.read_i32_at(&ppath, "bounds_min.x")?,
            reader.read_i32_at(&ppath, "bounds_min.y")?,
            reader.read_i32_at(&ppath, "bounds_min.z")?,
        ];
        let bounds_max = [
            reader.read_i32_at(&ppath, "bounds_max.x")?,
            reader.read_i32_at(&ppath, "bounds_max.y")?,
            reader.read_i32_at(&ppath, "bounds_max.z")?,
        ];

        let proposal = ProposalRecord {
            feature_type,
            seq,
            priority,
            tie_break,
            origin,
            bounds_min,
            bounds_max,
        };

        if let Some(axis) = proposal.invalid_axis() {
            return Err(SaveError::new(
                field_path(&ppath, "bounds"),
                base,
                SaveErrorKind::InvalidBounds { index, axis },
            ));
        }

        if let Some(previous) = proposals.last() {
            let previous_key = (previous.feature_type, previous.seq);
            let current_key = (proposal.feature_type, proposal.seq);
            expect_strictly_increasing(
                &ppath,
                "seq",
                base,
                "proposal (feature_type, seq)",
                (u64::from(previous_key.0) << 32) | u64::from(previous_key.1),
                (u64::from(current_key.0) << 32) | u64::from(current_key.1),
            )?;
        }

        proposals.push(proposal);
    }

    let entity_count = u32_len(reader.read_u32_at(path, "entity_count")?);
    // 每条实体至少 16 B（id + type + payload_len），先按上限判断再分配。
    if entity_count > reader.remaining() / 16 {
        return Err(SaveError::new(
            field_path(path, "entity_count"),
            reader.offset(),
            SaveErrorKind::Truncated {
                need: entity_count.saturating_mul(16),
                have: reader.remaining(),
            },
        ));
    }

    let mut entities: Vec<EntityRecord> = Vec::with_capacity(entity_count);
    for index in 0..entity_count {
        let epath = indexed_path(path, "entity", index);

        let id_offset = reader.offset();
        let stable_entity_id = reader.read_u64_at(&epath, "stable_entity_id")?;
        if let Some(previous) = entities.last().map(|entity| entity.stable_entity_id) {
            expect_strictly_increasing(
                &epath,
                "stable_entity_id",
                id_offset,
                "entity id in chunk",
                previous,
                stable_entity_id,
            )?;
        }

        let type_offset = reader.offset();
        let stable_type_id = reader.read_u32_at(&epath, "stable_type_id")?;
        expect_nonzero(&epath, "stable_type_id", type_offset, "stable_type_id", 0)?;

        let payload_len_offset = reader.offset();
        let payload_len = reader.read_u32_at(&epath, "payload_len")?;
        expect_max(
            &epath,
            "payload_len",
            payload_len_offset,
            "entity payload_len",
            u64::from(payload_len),
            u64::from(MAX_RECORD_LEN),
        )?;

        let payload_offset = reader.offset();
        let payload = reader
            .take_at(&epath, "payload", u32_len(payload_len))?
            .to_vec();
        decode_component_table_at(&payload, &field_path(&epath, "payload"), payload_offset)?;

        if !seen_entities.insert(stable_entity_id) {
            return Err(SaveError::new(
                field_path(&epath, "stable_entity_id"),
                id_offset,
                SaveErrorKind::Duplicate {
                    what: "entity id in region",
                    id: stable_entity_id,
                },
            ));
        }

        entities.push(EntityRecord {
            stable_entity_id,
            stable_type_id,
            payload,
        });
    }

    if reader.remaining() != 0 {
        return Err(SaveError::new(
            path,
            reader.offset(),
            SaveErrorKind::TrailingBytes {
                extra: reader.remaining(),
            },
        ));
    }

    if content.is_some() && !proposals.is_empty() {
        return Err(SaveError::new(
            field_path(path, "proposal_count"),
            base,
            SaveErrorKind::InvalidCombination {
                what: "content with proposals",
            },
        ));
    }

    let record = ChunkRecord {
        generator_version,
        content,
        proposals,
        entities,
    };

    if record.is_empty() {
        return Err(SaveError::new(
            path,
            base,
            SaveErrorKind::InvalidCombination {
                what: "empty record must not be written",
            },
        ));
    }

    Ok(record)
}

/// 单独编码一条 ChunkRecord 记录体（§3.1），不含 region 头/索引。
///
/// 主要给测试与迁移工具用；region 文件请用 encode_region。
pub fn encode_chunk_record(record: &ChunkRecord) -> Result<Vec<u8>, SaveError> {
    let mut seen_entities = BTreeSet::new();
    encode_record(record, "chunk.record", 0, &mut seen_entities)
}

/// 单独解码一条 ChunkRecord 记录体（§3.1）；generator_version 来自 region 索引项。
///
/// 必须恰好消费完 bytes，多一字节也算损坏。
pub fn decode_chunk_record(bytes: &[u8], generator_version: u32) -> Result<ChunkRecord, SaveError> {
    if bytes.len() > u32_len(MAX_RECORD_LEN) {
        return Err(SaveError::new(
            "chunk.record",
            0,
            SaveErrorKind::TooLarge {
                what: "record_len",
                value: bytes.len() as u64,
                max: u64::from(MAX_RECORD_LEN),
            },
        ));
    }
    let mut seen_entities = BTreeSet::new();
    decode_record(
        bytes,
        "chunk.record",
        0,
        generator_version,
        &mut seen_entities,
    )
}

// ---------------------------------------------------------------------------
// region 文件（§3）
// ---------------------------------------------------------------------------

/// 编码一个 region 文件：32 B 头 + 记录区 + 32 B × chunk_count 索引 + 8 B 摘要（§3）。
///
/// record_offset 连续、首项 = 32，index_offset = 32 + Σ record_len，
/// max_stable_entity_id 现算；chunk 按 ChunkKey 升序（BTreeMap），记录内提案/实体重排。
pub fn encode_region(region: &RegionFile) -> Result<Vec<u8>, SaveError> {
    let chunk_count = region.chunk_count();
    if chunk_count > MAX_CHUNK_COUNT {
        return Err(SaveError::new(
            "region.chunk_count",
            R_CHUNK_COUNT,
            SaveErrorKind::TooLarge {
                what: "chunk_count",
                value: u64::from(chunk_count),
                max: u64::from(MAX_CHUNK_COUNT),
            },
        ));
    }

    let mut records: Vec<(ChunkKey, u32, Vec<u8>)> = Vec::with_capacity(region.chunks.len());
    let mut seen_entities = BTreeSet::new();
    let mut record_offset = u64::from(REGION_HEADER_LEN);

    for (index, (key, record)) in region.chunks.iter().enumerate() {
        let start = usize::try_from(record_offset).unwrap_or(usize::MAX);
        let bytes = encode_record(
            record,
            &field_path(&indexed_path("region", "chunk", index), "record"),
            start,
            &mut seen_entities,
        )?;
        record_offset = record_offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| {
                SaveError::new(
                    "region.file_len",
                    start,
                    SaveErrorKind::TooLarge {
                        what: "record_offset",
                        value: u64::MAX,
                        max: MAX_REGION_FILE_LEN,
                    },
                )
            })?;
        records.push((*key, record.generator_version, bytes));
    }

    let index_offset = record_offset;
    let total_len = index_offset
        .checked_add(u64::from(chunk_count) * REGION_INDEX_ENTRY_LEN as u64)
        .and_then(|value| value.checked_add(DIGEST_LEN as u64))
        .ok_or_else(|| {
            SaveError::new(
                "region.file_len",
                0,
                SaveErrorKind::TooLarge {
                    what: "region file length",
                    value: u64::MAX,
                    max: MAX_REGION_FILE_LEN,
                },
            )
        })?;
    if total_len > MAX_REGION_FILE_LEN {
        return Err(SaveError::new(
            "region.file_len",
            0,
            SaveErrorKind::TooLarge {
                what: "region file length",
                value: total_len,
                max: MAX_REGION_FILE_LEN,
            },
        ));
    }

    let mut writer = Writer::new();
    writer.bytes(&REGION_MAGIC);
    writer.u16(FORMAT_VERSION);
    writer.u16(REGION_HEADER_LEN);
    writer.u32(chunk_count);
    writer.u64(index_offset);
    writer.u64(region.max_stable_entity_id());

    for (_, _, bytes) in &records {
        writer.bytes(bytes);
    }

    let mut entry_offset = u64::from(REGION_HEADER_LEN);
    for (key, generator_version, bytes) in &records {
        writer.i32(key.x);
        writer.i32(key.y);
        writer.i32(key.z);
        writer.u32(*generator_version);
        writer.u64(entry_offset);
        writer.u32(u32::try_from(bytes.len()).unwrap_or(u32::MAX));
        writer.u32(0);
        entry_offset += bytes.len() as u64;
    }

    let digest_value = digest(&writer.buf);
    writer.u64(digest_value);
    Ok(writer.into_vec())
}

/// 解码一个 region 文件（§3）。
///
/// 校验索引按 ChunkKey 升序且不重复、record_offset 从 32 起连续、record_len 精确、
/// index_offset = 32 + Σ record_len 且等于文件里索引表位置、max_stable_entity_id 与实算相等、
/// 记录体恰好消费完、同 region 实体 ID 不重复、尾摘要覆盖前面全部字节。
pub fn decode_region(bytes: &[u8]) -> Result<RegionFile, SaveError> {
    if bytes.len() as u64 > MAX_REGION_FILE_LEN {
        return Err(SaveError::new(
            "region.file_len",
            0,
            SaveErrorKind::TooLarge {
                what: "region file length",
                value: bytes.len() as u64,
                max: MAX_REGION_FILE_LEN,
            },
        ));
    }

    let min_len = usize::from(REGION_HEADER_LEN) + DIGEST_LEN;
    if bytes.len() < min_len {
        return Err(SaveError::new(
            "region",
            bytes.len(),
            SaveErrorKind::Truncated {
                need: min_len,
                have: bytes.len(),
            },
        ));
    }

    let mut reader = Reader::new(bytes, 0);

    let magic = reader.take(8, "region.magic")?;
    if magic != REGION_MAGIC {
        let mut found = [0u8; 8];
        found.copy_from_slice(magic);
        return Err(SaveError::new(
            "region.magic",
            0,
            SaveErrorKind::BadMagic {
                expected: "VXREGION",
                found,
            },
        ));
    }

    let format_version = reader.u16("region.format_version")?;
    if format_version != FORMAT_VERSION {
        return Err(SaveError::new(
            "region.format_version",
            R_FORMAT_VERSION,
            SaveErrorKind::Unsupported {
                what: "format_version",
                value: u64::from(format_version),
            },
        ));
    }

    let header_len = reader.u16("region.header_len")?;
    if header_len != REGION_HEADER_LEN {
        return Err(SaveError::new(
            "region.header_len",
            R_HEADER_LEN,
            SaveErrorKind::Inconsistent {
                what: "header_len",
                expected: u64::from(REGION_HEADER_LEN),
                found: u64::from(header_len),
            },
        ));
    }

    let chunk_count = reader.u32("region.chunk_count")?;
    if chunk_count > MAX_CHUNK_COUNT {
        return Err(SaveError::new(
            "region.chunk_count",
            R_CHUNK_COUNT,
            SaveErrorKind::TooLarge {
                what: "chunk_count",
                value: u64::from(chunk_count),
                max: u64::from(MAX_CHUNK_COUNT),
            },
        ));
    }

    let index_offset = reader.u64("region.index_offset")?;
    let max_stable_entity_id = reader.u64("region.max_stable_entity_id")?;

    let body_len = bytes.len() - DIGEST_LEN;
    if index_offset < u64::from(REGION_HEADER_LEN) {
        return Err(SaveError::new(
            "region.index_offset",
            R_INDEX_OFFSET,
            SaveErrorKind::InvalidValue {
                what: "index_offset (must be >= header_len)",
                value: index_offset as i64,
            },
        ));
    }
    if index_offset > body_len as u64 {
        return Err(SaveError::new(
            "region.index_offset",
            R_INDEX_OFFSET,
            SaveErrorKind::TooLarge {
                what: "index_offset",
                value: index_offset,
                max: body_len as u64,
            },
        ));
    }

    let expected_len = index_offset
        .checked_add(u64::from(chunk_count) * REGION_INDEX_ENTRY_LEN as u64)
        .and_then(|value| value.checked_add(DIGEST_LEN as u64));
    let Some(expected_len) = expected_len else {
        return Err(SaveError::new(
            "region.file_len",
            0,
            SaveErrorKind::TooLarge {
                what: "region file length",
                value: u64::MAX,
                max: MAX_REGION_FILE_LEN,
            },
        ));
    };
    if bytes.len() as u64 != expected_len {
        return Err(SaveError::new(
            "region.file_len",
            bytes.len(),
            SaveErrorKind::Inconsistent {
                what: "file length for index_offset and chunk_count",
                expected: expected_len,
                found: bytes.len() as u64,
            },
        ));
    }

    let index_start = usize::try_from(index_offset).map_err(|_| {
        SaveError::new(
            "region.index_offset",
            R_INDEX_OFFSET,
            SaveErrorKind::TooLarge {
                what: "index_offset",
                value: index_offset,
                max: usize::MAX as u64,
            },
        )
    })?;
    let index_len = u32_len(chunk_count) * REGION_INDEX_ENTRY_LEN;
    let mut index_reader = Reader::new(&bytes[index_start..index_start + index_len], index_start);

    let mut chunks: BTreeMap<ChunkKey, ChunkRecord> = BTreeMap::new();
    let mut seen_entities: BTreeSet<u64> = BTreeSet::new();
    let mut previous_key: Option<ChunkKey> = None;
    let mut expected_offset = u64::from(REGION_HEADER_LEN);
    let mut computed_max: u64 = 0;

    for index in 0..u32_len(chunk_count) {
        let ipath = indexed_path("region", "index", index);

        let x = index_reader.read_i32_at(&ipath, "x")?;
        let y = index_reader.read_i32_at(&ipath, "y")?;
        let z = index_reader.read_i32_at(&ipath, "z")?;
        let key = ChunkKey::new(x, y, z);

        let generator_version = index_reader.read_u32_at(&ipath, "generator_version")?;

        let offset_field = index_reader.offset();
        let record_offset = index_reader.read_u64_at(&ipath, "record_offset")?;

        let len_field = index_reader.offset();
        let record_len = index_reader.read_u32_at(&ipath, "record_len")?;

        let reserved_field = index_reader.offset();
        let reserved = index_reader.read_u32_at(&ipath, "reserved")?;
        if reserved != 0 {
            return Err(SaveError::new(
                field_path(&ipath, "reserved"),
                reserved_field,
                SaveErrorKind::NonZeroReserved {
                    value: u64::from(reserved),
                },
            ));
        }

        if let Some(previous) = previous_key {
            if key <= previous {
                return Err(SaveError::new(
                    ipath.clone(),
                    offset_field.saturating_sub(I_RECORD_OFFSET),
                    SaveErrorKind::ChunkKeyOrder {
                        previous: [previous.x, previous.y, previous.z],
                        current: [key.x, key.y, key.z],
                    },
                ));
            }
        }
        previous_key = Some(key);

        if record_offset != expected_offset {
            return Err(SaveError::new(
                field_path(&ipath, "record_offset"),
                offset_field,
                SaveErrorKind::Inconsistent {
                    what: "record_offset (records must be contiguous from header end)",
                    expected: expected_offset,
                    found: record_offset,
                },
            ));
        }

        if record_len == 0 {
            return Err(SaveError::new(
                field_path(&ipath, "record_len"),
                len_field,
                SaveErrorKind::InvalidValue {
                    what: "record_len",
                    value: 0,
                },
            ));
        }
        if record_len > MAX_RECORD_LEN {
            return Err(SaveError::new(
                field_path(&ipath, "record_len"),
                len_field,
                SaveErrorKind::TooLarge {
                    what: "record_len",
                    value: u64::from(record_len),
                    max: u64::from(MAX_RECORD_LEN),
                },
            ));
        }

        let end = record_offset
            .checked_add(u64::from(record_len))
            .ok_or_else(|| {
                SaveError::new(
                    field_path(&ipath, "record_offset"),
                    offset_field,
                    SaveErrorKind::TooLarge {
                        what: "record end",
                        value: u64::MAX,
                        max: index_offset,
                    },
                )
            })?;
        if end > index_offset {
            return Err(SaveError::new(
                field_path(&ipath, "record_offset"),
                offset_field,
                SaveErrorKind::TooLarge {
                    what: "record end",
                    value: end,
                    max: index_offset,
                },
            ));
        }

        let start = usize::try_from(record_offset).map_err(|_| {
            SaveError::new(
                field_path(&ipath, "record_offset"),
                offset_field,
                SaveErrorKind::TooLarge {
                    what: "record_offset",
                    value: record_offset,
                    max: usize::MAX as u64,
                },
            )
        })?;
        let end = usize::try_from(end).map_err(|_| {
            SaveError::new(
                field_path(&ipath, "record_len"),
                len_field,
                SaveErrorKind::TooLarge {
                    what: "record_len",
                    value: u64::from(record_len),
                    max: usize::MAX as u64,
                },
            )
        })?;

        let record = decode_record(
            &bytes[start..end],
            &field_path(&indexed_path("region", "chunk", index), "record"),
            start,
            generator_version,
            &mut seen_entities,
        )?;

        for entity in &record.entities {
            computed_max = computed_max.max(entity.stable_entity_id);
        }

        chunks.insert(key, record);
        expected_offset = record_offset + u64::from(record_len);
    }

    if expected_offset != index_offset {
        return Err(SaveError::new(
            "region.index_offset",
            R_INDEX_OFFSET,
            SaveErrorKind::Inconsistent {
                what: "index_offset (32 + sum of record_len)",
                expected: expected_offset,
                found: index_offset,
            },
        ));
    }

    if computed_max != max_stable_entity_id {
        return Err(SaveError::new(
            "region.max_stable_entity_id",
            R_MAX_STABLE_ENTITY_ID,
            SaveErrorKind::Inconsistent {
                what: "max_stable_entity_id",
                expected: computed_max,
                found: max_stable_entity_id,
            },
        ));
    }

    let stored = read_u64_at(bytes, body_len);
    let computed = digest(&bytes[..body_len]);
    if stored != computed {
        return Err(SaveError::new(
            "region.digest",
            body_len,
            SaveErrorKind::DigestMismatch { stored, computed },
        ));
    }

    Ok(RegionFile { chunks })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::save::format::{
        region_of, CODEC_ID, DIGEST_LEN, FORMAT_VERSION, MAX_BODY_COUNT, MAX_CHUNK_COUNT,
        MAX_COMPONENT_PAYLOAD_LEN, MAX_RECORD_LEN, PROPOSAL_RECORD_LEN, REGION_HEADER_LEN,
        REGION_MAGIC, WORLD_HEADER_LEN,
    };

    /// region 索引项内 record_len 的偏移（§3 索引项 32 B 布局）。
    const I_RECORD_LEN: usize = 24;

    fn key(id: u64) -> BodyKey {
        BodyKey::new(StableEntityId(id))
    }

    fn island(id: u64, biome: u8, anchored: u8) -> BodyRecord {
        BodyRecord {
            id: key(id),
            kind: BodyKind::Island,
            attach: AttachTag::Free,
            biome,
            anchored,
            voxel_size: 0x0100_0000, // I40F24 的 1.0
            drift: [0; 3],
            attach_to: BodyKey::INVALID,
        }
    }

    fn structure(id: u64, attach_to: u64) -> BodyRecord {
        BodyRecord {
            id: key(id),
            kind: BodyKind::Structure,
            attach: AttachTag::Body,
            biome: 0,
            anchored: 0,
            voxel_size: 0x0180_0000, // I40F24 的 1.5
            drift: [0; 3],
            attach_to: key(attach_to),
        }
    }

    /// 黄金样例的 world.meta：2 个 body，其中 id=1 前向引用 id=3。
    fn golden_world_meta() -> WorldMeta {
        let mut bodies = BTreeMap::new();
        let first = structure(1, 3);
        let second = island(3, 7, 1);
        bodies.insert(first.id, first);
        bodies.insert(second.id, second);
        WorldMeta {
            seed: 0x0123_4567_89AB_CDEF,
            world_tick: 12_345,
            next_stable_entity_id: 64,
            bodies,
        }
    }

    fn entity(id: u64, type_id: u32, components: &[ComponentRecord]) -> EntityRecord {
        EntityRecord {
            stable_entity_id: id,
            stable_type_id: type_id,
            payload: encode_component_table(components).expect("组件表必须可编码"),
        }
    }

    fn proposal(feature_type: u16, seq: u32) -> ProposalRecord {
        ProposalRecord {
            feature_type,
            seq,
            priority: -3,
            tie_break: 0xFEDC_BA98_7654_3210,
            origin: [-7, 8, 9],
            bounds_min: [-10, 8, 6],
            bounds_max: [4, 20, 12],
        }
    }

    /// 黄金样例的 region：3 个 chunk（content / proposal / entity 各一），实体最大 ID = 100。
    fn golden_region() -> RegionFile {
        let mut chunks = BTreeMap::new();
        chunks.insert(
            ChunkKey::new(0, 0, 0),
            ChunkRecord {
                generator_version: 7,
                content: Some(b"VOXELIO\0\x01\x00".to_vec()),
                proposals: Vec::new(),
                entities: vec![entity(
                    42,
                    1001,
                    &[ComponentRecord {
                        stable_type_id: 7,
                        payload: vec![1, 2, 3],
                    }],
                )],
            },
        );
        chunks.insert(
            ChunkKey::new(2, 3, 1),
            ChunkRecord {
                generator_version: 1,
                content: None,
                proposals: vec![proposal(2, 4)],
                entities: Vec::new(),
            },
        );
        chunks.insert(
            ChunkKey::new(3, 0, 2),
            ChunkRecord {
                generator_version: 2,
                content: None,
                proposals: Vec::new(),
                entities: vec![entity(
                    100,
                    5,
                    &[
                        ComponentRecord {
                            stable_type_id: 3,
                            payload: Vec::new(),
                        },
                        ComponentRecord {
                            stable_type_id: 9,
                            payload: vec![0xDE, 0xAD],
                        },
                    ],
                )],
            },
        );
        RegionFile { chunks }
    }

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            write!(out, "{byte:02x}").expect("写入 String 不会失败");
        }
        out
    }

    fn u16_at(bytes: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes(
            bytes[offset..offset + 2]
                .try_into()
                .expect("切片长度必须为 2"),
        )
    }

    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("切片长度必须为 4"),
        )
    }

    fn u64_at(bytes: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(
            bytes[offset..offset + 8]
                .try_into()
                .expect("切片长度必须为 8"),
        )
    }

    fn set_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn set_u64(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    /// 黄金字节：由《存档格式规格》（docs/待办/实现文档/存档格式规格.md，
    /// 2026-11-01，format_version = 1）§2 的布局生成。
    ///
    /// 生成方式：本文件 encode_world_meta 首次按规格实现后跑一次
    /// `cargo test -p game-engine --features voxel save::codec::tests::print_golden_hex -- --ignored --nocapture`
    /// 固化，之后只允许在**有意升级 format_version 并同步规格**时更新。
    /// 样例：2 个 body —— id 1 结构体 attach_to = 3（前向引用），id 3 岛 biome = 7 anchored = 1。
    /// 总长 164 B = 48 + 2 × 54 + 8。
    const GOLDEN_WORLD_META_HEX: &str = concat!(
        "5658574f524c44000100300001000000efcdab89674523013930000000000000",
        "4000000000000000020000000000000001000000000000000201000000000000",
        "8001000000000000000000000000000000000000000000000000000000000300",
        "0000000000000300000000000000000207010000000000010000000000000000",
        "000000000000000000000000000000000000000000000000000000008538633f",
        "dceb34f1",
    );

    /// 黄金字节：同一份规格 §3 的布局（format_version = 1）。
    ///
    /// 样例：3 个 chunk —— (0,0,0) content， (2,3,1) 1 条提案， (3,0,2) 1 个实体（2 个组件）；
    /// max_stable_entity_id = 100；总长 311 B。
    const GOLDEN_REGION_HEX: &str = concat!(
        "5658524547494f4e0100200003000000cf000000000000006400000000000000",
        "010001000a000000564f58454c494f00010000000000010000002a0000000000",
        "0000e90300000f00000001000000070000000300000001020301000000010000",
        "000200000004000000fdffffff1032547698badcfef9ffffff08000000090000",
        "00f6ffffff080000000600000004000000140000000c00000000000000010000",
        "0000000000010000006400000000000000050000001600000002000000030000",
        "00000000000900000002000000dead0000000000000000000000000700000020",
        "0000000000000039000000000000000200000003000000010000000100000059",
        "000000000000004400000000000000030000000000000002000000020000009d",
        "0000000000000032000000000000002ca73471cd1f9494",
    );

    #[test]
    fn world_meta_roundtrip() {
        let meta = golden_world_meta();
        let bytes = encode_world_meta(&meta).expect("world.meta 必须可编码");
        assert_eq!(bytes.len(), 48 + 2 * 54 + 8);
        assert_eq!(u16_at(&bytes, W_HEADER_LEN), WORLD_HEADER_LEN);
        assert_eq!(u16_at(&bytes, W_CODEC_ID), CODEC_ID);
        assert_eq!(u16_at(&bytes, W_FORMAT_VERSION), FORMAT_VERSION);
        assert_eq!(u32_at(&bytes, W_BODY_COUNT), 2);
        assert_eq!(u64_at(&bytes, 16), meta.seed);
        assert_eq!(u64_at(&bytes, 24), meta.world_tick);
        assert_eq!(u64_at(&bytes, 32), meta.next_stable_entity_id);

        let decoded = decode_world_meta(&bytes).expect("world.meta 必须可解码");
        assert_eq!(decoded, meta);
        assert_eq!(decoded.bodies.get(&key(1)).unwrap().attach_to, key(3));
        assert_eq!(
            decoded.bodies.get(&key(1)).unwrap().kind,
            BodyKind::Structure
        );
        assert_eq!(decoded.bodies.get(&key(3)).unwrap().kind, BodyKind::Island);
        assert_eq!(decoded.bodies.get(&key(3)).unwrap().biome, 7);
        assert_eq!(decoded.bodies.get(&key(3)).unwrap().anchored, 1);
        assert_eq!(
            encode_world_meta(&decoded).expect("重编码必须成功"),
            bytes,
            "解码后再编码必须逐字节相同"
        );
    }

    #[test]
    fn world_meta_empty_body_table_roundtrip() {
        let meta = WorldMeta {
            seed: 0,
            world_tick: 0,
            next_stable_entity_id: 1,
            bodies: BTreeMap::new(),
        };
        let bytes = encode_world_meta(&meta).expect("空 body 表合法");
        assert_eq!(bytes.len(), 48 + 8);
        assert_eq!(u32_at(&bytes, W_BODY_COUNT), 0);
        assert_eq!(decode_world_meta(&bytes).expect("空 body 表合法"), meta);
        assert_eq!(&bytes[48..56], &digest(&bytes[..48]).to_le_bytes());
    }

    #[test]
    fn region_roundtrip() {
        let region = golden_region();
        let bytes = encode_region(&region).expect("region 必须可编码");

        let index_offset = u64_at(&bytes, R_INDEX_OFFSET) as usize;
        let expected_index = 32
            + region
                .chunks
                .values()
                .map(|record| encode_chunk_record(record).expect("记录必须可编码").len())
                .sum::<usize>();
        assert_eq!(index_offset, expected_index);
        assert_eq!(
            u16_at(&bytes, 0),
            u16::from_le_bytes([REGION_MAGIC[0], REGION_MAGIC[1]])
        );
        assert_eq!(u16_at(&bytes, R_FORMAT_VERSION), FORMAT_VERSION);
        assert_eq!(u16_at(&bytes, R_HEADER_LEN), REGION_HEADER_LEN);
        assert_eq!(u32_at(&bytes, R_CHUNK_COUNT), 3);
        assert_eq!(u64_at(&bytes, R_MAX_STABLE_ENTITY_ID), 100);
        assert_eq!(bytes.len(), index_offset + 3 * 32 + 8);

        let decoded = decode_region(&bytes).expect("region 必须可解码");
        assert_eq!(decoded, region);
        assert_eq!(decoded.chunk_count(), 3);
        assert_eq!(decoded.max_stable_entity_id(), 100);
        assert_eq!(
            decoded.chunks[&ChunkKey::new(0, 0, 0)].content.as_deref(),
            Some(&b"VOXELIO\0\x01\x00"[..])
        );
        assert_eq!(
            decoded.chunks[&ChunkKey::new(2, 3, 1)].proposals[0],
            proposal(2, 4)
        );
        assert_eq!(
            encode_region(&decoded).expect("重编码必须成功"),
            bytes,
            "解码后再编码必须逐字节相同"
        );

        // 索引项：首项 offset = 32、record_len 精确、generator_version 与记录一致、reserved = 0。
        let mut expected_offset = 32u64;
        for (index, (chunk_key, record)) in region.chunks.iter().enumerate() {
            let entry = index_offset + index * 32;
            assert_eq!(u32_at(&bytes, entry), chunk_key.x as u32);
            assert_eq!(u32_at(&bytes, entry + 4), chunk_key.y as u32);
            assert_eq!(u32_at(&bytes, entry + 8), chunk_key.z as u32);
            assert_eq!(u32_at(&bytes, entry + 12), record.generator_version);
            assert_eq!(u64_at(&bytes, entry + 16), expected_offset);
            assert_eq!(
                u32_at(&bytes, entry + 24),
                encode_chunk_record(record).unwrap().len() as u32
            );
            assert_eq!(u32_at(&bytes, entry + 28), 0);
            expected_offset += u64::from(u32_at(&bytes, entry + 24));
        }
        assert_eq!(expected_offset as usize, index_offset);
    }

    #[test]
    fn region_empty_roundtrip() {
        let region = RegionFile::default();
        let bytes = encode_region(&region).expect("空 region 合法");
        assert_eq!(bytes.len(), 32 + 8);
        assert_eq!(u32_at(&bytes, R_CHUNK_COUNT), 0);
        assert_eq!(u64_at(&bytes, R_INDEX_OFFSET), 32);
        assert_eq!(u64_at(&bytes, R_MAX_STABLE_ENTITY_ID), 0);
        let decoded = decode_region(&bytes).expect("空 region 合法");
        assert!(decoded.is_empty());
        assert_eq!(decoded, region);
        assert_eq!(decoded.max_stable_entity_id(), 0);
        assert_eq!(encode_region(&decoded).unwrap(), bytes);
    }

    #[test]
    fn golden_world_meta_bytes_match_spec_v1() {
        let bytes = encode_world_meta(&golden_world_meta()).expect("world.meta 必须可编码");
        assert_eq!(bytes.len(), 164);
        assert_eq!(bytes.len() * 2, GOLDEN_WORLD_META_HEX.len());
        assert_eq!(hex(&bytes), GOLDEN_WORLD_META_HEX);
        assert_eq!(
            decode_world_meta(&bytes).expect("黄金字节必须可解码"),
            golden_world_meta()
        );
    }

    #[test]
    fn golden_region_bytes_match_spec_v1() {
        let bytes = encode_region(&golden_region()).expect("region 必须可编码");
        assert_eq!(bytes.len(), 311);
        assert_eq!(bytes.len() * 2, GOLDEN_REGION_HEX.len());
        assert_eq!(hex(&bytes), GOLDEN_REGION_HEX);
        assert_eq!(
            decode_region(&bytes).expect("黄金字节必须可解码"),
            golden_region()
        );
    }

    fn world_meta_fixture_bytes() -> Vec<u8> {
        encode_world_meta(&golden_world_meta()).expect("world.meta 必须可编码")
    }

    fn region_fixture_bytes() -> Vec<u8> {
        encode_region(&golden_region()).expect("region 必须可编码")
    }

    fn assert_world_meta_rejects(bytes: &[u8], marker: &str) {
        let err = decode_world_meta(bytes).expect_err("破坏后的 world.meta 必须被拒绝");
        assert!(
            err.to_string().contains(marker),
            "错误信息应包含 {marker:?}，实际是: {err}"
        );
    }

    fn assert_region_rejects(bytes: &[u8], marker: &str) {
        let err = decode_region(bytes).expect_err("破坏后的 region 必须被拒绝");
        assert!(
            err.to_string().contains(marker),
            "错误信息应包含 {marker:?}，实际是: {err}"
        );
    }

    #[test]
    fn world_meta_header_corruption_is_rejected() {
        let bytes = world_meta_fixture_bytes();
        assert_eq!(bytes.len(), 164);

        let cases: [(usize, u8, &str); 10] = [
            (0, 0x01, "magic"),
            (W_FORMAT_VERSION, 0x01, "format_version"),
            (W_FORMAT_VERSION, 0x02, "format_version"),
            (W_HEADER_LEN, 0x01, "header_len"),
            (W_CODEC_ID, 0x01, "codec_id"),
            (W_CODEC_ID, 0x02, "codec_id"),
            (W_FLAGS, 0x01, "flags"),
            (W_BODY_COUNT, 0x01, "file length for body_count"),
            (W_BODY_COUNT + 3, 0x80, "body_count"),
            (W_RESERVED, 0x01, "reserved"),
        ];
        for (offset, mask, marker) in cases {
            let mut corrupt = bytes.clone();
            corrupt[offset] ^= mask;
            assert_world_meta_rejects(&corrupt, marker);
        }

        let mut bad_digest = bytes.clone();
        let last = bad_digest.len() - 1;
        bad_digest[last] ^= 0x01;
        assert_world_meta_rejects(&bad_digest, "digest");

        let mut truncated = bytes.clone();
        truncated.pop();
        assert_world_meta_rejects(&truncated, "file length for body_count");

        let mut trailing = bytes;
        trailing.push(0);
        assert_world_meta_rejects(&trailing, "file length for body_count");
    }

    #[test]
    fn world_meta_body_corruption_is_rejected() {
        let bytes = world_meta_fixture_bytes();
        let body0 = usize::from(WORLD_HEADER_LEN);
        let body1 = body0 + BODY_RECORD_LEN;

        let cases: [(usize, u8, &str); 6] = [
            (body0 + 8, 0x04, "kind_tag"),
            (body0 + 9, 0x02, "attach_tag"),
            (body0 + 12, 0x01, "reserved"),
            (body0 + 14 + 7, 0x80, "voxel_size"),
            (body0 + 46, 0x02, "attach_to"),
            (body1 + 46, 0x01, "attach_to"),
        ];
        for (offset, mask, marker) in cases {
            let mut corrupt = bytes.clone();
            corrupt[offset] ^= mask;
            assert_world_meta_rejects(&corrupt, marker);
        }

        // attach_tag = body 但 attach_to 指向不存在的 body。
        let mut missing = bytes.clone();
        set_u64(&mut missing, body0 + 46, 2);
        assert_world_meta_rejects(&missing, "attach_to body");

        // id 重复。
        let mut duplicate = bytes.clone();
        let first = duplicate[body0..body0 + BODY_RECORD_LEN].to_vec();
        duplicate[body1..body1 + BODY_RECORD_LEN].copy_from_slice(&first);
        assert_world_meta_rejects(&duplicate, "body id");

        // id 非升序。
        let mut swapped = bytes.clone();
        let first = swapped[body0..body0 + BODY_RECORD_LEN].to_vec();
        let second = swapped[body1..body1 + BODY_RECORD_LEN].to_vec();
        swapped[body0..body0 + BODY_RECORD_LEN].copy_from_slice(&second);
        swapped[body1..body1 + BODY_RECORD_LEN].copy_from_slice(&first);
        assert_world_meta_rejects(&swapped, "body id");

        // 错误必须指到字段与偏移。
        let mut corrupt = bytes.clone();
        corrupt[body0 + 8] = 9;
        let err = decode_world_meta(&corrupt).expect_err("未知 kind_tag 必须拒绝");
        assert_eq!(err.path(), "world.meta.body[0].kind_tag");
        assert_eq!(err.offset(), body0 + 8);
    }

    #[test]
    fn world_meta_body_count_limit_and_extremes() {
        let mut bodies = BTreeMap::new();
        for id in 1..=u64::from(MAX_BODY_COUNT) {
            let record = island(id, 0, 0);
            bodies.insert(record.id, record);
        }
        let meta = WorldMeta {
            seed: 7,
            world_tick: 9,
            next_stable_entity_id: u64::MAX,
            bodies,
        };
        let bytes = encode_world_meta(&meta).expect("65536 条 body 合法");
        assert_eq!(bytes.len(), 48 + 65_536 * 54 + 8);
        let decoded = decode_world_meta(&bytes).expect("65536 条 body 合法");
        assert_eq!(decoded.body_count(), MAX_BODY_COUNT);
        assert_eq!(decoded.bodies.len(), 65_536);
        assert_eq!(
            decoded.bodies.get(&key(65_536)).unwrap().kind,
            BodyKind::Island
        );

        let mut too_many = meta.clone();
        let extra = island(u64::from(MAX_BODY_COUNT) + 1, 0, 0);
        too_many.bodies.insert(extra.id, extra);
        let err = encode_world_meta(&too_many).expect_err("65537 条 body 必须拒绝");
        assert!(err.to_string().contains("body_count"), "{err}");

        // 手工把 body_count 改大也要在分配前拒绝。
        let mut corrupt = bytes.clone();
        set_u32(&mut corrupt, W_BODY_COUNT, MAX_BODY_COUNT + 1);
        let err = decode_world_meta(&corrupt).expect_err("超限 body_count 必须拒绝");
        assert!(err.to_string().contains("body_count"), "{err}");

        // i64::MIN/MAX 的 voxel_size 与 drift。
        let mut extreme_record = island(1, 0, 0);
        extreme_record.voxel_size = i64::MAX;
        extreme_record.drift = [i64::MIN, i64::MAX, -1];
        let mut tiny = BTreeMap::new();
        tiny.insert(extreme_record.id, extreme_record);
        let extreme = WorldMeta {
            seed: u64::MAX,
            world_tick: u64::MAX,
            next_stable_entity_id: u64::MAX,
            bodies: tiny,
        };
        let encoded = encode_world_meta(&extreme).expect("极值合法");
        assert_eq!(decode_world_meta(&encoded).expect("极值合法"), extreme);

        for bad_voxel in [0i64, -1, i64::MIN] {
            let mut record = island(2, 0, 0);
            record.voxel_size = bad_voxel;
            let mut map = BTreeMap::new();
            map.insert(record.id, record);
            let bad = WorldMeta {
                seed: 0,
                world_tick: 0,
                next_stable_entity_id: 1,
                bodies: map,
            };
            let err = encode_world_meta(&bad).expect_err("voxel_size <= 0 必须拒绝");
            assert!(err.to_string().contains("voxel_size"), "{err}");
        }

        // 解码侧：把黄金样例的 voxel_size 改成 i64::MIN。
        let mut corrupt = world_meta_fixture_bytes();
        set_u64(
            &mut corrupt,
            usize::from(WORLD_HEADER_LEN) + B_VOXEL_SIZE,
            i64::MIN as u64,
        );
        assert_world_meta_rejects(&corrupt, "voxel_size");
    }

    #[test]
    fn world_meta_encode_rejects_inconsistent_keys_and_attach() {
        // map 键与记录 id 不一致。
        let mut record = island(1, 0, 0);
        record.id = key(2);
        let mut map = BTreeMap::new();
        map.insert(key(1), record);
        let meta = WorldMeta {
            seed: 0,
            world_tick: 0,
            next_stable_entity_id: 1,
            bodies: map,
        };
        let err = encode_world_meta(&meta).expect_err("键不一致必须拒绝");
        assert!(err.to_string().contains("body map key"), "{err}");

        // attach_to 悬空。
        let mut map = BTreeMap::new();
        let attached = structure(1, 9);
        map.insert(attached.id, attached);
        let meta = WorldMeta {
            seed: 0,
            world_tick: 0,
            next_stable_entity_id: 1,
            bodies: map,
        };
        let err = encode_world_meta(&meta).expect_err("悬空 attach_to 必须拒绝");
        assert!(err.to_string().contains("attach_to body"), "{err}");

        // attach_tag != body 时 attach_to 必须为 0。
        let mut record = island(1, 0, 0);
        record.attach_to = key(2);
        let mut map = BTreeMap::new();
        map.insert(record.id, record);
        let meta = WorldMeta {
            seed: 0,
            world_tick: 0,
            next_stable_entity_id: 1,
            bodies: map,
        };
        let err = encode_world_meta(&meta).expect_err("非 body attach 的 attach_to 必须拒绝");
        assert!(err.to_string().contains("attach_to"), "{err}");

        // attach_tag = body 但 attach_to = 0。
        let mut record = structure(1, 1);
        record.attach_to = BodyKey::INVALID;
        let mut map = BTreeMap::new();
        map.insert(record.id, record);
        let meta = WorldMeta {
            seed: 0,
            world_tick: 0,
            next_stable_entity_id: 1,
            bodies: map,
        };
        let err = encode_world_meta(&meta).expect_err("attach_to = 0 必须拒绝");
        assert!(err.to_string().contains("attach_to"), "{err}");
    }

    fn raw_region(entries: &[(ChunkKey, u32, Vec<u8>)], max_id: u64) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.bytes(&REGION_MAGIC);
        writer.u16(FORMAT_VERSION);
        writer.u16(REGION_HEADER_LEN);
        writer.u32(entries.len() as u32);
        let index_offset: usize = 32
            + entries
                .iter()
                .map(|(_, _, record)| record.len())
                .sum::<usize>();
        writer.u64(index_offset as u64);
        writer.u64(max_id);
        for (_, _, record) in entries {
            writer.bytes(record);
        }
        let mut offset = 32usize;
        for (chunk_key, generator_version, record) in entries {
            writer.i32(chunk_key.x);
            writer.i32(chunk_key.y);
            writer.i32(chunk_key.z);
            writer.u32(*generator_version);
            writer.u64(offset as u64);
            writer.u32(record.len() as u32);
            writer.u32(0);
            offset += record.len();
        }
        let digest_value = digest(&writer.buf);
        writer.u64(digest_value);
        writer.into_vec()
    }

    fn raw_record(
        content: Option<&[u8]>,
        proposals: &[ProposalRecord],
        entities: &[EntityRecord],
    ) -> Vec<u8> {
        let mut writer = Writer::new();
        writer.u16(1);
        writer.u16(if content.is_some() { 1 } else { 0 });
        if let Some(content) = content {
            writer.u32(content.len() as u32);
            writer.bytes(content);
        }
        writer.u32(proposals.len() as u32);
        for proposal in proposals {
            writer.u16(proposal.feature_type);
            writer.u16(0);
            writer.u32(proposal.seq);
            writer.i32(proposal.priority);
            writer.u64(proposal.tie_break);
            for value in proposal.origin {
                writer.i32(value);
            }
            for value in proposal.bounds_min {
                writer.i32(value);
            }
            for value in proposal.bounds_max {
                writer.i32(value);
            }
        }
        writer.u32(entities.len() as u32);
        for entity in entities {
            writer.u64(entity.stable_entity_id);
            writer.u32(entity.stable_type_id);
            writer.u32(entity.payload.len() as u32);
            writer.bytes(&entity.payload);
        }
        writer.into_vec()
    }

    #[test]
    fn region_header_corruption_is_rejected() {
        let bytes = region_fixture_bytes();

        let cases: [(usize, u8, &str); 7] = [
            (0, 0x01, "magic"),
            (R_FORMAT_VERSION, 0x01, "format_version"),
            (R_FORMAT_VERSION, 0x02, "format_version"),
            (R_HEADER_LEN, 0x01, "header_len"),
            (R_CHUNK_COUNT, 0x01, "chunk_count"),
            (R_INDEX_OFFSET, 0x01, "index_offset"),
            (R_MAX_STABLE_ENTITY_ID, 0x01, "max_stable_entity_id"),
        ];
        for (offset, mask, marker) in cases {
            let mut corrupt = bytes.clone();
            corrupt[offset] ^= mask;
            assert_region_rejects(&corrupt, marker);
        }

        let mut too_many = bytes.clone();
        set_u32(&mut too_many, R_CHUNK_COUNT, MAX_CHUNK_COUNT + 1);
        assert_region_rejects(&too_many, "chunk_count");

        let mut bad_digest = bytes.clone();
        let last = bad_digest.len() - 1;
        bad_digest[last] ^= 0x01;
        assert_region_rejects(&bad_digest, "digest");

        let mut truncated = bytes.clone();
        truncated.pop();
        assert_region_rejects(&truncated, "index_offset");

        let mut trailing = bytes;
        trailing.push(0);
        assert_region_rejects(&trailing, "index_offset");
    }

    #[test]
    fn region_index_corruption_is_rejected() {
        let bytes = region_fixture_bytes();
        let index_offset = u64_at(&bytes, R_INDEX_OFFSET) as usize;
        let entry0 = index_offset;
        let entry1 = index_offset + 32;

        // 第二个 ChunkKey 变成负数：键序破坏。
        let mut corrupt = bytes.clone();
        corrupt[entry1 + 3] ^= 0x80;
        assert_region_rejects(&corrupt, "ChunkKey");

        // 重复 ChunkKey。
        let mut corrupt = bytes.clone();
        corrupt[entry1..entry1 + 12].copy_from_slice(&bytes[entry0..entry0 + 12]);
        assert_region_rejects(&corrupt, "ChunkKey");

        // 首项 record_offset 必须是 32。
        let mut corrupt = bytes.clone();
        set_u64(&mut corrupt, entry0 + I_RECORD_OFFSET, 33);
        assert_region_rejects(&corrupt, "record_offset");

        // 第二项 record_offset 必须接在前一条记录之后。
        let mut corrupt = bytes.clone();
        let second_offset = u64_at(&bytes, entry1 + I_RECORD_OFFSET);
        set_u64(&mut corrupt, entry1 + I_RECORD_OFFSET, second_offset + 1);
        assert_region_rejects(&corrupt, "record_offset");

        // record_len 不得为 0。
        let mut corrupt = bytes.clone();
        set_u32(&mut corrupt, entry1 + I_RECORD_LEN, 0);
        assert_region_rejects(&corrupt, "record_len");

        // record_len 不得超过 16 MiB。
        let mut corrupt = bytes.clone();
        set_u32(&mut corrupt, entry1 + I_RECORD_LEN, MAX_RECORD_LEN + 1);
        assert_region_rejects(&corrupt, "record_len");

        // 索引项 reserved 必须为 0。
        let mut corrupt = bytes.clone();
        corrupt[entry0 + 28] ^= 0x01;
        assert_region_rejects(&corrupt, "reserved");
    }

    #[test]
    fn region_record_body_bit_flip_is_rejected() {
        // 黄金 region 第一条记录：content 段之后是实体表，实体 payload 的组件外层
        // reserved u16 在记录 + 44（= 32 + 4+4+10+4+4+8+4+4+4+ ... 见布局）。
        let bytes = region_fixture_bytes();
        let index_offset = u64_at(&bytes, R_INDEX_OFFSET) as usize;
        let record0 = u64_at(&bytes, index_offset + I_RECORD_OFFSET) as usize;
        // record: 4 头 + 4 content_len + 10 content + 4 proposal_count + 4 entity_count
        //         + 8 id + 4 type + 4 payload_len = 42，payload 的 component_count 2 B，
        //         reserved 在 payload 偏移 2，即记录 + 42 + 2 = 记录 + 44。
        let reserved = record0 + 44;
        assert_eq!(u16_at(&bytes, reserved), 0);
        let mut corrupt = bytes.clone();
        corrupt[reserved] ^= 0x01;
        assert_region_rejects(&corrupt, "reserved");
    }

    #[test]
    fn region_record_header_corruption_is_rejected() {
        let bytes = region_fixture_bytes();
        let index_offset = u64_at(&bytes, R_INDEX_OFFSET) as usize;
        let record0 = u64_at(&bytes, index_offset + I_RECORD_OFFSET) as usize;
        assert_eq!(record0, 32);

        let mut corrupt = bytes.clone();
        corrupt[record0] ^= 0x01; // record_version 1 -> 0
        assert_region_rejects(&corrupt, "record_version");

        let mut corrupt = bytes.clone();
        corrupt[record0 + 2] ^= 0x02; // flags bit1
        assert_region_rejects(&corrupt, "flags");
    }

    #[test]
    fn region_index_offset_must_equal_record_end() {
        let empty = encode_region(&RegionFile::default()).expect("空 region 合法");
        assert_eq!(empty.len(), 32 + DIGEST_LEN);

        // chunk_count = 0 时 index_offset 必须是 32；伪造 40 并保持文件长度自洽。
        let mut corrupt = empty[..32].to_vec();
        set_u64(&mut corrupt, R_INDEX_OFFSET, 40);
        corrupt.extend_from_slice(&[0u8; 8]);
        let err = decode_region(&corrupt).expect_err("index_offset 必须等于 32 + Σ record_len");
        assert!(err.to_string().contains("index_offset"), "{err}");
    }

    #[test]
    fn region_chunk_count_limit_is_enforced() {
        let mut chunks = BTreeMap::new();
        for index in 0..MAX_CHUNK_COUNT {
            let chunk_key = ChunkKey::new(index as i32, 0, 0);
            chunks.insert(
                chunk_key,
                ChunkRecord {
                    generator_version: index,
                    content: None,
                    proposals: Vec::new(),
                    entities: vec![entity(u64::from(index) + 1, 1, &[])],
                },
            );
        }
        let region = RegionFile { chunks };
        let bytes = encode_region(&region).expect("64 块合法");
        let decoded = decode_region(&bytes).expect("64 块合法");
        assert_eq!(decoded, region);
        assert_eq!(decoded.chunk_count(), MAX_CHUNK_COUNT);
        assert_eq!(decoded.max_stable_entity_id(), 64);

        let mut too_many = region.clone();
        too_many.chunks.insert(
            ChunkKey::new(100, 0, 0),
            ChunkRecord {
                generator_version: 0,
                content: None,
                proposals: Vec::new(),
                entities: vec![entity(65, 1, &[])],
            },
        );
        let err = encode_region(&too_many).expect_err("65 块必须拒绝");
        assert!(err.to_string().contains("chunk_count"), "{err}");
    }

    #[test]
    fn region_coordinate_extremes_roundtrip_and_region_of_uses_euclid() {
        let extreme_min = ChunkKey::new(i32::MIN, i32::MIN, i32::MIN);
        let extreme_max = ChunkKey::new(i32::MAX, i32::MAX, i32::MAX);
        let mut chunks = BTreeMap::new();
        chunks.insert(
            extreme_min,
            ChunkRecord {
                generator_version: u32::MAX,
                content: Some(vec![0x11]),
                proposals: Vec::new(),
                entities: Vec::new(),
            },
        );
        chunks.insert(
            extreme_max,
            ChunkRecord {
                generator_version: 0,
                content: Some(vec![0x22]),
                proposals: Vec::new(),
                entities: Vec::new(),
            },
        );
        let region = RegionFile { chunks };
        let bytes = encode_region(&region).expect("i32 极值坐标必须可编码");
        let decoded = decode_region(&bytes).expect("i32 极值坐标必须可解码");
        assert_eq!(decoded, region);
        assert_eq!(decoded.chunks.len(), 2);

        // region_of 必须用 div_euclid（负坐标不是截断除法）。
        assert_eq!(
            region_of(ChunkKey::new(i32::MIN, i32::MAX, -1)),
            ChunkKey::new(-536_870_912, 536_870_911, -1)
        );
        assert_eq!(region_of(ChunkKey::new(0, 3, 4)), ChunkKey::new(0, 0, 1));
    }

    #[test]
    fn region_rejects_duplicate_entity_ids() {
        let record = encode_chunk_record(&ChunkRecord {
            generator_version: 0,
            content: None,
            proposals: Vec::new(),
            entities: vec![entity(7, 1, &[])],
        })
        .expect("单条记录必须可编码");
        let bytes = raw_region(
            &[
                (ChunkKey::new(0, 0, 0), 0, record.clone()),
                (ChunkKey::new(1, 0, 0), 0, record),
            ],
            7,
        );
        let err = decode_region(&bytes).expect_err("同 region 重复实体 ID 必须拒绝");
        assert!(err.to_string().contains("entity id in region"), "{err}");

        // 编码侧同样拒绝。
        let mut chunks = BTreeMap::new();
        for x in 0..2 {
            chunks.insert(
                ChunkKey::new(x, 0, 0),
                ChunkRecord {
                    generator_version: 0,
                    content: None,
                    proposals: Vec::new(),
                    entities: vec![entity(7, 1, &[])],
                },
            );
        }
        let region = RegionFile { chunks };
        let err = encode_region(&region).expect_err("同 region 重复实体 ID 必须拒绝");
        assert!(err.to_string().contains("entity id in region"), "{err}");
    }

    #[test]
    fn chunk_record_rejects_invalid_combinations() {
        let empty = raw_record(None, &[], &[]);
        let err = decode_chunk_record(&empty, 1).expect_err("三者全空的记录必须拒绝");
        assert!(err.to_string().contains("empty record"), "{err}");

        assert_eq!(
            raw_record(None, &[proposal(1, 0)], &[]).len(),
            4 + 4 + PROPOSAL_RECORD_LEN + 4,
            "ProposalRecord 线格式必须是 56 B（§3.3）"
        );

        let conflict = raw_record(Some(&b"content"[..]), &[proposal(1, 0)], &[]);
        let err = decode_chunk_record(&conflict, 1).expect_err("content 与提案互斥");
        assert!(err.to_string().contains("content with proposals"), "{err}");

        let empty_record = ChunkRecord {
            generator_version: 0,
            content: None,
            proposals: Vec::new(),
            entities: Vec::new(),
        };
        assert!(encode_chunk_record(&empty_record)
            .expect_err("空记录不得写入")
            .to_string()
            .contains("empty record"));

        let conflict_record = ChunkRecord {
            generator_version: 0,
            content: Some(vec![1]),
            proposals: vec![proposal(1, 0)],
            entities: Vec::new(),
        };
        assert!(encode_chunk_record(&conflict_record)
            .expect_err("content 与提案互斥")
            .to_string()
            .contains("content with proposals"));

        let mut bad_bounds = proposal(1, 0);
        bad_bounds.bounds_max[0] = bad_bounds.bounds_min[0];
        let err = decode_chunk_record(&raw_record(None, &[bad_bounds], &[]), 1)
            .expect_err("非法包围盒必须拒绝");
        assert!(err.to_string().contains("bounds"), "{err}");
        let bad_bounds_record = ChunkRecord {
            generator_version: 0,
            content: None,
            proposals: vec![bad_bounds],
            entities: Vec::new(),
        };
        assert!(encode_chunk_record(&bad_bounds_record)
            .expect_err("非法包围盒必须拒绝")
            .to_string()
            .contains("bounds"));

        // 提案 reserved u16 必须为 0（record 布局：4 B 头 + 4 B count + 2 B feature_type）。
        let mut reserved = raw_record(None, &[proposal(1, 0)], &[]);
        reserved[10] = 1;
        assert!(decode_chunk_record(&reserved, 1)
            .expect_err("提案 reserved 非 0 必须拒绝")
            .to_string()
            .contains("reserved"));

        // 提案必须按 (feature_type, seq) 升序。
        let out_of_order = raw_record(None, &[proposal(2, 0), proposal(1, 0)], &[]);
        assert!(decode_chunk_record(&out_of_order, 1)
            .expect_err("提案乱序必须拒绝")
            .to_string()
            .contains("proposal"));

        // 记录体必须恰好消费完；多一字节 / 少一字节都算损坏。
        let valid = encode_chunk_record(&ChunkRecord {
            generator_version: 2,
            content: Some(vec![1, 2, 3]),
            proposals: Vec::new(),
            entities: vec![entity(1, 1, &[])],
        })
        .expect("记录必须可编码");
        let mut trailing = valid.clone();
        trailing.push(0);
        assert!(decode_chunk_record(&trailing, 2)
            .expect_err("尾随字节必须拒绝")
            .to_string()
            .contains("trailing"));
        let mut truncated = valid;
        truncated.pop();
        assert!(decode_chunk_record(&truncated, 2).is_err());
    }

    #[test]
    fn chunk_record_rejects_invalid_entities() {
        let zero_type = EntityRecord {
            stable_entity_id: 1,
            stable_type_id: 0,
            payload: encode_component_table(&[]).expect("空组件表合法"),
        };
        assert!(decode_chunk_record(&raw_record(None, &[], &[zero_type]), 1)
            .expect_err("stable_type_id = 0 必须拒绝")
            .to_string()
            .contains("stable_type_id"));

        // 组件外层 reserved 非 0（component_count = 0, reserved = 1）。
        let bad_payload = EntityRecord {
            stable_entity_id: 1,
            stable_type_id: 1,
            payload: vec![0, 0, 1, 0],
        };
        assert!(
            decode_chunk_record(&raw_record(None, &[], &[bad_payload]), 1)
                .expect_err("组件 reserved 非 0 必须拒绝")
                .to_string()
                .contains("reserved")
        );

        let duplicate = raw_record(None, &[], &[entity(1, 1, &[]), entity(1, 2, &[])]);
        assert!(decode_chunk_record(&duplicate, 1)
            .expect_err("同块实体 ID 重复必须拒绝")
            .to_string()
            .contains("entity id in chunk"));

        let out_of_order = raw_record(None, &[], &[entity(2, 1, &[]), entity(1, 2, &[])]);
        assert!(decode_chunk_record(&out_of_order, 1)
            .expect_err("同块实体 ID 乱序必须拒绝")
            .to_string()
            .contains("entity id in chunk"));

        let zero_type_record = ChunkRecord {
            generator_version: 0,
            content: None,
            proposals: Vec::new(),
            entities: vec![EntityRecord {
                stable_entity_id: 1,
                stable_type_id: 0,
                payload: encode_component_table(&[]).unwrap(),
            }],
        };
        assert!(encode_chunk_record(&zero_type_record)
            .expect_err("stable_type_id = 0 必须拒绝")
            .to_string()
            .contains("stable_type_id"));
    }

    #[test]
    fn entity_payload_is_opaque_and_preserved() {
        let payload = encode_component_table(&[ComponentRecord {
            stable_type_id: 9999,
            payload: vec![0xDE, 0xAD, 0xBE, 0xEF],
        }])
        .expect("未知组件必须原样编码");
        let record = ChunkRecord {
            generator_version: 3,
            content: None,
            proposals: Vec::new(),
            entities: vec![EntityRecord {
                stable_entity_id: 5,
                stable_type_id: 4242,
                payload: payload.clone(),
            }],
        };
        let bytes = encode_chunk_record(&record).expect("未知原型必须原样编码");
        let decoded = decode_chunk_record(&bytes, 3).expect("未知原型必须原样解码");
        assert_eq!(decoded, record);
        assert_eq!(decoded.entities[0].payload, payload, "未知组件字节必须不变");
        assert_eq!(encode_chunk_record(&decoded).unwrap(), bytes);
    }

    #[test]
    fn component_table_roundtrip_and_limits() {
        let components = vec![
            ComponentRecord {
                stable_type_id: 9,
                payload: vec![0xAA],
            },
            ComponentRecord {
                stable_type_id: 3,
                payload: Vec::new(),
            },
        ];
        let bytes = encode_component_table(&components).expect("组件表必须可编码");
        assert_eq!(u16_at(&bytes, 0), 2);
        assert_eq!(u16_at(&bytes, 2), 0);
        let decoded = decode_component_table(&bytes).expect("组件表必须可解码");
        assert_eq!(
            decoded,
            vec![components[1].clone(), components[0].clone()],
            "编码必须按 stable_type_id 升序"
        );
        assert_eq!(encode_component_table(&decoded).unwrap(), bytes);

        let empty = encode_component_table(&[]).expect("空组件表合法");
        assert_eq!(empty, vec![0, 0, 0, 0]);
        assert!(decode_component_table(&empty).unwrap().is_empty());

        let err = encode_component_table(&[ComponentRecord {
            stable_type_id: 0,
            payload: Vec::new(),
        }])
        .expect_err("0 组件 ID 必须拒绝");
        assert!(err.to_string().contains("stable_type_id"), "{err}");

        let err = encode_component_table(&[
            ComponentRecord {
                stable_type_id: 1,
                payload: Vec::new(),
            },
            ComponentRecord {
                stable_type_id: 1,
                payload: vec![1],
            },
        ])
        .expect_err("重复组件 ID 必须拒绝");
        assert!(err.to_string().contains("stable_type_id"), "{err}");

        let oversize = vec![0u8; MAX_COMPONENT_PAYLOAD_LEN as usize + 1];
        let err = encode_component_table(&[ComponentRecord {
            stable_type_id: 1,
            payload: oversize,
        }])
        .expect_err("组件负载超过 1 MiB 必须拒绝");
        assert!(err.to_string().contains("payload_len"), "{err}");
    }

    #[test]
    fn component_table_decode_rejects_invalid_bytes() {
        fn component_bytes(ids: &[u32]) -> Vec<u8> {
            let mut raw = Vec::new();
            raw.extend_from_slice(&(ids.len() as u16).to_le_bytes());
            raw.extend_from_slice(&0u16.to_le_bytes());
            for id in ids {
                raw.extend_from_slice(&id.to_le_bytes());
                raw.extend_from_slice(&0u32.to_le_bytes());
            }
            raw
        }

        let mut reserved = encode_component_table(&[ComponentRecord {
            stable_type_id: 1,
            payload: Vec::new(),
        }])
        .unwrap();
        reserved[2] = 1;
        assert!(decode_component_table(&reserved)
            .expect_err("reserved 非 0 必须拒绝")
            .to_string()
            .contains("reserved"));

        let mut trailing = encode_component_table(&[]).unwrap();
        trailing.push(0);
        assert!(decode_component_table(&trailing)
            .expect_err("尾随字节必须拒绝")
            .to_string()
            .contains("trailing"));

        assert!(decode_component_table(&component_bytes(&[0]))
            .expect_err("0 组件 ID 必须拒绝")
            .to_string()
            .contains("stable_type_id"));

        assert!(decode_component_table(&component_bytes(&[2, 1]))
            .expect_err("组件乱序必须拒绝")
            .to_string()
            .contains("stable_type_id"));

        assert!(decode_component_table(&component_bytes(&[1, 1]))
            .expect_err("重复组件必须拒绝")
            .to_string()
            .contains("stable_type_id"));

        let mut oversize = Vec::new();
        oversize.extend_from_slice(&1u16.to_le_bytes());
        oversize.extend_from_slice(&0u16.to_le_bytes());
        oversize.extend_from_slice(&1u32.to_le_bytes());
        oversize.extend_from_slice(&(MAX_COMPONENT_PAYLOAD_LEN + 1).to_le_bytes());
        assert!(decode_component_table(&oversize)
            .expect_err("超长组件负载必须拒绝")
            .to_string()
            .contains("payload_len"));

        assert!(decode_component_table(&[1, 0, 0, 0])
            .expect_err("截断必须拒绝")
            .to_string()
            .contains("truncated"));
    }

    #[test]
    fn record_size_limit_is_enforced() {
        let record = ChunkRecord {
            generator_version: 0,
            content: Some(vec![0u8; usize::try_from(MAX_RECORD_LEN).unwrap() + 1]),
            proposals: Vec::new(),
            entities: Vec::new(),
        };
        let err = encode_chunk_record(&record).expect_err("record_len > 16 MiB 必须拒绝");
        assert!(err.to_string().contains("record_len"), "{err}");
    }

    #[test]
    fn encoding_is_deterministic_and_order_independent() {
        let meta = golden_world_meta();
        let meta_bytes = encode_world_meta(&meta).expect("world.meta 必须可编码");
        assert_eq!(
            encode_world_meta(&meta).expect("同输入必须同字节"),
            meta_bytes
        );
        assert_eq!(
            encode_world_meta(&decode_world_meta(&meta_bytes).expect("必须可解码"))
                .expect("解码后再编码"),
            meta_bytes
        );

        // BTreeMap 插入顺序不影响字节。
        let mut reversed_bodies = BTreeMap::new();
        for (body_key, body) in meta.bodies.iter().rev() {
            reversed_bodies.insert(*body_key, *body);
        }
        let reversed = WorldMeta {
            bodies: reversed_bodies,
            ..meta.clone()
        };
        assert_eq!(
            encode_world_meta(&reversed).expect("反序插入必须同字节"),
            meta_bytes
        );

        // 记录内提案 / 实体按规格键重排；乱序传入必须与升序逐字节相同。
        let sorted = ChunkRecord {
            generator_version: 9,
            content: None,
            proposals: vec![proposal(1, 0), proposal(2, 0)],
            entities: vec![entity(1, 1, &[]), entity(2, 2, &[])],
        };
        let shuffled = ChunkRecord {
            generator_version: 9,
            content: None,
            proposals: vec![proposal(2, 0), proposal(1, 0)],
            entities: vec![entity(2, 2, &[]), entity(1, 1, &[])],
        };
        let canonical = encode_chunk_record(&sorted).expect("记录必须可编码");
        assert_eq!(
            encode_chunk_record(&shuffled).expect("乱序输入必须同字节"),
            canonical
        );
        let decoded = decode_chunk_record(&canonical, 9).expect("记录必须可解码");
        assert_eq!(decoded.proposals, vec![proposal(1, 0), proposal(2, 0)]);
        assert_eq!(
            decoded
                .entities
                .iter()
                .map(|entity| entity.stable_entity_id)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(encode_chunk_record(&decoded).unwrap(), canonical);

        let region = golden_region();
        let region_bytes = encode_region(&region).expect("region 必须可编码");
        assert_eq!(
            encode_region(&region).expect("同输入必须同字节"),
            region_bytes
        );
        assert_eq!(
            encode_region(&decode_region(&region_bytes).expect("必须可解码"))
                .expect("解码后再编码"),
            region_bytes
        );
    }

    #[test]
    fn non_island_fields_are_normalized_on_write() {
        let mut ship = island(7, 0, 0);
        ship.kind = BodyKind::Ship;
        ship.biome = 9;
        ship.anchored = 1;
        ship.drift = [1, 2, 3];
        let mut bodies = BTreeMap::new();
        bodies.insert(ship.id, ship);
        let meta = WorldMeta {
            seed: 1,
            world_tick: 2,
            next_stable_entity_id: 8,
            bodies,
        };

        if cfg!(debug_assertions) {
            // debug 下归一化前先断言：写方传脏值必须先炸，不能静默丢失。
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| encode_world_meta(&meta)));
            let payload = outcome.expect_err("debug 构建必须在归一化前断言");
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("");
            assert!(
                message.contains("must be normalized"),
                "panic 信息应说明写方未归零: {message:?}"
            );
        } else {
            // release 下没有 debug_assert：写侧归一化必须把脏值写成 0。
            let bytes = encode_world_meta(&meta).expect("release 下写侧归一化必须成功");
            let decoded = decode_world_meta(&bytes).expect("必须可解码");
            let record = &decoded.bodies[&key(7)];
            assert_eq!(record.biome, 0);
            assert_eq!(record.anchored, 0);
            assert_eq!(record.drift, [0; 3]);
        }

        // 写方按约定归零后，debug/release 都必须原样写出 0。
        let mut clean = meta.clone();
        {
            let record = clean.bodies.get_mut(&key(7)).expect("刚插入的 body");
            record.biome = 0;
            record.anchored = 0;
            record.drift = [0; 3];
        }
        let bytes = encode_world_meta(&clean).expect("归零后必须可编码");
        let decoded = decode_world_meta(&bytes).expect("必须可解码");
        let record = &decoded.bodies[&key(7)];
        assert_eq!(record.biome, 0);
        assert_eq!(record.anchored, 0);
        assert_eq!(record.drift, [0; 3]);
    }

    #[test]
    fn ship_with_body_attach_is_rejected_on_write() {
        let mut ship = island(7, 0, 0);
        ship.kind = BodyKind::Ship;
        ship.attach = AttachTag::Body;
        ship.attach_to = key(9);
        let mut bodies = BTreeMap::new();
        let anchor = island(9, 0, 0);
        bodies.insert(anchor.id, anchor);
        bodies.insert(ship.id, ship);
        let meta = WorldMeta {
            seed: 0,
            world_tick: 0,
            next_stable_entity_id: 10,
            bodies,
        };

        let err = encode_world_meta(&meta).expect_err("Ship 不得 attach_tag = Body");
        let message = err.to_string();
        assert!(message.contains("attach_tag"), "{message}");
        assert!(
            message.contains("body id 7"),
            "错误必须指明 body id: {message}"
        );
        assert!(
            message.contains("Structure"),
            "错误应说明只有 Structure 可以 attach_tag != Free: {message}"
        );
    }

    // __TESTS_END__
}
