//! mod 自定义表现载荷：线格式、上限、sub_kind 表与 collector 语义。
//!
//! 闸门用例：
//! - [wire_round_trip_sorts_groups_and_fields]（字节 / SoA 池双向往返 + 规范化）
//! - [collector_adds_updates_and_removes]（Add / Update / Remove + 基线重建）
//! - [collector_filters_server_only_and_required_by_perception]（Fail-Closed 可见性）

use bevy::prelude::*;

use game_engine::identity::StableEntityId;
use game_engine::modding::{
    ModFieldSpec, ModPayloadSpec, PresentationResolveError, ResolvedModPayloads,
};
use game_engine::presentation::packed::{
    put_u16, put_u32, put_u8, PackedError, PackedPayload, Reader,
};

use crate::logic_ext::{SetOutcome, MAX_EXT_FIELDS, MAX_EXT_TAGS};
use crate::presentation::payload::{
    mod_payload_table, publish_mod_payloads, CustomPayload, ExtField, ExtValue, ModPayloadError,
    ModPayloadGroup, ModPresentation, MAX_MOD_PAYLOAD_GROUPS,
};
use crate::presentation::sync::collect_mod_presentation;
use crate::presentation::{
    CorePendingPresentation, CoreSyncBaseline, PresentationCommand, PresentationVisibility,
    SyncPayload,
};
use crate::privacy::PerceptionMask;

// ───────────────────── 测试表 ─────────────────────

/// 一个子种类：三个字段覆盖三种可见性。
static AURA_FIELDS: &[ModFieldSpec] = &[
    ModFieldSpec::all(1, "hp"),
    ModFieldSpec::server_only(2, "secret"),
    ModFieldSpec::required(3, "owner", 0b0100),
];
static AURA_SPECS: &[ModPayloadSpec] = &[ModPayloadSpec {
    name: "aura",
    fields: AURA_FIELDS,
}];

fn field(key: u16, value: ExtValue) -> ExtField {
    ExtField { key, value }
}

fn group(sub_kind: u16, fields: Vec<ExtField>) -> ModPayloadGroup {
    ModPayloadGroup { sub_kind, fields }
}

/// 写字节体并推池；返回（字节，f32 池，i32 池）。
fn encode(payload: &CustomPayload) -> (Vec<u8>, Vec<f32>, Vec<i64>) {
    let mut bytes = Vec::new();
    payload.put_body(&mut bytes);
    let mut f = Vec::new();
    let mut i = Vec::new();
    payload.write_pools(&mut f, &mut i);
    (bytes, f, i)
}

/// 解字节体到池，并断言体正好消费完（read_body 与 write_pools 必须逐项对应）。
fn decode_body(bytes: &[u8]) -> (Vec<f32>, Vec<i64>) {
    let mut r = Reader::new(bytes);
    let mut f = Vec::new();
    let mut i = Vec::new();
    CustomPayload::read_body(&mut r, &mut f, &mut i).expect("mod 载荷体必须能解码");
    assert!(r.u8().is_err(), "载荷体必须正好消费完，不能有尾部字节");
    (f, i)
}

// ───────────────────── 线格式 ─────────────────────

/// 组 / 字段乱序输入 -> 规范化为升序；字节体与 SoA 池都能无损往返。
#[test]
fn wire_round_trip_sorts_groups_and_fields() {
    let payload = CustomPayload::from_groups(vec![
        group(9, vec![field(3, ExtValue::Tags(vec![1, 5]))]),
        group(
            2,
            vec![field(2, ExtValue::Bool(true)), field(1, ExtValue::I32(-7))],
        ),
    ])
    .expect("样本身份合法");

    assert_eq!(
        payload
            .groups()
            .iter()
            .map(|group| group.sub_kind)
            .collect::<Vec<_>>(),
        vec![2, 9],
        "组必须按 sub_kind 升序"
    );
    assert_eq!(
        payload.groups()[0]
            .fields
            .iter()
            .map(|field| field.key)
            .collect::<Vec<_>>(),
        vec![1, 2],
        "组内字段必须按 key 升序"
    );

    let (bytes, f, i) = encode(&payload);
    // 布局自检：u32 组数 + 每组 u16 sub_kind + 字段编码（与扩展袋一致）。
    assert_eq!(u32::from_le_bytes(bytes[0..4].try_into().unwrap()), 2);
    assert_eq!(u16::from_le_bytes(bytes[4..6].try_into().unwrap()), 2);

    let (f2, i2) = decode_body(&bytes);
    assert_eq!(f2, f, "read_body 与 write_pools 的 f32 池必须一致");
    assert_eq!(i2, i, "read_body 与 write_pools 的 i32 池必须逐槽一致");

    let decoded = CustomPayload::from_pools(&f2, &i2, 0, 0);
    assert_eq!(decoded, payload, "池往返必须无损");

    // 空载荷同样往返（组数 0）。
    let empty = CustomPayload::default();
    let decoded_empty = CustomPayload::from_pools(&[][..], &[0][..], 0, 0);
    assert_eq!(decoded_empty, empty);
    let (bytes, _, _) = encode(&empty);
    assert_eq!(bytes, 0u32.to_le_bytes().to_vec());
    let (f, i) = decode_body(&bytes);
    assert!(f.is_empty());
    assert_eq!(i, vec![0], "空载荷在池里仍占一个组数槽");
}

