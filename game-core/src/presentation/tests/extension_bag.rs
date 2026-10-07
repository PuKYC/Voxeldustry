//! 扩展袋语义回归测试（原型阶段：先钉语义，再展开宏）。
//!
//! 闸门用例（最可能推翻设计）：
//! - perception_change_resends_and_hides_required_field（G1，含 Required 感知变化）
//! - empty_bag_emits_remove_and_readd_emits_add（G2/G3）
//! 其余为去抖、字段序、字节稳定性、上限、清理所有权与注册顺序确定性。

use bevy::prelude::*;

use game_engine::identity::StableEntityId;
use game_engine::presentation::interp::RenderClockState;
use game_engine::presentation::packed::{decode_frame, decode_streams, encode_frame};

use crate::logic_ext::{SetOutcome, MAX_EXT_FIELDS, MAX_EXT_TAGS};
use crate::presentation::payload::{
    project, ExtError, ExtField, ExtValue, ExtensionBag, ExtensionFieldSchema, ExtensionPayload,
    ExtensionSchema, PayloadKind,
};
use crate::presentation::sync::{
    collect_extension_bag, finalize_presentation, reconcile_extension_baseline,
};
use crate::presentation::{
    CorePendingPresentation, CoreSyncBaseline, PresentationCommand, PresentationFrame,
    PresentationRuntime, PresentationSlot, PresentationVisibility, SyncPayload,
};
use crate::privacy::PerceptionMask;

fn test_app(schema: Vec<ExtensionFieldSchema>) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.insert_resource(ExtensionSchema::new(schema));
    app.init_resource::<CorePendingPresentation>();
    app.init_resource::<CoreSyncBaseline>();
    app.insert_resource(PresentationVisibility::default());
    app.add_systems(Update, collect_extension_bag);
    app
}

/// App 具备完整发布路径：collector + finalize（同帧整理 / 发布到单槽）。
fn pipeline_app(schema: Vec<ExtensionFieldSchema>) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.insert_resource(ExtensionSchema::new(schema));
    app.init_resource::<CorePendingPresentation>();
    app.init_resource::<CoreSyncBaseline>();
    app.insert_resource(PresentationVisibility::default());
    app.init_resource::<PresentationRuntime>();
    app.init_resource::<PresentationSlot>();
    app.init_resource::<RenderClockState>();
    app.init_resource::<crate::input::SimulationTick>();
    app.add_systems(
        Update,
        (collect_extension_bag, finalize_presentation).chain(),
    );
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

fn seed_bag(app: &mut App, entity: Entity, fields: &[(u16, ExtValue)]) {
    let mut em = app.world_mut().entity_mut(entity);
    let mut bag = em.get_mut::<ExtensionBag>().unwrap();
    for (key, value) in fields {
        bag.set_field(*key, value.clone());
    }
}

fn set_field(app: &mut App, entity: Entity, key: u16, value: ExtValue) {
    let mut em = app.world_mut().entity_mut(entity);
    em.get_mut::<ExtensionBag>().unwrap().set_field(key, value);
}

fn clear_field(app: &mut App, entity: Entity, key: u16) {
    let mut em = app.world_mut().entity_mut(entity);
    em.get_mut::<ExtensionBag>().unwrap().clear_field(key);
}

fn ext(cmd: &PresentationCommand) -> &[ExtField] {
    match cmd {
        PresentationCommand::Add {
            payload: SyncPayload::Extension(p),
            ..
        }
        | PresentationCommand::Update {
            payload: SyncPayload::Extension(p),
            ..
        } => p.fields(),
        other => panic!("期望扩展载荷，得到 {other:?}"),
    }
}

