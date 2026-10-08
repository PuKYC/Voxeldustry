//! GPF1 打包 roundtrip / 吞吐 / 跨语言一致性测试（载荷集合由 game-core 实例化）。

use std::time::Instant;

use game_engine::presentation::packed::{
    decode_frame, decode_header, decode_streams, encode_frame, frame_to_streams, is_packed_frame,
    kind, PackedError, HEADER_LEN, PACKED_VERSION, TRANSFORM_FLOATS,
};

use super::payload::{
    ExtField, ExtValue, ExtensionPayload, InteractionHint, PayloadKind, PresentationState,
    PresentedHealth, PresentedPrototype, PresentedVisibility, RawVoxelPayload, SyncPayload,
};
use super::RenderTransformSample;
use super::{PresentationCommand, PresentationFrame, PresentationSlot};
use crate::input::actions::ActionId;
use game_engine::identity::StableEntityId;
use game_engine::presentation::interp::RenderTransform;

fn sample_frame() -> PresentationFrame {
    let a = StableEntityId(1);
    let b = StableEntityId(2);
    let c = StableEntityId(3);
    let commands = vec![
        PresentationCommand::Attach { id: a },
        PresentationCommand::Add {
            id: a,
            payload: SyncPayload::Transform(RenderTransformSample {
                prev: RenderTransform {
                    position: [1.0, 2.0, 3.0],
                    yaw: 0.5,
                    correction: [0.1, 0.2, 0.3],
                },
                curr: RenderTransform {
                    position: [4.0, 5.0, 6.0],
                    yaw: 0.75,
                    correction: [0.4, 0.5, 0.6],
                },
            }),
        },
        PresentationCommand::Update {
            id: a,
            payload: SyncPayload::Presentation(PresentationState {
                locomotion_state: 2,
                action_state: 7,
                overlay_tags: vec![10, 20, 30],
            }),
        },
        PresentationCommand::Add {
            id: b,
            payload: SyncPayload::Health(PresentedHealth {
                current: 88.5,
                max: 100.0,
            }),
        },
        PresentationCommand::Update {
            id: b,
            payload: SyncPayload::Visibility(PresentedVisibility { visible: true }),
        },
        PresentationCommand::Add {
            id: c,
            payload: SyncPayload::Interaction(InteractionHint {
                action: crate::input::actions::ActionId(8),
                enabled: true,
            }),
        },
        // 扩展载荷：两条相邻命令，覆盖非零 i32 offset / count / Tags 槽布局。
        PresentationCommand::Add {
            id: c,
            payload: SyncPayload::Extension(
                ExtensionPayload::from_fields(vec![
                    ExtField {
                        key: 1,
                        value: ExtValue::I32(-3),
                    },
                    ExtField {
                        key: 2,
                        value: ExtValue::Tags(vec![5, 6]),
                    },
                ])
                .unwrap(),
            ),
        },
        PresentationCommand::Update {
            id: c,
            payload: SyncPayload::Extension(
                ExtensionPayload::from_fields(vec![ExtField {
                    key: 9,
                    value: ExtValue::Bool(true),
                }])
                .unwrap(),
            ),
        },
        PresentationCommand::Remove {
            id: c,
            kind: PayloadKind::Health,
        },
        PresentationCommand::Detach { id: c },
        PresentationCommand::Despawn { id: b },
        // 原型现在是普通 PROTOTYPE 载荷：i32 池占 1 槽，末命令用于校验池布局。
        PresentationCommand::Add {
            id: a,
            payload: SyncPayload::Prototype(PresentedPrototype(2)),
        },
    ];
    PresentationFrame {
        session: 42,
        seq: 7,
        tick: 1234,
        render_clock_ms: 98765,
        timestep_ms: 16.666,
        commands: commands.into(),
    }
}

