//! ResidencyPlugin 的单测：最小 App + 手动推进 FixedUpdate，不依赖墙钟 / 渲染。

use bevy::prelude::*;

// `Real` 与 bevy::prelude::Real 重名，显式导入以压过 glob。
use super::Observer as ObserverTag;
use super::Real;
use super::*;
use crate::math::{FixedPoint, Vec3F};

// ───────────────────────────── 测试脚手架 ─────────────────────────────

#[derive(Resource, Default)]
struct Recorder {
    changes: Vec<ResidencyChanged>,
}

fn record_changes(mut reader: MessageReader<ResidencyChanged>, mut recorder: ResMut<Recorder>) {
    recorder.changes.extend(reader.read().copied());
}

struct Harness {
    app: App,
}

impl Harness {
    fn new(config: ResidencyConfig) -> Self {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(ResidencyPlugin { config });
        app.init_resource::<Recorder>();
        app.add_systems(FixedUpdate, record_changes.after(ResidencySet::Emit));
        Self { app }
    }

    fn with_grace(grace: u64) -> Self {
        Self::new(ResidencyConfig {
            demote_grace_ticks: grace,
            memory_soft_limit: None,
        })
    }

    /// 带内存软上限的装配；[`MemoryPressure`] 由各测试按需插入（缺失 = 无压力）。
    fn with_soft_limit(grace: u64, limit: Option<u32>) -> Self {
        Self::new(ResidencyConfig {
            demote_grace_ticks: grace,
            memory_soft_limit: limit,
        })
    }

    /// 覆盖写入（或插入）本 tick 的内存压力快照。
    fn set_pressure(&mut self, used: usize, capacity: usize) {
        self.app
            .world_mut()
            .insert_resource(MemoryPressure::new(used, capacity));
    }

    fn tick(&mut self) {
        self.app.world_mut().run_schedule(FixedUpdate);
        self.app.world_mut().flush();
    }

    fn ticks(&mut self, count: usize) {
        for _ in 0..count {
            self.tick();
        }
    }

    fn flush(&mut self) {
        self.app.world_mut().flush();
    }

    fn take_changes(&mut self) -> Vec<ResidencyChanged> {
        let mut recorder = self.app.world_mut().resource_mut::<Recorder>();
        std::mem::take(&mut recorder.changes)
    }

    fn table(&self) -> &ResidencyTable {
        self.app.world().resource::<ResidencyTable>()
    }

    fn stats(&self) -> ResidencyStats {
        *self.app.world().resource::<ResidencyStats>()
    }

    fn spawn_real(&mut self, id: u64, pos: LogicPos) -> Entity {
        self.app
            .world_mut()
            .spawn((StableEntityId(id), Real, pos))
            .id()
    }

    fn spawn_observer(&mut self, id: u64, pos: LogicPos, radius: u8) -> Entity {
        self.app
            .world_mut()
            .spawn((StableEntityId(id), ObserverTag { radius }, pos))
            .id()
    }

    fn send_reached(&mut self, body: BodyKey, key: ChunkKey, level: Residency) {
        self.app
            .world_mut()
            .write_message(ChunkReached { body, key, level });
    }

    fn awaiting(&self, entity: Entity) -> bool {
        self.app.world().entity(entity).contains::<AwaitingChunks>()
    }
}

fn body(id: u64) -> BodyKey {
    BodyKey(StableEntityId(id))
}

fn key(x: i32, y: i32, z: i32) -> ChunkKey {
    ChunkKey::new(x, y, z)
}

/// 体素坐标 → 定点逻辑坐标（1 体素 = VOXEL_SIZE_RAW 个原始位）。
fn pos_voxels(x: i64, y: i64, z: i64) -> LogicPos {
    LogicPos(Vec3F::new(
        FixedPoint::from_bits(x * VOXEL_SIZE_RAW),
        FixedPoint::from_bits(y * VOXEL_SIZE_RAW),
        FixedPoint::from_bits(z * VOXEL_SIZE_RAW),
    ))
}

// ───────────────────────────── 档位与块归属 ─────────────────────────────

