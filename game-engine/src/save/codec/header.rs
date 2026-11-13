//! world.meta 文件（§2）的编解码：头字段、体表与摘要校验。

use super::reader::{field_path, indexed_path, read_u64_at, u32_len, Reader, Writer};
use crate::body_key::BodyKey;
use crate::identity::StableEntityId;
use crate::input::actions::MAX_INPUT_CHANNELS;
use crate::modding::ParamRecord;
use crate::save::format::{
    digest, ActionRecord, AttachTag, BodyKind, BodyRecord, ModRecord, SavedWith, WorldMeta,
    BODY_RECORD_LEN, CODEC_ID, DIGEST_LEN, FORMAT_VERSION, MAX_ACTION_COUNT, MAX_ACTION_NAME_LEN,
    MAX_BODY_COUNT, MAX_GAME_VERSION_LEN, MAX_MOD_COUNT, MAX_MOD_ID_LEN, MAX_PARAM_COUNT,
    MAX_PARAM_KEY_LEN, MAX_PLUGIN_COUNT, MAX_PLUGIN_ID_LEN, MIN_SUPPORTED_FORMAT_VERSION,
    WORLD_HEADER_LEN, WORLD_MAGIC,
};
use crate::save::{SaveError, SaveErrorKind};
use std::collections::{BTreeMap, BTreeSet};

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

    encode_extension(&mut writer, meta)?;
    let digest_value = digest(&writer.buf);
    writer.u64(digest_value);
    Ok(writer.into_vec())
}

/// 编码 v2 追加段：mod 清单 + 世界参数 + 动作表（均严格升序）+ 插件清单（装配顺序）。
fn encode_extension(writer: &mut Writer, meta: &WorldMeta) -> Result<(), SaveError> {
    encode_mods(writer, &meta.mods)?;
    encode_params(writer, &meta.params)?;
    encode_actions(writer, &meta.actions)?;
    encode_plugins(writer, &meta.plugins)?;
    encode_saved_with(writer, &meta.saved_with)?;
    Ok(())
}

/// 编码构建身份段：u16 version_len + game_version + u32 engine_api。
///
/// 字符串允许为空（表示「未知」，只出现在测试夹具里），但长度受限且不含控制字符。
fn encode_saved_with(writer: &mut Writer, saved: &SavedWith) -> Result<(), SaveError> {
    let offset = writer.len();
    if saved.game_version.len() > usize::from(MAX_GAME_VERSION_LEN) {
        return Err(SaveError::new(
            "world.meta.saved_with.game_version",
            offset,
            SaveErrorKind::TooLarge {
                what: "game_version length",
                value: saved.game_version.len() as u64,
                max: u64::from(MAX_GAME_VERSION_LEN),
            },
        ));
    }
    if saved.game_version.chars().any(char::is_control) {
        return Err(SaveError::new(
            "world.meta.saved_with.game_version",
            offset,
            SaveErrorKind::InvalidString {
                what: "game_version",
            },
        ));
    }
    writer.u16(u16::try_from(saved.game_version.len()).expect("长度已校验"));
    writer.bytes(saved.game_version.as_bytes());
    writer.u32(saved.engine_api);
    Ok(())
}

/// 编码 mod 清单段：count + 每条 (id_len + id + version)。
fn encode_mods(writer: &mut Writer, mods: &[ModRecord]) -> Result<(), SaveError> {
    let mod_count = u32::try_from(mods.len()).unwrap_or(u32::MAX);
    if mod_count > MAX_MOD_COUNT {
        return Err(SaveError::new(
            "world.meta.mod_count",
            writer.len(),
            SaveErrorKind::TooLarge {
                what: "mod_count",
                value: u64::from(mod_count),
                max: u64::from(MAX_MOD_COUNT),
            },
        ));
    }
    writer.u32(mod_count);
    let mut previous: Option<&str> = None;
    for (index, record) in mods.iter().enumerate() {
        let path = indexed_path("world.meta", "mod", index);
        validate_ordered_string(
            &path,
            "id",
            &record.id,
            previous,
            MAX_MOD_ID_LEN,
            writer.len(),
        )?;
        previous = Some(&record.id);
        writer.u16(u16::try_from(record.id.len()).expect("长度已校验"));
        writer.bytes(record.id.as_bytes());
        writer.u32(record.version);
    }
    Ok(())
}