#[test]
fn encode_decode_roundtrip_is_lossless() {
    let frame = sample_frame();
    let bytes = encode_frame(&frame);
    assert!(is_packed_frame(&bytes));

    let (header, commands) = decode_frame::<SyncPayload>(&bytes).expect("必须能解回");
    assert_eq!(header.command_count as usize, frame.commands.len());
    assert_eq!(&commands[..], &frame.commands[..], "命令必须逐条一致");

    assert_eq!(header.session, frame.session);
    assert_eq!(header.seq, frame.seq);
    assert_eq!(header.tick, frame.tick);
    assert_eq!(header.render_clock_ms, frame.render_clock_ms);
    assert_eq!(header.timestep_ms, frame.timestep_ms);

    let (_h, streams) = decode_streams::<SyncPayload>(&bytes).unwrap();
    assert_eq!(streams.command_count(), frame.commands.len());
    // 末命令是 Prototype Add：i32 池 1 槽、f32 池 0 槽。
    let last = streams.command_count() - 1;
    assert_eq!(streams.i32_counts[last], 1);
    assert_eq!(streams.f32_counts[last], 0);
}

/// 发布侧快通道必须与「编码成 GPF1 再解码」逐字段一致。
#[test]
fn frame_to_streams_matches_byte_roundtrip() {
    let frame = sample_frame();
    let direct = frame_to_streams::<SyncPayload>(&frame);
    let bytes = encode_frame::<SyncPayload>(&frame);
    let (_header, roundtrip) = decode_streams::<SyncPayload>(&bytes).unwrap();
    assert_eq!(
        direct, roundtrip,
        "frame_to_streams 必须与 encode→decode 得到完全相同的 SoA 流"
    );
}

/// 单槽快通道：take_streams 必须与 frame_to_streams 一致，帧头字段与帧一致。
#[test]
fn slot_take_streams_matches_frame_to_streams() {
    let slot: PresentationSlot = PresentationSlot::new();
    let frame = std::sync::Arc::new(sample_frame());
    slot.publish(frame.clone());

    let (header, streams) = slot.take_streams().expect("已发布，必须能取到");
    assert_eq!(streams, frame_to_streams::<SyncPayload>(frame.as_ref()));
    assert_eq!(header.session, frame.session);
    assert_eq!(header.seq, frame.seq);
    assert_eq!(header.tick, frame.tick);
    assert_eq!(header.render_clock_ms, frame.render_clock_ms);
    assert_eq!(header.timestep_ms, frame.timestep_ms);
    assert_eq!(header.command_count as usize, frame.commands.len());
}

#[test]
fn streams_are_indexable_by_command() {
    let (_, streams) = decode_streams::<SyncPayload>(&encode_frame(&sample_frame())).unwrap();
    let n = streams.command_count();
    assert_eq!(streams.kinds.len(), n);
    assert_eq!(streams.entity_ids.len(), n);
    assert_eq!(streams.payload_kinds.len(), n);
    assert_eq!(streams.f32_offsets.len(), n);
    assert_eq!(streams.i32_offsets.len(), n);

    assert_eq!(streams.kinds[0], kind::ATTACH);
    assert_eq!(streams.f32_counts[0], 0);
    assert_eq!(streams.i32_counts[0], 0);
    assert_eq!(streams.kinds[1], kind::ADD);
    assert_eq!(streams.f32_counts[1] as usize, TRANSFORM_FLOATS);
    assert_eq!(streams.i32_counts[2], 6);
    assert_eq!(streams.f32_counts[3], 2);
}

#[test]
fn header_only_parse_ignores_body() {
    let bytes = encode_frame(&sample_frame());
    let header = decode_header(&bytes).unwrap();
    assert_eq!(header.command_count as usize, sample_frame().commands.len());
    let header_only = decode_header(&bytes[..HEADER_LEN]).unwrap();
    assert_eq!(header_only.tick, 1234);
}

#[test]
fn malformed_input_returns_error_not_panic() {
    assert_eq!(decode_header(b""), Err(PackedError::Truncated));
    assert_eq!(decode_header(b"NOPE1234"), Err(PackedError::BadMagic));

    let bytes = encode_frame(&sample_frame());
    for cut in 0..bytes.len() {
        let _ = decode_frame::<SyncPayload>(&bytes[..cut]);
        let _ = decode_streams::<SyncPayload>(&bytes[..cut]);
    }

    let mut bad = bytes.clone();
    bad[4] = 99;
    assert!(matches!(
        decode_header(&bad),
        Err(PackedError::UnsupportedVersion(99))
    ));
}

