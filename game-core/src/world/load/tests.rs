use super::*;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use bevy::prelude::Component;
use serde::{Deserialize, Serialize};

use game_engine::identity::{StableEntityId, StableEntityIndex};
use game_engine::input::{ActionMap, ResolvedActions};
use game_engine::modding::{
    AppliedModSet, ModManifest, ModRef, ParamScope, ParamSpec, ParamTable, ParamType,
};
use game_engine::residency::ChunkAnchor;
use game_engine::save::{write_world_meta, ModRecord, Persist, PersistComponent, StableTypeId};
use game_engine::voxel::{chunk_content_hash, MaxDepth, VoxOpsBulkWrite, VoxTree, CHUNK_DEPTH};

use crate::voxel::body::spawn_island_entity;
use crate::world::{capture_world, write_capture};

#[derive(Component, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct TestMarker {
    value: u32,
    label: String,
}

impl PersistComponent for TestMarker {
    const STABLE_TYPE_ID: StableTypeId = StableTypeId::new(7);
}

/// 读档需要的全部资源：插件在真实 App 里注册它们，测试里手工插入。
fn test_world(seed: u64) -> World {
    let mut world = World::new();
    world.insert_resource(WorldSeed(seed));
    world.insert_resource(IslandRegistry::new());
    world.insert_resource(StableIdAllocator::default());
    world.insert_resource(StableEntityIndex::default());
    world.insert_resource(VoxelInterner::new(4 * 1024 * 1024));
    world.insert_resource(PersistRegistry::new());
    world.insert_resource(OpaqueComponents::default());
    world
}

fn insert_filled_chunk(world: &mut World, volume: &mut VoxVolume, key: ChunkKey, value: u8) {
    let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
    {
        let mut interner = world.resource_mut::<VoxelInterner>();
        tree.fill(interner.inner_mut(), value);
    }
    volume.insert_chunk(key, tree);
}