/// G1：字段不变，只改观察者感知 -> 必须发 Update，且 Required 字段消失；
/// 同感知重复帧 = 0 命令；恢复感知 -> 含 Required key 的 Update。
#[test]
fn perception_change_resends_and_hides_required_field() {
    let mut app = test_app(vec![
        ExtensionFieldSchema::required(0x0101, "secret", 0b0100),
        ExtensionFieldSchema::all(0x0102, "public"),
    ]);
    let entity = app
        .world_mut()
        .spawn((StableEntityId(1), ExtensionBag::default()))
        .id();
    seed_bag(
        &mut app,
        entity,
        &[(0x0101, ExtValue::I32(7)), (0x0102, ExtValue::I32(9))],
    );

    // 观察者有 0b0100 -> 两个字段都可见
    visible(&mut app, entity, 1, PerceptionMask(0b0100));
    app.update();
    let cmds = drain(&mut app);
    assert_eq!(cmds.len(), 1, "首次应 1 条 Add");
    assert!(matches!(cmds[0], PresentationCommand::Add { .. }));
    assert_eq!(ext(&cmds[0]).len(), 2);

    // 只改感知：secret 的位被剥夺 -> 必须重发
    visible(&mut app, entity, 1, PerceptionMask(0b0001));
    app.update();
    let cmds = drain(&mut app);
    assert_eq!(cmds.len(), 1, "感知变化应发 1 条 Update");
    assert!(matches!(cmds[0], PresentationCommand::Update { .. }));
    assert_eq!(
        ext(&cmds[0]).iter().map(|f| f.key).collect::<Vec<_>>(),
        vec![0x0102],
        "Required 字段应消失"
    );

    // 第三次同感知 -> 0 命令
    app.update();
    assert!(drain(&mut app).is_empty(), "投影未变不得重发");

    // 恢复感知 -> 1 条 Update 且含原 Required key
    visible(&mut app, entity, 1, PerceptionMask(0b0100));
    app.update();
    let cmds = drain(&mut app);
    assert_eq!(cmds.len(), 1);
    assert!(matches!(cmds[0], PresentationCommand::Update { .. }));
    assert_eq!(
        ext(&cmds[0]).iter().map(|f| f.key).collect::<Vec<_>>(),
        vec![0x0101, 0x0102],
        "恢复后 Required key 必须回来"
    );
}

/// G2/G3：全删 -> Remove 且清基线；再加回 -> Add（不是 Update），载荷与基线重建。
#[test]
fn empty_bag_emits_remove_and_readd_emits_add() {
    let mut app = test_app(vec![ExtensionFieldSchema::all(0x0102, "p")]);
    let entity = app
        .world_mut()
        .spawn((StableEntityId(1), ExtensionBag::default()))
        .id();
    seed_bag(&mut app, entity, &[(0x0102, ExtValue::I32(1))]);
    visible(&mut app, entity, 1, PerceptionMask::EMPTY);
    app.update();
    assert!(matches!(
        drain(&mut app).as_slice(),
        [PresentationCommand::Add { .. }]
    ));

    clear_field(&mut app, entity, 0x0102);
    app.update();
    let cmds = drain(&mut app);
    assert_eq!(cmds.len(), 1, "空袋应发 1 条 Remove");
    assert!(matches!(cmds[0], PresentationCommand::Remove { .. }));
    assert!(
        !app.world()
            .resource::<CoreSyncBaseline>()
            .has_component(StableEntityId(1), PayloadKind::Extension),
        "Remove 后基线必须清空"
    );

    set_field(&mut app, entity, 0x0102, ExtValue::I32(42));
    app.update();
    let cmds = drain(&mut app);
    assert_eq!(cmds.len(), 1);
    assert!(
        matches!(cmds[0], PresentationCommand::Add { .. }),
        "重新出现应发 Add 而非 Update"
    );
    assert_eq!(ext(&cmds[0]).len(), 1);
    assert_eq!(
        ext(&cmds[0])[0],
        ExtField {
            key: 0x0102,
            value: ExtValue::I32(42)
        }
    );
    assert!(
        app.world()
            .resource::<CoreSyncBaseline>()
            .has_component(StableEntityId(1), PayloadKind::Extension),
        "重新出现后基线必须重建"
    );
}