/// 越界与重复都必须被公开构造入口拒收（错误变体可区分）。
#[test]
fn overflow_and_duplicate_groups_are_rejected() {
    let too_many_groups: Vec<ModPayloadGroup> = (0..=MAX_MOD_PAYLOAD_GROUPS as u16)
        .map(|sub_kind| group(sub_kind, Vec::new()))
        .collect();
    assert_eq!(
        CustomPayload::from_groups(too_many_groups).unwrap_err(),
        ModPayloadError::TooManyGroups
    );

    assert_eq!(
        CustomPayload::from_groups(vec![group(1, Vec::new()), group(1, Vec::new())]).unwrap_err(),
        ModPayloadError::DuplicateSubKind { sub_kind: 1 }
    );

    let too_many_fields: Vec<ExtField> = (0..=MAX_EXT_FIELDS as u16)
        .map(|key| field(key, ExtValue::I32(0)))
        .collect();
    assert_eq!(
        CustomPayload::from_groups(vec![group(2, too_many_fields)]).unwrap_err(),
        ModPayloadError::TooManyFields { sub_kind: 2 }
    );

    assert_eq!(
        CustomPayload::from_groups(vec![group(
            3,
            vec![field(7, ExtValue::I32(1)), field(7, ExtValue::I32(2))]
        )])
        .unwrap_err(),
        ModPayloadError::DuplicateFieldKey {
            sub_kind: 3,
            key: 7
        }
    );

    let too_many_tags = vec![0u32; MAX_EXT_TAGS + 1];
    assert_eq!(
        CustomPayload::from_groups(vec![group(
            4,
            vec![field(8, ExtValue::Tags(too_many_tags))]
        )])
        .unwrap_err(),
        ModPayloadError::TooManyTags {
            sub_kind: 4,
            key: 8
        }
    );
}

/// read_body 对超限组数 / 未知字段 tag 返回既有 PackedError（不 panic、不静默）。
#[test]
fn read_body_rejects_oversized_group_count_and_unknown_tag() {
    let mut bytes = Vec::new();
    put_u32(&mut bytes, MAX_MOD_PAYLOAD_GROUPS as u32 + 1);
    let mut r = Reader::new(&bytes);
    let mut f = Vec::new();
    let mut i = Vec::new();
    assert_eq!(
        CustomPayload::read_body(&mut r, &mut f, &mut i),
        Err(PackedError::TooManyModGroups)
    );

    // 1 组、1 字段、tag 9（未知）：无长度信息，整帧拒绝。
    let mut bytes = Vec::new();
    put_u32(&mut bytes, 1);
    put_u16(&mut bytes, 1);
    put_u32(&mut bytes, 1);
    put_u32(&mut bytes, 1);
    put_u8(&mut bytes, 9);
    let mut r = Reader::new(&bytes);
    assert_eq!(
        CustomPayload::read_body(&mut r, &mut f, &mut i),
        Err(PackedError::BadExtensionTag(9))
    );
}

/// 组件写入路径同样限流：组数 / 字段数满了返回 Rejected，Tags 归一化。
#[test]
fn presentation_writes_are_normalized_and_limited() {
    let mut presentation = ModPresentation::default();
    // 同值写入 -> Unchanged；Tags 排序去重。
    assert_eq!(
        presentation.set_field(1, 1, ExtValue::Tags(vec![3, 1, 3])),
        SetOutcome::Changed
    );
    assert_eq!(
        presentation.groups()[0].field(1).expect("字段存在").value,
        ExtValue::Tags(vec![1, 3]),
        "Tags 必须排序去重"
    );
    assert_eq!(
        presentation.set_field(1, 1, ExtValue::Tags(vec![1, 3])),
        SetOutcome::Unchanged
    );

    // 字段数上限（组 2）。
    for key in 0..MAX_EXT_FIELDS as u16 {
        presentation.set_field(2, key, ExtValue::I32(0));
    }
    assert_eq!(
        presentation.set_field(2, MAX_EXT_FIELDS as u16, ExtValue::I32(0)),
        SetOutcome::Rejected,
        "字段数满后新 key 必须被拒"
    );

    // 组数上限：已有 2 组，再补到 32 组后拒绝第 33 组。
    for sub_kind in 3..=MAX_MOD_PAYLOAD_GROUPS as u16 {
        assert_eq!(
            presentation.set_field(sub_kind, 1, ExtValue::I32(0)),
            SetOutcome::Changed
        );
    }
    assert_eq!(
        presentation.set_field(MAX_MOD_PAYLOAD_GROUPS as u16 + 1, 1, ExtValue::I32(0)),
        SetOutcome::Rejected,
        "组数满后新 sub_kind 必须被拒"
    );
    assert!(presentation.clear_group(1));
    assert!(!presentation.clear_group(1), "重复清理必须返回 false");
}

