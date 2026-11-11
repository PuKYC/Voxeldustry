//! codec 的单元测试。

use super::header::*;
use super::reader::*;
use super::record::*;
use super::region::*;
use crate::body_key::BodyKey;
use crate::identity::StableEntityId;
use crate::modding::ParamRecord;
use crate::save::format::{
    digest, ActionRecord, AttachTag, BodyKind, BodyRecord, ChunkRecord, ComponentRecord,
    EntityRecord, ModRecord, RegionFile, WorldMeta, BODY_RECORD_LEN,
};
use crate::streaming::ProposalRecord;
use ::voxel::ChunkKey;
use std::collections::BTreeMap;

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
        mods: Vec::new(),
        params: Vec::new(),
        actions: Vec::new(),
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

fn hex_bytes(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&text[index..index + 2], 16).expect("黄金样例必须是合法十六进制")
        })
        .collect()
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

/// v1 黄金字节（format_version = 1）：只用于验证旧存档仍可读。
///
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

/// v2 黄金字节：v1 的 body 表后追加空的「mod 清单 + 世界参数 + 动作表」段（12 B 全 0）。
/// 总长 176 B = 48 + 2 × 54 + 12 + 8。
const GOLDEN_WORLD_META_V2_HEX: &str = concat!(
    "5658574f524c44000200300001000000efcdab89674523013930000000000000",
    "4000000000000000020000000000000001000000000000000201000000000000",
    "8001000000000000000000000000000000000000000000000000000000000300",
    "0000000000000300000000000000000207010000000000010000000000000000",
    "0000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000024009508ec51f722",
);

/// v1 黄金字节：同一份规格 §3 的布局（format_version = 1）。
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

/// v2 黄金字节：region 布局不变，仅 format_version 与尾摘要更新。
/// 总长 311 B。
const GOLDEN_REGION_V2_HEX: &str = concat!(
    "5658524547494f4e0200200003000000cf000000000000006400000000000000",
    "010001000a000000564f58454c494f00010000000000010000002a0000000000",
    "0000e90300000f00000001000000070000000300000001020301000000010000",
    "000200000004000000fdffffff1032547698badcfef9ffffff08000000090000",
    "00f6ffffff080000000600000004000000140000000c00000000000000010000",
    "0000000000010000006400000000000000050000001600000002000000030000",
    "00000000000900000002000000dead0000000000000000000000000700000020",
    "0000000000000039000000000000000200000003000000010000000100000059",
    "000000000000004400000000000000030000000000000002000000020000009d",
    "00000000000000320000000000000001014b5446b0af2f",
);

#[test]
fn world_meta_roundtrip() {
    let meta = golden_world_meta();
    let bytes = encode_world_meta(&meta).expect("world.meta 必须可编码");
    assert_eq!(bytes.len(), 48 + 2 * 54 + 12 + 8);
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
        mods: Vec::new(),
        params: Vec::new(),
        actions: Vec::new(),
    };
    let bytes = encode_world_meta(&meta).expect("空 body 表合法");
    assert_eq!(bytes.len(), 48 + 12 + 8);
    assert_eq!(u32_at(&bytes, W_BODY_COUNT), 0);
    assert_eq!(decode_world_meta(&bytes).expect("空 body 表合法"), meta);
    assert_eq!(&bytes[48..60], &[0u8; 12], "空的 mods/params/actions 段");
    assert_eq!(&bytes[60..68], &digest(&bytes[..60]).to_le_bytes());
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
fn golden_world_meta_bytes_match_spec_v2() {
    let bytes = encode_world_meta(&golden_world_meta()).expect("world.meta 必须可编码");
    assert_eq!(bytes.len(), 176);
    assert_eq!(bytes.len() * 2, GOLDEN_WORLD_META_V2_HEX.len());
    assert_eq!(hex(&bytes), GOLDEN_WORLD_META_V2_HEX);
    assert_eq!(
        decode_world_meta(&bytes).expect("黄金字节必须可解码"),
        golden_world_meta()
    );
}

