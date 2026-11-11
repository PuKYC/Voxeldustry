//! region 文件（§3）的编解码：头、索引表、记录区与尾摘要。

use super::reader::{field_path, indexed_path, read_u64_at, u32_len, Reader, Writer};
use super::record::{decode_record, encode_record};
use super::{expect_eq, expect_max, expect_nonzero, expect_reserved_zero};
use crate::save::format::{
    digest, ChunkRecord, RegionFile, DIGEST_LEN, FORMAT_VERSION, MAX_CHUNK_COUNT, MAX_RECORD_LEN,
    MAX_REGION_FILE_LEN, MIN_SUPPORTED_FORMAT_VERSION, REGION_HEADER_LEN, REGION_INDEX_ENTRY_LEN,
    REGION_MAGIC,
};
use crate::save::{SaveError, SaveErrorKind};
use ::voxel::ChunkKey;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const R_FORMAT_VERSION: usize = 8;
pub(crate) const R_HEADER_LEN: usize = 10;
pub(crate) const R_CHUNK_COUNT: usize = 12;
pub(crate) const R_INDEX_OFFSET: usize = 16;
pub(crate) const R_MAX_STABLE_ENTITY_ID: usize = 24;

pub(crate) const I_RECORD_OFFSET: usize = 16;

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
    let header = check_header(bytes)?;
    let entries = parse_directory(bytes, &header)?;

    let expected_offset = entries
        .last()
        .map_or(u64::from(REGION_HEADER_LEN), |entry| entry.end as u64);
    expect_eq(
        "region",
        "index_offset",
        R_INDEX_OFFSET,
        "index_offset (32 + sum of record_len)",
        expected_offset,
        header.index_offset,
    )?;

    let mut seen_entities: BTreeSet<u64> = BTreeSet::new();
    let (chunks, computed_max) = decode_records(bytes, &entries, &mut seen_entities)?;

    expect_eq(
        "region",
        "max_stable_entity_id",
        R_MAX_STABLE_ENTITY_ID,
        "max_stable_entity_id",
        computed_max,
        header.max_stable_entity_id,
    )?;

    let stored = read_u64_at(bytes, header.body_len);
    let computed = digest(&bytes[..header.body_len]);
    if stored != computed {
        return Err(SaveError::new(
            "region.digest",
            header.body_len,
            SaveErrorKind::DigestMismatch { stored, computed },
        ));
    }

    Ok(RegionFile { chunks })
}

/// region 头（§3，32 B）中后续解码步骤需要的布局信息。
pub(crate) struct RegionHeader {
    chunk_count: u32,
    index_offset: u64,
    max_stable_entity_id: u64,
    /// 索引表在文件里的起点与长度（字节）。
    index_start: usize,
    index_len: usize,
    /// 尾摘要之前的正文字节数。
    body_len: usize,
}

/// 一条 region 索引项（§3，32 B）的解析结果。
pub(crate) struct IndexEntry {
    key: ChunkKey,
    generator_version: u32,
    start: usize,
    end: usize,
}

/// 解析并校验 region 头（§3），返回索引表位置与正文长度。
pub(crate) fn check_header(bytes: &[u8]) -> Result<RegionHeader, SaveError> {
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
    if !(MIN_SUPPORTED_FORMAT_VERSION..=FORMAT_VERSION).contains(&format_version) {
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
    expect_eq(
        "region",
        "header_len",
        R_HEADER_LEN,
        "header_len",
        u64::from(REGION_HEADER_LEN),
        u64::from(header_len),
    )?;

    let chunk_count = reader.u32("region.chunk_count")?;
    expect_max(
        "region",
        "chunk_count",
        R_CHUNK_COUNT,
        "chunk_count",
        u64::from(chunk_count),
        u64::from(MAX_CHUNK_COUNT),
    )?;

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
    expect_max(
        "region",
        "index_offset",
        R_INDEX_OFFSET,
        "index_offset",
        index_offset,
        body_len as u64,
    )?;

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
    expect_eq(
        "region",
        "file_len",
        bytes.len(),
        "file length for index_offset and chunk_count",
        expected_len,
        bytes.len() as u64,
    )?;

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

    Ok(RegionHeader {
        chunk_count,
        index_offset,
        max_stable_entity_id,
        index_start,
        index_len,
        body_len,
    })
}

/// 解析 region 索引表（§3）。
///
/// 校验 ChunkKey 严格升序且不重复、record_offset 从 32 起连续、record_len 非 0 且不超上限、
/// 记录区间完全落在索引表之前。
pub(crate) fn parse_directory(
    bytes: &[u8],
    header: &RegionHeader,
) -> Result<Vec<IndexEntry>, SaveError> {
    let mut index_reader = Reader::new(
        &bytes[header.index_start..header.index_start + header.index_len],
        header.index_start,
    );

    let mut entries: Vec<IndexEntry> = Vec::with_capacity(u32_len(header.chunk_count));
    let mut previous_key: Option<ChunkKey> = None;
    let mut expected_offset = u64::from(REGION_HEADER_LEN);

    for index in 0..u32_len(header.chunk_count) {
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
        expect_reserved_zero(&ipath, "reserved", reserved_field, u64::from(reserved))?;

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

        expect_eq(
            &ipath,
            "record_offset",
            offset_field,
            "record_offset (records must be contiguous from header end)",
            expected_offset,
            record_offset,
        )?;

        expect_nonzero(
            &ipath,
            "record_len",
            len_field,
            "record_len",
            i64::from(record_len),
        )?;
        expect_max(
            &ipath,
            "record_len",
            len_field,
            "record_len",
            u64::from(record_len),
            u64::from(MAX_RECORD_LEN),
        )?;

        let end = record_offset
            .checked_add(u64::from(record_len))
            .ok_or_else(|| {
                SaveError::new(
                    field_path(&ipath, "record_offset"),
                    offset_field,
                    SaveErrorKind::TooLarge {
                        what: "record end",
                        value: u64::MAX,
                        max: header.index_offset,
                    },
                )
            })?;
        if end > header.index_offset {
            return Err(SaveError::new(
                field_path(&ipath, "record_offset"),
                offset_field,
                SaveErrorKind::TooLarge {
                    what: "record end",
                    value: end,
                    max: header.index_offset,
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

        entries.push(IndexEntry {
            key,
            generator_version,
            start,
            end,
        });
        expected_offset = record_offset + u64::from(record_len);
    }

    Ok(entries)
}

/// 逐条解码 region 记录体（§3.1），并统计 region 内实体 ID 上界。
pub(crate) fn decode_records(
    bytes: &[u8],
    entries: &[IndexEntry],
    seen_entities: &mut BTreeSet<u64>,
) -> Result<(BTreeMap<ChunkKey, ChunkRecord>, u64), SaveError> {
    let mut chunks: BTreeMap<ChunkKey, ChunkRecord> = BTreeMap::new();
    let mut computed_max: u64 = 0;

    for (index, entry) in entries.iter().enumerate() {
        let record = decode_record(
            &bytes[entry.start..entry.end],
            &field_path(&indexed_path("region", "chunk", index), "record"),
            entry.start,
            entry.generator_version,
            seen_entities,
        )?;

        for entity in &record.entities {
            computed_max = computed_max.max(entity.stable_entity_id);
        }

        chunks.insert(entry.key, record);
    }

    Ok((chunks, computed_max))
}