/// std::env::temp_dir 下的唯一子目录；Drop 时整棵删掉。
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("系统时钟早于 UNIX_EPOCH")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ecs_game_world_load_{tag}_{}_{nanos}_{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
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

/// 只挂一个带若干实心块的岛 body（岛走唯一生成入口）。
fn spawn_island_with_chunks(
    world: &mut World,
    drift: Vec3F,
    anchored: bool,
    biome: BiomeId,
    chunks: &[ChunkKey],
) -> (Entity, BodyKey) {
    let (entity, island_id) = spawn_island_entity(world, drift, anchored, biome);
    let mut volume = VoxVolume::new(FixedPoint::from_num(1));
    for (index, chunk) in chunks.iter().enumerate() {
        insert_filled_chunk(
            world,
            &mut volume,
            *chunk,
            u8::try_from(index + 1).unwrap_or(1),
        );
    }
    world.entity_mut(entity).insert(volume);
    (entity, BodyKey::from(island_id))
}

fn chunk_hashes(world: &World) -> BTreeMap<(u64, ChunkKey), u64> {
    let interner = world.resource::<VoxelInterner>();
    let mut hashes = BTreeMap::new();
    for entity in world.iter_entities() {
        let Some(id) = entity.get::<StableEntityId>() else {
            continue;
        };
        let Some(volume) = entity.get::<VoxVolume>() else {
            continue;
        };
        for (key, tree) in volume.iter() {
            hashes.insert((id.0, *key), chunk_content_hash(interner.inner(), tree));
        }
    }
    hashes
}

fn body_ids(world: &World) -> BTreeSet<u64> {
    world
        .iter_entities()
        .filter_map(|entity| {
            let id = entity.get::<StableEntityId>()?;
            entity.get::<VoxVolume>()?;
            Some(id.0)
        })
        .collect()
}

fn stable_ids(world: &World) -> BTreeSet<u64> {
    world
        .iter_entities()
        .filter_map(|entity| entity.get::<StableEntityId>().map(|id| id.0))
        .collect()
}

#[test]
fn load_world_round_trips_capture() {
    const TICK: u64 = 4321;
    const GENERATOR_VERSION: u32 = 3;

    let mut source = test_world(0x5EED);
    source
        .resource_mut::<PersistRegistry>()
        .register::<TestMarker>()
        .expect("注册测试组件");
    source.resource_mut::<StableIdAllocator>().restore(100);

    let drift = Vec3F::new(
        FixedPoint::from_num(3),
        FixedPoint::from_num(-4),
        FixedPoint::from_num(5),
    );
    let (body_entity, island_id) = spawn_island_entity(&mut source, drift, true, BiomeId(3));
    let body_key = BodyKey::from(island_id);

    let chunk_a = ChunkKey::new(0, 0, 0);
    let chunk_b = ChunkKey::new(1, 0, 0);
    let mut volume = VoxVolume::new(FixedPoint::from_num(2));
    insert_filled_chunk(&mut source, &mut volume, chunk_a, 1);
    insert_filled_chunk(&mut source, &mut volume, chunk_b, 2);
    source.entity_mut(body_entity).insert(volume);

    let marker = TestMarker {
        value: 77,
        label: "roundtrip".to_string(),
    };
    let entity_id = source.resource_mut::<StableIdAllocator>().allocate();
    source.spawn((
        entity_id,
        marker.clone(),
        Persist {
            prototype_type_id: 7,
        },
        ChunkAnchor::new(body_key, chunk_a),
    ));

    let capture = capture_world(&mut source, TICK, GENERATOR_VERSION).expect("采集成功");
    let dir = TempDir::new("round_trip");
    let paths = SavePaths::new(dir.path());
    write_capture(&paths, &capture).expect("落盘成功");

    let hashes_before = chunk_hashes(&source);

    let mut loaded = test_world(1);
    loaded
        .resource_mut::<PersistRegistry>()
        .register::<TestMarker>()
        .expect("注册测试组件");
    let result = load_world(&mut loaded, &paths, GENERATOR_VERSION)
        .expect("读档成功")
        .expect("world.meta 存在");

    assert_eq!(result.world_tick, TICK);
    assert_eq!(
        result.stats,
        LoadStats {
            bodies: 1,
            chunks: 2,
            stale_chunks: 0,
            entities: 1,
            orphan_regions: 0,
        }
    );
    assert_eq!(
        read_world_meta(&paths).expect("读 meta"),
        Some(capture.meta.clone()),
        "meta 往返必须逐字段相等"
    );
    assert_eq!(loaded.resource::<WorldSeed>().0, capture.meta.seed);
    assert_eq!(
        loaded.resource::<StableIdAllocator>().next_id(),
        capture.meta.next_stable_entity_id
    );

    let source_island = source
        .resource::<IslandRegistry>()
        .get(IslandId(body_key.as_u64()))
        .copied();
    let loaded_island = loaded
        .resource::<IslandRegistry>()
        .get(IslandId(body_key.as_u64()))
        .copied();
    assert_eq!(loaded_island, source_island, "IslandRegistry 条目必须相等");
    assert_eq!(loaded.resource::<IslandRegistry>().len(), 1);

    let loaded_body = loaded
        .resource::<StableEntityIndex>()
        .entity(body_key.stable_id())
        .expect("body 实体必须存在");
    assert_eq!(
        loaded.get::<Body>(loaded_body),
        Some(&Body::island(true, BiomeId(3)))
    );
    assert!(loaded.get::<IslandTag>(loaded_body).is_some());
    assert_eq!(
        loaded
            .get::<VoxVolume>(loaded_body)
            .expect("body 必须有 VoxVolume")
            .voxel_size
            .to_bits(),
        capture.meta.bodies[&body_key].voxel_size
    );

    assert_eq!(chunk_hashes(&loaded), hashes_before, "每块内容哈希必须相等");

    let entity = loaded
        .resource::<StableEntityIndex>()
        .entity(entity_id)
        .expect("持久实体必须存在");
    assert_eq!(loaded.get::<TestMarker>(entity), Some(&marker));
}

#[test]
fn load_without_meta_returns_none_and_leaves_world_alone() {
    let dir = TempDir::new("no_meta");
    let paths = SavePaths::new(dir.path());

    let mut world = test_world(1);
    world.spawn(StableEntityId(9));
    let before = world.iter_entities().count();

    let loaded = load_world(&mut world, &paths, 1).expect("没有 meta 不是错误");

    assert!(loaded.is_none());
    assert_eq!(world.iter_entities().count(), before, "世界实体数不变");
    assert_eq!(world.resource::<StableIdAllocator>().next_id(), 1);
}

#[test]
fn loaded_content_is_marked_edited() {
    const VERSION: u32 = 1;
    let mut source = test_world(0xED17);
    let drift = Vec3F::new(
        FixedPoint::from_num(0),
        FixedPoint::from_num(0),
        FixedPoint::from_num(0),
    );
    let chunk_a = ChunkKey::new(0, 0, 0);
    let chunk_b = ChunkKey::new(1, 0, 0);
    let (_, body_key) =
        spawn_island_with_chunks(&mut source, drift, false, BiomeId(0), &[chunk_a, chunk_b]);

    let dir = TempDir::new("loaded_edited");
    let paths = SavePaths::new(dir.path());
    let capture = capture_world(&mut source, 1, VERSION).expect("采集成功");
    write_capture(&paths, &capture).expect("落盘成功");

    let mut loaded = test_world(0xED17);
    loaded.insert_resource(VoxelDirtySet::default());
    let result = load_world(&mut loaded, &paths, VERSION)
        .expect("读档成功")
        .expect("world.meta 存在");
    assert_eq!(result.stats.chunks, 2);

    let dirty = loaded.resource::<VoxelDirtySet>();
    assert!(
        dirty.is_edited_for(Some(body_key), &chunk_a),
        "装回的块必须标成已编辑"
    );
    assert!(
        dirty.is_edited_for(Some(body_key), &chunk_b),
        "装回的块必须标成已编辑"
    );
    assert_eq!(dirty.edited_len(), 2, "只标记真正装回内容的块");
    assert_eq!(
        dirty.edited_keys(),
        vec![(Some(body_key), chunk_a), (Some(body_key), chunk_b)],
        "edited 集合必须按 BTreeSet 升序（确定性）"
    );
    assert!(
        !dirty.is_edited_for(Some(body_key), &ChunkKey::new(2, 0, 0)),
        "没有记录的块不得被标"
    );
}

#[test]
fn content_none_chunks_are_not_installed() {
    let mut source = test_world(2);
    source
        .resource_mut::<PersistRegistry>()
        .register::<TestMarker>()
        .expect("注册测试组件");

    let (body_entity, island_id) = spawn_island_entity(&mut source, Vec3F::ZERO, true, BiomeId(1));
    let body_key = BodyKey::from(island_id);
    let chunk_a = ChunkKey::new(0, 0, 0);
    let chunk_b = ChunkKey::new(1, 0, 0);

    let mut volume = VoxVolume::new(FixedPoint::from_num(1));
    insert_filled_chunk(&mut source, &mut volume, chunk_a, 1);
    source.entity_mut(body_entity).insert(volume);

    let entity_id = source.resource_mut::<StableIdAllocator>().allocate();
    source.spawn((
        entity_id,
        TestMarker {
            value: 5,
            label: "contentless".to_string(),
        },
        Persist {
            prototype_type_id: 7,
        },
        ChunkAnchor::new(body_key, chunk_b),
    ));

    let capture = capture_world(&mut source, 6, 1).expect("采集成功");
    let dir = TempDir::new("content_none");
    let paths = SavePaths::new(dir.path());
    write_capture(&paths, &capture).expect("落盘成功");

    let mut loaded = test_world(1);
    loaded
        .resource_mut::<PersistRegistry>()
        .register::<TestMarker>()
        .expect("注册测试组件");
    let result = load_world(&mut loaded, &paths, 1)
        .expect("读档成功")
        .expect("world.meta 存在");

    assert_eq!(result.stats.chunks, 2, "content = None 的记录也要计数");
    assert_eq!(result.stats.entities, 1);

    let loaded_body = loaded
        .resource::<StableEntityIndex>()
        .entity(body_key.stable_id())
        .expect("body 实体必须存在");
    let volume = loaded.get::<VoxVolume>(loaded_body).expect("VoxVolume");
    assert!(volume.get_chunk(&chunk_a).is_some(), "有内容的块必须装载");
    assert!(
        volume.get_chunk(&chunk_b).is_none(),
        "content = None 的块留给生成器，不能凭空产生"
    );
    assert_eq!(volume.chunks.len(), 1);

    let entity = loaded
        .resource::<StableEntityIndex>()
        .entity(entity_id)
        .expect("只有实体的块也要恢复实体");
    assert!(loaded.get::<TestMarker>(entity).is_some());
    assert_eq!(
        loaded.get::<ChunkAnchor>(entity),
        Some(&ChunkAnchor::new(body_key, chunk_b)),
        "读档必须按分组键补回 ChunkAnchor，否则实体再存不回去"
    );
}

#[test]
fn load_drops_content_when_generator_version_differs() {
    // 存档是版本 1 的生成器写的；用版本 2 读，旧地形必须被丢弃。
    let mut source = test_world(7);
    let (_, body_key) = spawn_island_with_chunks(
        &mut source,
        Vec3F::ZERO,
        true,
        BiomeId(2),
        &[ChunkKey::new(0, 0, 0)],
    );
    let capture = capture_world(&mut source, 1, 1).expect("采集成功");
    let dir = TempDir::new("stale_version");
    let paths = SavePaths::new(dir.path());
    write_capture(&paths, &capture).expect("落盘成功");

    let mut loaded = test_world(1);
    let result = load_world(&mut loaded, &paths, 2)
        .expect("版本不匹配不是错误")
        .expect("world.meta 存在");

    // 记录还在（chunks 照计），只是内容被丢；实体阶段照常恢复。
    assert_eq!(result.stats.chunks, 1);
    assert_eq!(result.stats.stale_chunks, 1);

    let loaded_body = loaded
        .resource::<StableEntityIndex>()
        .entity(body_key.stable_id())
        .expect("body 实体必须保留");
    assert_eq!(
        loaded
            .get::<VoxVolume>(loaded_body)
            .expect("body 必须有 VoxVolume")
            .chunk_count(),
        0,
        "旧版本内容不能被装进卷"
    );
}

#[test]
fn load_keeps_content_when_generator_version_matches() {
    let mut source = test_world(8);
    let (_, body_key) = spawn_island_with_chunks(
        &mut source,
        Vec3F::ZERO,
        true,
        BiomeId(2),
        &[ChunkKey::new(0, 0, 0)],
    );
    let capture = capture_world(&mut source, 1, 1).expect("采集成功");
    let dir = TempDir::new("fresh_version");
    let paths = SavePaths::new(dir.path());
    write_capture(&paths, &capture).expect("落盘成功");

    let mut loaded = test_world(1);
    let result = load_world(&mut loaded, &paths, 1)
        .expect("读档成功")
        .expect("world.meta 存在");

    assert_eq!(result.stats.stale_chunks, 0);
    let loaded_body = loaded
        .resource::<StableEntityIndex>()
        .entity(body_key.stable_id())
        .expect("body 实体必须存在");
    assert_eq!(
        loaded
            .get::<VoxVolume>(loaded_body)
            .expect("body 必须有 VoxVolume")
            .chunk_count(),
        1
    );
}

#[test]
fn load_restores_allocator_and_never_reuses_loaded_ids() {
    let mut source = test_world(3);
    let (_, body_key) = spawn_island_with_chunks(
        &mut source,
        Vec3F::ZERO,
        true,
        BiomeId(2),
        &[ChunkKey::new(0, 0, 0)],
    );

    let capture = capture_world(&mut source, 1, 1).expect("采集成功");
    let dir = TempDir::new("allocator");
    let paths = SavePaths::new(dir.path());
    write_capture(&paths, &capture).expect("落盘成功");

    let mut loaded = test_world(1);
    load_world(&mut loaded, &paths, 1)
        .expect("读档成功")
        .expect("world.meta 存在");

    let next = loaded.resource::<StableIdAllocator>().next_id();
    assert_eq!(next, capture.meta.next_stable_entity_id);
    assert!(next > body_key.as_u64());

    let before = stable_ids(&loaded);
    let allocated = loaded.resource_mut::<StableIdAllocator>().allocate();
    assert_eq!(allocated.0, next, "恢复后第一次分配必须正好是 next");
    assert!(!before.contains(&allocated.0), "新 ID 不能与已加载 ID 冲突");
    assert!(!loaded
        .resource::<StableEntityIndex>()
        .contains_id(allocated));
}

#[test]
fn truncated_region_is_an_error_before_world_is_touched() {
    let mut source = test_world(4);
    let (_, body_key) = spawn_island_with_chunks(
        &mut source,
        Vec3F::ZERO,
        true,
        BiomeId(0),
        &[ChunkKey::new(0, 0, 0)],
    );

    let capture = capture_world(&mut source, 1, 1).expect("采集成功");
    let dir = TempDir::new("truncated");
    let paths = SavePaths::new(dir.path());
    write_capture(&paths, &capture).expect("落盘成功");

    let region_path = paths.region(body_key, ChunkKey::new(0, 0, 0));
    assert!(region_path.exists(), "region 文件必须存在");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&region_path)
        .expect("打开 region");
    file.set_len(8).expect("截断 region");
    drop(file);

    let mut world = test_world(9);
    spawn_island_entity(&mut world, Vec3F::ZERO, true, BiomeId(0));
    let entities_before = world.iter_entities().count();
    let islands_before = world.resource::<IslandRegistry>().len();
    let next_before = world.resource::<StableIdAllocator>().next_id();

    let err = load_world(&mut world, &paths, 1).expect_err("截断的 region 必须报错");
    assert!(!err.path().is_empty());

    assert_eq!(
        world.iter_entities().count(),
        entities_before,
        "阶段一失败不能新建实体"
    );
    assert_eq!(
        world.resource::<IslandRegistry>().len(),
        islands_before,
        "阶段一失败不能改注册表"
    );
    assert_eq!(
        world.resource::<StableIdAllocator>().next_id(),
        next_before,
        "阶段一失败不能改发号器"
    );
    assert_eq!(world.resource::<StableEntityIndex>().len(), 1);
}