/// 字段按 key 升序；无变化 / 同值写入都不发命令。
#[test]
fn fields_sorted_and_debounced() {
    let mut app = test_app(vec![
        ExtensionFieldSchema::all(2, "b"),
        ExtensionFieldSchema::all(1, "a"),
    ]);
    let entity = app
        .world_mut()
        .spawn((StableEntityId(1), ExtensionBag::default()))
        .id();
    // 故意逆序写入
    seed_bag(
        &mut app,
        entity,
        &[(2, ExtValue::I32(20)), (1, ExtValue::I32(10))],
    );
    visible(&mut app, entity, 1, PerceptionMask::EMPTY);
    app.update();
    let cmds = drain(&mut app);
    assert_eq!(cmds.len(), 1);
    assert_eq!(
        ext(&cmds[0]).iter().map(|f| f.key).collect::<Vec<_>>(),
        vec![1, 2],
        "字段必须按 key 升序"
    );

    // 无变化
    app.update();
    assert!(drain(&mut app).is_empty(), "无变化不应发命令");

    // 同值写入：Bevy 会因可变借用标记 Changed，但投影比较应去抖
    set_field(&mut app, entity, 1, ExtValue::I32(10));
    app.update();
    assert!(drain(&mut app).is_empty(), "同值写入不应发命令");
}

/// 删一个字段只清该 key（整袋快照，缺席即清空）。
#[test]
fn removing_one_field_updates_without_it() {
    let mut app = test_app(vec![
        ExtensionFieldSchema::all(1, "a"),
        ExtensionFieldSchema::all(2, "b"),
    ]);
    let entity = app
        .world_mut()
        .spawn((StableEntityId(1), ExtensionBag::default()))
        .id();
    seed_bag(
        &mut app,
        entity,
        &[(1, ExtValue::I32(10)), (2, ExtValue::I32(20))],
    );
    visible(&mut app, entity, 1, PerceptionMask::EMPTY);
    app.update();
    let _ = drain(&mut app);

    clear_field(&mut app, entity, 1);
    app.update();
    let cmds = drain(&mut app);
    assert_eq!(cmds.len(), 1);
    assert!(matches!(cmds[0], PresentationCommand::Update { .. }));
    assert_eq!(
        ext(&cmds[0]).iter().map(|f| f.key).collect::<Vec<_>>(),
        vec![2]
    );
}

/// project 是纯函数，观察者作参数：ServerOnly / 未登记 key / 无授权位一律 Fail-Closed。
#[test]
fn project_is_pure_and_fail_closed() {
    let mut bag = ExtensionBag::default();
    bag.set_field(0x0101, ExtValue::I32(7));
    bag.set_field(0x0102, ExtValue::I32(9));
    bag.set_field(0x0103, ExtValue::I32(11));
    bag.set_field(0x0104, ExtValue::I32(13));
    let schema = ExtensionSchema::new(vec![
        ExtensionFieldSchema::required(0x0101, "secret", 0b0100),
        ExtensionFieldSchema::all(0x0102, "public"),
        ExtensionFieldSchema::server_only(0x0103, "server"),
        // 0x0104 未登记
    ]);

    let shown = project(&bag, PerceptionMask(0b0100), &schema);
    let hidden = project(&bag, PerceptionMask(0b0001), &schema);
    assert_eq!(
        shown.fields().iter().map(|f| f.key).collect::<Vec<_>>(),
        vec![0x0101, 0x0102],
        "有授权位：Required + All；ServerOnly/未登记不下发"
    );
    assert_eq!(
        hidden.fields().iter().map(|f| f.key).collect::<Vec<_>>(),
        vec![0x0102],
        "无授权位：Required 隐藏，All 仍可见"
    );

    // 纯函数：同输入两次结果一致（无隐藏状态）。
    assert_eq!(shown, project(&bag, PerceptionMask(0b0100), &schema));
}

