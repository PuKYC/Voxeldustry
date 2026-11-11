use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use bevy::prelude::*;
use game_engine::identity::{StableEntityId, StableIdPlugin, StableIdWorldExt};
use game_engine::math::FixedPoint;
use game_engine::presentation::voxel::raw_halo_has_solid_interior;
use game_engine::residency::ChunkAnchor;
use game_engine::save::{
    export_chunk, read_region, write_world_meta, AttachTag, BodyKind as SaveBodyKind, BodyRecord,
    EntityRecord, OpaqueComponents, Persist, PersistComponent, PersistRegistry, SaveConfig,
    SavePaths, SaveStore, StableTypeId, WorldMeta,
};
use game_engine::streaming::{ChunkRecord, ChunkStore, StoreLookup};
use game_engine::voxel::{
    chunk_content_hash, chunk_key, MaxDepth, VoxOpsBulkWrite, VoxOpsRead, CHUNK_DEPTH,
};
use serde::{Deserialize, Serialize};

use super::*;
use crate::static_data::voxel::DEFAULT_VOXEL_SIZE;
use crate::voxel::{ChunkSink, ProposalSpec, WorldFeature};
use crate::world::IslandRegistry;

/// 测试世界种子：与 `WorldSeed` 资源、`generated_bytes` 保持一致。
const SEED: u64 = 0x00C0_FFEE;

/// 假存档的一条回答脚本。
#[derive(Clone, Debug)]
enum Answer {
    Miss,
    Pending,
    Hit(Option<Vec<u8>>),
    /// 完整记录（可带实体），用于验证恢复消息。
    Record(ChunkRecord),
}

/// 按调用顺序回答的假存档；脚本用尽后重复最后一条。
struct ScriptedStore {
    state: Mutex<ScriptedState>,
}

struct ScriptedState {
    cursor: usize,
    answers: Vec<Answer>,
}

impl ScriptedStore {
    fn new(answers: Vec<Answer>) -> Self {
        Self {
            state: Mutex::new(ScriptedState { cursor: 0, answers }),
        }
    }
}

impl ChunkStore for ScriptedStore {
    fn lookup(&self, _body: BodyKey, _key: ChunkKey) -> StoreLookup {
        let mut state = self.state.lock().expect("scripted store 被毒化");
        if state.answers.is_empty() {
            return StoreLookup::Miss;
        }
        let index = state.cursor.min(state.answers.len() - 1);
        state.cursor += 1;
        match &state.answers[index] {
            Answer::Miss => StoreLookup::Miss,
            Answer::Pending => StoreLookup::Pending,
            Answer::Hit(content) => StoreLookup::Hit(ChunkRecord {
                generator_version: 1,
                content: content.clone(),
                proposals: Vec::new(),
                entities: Vec::new(),
            }),
            Answer::Record(record) => StoreLookup::Hit(record.clone()),
        }
    }
}

/// 最小流式世界：一个带 `VoxVolume` 的稳定实体 + 假存档。
struct Harness {
    app: App,
    entity: Entity,
    body: BodyKey,
}

fn harness(answers: Vec<Answer>) -> Harness {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.add_plugins(StableIdPlugin);
    app.insert_resource(WorldSeed(SEED));
    // 生产装配由 ResidencyPlugin 注册这两条消息；本插件刻意只注册自己的
    // ChunkWillUnload，所以测试里显式补齐。
    app.add_message::<ResidencyChanged>();
    app.add_message::<ChunkReached>();
    app.add_plugins(crate::voxel::GameVoxelPlugin);
    app.add_plugins(VoxelStreamPlugin);
    app.insert_resource(ChunkPersistence(Box::new(ScriptedStore::new(answers))));
    // 只用 run_schedule 的测试也要 finish：Avian 在 Plugin::finish 注册诊断资源。
    app.finish();
    let (entity, id) = app
        .world_mut()
        .spawn_stable(VoxVolume::with_capacity(DEFAULT_VOXEL_SIZE, 4));
    Harness {
        app,
        entity,
        body: BodyKey::new(id),
    }
}

/// 独立生成一份「存档内容」：同样的种子/生物群系 + 导出字节 + 内容哈希。
fn generated_bytes(key: ChunkKey) -> (Vec<u8>, u64) {
    let mut interner = VoxelInterner::new(4 * 1024 * 1024);
    let mut volume = VoxVolume::with_capacity(DEFAULT_VOXEL_SIZE, 1);
    let tree = WorldGenerator::new(WorldSeed(SEED), DEFAULT_BIOME)
        .generate_chunk(interner.inner_mut(), key);
    volume.insert_chunk(key, tree);
    let bytes = export_chunk(&volume, key, interner.inner()).expect("生成的块必须能导出");
    let hash = chunk_content_hash(
        interner.inner(),
        volume.get_chunk(&key).expect("块在容器里"),
    );
    (bytes, hash)
}

/// 一份「只含实心块」的存档内容：value 不同内容就不同，与地形生成器的
/// 产出也一定不同——用来一眼区分「装进来的是注入内容」还是「走了生成器」。
///
/// 用独立 interner 构造并导出，拿到的是自包含字节；解出来装进 harness 的
/// interner 后，内容哈希与这里算的一致（哈希只看体素，不看块坐标）。
fn solid_bytes(value: u8) -> (Vec<u8>, u64) {
    let key = chunk_key(0, 0, 0);
    let mut interner = VoxelInterner::new(4 * 1024 * 1024);
    let mut volume = VoxVolume::with_capacity(DEFAULT_VOXEL_SIZE, 1);
    let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
    tree.fill(interner.inner_mut(), value);
    volume.insert_chunk(key, tree);
    let bytes = export_chunk(&volume, key, interner.inner()).expect("实心块必须能导出");
    let hash = chunk_content_hash(
        interner.inner(),
        volume.get_chunk(&key).expect("块在容器里"),
    );
    (bytes, hash)
}

impl Harness {
    fn tick(&mut self) {
        self.app.world_mut().run_schedule(FixedUpdate);
    }

    /// 跑两个 tick：第一个只推进（提案 + 暂存），第二个才裁决落体。
    fn settle_all(&mut self) {
        self.tick();
        self.tick();
    }

    fn upgrade(&mut self, key: ChunkKey) {
        self.app.world_mut().write_message(ResidencyChanged {
            body: self.body,
            key,
            from: Residency::None,
            to: Residency::Data,
        });
    }

    fn demote(&mut self, key: ChunkKey) {
        self.app.world_mut().write_message(ResidencyChanged {
            body: self.body,
            key,
            from: Residency::Data,
            to: Residency::None,
        });
    }

    /// 模拟玩家/系统真的写过该块：生产路径由 `set_voxel_for` 标记。
    /// 没标过的块在卸载/存档时只有墓碑语义，内容交给生成器重算。
    fn mark_edited(&mut self, key: ChunkKey) {
        self.app
            .world_mut()
            .resource_mut::<VoxelDirtySet>()
            .mark_edited_for(Some(self.body), key, VoxelBox::FULL);
    }

    fn reached(&mut self) -> Vec<ChunkReached> {
        self.app
            .world_mut()
            .resource_mut::<Messages<ChunkReached>>()
            .drain()
            .collect()
    }

    fn will_unload(&mut self) -> Vec<ChunkWillUnload> {
        self.app
            .world_mut()
            .resource_mut::<Messages<ChunkWillUnload>>()
            .drain()
            .collect()
    }

    fn chunk_entities(&mut self) -> Vec<ChunkEntities> {
        self.app
            .world_mut()
            .resource_mut::<Messages<ChunkEntities>>()
            .drain()
            .collect()
    }

    fn stats(&self) -> &StreamStats {
        self.app.world().resource::<StreamStats>()
    }

    fn volume(&self) -> &VoxVolume {
        self.app
            .world()
            .entity(self.entity)
            .get::<VoxVolume>()
            .expect("body 实体必须带 VoxVolume")
    }

    fn alive_nodes(&self) -> usize {
        self.app
            .world()
            .resource::<VoxelInterner>()
            .inner()
            .alive_nodes()
    }

    fn content_hash(&self, key: &ChunkKey) -> u64 {
        chunk_content_hash(
            self.app.world().resource::<VoxelInterner>().inner(),
            self.volume().get_chunk(key).expect("块必须已装进容器"),
        )
    }

    /// 一条任意 `from -> to` 的驻留变化（Render 档测试用）。
    fn request(&mut self, key: ChunkKey, from: Residency, to: Residency) {
        self.app.world_mut().write_message(ResidencyChanged {
            body: self.body,
            key,
            from,
            to,
        });
    }

    fn upgrade_to_render(&mut self, key: ChunkKey) {
        self.request(key, Residency::None, Residency::Render);
    }

    fn render_groups(&self) -> &RenderGroups {
        self.app.world().resource::<RenderGroups>()
    }

    /// 世界里的分组实体（按实体 id 升序，断言与迭代顺序无关）。
    fn group_entities(&mut self) -> Vec<Entity> {
        let mut query = self
            .app
            .world_mut()
            .query_filtered::<Entity, With<VoxChunkGroupRaw>>();
        let world = self.app.world();
        let mut out: Vec<Entity> = query.iter(world).collect();
        out.sort();
        out
    }

    fn group_payload(&self, entity: Entity) -> VoxChunkGroupRaw {
        self.app
            .world()
            .entity(entity)
            .get::<VoxChunkGroupRaw>()
            .expect("分组实体必须带 VoxChunkGroupRaw")
            .clone()
    }

    fn render_messages(&mut self) -> Vec<ChunkReached> {
        self.reached()
            .into_iter()
            .filter(|message| message.level == Residency::Render)
            .collect()
    }
}

