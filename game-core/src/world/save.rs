//! 世界级存档采集与落盘（§2 `world.meta` + §3 region 文件的**编排层**）。
//!
//! 本模块只做「世界 -> [`WorldCapture`] -> 磁盘」的接线：字节编解码在
//! `game_engine::save`（format / codec / store），实体快照在
//! `game_engine::save::snapshot`，块内容在 `game_engine::save::content`。
//!
//! 两条铁律：
//!
//! 1. **世界级顺序**：先写全部 region，全部成功后才写 `world.meta`。中途崩溃最多
//!    留下孤儿 region 文件（下次覆盖 / 清理），绝不会出现「meta 指向不存在的 region」。
//! 2. **不静默丢数据**：缺资源、缺 `Body` 语义、`attach_to` 悬空、快照指向不存在的
//!    body …… 一律返回 [`SaveError`]，不跳过、不补默认值。
//!
//! 确定性（§8）：判定 / 分组只用 `BTreeMap` / `BTreeSet`，body 键与块键天然升序；
//! 定点量走 `FixedPoint::to_bits` 的**位模式**原样写盘，任何路径上都没有 f32 / f64。
//!
//! # 采集规则（与实现一一对应）
//!
//! | 来源 | kind | attach | attach_to | biome / anchored / drift |
//! | --- | --- | --- | --- | --- |
//! | `IslandRegistry` 命中（权威） | `Island` | `Free` | `INVALID` | 注册表 |
//! | `Body::island` 但注册表没条目 | `Island` | `Free` | `INVALID` | `Body` 的 anchored / biome，drift 全 0 |
//! | `Body::ship` | `Ship` | `Free` | `INVALID` | 全 0 |
//! | `Body::structure{attach}` | `Structure` | `Attachment` 映射 | `Attachment::Body` -> 目标 body，其余 `INVALID` | 全 0 |
//! | `Body::automaton` | `Automaton` | `Free` | `INVALID` | 全 0 |
//!
//! **偏离说明（岛不写 `attach = World`）**：任务书要求岛写 `World`，但存档规格 §2
//! 规定「attach_tag 非 2（Free）只有结构可用」，且 `encode_world_meta` 在写侧硬校验
//! （`game-engine/src/save/codec.rs` 的 "attach_tag on non-Structure body"）——写
//! `World` 会让每个带岛的世界都无法落盘。因此岛按 §2 写 `Free`，`attach_to` 仍
//! `INVALID`；岛的语义（anchored / biome / drift）完整保留，不丢数据。

use std::collections::{BTreeMap, BTreeSet};

use bevy::prelude::{Entity, World};
use game_engine::body_key::BodyKey;
use game_engine::identity::{StableEntityId, StableIdAllocator};
use game_engine::save::{
    capture as capture_snapshot, export_chunk, region_of, write_region, write_world_meta, AttachTag,
    BodyKind as SaveBodyKind, BodyRecord, ChunkRecord, OpaqueComponents, PersistRegistry, RegionFile,
    SaveError, SaveErrorKind, SavePaths, WorldMeta,
};
use game_engine::voxel::{Attachment, ChunkKey, VoxVolume, VoxelInterner};

use crate::voxel::body::{Body, BodyKind};
use crate::world::{IslandId, IslandRegistry, WorldSeed};

/// 一次**世界级**采集结果：一份 `world.meta` 的语义 + 按 (body, region) 分组的 region 文件。
///
/// `regions` 的键是 **(body, region 键)**，不是块键：同一个 body 的同一 region 里
/// 的所有块合并在一个 [`RegionFile::chunks`] 里（§3 一个文件一个 region）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorldCapture {
    /// 世界元数据（body 表按 `BodyKey` 升序）。
    pub meta: WorldMeta,
    /// (body, region 键) -> region 文件；键升序，落盘顺序即此顺序。
    pub regions: BTreeMap<(BodyKey, ChunkKey), RegionFile>,
}