#[test]
fn residency_ord_is_none_data_render() {
    assert!(Residency::None < Residency::Data);
    assert!(Residency::Data < Residency::Render);
    assert!(Residency::None < Residency::Render);
    assert_eq!(
        [Residency::Render, Residency::None, Residency::Data]
            .into_iter()
            .max(),
        Some(Residency::Render)
    );
    assert_eq!(Residency::default(), Residency::None);
}

#[test]
fn chunk_edge_raw_matches_documented_value() {
    assert_eq!(VOXEL_SIZE_RAW, 7_549_747);
    // 注意：实现文档 / 任务书写的是 241_591_968，但 7_549_747 * 32 = 241_591_904
    // （241_591_968 / 32 = 7_549_749，不是 7_549_747）。以算术为准。
    assert_eq!(VOXEL_SIZE_RAW * CHUNK_VOXELS_PER_AXIS, 241_591_904);
    assert_eq!(CHUNK_EDGE_RAW, 241_591_904);
    // 与 crates/voxel 的真实 CHUNK_DEPTH 对齐，防止两边各写一个 32。
    assert_eq!(
        i64::from(crate::voxel::chunk_voxels_per_axis()),
        CHUNK_VOXELS_PER_AXIS
    );
}

#[test]
fn anchor_negative_coordinates_use_floor_division() {
    // 体素坐标向下取整到块：-1 体素属于 -1 块，而不是 0 块。
    assert_eq!(chunk_of(pos_voxels(-1, 0, 0).0), key(-1, 0, 0));
    assert_eq!(chunk_of(pos_voxels(-32, 0, 0).0), key(-1, 0, 0));
    assert_eq!(chunk_of(pos_voxels(-33, 0, 0).0), key(-2, 0, 0));
    assert_eq!(chunk_of(pos_voxels(31, 0, 0).0), key(0, 0, 0));
    assert_eq!(chunk_of(pos_voxels(32, 0, 0).0), key(1, 0, 0));

    // 三个轴都要正确。
    assert_eq!(chunk_of(pos_voxels(-1, -33, 64).0), key(-1, -2, 2));

    assert_eq!(chunk_coord(0), 0);
    assert_eq!(chunk_coord(-1), -1);
    assert_eq!(chunk_coord(-CHUNK_EDGE_RAW), -1);
    assert_eq!(chunk_coord(-CHUNK_EDGE_RAW - 1), -2);
}

#[test]
fn update_anchors_writes_negative_chunk() {
    let mut harness = Harness::new(ResidencyConfig::default());
    let entity = harness.spawn_real(1, pos_voxels(-1, -33, 0));

    harness.tick();

    let anchor = *harness
        .app
        .world()
        .entity(entity)
        .get::<ChunkAnchor>()
        .unwrap();
    assert_eq!(anchor.body, body(1));
    assert_eq!(anchor.chunk, key(-1, -2, 0));
    // 负块坐标同样进请求表。
    assert!(harness.table().is_requested(body(1), key(-1, -2, 0)));
}

// ───────────────────────────── BodyRef 覆盖 ─────────────────────────────

#[test]
fn body_ref_overrides_self_id() {
    let mut harness = Harness::new(ResidencyConfig::default());
    let entity = harness
        .app
        .world_mut()
        .spawn((
            StableEntityId(7),
            Real,
            pos_voxels(32, 0, 0),
            BodyRef(body(99)),
        ))
        .id();

    harness.tick();

    let anchor = *harness
        .app
        .world()
        .entity(entity)
        .get::<ChunkAnchor>()
        .unwrap();
    assert_eq!(anchor.body, body(99));
    assert_eq!(anchor.chunk, key(1, 0, 0));
    // 请求记在覆盖后的体上，而不是实体自己的 ID 上。
    assert!(harness.table().is_requested(body(99), key(1, 0, 0)));
    assert!(!harness.table().is_requested(body(7), key(1, 0, 0)));
}