/// 测试用临时目录：仓库没有 tempfile 依赖，用进程 id + 纳秒 + 序号拼唯一
/// 目录名；TempDir 的 Drop 里整目录清理（测试 panic 也会走到）。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("系统时钟早于 UNIX_EPOCH")
            .as_nanos();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "game-core-stream-{}-{nanos}-{seq}-{label}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("创建临时目录");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// 测试用可存档组件：注册后它的负载会进 EntityRecord。
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct TestMarker(u32);

impl PersistComponent for TestMarker {
    const STABLE_TYPE_ID: StableTypeId = StableTypeId::new(11);
}

/// 给 harness 装一套真实存档：临时目录 root + 注册了 TestMarker 的注册表。
///
/// 不装 SaveStore 的测试覆盖 headless 路径（跳过写盘、照常释放）。
fn attach_save_store(harness: &mut Harness, root: &Path) {
    let mut registry = PersistRegistry::new();
    registry.register::<TestMarker>().expect("注册测试组件");
    let world = harness.app.world_mut();
    world.insert_resource(registry);
    world.insert_resource(OpaqueComponents::default());
    world.insert_resource(SaveStore::new(SaveConfig {
        root: root.to_path_buf(),
        write_budget_per_tick: 4,
        ..SaveConfig::default()
    }));
}