/// 采集当前世界：`world.meta` + 全部 region。
///
/// 要求 `PersistRegistry` / `OpaqueComponents`（`PersistPlugin` 注册）、
/// `IslandRegistry` / `WorldSeed`（`WorldPlugin` 注册）、`VoxelInterner`
/// （`VoxelPlugin` 注册）、`StableIdAllocator`（`StableIdPlugin` 注册）都已就位；
/// 缺任何一个都返回 Err，绝不静默跳过（跳过等于丢数据）。
///
/// body 集合 = 同时带 `StableEntityId` 与 `VoxVolume` 的实体；块集合 = 该 body
/// 的 `VoxVolume.chunks` ∪ 快照里属于它的块（后者可能是「只有实体、没有体素」的块）。
pub fn capture_world(
    world: &mut World,
    world_tick: u64,
    generator_version: u32,
) -> Result<WorldCapture, SaveError> {
    // 查询状态要在 `&mut World` 上建；建好之后整个采集过程只读。
    let mut query =
        world.query_filtered::<(Entity, &StableEntityId, &VoxVolume, Option<&Body>), ()>();
    let world: &World = &*world;

    // 实体快照（Persist 实体）：缺注册表 / 不透明组件直接报错。
    let snapshot = {
        let registry = world
            .get_resource::<PersistRegistry>()
            .ok_or_else(|| missing_resource("PersistRegistry"))?;
        let opaque = world
            .get_resource::<OpaqueComponents>()
            .ok_or_else(|| missing_resource("OpaqueComponents"))?;
        capture_snapshot(world, registry, opaque)?
    };

    let islands = world
        .get_resource::<IslandRegistry>()
        .ok_or_else(|| missing_resource("IslandRegistry"))?;
    let interner = world
        .get_resource::<VoxelInterner>()
        .ok_or_else(|| missing_resource("VoxelInterner"))?;
    let seed = world
        .get_resource::<WorldSeed>()
        .ok_or_else(|| missing_resource("WorldSeed"))?
        .0;
    let next_stable_entity_id = world
        .get_resource::<StableIdAllocator>()
        .ok_or_else(|| missing_resource("StableIdAllocator"))?
        .next_id();

    // body 表：BTreeMap 保证按 BodyKey 严格升序，且重复稳定 ID 会被发现。
    let mut bodies: BTreeMap<BodyKey, (Entity, BodyRecord)> = BTreeMap::new();
    for (entity, stable, volume, body) in query.iter(world) {
        let key = BodyKey::new(*stable);
        let record = body_record(entity, key, volume, body, islands)?;
        if bodies.insert(key, (entity, record)).is_some() {
            return Err(SaveError::semantic(
                format!("capture.body[{key:?}]"),
                SaveErrorKind::Duplicate {
                    what: "capture body StableEntityId",
                    id: key.as_u64(),
                },
            ));
        }
    }

    // 快照里每个 body 的块集合：先按 body 归档，便于和体素的块做并集。
    let mut snapshot_chunks: BTreeMap<BodyKey, BTreeSet<ChunkKey>> = BTreeMap::new();
    for ((body_key, chunk_key), _) in snapshot.chunks() {
        snapshot_chunks
            .entry(body_key)
            .or_default()
            .insert(chunk_key);
    }

    // 快照指向的 body 必须存在：否则这些实体在落盘时会凭空消失。
    for body_key in snapshot_chunks.keys() {
        if !bodies.contains_key(body_key) {
            return Err(SaveError::semantic(
                format!("capture.snapshot[{body_key:?}]"),
                SaveErrorKind::Missing {
                    what: "body entity (StableEntityId + VoxVolume) for persisted entity",
                    id: body_key.as_u64(),
                },
            ));
        }
    }

    // attach_to 必须引用 body 表里的另一个 body（§2）。
    for (body_key, (_, record)) in &bodies {
        if record.attach == AttachTag::Body && !bodies.contains_key(&record.attach_to) {
            return Err(SaveError::semantic(
                format!("capture.body[{body_key:?}].attach_to"),
                SaveErrorKind::Missing {
                    what: "attach_to body (structure attached to a body outside the save)",
                    id: record.attach_to.as_u64(),
                },
            ));
        }
    }

    // region：同 (body, region) 的块合并进一个文件。
    let mut regions: BTreeMap<(BodyKey, ChunkKey), RegionFile> = BTreeMap::new();
    for (body_key, (entity, _)) in &bodies {
        let Some(volume) = world.get::<VoxVolume>(*entity) else {
            return Err(SaveError::semantic(
                format!("capture.body[{body_key:?}].VoxVolume"),
                SaveErrorKind::Missing {
                    what: "VoxVolume component for captured body",
                    id: body_key.as_u64(),
                },
            ));
        };

        let mut chunk_keys: BTreeSet<ChunkKey> = volume.chunks.keys().copied().collect();
        if let Some(extra) = snapshot_chunks.get(body_key) {
            chunk_keys.extend(extra.iter().copied());
        }

        for chunk_key in chunk_keys {
            let content = export_chunk(volume, chunk_key, interner.inner());
            let entities = snapshot.for_chunk(*body_key, chunk_key).to_vec();
            // 空记录不得写入（§3.1）：没有体素内容也没有实体就整条丢掉。
            if content.is_none() && entities.is_empty() {
                continue;
            }
            let record = ChunkRecord {
                generator_version,
                content,
                proposals: Vec::new(),
                entities,
            };
            regions
                .entry((*body_key, region_of(chunk_key)))
                .or_default()
                .chunks
                .insert(chunk_key, record);
        }
    }

    let mut body_table: BTreeMap<BodyKey, BodyRecord> = BTreeMap::new();
    for (key, (_, record)) in bodies {
        body_table.insert(key, record);
    }

    Ok(WorldCapture {
        meta: WorldMeta {
            seed,
            world_tick,
            next_stable_entity_id,
            bodies: body_table,
        },
        regions,
    })
}