/// 写入顺序不影响字段序与 GPF1 字节。
#[test]
fn insertion_order_does_not_change_bytes() {
    let schema = ExtensionSchema::new(vec![
        ExtensionFieldSchema::all(1, "a"),
        ExtensionFieldSchema::all(2, "b"),
    ]);
    let build = |first: u16| {
        let mut bag = ExtensionBag::default();
        if first == 1 {
            bag.set_field(1, ExtValue::I32(10));
            bag.set_field(2, ExtValue::Tags(vec![3, 4]));
        } else {
            bag.set_field(2, ExtValue::Tags(vec![3, 4]));
            bag.set_field(1, ExtValue::I32(10));
        }
        project(&bag, PerceptionMask::EMPTY, &schema)
    };
    let forward = build(1);
    let reverse = build(2);
    assert_eq!(forward, reverse);

    let encode = |payload: ExtensionPayload| {
        let frame = PresentationFrame {
            session: 1,
            seq: 1,
            tick: 1,
            render_clock_ms: 0,
            timestep_ms: 0.0,
            commands: vec![PresentationCommand::Add {
                id: StableEntityId(1),
                payload: SyncPayload::Extension(payload),
            }]
            .into(),
        };
        encode_frame(&frame)
    };
    assert_eq!(encode(forward), encode(reverse), "GPF1 字节必须一致");
}

/// T5：同帧先删后加同值：视为仍在、零命令；随后真正改值 -> 恰好 1 条唯一 Update。
#[test]
fn same_frame_delete_then_add_keeps_field() {
    let mut app = test_app(vec![ExtensionFieldSchema::all(1, "a")]);
    let entity = app
        .world_mut()
        .spawn((StableEntityId(1), ExtensionBag::default()))
        .id();
    seed_bag(&mut app, entity, &[(1, ExtValue::I32(10))]);
    visible(&mut app, entity, 1, PerceptionMask::EMPTY);
    app.update();
    let _ = drain(&mut app);

    // 同帧：移除袋组件，再立刻挂回，并写入同值
    app.world_mut().entity_mut(entity).remove::<ExtensionBag>();
    app.world_mut()
        .entity_mut(entity)
        .insert(ExtensionBag::default());
    set_field(&mut app, entity, 1, ExtValue::I32(10));

    app.update();
    assert!(
        drain(&mut app).is_empty(),
        "同帧删后加同值必须零命令（空集合也能过的假闸门已被强化）"
    );
    assert!(
        app.world()
            .resource::<CoreSyncBaseline>()
            .has_component(StableEntityId(1), PayloadKind::Extension),
        "基线应仍在"
    );

    // 真正改值 -> 恰好 1 条 Update，且读出唯一非空载荷
    set_field(&mut app, entity, 1, ExtValue::I32(11));
    app.update();
    let cmds = drain(&mut app);
    assert_eq!(cmds.len(), 1, "改值应恰好 1 条 Update");
    assert!(matches!(cmds[0], PresentationCommand::Update { .. }));
    assert_eq!(ext(&cmds[0]).len(), 1);
    assert_eq!(ext(&cmds[0])[0].value, ExtValue::I32(11));
}

/// T9：记录整袋命令的字节数作为基线；用 Update 命令（注释与构造一致）。
#[test]
fn extension_payload_wire_size_baseline() {
    const EXT_FRAME_BYTES: usize = 88;
    let schema = ExtensionSchema::new(vec![
        ExtensionFieldSchema::all(1, "a"),
        ExtensionFieldSchema::all(2, "b"),
    ]);
    let mut bag = ExtensionBag::default();
    bag.set_field(1, ExtValue::I32(10));
    bag.set_field(2, ExtValue::Tags(vec![3, 4, 5]));
    let payload = project(&bag, PerceptionMask::EMPTY, &schema);
    let frame = PresentationFrame {
        session: 1,
        seq: 1,
        tick: 1,
        render_clock_ms: 0,
        timestep_ms: 0.0,
        commands: vec![PresentationCommand::Update {
            id: StableEntityId(1),
            payload: SyncPayload::Extension(payload),
        }]
        .into(),
    };
    let bytes = encode_frame(&frame);
    // 44 头 + 1 kind + 8 id + 1 payload kind + 34 body(I32 9 + Tags 21)。
    assert_eq!(
        bytes.len(),
        EXT_FRAME_BYTES,
        "扩展帧字节数变了，请同步核对两端线格式"
    );
}