#[test]
fn same_save_loads_identically() {
    let mut source = test_world(5);
    source
        .resource_mut::<PersistRegistry>()
        .register::<TestMarker>()
        .expect("注册测试组件");
    let (_, body_key) = spawn_island_with_chunks(
        &mut source,
        Vec3F::new(
            FixedPoint::from_num(-1),
            FixedPoint::from_num(2),
            FixedPoint::from_num(0),
        ),
        false,
        BiomeId(4),
        &[ChunkKey::new(0, 0, 0), ChunkKey::new(4, 0, 0)],
    );
    let entity_id = source.resource_mut::<StableIdAllocator>().allocate();
    source.spawn((
        entity_id,
        TestMarker {
            value: 11,
            label: "deterministic".to_string(),
        },
        Persist {
            prototype_type_id: 7,
        },
        ChunkAnchor::new(body_key, ChunkKey::new(4, 0, 0)),
    ));

    let capture = capture_world(&mut source, 8, 2).expect("采集成功");
    let dir = TempDir::new("deterministic");
    let paths = SavePaths::new(dir.path());
    write_capture(&paths, &capture).expect("落盘成功");

    let mut first = test_world(1);
    let mut second = test_world(2);
    let first_result = load_world(&mut first, &paths, 2)
        .expect("第一次读档")
        .expect("world.meta 存在");
    let second_result = load_world(&mut second, &paths, 2)
        .expect("第二次读档")
        .expect("world.meta 存在");

    assert_eq!(first_result, second_result);
    assert_eq!(body_ids(&first), body_ids(&second), "body 键集合必须相等");
    assert_eq!(
        chunk_hashes(&first),
        chunk_hashes(&second),
        "块内容哈希必须相等"
    );
    assert_eq!(
        stable_ids(&first),
        stable_ids(&second),
        "实体 ID 集合必须相等"
    );
    assert_eq!(
        first.resource::<IslandRegistry>().len(),
        second.resource::<IslandRegistry>().len()
    );
}