/// 把内存缓冲里的记录交给后台线程并轮询到落盘完成。
fn flush_store(harness: &Harness) {
    let store = harness.app.world().resource::<SaveStore>();
    store.flush();
    for _ in 0..1000 {
        store.poll();
        let stats = store.stats();
        if stats.in_flight_writes == 0 && stats.dirty == 0 {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!("SaveStore 在 1000 次轮询内没有写完");
}

/// 真实 SaveStore 的首次查询必然先返回 Pending：空 store 也要先发一次
/// 后台读才知道是 Miss。生产里 SavePlugin 的 Poll 系统每 tick 收结果；
/// 测试没有装那个插件，所以这里手动 poll，并把 tick 推进到块真的进容器。
///
/// 旧测试假设「请求后一 tick 就生成」，接上 SaveStore 后不再成立（异步读
/// 至少多一 tick），这里统一处理，避免每个测试各写一遍轮询。
fn resolve_with_store(harness: &mut Harness, key: ChunkKey) {
    for _ in 0..1000 {
        harness.tick();
        if harness.volume().contains_chunk(&key) {
            return;
        }
        let store = harness.app.world().resource::<SaveStore>();
        store.poll();
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!("块 {key:?} 在 1000 次 tick 内没有装进容器");
}

/// 从磁盘读回该 body 的 region，再取出目标块的记录。
fn read_back(harness: &Harness, root: &Path, key: ChunkKey) -> Option<ChunkRecord> {
    let paths = SavePaths::new(root);
    let region = read_region(&paths, harness.body, key).expect("读回 region 失败");
    region.and_then(|region| region.chunks.get(&key).cloned())
}

#[test]
fn hit_with_content_installs_chunk_and_reports_reached() {
    let key = chunk_key(1, 0, 2);
    let (bytes, expected) = generated_bytes(key);
    let mut harness = harness(vec![Answer::Hit(Some(bytes))]);

    harness.upgrade(key);
    harness.tick();

    let reached = harness.reached();
    assert_eq!(
        reached,
        vec![ChunkReached {
            body: harness.body,
            key,
            level: Residency::Data,
        }]
    );
    assert!(harness.volume().contains_chunk(&key));
    assert_eq!(harness.content_hash(&key), expected);
    assert_eq!(harness.stats().loaded_from_store, 1);
    assert_eq!(harness.stats().generated, 0);
    assert_eq!(harness.stats().queued, 0);
    assert_eq!(harness.stats().pending, 0);
}

#[test]
fn miss_generates_chunk_and_reports_reached() {
    let key = chunk_key(0, 0, 1);
    let (_, expected) = generated_bytes(key);
    let mut harness = harness(vec![Answer::Miss]);

    harness.upgrade(key);
    harness.settle_all();

    let reached = harness.reached();
    assert_eq!(reached.len(), 1, "两段流水线只允许一条到达消息");
    assert_eq!(reached[0].key, key);
    assert_eq!(reached[0].level, Residency::Data);
    // 与直接调用生成器的哈希逐位一致：走的是同一条确定性生成路径。
    assert_eq!(harness.content_hash(&key), expected);
    assert_eq!(harness.stats().generated, 1);
    assert_eq!(harness.stats().loaded_from_store, 0);
    assert_eq!(harness.stats().queued, 0);
}

#[test]
fn pending_keeps_request_then_hit() {
    let key = chunk_key(2, 0, 0);
    let (bytes, expected) = generated_bytes(key);
    let mut harness = harness(vec![
        Answer::Pending,
        Answer::Pending,
        Answer::Hit(Some(bytes)),
    ]);

    harness.upgrade(key);
    for _ in 0..2 {
        harness.tick();
        assert!(harness.reached().is_empty());
        assert!(!harness.volume().contains_chunk(&key));
        assert_eq!(harness.stats().queued, 1);
        assert_eq!(harness.stats().pending, 1);
    }

    harness.tick();
    assert_eq!(harness.reached().len(), 1);
    assert!(harness.volume().contains_chunk(&key));
    assert_eq!(harness.content_hash(&key), expected);
    assert_eq!(harness.stats().queued, 0);
    assert_eq!(harness.stats().pending, 0);

    // 队列已空：后续 tick 不再查存档、不再发消息、不再重装。
    harness.tick();
    assert!(harness.reached().is_empty());
    assert_eq!(harness.stats().loaded_from_store, 1);
}

#[test]
fn existing_chunk_is_not_regenerated() {
    let key = chunk_key(3, 0, 3);
    let (_, expected) = generated_bytes(key);
    let mut harness = harness(vec![Answer::Miss]);

    // 模拟「地形 Startup 已经把这块生成进容器」：树直接进 VoxVolume，
    // 内容态是 Undetermined（有数据、未定论）。
    let tree = {
        let world = harness.app.world_mut();
        let mut interner = world.resource_mut::<VoxelInterner>();
        WorldGenerator::new(WorldSeed(SEED), DEFAULT_BIOME)
            .generate_chunk(interner.inner_mut(), key)
    };
    harness
        .app
        .world_mut()
        .entity_mut(harness.entity)
        .get_mut::<VoxVolume>()
        .expect("body 实体必须带 VoxVolume")
        .insert_chunk(key, tree);
    harness
        .app
        .world_mut()
        .resource_mut::<GenTable>()
        .advance((harness.body, key), GenState::Undetermined)
        .expect("Unloaded -> Undetermined 是合法推进");

    harness.upgrade(key);
    harness.tick();

    let reached = harness.reached();
    assert_eq!(reached.len(), 1);
    assert_eq!(reached[0].level, Residency::Data);
    assert_eq!(harness.stats().generated, 0);
    assert_eq!(harness.stats().loaded_from_store, 0);
    assert_eq!(harness.stats().queued, 0);
    assert_eq!(harness.content_hash(&key), expected);
}

#[test]
fn unload_emits_will_unload_and_frees_nodes() {
    let key = chunk_key(4, 0, 4);
    let (bytes, _) = generated_bytes(key);
    let mut harness = harness(vec![Answer::Hit(Some(bytes))]);
    let baseline = harness.alive_nodes();

    harness.upgrade(key);
    harness.tick();
    assert!(harness.volume().contains_chunk(&key));
    assert!(harness.alive_nodes() > baseline);

    harness.demote(key);
    harness.tick();

    let unloads = harness.will_unload();
    assert_eq!(
        unloads,
        vec![ChunkWillUnload {
            body: harness.body,
            key,
            from: Residency::Data,
            to: Residency::None,
        }]
    );
    assert!(!harness.volume().contains_chunk(&key));
    assert_eq!(harness.alive_nodes(), baseline);
    assert_eq!(harness.stats().unloaded, 1);

    // 重复的降档消息：不二次释放、不二次计数、不再发 ChunkWillUnload。
    harness.demote(key);
    harness.tick();
    assert!(harness.will_unload().is_empty());
    assert_eq!(harness.alive_nodes(), baseline);
    assert_eq!(harness.stats().unloaded, 1);
}

#[test]
fn streaming_reports_memory_pressure() {
    let key = chunk_key(7, 0, 7);
    let (bytes, _) = generated_bytes(key);
    let mut harness = harness(vec![Answer::Hit(Some(bytes))]);

    // 每 tick 覆盖写一份快照：容量来自 interner，必须 > 0。
    harness.tick();
    let empty = *harness.app.world().resource::<MemoryPressure>();
    assert!(empty.capacity_nodes > 0, "容量必须来自 interner：{empty:?}");
    assert_eq!(empty.capacity_nodes as usize, {
        harness.app.world().resource::<VoxelInterner>().capacity()
    });

    harness.upgrade(key);
    harness.tick();
    assert!(harness.volume().contains_chunk(&key));
    // 报告系统排在装载之前，所以装载结果要下一 tick 才写进压力快照。
    harness.tick();

    let loaded = *harness.app.world().resource::<MemoryPressure>();
    assert_eq!(loaded.capacity_nodes, empty.capacity_nodes);
    assert!(loaded.used_nodes > 0, "装载后已用节点必须 > 0：{loaded:?}");
    assert!(
        loaded.used_nodes <= loaded.capacity_nodes,
        "已用不应超过容量：{loaded:?}"
    );
    assert!(loaded.fill_percent() <= 100, "填充率是百分比：{loaded:?}");
}

#[test]
fn budget_limits_resolves_per_tick() {
    let first = chunk_key(5, 0, 5);
    let second = chunk_key(6, 0, 5);
    let third = chunk_key(7, 0, 5);
    let mut harness = harness(vec![Answer::Miss]);
    harness
        .app
        .world_mut()
        .resource_mut::<VoxelStreamConfig>()
        .max_resolve_per_tick = 1;

    harness.upgrade(first);
    harness.upgrade(second);
    harness.upgrade(third);

    // 预算同时管推进与结算：第一个 tick 只能暂存一个块，结算从下一 tick 开始。
    harness.tick();
    assert!(harness.reached().is_empty(), "第一 tick 只提议案，不落体");
    assert_eq!(harness.stats().proposed, 1, "推进预算按块计");
    assert_eq!(harness.stats().staged, 1);

    for expected in [first, second, third] {
        harness.tick();
        let reached = harness.reached();
        assert_eq!(reached.len(), 1, "每 tick 恰好结算一个块");
        assert_eq!(reached[0].key, expected, "按队列键序结算");
    }

    assert_eq!(harness.stats().queued, 0);
    assert_eq!(harness.stats().generated, 3);
    assert_eq!(harness.stats().proposed, 3);
    assert_eq!(harness.stats().staged, 0);
    assert!(harness.volume().contains_chunk(&first));
    assert!(harness.volume().contains_chunk(&second));
    assert!(harness.volume().contains_chunk(&third));
}

#[test]
fn upgrade_after_unload_regenerates_identically() {
    let key = chunk_key(8, 0, 8);
    let (_, expected) = generated_bytes(key);
    let mut harness = harness(vec![Answer::Miss]);

    harness.upgrade(key);
    harness.settle_all();
    let first_hash = harness.content_hash(&key);
    assert_eq!(first_hash, expected);

    harness.demote(key);
    harness.tick();
    assert!(!harness.volume().contains_chunk(&key));

    harness.upgrade(key);
    harness.settle_all();
    assert_eq!(harness.content_hash(&key), first_hash);
    assert_eq!(harness.stats().generated, 2);
    assert_eq!(harness.stats().unloaded, 1);
}

// ---- Render 档：分组实体的产出 / 幂等 / 降档 / 重载 ----

#[test]
fn render_request_creates_group_entity_and_reports_render() {
    let key = chunk_key(10, 0, 1);
    let (bytes, expected) = generated_bytes(key);
    let mut harness = harness(vec![Answer::Hit(Some(bytes))]);

    harness.upgrade_to_render(key);
    harness.tick();

    assert_eq!(
        harness.reached(),
        vec![
            ChunkReached {
                body: harness.body,
                key,
                level: Residency::Data,
            },
            ChunkReached {
                body: harness.body,
                key,
                level: Residency::Render,
            },
        ],
        "同一块必须先发 Data、再发 Render"
    );
    assert_eq!(
        harness.content_hash(&key),
        expected,
        "Data 档必须真的装上存档内容"
    );

    let entities = harness.group_entities();
    assert_eq!(entities.len(), 1, "必须恰好产出一个分组实体");
    let raw = harness.group_payload(entities[0]);
    assert_eq!(raw.lod, 0);
    assert_eq!(raw.chunks.len(), 1, "单块单组：分组里只能有该块");
    let (payload, mask) = raw.chunks.get(&key).expect("分组必须恰含该 key");
    assert_eq!(payload.len(), RAW_VOXELS);
    assert!(
        raw_halo_has_solid_interior(payload),
        "生成的块本来就该是实心地形"
    );
    assert_eq!(*mask, 0, "自封边、无跨 LOD 裙边，掩码为 0");
    assert_eq!(harness.stats().render_groups, 1);
}

/// 全空气块到达 Render 档时也要有分组实体：载荷是一份全空气 34³
/// （渲染 0 个矩形），并挂 [RenderGroupBody] 标出所属 body。这样在空块里
/// 放第一块方块时可以直接就地刷新，不必等块重新加载。
#[test]
fn air_chunk_still_gets_a_group_entity() {
    let key = chunk_key(15, 0, 1);
    let mut harness = harness(vec![Answer::Miss]);

    // 模拟地形已经把一块空树生成进容器（内容态 Undetermined）。
    let tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
    harness
        .app
        .world_mut()
        .entity_mut(harness.entity)
        .get_mut::<VoxVolume>()
        .expect("body 实体必须带 VoxVolume")
        .insert_chunk(key, tree);
    harness
        .app
        .world_mut()
        .resource_mut::<GenTable>()
        .advance((harness.body, key), GenState::Undetermined)
        .expect("Unloaded -> Undetermined 是合法推进");

    harness.upgrade_to_render(key);
    harness.tick();

    assert_eq!(harness.stats().render_groups, 1, "空块也算一个分组");
    let entities = harness.group_entities();
    assert_eq!(entities.len(), 1, "全空气块也必须有一个分组实体");
    let entity = entities[0];

    let owner = harness
        .app
        .world()
        .entity(entity)
        .get::<RenderGroupBody>()
        .copied()
        .expect("流式分组必须带 RenderGroupBody");
    assert_eq!(owner, RenderGroupBody(harness.body), "owner 必须是该 body");

    let raw = harness.group_payload(entity);
    assert_eq!(raw.lod, 0);
    assert_eq!(raw.chunks.len(), 1, "全空气块也有一份 34³ 载荷");
    let (payload, mask) = raw.chunks.get(&key).expect("分组必须恰含该 key");
    assert_eq!(payload.len(), RAW_VOXELS);
    assert!(
        !raw_halo_has_solid_interior(payload),
        "全空气载荷不得被当成实心"
    );
    assert_eq!(*mask, 0);
    assert!(
        harness.app.world().get_entity(entity).is_ok(),
        "分组实体必须真的存在"
    );
}

#[test]
fn data_only_request_creates_no_render_entity() {
    let key = chunk_key(11, 0, 1);
    let mut harness = harness(vec![Answer::Miss]);

    harness.upgrade(key);
    harness.settle_all();

    assert_eq!(
        harness.reached(),
        vec![ChunkReached {
            body: harness.body,
            key,
            level: Residency::Data,
        }]
    );
    assert!(
        harness.group_entities().is_empty(),
        "Data 档不得 spawn 分组实体"
    );
    assert!(harness.render_groups().0.is_empty());
    assert_eq!(harness.stats().render_groups, 0);
}

#[test]
fn render_is_idempotent() {
    let key = chunk_key(12, 0, 1);
    let mut harness = harness(vec![Answer::Miss]);

    harness.upgrade_to_render(key);
    harness.settle_all();
    assert_eq!(harness.group_entities().len(), 1);
    assert_eq!(harness.render_messages().len(), 1);

    // 第二条相同的 Render 消息：既不 spawn 第二个实体，也不重发 Render。
    harness.upgrade_to_render(key);
    harness.tick();
    assert_eq!(
        harness.group_entities().len(),
        1,
        "同一块只能有一个分组实体"
    );
    assert!(
        harness.render_messages().is_empty(),
        "重复消息不得重复发 Render"
    );
    assert_eq!(harness.stats().render_groups, 1);
    assert_eq!(harness.stats().generated, 1, "幂等请求不得触发第二次生成");
}

#[test]
fn render_to_data_keeps_voxels_and_drops_entity() {
    let key = chunk_key(13, 0, 1);
    let (_, expected) = generated_bytes(key);
    let mut harness = harness(vec![Answer::Miss]);

    harness.upgrade_to_render(key);
    harness.settle_all();
    let entities = harness.group_entities();
    assert_eq!(entities.len(), 1);
    let _ = harness.reached();

    harness.request(key, Residency::Render, Residency::Data);
    harness.tick();

    assert!(harness.group_entities().is_empty(), "分组实体必须销毁");
    assert!(harness.render_groups().0.is_empty(), "表现层记录必须清掉");
    assert!(
        harness.app.world().get_entity(entities[0]).is_err(),
        "实体必须真的 despawn"
    );
    assert!(harness.volume().contains_chunk(&key), "体素内容必须保留");
    assert_eq!(harness.content_hash(&key), expected);
    assert!(harness.will_unload().is_empty(), "降渲染档不得发卸载消息");
    assert_eq!(harness.stats().unloaded, 0);
    assert_eq!(harness.stats().render_groups, 0);
    assert_eq!(harness.stats().generated, 1, "降渲染档不得重生成");
}

#[test]
fn render_to_none_despawns_and_frees() {
    let key = chunk_key(14, 0, 1);
    let mut harness = harness(vec![Answer::Miss]);
    let baseline = harness.alive_nodes();

    harness.upgrade_to_render(key);
    harness.settle_all();
    assert!(
        harness.alive_nodes() > baseline,
        "Render 档必须真的装进体素树"
    );
    let entities = harness.group_entities();
    assert_eq!(entities.len(), 1);
    let _ = harness.reached();

    harness.request(key, Residency::Render, Residency::None);
    harness.tick();

    assert!(harness.group_entities().is_empty());
    assert!(harness.app.world().get_entity(entities[0]).is_err());
    assert!(!harness.volume().contains_chunk(&key), "块必须被卸载");
    assert_eq!(harness.alive_nodes(), baseline, "节点引用必须回基线");
    assert_eq!(harness.stats().unloaded, 1);
    assert_eq!(harness.stats().render_groups, 0);
    let unloads = harness.will_unload();
    assert_eq!(unloads.len(), 1);
    assert_eq!(unloads[0].from, Residency::Render);
    assert_eq!(unloads[0].to, Residency::None);
}

#[test]
fn render_then_reload_recreates_entity() {
    let key = chunk_key(15, 0, 1);
    let mut harness = harness(vec![Answer::Miss]);

    harness.upgrade_to_render(key);
    harness.settle_all();
    let first_entity = harness.group_entities()[0];
    let first = harness
        .group_payload(first_entity)
        .chunks
        .get(&key)
        .cloned()
        .expect("分组必须恰含该 key");
    let _ = harness.reached();

    harness.request(key, Residency::Render, Residency::None);
    harness.tick();
    assert!(harness.group_entities().is_empty());

    harness.upgrade_to_render(key);
    harness.settle_all();
    let entities = harness.group_entities();
    assert_eq!(entities.len(), 1, "重新到达 Render 必须重建分组实体");
    let second = harness
        .group_payload(entities[0])
        .chunks
        .get(&key)
        .cloned()
        .expect("分组必须恰含该 key");
    assert_eq!(first, second, "重载后的载荷必须逐字节相同");
    assert_eq!(harness.stats().render_groups, 1);
    assert_eq!(harness.stats().generated, 2);
    assert_eq!(harness.stats().unloaded, 1);
    assert_eq!(harness.render_messages().len(), 1);
}

#[test]
fn render_payload_matches_direct_extraction() {
    let key = chunk_key(16, 0, 1);
    let (bytes, _) = generated_bytes(key);
    let mut harness = harness(vec![Answer::Hit(Some(bytes))]);

    harness.upgrade_to_render(key);
    harness.tick();

    let entity = harness.group_entities()[0];
    let payload = harness
        .group_payload(entity)
        .chunks
        .get(&key)
        .expect("分组必须恰含该 key")
        .0
        .clone();

    // 独立路径：同一份体素内容 + 同样的 Neighbourhood::none()。
    let mut interner = VoxelInterner::new(4 * 1024 * 1024);
    let tree = WorldGenerator::new(WorldSeed(SEED), DEFAULT_BIOME)
        .generate_chunk(interner.inner_mut(), key);
    let expected = extract_raw_halo(
        &tree,
        interner.inner(),
        MeshBlock::new(key, Lod::new(0)),
        Neighbourhood::none(),
    );

    assert_eq!(payload.len(), RAW_VOXELS);
    assert_eq!(payload, expected, "分组载荷必须与独立提取逐字节相等");
}

#[test]
fn unload_persists_chunk_content_to_store() {
    let dir = TempDir::new("unload-content");
    let key = chunk_key(20, 0, 20);
    let (expected, _) = generated_bytes(key);
    let mut harness = harness(vec![Answer::Miss]);
    attach_save_store(&mut harness, dir.path());

    harness.upgrade(key);
    resolve_with_store(&mut harness, key);
    assert!(harness.volume().contains_chunk(&key));
    harness.mark_edited(key);

    harness.demote(key);
    harness.tick();

    // 未确认落盘：树还在、无消息、不计数；记录只在内存缓冲里。
    assert!(harness.volume().contains_chunk(&key), "未确认前不得释放");
    assert!(harness.will_unload().is_empty(), "未确认前不得发卸载消息");
    assert_eq!(harness.stats().unloaded, 0);
    assert_eq!(harness.stats().pending_unloads, 1);
    assert_eq!(
        harness.app.world().resource::<SaveStore>().stats().buffered,
        1,
        "卸载必须把记录写进内存缓冲"
    );
    assert_eq!(
        read_back(&harness, dir.path(), key),
        None,
        "还在缓冲，未落盘"
    );

    flush_store(&harness);
    harness.tick();

    assert!(!harness.volume().contains_chunk(&key), "确认落盘后才释放");
    assert_eq!(harness.stats().unloaded, 1);
    assert_eq!(harness.stats().pending_unloads, 0);
    assert_eq!(harness.will_unload().len(), 1);

    let record = read_back(&harness, dir.path(), key).expect("卸载后存档里必须留下该块");
    assert_eq!(
        record.content.as_deref(),
        Some(expected.as_slice()),
        "content 必须与导出字节逐字节相等"
    );
    assert_eq!(record.generator_version, 1);
    assert!(record.entities.is_empty());
    assert!(record.proposals.is_empty());
    assert_eq!(
        harness.app.world().resource::<SaveStore>().stats().buffered,
        0
    );
}

#[test]
fn unload_persists_anchored_entities_in_same_record() {
    let dir = TempDir::new("unload-entities");
    let key = chunk_key(21, 0, 21);
    let mut harness = harness(vec![Answer::Miss]);
    attach_save_store(&mut harness, dir.path());

    let body = harness.body;
    let (_, entity_id) = harness.app.world_mut().spawn_stable((
        TestMarker(3),
        Persist {
            prototype_type_id: 7,
        },
        ChunkAnchor::new(body, key),
    ));

    harness.upgrade(key);
    resolve_with_store(&mut harness, key);
    harness.mark_edited(key);
    harness.demote(key);
    harness.tick();

    assert!(harness.volume().contains_chunk(&key), "未确认前不得释放");
    assert!(harness.will_unload().is_empty(), "未确认前不得发卸载消息");
    assert_eq!(harness.stats().unloaded, 0);
    assert_eq!(harness.stats().pending_unloads, 1);

    flush_store(&harness);
    harness.tick();

    assert!(!harness.volume().contains_chunk(&key), "确认落盘后才释放");
    assert_eq!(harness.stats().unloaded, 1);
    assert_eq!(harness.stats().pending_unloads, 0);
    assert_eq!(harness.will_unload().len(), 1);

    let record = read_back(&harness, dir.path(), key).expect("卸载后存档里必须留下该块");
    assert!(record.content.is_some(), "内容与实体必须落在同一条记录");
    assert_eq!(record.entities.len(), 1, "该块只应有一条实体记录");
    assert_eq!(record.entities[0].stable_entity_id, entity_id.0);
    assert_eq!(record.entities[0].stable_type_id, 7);
}

#[test]
fn unedited_chunk_unload_writes_no_content() {
    let dir = TempDir::new("unload-unedited");
    let key = chunk_key(32, 0, 32);
    let (_, expected_hash) = generated_bytes(key);
    let mut harness = harness(vec![Answer::Miss]);
    attach_save_store(&mut harness, dir.path());

    harness.upgrade(key);
    resolve_with_store(&mut harness, key);
    assert!(harness.volume().contains_chunk(&key));
    assert!(
        !harness
            .app
            .world()
            .resource::<VoxelDirtySet>()
            .is_edited_for(Some(harness.body), &key),
        "生成器装进来的块默认不算已编辑"
    );

    // 不标编辑：卸载时没有可写的 content，空实体 → 墓碑。
    harness.demote(key);
    harness.tick();
    flush_store(&harness);
    harness.tick();

    assert!(!harness.volume().contains_chunk(&key), "确认落盘后才释放");
    assert_eq!(harness.stats().unloaded, 1);
    let paths = SavePaths::new(dir.path());
    assert!(
        !paths.region(harness.body, key).exists(),
        "未编辑且无实体的块不得留下 region 文件"
    );
    assert_eq!(
        read_region(&paths, harness.body, key).expect("读回 region 失败"),
        None,
        "墓碑删除后 region 里不该有该块"
    );
    assert_eq!(read_back(&harness, dir.path(), key), None);

    // 再请求：内容交给生成器逐字节重算，与第一次一致。
    harness.upgrade(key);
    resolve_with_store(&mut harness, key);
    assert_eq!(harness.content_hash(&key), expected_hash);
    assert_eq!(harness.stats().generated, 2);
    assert_eq!(harness.stats().loaded_from_store, 0);
}

#[test]
fn edited_chunk_unload_keeps_content() {
    let dir = TempDir::new("unload-edited");
    let key = chunk_key(33, 0, 33);
    let (expected, expected_hash) = generated_bytes(key);
    let mut harness = harness(vec![Answer::Miss]);
    attach_save_store(&mut harness, dir.path());

    harness.upgrade(key);
    resolve_with_store(&mut harness, key);
    harness.mark_edited(key);

    harness.demote(key);
    harness.tick();
    flush_store(&harness);
    harness.tick();

    assert!(!harness.volume().contains_chunk(&key));
    let record = read_back(&harness, dir.path(), key).expect("已编辑块必须落盘");
    assert_eq!(record.content.as_deref(), Some(expected.as_slice()));
    assert!(record.entities.is_empty());

    // 重新请求：从存档装回同一份字节，不再走生成器。
    harness.upgrade(key);
    resolve_with_store(&mut harness, key);
    assert!(harness.volume().contains_chunk(&key));
    assert_eq!(harness.content_hash(&key), expected_hash);
    assert_eq!(harness.stats().loaded_from_store, 1);
    assert_eq!(harness.stats().generated, 1);
}

#[test]
fn store_hit_marks_edited_so_content_survives_next_unload() {
    let dir = TempDir::new("unload-hit-edited");
    let key = chunk_key(34, 0, 34);
    let (bytes, expected_hash) = solid_bytes(201);
    let mut harness = harness(vec![Answer::Miss]);
    attach_save_store(&mut harness, dir.path());

    harness
        .app
        .world()
        .resource::<SaveStore>()
        .write(
            harness.body,
            key,
            ChunkRecord {
                generator_version: 1,
                content: Some(bytes.clone()),
                proposals: Vec::new(),
                entities: Vec::new(),
            },
        )
        .expect("预写存档记录");
    flush_store(&harness);

    harness.upgrade(key);
    resolve_with_store(&mut harness, key);
    assert_eq!(
        harness.content_hash(&key),
        expected_hash,
        "必须装的是磁盘内容"
    );
    assert!(
        harness
            .app
            .world()
            .resource::<VoxelDirtySet>()
            .is_edited_for(Some(harness.body), &key),
        "存档命中的块必须标成已编辑"
    );

    harness.demote(key);
    harness.tick();
    flush_store(&harness);
    harness.tick();

    assert!(!harness.volume().contains_chunk(&key));
    let record = read_back(&harness, dir.path(), key).expect("记录必须仍在");
    assert_eq!(
        record.content.as_deref(),
        Some(bytes.as_slice()),
        "下一次卸载不得把存档内容当未编辑删成墓碑"
    );
    assert_eq!(
        harness
            .app
            .world()
            .resource::<SaveStore>()
            .stats()
            .tombstones,
        0,
        "不得走墓碑删除"
    );
}

#[test]
fn entity_only_record_survives_unedited_unload() {
    let dir = TempDir::new("unload-entity-only");
    let key = chunk_key(35, 0, 35);
    let mut harness = harness(vec![Answer::Miss]);
    attach_save_store(&mut harness, dir.path());

    let body = harness.body;
    let (_, entity_id) = harness.app.world_mut().spawn_stable((
        TestMarker(5),
        Persist {
            prototype_type_id: 7,
        },
        ChunkAnchor::new(body, key),
    ));

    harness.upgrade(key);
    resolve_with_store(&mut harness, key);
    // 不标编辑：内容可以丢，实体必须留。
    harness.demote(key);
    harness.tick();
    flush_store(&harness);
    harness.tick();

    assert!(!harness.volume().contains_chunk(&key));
    let record = read_back(&harness, dir.path(), key).expect("实体记录必须留在存档里");
    assert_eq!(record.content, None, "未编辑块不得写 content");
    assert_eq!(record.entities.len(), 1);
    assert_eq!(record.entities[0].stable_entity_id, entity_id.0);
    assert_eq!(record.entities[0].stable_type_id, 7);

    // 造一份最小 meta，再用读档路径证明实体确实被恢复。
    let mut meta = WorldMeta {
        seed: SEED,
        world_tick: 0,
        next_stable_entity_id: body.as_u64().max(entity_id.0) + 1,
        bodies: BTreeMap::new(),
        mods: Vec::new(),
        params: Vec::new(),
        actions: Vec::new(),
    };
    meta.bodies.insert(
        body,
        BodyRecord {
            id: body,
            kind: SaveBodyKind::Island,
            attach: AttachTag::Free,
            biome: 0,
            anchored: 0,
            voxel_size: FixedPoint::from_num(1).to_bits(),
            drift: [0; 3],
            attach_to: BodyKey::INVALID,
        },
    );
    let paths = SavePaths::new(dir.path());
    write_world_meta(&paths, &meta).expect("写 meta");

    let mut loaded = World::new();
    loaded.insert_resource(WorldSeed(SEED));
    loaded.insert_resource(IslandRegistry::new());
    loaded.insert_resource(StableIdAllocator::default());
    loaded.insert_resource(StableEntityIndex::default());
    loaded.insert_resource(VoxelInterner::new(4 * 1024 * 1024));
    loaded.insert_resource(VoxelDirtySet::default());
    let mut registry = PersistRegistry::new();
    registry.register::<TestMarker>().expect("注册测试组件");
    loaded.insert_resource(registry);
    loaded.insert_resource(OpaqueComponents::default());

    let result = crate::world::load::load_world(&mut loaded, &paths, 1)
        .expect("读档必须成功")
        .expect("meta 存在时必须返回 LoadedWorld");
    assert_eq!(result.stats.entities, 1, "实体记录必须被恢复");

    let mut query = loaded.query::<(&StableEntityId, &TestMarker)>();
    let restored: Vec<(StableEntityId, TestMarker)> = query
        .iter(&loaded)
        .map(|(id, marker)| (*id, *marker))
        .collect();
    assert_eq!(
        restored,
        vec![(entity_id, TestMarker(5))],
        "恢复的实体必须保留稳定 ID 与组件负载"
    );
}

#[test]
fn unload_without_save_store_still_releases() {
    let key = chunk_key(22, 0, 22);
    let mut harness = harness(vec![Answer::Miss]);
    let baseline = harness.alive_nodes();

    harness.upgrade(key);
    harness.settle_all();
    assert!(harness.alive_nodes() > baseline, "请求必须先真的装进体素树");

    harness.demote(key);
    harness.tick();

    assert!(!harness.volume().contains_chunk(&key), "块必须被卸载");
    assert_eq!(harness.alive_nodes(), baseline, "节点引用必须回基线");
    assert_eq!(harness.stats().unloaded, 1);
    assert!(
        harness
            .app
            .world()
            .resource::<PendingUnloads>()
            .0
            .is_empty(),
        "释放后 pending 必须清空"
    );
}

#[test]
fn duplicate_demote_writes_once_and_emits_once() {
    let dir = TempDir::new("unload-dedupe");
    let key = chunk_key(23, 0, 23);
    let mut harness = harness(vec![Answer::Miss]);
    attach_save_store(&mut harness, dir.path());

    harness.upgrade(key);
    resolve_with_store(&mut harness, key);
    harness.mark_edited(key);
    let _ = harness.reached();

    harness.demote(key);
    harness.demote(key);
    harness.tick();

    // 未确认：只登记一次、只提交一次，不发消息也不释放。
    assert!(
        harness.will_unload().is_empty(),
        "未确认落盘前不得发卸载消息"
    );
    assert_eq!(harness.stats().unloaded, 0, "未确认前不得释放");
    assert_eq!(harness.stats().pending_unloads, 1, "重复降档只许登记一次");
    assert!(harness.volume().contains_chunk(&key));
    assert_eq!(
        harness.app.world().resource::<SaveStore>().stats().buffered,
        1,
        "重复降档只许写一条记录"
    );

    // 再跑一 tick：version 已是 Some，不得重写。
    harness.tick();
    assert_eq!(
        harness.app.world().resource::<SaveStore>().stats().buffered,
        1,
        "已提交的块不得每 tick 重写"
    );

    flush_store(&harness);
    harness.tick();

    assert_eq!(
        harness.will_unload().len(),
        1,
        "确认后只许发一条 ChunkWillUnload"
    );
    assert_eq!(harness.stats().unloaded, 1, "重复降档只许释放一次");
    assert!(!harness.volume().contains_chunk(&key));

    let paths = SavePaths::new(dir.path());
    let region = read_region(&paths, harness.body, key)
        .expect("读回 region 失败")
        .expect("region 文件必须存在");
    assert_eq!(region.chunk_count(), 1, "同一块不得写出两条记录");
    assert!(region.chunks.contains_key(&key));
}

#[test]
fn empty_chunk_writes_no_record() {
    let dir = TempDir::new("unload-empty");
    let key = chunk_key(24, 0, 24);
    let mut harness = harness(vec![Answer::Pending]);
    attach_save_store(&mut harness, dir.path());

    harness.upgrade(key);
    harness.tick();
    assert!(!harness.volume().contains_chunk(&key), "Pending 不得装块");
    assert_eq!(harness.stats().pending, 1);

    harness.demote(key);
    harness.tick();

    // 空块走 delete：墓碑也要等确认落盘才释放。
    assert!(harness.will_unload().is_empty(), "未确认前不得发卸载消息");
    assert_eq!(harness.stats().unloaded, 0);
    assert_eq!(harness.stats().pending_unloads, 1);
    assert_eq!(
        harness.app.world().resource::<SaveStore>().stats().buffered,
        0,
        "无内容无实体的块不得写记录"
    );
    assert_eq!(
        harness
            .app
            .world()
            .resource::<SaveStore>()
            .stats()
            .tombstones,
        1,
        "空块必须标删除"
    );

    flush_store(&harness);
    harness.tick();

    assert_eq!(harness.stats().unloaded, 1);
    assert_eq!(harness.stats().pending_unloads, 0);
    assert_eq!(harness.will_unload().len(), 1);
    assert_eq!(
        read_back(&harness, dir.path(), key),
        None,
        "空块不得在存档里留下记录"
    );
}

#[test]
fn unload_air_chunk_deletes_existing_record() {
    let dir = TempDir::new("unload-air-delete");
    let key = chunk_key(25, 0, 25);
    // 读档走 ScriptedStore：Pending 保证卸载时块里没有树（= 空内容）。
    let mut harness = harness(vec![Answer::Pending]);
    attach_save_store(&mut harness, dir.path());

    // 预置一条磁盘旧记录，模拟上一次卸载/编辑留下的内容。
    {
        let store = harness.app.world().resource::<SaveStore>();
        store
            .write(
                harness.body,
                key,
                ChunkRecord {
                    generator_version: 1,
                    content: Some(vec![7; 4]),
                    proposals: Vec::new(),
                    entities: Vec::new(),
                },
            )
            .expect("预写旧记录");
    }
    flush_store(&harness);
    let paths = SavePaths::new(dir.path());
    assert!(
        paths.region(harness.body, key).is_file(),
        "旧记录必须先在磁盘上"
    );

    harness.upgrade(key);
    harness.tick();
    assert!(!harness.volume().contains_chunk(&key), "Pending 不得装块");

    harness.demote(key);
    harness.tick();

    // 删除只是墓碑：确认落盘前不释放、不发消息。
    assert!(harness.will_unload().is_empty(), "未确认前不得发卸载消息");
    assert_eq!(harness.stats().unloaded, 0);
    assert_eq!(harness.stats().pending_unloads, 1);
    {
        let store = harness.app.world().resource::<SaveStore>();
        assert_eq!(store.stats().buffered, 0, "空块不得写记录");
        assert_eq!(store.stats().tombstones, 1, "空块必须标删除");
        assert_eq!(store.lookup(harness.body, key), StoreLookup::Miss);
    }

    flush_store(&harness);
    harness.tick();

    assert_eq!(harness.stats().unloaded, 1);
    assert_eq!(harness.stats().pending_unloads, 0);
    assert_eq!(harness.will_unload().len(), 1);
    assert!(
        read_back(&harness, dir.path(), key).is_none(),
        "卸载空块必须删掉磁盘上的旧记录"
    );
    assert!(
        !paths.region(harness.body, key).exists(),
        "region 变空后文件必须删除"
    );
}

#[test]
fn unload_waits_for_commit_before_releasing() {
    let dir = TempDir::new("unload-wait-commit");
    let key = chunk_key(26, 0, 26);
    let mut harness = harness(vec![Answer::Miss]);
    attach_save_store(&mut harness, dir.path());

    harness.upgrade(key);
    resolve_with_store(&mut harness, key);
    assert!(harness.volume().contains_chunk(&key));
    harness.mark_edited(key);

    harness.demote(key);
    harness.tick();

    assert!(
        harness.volume().contains_chunk(&key),
        "未确认落盘前树必须留在容器里"
    );
    assert!(harness.will_unload().is_empty(), "未确认前不得发卸载消息");
    assert_eq!(harness.stats().unloaded, 0);
    assert_eq!(harness.stats().pending_unloads, 1);
    assert_eq!(
        harness.app.world().resource::<SaveStore>().stats().buffered,
        1,
        "demote 后必须已经提交给存档（进内存缓冲）"
    );
    assert_eq!(
        read_back(&harness, dir.path(), key),
        None,
        "还在缓冲，未落盘"
    );

    flush_store(&harness);
    harness.tick();

    assert!(!harness.volume().contains_chunk(&key), "确认落盘后必须释放");
    assert_eq!(
        harness.will_unload(),
        vec![ChunkWillUnload {
            body: harness.body,
            key,
            from: Residency::Data,
            to: Residency::None,
        }]
    );
    assert_eq!(harness.stats().unloaded, 1);
    assert_eq!(harness.stats().pending_unloads, 0);
    assert!(
        read_back(&harness, dir.path(), key).is_some(),
        "落盘后必须能读回内容"
    );
}

#[test]
fn upgrade_before_commit_keeps_chunk_and_cancels_unload() {
    let dir = TempDir::new("unload-revive-before-commit");
    let key = chunk_key(27, 0, 27);
    let (_, expected) = generated_bytes(key);
    let mut harness = harness(vec![Answer::Miss]);
    attach_save_store(&mut harness, dir.path());

    harness.upgrade(key);
    resolve_with_store(&mut harness, key);
    let generated_after_load = harness.stats().generated;

    harness.demote(key);
    harness.tick();
    assert_eq!(harness.stats().pending_unloads, 1);
    assert!(harness.volume().contains_chunk(&key));
    let _ = harness.will_unload();

    harness.upgrade(key);
    harness.tick();

    assert!(harness.volume().contains_chunk(&key), "复活后树必须还在");
    assert!(harness.will_unload().is_empty(), "复活不得发卸载消息");
    assert_eq!(harness.stats().unloaded, 0);
    assert_eq!(harness.stats().pending_unloads, 0);
    assert_eq!(
        harness.stats().generated,
        generated_after_load,
        "复活走快路径，不得重生成"
    );
    assert_eq!(harness.content_hash(&key), expected, "内容必须保持原样");

    // 存档里那条未确认的写仍可落盘，但不得反过来触发释放。
    flush_store(&harness);
    harness.tick();
    assert!(harness.volume().contains_chunk(&key), "落盘后树仍必须保留");
    assert!(harness.will_unload().is_empty());
    assert_eq!(harness.stats().pending_unloads, 0);
}

#[test]
fn upgrade_after_commit_still_keeps_chunk() {
    let dir = TempDir::new("unload-revive-after-commit");
    let key = chunk_key(28, 0, 28);
    let mut harness = harness(vec![Answer::Miss]);
    attach_save_store(&mut harness, dir.path());

    harness.upgrade(key);
    resolve_with_store(&mut harness, key);

    harness.demote(key);
    harness.tick();
    assert!(harness.volume().contains_chunk(&key));
    assert_eq!(harness.stats().pending_unloads, 1);

    // 已 committed，但还没到 release 阶段就被升档撤回。
    flush_store(&harness);
    let _ = harness.will_unload();

    harness.upgrade(key);
    harness.tick();

    assert!(
        harness.volume().contains_chunk(&key),
        "已 committed 但还没释放时升档必须保留块"
    );
    assert!(harness.will_unload().is_empty(), "复活不得发卸载消息");
    assert_eq!(harness.stats().unloaded, 0);
    assert_eq!(harness.stats().pending_unloads, 0);

    harness.tick();
    assert!(harness.volume().contains_chunk(&key));
    assert!(harness.will_unload().is_empty());
    assert_eq!(harness.stats().unloaded, 0);
}

#[test]
fn failed_write_does_not_release() {
    let dir = TempDir::new("unload-failed-write");
    let key = chunk_key(29, 0, 29);
    let mut harness = harness(vec![Answer::Miss]);
    attach_save_store(&mut harness, dir.path());

    harness.upgrade(key);
    resolve_with_store(&mut harness, key);

    // 用目录占住 region 文件路径：后台写必然失败。
    let region = SavePaths::new(dir.path()).region(harness.body, key);
    std::fs::create_dir_all(&region).expect("创建占位目录");

    harness.demote(key);
    harness.tick();

    {
        let store = harness.app.world().resource::<SaveStore>();
        store.flush();
        for _ in 0..1000 {
            store.poll();
            if store.stats().in_flight_writes == 0 && store.stats().dirty > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(store.stats().in_flight_writes, 0, "写盘请求必须已回收");
        assert!(store.stats().dirty > 0, "失败的写必须重新标脏");
        let entry = harness
            .app
            .world()
            .resource::<PendingUnloads>()
            .0
            .get(&(harness.body, key))
            .expect("条目必须保留");
        let version = entry.version.expect("失败前必须已经提交过版本");
        assert!(
            !store.is_committed(harness.body, key, version),
            "写盘失败不得算 committed"
        );
    }

    assert!(
        harness.volume().contains_chunk(&key),
        "写盘失败时树必须保留"
    );
    assert!(harness.will_unload().is_empty(), "写盘失败不得发卸载消息");
    assert_eq!(harness.stats().unloaded, 0);
    assert_eq!(harness.stats().pending_unloads, 1);

    // 清掉占位目录后重试，落盘成功才释放。
    std::fs::remove_dir_all(&region).expect("删除占位目录");
    flush_store(&harness);
    harness.tick();

    assert!(!harness.volume().contains_chunk(&key), "确认落盘后才释放");
    assert_eq!(harness.stats().unloaded, 1);
    assert_eq!(harness.stats().pending_unloads, 0);
    assert_eq!(harness.will_unload().len(), 1);
}

#[test]
fn air_chunk_delete_waits_for_commit_before_releasing() {
    let dir = TempDir::new("unload-air-wait-commit");
    let key = chunk_key(30, 0, 30);
    let mut harness = harness(vec![Answer::Pending]);
    attach_save_store(&mut harness, dir.path());

    // 预置一条磁盘旧记录并落盘。
    harness
        .app
        .world()
        .resource::<SaveStore>()
        .write(
            harness.body,
            key,
            ChunkRecord {
                generator_version: 1,
                content: Some(vec![7; 4]),
                proposals: Vec::new(),
                entities: Vec::new(),
            },
        )
        .expect("预写旧记录");
    flush_store(&harness);
    let paths = SavePaths::new(dir.path());
    assert!(
        paths.region(harness.body, key).is_file(),
        "旧记录必须先在磁盘上"
    );

    harness.upgrade(key);
    harness.tick();
    assert!(!harness.volume().contains_chunk(&key), "Pending 不得装块");

    harness.demote(key);
    harness.tick();

    // 删除只是墓碑：确认落盘前不释放、不发消息。
    assert!(harness.will_unload().is_empty(), "未确认前不得发卸载消息");
    assert_eq!(harness.stats().unloaded, 0);
    assert_eq!(harness.stats().pending_unloads, 1);
    assert_eq!(
        harness
            .app
            .world()
            .resource::<SaveStore>()
            .stats()
            .tombstones,
        1,
        "空块必须标删除"
    );

    flush_store(&harness);
    harness.tick();

    assert_eq!(harness.stats().unloaded, 1);
    assert_eq!(harness.stats().pending_unloads, 0);
    assert_eq!(harness.will_unload().len(), 1);
    assert!(
        read_back(&harness, dir.path(), key).is_none(),
        "卸载空块必须删掉磁盘上的旧记录"
    );
    assert!(
        !paths.region(harness.body, key).exists(),
        "region 变空后文件必须删除"
    );
}

#[test]
fn pending_unloads_release_only_once() {
    let dir = TempDir::new("unload-once");
    let key = chunk_key(31, 0, 31);
    let mut harness = harness(vec![Answer::Miss]);
    attach_save_store(&mut harness, dir.path());

    harness.upgrade(key);
    harness.tick();
    harness.demote(key);
    harness.tick();
    flush_store(&harness);
    harness.tick();

    assert_eq!(harness.stats().unloaded, 1);
    assert_eq!(harness.stats().pending_unloads, 0);
    assert_eq!(harness.will_unload().len(), 1);

    // 再 tick 两次：不得重复释放、不得重复发消息。
    harness.tick();
    harness.tick();
    assert_eq!(harness.stats().unloaded, 1, "同一块只许释放一次");
    assert_eq!(harness.stats().pending_unloads, 0);
    assert!(harness.will_unload().is_empty(), "不得重复发卸载消息");
}

#[test]
fn hit_with_entities_emits_chunk_entities() {
    let key = chunk_key(3, 0, 3);
    let (bytes, _) = generated_bytes(key);
    let entities = vec![
        EntityRecord {
            stable_entity_id: 7,
            stable_type_id: 11,
            payload: vec![1, 2, 3],
        },
        EntityRecord {
            stable_entity_id: 9,
            stable_type_id: 11,
            payload: vec![4],
        },
    ];
    let record = ChunkRecord {
        generator_version: 1,
        content: Some(bytes),
        proposals: Vec::new(),
        entities: entities.clone(),
    };
    let mut harness = harness(vec![Answer::Record(record)]);

    harness.upgrade(key);
    harness.tick();

    let emitted = harness.chunk_entities();
    assert_eq!(emitted.len(), 1, "命中带实体只发一条 ChunkEntities");
    assert_eq!(emitted[0].body, harness.body);
    assert_eq!(emitted[0].key, key);
    assert_eq!(emitted[0].entities, entities);
    assert!(harness.volume().contains_chunk(&key));
    assert_eq!(harness.stats().loaded_from_store, 1);
    assert_eq!(harness.stats().generated, 0);
}

#[test]
fn generated_chunk_emits_no_chunk_entities() {
    let key = chunk_key(4, 0, 4);
    let mut harness = harness(vec![Answer::Miss]);

    harness.upgrade(key);
    harness.settle_all();

    assert!(harness.volume().contains_chunk(&key));
    assert_eq!(harness.stats().generated, 1);
    assert!(
        harness.chunk_entities().is_empty(),
        "生成路径不得发 ChunkEntities"
    );
}

/// 版本不一致的存档内容必须被丢掉：重生成，不装旧块，也不补发实体。
///
/// 这条测试盯的是「发布新地形」后的运行期路径：磁盘上还留着旧版本记录，
/// 若直接 install，旧地表会盖掉新生成器的结果。
#[test]
fn stale_record_is_regenerated_not_installed() {
    let key = chunk_key(5, 0, 5);
    // 注入内容与生成器产出必须可区分：实心块 vs 地形。
    let (injected, injected_hash) = solid_bytes(200);
    let (_, generated_hash) = generated_bytes(key);
    let entities = vec![EntityRecord {
        stable_entity_id: 7,
        stable_type_id: 11,
        payload: vec![1, 2, 3],
    }];
    let record = ChunkRecord {
        // 当前 VoxelStreamConfig::generator_version 是 1，这里给 2。
        generator_version: 2,
        content: Some(injected),
        proposals: Vec::new(),
        entities,
    };
    let mut harness = harness(vec![Answer::Record(record)]);

    harness.upgrade(key);
    harness.settle_all();

    assert_eq!(
        harness.stats().loaded_from_store,
        0,
        "stale 记录不得计入命中"
    );
    assert_eq!(harness.stats().generated, 1, "必须走生成路径");
    assert_eq!(
        harness.content_hash(&key),
        generated_hash,
        "块内容应是生成器产出"
    );
    assert_ne!(
        harness.content_hash(&key),
        injected_hash,
        "不得装上注入的旧内容"
    );
    assert!(
        harness.chunk_entities().is_empty(),
        "stale 分支不得发 ChunkEntities（实体已由启动读档恢复，重复发会二次 apply）"
    );
    assert_eq!(harness.reached().len(), 1, "块仍要正常完成");
}

/// 版本一致的记录行为不变：装块、计数、发实体。
#[test]
fn matching_record_is_installed() {
    let key = chunk_key(6, 0, 6);
    let (bytes, injected_hash) = solid_bytes(200);
    let entities = vec![EntityRecord {
        stable_entity_id: 7,
        stable_type_id: 11,
        payload: vec![1, 2, 3],
    }];
    let record = ChunkRecord {
        generator_version: 1,
        content: Some(bytes),
        proposals: Vec::new(),
        entities: entities.clone(),
    };
    let mut harness = harness(vec![Answer::Record(record)]);

    harness.upgrade(key);
    harness.tick();

    assert_eq!(harness.stats().loaded_from_store, 1);
    assert_eq!(harness.stats().generated, 0);
    assert_eq!(harness.content_hash(&key), injected_hash, "应装存档内容");
    let emitted = harness.chunk_entities();
    assert_eq!(emitted.len(), 1, "版本一致时照旧发一条 ChunkEntities");
    assert_eq!(emitted[0].body, harness.body);
    assert_eq!(emitted[0].key, key);
    assert_eq!(emitted[0].entities, entities);
}

/// 两者同时存在时，读路径以真实 SaveStore 为准，测试替身不许挡路。
///
/// 走完整链路：写入真 store → flush 确认落盘（buffer 被清掉）→ 第一次
/// resolve 只能拿到 Pending 并发起后台读 → poll 等到 ready → 下一次
/// resolve 装磁盘内容。这样才真正验证「优先读 SaveStore」，而不是只验证
/// 内存缓冲。
#[test]
fn resolve_prefers_save_store_over_scripted_store() {
    let root = TempDir::new("prefer-save-store");
    let key = chunk_key(7, 0, 7);
    let (scripted, scripted_hash) = solid_bytes(9);
    let (real, real_hash) = solid_bytes(200);
    assert_ne!(scripted_hash, real_hash, "两份内容必须不同才有判别力");

    // 假存档给一个「明显不同」的答案；若读路径错误地用了它，哈希会对上。
    let mut harness = harness(vec![Answer::Hit(Some(scripted))]);
    attach_save_store(&mut harness, root.path());

    // 写入真 store 并确认落盘。落盘后 apply_write 会清掉内存缓冲，之后
    // 的 lookup 必须走后台读，正好覆盖生产里的「卸载后再请求」。
    {
        let store = harness.app.world().resource::<SaveStore>();
        store
            .write(
                harness.body,
                key,
                ChunkRecord {
                    generator_version: 1,
                    content: Some(real),
                    proposals: Vec::new(),
                    entities: Vec::new(),
                },
            )
            .expect("写入 SaveStore 缓冲");
        store.flush();
    }
    flush_store(&harness);
    {
        let store = harness.app.world().resource::<SaveStore>();
        assert!(
            store.is_committed(harness.body, key, 1),
            "前置条件：记录必须已确认落盘"
        );
        assert_eq!(store.stats().buffered, 0, "落盘后内存缓冲应已清空");
    }

    // 第一次 resolve：磁盘命中要等后台读，先 Pending。
    harness.upgrade(key);
    harness.tick();
    assert_eq!(harness.stats().pending, 1, "首次查询应等后台读");
    assert_eq!(harness.stats().loaded_from_store, 0);

    // 轮询后台结果直到记录进入 ready（没有 SavePlugin 的 Poll 系统，手动推进）。
    let ready = {
        let store = harness.app.world().resource::<SaveStore>();
        let mut ready = false;
        for _ in 0..1000 {
            store.poll();
            if store.stats().ready_hits > 0 {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        ready
    };
    assert!(ready, "SaveStore 没有把磁盘记录读回 ready 队列");

    // 第二次 resolve：从 ready 交付，装的是磁盘内容。
    harness.tick();
    assert_eq!(harness.stats().loaded_from_store, 1);
    assert_eq!(harness.stats().generated, 0);
    assert_eq!(harness.content_hash(&key), real_hash, "应装真 store 的内容");
    assert_ne!(
        harness.content_hash(&key),
        scripted_hash,
        "假存档的答案不得被用"
    );

    let mut store = harness.app.world_mut().resource_mut::<SaveStore>();
    store.shutdown_worker();
}

// ---- 两段流水线：提案 → 邻门 → 裁决 → realise ----

/// 区块的体素原点（测试只覆盖小坐标，不会溢出）。
fn chunk_origin(key: ChunkKey) -> [i32; 3] {
    let extent = game_engine::voxel::chunk_voxels_per_axis();
    [key.x * extent, key.y * extent, key.z * extent]
}

/// 在标准 harness 上装一个世界物体注册表。
fn harness_with_registry(answers: Vec<Answer>, registry: WorldFeatureRegistry) -> Harness {
    let mut harness = harness(answers);
    harness.app.insert_resource(registry);
    harness
}

const TREE_BLOCK: u8 = 7;
const CROSS_BLOCK: u8 = 9;

/// 测试物体：每个区块原点附近长一根 1×3×1 的柱子。
#[derive(Default)]
struct TreeFeature;

impl WorldFeature for TreeFeature {
    const TYPE_ID: u16 = 7;

    fn propose(&self, _seed: u64, owner: ChunkKey) -> Vec<ProposalSpec> {
        let base = chunk_origin(owner);
        vec![ProposalSpec {
            origin: [base[0] + 8, base[1] + 8, base[2] + 8],
            bounds_min: [base[0] + 8, base[1] + 8, base[2] + 8],
            bounds_max: [base[0] + 9, base[1] + 11, base[2] + 9],
            priority: 0,
        }]
    }

    fn realize(&self, _seed: u64, proposal: &ProposalRecord, sink: &mut ChunkSink<'_>) {
        for y in proposal.bounds_min[1]..proposal.bounds_max[1] {
            sink.set_world([proposal.origin[0], y, proposal.origin[2]], TREE_BLOCK);
        }
    }
}

/// 测试物体：只在 (0,0,0) 里提出一条跨到 +x 邻块的提案。
#[derive(Default)]
struct CrossFeature;

impl WorldFeature for CrossFeature {
    const TYPE_ID: u16 = 8;

    fn propose(&self, _seed: u64, owner: ChunkKey) -> Vec<ProposalSpec> {
        if owner != chunk_key(0, 0, 0) {
            return Vec::new();
        }
        vec![ProposalSpec {
            origin: [30, 0, 0],
            bounds_min: [30, 0, 0],
            bounds_max: [34, 3, 1],
            priority: 0,
        }]
    }

    fn realize(&self, _seed: u64, proposal: &ProposalRecord, sink: &mut ChunkSink<'_>) {
        for x in proposal.bounds_min[0]..proposal.bounds_max[0] {
            for y in proposal.bounds_min[1]..proposal.bounds_max[1] {
                for z in proposal.bounds_min[2]..proposal.bounds_max[2] {
                    sink.set_world([x, y, z], CROSS_BLOCK);
                }
            }
        }
    }
}

#[test]
fn miss_stages_then_settles_in_two_ticks() {
    let key = chunk_key(9, 0, 2);
    let mut harness = harness(vec![Answer::Miss]);

    harness.upgrade(key);
    harness.tick();

    assert_eq!(harness.stats().proposed, 1, "第一 tick 只提案");
    assert_eq!(harness.stats().staged, 1);
    assert!(!harness.volume().contains_chunk(&key), "暂存块不得进容器");
    assert!(harness.reached().is_empty(), "暂存块不得发到达消息");
    assert_eq!(
        harness
            .app
            .world()
            .resource::<GenTable>()
            .gen_state((harness.body, key)),
        GenState::Undetermined
    );
    assert!(harness
        .app
        .world()
        .resource::<StagedChunks>()
        .0
        .contains_key(&(harness.body, key)));

    harness.tick();

    assert_eq!(harness.stats().staged, 0);
    assert!(harness.volume().contains_chunk(&key));
    assert_eq!(
        harness
            .app
            .world()
            .resource::<GenTable>()
            .gen_state((harness.body, key)),
        GenState::Determined
    );
    assert_eq!(
        harness.reached(),
        vec![ChunkReached {
            body: harness.body,
            key,
            level: Residency::Data,
        }]
    );
}

#[test]
fn feature_is_realized_into_settled_chunk() {
    let key = chunk_key(11, 0, 3);
    let mut registry = WorldFeatureRegistry::default();
    registry.register::<TreeFeature>().expect("注册测试物体");
    let mut harness = harness_with_registry(vec![Answer::Miss], registry);

    harness.upgrade(key);
    harness.settle_all();

    let world = harness.app.world();
    let interner = world.resource::<VoxelInterner>();
    let volume = world
        .entity(harness.entity)
        .get::<VoxVolume>()
        .expect("body 必须带容器");
    let tree = volume.get_chunk(&key).expect("块必须已装进容器");
    for y in 8..11 {
        assert_eq!(
            tree.get(interner.inner(), IVec3::new(8, y, 8)),
            Some(TREE_BLOCK),
            "裁决后的 realise 必须把 feature 的体素写进树"
        );
    }
}

#[test]
fn cross_chunk_feature_writes_both_sides() {
    let left = chunk_key(0, 0, 0);
    let right = chunk_key(1, 0, 0);
    let mut registry = WorldFeatureRegistry::default();
    registry.register::<CrossFeature>().expect("注册跨块物体");
    let mut harness = harness_with_registry(vec![Answer::Miss], registry);

    harness.upgrade(left);
    harness.upgrade(right);
    harness.settle_all();

    assert!(harness.volume().contains_chunk(&left));
    assert!(harness.volume().contains_chunk(&right));

    let world = harness.app.world();
    let interner = world.resource::<VoxelInterner>();
    let volume = world
        .entity(harness.entity)
        .get::<VoxVolume>()
        .expect("body 必须带容器");
    let left_tree = volume.get_chunk(&left).expect("左块必须在容器里");
    let right_tree = volume.get_chunk(&right).expect("右块必须在容器里");

    for (x, y, z) in [(30, 0, 0), (30, 1, 0), (31, 2, 0)] {
        assert_eq!(
            left_tree.get(interner.inner(), IVec3::new(x, y, z)),
            Some(CROSS_BLOCK),
            "左块必须写上自己那一段"
        );
    }
    for (x, y, z) in [(0, 0, 0), (0, 1, 0), (1, 2, 0)] {
        assert_eq!(
            right_tree.get(interner.inner(), IVec3::new(x, y, z)),
            Some(CROSS_BLOCK),
            "右块必须写上自己那一段（世界坐标减去本块原点）"
        );
    }
    assert_ne!(
        left_tree.get(interner.inner(), IVec3::new(0, 0, 0)),
        Some(CROSS_BLOCK),
        "左块不该出现邻块那段"
    );
    assert_ne!(
        right_tree.get(interner.inner(), IVec3::new(30, 0, 0)),
        Some(CROSS_BLOCK),
        "右块不该出现左块那段"
    );
}

#[test]
fn neighbour_proposals_are_computed_on_demand() {
    let key = chunk_key(12, 0, 3);
    let mut registry = WorldFeatureRegistry::default();
    registry.register::<TreeFeature>().expect("注册测试物体");
    let mut harness = harness_with_registry(vec![Answer::Miss], registry);

    harness.upgrade(key);
    harness.settle_all();

    let mut neighbours = Vec::new();
    for dx in -1..=1 {
        for dy in -1..=1 {
            for dz in -1..=1 {
                if dx == 0 && dy == 0 && dz == 0 {
                    continue;
                }
                neighbours.push(chunk_key(key.x + dx, key.y + dy, key.z + dz));
            }
        }
    }

    let body = harness.body;
    let world = harness.app.world();
    let registry = world.resource::<WorldFeatureRegistry>();
    let cache = world.resource::<ProposalCache>();
    for neighbour in &neighbours {
        let expected = registry
            .propose_chunk(SEED, *neighbour)
            .expect("邻块提案必须可算");
        assert_eq!(
            cache.0.get(&(body, *neighbour)),
            Some(&expected),
            "邻块 {neighbour:?} 的提案必须按需补齐"
        );
    }
    assert_eq!(cache.0.len(), 27, "自身 + 26 邻");
}

/// 1.1 的等价性证明：常量数组必须与旧的「三轴生成 + sort」逐项一致。
#[test]
fn neighbour_offsets_match_the_sorted_vec() {
    let key = chunk_key(-3, 5, -1);

    // 旧实现：三轴 -1..=1、跳过原点、按邻键排序。
    let mut expected: Vec<ChunkKey> = Vec::with_capacity(26);
    for dx in -1..=1 {
        for dy in -1..=1 {
            for dz in -1..=1 {
                if dx == 0 && dy == 0 && dz == 0 {
                    continue;
                }
                expected.push(chunk_key(key.x + dx, key.y + dy, key.z + dz));
            }
        }
    }
    expected.sort();

    // 新实现：直接遍历常量数组。
    let actual: Vec<ChunkKey> = NEIGHBOUR_OFFSETS
        .iter()
        .map(|offset| chunk_key(key.x + offset.x, key.y + offset.y, key.z + offset.z))
        .collect();

    assert_eq!(actual, expected, "常量数组的顺序必须与旧的 sort 结果一致");
    assert_eq!(actual.len(), 26, "邻域恰好 26 项");
    assert!(
        actual.windows(2).all(|pair| pair[0] < pair[1]),
        "邻键必须严格升序（无重复、也不含自身）"
    );
}

#[test]
fn unload_before_settle_releases_staged_tree() {
    let key = chunk_key(13, 0, 3);
    let mut harness = harness(vec![Answer::Miss]);
    let baseline = harness.alive_nodes();

    harness.upgrade(key);
    harness.tick();
    assert_eq!(harness.stats().staged, 1);
    assert!(harness.alive_nodes() > baseline, "暂存树必须先真的生成出来");

    harness.demote(key);
    harness.tick();

    let pair = (harness.body, key);
    assert!(
        harness.app.world().resource::<StagedChunks>().0.is_empty(),
        "降档必须丢弃暂存块"
    );
    assert!(!harness
        .app
        .world()
        .resource::<ProposalCache>()
        .0
        .contains_key(&pair));
    assert_eq!(
        harness.alive_nodes(),
        baseline,
        "暂存树必须被释放，不能泄漏节点"
    );
    assert!(
        harness.will_unload().is_empty(),
        "未定稿的块不走落盘/卸载消息"
    );
    assert!(!harness.volume().contains_chunk(&key));
}

#[test]
fn store_hit_still_settles_without_propose() {
    let dir = TempDir::new("store-hit-settles");
    let key = chunk_key(21, 0, 21);
    let (bytes, expected) = generated_bytes(key);
    let mut harness = harness(vec![Answer::Miss]);
    attach_save_store(&mut harness, dir.path());

    // 预写一条 content=Some 的记录并留在写缓冲里：lookup 会同步命中它。
    {
        let store = harness.app.world().resource::<SaveStore>();
        store
            .write(
                harness.body,
                key,
                ChunkRecord {
                    generator_version: 1,
                    content: Some(bytes),
                    proposals: Vec::new(),
                    entities: Vec::new(),
                },
            )
            .expect("写入 SaveStore 写缓冲");
    }

    harness.upgrade(key);
    harness.tick();

    assert!(
        harness.volume().contains_chunk(&key),
        "命中存档必须一 tick 装块"
    );
    assert_eq!(harness.content_hash(&key), expected);
    assert_eq!(harness.stats().loaded_from_store, 1);
    assert_eq!(harness.stats().proposed, 0, "命中存档不得走提案路径");
    assert_eq!(harness.stats().generated, 0);
    assert_eq!(harness.stats().staged, 0);
    assert_eq!(
        harness
            .app
            .world()
            .resource::<GenTable>()
            .gen_state((harness.body, key)),
        GenState::Determined
    );
}