/// 手工拼接的扩展畸形体：字段数 / 未知 tag / Tags 超限都必须返回明确错误而非 panic。
#[test]
fn extension_malformed_bodies_return_error_not_panic() {
    // header + 1 条 ADD(ext) 命令 + 自定义 body（小端）。
    fn raw_ext(body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"GPF1");
        out.extend_from_slice(&PACKED_VERSION.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
        out.extend_from_slice(&0f32.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.push(1); // ADD
        out.extend_from_slice(&7u64.to_le_bytes());
        out.push(5); // payload kind = extension
        out.extend_from_slice(body);
        out
    }

    // count = 1025 -> TooManyExtensionFields
    let over_fields = raw_ext(&1025u32.to_le_bytes());
    assert_eq!(
        decode_streams::<SyncPayload>(&over_fields),
        Err(PackedError::TooManyExtensionFields)
    );

    // 未知 tag -> BadExtensionTag（无长度信息，不能安全跳过）
    let mut body = Vec::new();
    body.extend_from_slice(&1u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.push(9);
    assert_eq!(
        decode_streams::<SyncPayload>(&raw_ext(&body)),
        Err(PackedError::BadExtensionTag(9))
    );

    // tag=2, n=4097 -> TooManyTags
    let mut body = Vec::new();
    body.extend_from_slice(&1u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.push(2);
    body.extend_from_slice(&4097u32.to_le_bytes());
    assert_eq!(
        decode_streams::<SyncPayload>(&raw_ext(&body)),
        Err(PackedError::TooManyTags)
    );
}

/// from_pools 对越界 offset / 脏池 / 截断 Tags 一律退化为空载荷，绝不 panic。
#[test]
fn extension_from_pools_degrades_on_dirty_pool() {
    use game_engine::presentation::packed::PackedPayload;

    assert_eq!(
        ExtensionPayload::from_pools(&[], &[], 0, usize::MAX),
        ExtensionPayload::default(),
        "越界 offset 必须退化"
    );
    // count=2 但槽位不足
    assert_eq!(
        ExtensionPayload::from_pools(&[], &[2i64, 1, 2, 3], 0, 0),
        ExtensionPayload::default()
    );
    // count=1, key=1, tag=2, n=5，但只有 2 个 tag 槽
    assert_eq!(
        ExtensionPayload::from_pools(&[], &[1i64, 1, 2, 5, 7, 8], 0, 0),
        ExtensionPayload::default()
    );
}

#[test]
fn empty_frame_roundtrips() {
    let frame = PresentationFrame {
        session: 1,
        seq: 0,
        tick: 0,
        render_clock_ms: 0,
        timestep_ms: 16.0,
        commands: Vec::new().into(),
    };
    let bytes = encode_frame(&frame);
    assert_eq!(bytes.len(), HEADER_LEN);
    let (header, commands) = decode_frame::<SyncPayload>(&bytes).unwrap();
    assert_eq!(header.command_count, 0);
    assert!(commands.is_empty());
}

// ───────────────── P3：跨语言常量一致性 ─────────────────

/// 从 GDScript 源码里抽出 `enum NAME { ... }` 的块体（不含大括号）。
fn extract_enum_block(source: &str, name: &str) -> Option<String> {
    let needle = format!("enum {name} {{");
    let start = source.find(&needle)?;
    let body_start = start + needle.len();
    let body_end = source[body_start..].find('}')? + body_start;
    Some(source[body_start..body_end].to_string())
}

/// GDScript 的 `enum Payload` 必须与 Rust 注册表逐项一致。
///
/// 用 `CARGO_MANIFEST_DIR` 定位（`cargo test -p game-core` 的 CWD 是包根，
/// 不能依赖进程 CWD）；纯文本解析，不需要安装 Godot。
#[test]
fn godot_payload_enum_matches_rust_registry() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../godot-project/bevy_client/contract/bevy_enums.gd");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("读不到 {}: {error}", path.display()));

    let block = extract_enum_block(&source, "Payload")
        .unwrap_or_else(|| panic!("{} 里找不到 enum Payload {{ ... }}", path.display()));

    // 解析 `NAME = code,`，忽略行内注释与空行。
    let mut entries: Vec<(String, u8)> = Vec::new();
    for raw in block.lines() {
        let line = raw.split_once("##").map_or(raw, |(code, _)| code);
        let line = line.trim().trim_end_matches(',');
        if line.is_empty() {
            continue;
        }
        let (name, code) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("无法解析枚举行: {raw:?}"));
        entries.push((
            name.trim().to_ascii_uppercase(),
            code.trim()
                .parse()
                .unwrap_or_else(|_| panic!("枚举 code 不是数字: {raw:?}")),
        ));
    }

    assert_eq!(
        entries.len(),
        PayloadKind::ALL.len(),
        "GDScript enum Payload 与 Rust PayloadKind::ALL 数量不一致"
    );
    for &kind in PayloadKind::ALL {
        let expected = kind.as_str().to_ascii_uppercase();
        let actual = entries
            .iter()
            .find(|(name, _)| *name == expected)
            .unwrap_or_else(|| panic!("GDScript enum Payload 缺少条目 {expected}"));
        assert_eq!(
            actual.1,
            kind.code(),
            "载荷 {expected} 的 code 不一致：GDScript={} Rust={}",
            actual.1,
            kind.code()
        );
    }
}

// ───────────────── P3.5：GDScript PAYLOAD_SCHEMA 与 Rust 池布局一致性 ─────────────────

/// 抽取 `const NAME ... = { ... }` 最外层大括号内的内容（括号配对，跳过字符串）。
fn extract_const_dict(source: &str, name: &str) -> Option<String> {
    let marker = format!("const {name}");
    let start = source.find(&marker)?;
    let (_, _, inner) = first_braced(&source[start..])?;
    Some(inner)
}

/// 从 `s` 里第一个 `{` 起找到配对的 `}`，返回 (open, close, 内容)。
fn first_braced(s: &str) -> Option<(usize, usize, String)> {
    let bytes = s.as_bytes();
    let open = s.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut i = open;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
        } else if b == b'"' {
            in_string = true;
        } else if b == b'{' {
            depth += 1;
        } else if b == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some((open, i, s[open + 1..i].to_string()));
            }
        }
        i += 1;
    }
    None
}