#[test]
fn orphan_regions_are_counted_not_deleted_or_rejected() {
    let mut source = test_world(6);
    let (_, body_key) = spawn_island_with_chunks(
        &mut source,
        Vec3F::ZERO,
        true,
        BiomeId(1),
        &[ChunkKey::new(0, 0, 0)],
    );
    let capture = capture_world(&mut source, 1, 1).expect("采集成功");
    let dir = TempDir::new("orphan");
    let paths = SavePaths::new(dir.path());
    write_capture(&paths, &capture).expect("落盘成功");

    let orphan = BodyKey::new(StableEntityId(999));
    let orphan_dir = paths.body_dir(orphan);
    std::fs::create_dir_all(&orphan_dir).expect("创建孤儿 body 目录");
    let source_region = paths.region(body_key, ChunkKey::new(0, 0, 0));
    let orphan_region = paths.region(orphan, ChunkKey::new(0, 0, 0));
    std::fs::copy(&source_region, &orphan_region).expect("复制孤儿 region");
    let recovered = PathBuf::from(format!("{}.recovered", orphan_region.display()));
    std::fs::copy(&source_region, &recovered).expect("复制 recovered");
    let tmp = PathBuf::from(format!("{}.tmp", orphan_region.display()));
    std::fs::write(&tmp, b"garbage").expect("写 tmp");

    let mut loaded = test_world(1);
    let result = load_world(&mut loaded, &paths, 1)
        .expect("孤儿 region 不是错误")
        .expect("world.meta 存在");

    assert_eq!(
        result.stats.orphan_regions, 1,
        "主文件与 recovered 只算一个"
    );
    assert_eq!(result.stats.bodies, 1);
    assert_eq!(result.stats.chunks, 1);
    assert!(orphan_region.exists(), "孤儿 region 不能被删");
    assert!(recovered.exists(), "孤儿 recovered 不能被删");
}