/// 落盘：**先**按 `BTreeMap` 顺序写完所有 region，**全部成功**后才写 `world.meta`。
///
/// 崩溃一致性（§5）：region 先落、meta 后落，所以磁盘上永远不会出现「meta 指向
/// 不存在的 region」；最坏情况是孤儿 region 文件，可被下次写入覆盖。
///
/// **region 键的口径**：`regions` 的键是 §1 的 region 键（逐轴 `div_euclid(4)`），
/// 而 `store::write_region` 的第三个参数吃的是**块键**——它自己再 `region_of` 一次
/// 得到文件名。所以这里传该 region 里最小的块键（`region_of(块键) == 键`，文件名
/// 恰好是 region 键）；直接传 region 键会被再除一次，region 索引 ≥ 2 时落错文件。
pub fn write_capture(paths: &SavePaths, capture: &WorldCapture) -> Result<(), SaveError> {
    for ((body, region_key), file) in &capture.regions {
        let Some(anchor) = file.chunks.keys().next().copied() else {
            return Err(SaveError::semantic(
                format!("write_capture.region[{body:?},{region_key:?}]"),
                SaveErrorKind::InvalidCombination {
                    what: "empty RegionFile in WorldCapture (采集不会产生空 region)",
                },
            ));
        };
        debug_assert_eq!(
            region_of(anchor),
            *region_key,
            "WorldCapture 的 region 键必须是其块键的 region_of"
        );
        write_region(paths, *body, anchor, file)?;
    }
    write_world_meta(paths, &capture.meta)?;
    Ok(())
}

/// 缺资源：`what` 里写清是哪个 Resource，`path` 定位到采集阶段。
fn missing_resource(what: &'static str) -> SaveError {
    SaveError::semantic(
        format!("capture.resources.{what}"),
        SaveErrorKind::Missing {
            what: "required resource for world capture",
            id: 0,
        },
    )
}