/// 把 schema 主体切成 (大写载荷名, 条目体) 列表。
fn split_schema_entries(body: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = body;
    const PREFIX: &str = "Payload.";
    while let Some(pos) = rest.find(PREFIX) {
        let after = &rest[pos + PREFIX.len()..];
        let colon = match after.find(':') {
            Some(c) => c,
            None => break,
        };
        let name = after[..colon].trim().to_ascii_uppercase();
        let (_, close, inner) = match first_braced(after) {
            Some(found) => found,
            None => break,
        };
        out.push((name, inner));
        rest = &after[close + 1..];
    }
    out
}

fn quoted_list(entry: &str, key: &str) -> Vec<String> {
    let Some(pos) = entry.find(&format!("\"{key}\"")) else {
        return Vec::new();
    };
    let after = &entry[pos + key.len() + 2..];
    let Some(open) = after.find('[') else {
        return Vec::new();
    };
    let Some(close) = after[open..].find(']') else {
        return Vec::new();
    };
    after[open + 1..open + close]
        .split(',')
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            match part.find('"') {
                Some(start) => {
                    let end = part[start + 1..].find('"')? + start + 1;
                    Some(part[start + 1..end].to_string())
                }
                // GDScript 常量名（EXT_FIELD / PROTOTYPE_FIELD）原样保留为一个字段。
                None => Some(part.to_string()),
            }
        })
        .collect()
}

fn token_list(entry: &str, key: &str) -> Vec<String> {
    let Some(pos) = entry.find(&format!("\"{key}\"")) else {
        return Vec::new();
    };
    let after = &entry[pos + key.len() + 2..];
    let Some(open) = after.find('[') else {
        return Vec::new();
    };
    let Some(close) = after[open..].find(']') else {
        return Vec::new();
    };
    after[open + 1..open + close]
        .split(',')
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            Some(part.strip_prefix("Wire.").unwrap_or(part).to_string())
        })
        .collect()
}