const TEST_PARAM_SPECS: &[ParamSpec] = &[
    ParamSpec {
        key: "flag",
        ty: ParamType::Bool,
        default: 0,
        scope: ParamScope::World,
        group: "test",
        visible_if: None,
    },
    ParamSpec {
        key: "ratio",
        ty: ParamType::Fixed {
            scale: 1000,
            min: 0,
            max: 10_000,
            step: 10,
        },
        default: 1000,
        scope: ParamScope::World,
        group: "test",
        visible_if: None,
    },
];

static TEST_MANIFEST: ModManifest = ModManifest {
    id: "test",
    version: 7,
    engine_api: 1,
    load_after: &[],
    load_after_optional: &[],
    conflicts: &[],
    exclusive_groups: &[],
    params: TEST_PARAM_SPECS,
    actions: &[],
};

#[test]
fn load_applies_saved_world_params_and_mod_records() {
    let overrides = BTreeMap::from([
        ("test.flag".to_string(), 1),
        ("test.ratio".to_string(), 2500),
    ]);
    let mut source = test_world(0xABCD);
    source.insert_resource(ParamTable::resolve(&[&TEST_MANIFEST], &overrides).expect("解析参数表"));

    let capture = capture_world(&mut source, 1, 1).expect("采集成功");
    assert_eq!(
        capture.meta.mods,
        vec![ModRecord {
            id: "test".to_string(),
            version: 7,
        }]
    );
    assert_eq!(capture.meta.params.len(), 2);

    let dir = TempDir::new("params");
    let paths = SavePaths::new(dir.path());
    write_capture(&paths, &capture).expect("落盘成功");

    let mut loaded = test_world(1);
    loaded.insert_resource(
        ParamTable::resolve(&[&TEST_MANIFEST], &BTreeMap::new()).expect("解析默认参数表"),
    );
    loaded.insert_resource(applied_mod_set(&[("test", 7)]));
    load_world(&mut loaded, &paths, 1)
        .expect("读档成功")
        .expect("world.meta 存在");

    let table = loaded.resource::<ParamTable>();
    assert_eq!(table.get("test.flag").map(|entry| entry.value), Some(1));
    assert_eq!(
        table.get("test.ratio").map(|entry| entry.value),
        Some(2500),
        "存档里的世界参数必须覆盖默认值"
    );
    assert_eq!(
        table.mods().collect::<Vec<_>>(),
        vec![("test", 7)],
        "mod 清单必须随存档往返"
    );
}

#[test]
fn load_rejects_unknown_saved_param_key() {
    let overrides = BTreeMap::from([("test.flag".to_string(), 1)]);
    let mut source = test_world(1);
    source.insert_resource(ParamTable::resolve(&[&TEST_MANIFEST], &overrides).expect("解析参数表"));
    let capture = capture_world(&mut source, 1, 1).expect("采集成功");
    let dir = TempDir::new("params_unknown");
    let paths = SavePaths::new(dir.path());
    write_capture(&paths, &capture).expect("落盘成功");

    let mut meta = capture.meta.clone();
    meta.params[1].key = "test.zzz".to_string();
    write_world_meta(&paths, &meta).expect("写 meta");

    let mut loaded = test_world(1);
    loaded.insert_resource(
        ParamTable::resolve(&[&TEST_MANIFEST], &BTreeMap::new()).expect("解析默认参数表"),
    );
    loaded.insert_resource(applied_mod_set(&[("test", 7)]));
    let error = load_world(&mut loaded, &paths, 1).expect_err("未知参数 key 必须拒绝整档");
    assert!(
        error.to_string().contains("未知参数"),
        "错误应指出未知 key，实际：{error}"
    );
}