/// 编码世界参数段：count + 每条 (key_len + key + ty + scale + value)。
fn encode_params(writer: &mut Writer, params: &[ParamRecord]) -> Result<(), SaveError> {
    let param_count = u32::try_from(params.len()).unwrap_or(u32::MAX);
    if param_count > MAX_PARAM_COUNT {
        return Err(SaveError::new(
            "world.meta.param_count",
            writer.len(),
            SaveErrorKind::TooLarge {
                what: "param_count",
                value: u64::from(param_count),
                max: u64::from(MAX_PARAM_COUNT),
            },
        ));
    }
    writer.u32(param_count);
    let mut previous: Option<&str> = None;
    for (index, record) in params.iter().enumerate() {
        let path = indexed_path("world.meta", "param", index);
        validate_ordered_string(
            &path,
            "key",
            &record.key,
            previous,
            MAX_PARAM_KEY_LEN,
            writer.len(),
        )?;
        previous = Some(&record.key);
        encode_param_record(writer, &path, record)?;
    }
    Ok(())
}

/// 编码动作表段：count + 每条 (name_len + name + action + channel_bit)。
fn encode_actions(writer: &mut Writer, actions: &[ActionRecord]) -> Result<(), SaveError> {
    let action_count = u32::try_from(actions.len()).unwrap_or(u32::MAX);
    if action_count > MAX_ACTION_COUNT {
        return Err(SaveError::new(
            "world.meta.action_count",
            writer.len(),
            SaveErrorKind::TooLarge {
                what: "action_count",
                value: u64::from(action_count),
                max: u64::from(MAX_ACTION_COUNT),
            },
        ));
    }
    writer.u32(action_count);
    let mut previous: Option<&str> = None;
    for (index, record) in actions.iter().enumerate() {
        let path = indexed_path("world.meta", "action", index);
        validate_ordered_string(
            &path,
            "name",
            &record.name,
            previous,
            MAX_ACTION_NAME_LEN,
            writer.len(),
        )?;
        previous = Some(&record.name);
        encode_action_record(writer, &path, record)?;
    }
    Ok(())
}

/// 写一条动作记录：name 长度已校验，action 非 0、channel_bit 在可用通道内。
fn encode_action_record(
    writer: &mut Writer,
    path: &str,
    record: &ActionRecord,
) -> Result<(), SaveError> {
    if record.action == 0 {
        return Err(SaveError::new(
            field_path(path, "action"),
            writer.len(),
            SaveErrorKind::InvalidValue {
                what: "action id (must be non-zero)",
                value: 0,
            },
        ));
    }
    if record.channel_bit >= MAX_INPUT_CHANNELS {
        return Err(SaveError::new(
            field_path(path, "channel_bit"),
            writer.len(),
            SaveErrorKind::TooLarge {
                what: "channel_bit",
                value: u64::from(record.channel_bit),
                max: u64::from(MAX_INPUT_CHANNELS),
            },
        ));
    }
    let name_len = match u16::try_from(record.name.len()) {
        Ok(len) => len,
        Err(_) => {
            return Err(SaveError::new(
                field_path(path, "name"),
                writer.len(),
                SaveErrorKind::TooLarge {
                    what: "action name length",
                    value: record.name.len() as u64,
                    max: u64::from(MAX_ACTION_NAME_LEN),
                },
            ))
        }
    };
    writer.u16(name_len);
    writer.bytes(record.name.as_bytes());
    writer.u32(record.action);
    writer.u32(record.channel_bit);
    Ok(())
}