fn int_value(entry: &str, key: &str) -> Option<i64> {
    let pos = entry.find(&format!("\"{key}\""))?;
    let after = &entry[pos + key.len() + 2..];
    let colon = after.find(':')? + 1;
    let digits: String = after[colon..]
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

fn bool_value(entry: &str, key: &str) -> bool {
    match entry.find(&format!("\"{key}\"")) {
        Some(pos) => entry[pos..].contains("true"),
        None => false,
    }
}

/// wire token 的定长消耗：(f32 个数, i32 个数, 含变长 i32 列表（TAGS/WORDS）, 含 EXT_BAG)。
fn wire_shape(tokens: &[String]) -> (usize, usize, bool, bool) {
    let mut f32_count = 0usize;
    let mut i32_count = 0usize;
    let mut has_varlen = false;
    let mut has_ext = false;
    for token in tokens {
        match token.as_str() {
            "F32" | "ANGLE" => f32_count += 1,
            "VEC3" => f32_count += 3,
            "I32" | "BOOL" => i32_count += 1,
            // TAGS / BYTES 都是「count + n 个 i32 池槽」的变长列表。
            "TAGS" | "BYTES" => has_varlen = true,
            "EXT_BAG" => has_ext = true,
            _ => {}
        }
    }
    (f32_count, i32_count, has_varlen, has_ext)
}

fn sample_payload(kind: PayloadKind) -> SyncPayload {
    match kind {
        PayloadKind::Transform => SyncPayload::Transform(RenderTransformSample::default()),
        PayloadKind::Presentation => SyncPayload::Presentation(PresentationState {
            locomotion_state: 2,
            action_state: 7,
            overlay_tags: vec![10, 20, 30],
        }),
        PayloadKind::Health => SyncPayload::Health(PresentedHealth {
            current: 1.0,
            max: 2.0,
        }),
        PayloadKind::Visibility => SyncPayload::Visibility(PresentedVisibility { visible: true }),
        PayloadKind::Interaction => SyncPayload::Interaction(InteractionHint {
            action: ActionId(3),
            enabled: true,
        }),
        PayloadKind::Extension => SyncPayload::Extension(
            ExtensionPayload::from_fields(vec![
                ExtField {
                    key: 0x0101,
                    value: ExtValue::I32(5),
                },
                ExtField {
                    key: 0x0102,
                    value: ExtValue::Tags(vec![1, 2]),
                },
            ])
            .expect("测试样本身份合法"),
        ),
        PayloadKind::Prototype => SyncPayload::Prototype(PresentedPrototype(9)),
        PayloadKind::RawVoxels => SyncPayload::RawVoxels(RawVoxelPayload::from_halo(
            1,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
        )),
    }
}

/// GDScript `PAYLOAD_SCHEMA` 必须能描述 Rust 各载荷写进 SoA 池的形状。
///
/// 覆盖：条目名集合、wire token 的定长 f32/i32 数、插值 stride/samples、
/// TAGS / WORDS / EXT_BAG 的存在性。Rust 侧改布局而 GDScript 忘记同步时此测试失败。
#[test]
fn godot_payload_schema_matches_rust_pool_layout() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../godot-project/bevy_client/contract/bevy_enums.gd");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("读不到 {}: {error}", path.display()));

    let body = extract_const_dict(&source, "PAYLOAD_SCHEMA").unwrap_or_else(|| {
        panic!(
            "{} 里找不到 const PAYLOAD_SCHEMA := {{ ... }}",
            path.display()
        )
    });
    let entries = split_schema_entries(&body);
    assert!(!entries.is_empty(), "PAYLOAD_SCHEMA 解析为空");
    assert_eq!(
        entries.len(),
        PayloadKind::ALL.len(),
        "PAYLOAD_SCHEMA 条目数与 Rust PayloadKind::ALL 不一致"
    );

    for &kind in PayloadKind::ALL {
        let name = kind.as_str().to_ascii_uppercase();
        let (_, entry) = entries
            .iter()
            .find(|(entry_name, _)| *entry_name == name)
            .unwrap_or_else(|| panic!("PAYLOAD_SCHEMA 缺少 {name}"));

        let fields = quoted_list(entry, "fields");
        let wire = token_list(entry, "wire");
        assert!(!fields.is_empty(), "{name}: fields 不能为空");
        assert!(
            fields.len() <= wire.len(),
            "{name}: fields 数 ({}) 不能超过 wire token 数 ({})",
            fields.len(),
            wire.len()
        );
        assert!(
            fields.len() == wire.len() || bool_value(entry, "interp"),
            "{name}: 非插值载荷的 fields 数必须等于 wire token 数"
        );

        let (f32_fixed, i32_fixed, has_varlen, has_ext) = wire_shape(&wire);
        let mut f32_pool: Vec<f32> = Vec::new();
        let mut i32_pool: Vec<i64> = Vec::new();
        sample_payload(kind).write_pools(&mut f32_pool, &mut i32_pool);

        if bool_value(entry, "interp") {
            let stride = int_value(entry, "stride").expect("插值载荷必须声明 stride") as usize;
            let samples = int_value(entry, "samples").expect("插值载荷必须声明 samples") as usize;
            assert!(samples >= 2, "{name}: samples 至少为 2");
            assert_eq!(
                stride, f32_fixed,
                "{name}: stride 与 wire 的每采样 f32 数不一致"
            );
            assert_eq!(
                stride * samples,
                f32_pool.len(),
                "{name}: stride*samples 与 Rust f32 池长度不一致"
            );
            assert_eq!(i32_pool.len(), 0, "{name}: 插值载荷不应写 i32 池");
        } else {
            assert_eq!(
                f32_fixed,
                f32_pool.len(),
                "{name}: wire 的 f32 数与 Rust f32 池长度不一致"
            );
            if has_ext {
                assert_eq!(i32_fixed, 0, "{name}: EXT_BAG 不应伴随定长 i32 token");
            } else if has_varlen {
                assert!(
                    i32_pool.len() >= i32_fixed + 1,
                    "{name}: 变长 i32 列表（TAGS/WORDS）至少需要一个 count 槽"
                );
            } else {
                assert_eq!(
                    i32_fixed,
                    i32_pool.len(),
                    "{name}: wire 的 i32 数与 Rust i32 池长度不一致"
                );
            }
        }
    }
}