/// 单个 body 的 [`BodyRecord`]。
///
/// 岛的语义权威是 [`IslandRegistry`]（命中优先）；没命中就必须有 `Body` 组件，
/// 否则返回 Err —— 一个带稳定 ID 和体素、却没有语义的 body 不能猜。
fn body_record(
    entity: Entity,
    key: BodyKey,
    volume: &VoxVolume,
    body: Option<&Body>,
    islands: &IslandRegistry,
) -> Result<BodyRecord, SaveError> {
    let voxel_size = volume.voxel_size.to_bits();
    if voxel_size <= 0 {
        return Err(SaveError::semantic(
            format!("capture.body[{entity:?}].voxel_size"),
            SaveErrorKind::InvalidValue {
                what: "body voxel_size (must be positive)",
                value: voxel_size,
            },
        ));
    }

    // 注册表命中：岛的一切语义以注册表为准（drift / anchored / biome）。
    if let Some(island) = islands.get(IslandId(key.as_u64())) {
        return Ok(BodyRecord {
            id: key,
            kind: SaveBodyKind::Island,
            // §2：非 Structure 的 attach_tag 必须是 Free（写侧 codec 会拒绝 World）。
            attach: AttachTag::Free,
            biome: island.biome.0,
            anchored: u8::from(island.anchored),
            voxel_size,
            drift: [
                island.drift.x.to_bits(),
                island.drift.y.to_bits(),
                island.drift.z.to_bits(),
            ],
            attach_to: BodyKey::INVALID,
        });
    }

    let Some(body) = body else {
        return Err(SaveError::semantic(
            format!("capture.body[{entity:?}]"),
            SaveErrorKind::Missing {
                what: "Body component for voxel body outside IslandRegistry",
                id: key.as_u64(),
            },
        ));
    };

    let (kind, attach, attach_to, biome, anchored, drift) = match body.kind {
        // 注册表没条目但组件说是岛：组件是唯一真相，drift 没有来源，写 0。
        BodyKind::Island { anchored, biome } => (
            SaveBodyKind::Island,
            AttachTag::Free,
            BodyKey::INVALID,
            biome.0,
            u8::from(anchored),
            [0_i64; 3],
        ),
        BodyKind::Ship => (
            SaveBodyKind::Ship,
            AttachTag::Free,
            BodyKey::INVALID,
            0,
            0,
            [0_i64; 3],
        ),
        BodyKind::Structure { attach } => {
            let (tag, target) = match attach {
                Attachment::World => (AttachTag::World, BodyKey::INVALID),
                Attachment::Body(stable) => (AttachTag::Body, BodyKey::new(stable)),
                Attachment::Free => (AttachTag::Free, BodyKey::INVALID),
            };
            if tag == AttachTag::Body && !target.is_valid() {
                return Err(SaveError::semantic(
                    format!("capture.body[{entity:?}].attach"),
                    SaveErrorKind::InvalidValue {
                        what: "structure attach body",
                        value: 0,
                    },
                ));
            }
            (
                SaveBodyKind::Structure,
                tag,
                target,
                0,
                0,
                [0_i64; 3],
            )
        }
        BodyKind::Automaton => (
            SaveBodyKind::Automaton,
            AttachTag::Free,
            BodyKey::INVALID,
            0,
            0,
            [0_i64; 3],
        ),
    };

    Ok(BodyRecord {
        id: key,
        kind,
        attach,
        biome,
        anchored,
        voxel_size,
        drift,
        attach_to,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use game_engine::identity::StableEntityIndex;
    use game_engine::math::{FixedPoint, Vec3F};
    use game_engine::residency::ChunkAnchor;
    use game_engine::save::{read_region, read_world_meta, Persist};
    use game_engine::voxel::{MaxDepth, VoxOpsBulkWrite, VoxTree, CHUNK_DEPTH};

    use crate::static_data::voxel::BiomeId;
    use crate::voxel::body::spawn_island_entity;

    /// 采集需要的全部资源：插件在真实 App 里注册它们，测试里手工插入。
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

    /// 造一个"实心"块并挂到 volume 上（用世界里的 interner，导出的字节才认得出节点）。
    fn insert_filled_chunk(world: &mut World, volume: &mut VoxVolume, key: ChunkKey, value: u8) {
        let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH));
        {
            let mut interner = world.resource_mut::<VoxelInterner>();
            tree.fill(interner.inner_mut(), value);
        }
        volume.insert_chunk(key, tree);
    }

    /// `std::env::temp_dir` 下的唯一子目录；Drop 时整棵删掉。
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
                "ecs_game_world_save_{tag}_{}_{nanos}_{}",
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

    /// 只挂一个带一个块的岛 body（岛走唯一生成入口）。
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
            insert_filled_chunk(world, &mut volume, *chunk, u8::try_from(index + 1).unwrap_or(1));
        }
        world.entity_mut(entity).insert(volume);
        (entity, BodyKey::from(island_id))
    }

    #[test]
    fn empty_world_captures_meta_only() {
        let mut world = test_world(7);
        world.resource_mut::<StableIdAllocator>().restore(41);

        let capture = capture_world(&mut world, 99, 3).expect("空世界也必须能采集");

        assert_eq!(
            capture.meta,
            WorldMeta {
                seed: 7,
                world_tick: 99,
                next_stable_entity_id: 41,
                bodies: BTreeMap::new(),
            }
        );
        assert!(capture.regions.is_empty(), "空世界不产生 region");
    }

    #[test]
    fn island_body_captures_record_and_chunks() {
        const TICK: u64 = 1234;
        const GENERATOR_VERSION: u32 = 9;

        let mut world = test_world(0x5EED);
        world.resource_mut::<StableIdAllocator>().restore(100);

        let drift = Vec3F::new(
            FixedPoint::from_num(3),
            FixedPoint::from_num(-4),
            FixedPoint::from_num(5),
        );
        let (island_entity, island_id) = spawn_island_entity(&mut world, drift, true, BiomeId(3));
        let body_key = BodyKey::from(island_id);

        let chunk_a = ChunkKey::new(0, 0, 0);
        let chunk_b = ChunkKey::new(1, 0, 0);
        // 只在快照里、不在 volume 里的块：内容必须是 None，但记录要留下。
        let chunk_c = ChunkKey::new(2, 0, 0);

        let mut volume = VoxVolume::new(FixedPoint::from_num(2));
        insert_filled_chunk(&mut world, &mut volume, chunk_a, 1);
        insert_filled_chunk(&mut world, &mut volume, chunk_b, 2);
        world.entity_mut(island_entity).insert(volume);

        let entity_in_a = world.resource_mut::<StableIdAllocator>().allocate();
        world.spawn((
            entity_in_a,
            Persist {
                prototype_type_id: 7,
            },
            ChunkAnchor::new(body_key, chunk_a),
        ));
        let entity_in_c = world.resource_mut::<StableIdAllocator>().allocate();
        world.spawn((
            entity_in_c,
            Persist {
                prototype_type_id: 8,
            },
            ChunkAnchor::new(body_key, chunk_c),
        ));

        let capture = capture_world(&mut world, TICK, GENERATOR_VERSION).expect("采集成功");

        assert_eq!(capture.meta.seed, 0x5EED);
        assert_eq!(capture.meta.world_tick, TICK);
        assert_eq!(capture.meta.next_stable_entity_id, 103, "岛 + 2 个实体");
        assert_eq!(capture.meta.bodies.len(), 1);

        let record = capture.meta.bodies.get(&body_key).expect("岛 body 必须在表里");
        assert_eq!(record.id, body_key);
        assert_eq!(record.kind, SaveBodyKind::Island);
        // §2：只有 Structure 可以 attach_tag != Free，岛写 Free（raw 编码会拒绝 World）。
        assert_eq!(record.attach, AttachTag::Free);
        assert_eq!(record.attach_to, BodyKey::INVALID);
        assert_eq!(record.anchored, 1);
        assert_eq!(record.biome, 3);
        assert_eq!(record.voxel_size, FixedPoint::from_num(2).to_bits());
        assert_eq!(
            record.drift,
            [drift.x.to_bits(), drift.y.to_bits(), drift.z.to_bits()],
            "drift 必须是定点位模式，不是浮点"
        );

        assert_eq!(capture.regions.len(), 1, "三个块同属一个 region");
        let region = capture
            .regions
            .get(&(body_key, region_of(chunk_a)))
            .expect("region 必须存在");
        let keys: Vec<ChunkKey> = region.chunks.keys().copied().collect();
        assert_eq!(keys, vec![chunk_a, chunk_b, chunk_c], "空块不出现、块键升序");

        let a = region.chunks.get(&chunk_a).expect("chunk_a");
        assert_eq!(a.generator_version, GENERATOR_VERSION);
        assert!(a.content.is_some(), "有体素的块必须带 content");
        assert_eq!(a.entities.len(), 1);
        assert_eq!(a.entities[0].stable_entity_id, entity_in_a.0);
        assert!(a.proposals.is_empty(), "采集不产生提案");

        let b = region.chunks.get(&chunk_b).expect("chunk_b");
        assert!(b.content.is_some());
        assert!(b.entities.is_empty(), "只有体素的块也要留记录");

        let c = region.chunks.get(&chunk_c).expect("只有实体的块必须留记录");
        assert!(c.content.is_none(), "块不在 volume 里就没有 content");
        assert_eq!(c.entities.len(), 1);
        assert_eq!(c.entities[0].stable_entity_id, entity_in_c.0);
    }

    #[test]
    fn capture_is_deterministic() {
        let mut world = test_world(11);
        let (_, body_key) = spawn_island_with_chunks(
            &mut world,
            Vec3F::new(
                FixedPoint::from_num(-1),
                FixedPoint::from_num(2),
                FixedPoint::from_num(0),
            ),
            false,
            BiomeId(4),
            &[ChunkKey::new(0, 0, 0), ChunkKey::new(0, 1, 0)],
        );
        let stable = world.resource_mut::<StableIdAllocator>().allocate();
        world.spawn((
            stable,
            Persist {
                prototype_type_id: 3,
            },
            ChunkAnchor::new(body_key, ChunkKey::new(0, 1, 0)),
        ));

        let first = capture_world(&mut world, 5, 1).expect("第一次采集");
        let second = capture_world(&mut world, 5, 1).expect("第二次采集");

        assert_eq!(first.meta, second.meta, "WorldMeta 必须逐字段相等");
        assert_eq!(first.regions, second.regions, "regions 必须逐字节相等");
        assert_eq!(first, second);
    }

    #[test]
    fn voxel_body_without_body_component_is_an_error() {
        let mut world = test_world(1);
        let stable = world.resource_mut::<StableIdAllocator>().allocate();
        let mut volume = VoxVolume::new(FixedPoint::from_num(1));
        insert_filled_chunk(&mut world, &mut volume, ChunkKey::ZERO, 1);
        // 有稳定 ID + 体素，但没有 Body，也不在 IslandRegistry 里。
        world.spawn((stable, volume));

        let err = capture_world(&mut world, 0, 1).expect_err("缺 Body 语义必须报错");

        match err.kind() {
            SaveErrorKind::Missing { what, id } => {
                assert!(what.contains("Body"), "错误里要写清缺什么：{what}");
                assert_eq!(*id, stable.0, "错误里要带上是哪个稳定 ID");
            }
            other => panic!("期望 SaveErrorKind::Missing，得到 {other:?}"),
        }
        assert!(
            err.path().contains("capture.body"),
            "路径要能定位到 body：{}",
            err.path()
        );
    }

    #[test]
    fn write_capture_round_trips_through_disk() {
        let mut world = test_world(0xABCD);
        let (_, body_key) = spawn_island_with_chunks(
            &mut world,
            Vec3F::new(FixedPoint::from_num(1), FixedPoint::from_num(1), FixedPoint::from_num(1)),
            true,
            BiomeId(2),
            &[ChunkKey::new(0, 0, 0), ChunkKey::new(4, 0, 0)],
        );
        let stable = world.resource_mut::<StableIdAllocator>().allocate();
        world.spawn((
            stable,
            Persist {
                prototype_type_id: 5,
            },
            ChunkAnchor::new(body_key, ChunkKey::new(0, 0, 0)),
        ));

        let capture = capture_world(&mut world, 8, 2).expect("采集成功");
        let dir = TempDir::new("round_trip");
        let paths = SavePaths::new(dir.path());

        write_capture(&paths, &capture).expect("落盘成功");

        assert!(paths.world_meta().exists(), "world.meta 必须存在");
        assert_eq!(
            read_world_meta(&paths).expect("读 meta"),
            Some(capture.meta.clone()),
            "meta 往返必须逐字段相等"
        );
        assert!(!capture.regions.is_empty());
        for ((body, region_key), file) in &capture.regions {
            // read_region 的键同样是"region 里的块键"（内部再 region_of）。
            let anchor = *file.chunks.keys().next().expect("采集不会产生空 region");
            assert_eq!(region_of(anchor), *region_key);
            assert_eq!(
                read_region(&paths, *body, anchor)
                    .expect("读 region")
                    .as_ref(),
                Some(file),
                "region ({body:?}, {region_key:?}) 往返必须逐字段相等"
            );
        }
    }

    #[test]
    fn chunks_in_different_regions_are_split() {
        let mut world = test_world(3);
        let chunk_low = ChunkKey::new(0, 0, 0);
        let chunk_high = ChunkKey::new(4, 0, 0);
        let (_, body_key) = spawn_island_with_chunks(
            &mut world,
            Vec3F::ZERO,
            true,
            BiomeId(1),
            &[chunk_low, chunk_high],
        );

        let capture = capture_world(&mut world, 1, 1).expect("采集成功");

        assert_eq!(region_of(chunk_low), ChunkKey::new(0, 0, 0));
        assert_eq!(region_of(chunk_high), ChunkKey::new(1, 0, 0));
        assert_eq!(capture.regions.len(), 2, "两个块必须落进两个 region");

        let low = capture
            .regions
            .get(&(body_key, ChunkKey::new(0, 0, 0)))
            .expect("低 region");
        let low_keys: Vec<ChunkKey> = low.chunks.keys().copied().collect();
        assert_eq!(low_keys, vec![chunk_low], "低 region 只含自己的块");

        let high = capture
            .regions
            .get(&(body_key, ChunkKey::new(1, 0, 0)))
            .expect("高 region");
        let high_keys: Vec<ChunkKey> = high.chunks.keys().copied().collect();
        assert_eq!(high_keys, vec![chunk_high], "高 region 只含自己的块");
    }

    #[test]
    fn missing_persist_registry_is_an_error() {
        let mut world = test_world(1);
        world.remove_resource::<PersistRegistry>();

        let err = capture_world(&mut world, 0, 1).expect_err("缺 PersistRegistry 必须报错");

        assert!(matches!(err.kind(), SaveErrorKind::Missing { .. }));
        assert!(
            err.path().contains("PersistRegistry"),
            "路径要写清缺的是哪个资源：{}",
            err.path()
        );
    }

    #[test]
    fn body_kinds_map_to_kind_and_attach_tags() {
        let mut world = test_world(1);
        let (_, island_key) = spawn_island_with_chunks(
            &mut world,
            Vec3F::ZERO,
            true,
            BiomeId(0),
            &[ChunkKey::ZERO],
        );

        let structure_stable = world.resource_mut::<StableIdAllocator>().allocate();
        world.spawn((
            structure_stable,
            Body::structure(Attachment::Body(island_key.stable_id())),
            VoxVolume::new(FixedPoint::from_num(1)),
        ));
        let ship_stable = world.resource_mut::<StableIdAllocator>().allocate();
        world.spawn((
            ship_stable,
            Body::ship(),
            VoxVolume::new(FixedPoint::from_num(1)),
        ));

        let capture = capture_world(&mut world, 0, 1).expect("采集成功");

        let structure = capture
            .meta
            .bodies
            .get(&BodyKey::new(structure_stable))
            .expect("结构 body");
        assert_eq!(structure.kind, SaveBodyKind::Structure);
        assert_eq!(structure.attach, AttachTag::Body);
        assert_eq!(structure.attach_to, island_key);

        let ship = capture
            .meta
            .bodies
            .get(&BodyKey::new(ship_stable))
            .expect("船 body");
        assert_eq!(ship.kind, SaveBodyKind::Ship);
        assert_eq!(ship.attach, AttachTag::Free);
        assert_eq!(ship.attach_to, BodyKey::INVALID);
        assert_eq!(ship.biome, 0);
        assert_eq!(ship.anchored, 0);
        assert_eq!(ship.drift, [0; 3]);
    }
}