#[test]
fn golden_world_meta_v1_still_decodes() {
    let bytes = hex_bytes(GOLDEN_WORLD_META_HEX);
    let decoded = decode_world_meta(&bytes).expect("v1 黄金字节必须可解码");
    assert_eq!(decoded, golden_world_meta());
    assert!(decoded.mods.is_empty());
    assert!(decoded.params.is_empty());
    assert_eq!(
        encode_world_meta(&decoded).unwrap(),
        hex_bytes(GOLDEN_WORLD_META_V2_HEX)
    );
}

#[test]
fn world_meta_v2_extension_roundtrip() {
    let mut meta = golden_world_meta();
    meta.mods = vec![
        ModRecord {
            id: "core.gameplay".to_string(),
            version: 3,
        },
        ModRecord {
            id: "island".to_string(),
            version: 1,
        },
    ];
    meta.params = vec![
        ParamRecord {
            key: "island.flag".to_string(),
            ty: 0,
            scale: 0,
            value: 1,
        },
        ParamRecord {
            key: "island.ratio".to_string(),
            ty: 2,
            scale: 1000,
            value: 2500,
        },
    ];
    meta.actions = vec![
        ActionRecord {
            name: "attack".to_string(),
            action: 9,
            channel_bit: 8,
        },
        ActionRecord {
            name: "mod.build".to_string(),
            action: 1000,
            channel_bit: 15,
        },
    ];

    let bytes = encode_world_meta(&meta).expect("v2 扩展必须可编码");
    let decoded = decode_world_meta(&bytes).expect("v2 扩展必须可解码");
    assert_eq!(decoded, meta);
    assert_eq!(encode_world_meta(&decoded).unwrap(), bytes);
}

#[test]
fn world_meta_v2_without_actions_segment_is_rejected() {
    let mut meta = golden_world_meta();
    meta.mods = vec![ModRecord {
        id: "core.data".to_string(),
        version: 2,
    }];
    meta.params = vec![ParamRecord {
        key: "island.flag".to_string(),
        ty: 0,
        scale: 0,
        value: 1,
    }];

    let bytes = encode_world_meta(&meta).expect("v2 扩展必须可编码");
    // 空动作表段只有 4 B 的 action_count，位于摘要之前；裁掉它再重算摘要，
    // 就得到一份「有 mods/params、没有 actions 段」的字节。v2 起该段必填，
    // 未发布前不做旧档兼容，必须拒绝。
    let cut = bytes.len() - DIGEST_LEN - 4;
    assert_eq!(&bytes[cut..cut + 4], &[0u8; 4]);
    let mut legacy = bytes[..cut].to_vec();
    let checksum = digest(&legacy);
    legacy.extend_from_slice(&checksum.to_le_bytes());

    let error = decode_world_meta(&legacy).expect_err("缺动作表段的 v2 必须拒绝");
    assert!(
        error.to_string().contains("action_count"),
        "错误应指向动作表段，实际：{error}"
    );
    assert!(
        matches!(error.kind(), crate::save::SaveErrorKind::Truncated { .. }),
        "缺段应报截断，实际：{:?}",
        error.kind()
    );
}