/// T3：改一个字段 -> 恰好一条 Update，整袋按 key 有序，再更新为 0 命令。
#[test]
fn changing_one_field_emits_single_ordered_update() {
    let mut app = test_app(vec![
        ExtensionFieldSchema::all(1, "a"),
        ExtensionFieldSchema::all(2, "b"),
    ]);
    let entity = app
        .world_mut()
        .spawn((StableEntityId(1), ExtensionBag::default()))
        .id();
    seed_bag(
        &mut app,
        entity,
        &[(1, ExtValue::I32(10)), (2, ExtValue::I32(20))],
    );
    visible(&mut app, entity, 1, PerceptionMask::EMPTY);
    app.update();
    let _ = drain(&mut app); // 首帧 Add

    set_field(&mut app, entity, 1, ExtValue::I32(11));
    app.update();
    let cmds = drain(&mut app);
    assert_eq!(cmds.len(), 1, "改一个字段应发 1 条 Update");
    assert!(matches!(cmds[0], PresentationCommand::Update { .. }));
    let fields = ext(&cmds[0]);
    assert_eq!(
        fields.iter().map(|f| f.key).collect::<Vec<_>>(),
        vec![1, 2],
        "整袋仍按 key 升序"
    );
    assert_eq!(fields[0].value, ExtValue::I32(11));

    app.update();
    assert!(drain(&mut app).is_empty(), "再更新应 0 命令");
}

/// 反查基线：基线有 Extension 但当前无袋 -> 恰好 1 条 Remove 且清基线。
#[test]
fn baseline_reverse_lookup_emits_remove() {
    let mut app = test_app(vec![ExtensionFieldSchema::all(1, "a")]);
    let entity = app.world_mut().spawn(StableEntityId(1)).id(); // 故意没有袋
    {
        let mut baseline = app.world_mut().resource_mut::<CoreSyncBaseline>();
        baseline.components.insert((1, PayloadKind::Extension));
    }
    visible(&mut app, entity, 1, PerceptionMask::EMPTY);
    app.update();
    let cmds = drain(&mut app);
    assert_eq!(cmds.len(), 1);
    assert!(matches!(
        cmds[0],
        PresentationCommand::Remove {
            kind: PayloadKind::Extension,
            ..
        }
    ));
    assert!(
        !app.world()
            .resource::<CoreSyncBaseline>()
            .has_component(StableEntityId(1), PayloadKind::Extension),
        "反查命中后基线必须清空"
    );
}

/// P2：collector 不注册时，删袋仍能由发布侧 reconcile 产生 Remove 并清基线。
#[test]
fn reconcile_removes_stale_extension_when_collector_absent() {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.init_resource::<CorePendingPresentation>();
    app.init_resource::<CoreSyncBaseline>();
    app.insert_resource(PresentationVisibility::default());
    app.init_resource::<PresentationRuntime>();
    app.init_resource::<PresentationSlot>();
    app.init_resource::<RenderClockState>();
    app.init_resource::<crate::input::SimulationTick>();
    // 只注册发布侧：reconcile + finalize；**没有** collect_extension_bag。
    // reconcile 先跑（把 Remove 交回 pending），finalize 后跑并发布。
    app.add_systems(
        Update,
        (reconcile_extension_baseline, finalize_presentation).chain(),
    );

    let entity = app.world_mut().spawn(StableEntityId(1)).id(); // 袋已被移除
    {
        let mut baseline = app.world_mut().resource_mut::<CoreSyncBaseline>();
        baseline.entities.insert(1, entity);
        baseline.components.insert((1, PayloadKind::Extension));
    }
    {
        let mut vis = app.world_mut().resource_mut::<PresentationVisibility>();
        vis.entities = vec![entity];
        vis.ids = [1u64].into_iter().collect();
    }
    app.update();
    let frame = app
        .world()
        .resource::<PresentationSlot>()
        .take()
        .expect("应发布 Remove 帧");
    assert_eq!(
        frame
            .commands
            .iter()
            .map(|c| c.kind_str())
            .collect::<Vec<_>>(),
        vec!["remove"]
    );
    assert!(!app
        .world()
        .resource::<CoreSyncBaseline>()
        .has_component(StableEntityId(1), PayloadKind::Extension));
}