/// 性能冒烟：把「1000 实体 × 5 命令」的帧编码 / 解析 N 次，打印吞吐。
///
/// 用 cargo test -p game-core presentation::tests::packed::bench -- --nocapture 查看输出。
#[test]
fn bench_packed_frame_throughput() {
    const ENTITIES: usize = 1000;
    let mut commands = Vec::with_capacity(ENTITIES * 5);
    for i in 0..ENTITIES as u64 {
        let id = StableEntityId(i + 1);
        commands.push(PresentationCommand::Attach { id });
        commands.push(PresentationCommand::Add {
            id,
            payload: SyncPayload::Transform(RenderTransformSample::default()),
        });
        commands.push(PresentationCommand::Update {
            id,
            payload: SyncPayload::Presentation(PresentationState {
                locomotion_state: 1,
                action_state: 0,
                overlay_tags: vec![1, 2, 3],
            }),
        });
        commands.push(PresentationCommand::Add {
            id,
            payload: SyncPayload::Health(PresentedHealth {
                current: 90.0,
                max: 100.0,
            }),
        });
        commands.push(PresentationCommand::Add {
            id,
            payload: SyncPayload::Prototype(PresentedPrototype(1 + (i % 3) as u32)),
        });
    }
    let frame = PresentationFrame {
        session: 1,
        seq: 1,
        tick: 1,
        render_clock_ms: 0,
        timestep_ms: 16.6,
        commands: commands.into(),
    };

    const ITERS: usize = 100;

    let start = Instant::now();
    for _ in 0..ITERS {
        let _ = encode_frame(&frame);
    }
    let encode_time = start.elapsed();

    let bytes = encode_frame(&frame);
    let start = Instant::now();
    for _ in 0..ITERS {
        let _ = decode_streams::<SyncPayload>(&bytes).unwrap();
    }
    let streams_time = start.elapsed();

    let start = Instant::now();
    for _ in 0..ITERS {
        let _ = decode_frame::<SyncPayload>(&bytes).unwrap();
    }
    let commands_time = start.elapsed();

    let per = |d: std::time::Duration| d.as_secs_f64() * 1e3 / ITERS as f64;
    println!(
        "packed frame: {} commands, {} bytes | encode {:.3} ms | decode-streams {:.3} ms | decode-commands {:.3} ms",
        frame.commands.len(),
        bytes.len(),
        per(encode_time),
        per(streams_time),
        per(commands_time),
    );
}