/// 编码插件清单段：count + 每条 (id_len + id)，**按装配顺序**写，不排序。
///
/// 顺序是数据：确定性拓扑排序的结果就是世界的一部分，读档要求逐项一致，
/// 所以这里只校验非空 / 长度 / 唯一，不校验升降序。
fn encode_plugins(writer: &mut Writer, plugins: &[String]) -> Result<(), SaveError> {
    let plugin_count = u32::try_from(plugins.len()).unwrap_or(u32::MAX);
    if plugin_count > MAX_PLUGIN_COUNT {
        return Err(SaveError::new(
            "world.meta.plugin_count",
            writer.len(),
            SaveErrorKind::TooLarge {
                what: "plugin_count",
                value: u64::from(plugin_count),
                max: u64::from(MAX_PLUGIN_COUNT),
            },
        ));
    }
    writer.u32(plugin_count);
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for (index, id) in plugins.iter().enumerate() {
        let path = indexed_path("world.meta", "plugin", index);
        let offset = writer.len();
        if id.is_empty() {
            return Err(SaveError::new(
                field_path(&path, "id"),
                offset,
                SaveErrorKind::InvalidValue {
                    what: "plugin id (must be non-empty)",
                    value: 0,
                },
            ));
        }
        if id.len() > usize::from(MAX_PLUGIN_ID_LEN) {
            return Err(SaveError::new(
                field_path(&path, "id"),
                offset,
                SaveErrorKind::TooLarge {
                    what: "plugin id length",
                    value: id.len() as u64,
                    max: u64::from(MAX_PLUGIN_ID_LEN),
                },
            ));
        }
        if !seen.insert(id.as_str()) {
            return Err(SaveError::new(
                field_path(&path, "id"),
                offset,
                SaveErrorKind::InvalidValue {
                    what: "plugin id (must be unique)",
                    value: 0,
                },
            ));
        }
        writer.u16(u16::try_from(id.len()).expect("长度已校验"));
        writer.bytes(id.as_bytes());
    }
    Ok(())
}

/// 校验字符串非空、长度受限且严格升序，并返回可写盘的长度。
fn validate_ordered_string(
    path: &str,
    field: &str,
    value: &str,
    previous: Option<&str>,
    max_len: u16,
    offset: usize,
) -> Result<(), SaveError> {
    if value.is_empty() {
        return Err(SaveError::new(
            field_path(path, field),
            offset,
            SaveErrorKind::InvalidValue {
                what: "string (must be non-empty)",
                value: 0,
            },
        ));
    }
    if value.len() > usize::from(max_len) {
        return Err(SaveError::new(
            field_path(path, field),
            offset,
            SaveErrorKind::TooLarge {
                what: "string length",
                value: value.len() as u64,
                max: u64::from(max_len),
            },
        ));
    }
    if previous.is_some_and(|previous| value <= previous) {
        return Err(SaveError::new(
            field_path(path, field),
            offset,
            SaveErrorKind::InvalidValue {
                what: "string order (must be strictly increasing)",
                value: 0,
            },
        ));
    }
    Ok(())
}