/// 同帧删袋 + despawn：Despawn 吃掉 Remove，基线无残留。
#[test]
fn same_frame_remove_then_despawn_leaves_no_residue() {
    let mut app = pipeline_app(vec![ExtensionFieldSchema::all(1, "a")]);
    let entity = app
        .world_mut()
        .spawn((StableEntityId(1), ExtensionBag::default()))
        .id();
    seed_bag(&mut app, entity, &[(1, ExtValue::I32(10))]);
    visible(&mut app, entity, 1, PerceptionMask::EMPTY);
    app.update();
    let first = app
        .world()
        .resource::<PresentationSlot>()
        .take()
        .expect("首帧");
    assert!(
        first
            .commands
            .iter()
            .any(|c| matches!(c, PresentationCommand::Add { .. })),
        "首帧应含 Add"
    );

    // 同帧：离开视野 + 删袋 + despawn。
    {
        let mut vis = app.world_mut().resource_mut::<PresentationVisibility>();
        vis.entities = Vec::new();
        vis.ids.clear();
        vis.local_perception = PerceptionMask::EMPTY;
    }
    app.world_mut().entity_mut(entity).remove::<ExtensionBag>();
    app.world_mut().despawn(entity);
    app.update();

    let frame = app
        .world()
        .resource::<PresentationSlot>()
        .take()
        .expect("终结帧");
    assert_eq!(
        frame
            .commands
            .iter()
            .map(|c| c.kind_str())
            .collect::<Vec<_>>(),
        vec!["despawn"],
        "Despawn 必须吃掉同帧 Remove"
    );
    assert!(!app
        .world()
        .resource::<CoreSyncBaseline>()
        .has_component(StableEntityId(1), PayloadKind::Extension));
}

/// P6：离开视野再回来必须重新发 Attach + Add（last_sent 以基线为准，不会误判为未变）。
#[test]
fn leave_visibility_then_return_reemits_add() {
    let mut app = pipeline_app(vec![ExtensionFieldSchema::all(1, "a")]);
    let entity = app
        .world_mut()
        .spawn((StableEntityId(1), ExtensionBag::default()))
        .id();
    seed_bag(&mut app, entity, &[(1, ExtValue::I32(10))]);
    visible(&mut app, entity, 1, PerceptionMask::EMPTY);
    app.update();
    let first = app
        .world()
        .resource::<PresentationSlot>()
        .take()
        .expect("首帧");
    assert!(first
        .commands
        .iter()
        .any(|c| matches!(c, PresentationCommand::Add { .. })));

    // 离开视野 -> Detach，基线清空。
    {
        let mut vis = app.world_mut().resource_mut::<PresentationVisibility>();
        vis.entities = Vec::new();
        vis.ids.clear();
    }
    app.update();
    let gone = app
        .world()
        .resource::<PresentationSlot>()
        .take()
        .expect("Detach 帧");
    assert_eq!(
        gone.commands
            .iter()
            .map(|c| c.kind_str())
            .collect::<Vec<_>>(),
        vec!["detach"]
    );
    assert!(!app
        .world()
        .resource::<CoreSyncBaseline>()
        .has_component(StableEntityId(1), PayloadKind::Extension));

    // 回到视野 -> 必须重新 Attach + Add，而不是静默复用旧 last_sent。
    visible(&mut app, entity, 1, PerceptionMask::EMPTY);
    app.update();
    let back = app
        .world()
        .resource::<PresentationSlot>()
        .take()
        .expect("回归帧");
    let kinds: Vec<&str> = back.commands.iter().map(|c| c.kind_str()).collect();
    assert!(
        kinds.contains(&"attach"),
        "回归必须重新 Attach，得到 {kinds:?}"
    );
    assert!(kinds.contains(&"add"), "回归必须重新 Add，得到 {kinds:?}");

    // 稳定后不再发帧。
    app.update();
    assert!(
        !app.world().resource::<PresentationSlot>().has_pending(),
        "稳定后不应再发帧"
    );
}