#[test]
fn missing_body_ref_falls_back_to_self() {
    let mut harness = Harness::new(ResidencyConfig::default());
    let entity = harness.spawn_real(7, pos_voxels(0, 0, 0));

    harness.tick();

    let anchor = *harness
        .app
        .world()
        .entity(entity)
        .get::<ChunkAnchor>()
        .unwrap();
    assert_eq!(anchor.body, body(7));
    assert_eq!(anchor.chunk, key(0, 0, 0));
}

#[test]
fn invalid_body_ref_unanchors_entity() {
    let mut harness = Harness::new(ResidencyConfig::default());
    let entity = harness
        .app
        .world_mut()
        .spawn((
            StableEntityId(7),
            Real,
            pos_voxels(0, 0, 0),
            BodyRef(BodyKey::INVALID),
        ))
        .id();

    harness.tick();

    let anchor = *harness
        .app
        .world()
        .entity(entity)
        .get::<ChunkAnchor>()
        .unwrap();
    assert_eq!(anchor.body, BodyKey::INVALID);
    assert!(!anchor.is_valid());
    // 无效 BodyRef 的实体不参与驻留，也不发任何变化消息。
    assert!(harness
        .table()
        .requests_of(RequestSource::Agent(StableEntityId(7)))
        .is_empty());
    assert!(harness.take_changes().is_empty());
}

#[test]
fn body_ref_change_reanchors() {
    let mut harness = Harness::new(ResidencyConfig::default());
    let entity = harness
        .app
        .world_mut()
        .spawn((
            StableEntityId(7),
            Real,
            pos_voxels(0, 0, 0),
            BodyRef(body(99)),
        ))
        .id();

    harness.tick();
    let anchor = *harness
        .app
        .world()
        .entity(entity)
        .get::<ChunkAnchor>()
        .unwrap();
    assert_eq!(anchor.body, body(99));

    // 只改 BodyRef（位置不变）：Changed<BodyRef> 必须触发锚点重算。
    harness
        .app
        .world_mut()
        .entity_mut(entity)
        .insert(BodyRef(body(100)));
    harness.tick();

    let anchor = *harness
        .app
        .world()
        .entity(entity)
        .get::<ChunkAnchor>()
        .unwrap();
    assert_eq!(anchor.body, body(100));
    assert_eq!(anchor.chunk, key(0, 0, 0));
    assert!(harness.table().is_requested(body(100), key(0, 0, 0)));
}

// ───────────────────────────── 请求收集 ─────────────────────────────

#[test]
fn upgrade_is_immediate_and_emits_none_to_data() {
    let mut harness = Harness::with_grace(5);
    harness.spawn_real(10, pos_voxels(0, 0, 0));

    harness.tick();

    assert_eq!(
        harness.table().target(body(10), key(0, 0, 0)),
        Residency::Data
    );
    let changes = harness.take_changes();
    assert_eq!(
        changes,
        vec![ResidencyChanged {
            body: body(10),
            key: key(0, 0, 0),
            from: Residency::None,
            to: Residency::Data,
        }]
    );
}

#[test]
fn same_source_same_chunk_counts_once() {
    let mut harness = Harness::with_grace(5);
    let id = 11;
    harness.app.world_mut().spawn((
        StableEntityId(id),
        Real,
        pos_voxels(0, 0, 0),
        WorkSites(vec![(body(id), key(0, 0, 0))]),
    ));

    harness.tick();

    // 锚点块与 WorkSites 指向同一块，只记一条，也只发一条消息。
    assert_eq!(
        harness
            .table()
            .slot(body(id), key(0, 0, 0))
            .unwrap()
            .requests()
            .len(),
        1
    );
    assert_eq!(harness.take_changes().len(), 1);
}

#[test]
fn work_sites_and_corridor_are_requested() {
    let mut harness = Harness::with_grace(5);
    let id = 12;
    harness.app.world_mut().spawn((
        StableEntityId(id),
        Real,
        pos_voxels(0, 0, 0),
        WorkSites(vec![(body(id), key(4, 0, 0))]),
        PathCorridor(vec![(body(id), key(0, 0, 5))]),
    ));

    harness.tick();

    let requested = harness
        .table()
        .requests_of(RequestSource::Agent(StableEntityId(id)));
    assert_eq!(
        requested,
        &[
            (body(id), key(0, 0, 0)),
            (body(id), key(0, 0, 5)),
            (body(id), key(4, 0, 0)),
        ]
    );
}