/// 未发布前不做旧档兼容：meta 缺动作表段（动作表写进 meta 之前的 v2）必须拒绝，
/// 否则 mod 动作位无法还原、读档结果会静默漂移。
#[test]
fn meta_without_action_table_is_rejected() {
    let mut source = test_world(1);
    let capture = capture_world(&mut source, 1, 1).expect("采集成功");
    assert!(
        capture.meta.actions.is_empty(),
        "没有 ResolvedActions 时采集到空表"
    );
    let dir = TempDir::new("broken_actions");
    let paths = SavePaths::new(dir.path());
    write_world_meta(&paths, &capture.meta).expect("写 meta");

    let mut loaded = test_world(1);
    loaded.insert_resource(
        ResolvedActions::resolve(CoreActions::table(), &[]).expect("核心动作表解析"),
    );
    let error = load_world(&mut loaded, &paths, 1).expect_err("缺动作表必须拒绝");
    assert!(
        error.to_string().contains("动作"),
        "错误应指向动作表，实际：{error}"
    );
}

/// 计划的输出顺序是确定性契约：body 按 BodyKey 升序、每个 body 的 region
/// 按 region 键升序，与插入顺序 / 磁盘枚举顺序无关。
#[test]
fn build_plan_orders_bodies_and_regions() {
    let mut source = test_world(7);
    let mut expected: Vec<BodyKey> = Vec::new();
    for index in 0..3u8 {
        let drift = Vec3F::new(
            FixedPoint::from_num(i64::from(index)),
            FixedPoint::ZERO,
            FixedPoint::ZERO,
        );
        let (_, body_key) = spawn_island_with_chunks(
            &mut source,
            drift,
            index % 2 == 0,
            BiomeId(index),
            &[ChunkKey::new(0, 0, 0), ChunkKey::new(4, 0, 0)],
        );
        expected.push(body_key);
    }

    let capture = capture_world(&mut source, 1, 1).expect("采集成功");
    let dir = TempDir::new("plan_order");
    let paths = SavePaths::new(dir.path());
    write_capture(&paths, &capture).expect("落盘成功");

    let meta = read_world_meta(&paths)
        .expect("读 meta")
        .expect("world.meta 存在");
    let reader = test_world(1);
    let plan = build_plan(&reader, &paths, meta, 1).expect("建计划成功");

    expected.sort_unstable();
    let keys: Vec<BodyKey> = plan.bodies.iter().map(|body| body.key).collect();
    assert_eq!(keys, expected, "body 必须按 BodyKey 升序");
    assert_eq!(plan.chunk_count, expected.len() * 2, "每个 body 两块");

    for body in &plan.bodies {
        let regions: Vec<ChunkKey> = body
            .regions
            .iter()
            .map(|file| region_of(*file.chunks.keys().next().expect("region 非空")))
            .collect();
        let mut sorted = regions.clone();
        sorted.sort_unstable();
        assert_eq!(regions, sorted, "region 必须按 region 键升序");
        assert_eq!(regions.len(), 2, "每个 body 两个 region");
    }
}

// ---- mod 集合、动作表与插件序列的读档硬校验 ----

use crate::input::CoreActions;
use crate::world::ActivePlugins;

/// 读档只读校验的指纹：种子、稳定 ID、body 集合、区块内容哈希。
#[derive(Debug, PartialEq, Eq)]
struct WorldFingerprint {
    seed: u64,
    stable_ids: BTreeSet<u64>,
    bodies: BTreeSet<u64>,
    chunks: BTreeMap<(u64, ChunkKey), u64>,
}

fn fingerprint(world: &World) -> WorldFingerprint {
    WorldFingerprint {
        seed: world.resource::<WorldSeed>().0,
        stable_ids: stable_ids(world),
        bodies: body_ids(world),
        chunks: chunk_hashes(world),
    }
}

/// 世界里先放一个实体，用来验证校验失败时它不会被改动。
fn spawn_marker(world: &mut World, id: u64) {
    world
        .spawn(TestMarker {
            value: 1,
            label: "keep".to_string(),
        })
        .insert(StableEntityId(id));
}

fn applied_mod_set(mods: &[(&str, u32)]) -> AppliedModSet {
    AppliedModSet {
        mod_set_hash: 0x5EED,
        mods: mods
            .iter()
            .map(|(id, version)| ModRef {
                id: (*id).to_string(),
                version: *version,
            })
            .collect(),
    }
}

fn resolved_actions(mods: &'static [(&'static str, &'static [&'static str])]) -> ResolvedActions {
    ResolvedActions::resolve(CoreActions::table(), mods).expect("测试动作表应能解析")
}