/// 上限与规范化：1024 字段、1025 拒绝；Tags 排序去重；4097 截断。
#[test]
fn caps_and_normalization() {
    let mut bag = ExtensionBag::default();
    for key in 0..MAX_EXT_FIELDS as u16 {
        assert_eq!(
            bag.set_field(key, ExtValue::I32(i32::from(key))),
            SetOutcome::Changed
        );
    }
    assert_eq!(bag.field_count(), MAX_EXT_FIELDS);
    assert_eq!(
        bag.set_field(MAX_EXT_FIELDS as u16, ExtValue::I32(0)),
        SetOutcome::Rejected,
        "第 1025 个新字段必须被拒绝"
    );
    assert_eq!(bag.field_count(), MAX_EXT_FIELDS);
    assert_eq!(
        bag.set_field(0, ExtValue::I32(123)),
        SetOutcome::Changed,
        "已存在键满袋仍可更新"
    );
    assert_eq!(bag.set_field(0, ExtValue::I32(123)), SetOutcome::Unchanged);

    let mut tags_bag = ExtensionBag::default();
    assert_eq!(
        tags_bag.set_field(1, ExtValue::Tags(vec![9, 1, 1, 5])),
        SetOutcome::Changed
    );
    assert_eq!(
        tags_bag.sorted_fields(),
        vec![(1, ExtValue::Tags(vec![1, 5, 9]))]
    );

    let big: Vec<u32> = (0..=(MAX_EXT_TAGS as u32)).rev().collect();
    tags_bag.set_field(2, ExtValue::Tags(big));
    let fields = tags_bag.sorted_fields();
    match &fields[1].1 {
        ExtValue::Tags(tags) => assert_eq!(tags.len(), MAX_EXT_TAGS, "超限 Tags 必须截断"),
        other => panic!("期望 Tags，得到 {other:?}"),
    }
}