#[test]
fn removed_work_sites_rebuilds_agent_requests() {
    let mut harness = Harness::with_grace(5);
    let id = 13;
    let entity = harness
        .app
        .world_mut()
        .spawn((
            StableEntityId(id),
            Real,
            pos_voxels(0, 0, 0),
            WorkSites(vec![(body(id), key(7, 0, 0))]),
        ))
        .id();

    harness.tick();
    assert_eq!(
        harness
            .table()
            .requests_of(RequestSource::Agent(StableEntityId(id)))
            .len(),
        2
    );

    harness
        .app
        .world_mut()
        .entity_mut(entity)
        .remove::<WorkSites>();
    harness.flush();
    harness.tick();

    assert_eq!(
        harness
            .table()
            .requests_of(RequestSource::Agent(StableEntityId(id))),
        &[(body(id), key(0, 0, 0))]
    );
}

#[test]
fn observer_radius_requests_render_level_in_key_order() {
    let mut harness = Harness::with_grace(5);
    harness.spawn_observer(7, pos_voxels(0, 0, 0), 1);

    harness.tick();

    let requested = harness
        .table()
        .requests_of(RequestSource::Observer(StableEntityId(7)));
    // 半径 1 的三维球 = 中心 + 6 邻。
    assert_eq!(requested.len(), 7);
    for &(requested_body, requested_key) in requested {
        assert_eq!(requested_body, body(7));
        assert_eq!(
            harness.table().target(requested_body, requested_key),
            Residency::Render
        );
    }

    let changes = harness.take_changes();
    assert_eq!(changes.len(), 7);
    let keys: Vec<ChunkKey> = changes.iter().map(|change| change.key).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
    for change in &changes {
        assert_eq!(change.body, body(7));
        assert_eq!(change.from, Residency::None);
        assert_eq!(change.to, Residency::Render);
    }
}

#[test]
fn emit_sequence_is_byte_identical_under_unordered_spawn() {
    fn fixture(order: &[u64]) -> (Vec<ResidencyChanged>, String) {
        let mut harness = Harness::with_grace(5);
        for &id in order {
            let x = i64::try_from(id).unwrap() * 40;
            harness.spawn_real(id, pos_voxels(x, 0, 0));
            harness.spawn_observer(id + 100, pos_voxels(x + 1, 0, 0), 1);
        }
        harness.tick();
        let changes = harness.take_changes();
        let bytes = format!("{changes:?}");
        (changes, bytes)
    }

    let ascending = fixture(&[3, 5, 9, 11]);
    let shuffled = fixture(&[11, 9, 5, 3]);
    let interleaved = fixture(&[9, 3, 11, 5]);

    assert_eq!(ascending.0, shuffled.0);
    assert_eq!(ascending.1, shuffled.1);
    assert_eq!(ascending.0, interleaved.0);
    assert_eq!(ascending.1, interleaved.1);
    assert!(!ascending.0.is_empty());
}

// ───────────────────────────── 来源清理 ─────────────────────────────

#[test]
fn removing_real_clears_requests() {
    let mut harness = Harness::with_grace(5);
    let id = 20;
    let entity = harness.spawn_real(id, pos_voxels(0, 0, 0));

    harness.tick();
    assert!(!harness
        .table()
        .requests_of(RequestSource::Agent(StableEntityId(id)))
        .is_empty());

    harness.app.world_mut().entity_mut(entity).remove::<Real>();
    harness.flush();

    // observer 立刻按 by_source 清理；此时实体还在，StableEntityId 读得到。
    assert!(harness
        .table()
        .requests_of(RequestSource::Agent(StableEntityId(id)))
        .is_empty());
    assert!(!harness.table().is_requested(body(id), key(0, 0, 0)));

    // 防抖期内档位仍是 Data。
    harness.tick();
    assert_eq!(
        harness.table().target(body(id), key(0, 0, 0)),
        Residency::Data
    );
}