// ───────────────────── 全局快照 ─────────────────────

/// 发布后的表可从全局快照读回（Godot 线程路径），sub_kind 与字段升序。
#[test]
fn published_table_is_readable_from_global_snapshot() {
    let resolved = ResolvedModPayloads::resolve(&[("mod.test", AURA_SPECS)]).expect("表合法");
    publish_mod_payloads(&resolved);

    let table = mod_payload_table();
    assert_eq!(table.len(), 1);
    assert_eq!(table[0].sub_kind, 1, "mod 的 sub_kind 从 1 起");
    assert_eq!(table[0].mod_id, "mod.test");
    assert_eq!(table[0].name, "aura");
    assert_eq!(
        table[0]
            .fields
            .iter()
            .map(|field| field.key)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

// ───────────────────── collector ─────────────────────

fn test_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.insert_resource(ResolvedModPayloads::resolve(&[("mod.test", AURA_SPECS)]).expect("表合法"));
    app.init_resource::<CorePendingPresentation>();
    app.init_resource::<CoreSyncBaseline>();
    app.insert_resource(PresentationVisibility::default());
    app.add_systems(Update, collect_mod_presentation);
    app
}

fn visible(app: &mut App, entity: Entity, stable: u64, mask: PerceptionMask) {
    let mut vis = app.world_mut().resource_mut::<PresentationVisibility>();
    vis.local_perception = mask;
    vis.entities = vec![entity];
    vis.ids = [stable].into_iter().collect();
}

fn drain(app: &mut App) -> Vec<PresentationCommand> {
    let mut pending = app.world_mut().resource_mut::<CorePendingPresentation>();
    std::mem::take(&mut pending.commands)
}

fn write(app: &mut App, entity: Entity, sub_kind: u16, key: u16, value: ExtValue) -> SetOutcome {
    let mut em = app.world_mut().entity_mut(entity);
    em.get_mut::<ModPresentation>()
        .expect("实体带 ModPresentation")
        .set_field(sub_kind, key, value)
}

fn clear_group(app: &mut App, entity: Entity, sub_kind: u16) {
    let mut em = app.world_mut().entity_mut(entity);
    em.get_mut::<ModPresentation>()
        .expect("实体带 ModPresentation")
        .clear_group(sub_kind);
}

fn custom(cmd: &PresentationCommand) -> &CustomPayload {
    match cmd {
        PresentationCommand::Add {
            payload: SyncPayload::Custom(payload),
            ..
        }
        | PresentationCommand::Update {
            payload: SyncPayload::Custom(payload),
            ..
        } => payload,
        other => panic!("期望 Custom 载荷，得到 {other:?}"),
    }
}

/// 可见字段的 (sub_kind, keys) 视图。
fn shape(payload: &CustomPayload) -> Vec<(u16, Vec<u16>)> {
    payload
        .groups()
        .iter()
        .map(|group| {
            (
                group.sub_kind,
                group.fields.iter().map(|field| field.key).collect(),
            )
        })
        .collect()
}

/// Add -> Update（同值 0 命令）-> Remove（清空组）-> 再写回 Add（基线重建）。
#[test]
fn collector_adds_updates_and_removes() {
    let mut app = test_app();
    let entity = app
        .world_mut()
        .spawn((StableEntityId(1), ModPresentation::default()))
        .id();
    write(&mut app, entity, 1, 1, ExtValue::I32(5));
    visible(&mut app, entity, 1, PerceptionMask::EMPTY);
    app.update();
    let cmds = drain(&mut app);
    assert_eq!(cmds.len(), 1, "首次必须 1 条 Add");
    assert!(matches!(cmds[0], PresentationCommand::Add { .. }));
    assert_eq!(
        shape(custom(&cmds[0])),
        vec![(1, vec![1])],
        "ServerOnly / Required 未命中，只发 All 字段"
    );

    write(&mut app, entity, 1, 1, ExtValue::I32(6));
    app.update();
    let cmds = drain(&mut app);
    assert_eq!(cmds.len(), 1);
    assert!(matches!(cmds[0], PresentationCommand::Update { .. }));

    // 同值写入：投影未变，不得重发。
    write(&mut app, entity, 1, 1, ExtValue::I32(6));
    app.update();
    assert!(drain(&mut app).is_empty(), "投影未变不得重发");

    clear_group(&mut app, entity, 1);
    app.update();
    let cmds = drain(&mut app);
    assert_eq!(cmds.len(), 1, "空包应发 1 条 Remove");
    assert!(matches!(
        cmds[0],
        PresentationCommand::Remove {
            kind: crate::presentation::PayloadKind::Custom,
            ..
        }
    ));

    // 基线已清：再写回必须是 Add，不是 Update。
    write(&mut app, entity, 1, 1, ExtValue::I32(7));
    app.update();
    let cmds = drain(&mut app);
    assert_eq!(cmds.len(), 1, "重挂必须重新 Add");
    assert!(matches!(cmds[0], PresentationCommand::Add { .. }));
}

/// ServerOnly 恒隐藏；Required 只在观察者命中 required_bits 时出现。
#[test]
fn collector_filters_server_only_and_required_by_perception() {
    let mut app = test_app();
    let entity = app
        .world_mut()
        .spawn((StableEntityId(1), ModPresentation::default()))
        .id();
    write(&mut app, entity, 1, 1, ExtValue::I32(1));
    write(&mut app, entity, 1, 2, ExtValue::I32(2));
    write(&mut app, entity, 1, 3, ExtValue::I32(3));

    // 空感知：只发 All。
    visible(&mut app, entity, 1, PerceptionMask::EMPTY);
    app.update();
    let cmds = drain(&mut app);
    assert_eq!(shape(custom(&cmds[0])), vec![(1, vec![1])]);

    // 命中 Required 位 -> Update 带上 key 3；key 2 仍隐藏。
    visible(&mut app, entity, 1, PerceptionMask(0b0100));
    app.update();
    let cmds = drain(&mut app);
    assert_eq!(cmds.len(), 1, "感知变化必须重发");
    assert!(matches!(cmds[0], PresentationCommand::Update { .. }));
    assert_eq!(shape(custom(&cmds[0])), vec![(1, vec![1, 3])]);

    // 失去感知位 -> 退回只剩 All。
    visible(&mut app, entity, 1, PerceptionMask(0b0001));
    app.update();
    let cmds = drain(&mut app);
    assert_eq!(shape(custom(&cmds[0])), vec![(1, vec![1])]);
}

/// 找不到 schema 的 sub_kind 整组丢弃；整包因此变空时按 Remove 处理。
#[test]
fn collector_drops_groups_without_schema() {
    let mut app = test_app();
    let entity = app
        .world_mut()
        .spawn((StableEntityId(1), ModPresentation::default()))
        .id();
    write(&mut app, entity, 9, 1, ExtValue::I32(1));
    visible(&mut app, entity, 1, PerceptionMask::EMPTY);
    app.update();
    assert!(drain(&mut app).is_empty(), "无 schema 的组不得产生任何载荷");

    write(&mut app, entity, 1, 1, ExtValue::I32(2));
    app.update();
    let cmds = drain(&mut app);
    assert_eq!(shape(custom(&cmds[0])), vec![(1, vec![1])]);

    clear_group(&mut app, entity, 1);
    app.update();
    assert!(matches!(
        drain(&mut app).as_slice(),
        [PresentationCommand::Remove { .. }]
    ));
}

/// 分配错误是结构化变体（重名 / 重复 key），且 Display 是中文。
#[test]
fn resolve_errors_are_structured() {
    static DUP_KINDS: &[ModPayloadSpec] = &[
        ModPayloadSpec {
            name: "aura",
            fields: &[],
        },
        ModPayloadSpec {
            name: "aura",
            fields: &[],
        },
    ];
    let error = ResolvedModPayloads::resolve(&[("mod.dup", DUP_KINDS)]).unwrap_err();
    assert!(matches!(
        error,
        PresentationResolveError::DuplicateKindName { .. }
    ));
    assert!(error.to_string().contains("子种类名重复"), "{error}");

    static DUP_KEYS: &[ModFieldSpec] = &[ModFieldSpec::all(1, "a"), ModFieldSpec::all(1, "b")];
    static DUP_KEYS_SPECS: &[ModPayloadSpec] = &[ModPayloadSpec {
        name: "beam",
        fields: DUP_KEYS,
    }];
    let error = ResolvedModPayloads::resolve(&[("mod.dup", DUP_KEYS_SPECS)]).unwrap_err();
    assert!(matches!(
        error,
        PresentationResolveError::DuplicateFieldKey { .. }
    ));
    assert!(error.to_string().contains("字段 key 重复"), "{error}");
}