#[test]
fn load_rejects_changed_mod_set() {
    let mut source = test_world(0x51);
    source.insert_resource(
        ParamTable::resolve(&[&TEST_MANIFEST], &BTreeMap::new()).expect("解析默认参数表"),
    );
    let capture = capture_world(&mut source, 1, 1).expect("采集成功");
    assert_eq!(
        capture.meta.mods,
        vec![ModRecord {
            id: "test".to_string(),
            version: 7,
        }]
    );

    let dir = TempDir::new("mod_drift");
    let paths = SavePaths::new(dir.path());
    write_capture(&paths, &capture).expect("落盘成功");

    // 1) 存档加载过 mod，当前装配里没有。
    let mut loaded = test_world(1);
    spawn_marker(&mut loaded, 42);
    loaded.insert_resource(applied_mod_set(&[]));
    let before = fingerprint(&loaded);
    let error = load_world(&mut loaded, &paths, 1).expect_err("缺少 mod 必须拒绝整档");
    assert!(
        error.to_string().contains("当前装配里没有"),
        "错误应指出缺失的 mod，实际：{error}"
    );
    assert_eq!(fingerprint(&loaded), before, "校验失败时 world 不得改动");

    // 2) 同名 mod 版本不一致。
    let mut loaded = test_world(1);
    spawn_marker(&mut loaded, 42);
    loaded.insert_resource(applied_mod_set(&[("test", 8)]));
    let before = fingerprint(&loaded);
    let error = load_world(&mut loaded, &paths, 1).expect_err("版本不一致必须拒绝整档");
    assert!(
        error.to_string().contains("版本不一致"),
        "错误应指出版本不一致，实际：{error}"
    );
    assert_eq!(fingerprint(&loaded), before, "校验失败时 world 不得改动");

    // 3) 当前装配多出存档没记录过的 mod。
    let mut loaded = test_world(1);
    spawn_marker(&mut loaded, 42);
    loaded.insert_resource(applied_mod_set(&[("test", 7), ("ghost", 1)]));
    let before = fingerprint(&loaded);
    let error = load_world(&mut loaded, &paths, 1).expect_err("多出 mod 必须拒绝整档");
    assert!(
        error.to_string().contains("存档没有记录"),
        "错误应指出多出的 mod，实际：{error}"
    );
    assert_eq!(fingerprint(&loaded), before, "校验失败时 world 不得改动");
}

#[test]
fn load_accepts_matching_mod_set_and_action_table() {
    const MODS: &[(&str, &[&str])] = &[("test", &["mod.build"])];
    let mut source = test_world(0x52);
    source.insert_resource(
        ParamTable::resolve(&[&TEST_MANIFEST], &BTreeMap::new()).expect("解析默认参数表"),
    );
    source.insert_resource(resolved_actions(MODS));
    let capture = capture_world(&mut source, 1, 1).expect("采集成功");
    assert_eq!(capture.meta.mods.len(), 1);
    assert_eq!(
        capture.meta.actions.len(),
        CoreActions::table().len() + 1,
        "core 动作 + 1 个 mod 动作都要进 meta"
    );
    assert!(
        capture
            .meta
            .actions
            .windows(2)
            .all(|pair| pair[0].name < pair[1].name),
        "meta.actions 必须按 name 严格升序"
    );

    let dir = TempDir::new("mod_match");
    let paths = SavePaths::new(dir.path());
    write_capture(&paths, &capture).expect("落盘成功");

    let mut loaded = test_world(1);
    loaded.insert_resource(
        ParamTable::resolve(&[&TEST_MANIFEST], &BTreeMap::new()).expect("解析默认参数表"),
    );
    loaded.insert_resource(applied_mod_set(&[("test", 7)]));
    loaded.insert_resource(resolved_actions(MODS));
    load_world(&mut loaded, &paths, 1)
        .expect("一致的 mod 集合与动作表必须放行")
        .expect("world.meta 存在");
}

#[test]
fn load_rejects_action_table_drift() {
    const MODS: &[(&str, &[&str])] = &[("test", &["mod.build"])];
    let mut source = test_world(0x53);
    source.insert_resource(resolved_actions(MODS));
    let capture = capture_world(&mut source, 1, 1).expect("采集成功");
    assert!(!capture.meta.actions.is_empty());

    let dir = TempDir::new("action_drift");
    let paths = SavePaths::new(dir.path());
    write_capture(&paths, &capture).expect("落盘成功");

    // 1) 条数不一致（当前表没有 mod 动作）。
    let mut loaded = test_world(1);
    spawn_marker(&mut loaded, 42);
    loaded.insert_resource(resolved_actions(&[]));
    let before = fingerprint(&loaded);
    let error = load_world(&mut loaded, &paths, 1).expect_err("动作表条数不一致必须拒绝");
    assert!(
        error.to_string().contains("动作表条数不一致"),
        "错误应指出条数不一致，实际：{error}"
    );
    assert_eq!(fingerprint(&loaded), before, "校验失败时 world 不得改动");

    // 2) 条数相同但名字不同。
    let mut loaded = test_world(1);
    spawn_marker(&mut loaded, 42);
    loaded.insert_resource(resolved_actions(&[("test", &["mod.other"])]));
    let before = fingerprint(&loaded);
    let error = load_world(&mut loaded, &paths, 1).expect_err("动作表内容不一致必须拒绝");
    assert!(
        error.to_string().contains("动作表记录与存档不一致"),
        "错误应指出具体动作，实际：{error}"
    );
    assert_eq!(fingerprint(&loaded), before, "校验失败时 world 不得改动");

    // 3) 存档带动作表但世界没有运行期动作表。
    let mut loaded = test_world(1);
    spawn_marker(&mut loaded, 42);
    let before = fingerprint(&loaded);
    let error = load_world(&mut loaded, &paths, 1).expect_err("缺少动作表资源必须拒绝");
    assert!(
        error.to_string().contains("ResolvedActions"),
        "错误应指出缺失的资源，实际：{error}"
    );
    assert_eq!(fingerprint(&loaded), before, "校验失败时 world 不得改动");
}