#[test]
fn despawning_real_clears_requests() {
    let mut harness = Harness::with_grace(5);
    let id = 21;
    let entity = harness.spawn_real(id, pos_voxels(0, 0, 0));

    harness.tick();
    harness.app.world_mut().entity_mut(entity).despawn();
    harness.flush();

    assert!(harness
        .table()
        .requests_of(RequestSource::Agent(StableEntityId(id)))
        .is_empty());
}

#[test]
fn despawning_observer_clears_render_requests() {
    let mut harness = Harness::with_grace(5);
    let id = 22;
    let entity = harness.spawn_observer(id, pos_voxels(0, 0, 0), 1);

    harness.tick();
    assert_eq!(
        harness
            .table()
            .requests_of(RequestSource::Observer(StableEntityId(id)))
            .len(),
        7
    );

    harness.app.world_mut().entity_mut(entity).despawn();
    harness.flush();

    assert!(harness
        .table()
        .requests_of(RequestSource::Observer(StableEntityId(id)))
        .is_empty());
}

// ───────────────────────────── 门控 ─────────────────────────────

#[test]
fn gate_lags_one_tick_behind_chunk_reached() {
    let mut harness = Harness::with_grace(10);
    let id = 30;
    let entity = harness.spawn_real(id, pos_voxels(0, 0, 0));

    harness.tick();
    assert_eq!(
        harness.table().target(body(id), key(0, 0, 0)),
        Residency::Data
    );
    assert!(
        harness.awaiting(entity),
        "请求刚出现、流式还没汇报时必须挂起"
    );
    assert_eq!(harness.stats().awaiting_entities, 1);

    // 没有 ChunkReached：一直挂着。
    harness.tick();
    assert!(harness.awaiting(entity));

    // 流式汇报到达；下一 tick 的 Gate 才摘掉（消息在 Collect 里才被读）。
    harness.send_reached(body(id), key(0, 0, 0), Residency::Data);
    harness.tick();
    assert!(!harness.awaiting(entity));
    assert_eq!(harness.stats().awaiting_entities, 0);
}

#[test]
fn gate_waits_for_work_site_and_corridor() {
    let mut harness = Harness::with_grace(10);
    let id = 31;
    let entity = harness
        .app
        .world_mut()
        .spawn((
            StableEntityId(id),
            Real,
            pos_voxels(0, 0, 0),
            WorkSites(vec![(body(id), key(1, 0, 0))]),
            PathCorridor(vec![(body(id), key(0, 0, 1))]),
        ))
        .id();

    harness.tick();
    assert!(harness.awaiting(entity));

    // 只有锚点块到达还不够。
    harness.send_reached(body(id), key(0, 0, 0), Residency::Data);
    harness.tick();
    assert!(harness.awaiting(entity));

    harness.send_reached(body(id), key(1, 0, 0), Residency::Data);
    harness.tick();
    assert!(harness.awaiting(entity));

    harness.send_reached(body(id), key(0, 0, 1), Residency::Data);
    harness.tick();
    assert!(!harness.awaiting(entity));
}

// ───────────────────────────── 降级防抖 ─────────────────────────────

#[test]
fn demote_waits_for_grace_ticks() {
    let mut harness = Harness::with_grace(2);
    let id = 40;
    let entity = harness.spawn_real(id, pos_voxels(0, 0, 0));

    harness.tick();
    harness.take_changes();
    assert_eq!(
        harness.table().target(body(id), key(0, 0, 0)),
        Residency::Data
    );

    harness.app.world_mut().entity_mut(entity).remove::<Real>();
    harness.flush();

    // 第 1 tick：还没到 grace，不降。
    harness.tick();
    assert_eq!(
        harness.table().target(body(id), key(0, 0, 0)),
        Residency::Data
    );
    assert!(harness.take_changes().is_empty());

    // 第 2 tick：到点，降一档（Data -> None）。
    harness.tick();
    assert_eq!(
        harness.table().target(body(id), key(0, 0, 0)),
        Residency::None
    );
    assert_eq!(
        harness.take_changes(),
        vec![ResidencyChanged {
            body: body(id),
            key: key(0, 0, 0),
            from: Residency::Data,
            to: Residency::None,
        }]
    );
}