#[test]
fn world_meta_v2_extension_rejects_bad_records() {
    let mut meta = golden_world_meta();
    meta.mods = vec![
        ModRecord {
            id: "b".to_string(),
            version: 1,
        },
        ModRecord {
            id: "a".to_string(),
            version: 1,
        },
    ];
    let err = encode_world_meta(&meta).expect_err("mod 顺序错必须拒绝");
    assert!(err.to_string().contains("string order"), "{err}");

    meta.mods.clear();
    meta.params = vec![ParamRecord {
        key: "island.flag".to_string(),
        ty: 0,
        scale: 0,
        value: 2,
    }];
    let err = encode_world_meta(&meta).expect_err("布尔值只能是 0/1");
    assert!(err.to_string().contains("bool param value"), "{err}");

    meta.params[0].value = 1;
    meta.params[0].ty = 4;
    let err = encode_world_meta(&meta).expect_err("未知类型必须拒绝");
    assert!(err.to_string().contains("param type tag"), "{err}");

    meta.params[0].ty = 1;
    meta.params[0].scale = 10;
    let err = encode_world_meta(&meta).expect_err("非定点参数不得有 scale");
    assert!(err.to_string().contains("scale on non-Fixed"), "{err}");

    meta.params.clear();
    meta.actions = vec![
        ActionRecord {
            name: "b".to_string(),
            action: 1000,
            channel_bit: 15,
        },
        ActionRecord {
            name: "a".to_string(),
            action: 1001,
            channel_bit: 16,
        },
    ];
    let err = encode_world_meta(&meta).expect_err("动作名顺序错必须拒绝");
    assert!(err.to_string().contains("string order"), "{err}");

    meta.actions = vec![ActionRecord {
        name: "a".to_string(),
        action: 0,
        channel_bit: 15,
    }];
    let err = encode_world_meta(&meta).expect_err("动作 ID 0 必须拒绝");
    assert!(err.to_string().contains("action id"), "{err}");

    meta.actions[0].action = 1000;
    meta.actions[0].channel_bit = 64;
    let err = encode_world_meta(&meta).expect_err("通道位超界必须拒绝");
    assert!(err.to_string().contains("channel_bit"), "{err}");
}

#[test]
fn golden_region_bytes_match_spec_v2() {
    let bytes = encode_region(&golden_region()).expect("region 必须可编码");
    assert_eq!(bytes.len(), 311);
    assert_eq!(bytes.len() * 2, GOLDEN_REGION_V2_HEX.len());
    assert_eq!(hex(&bytes), GOLDEN_REGION_V2_HEX);
    assert_eq!(
        decode_region(&bytes).expect("黄金字节必须可解码"),
        golden_region()
    );
}

#[test]
fn golden_region_v1_still_decodes() {
    let bytes = hex_bytes(GOLDEN_REGION_HEX);
    assert_eq!(
        decode_region(&bytes).expect("v1 黄金字节必须可解码"),
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
    assert_eq!(bytes.len(), 176);

    let cases: [(usize, u8, &str); 10] = [
        (0, 0x01, "magic"),
        (W_FORMAT_VERSION, 0x01, "format_version"),
        (W_FORMAT_VERSION, 0x02, "format_version"),
        (W_HEADER_LEN, 0x01, "header_len"),
        (W_CODEC_ID, 0x01, "codec_id"),
        (W_CODEC_ID, 0x02, "codec_id"),
        (W_FLAGS, 0x01, "flags"),
        (W_BODY_COUNT, 0x01, "truncated"),
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
    assert_world_meta_rejects(&truncated, "truncated");

    let mut trailing = bytes;
    trailing.push(0);
    assert_world_meta_rejects(&trailing, "trailing byte");
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
        mods: Vec::new(),
        params: Vec::new(),
        actions: Vec::new(),
    };
    let bytes = encode_world_meta(&meta).expect("65536 条 body 合法");
    assert_eq!(bytes.len(), 48 + 65_536 * 54 + 12 + 8);
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
        mods: Vec::new(),
        params: Vec::new(),
        actions: Vec::new(),
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
            mods: Vec::new(),
            params: Vec::new(),
            actions: Vec::new(),
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
        mods: Vec::new(),
        params: Vec::new(),
        actions: Vec::new(),
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
        mods: Vec::new(),
        params: Vec::new(),
        actions: Vec::new(),
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
        mods: Vec::new(),
        params: Vec::new(),
        actions: Vec::new(),
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
        mods: Vec::new(),
        params: Vec::new(),
        actions: Vec::new(),
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
        encode_region(&decode_region(&region_bytes).expect("必须可解码")).expect("解码后再编码"),
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
        mods: Vec::new(),
        params: Vec::new(),
        actions: Vec::new(),
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
        mods: Vec::new(),
        params: Vec::new(),
        actions: Vec::new(),
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