// ---- 插件装配序列的读档硬校验 ----

#[test]
fn load_rejects_plugin_drift() {
    let mut source = test_world(0x54);
    source.insert_resource(resolved_actions(&[]));
    source.insert_resource(ActivePlugins::new(vec!["core.world", "core.terrain"]));
    let capture = capture_world(&mut source, 1, 1).expect("采集成功");
    assert_eq!(
        capture.meta.plugins,
        vec!["core.world".to_string(), "core.terrain".to_string()],
        "装配顺序必须原样进 meta"
    );

    let dir = TempDir::new("plugin_drift");
    let paths = SavePaths::new(dir.path());
    write_capture(&paths, &capture).expect("落盘成功");

    // 1) 集合相同但顺序不同。
    let mut loaded = test_world(1);
    spawn_marker(&mut loaded, 42);
    loaded.insert_resource(resolved_actions(&[]));
    loaded.insert_resource(ActivePlugins::new(vec!["core.terrain", "core.world"]));
    let before = fingerprint(&loaded);
    let error = load_world(&mut loaded, &paths, 1).expect_err("顺序不一致必须拒绝整档");
    assert!(
        error.to_string().contains("装配顺序"),
        "错误应指出装配顺序，实际：{error}"
    );
    assert_eq!(fingerprint(&loaded), before, "校验失败时 world 不得改动");

    // 2) 当前装配少了存档记录的插件。
    let mut loaded = test_world(1);
    spawn_marker(&mut loaded, 42);
    loaded.insert_resource(resolved_actions(&[]));
    loaded.insert_resource(ActivePlugins::new(vec!["core.world"]));
    let before = fingerprint(&loaded);
    let error = load_world(&mut loaded, &paths, 1).expect_err("缺少插件必须拒绝整档");
    assert!(
        error.to_string().contains("当前注册表里没有"),
        "错误应指出缺失的插件，实际：{error}"
    );
    assert_eq!(fingerprint(&loaded), before, "校验失败时 world 不得改动");

    // 3) 当前装配多出存档没记录过的插件。
    let mut loaded = test_world(1);
    spawn_marker(&mut loaded, 42);
    loaded.insert_resource(resolved_actions(&[]));
    loaded.insert_resource(ActivePlugins::new(vec![
        "core.world",
        "core.terrain",
        "core.extra",
    ]));
    let before = fingerprint(&loaded);
    let error = load_world(&mut loaded, &paths, 1).expect_err("多出插件必须拒绝整档");
    assert!(
        error.to_string().contains("存档没有记录"),
        "错误应指出多出的插件，实际：{error}"
    );
    assert_eq!(fingerprint(&loaded), before, "校验失败时 world 不得改动");

    // 4) 世界根本没跑注册表，无法核对装配序列。
    let mut loaded = test_world(1);
    spawn_marker(&mut loaded, 42);
    loaded.insert_resource(resolved_actions(&[]));
    let before = fingerprint(&loaded);
    let error = load_world(&mut loaded, &paths, 1).expect_err("缺少 ActivePlugins 必须拒绝");
    assert!(
        error.to_string().contains("ActivePlugins"),
        "错误应指出缺失的资源，实际：{error}"
    );
    assert_eq!(fingerprint(&loaded), before, "校验失败时 world 不得改动");
}

#[test]
fn load_accepts_matching_plugins() {
    let mut source = test_world(0x55);
    source.insert_resource(resolved_actions(&[]));
    source.insert_resource(ActivePlugins::new(vec!["core.world", "core.terrain"]));
    let capture = capture_world(&mut source, 1, 1).expect("采集成功");

    let dir = TempDir::new("plugin_match");
    let paths = SavePaths::new(dir.path());
    write_capture(&paths, &capture).expect("落盘成功");

    let mut loaded = test_world(1);
    loaded.insert_resource(resolved_actions(&[]));
    loaded.insert_resource(ActivePlugins::new(vec!["core.world", "core.terrain"]));
    load_world(&mut loaded, &paths, 1)
        .expect("一致的插件序列必须放行")
        .expect("world.meta 存在");
}