#[test]
fn demote_steps_one_level_per_grace() {
    let mut harness = Harness::with_grace(1);
    let id = 50;
    let entity = harness.spawn_observer(id, pos_voxels(0, 0, 0), 0);

    harness.tick();
    harness.take_changes();
    assert_eq!(
        harness.table().target(body(id), key(0, 0, 0)),
        Residency::Render
    );

    harness
        .app
        .world_mut()
        .entity_mut(entity)
        .remove::<ObserverTag>();
    harness.flush();

    harness.tick();
    assert_eq!(
        harness.table().target(body(id), key(0, 0, 0)),
        Residency::Data
    );
    assert_eq!(
        harness.take_changes(),
        vec![ResidencyChanged {
            body: body(id),
            key: key(0, 0, 0),
            from: Residency::Render,
            to: Residency::Data,
        }]
    );

    harness.tick();
    assert_eq!(
        harness.table().target(body(id), key(0, 0, 0)),
        Residency::None
    );
    assert_eq!(
        harness.take_changes(),
        vec![ResidencyChanged {
            body: body(id),
            key: key(0, 0, 0),
            from: Residency::Data,
            to: Residency::None,
        }]
    );
}

#[test]
fn request_after_grace_deadline_cancels_demotion() {
    let mut harness = Harness::with_grace(3);
    let id = 41;
    let entity = harness.spawn_real(id, pos_voxels(0, 0, 0));

    harness.tick();
    harness.app.world_mut().entity_mut(entity).remove::<Real>();
    harness.flush();

    harness.ticks(2);
    assert_eq!(
        harness.table().target(body(id), key(0, 0, 0)),
        Residency::Data
    );

    // 防抖期内又出现请求：取消降级。
    harness.app.world_mut().entity_mut(entity).insert(Real);
    harness.flush();
    harness.ticks(3);

    assert_eq!(
        harness.table().target(body(id), key(0, 0, 0)),
        Residency::Data
    );
    assert!(harness.table().is_requested(body(id), key(0, 0, 0)));
}

// ───────────────────────────── 钉住与未存档编辑 ─────────────────────────────

#[test]
fn pin_namespaces_do_not_collide() {
    let mut harness = Harness::with_grace(2);
    {
        let mut pins = harness.app.world_mut().resource_mut::<PinRequests>();
        pins.pin(1, 7, body(101), key(1, 0, 0), Residency::Data);
        pins.pin(2, 7, body(101), key(2, 0, 0), Residency::Render);
    }

    harness.tick();

    assert_eq!(
        harness.table().target(body(101), key(1, 0, 0)),
        Residency::Data
    );
    assert_eq!(
        harness.table().target(body(101), key(2, 0, 0)),
        Residency::Render
    );
    {
        let pins = harness.app.world().resource::<PinRequests>();
        assert_eq!(
            pins.pinned(1, 7, body(101), key(1, 0, 0)),
            Some(Residency::Data)
        );
        assert_eq!(
            pins.pinned(2, 7, body(101), key(2, 0, 0)),
            Some(Residency::Render)
        );
        assert_eq!(pins.source_len(1, 7), 1);
        assert_eq!(pins.source_len(2, 7), 1);
    }

    // 撤销 namespace 1 只影响它自己。
    harness
        .app
        .world_mut()
        .resource_mut::<PinRequests>()
        .unpin_all(1, 7);
    harness.tick();
    assert!(harness
        .table()
        .requests_of(RequestSource::System {
            namespace: 1,
            id: 7
        })
        .is_empty());
    assert_eq!(
        harness.table().target(body(101), key(2, 0, 0)),
        Residency::Render
    );

    // 防抖到期后 namespace 1 的块降回 None。
    harness.tick();
    harness.tick();
    assert_eq!(
        harness.table().target(body(101), key(1, 0, 0)),
        Residency::None
    );
    assert_eq!(
        harness.table().target(body(101), key(2, 0, 0)),
        Residency::Render
    );
}

