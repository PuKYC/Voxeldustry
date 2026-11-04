//! region 记录体（§3.1–§3.3）与组件外层框（§4.1）的编解码。

use super::reader::{field_path, indexed_path, read_u64_at, u32_len, Reader, Writer};
use super::{
    expect_flags_mask, expect_max, expect_nonzero, expect_reserved_zero,
    expect_strictly_increasing, expect_supported,
};
use crate::save::format::{
    ChunkRecord, ComponentRecord, EntityRecord, MAX_COMPONENT_PAYLOAD_LEN, MAX_RECORD_LEN,
    PROPOSAL_RECORD_LEN,
};
use crate::save::{SaveError, SaveErrorKind};
use crate::streaming::ProposalRecord;
use std::collections::{BTreeMap, BTreeSet};

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
        let path = &indexed_path("component_table", "component", index);
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

pub(crate) fn decode_component_table_at(
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

pub(crate) fn canonical_proposals(
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

pub(crate) fn canonical_entities<'a>(
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

pub(crate) fn encode_record(
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

pub(crate) fn decode_record(
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

    let content = decode_content(&mut reader, path, flags)?;

    let proposals = decode_proposals(&mut reader, path, base)?;

    let entities = decode_entities(&mut reader, path, seen_entities)?;

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

/// 解码可选的 content 段（§4）：flags bit0 置位时存在，先校验长度再取字节。
pub(crate) fn decode_content(
    reader: &mut Reader<'_>,
    path: &str,
    flags: u16,
) -> Result<Option<Vec<u8>>, SaveError> {
    if flags & 1 == 0 {
        return Ok(None);
    }

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
    Ok(Some(
        reader
            .take_at(path, "content", u32_len(content_len))?
            .to_vec(),
    ))
}

/// 解码 proposal 表（§4），校验边界与 (feature_type, seq) 严格升序。
pub(crate) fn decode_proposals(
    reader: &mut Reader<'_>,
    path: &str,
    base: usize,
) -> Result<Vec<ProposalRecord>, SaveError> {
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

    Ok(proposals)
}

/// 解码实体表（§4），校验 id 升序、类型非 0、payload 合法且 region 内不重复。
pub(crate) fn decode_entities(
    reader: &mut Reader<'_>,
    path: &str,
    seen_entities: &mut BTreeSet<u64>,
) -> Result<Vec<EntityRecord>, SaveError> {
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
        expect_nonzero(
            &epath,
            "stable_type_id",
            type_offset,
            "stable_type_id",
            i64::from(stable_type_id),
        )?;

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

    Ok(entities)
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