/// 公开构造路径：from_fields 对超限返回明确错误。
#[test]
fn from_fields_rejects_over_cap() {
    let too_many: Vec<ExtField> = (0..=MAX_EXT_FIELDS)
        .map(|i| ExtField {
            key: i as u16,
            value: ExtValue::I32(0),
        })
        .collect();
    assert_eq!(
        ExtensionPayload::from_fields(too_many),
        Err(ExtError::TooManyFields)
    );

    let too_many_tags = vec![ExtField {
        key: 1,
        value: ExtValue::Tags(vec![0; MAX_EXT_TAGS + 1]),
    }];
    assert_eq!(
        ExtensionPayload::from_fields(too_many_tags),
        Err(ExtError::TooManyTags)
    );

    // 乱序输入被规范化成 key 升序。
    let sorted = ExtensionPayload::from_fields(vec![
        ExtField {
            key: 2,
            value: ExtValue::I32(2),
        },
        ExtField {
            key: 1,
            value: ExtValue::I32(1),
        },
    ])
    .unwrap();
    assert_eq!(
        sorted.fields().iter().map(|f| f.key).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

/// T8：注册顺序不影响 GPF1 字节（覆盖 Extension 与普通载荷相邻的多命令 offset）。
#[test]
fn registration_order_does_not_change_frame_bytes() {
    use crate::presentation::payload::{InteractionHint, PresentationState};
    use crate::presentation::PresentationAppExt;

    fn order_app(reverse: bool) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(ExtensionSchema::new(vec![ExtensionFieldSchema::all(
            1, "a",
        )]));
        app.init_resource::<CorePendingPresentation>();
        app.init_resource::<CoreSyncBaseline>();
        app.insert_resource(PresentationVisibility::default());
        app.init_resource::<PresentationRuntime>();
        app.init_resource::<PresentationSlot>();
        app.init_resource::<RenderClockState>();
        app.init_resource::<crate::input::SimulationTick>();
        if reverse {
            app.present::<InteractionHint>();
            app.present::<PresentationState>();
        } else {
            app.present::<PresentationState>();
            app.present::<InteractionHint>();
        }
        // 与生产一致：所有 collector 在 Collect 集合，finalize 在 Finalize 集合，
        // 两个集合显式排序，避免 finalize 与 collector 交错导致帧内容随机。
        app.add_systems(
            Update,
            collect_extension_bag.in_set(game_engine::presentation::CollectPresentationSet),
        );
        app.add_systems(
            Update,
            finalize_presentation.in_set(game_engine::presentation::FinalizePresentationSet),
        );
        app.configure_sets(
            Update,
            game_engine::presentation::CollectPresentationSet
                .before(game_engine::presentation::FinalizePresentationSet),
        );
        app
    }

    let build = |reverse: bool| {
        let mut app = order_app(reverse);
        let entity = app
            .world_mut()
            .spawn((
                StableEntityId(1),
                PresentationState {
                    locomotion_state: 3,
                    action_state: 1,
                    overlay_tags: vec![7],
                },
                InteractionHint {
                    action: crate::input::actions::ActionId(2),
                    enabled: true,
                },
                ExtensionBag::default(),
            ))
            .id();
        seed_bag(&mut app, entity, &[(1, ExtValue::I32(99))]);
        visible(&mut app, entity, 1, PerceptionMask::EMPTY);
        app.update();
        let frame = app
            .world()
            .resource::<PresentationSlot>()
            .take()
            .expect("帧已发布");
        encode_frame(&frame)
    };

    let forward = build(false);
    let reverse = build(true);
    assert_eq!(forward, reverse, "注册顺序不得影响 GPF1 字节");
}

/// 跨端线格式闭环：Extension 的 put_body / read_body 往返 + 池布局精确断言
/// （Godot 两个解码器都假定这个池布局，是唯一的防漂移闸门）。
#[test]
fn extension_encode_decode_roundtrip_and_pool_layout() {
    let payload = ExtensionPayload::from_fields(vec![
        ExtField {
            key: 1,
            value: ExtValue::I32(-5),
        },
        ExtField {
            key: 2,
            value: ExtValue::Bool(true),
        },
        ExtField {
            key: 3,
            value: ExtValue::Tags(vec![7, 8]),
        },
    ])
    .unwrap();
    let frame = PresentationFrame {
        session: 1,
        seq: 1,
        tick: 1,
        render_clock_ms: 0,
        timestep_ms: 0.0,
        commands: vec![PresentationCommand::Add {
            id: StableEntityId(1),
            payload: SyncPayload::Extension(payload.clone()),
        }]
        .into(),
    };
    let bytes = encode_frame(&frame);

    // 池布局：count, 然后每字段 [key, tag, value]；Tags 为 n + n 槽。
    let (_header, streams) = decode_streams::<SyncPayload>(&bytes).expect("解析扩展帧");
    assert_eq!(
        streams.i32_pool,
        vec![3, 1, 0, -5, 2, 1, 1, 3, 2, 2, 7, 8],
        "i32 池布局必须与 Godot 解码器一致"
    );

    // 命令级往返
    let (_h2, commands) = decode_frame::<SyncPayload>(&bytes).expect("解析扩展帧命令");
    assert_eq!(commands.len(), 1);
    match &commands[0] {
        PresentationCommand::Add {
            payload: SyncPayload::Extension(decoded),
            ..
        } => assert_eq!(decoded, &payload, "Extension 往返必须无损"),
        other => panic!("期望 Add(Extension)，得到 {other:?}"),
    }
}