#[test]
fn unsaved_edit_mark_and_clear_hold_data_level() {
    let mut harness = Harness::with_grace(0);
    harness.app.world_mut().write_message(UnsavedEditMark {
        body: body(102),
        key: key(3, 0, 0),
    });

    harness.tick();
    assert_eq!(
        harness.table().target(body(102), key(3, 0, 0)),
        Residency::Data
    );
    assert_eq!(
        harness.table().requests_of(RequestSource::UnsavedEdit),
        &[(body(102), key(3, 0, 0))]
    );

    harness.app.world_mut().write_message(UnsavedEditClear {
        body: body(102),
        key: key(3, 0, 0),
    });
    harness.tick();

    assert!(harness
        .table()
        .requests_of(RequestSource::UnsavedEdit)
        .is_empty());
    assert_eq!(
        harness.table().target(body(102), key(3, 0, 0)),
        Residency::None
    );
    assert_eq!(
        harness.take_changes(),
        vec![
            ResidencyChanged {
                body: body(102),
                key: key(3, 0, 0),
                from: Residency::None,
                to: Residency::Data,
            },
            ResidencyChanged {
                body: body(102),
                key: key(3, 0, 0),
                from: Residency::Data,
                to: Residency::None,
            },
        ]
    );
}

#[test]
fn stats_track_target_levels() {
    let mut harness = Harness::with_grace(5);
    harness.spawn_real(60, pos_voxels(0, 0, 0));
    harness.spawn_observer(61, pos_voxels(320, 0, 0), 0);

    harness.tick();

    let stats = harness.stats();
    assert_eq!(stats.data_chunks, 1);
    assert_eq!(stats.render_chunks, 1);
    assert_eq!(stats.none_chunks, 0);
    assert_eq!(stats.awaiting_entities, 1);
    assert_eq!(stats.deferred_upgrades, 0);
}
#[test]
fn multiple_sources_take_max_and_are_independent() {
    let mut harness = Harness::with_grace(0);
    {
        let mut pins = harness.app.world_mut().resource_mut::<PinRequests>();
        pins.pin(1, 1, body(103), key(0, 0, 0), Residency::Data);
        pins.pin(2, 1, body(103), key(0, 0, 0), Residency::Render);
    }

    harness.tick();
    assert_eq!(
        harness.table().target(body(103), key(0, 0, 0)),
        Residency::Render
    );
    assert_eq!(
        harness
            .table()
            .slot(body(103), key(0, 0, 0))
            .unwrap()
            .requests()
            .len(),
        2
    );

    // 撤销 Render 来源后，Data 来源仍然把块钉在 Data。
    harness
        .app
        .world_mut()
        .resource_mut::<PinRequests>()
        .unpin_all(2, 1);
    harness.tick();
    assert_eq!(
        harness.table().target(body(103), key(0, 0, 0)),
        Residency::Data
    );

    harness
        .app
        .world_mut()
        .resource_mut::<PinRequests>()
        .unpin_all(1, 1);
    harness.tick();
    assert_eq!(
        harness.table().target(body(103), key(0, 0, 0)),
        Residency::None
    );
}

// ───────────────────────────── 内存压力 ─────────────────────────────

#[test]
fn pressure_blocks_upgrade_and_counts() {
    let mut harness = Harness::with_soft_limit(5, Some(80));
    harness.set_pressure(90, 100);
    harness.spawn_real(70, pos_voxels(0, 0, 0));

    harness.tick();

    // 目标档位不变、没有升级消息，只把这次升级记进统计。
    assert_eq!(
        harness.table().target(body(70), key(0, 0, 0)),
        Residency::None
    );
    assert!(harness.take_changes().is_empty());
    assert_eq!(harness.stats().deferred_upgrades, 1);

    // 压力仍在：下一 tick 再记一次（槽位被重新标脏，不会卡死）。
    harness.tick();
    assert_eq!(harness.stats().deferred_upgrades, 2);
    assert_eq!(
        harness.table().target(body(70), key(0, 0, 0)),
        Residency::None
    );
}