/// 编码一条参数记录：key_len + key + ty + scale + value。
fn encode_param_record(
    writer: &mut Writer,
    path: &str,
    record: &ParamRecord,
) -> Result<(), SaveError> {
    if record.ty > 3 {
        return Err(SaveError::new(
            field_path(path, "ty"),
            writer.len(),
            SaveErrorKind::Unsupported {
                what: "param type tag",
                value: u64::from(record.ty),
            },
        ));
    }
    if record.ty == 0 && record.value != 0 && record.value != 1 {
        return Err(SaveError::new(
            field_path(path, "value"),
            writer.len(),
            SaveErrorKind::InvalidValue {
                what: "bool param value",
                value: record.value,
            },
        ));
    }
    if record.ty != 2 && record.scale != 0 {
        return Err(SaveError::new(
            field_path(path, "scale"),
            writer.len(),
            SaveErrorKind::InvalidValue {
                what: "scale on non-Fixed param",
                value: i64::from(record.scale),
            },
        ));
    }
    writer.u16(u16::try_from(record.key.len()).expect("长度已校验"));
    writer.bytes(record.key.as_bytes());
    writer.u8(record.ty);
    writer.u32(record.scale);
    writer.i64(record.value);
    Ok(())
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
    if !(MIN_SUPPORTED_FORMAT_VERSION..=FORMAT_VERSION).contains(&format_version) {
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

    let body_end = header + u32_len(body_count) * BODY_RECORD_LEN;
    let min_len = body_end + DIGEST_LEN;
    if bytes.len() < min_len {
        return Err(SaveError::new(
            "world.meta.file_len",
            bytes.len(),
            SaveErrorKind::Truncated {
                need: min_len,
                have: bytes.len(),
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

    let (mods, params, actions, plugins, saved_with, digest_offset) = if format_version >= 2 {
        let payload_end = bytes.len() - DIGEST_LEN;
        let mut extension = Reader::new(&bytes[body_end..payload_end], body_end);
        let mut mods = Vec::new();
        let mut params = Vec::new();
        let mut actions = Vec::new();
        let mut plugins = Vec::new();
        let mut saved_with = SavedWith::default();
        decode_extension(
            &mut extension,
            &mut mods,
            &mut params,
            &mut actions,
            &mut plugins,
            &mut saved_with,
        )?;
        if extension.remaining() != 0 {
            return Err(SaveError::new(
                "world.meta.file_len",
                extension.offset(),
                SaveErrorKind::TrailingBytes {
                    extra: extension.remaining(),
                },
            ));
        }
        (mods, params, actions, plugins, saved_with, payload_end)
    } else {
        let expected_len = body_end + DIGEST_LEN;
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
        (
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            SavedWith::default(),
            body_end,
        )
    };

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
        mods,
        params,
        actions,
        plugins,
        saved_with,
    })
}

/// 解码 v2 追加段；调用方保证 Reader 的切片在 digest 之前正好覆盖扩展区。
fn decode_extension(
    reader: &mut Reader,
    mods: &mut Vec<ModRecord>,
    params: &mut Vec<ParamRecord>,
    actions: &mut Vec<ActionRecord>,
    plugins: &mut Vec<String>,
    saved_with: &mut SavedWith,
) -> Result<(), SaveError> {
    let count_offset = reader.offset();
    let mod_count = reader.u32("world.meta.mod_count")?;
    if mod_count > MAX_MOD_COUNT {
        return Err(SaveError::new(
            "world.meta.mod_count",
            count_offset,
            SaveErrorKind::TooLarge {
                what: "mod_count",
                value: u64::from(mod_count),
                max: u64::from(MAX_MOD_COUNT),
            },
        ));
    }
    let mut previous: Option<String> = None;
    for index in 0..u32_len(mod_count) {
        let path = indexed_path("world.meta", "mod", index);
        let len_offset = reader.offset();
        let id_len = reader.u16(&field_path(&path, "id_len"))?;
        if id_len > MAX_MOD_ID_LEN {
            return Err(SaveError::new(
                field_path(&path, "id_len"),
                len_offset,
                SaveErrorKind::TooLarge {
                    what: "mod id length",
                    value: u64::from(id_len),
                    max: u64::from(MAX_MOD_ID_LEN),
                },
            ));
        }
        let id_offset = reader.offset();
        let raw = reader.take(usize::from(id_len), &field_path(&path, "id"))?;
        let id = std::str::from_utf8(raw).map_err(|_| {
            SaveError::new(
                field_path(&path, "id"),
                id_offset,
                SaveErrorKind::InvalidString { what: "mod id" },
            )
        })?;
        if id.is_empty() {
            return Err(SaveError::new(
                field_path(&path, "id"),
                id_offset,
                SaveErrorKind::InvalidValue {
                    what: "mod id (must be non-empty)",
                    value: 0,
                },
            ));
        }
        if previous.as_deref().is_some_and(|previous| id <= previous) {
            return Err(SaveError::new(
                field_path(&path, "id"),
                id_offset,
                SaveErrorKind::InvalidValue {
                    what: "mod id order (must be strictly increasing)",
                    value: 0,
                },
            ));
        }
        let version = reader.u32(&field_path(&path, "version"))?;
        let id = id.to_string();
        previous = Some(id.clone());
        mods.push(ModRecord { id, version });
    }

    let count_offset = reader.offset();
    let param_count = reader.u32("world.meta.param_count")?;
    if param_count > MAX_PARAM_COUNT {
        return Err(SaveError::new(
            "world.meta.param_count",
            count_offset,
            SaveErrorKind::TooLarge {
                what: "param_count",
                value: u64::from(param_count),
                max: u64::from(MAX_PARAM_COUNT),
            },
        ));
    }
    let mut previous: Option<String> = None;
    for index in 0..u32_len(param_count) {
        let path = indexed_path("world.meta", "param", index);
        let len_offset = reader.offset();
        let key_len = reader.u16(&field_path(&path, "key_len"))?;
        if key_len > MAX_PARAM_KEY_LEN {
            return Err(SaveError::new(
                field_path(&path, "key_len"),
                len_offset,
                SaveErrorKind::TooLarge {
                    what: "param key length",
                    value: u64::from(key_len),
                    max: u64::from(MAX_PARAM_KEY_LEN),
                },
            ));
        }
        let key_offset = reader.offset();
        let raw = reader.take(usize::from(key_len), &field_path(&path, "key"))?;
        let key = std::str::from_utf8(raw).map_err(|_| {
            SaveError::new(
                field_path(&path, "key"),
                key_offset,
                SaveErrorKind::InvalidString { what: "param key" },
            )
        })?;
        if key.is_empty() {
            return Err(SaveError::new(
                field_path(&path, "key"),
                key_offset,
                SaveErrorKind::InvalidValue {
                    what: "param key (must be non-empty)",
                    value: 0,
                },
            ));
        }
        if previous.as_deref().is_some_and(|previous| key <= previous) {
            return Err(SaveError::new(
                field_path(&path, "key"),
                key_offset,
                SaveErrorKind::InvalidValue {
                    what: "param key order (must be strictly increasing)",
                    value: 0,
                },
            ));
        }

        let ty_offset = reader.offset();
        let ty = reader.u8(&field_path(&path, "ty"))?;
        if ty > 3 {
            return Err(SaveError::new(
                field_path(&path, "ty"),
                ty_offset,
                SaveErrorKind::Unsupported {
                    what: "param type tag",
                    value: u64::from(ty),
                },
            ));
        }
        let scale = reader.u32(&field_path(&path, "scale"))?;
        let value_offset = reader.offset();
        let value = reader.i64(&field_path(&path, "value"))?;
        if ty == 0 && value != 0 && value != 1 {
            return Err(SaveError::new(
                field_path(&path, "value"),
                value_offset,
                SaveErrorKind::InvalidValue {
                    what: "bool param value",
                    value,
                },
            ));
        }
        if ty != 2 && scale != 0 {
            return Err(SaveError::new(
                field_path(&path, "scale"),
                value_offset,
                SaveErrorKind::InvalidValue {
                    what: "scale on non-Fixed param",
                    value: i64::from(scale),
                },
            ));
        }

        let key = key.to_string();
        previous = Some(key.clone());
        params.push(ParamRecord {
            key,
            ty,
            scale,
            value,
        });
    }

    // v2 起动作表段是必填的：params 之后没字节就是坏档（旧 v2 不再兼容）。
    decode_actions(reader, actions)?;
    // 插件清单段同样必填：缺段即坏档，不做「按空清单放行」。
    decode_plugins(reader, plugins)?;
    // 构建身份段同样必填，位于插件段之后、digest 之前。
    decode_saved_with(reader, saved_with)
}

/// 解码构建身份段：u16 version_len + game_version + u32 engine_api。
fn decode_saved_with(reader: &mut Reader, saved: &mut SavedWith) -> Result<(), SaveError> {
    let len_offset = reader.offset();
    let len = reader.u16("world.meta.saved_with.version_len")?;
    if len > MAX_GAME_VERSION_LEN {
        return Err(SaveError::new(
            "world.meta.saved_with.version_len",
            len_offset,
            SaveErrorKind::TooLarge {
                what: "game_version length",
                value: u64::from(len),
                max: u64::from(MAX_GAME_VERSION_LEN),
            },
        ));
    }
    let text_offset = reader.offset();
    let raw = reader.take(usize::from(len), "world.meta.saved_with.game_version")?;
    let text = std::str::from_utf8(raw).map_err(|_| {
        SaveError::new(
            "world.meta.saved_with.game_version",
            text_offset,
            SaveErrorKind::InvalidString {
                what: "game_version",
            },
        )
    })?;
    if text.chars().any(char::is_control) {
        return Err(SaveError::new(
            "world.meta.saved_with.game_version",
            text_offset,
            SaveErrorKind::InvalidString {
                what: "game_version",
            },
        ));
    }
    saved.game_version = text.to_string();
    saved.engine_api = reader.u32("world.meta.saved_with.engine_api")?;
    Ok(())
}

/// 解码动作表段：count + 每条 (name_len + name + action + channel_bit)。
fn decode_actions(reader: &mut Reader, actions: &mut Vec<ActionRecord>) -> Result<(), SaveError> {
    let count_offset = reader.offset();
    let action_count = reader.u32("world.meta.action_count")?;
    if action_count > MAX_ACTION_COUNT {
        return Err(SaveError::new(
            "world.meta.action_count",
            count_offset,
            SaveErrorKind::TooLarge {
                what: "action_count",
                value: u64::from(action_count),
                max: u64::from(MAX_ACTION_COUNT),
            },
        ));
    }
    let mut previous: Option<String> = None;
    for index in 0..u32_len(action_count) {
        let path = indexed_path("world.meta", "action", index);
        let len_offset = reader.offset();
        let name_len = reader.u16(&field_path(&path, "name_len"))?;
        if name_len > MAX_ACTION_NAME_LEN {
            return Err(SaveError::new(
                field_path(&path, "name_len"),
                len_offset,
                SaveErrorKind::TooLarge {
                    what: "action name length",
                    value: u64::from(name_len),
                    max: u64::from(MAX_ACTION_NAME_LEN),
                },
            ));
        }
        let name_offset = reader.offset();
        let raw = reader.take(usize::from(name_len), &field_path(&path, "name"))?;
        let name = std::str::from_utf8(raw).map_err(|_| {
            SaveError::new(
                field_path(&path, "name"),
                name_offset,
                SaveErrorKind::InvalidString {
                    what: "action name",
                },
            )
        })?;
        if name.is_empty() {
            return Err(SaveError::new(
                field_path(&path, "name"),
                name_offset,
                SaveErrorKind::InvalidValue {
                    what: "action name (must be non-empty)",
                    value: 0,
                },
            ));
        }
        if previous.as_deref().is_some_and(|previous| name <= previous) {
            return Err(SaveError::new(
                field_path(&path, "name"),
                name_offset,
                SaveErrorKind::InvalidValue {
                    what: "action name order (must be strictly increasing)",
                    value: 0,
                },
            ));
        }

        let action_offset = reader.offset();
        let action = reader.u32(&field_path(&path, "action"))?;
        if action == 0 {
            return Err(SaveError::new(
                field_path(&path, "action"),
                action_offset,
                SaveErrorKind::InvalidValue {
                    what: "action id (must be non-zero)",
                    value: 0,
                },
            ));
        }
        let bit_offset = reader.offset();
        let channel_bit = reader.u32(&field_path(&path, "channel_bit"))?;
        if channel_bit >= MAX_INPUT_CHANNELS {
            return Err(SaveError::new(
                field_path(&path, "channel_bit"),
                bit_offset,
                SaveErrorKind::TooLarge {
                    what: "channel_bit",
                    value: u64::from(channel_bit),
                    max: u64::from(MAX_INPUT_CHANNELS),
                },
            ));
        }

        let name = name.to_string();
        previous = Some(name.clone());
        actions.push(ActionRecord {
            name,
            action,
            channel_bit,
        });
    }
    Ok(())
}

/// 解码插件清单段：count + 每条 (id_len + id)。
///
/// 顺序即装配顺序，因此不校验升降序，只校验非空 / 长度 / 唯一。
fn decode_plugins(reader: &mut Reader, plugins: &mut Vec<String>) -> Result<(), SaveError> {
    let count_offset = reader.offset();
    let plugin_count = reader.u32("world.meta.plugin_count")?;
    if plugin_count > MAX_PLUGIN_COUNT {
        return Err(SaveError::new(
            "world.meta.plugin_count",
            count_offset,
            SaveErrorKind::TooLarge {
                what: "plugin_count",
                value: u64::from(plugin_count),
                max: u64::from(MAX_PLUGIN_COUNT),
            },
        ));
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for index in 0..u32_len(plugin_count) {
        let path = indexed_path("world.meta", "plugin", index);
        let len_offset = reader.offset();
        let id_len = reader.u16(&field_path(&path, "id_len"))?;
        if id_len > MAX_PLUGIN_ID_LEN {
            return Err(SaveError::new(
                field_path(&path, "id_len"),
                len_offset,
                SaveErrorKind::TooLarge {
                    what: "plugin id length",
                    value: u64::from(id_len),
                    max: u64::from(MAX_PLUGIN_ID_LEN),
                },
            ));
        }
        let id_offset = reader.offset();
        let raw = reader.take(usize::from(id_len), &field_path(&path, "id"))?;
        let id = std::str::from_utf8(raw).map_err(|_| {
            SaveError::new(
                field_path(&path, "id"),
                id_offset,
                SaveErrorKind::InvalidString { what: "plugin id" },
            )
        })?;
        if id.is_empty() {
            return Err(SaveError::new(
                field_path(&path, "id"),
                id_offset,
                SaveErrorKind::InvalidValue {
                    what: "plugin id (must be non-empty)",
                    value: 0,
                },
            ));
        }
        if !seen.insert(id.to_string()) {
            return Err(SaveError::new(
                field_path(&path, "id"),
                id_offset,
                SaveErrorKind::InvalidValue {
                    what: "plugin id (must be unique)",
                    value: 0,
                },
            ));
        }
        plugins.push(id.to_string());
    }
    Ok(())
}
