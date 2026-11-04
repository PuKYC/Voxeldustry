//! world.meta 文件（§2）的编解码：头字段、体表与摘要校验。

use super::reader::{field_path, indexed_path, read_u64_at, u32_len, Reader, Writer};
use crate::body_key::BodyKey;
use crate::identity::StableEntityId;
use crate::save::format::{
    digest, AttachTag, BodyKind, BodyRecord, WorldMeta, BODY_RECORD_LEN, CODEC_ID, DIGEST_LEN,
    FORMAT_VERSION, MAX_BODY_COUNT, WORLD_HEADER_LEN, WORLD_MAGIC,
};
use crate::save::{SaveError, SaveErrorKind};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// 线格式偏移常量（§2 / §3）：既用于报错定位，也让编码顺序与规格表一一对应。
// ---------------------------------------------------------------------------

pub(crate) const W_FORMAT_VERSION: usize = 8;
pub(crate) const W_HEADER_LEN: usize = 10;
pub(crate) const W_CODEC_ID: usize = 12;
pub(crate) const W_FLAGS: usize = 14;
pub(crate) const W_BODY_COUNT: usize = 40;
pub(crate) const W_RESERVED: usize = 44;

pub(crate) const B_ID: usize = 0;
pub(crate) const B_ATTACH: usize = 9;
pub(crate) const B_VOXEL_SIZE: usize = 14;
pub(crate) const B_ATTACH_TO: usize = 46;

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