#[test]
fn upgrade_resumes_after_pressure_clears() {
    let mut harness = Harness::with_soft_limit(5, Some(80));
    harness.set_pressure(90, 100);
    harness.spawn_real(71, pos_voxels(0, 0, 0));

    harness.tick();
    assert_eq!(
        harness.table().target(body(71), key(0, 0, 0)),
        Residency::None
    );
    assert!(harness.take_changes().is_empty());
    assert_eq!(harness.stats().deferred_upgrades, 1);

    // 压力消失：被扣下的升级在下一 tick 自动兑现。
    harness.set_pressure(10, 100);
    harness.tick();

    assert_eq!(
        harness.table().target(body(71), key(0, 0, 0)),
        Residency::Data
    );
    assert_eq!(
        harness.take_changes(),
        vec![ResidencyChanged {
            body: body(71),
            key: key(0, 0, 0),
            from: Residency::None,
            to: Residency::Data,
        }]
    );
    assert_eq!(harness.stats().deferred_upgrades, 1, "兑现本身不再计数");
}

#[test]
fn demotion_is_not_blocked_by_pressure() {
    let mut harness = Harness::with_soft_limit(0, Some(80));
    {
        let mut pins = harness.app.world_mut().resource_mut::<PinRequests>();
        pins.pin(9, 3, body(74), key(0, 0, 0), Residency::Render);
    }

    // 先在无压力下升到 Render。
    harness.tick();
    assert_eq!(
        harness.table().target(body(74), key(0, 0, 0)),
        Residency::Render
    );
    assert_eq!(harness.take_changes().len(), 1);

    // 请求消失 + 压力很高 + 防抖到期（grace = 0）：仍然降级。
    harness.set_pressure(90, 100);
    harness
        .app
        .world_mut()
        .resource_mut::<PinRequests>()
        .unpin_all(9, 3);
    harness.tick();

    assert_eq!(
        harness.table().target(body(74), key(0, 0, 0)),
        Residency::Data
    );
    assert_eq!(
        harness.take_changes(),
        vec![ResidencyChanged {
            body: body(74),
            key: key(0, 0, 0),
            from: Residency::Render,
            to: Residency::Data,
        }]
    );
    assert_eq!(harness.stats().deferred_upgrades, 0, "压力不阻止降级");
}

#[test]
fn no_pressure_resource_keeps_upgrading() {
    // 上限配了但资源缺失 = 没有压力来源，照常立即升级。
    let mut harness = Harness::with_soft_limit(5, Some(1));
    harness.spawn_real(75, pos_voxels(0, 0, 0));

    harness.tick();

    assert_eq!(
        harness.table().target(body(75), key(0, 0, 0)),
        Residency::Data
    );
    assert_eq!(
        harness.take_changes(),
        vec![ResidencyChanged {
            body: body(75),
            key: key(0, 0, 0),
            from: Residency::None,
            to: Residency::Data,
        }]
    );
    assert_eq!(harness.stats().deferred_upgrades, 0);
}

#[test]
fn fill_percent_is_saturating() {
    // 默认（0 / 0）按 0 处理，不做除法。
    assert_eq!(MemoryPressure::default().fill_percent(), 0);
    assert_eq!(MemoryPressure::new(0, 0).fill_percent(), 0);
    assert!(!MemoryPressure::new(0, 0).is_over(1), "容量 0 的填充率是 0");
    assert_eq!(MemoryPressure::new(50, 100).fill_percent(), 50);
    // 向下取整：2 / 3 = 66.66% -> 66。
    assert_eq!(MemoryPressure::new(2, 3).fill_percent(), 66);
    // usize::MAX -> u64 饱和乘 100 -> 夹到 100（不会回绕成小数字）。
    assert_eq!(MemoryPressure::new(usize::MAX, 1).fill_percent(), 100);
    assert!(MemoryPressure::new(usize::MAX, 1).is_over(100));
    assert!(!MemoryPressure::new(79, 100).is_over(80));
    assert!(MemoryPressure::new(80, 100).is_over(80));
}
