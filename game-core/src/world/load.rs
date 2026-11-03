//! 世界级读档：world.meta + region 文件 -> 新世界。
//!
//! 与 crate::world::save 对称：那边是「世界 -> 磁盘」，这边是「磁盘 -> 世界」。
//! 字节解码、文件定位、组件反序列化都在 game_engine::save；本模块只做编排与
//! 世界级校验。
//!
//! # 两阶段
//!
//! 1. 只读阶段：读 world.meta，枚举每个 body 的 region 文件，解码并校验全部
//!    语义（键、ID、attach、块内容可解码……）。任何失败都在碰 World 之前返回
//!    SaveError，世界一个实体都不多。
//! 2. 写入阶段：按 BodyKey 升序建 body、按块键升序装块、按块键升序应用实体，
//!    最后恢复发号器与种子。
//!
//! # 不静默
//!
//! meta 指向的 region 文件不存在、块键不在自己的 region、实体 ID 重复、attach_to
//! 悬空、next_stable_entity_id 不大于已出现的 ID …… 一律 Err。content = None
//! 的块是「留给生成器」的合法记录，不装载也不报错；孤儿 region（meta 里没有的
//! body 目录）不删、不报错，只计入 LoadStats::orphan_regions。
//!
//! 确定性（§8）：枚举 / 遍历只用 BTreeMap / BTreeSet；定点量走
//! FixedPoint::from_bits 的位模式，全程没有 f32 / f64。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use bevy::prelude::{warn, Entity, Resource, World};

use game_engine::body_key::BodyKey;
use game_engine::identity::{StableEntityId, StableEntityIndex, StableIdAllocator};
use game_engine::math::{FixedPoint, Vec3F};
use game_engine::residency::ChunkAnchor;
use game_engine::save::{
    install_chunk, read_region, read_world_meta, reclaim_orphans, region_of, regions_on_disk,
    AttachTag, BodyKind, BodyRecord, OpaqueComponents, PersistRegistry, ReclaimStats, RegionFile,
    SaveError, SaveErrorKind, SavePaths, WorldMeta, REGION_SIZE,
};
use game_engine::voxel::{
    dec_ref_recursive, Attachment, ChunkKey, VoxVolume, VoxelBox, VoxelDirtySet, VoxelInterner,
    DEFAULT_INTERNER_BUDGET_BYTES,
};

use crate::static_data::voxel::BiomeId;
use crate::voxel::body::{AutomatonTag, Body, IslandTag, ShipTag, StructureTag};
use crate::world::{Island, IslandId, IslandRegistry, WorldSeed};

/// 一次读档的统计数据。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadStats {
    /// 从 world.meta.bodies 重建的 body 数。
    pub bodies: usize,
    /// 读到的 ChunkRecord 数（含 content = None 的记录）。
    pub chunks: usize,
    /// 因 generator_version 不匹配而**丢弃内容**、但保留实体记录的块数。
    ///
    /// 这些块没有装进 [VoxVolume]，但记录与实体保留；下次被请求时由生成
    /// 路径重新生成，而不是把旧地形当成新地形。content = None 的块本来就要
    /// 生成，不算 stale。
    pub stale_chunks: usize,
    /// 应用成功的 EntityRecord 数。
    pub entities: usize,
    /// meta 里没有对应 body 的孤儿 region 文件数（按 region 键去重）。
    pub orphan_regions: usize,
}

/// 读档成功后的世界级结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadedWorld {
    /// world.meta.world_tick。
    pub world_tick: u64,
    /// 本次读档的统计。
    pub stats: LoadStats,
}

/// 从 paths 读一个世界；generator_version 是当前生成器的版本号。
///
/// 返回 Ok(None) 表示磁盘上没有 world.meta（新世界，调用方走生成器），
/// 此时 world 一点都不会被改动。其余错误都表示存档损坏或世界缺资源，
/// 且阶段一（只读校验）的错误保证 world 未被改动。
///
/// # 生成器版本过滤
///
/// [ChunkRecord](game_engine::streaming::ChunkRecord) 的 content 只对写入它的
/// 生成器版本有效。版本不等的块在**装载阶段**只丢弃内容（计入
/// [LoadStats::stale_chunks]），**保留记录里的实体**：实体是玩家造的东西，
/// 与地形版本无关。body 照建，内容留空，下次请求时走生成路径重新生成——
/// 不会把旧地形当成新地形，也不会因为版本不同就把块永久判死。
/// content = None 的块本来就要生成，不算 stale。
pub fn load_world(
    world: &mut World,
    paths: &SavePaths,
    generator_version: u32,
) -> Result<Option<LoadedWorld>, SaveError> {
    let Some(meta) = read_world_meta(paths)? else {
        return Ok(None);
    };

    // 阶段一：只读文件 + 全部校验；这一步不碰 world。
    let plan = build_plan(world, paths, meta, generator_version)?;

    // 阶段二：先确认所有必需资源都在，再开始写世界。
    require_resources(world)?;
    let body_entities = spawn_plan_bodies(world, &plan)?;
    let stale_chunks = install_plan_chunks(world, &plan, &body_entities, generator_version)?;
    let entities = apply_plan_entities(world, &plan)?;

    world
        .resource_mut::<StableIdAllocator>()
        .restore(plan.meta.next_stable_entity_id);
    world.resource_mut::<WorldSeed>().0 = plan.meta.seed;

    Ok(Some(LoadedWorld {
        world_tick: plan.meta.world_tick,
        stats: LoadStats {
            bodies: plan.bodies.len(),
            chunks: plan.chunk_count,
            stale_chunks,
            entities,
            orphan_regions: plan.orphan_regions,
        },
    }))
}

/// 回收孤儿 region：删掉 `world.meta.bodies` 里没有的 body 目录。
///
/// 只在读档**成功之后**由 [StreamingWorldPlugin](crate::world::StreamingWorldPlugin)
/// 按 [OrphanPolicy](crate::world::OrphanPolicy) 调用：读档失败时一份坏档旁边可能
/// 还有可以手工抢救的目录，这时候不该做破坏性操作。
///
/// 这里重新读一次 world.meta（几十字节），换取 [load_world] 签名不变——
/// 它的 14 个调用点没有理由为一个清理策略多传一个参数。
pub fn reclaim_orphan_regions(paths: &SavePaths) -> Result<ReclaimStats, SaveError> {
    let Some(meta) = read_world_meta(paths)? else {
        return Ok(ReclaimStats::default());
    };
    reclaim_orphans(paths, &meta)
}

/// 阶段一的产物：全部已解码、已校验的数据。
struct Plan {
    meta: WorldMeta,
    bodies: Vec<BodyPlan>,
    chunk_count: usize,
    orphan_regions: usize,
}

struct BodyPlan {
    key: BodyKey,
    record: BodyRecord,
    /// 该 body 的 region 文件，按 region 键升序。
    regions: Vec<RegionFile>,
}

fn build_plan(
    world: &World,
    paths: &SavePaths,
    meta: WorldMeta,
    generator_version: u32,
) -> Result<Plan, SaveError> {
    validate_meta(&meta)?;

    let mut seen_entities: BTreeSet<u64> = BTreeSet::new();
    let mut bodies: Vec<BodyPlan> = Vec::with_capacity(meta.bodies.len());
    let mut chunk_count = 0usize;

    for (body_key, record) in &meta.bodies {
        let mut regions: Vec<RegionFile> = Vec::new();
        for region_key in regions_on_disk(paths, *body_key)? {
            let anchor = region_anchor(*body_key, region_key)?;
            let file = read_region(paths, *body_key, anchor)?.ok_or_else(|| {
                region_error(
                    *body_key,
                    region_key,
                    SaveErrorKind::Missing {
                        what: "region file (meta body has no readable region)",
                        id: 0,
                    },
                )
            })?;

            for (chunk_key, chunk) in &file.chunks {
                if region_of(*chunk_key) != region_key {
                    return Err(chunk_error(
                        *body_key,
                        *chunk_key,
                        SaveErrorKind::InvalidCombination {
                            what: "chunk key does not belong to its region file",
                        },
                    ));
                }
                if !chunk.proposals.is_empty() {
                    debug_assert!(
                        chunk.content.is_none(),
                        "§3.1：content 有值时 proposals 必须为空"
                    );
                }
                for entity in &chunk.entities {
                    let id = entity.stable_entity_id;
                    if id == 0 {
                        return Err(chunk_error(
                            *body_key,
                            *chunk_key,
                            SaveErrorKind::InvalidValue {
                                what: "entity stable_entity_id",
                                value: 0,
                            },
                        ));
                    }
                    if !seen_entities.insert(id) {
                        return Err(chunk_error(
                            *body_key,
                            *chunk_key,
                            SaveErrorKind::Duplicate {
                                what: "entity stable_entity_id across regions",
                                id,
                            },
                        ));
                    }
                }
                chunk_count += 1;
            }

            regions.push(file);
        }
        bodies.push(BodyPlan {
            key: *body_key,
            record: *record,
            regions,
        });
    }

    // next_stable_entity_id 必须大于所有已经出现的稳定 ID（body 与实体）。
    // 空表时 next = 0 也是坏档：发号器从 1 开始。
    let max_seen = meta
        .bodies
        .keys()
        .map(|key| key.as_u64())
        .chain(seen_entities.iter().copied())
        .max()
        .unwrap_or(0);
    if meta.next_stable_entity_id <= max_seen {
        return Err(SaveError::semantic(
            "load.meta.next_stable_entity_id",
            SaveErrorKind::InvalidValue {
                what: "next_stable_entity_id (must exceed every loaded stable id)",
                value: i64::try_from(meta.next_stable_entity_id).unwrap_or(i64::MAX),
            },
        ));
    }

    // 块内容与实体负载也要在阶段一解码：否则 stage 2 装到一半才发现坏数据，
    // 世界已经被改。旧版本的内容不会被安装，所以也不在此解码：它本来就要丢弃，
    // 没有理由让一段无法解码的旧内容把整份存档判成坏档。
    validate_contents(&bodies, content_budget(world), generator_version)?;
    validate_entities(world, &bodies)?;
    let orphan_regions = count_orphan_regions(paths, &meta)?;

    Ok(Plan {
        meta,
        bodies,
        chunk_count,
        orphan_regions,
    })
}

/// 阶段一：校验 meta 自身（解码器已做的检查之外，世界级语义再查一遍）。
fn validate_meta(meta: &WorldMeta) -> Result<(), SaveError> {
    for (key, record) in &meta.bodies {
        if key.as_u64() == 0 {
            return Err(body_error(
                *key,
                SaveErrorKind::InvalidValue {
                    what: "body id",
                    value: 0,
                },
            ));
        }
        if record.id != *key {
            return Err(body_error(
                *key,
                SaveErrorKind::Inconsistent {
                    what: "body id (map key must equal BodyRecord::id)",
                    expected: key.as_u64(),
                    found: record.id.as_u64(),
                },
            ));
        }
        if record.voxel_size <= 0 {
            return Err(body_error(
                *key,
                SaveErrorKind::InvalidValue {
                    what: "voxel_size",
                    value: record.voxel_size,
                },
            ));
        }

        match record.attach {
            AttachTag::Body => {
                if !record.attach_to.is_valid() || record.attach_to == *key {
                    return Err(body_error(
                        *key,
                        SaveErrorKind::InvalidValue {
                            what: "attach_to (must be another body id)",
                            value: record.attach_to.as_u64() as i64,
                        },
                    ));
                }
                if !meta.bodies.contains_key(&record.attach_to) {
                    return Err(body_error(
                        *key,
                        SaveErrorKind::Missing {
                            what: "attach_to body (structure points outside the save)",
                            id: record.attach_to.as_u64(),
                        },
                    ));
                }
            }
            AttachTag::World | AttachTag::Free => {
                if record.attach_to.is_valid() {
                    return Err(body_error(
                        *key,
                        SaveErrorKind::Inconsistent {
                            what: "attach_to (must be 0 when attach is not Body)",
                            expected: 0,
                            found: record.attach_to.as_u64(),
                        },
                    ));
                }
            }
        }

        // §2：只有 Structure 的 attach_tag 可以不是 Free。
        if record.kind != BodyKind::Structure && record.attach != AttachTag::Free {
            return Err(body_error(
                *key,
                SaveErrorKind::InvalidCombination {
                    what: "non-Structure body must use Free attach",
                },
            ));
        }

        // 岛使用 anchored 字段；其余 kind 的 0 是写方归一化，不据此判损坏。
        if record.kind == BodyKind::Island && record.anchored > 1 {
            return Err(body_error(
                *key,
                SaveErrorKind::InvalidValue {
                    what: "anchored (island must be 0 or 1)",
                    value: i64::from(record.anchored),
                },
            ));
        }
    }
    Ok(())
}

/// region 键 -> 该 region 的锚点块键。
///
/// read_region 的第三个参数是块键（内部还会 region_of 一次），所以必须传
/// (rx * 4, ry * 4, rz * 4)，否则 region 索引 >= 2 时读错文件。
fn region_anchor(body: BodyKey, region: ChunkKey) -> Result<ChunkKey, SaveError> {
    let x = region.x.checked_mul(REGION_SIZE).ok_or_else(|| {
        region_error(
            body,
            region,
            SaveErrorKind::InvalidBounds { index: 0, axis: 0 },
        )
    })?;
    let y = region.y.checked_mul(REGION_SIZE).ok_or_else(|| {
        region_error(
            body,
            region,
            SaveErrorKind::InvalidBounds { index: 1, axis: 1 },
        )
    })?;
    let z = region.z.checked_mul(REGION_SIZE).ok_or_else(|| {
        region_error(
            body,
            region,
            SaveErrorKind::InvalidBounds { index: 2, axis: 2 },
        )
    })?;
    let anchor = ChunkKey::new(x, y, z);
    debug_assert_eq!(
        region_of(anchor),
        region,
        "region 锚点块键必须落回本 region"
    );
    Ok(anchor)
}

/// 阶段一：用临时 interner / volume 解码每一块内容，解完立刻释放，不占整世界内存。
///
/// 版本不匹配的块跳过解码：它们的 content 不会被安装，只保留记录与实体。
fn validate_contents(
    bodies: &[BodyPlan],
    budget: usize,
    generator_version: u32,
) -> Result<(), SaveError> {
    let mut interner = VoxelInterner::new(budget.max(1));
    let mut volume = VoxVolume::new(FixedPoint::from_bits(1));

    for body in bodies {
        for file in &body.regions {
            for (key, chunk) in &file.chunks {
                let Some(bytes) = chunk.content.as_deref() else {
                    continue;
                };
                if chunk.generator_version != generator_version {
                    continue;
                }
                let old = install_chunk(&mut volume, *key, bytes, interner.inner_mut())?;
                if let Some(old) = old {
                    dec_ref_recursive(interner.inner_mut(), &old.get_root_id());
                }
                let tree = volume
                    .take_chunk_raw(key)
                    .expect("install_chunk 成功后块必须存在");
                dec_ref_recursive(interner.inner_mut(), &tree.get_root_id());
            }
        }
    }
    Ok(())
}

/// 阶段一：在临时 World 上试应用每条实体记录，提前暴露 payload / 组件解码错误。
///
/// 只借用真实世界的 PersistRegistry（只读），所有插入都落到临时 World；缺注册表时
/// 直接跳过，让阶段二的资源检查给出 Missing（仍然在写世界之前）。
fn validate_entities(world: &World, bodies: &[BodyPlan]) -> Result<(), SaveError> {
    let Some(registry) = world.get_resource::<PersistRegistry>() else {
        return Ok(());
    };
    let mut scratch = World::new();
    let entity = scratch.spawn_empty().id();
    let mut opaque = OpaqueComponents::default();

    for body in bodies {
        for file in &body.regions {
            for chunk in file.chunks.values() {
                for record in &chunk.entities {
                    registry.apply_entity(&mut scratch, entity, record, &mut opaque)?;
                }
            }
        }
    }
    Ok(())
}

fn content_budget(world: &World) -> usize {
    world
        .get_resource::<VoxelInterner>()
        .map_or(DEFAULT_INTERNER_BUDGET_BYTES, |interner| {
            interner.budget_bytes()
        })
}

/// 阶段二：所有必需资源必须在写世界之前就位；缺一个都 Err，不静默跳过。
fn require_resources(world: &World) -> Result<(), SaveError> {
    require_resource::<VoxelInterner>(world, "VoxelInterner")?;
    require_resource::<StableEntityIndex>(world, "StableEntityIndex")?;
    require_resource::<StableIdAllocator>(world, "StableIdAllocator")?;
    require_resource::<IslandRegistry>(world, "IslandRegistry")?;
    require_resource::<WorldSeed>(world, "WorldSeed")?;
    require_resource::<PersistRegistry>(world, "PersistRegistry")?;
    require_resource::<OpaqueComponents>(world, "OpaqueComponents")?;
    Ok(())
}

fn require_resource<T: Resource>(world: &World, what: &'static str) -> Result<(), SaveError> {
    if world.get_resource::<T>().is_some() {
        Ok(())
    } else {
        Err(missing_resource(what))
    }
}

/// 阶段二：按 BodyKey 升序建 body 实体（StableEntityId + Body + 标签 + VoxVolume）。
fn spawn_plan_bodies(
    world: &mut World,
    plan: &Plan,
) -> Result<BTreeMap<BodyKey, Entity>, SaveError> {
    let mut entities = BTreeMap::new();
    for body in &plan.bodies {
        let entity = spawn_body(world, body.key, &body.record)?;
        entities.insert(body.key, entity);
    }
    Ok(entities)
}

fn spawn_body(world: &mut World, key: BodyKey, record: &BodyRecord) -> Result<Entity, SaveError> {
    let id = key.stable_id();
    let volume = VoxVolume::new(FixedPoint::from_bits(record.voxel_size));

    let entity = match record.kind {
        BodyKind::Island => {
            // anchored 只对岛有意义；其余 kind 的该字段是写方归一化字段，忽略。
            let anchored = match record.anchored {
                0 => false,
                1 => true,
                other => {
                    return Err(body_error(
                        key,
                        SaveErrorKind::InvalidValue {
                            what: "anchored",
                            value: i64::from(other),
                        },
                    ));
                }
            };
            let entity = world
                .spawn((
                    id,
                    Body::island(anchored, BiomeId(record.biome)),
                    IslandTag,
                    volume,
                ))
                .id();
            let drift = Vec3F::new(
                FixedPoint::from_bits(record.drift[0]),
                FixedPoint::from_bits(record.drift[1]),
                FixedPoint::from_bits(record.drift[2]),
            );
            world.resource_mut::<IslandRegistry>().insert(
                IslandId(key.as_u64()),
                Island::new(drift, anchored, BiomeId(record.biome)),
            );
            entity
        }
        BodyKind::Ship => world.spawn((id, Body::ship(), ShipTag, volume)).id(),
        BodyKind::Structure => {
            let attach = match record.attach {
                AttachTag::World => Attachment::World,
                AttachTag::Body => Attachment::Body(record.attach_to.stable_id()),
                AttachTag::Free => Attachment::Free,
            };
            world
                .spawn((id, Body::structure(attach), StructureTag, volume))
                .id()
        }
        BodyKind::Automaton => world
            .spawn((id, Body::automaton(), AutomatonTag, volume))
            .id(),
    };

    world.resource_mut::<StableEntityIndex>().insert(id, entity);

    Ok(entity)
}

/// 阶段二：按 region / 块键升序装块；content = None 的块不装，留给生成器。
///
/// 返回因生成器版本不匹配而丢弃内容的块数（[LoadStats::stale_chunks]）。
/// 版本过滤只在装载阶段做，不在 resolve 阶段硬拒绝：记录仍留在存档里，
/// 下次请求走正常的生成路径，旧块因此能自我修复。
fn install_plan_chunks(
    world: &mut World,
    plan: &Plan,
    body_entities: &BTreeMap<BodyKey, Entity>,
    generator_version: u32,
) -> Result<usize, SaveError> {
    // 读档装回的内容 != 生成器输出，必须记成已编辑；否则下一次卸载会把它删成
    // 墓碑，玩家改过的地形就丢了。资源缺失只在函数入口 warn 一次。
    let has_dirty = world.get_resource::<VoxelDirtySet>().is_some();
    if !has_dirty {
        warn!("load: 缺少 VoxelDirtySet 资源，读档装回的区块不会被标记为已编辑");
    }
    world.resource_scope::<VoxelInterner, _>(|world, mut interner| {
        let mut stale = 0usize;
        for body in &plan.bodies {
            let entity = body_entities.get(&body.key).copied().ok_or_else(|| {
                body_error(
                    body.key,
                    SaveErrorKind::Missing {
                        what: "body entity (spawn stage did not produce it)",
                        id: body.key.as_u64(),
                    },
                )
            })?;

            for file in &body.regions {
                for (key, chunk) in &file.chunks {
                    let Some(bytes) = chunk.content.as_deref() else {
                        continue;
                    };
                    // 版本不等的旧内容不能装：否则生成器改版后旧地形会被当成新地形。
                    // 只丢内容、不丢记录与实体（实体是玩家造的东西）。
                    if chunk.generator_version != generator_version {
                        stale += 1;
                        continue;
                    }
                    {
                        let mut entity_mut = world.entity_mut(entity);
                        let mut volume = entity_mut.get_mut::<VoxVolume>().ok_or_else(|| {
                            body_error(
                                body.key,
                                SaveErrorKind::Missing {
                                    what: "VoxVolume component for loaded body",
                                    id: body.key.as_u64(),
                                },
                            )
                        })?;
                        let old = install_chunk(&mut volume, *key, bytes, interner.inner_mut())?;
                        if let Some(old) = old {
                            // VoxTree 没有 Drop，旧树必须显式减引用。
                            dec_ref_recursive(interner.inner_mut(), &old.get_root_id());
                        }
                    }
                    if has_dirty {
                        if let Some(mut dirty) = world.get_resource_mut::<VoxelDirtySet>() {
                            dirty.mark_edited_for(Some(body.key), *key, VoxelBox::FULL);
                        }
                    }
                }
            }
        }
        Ok::<usize, SaveError>(stale)
    })
}

/// 阶段二：按块键升序应用实体记录；找不到稳定 ID 就新建（挂 StableEntityId + 索引）。
fn apply_plan_entities(world: &mut World, plan: &Plan) -> Result<usize, SaveError> {
    let mut applied = 0usize;
    world.resource_scope::<PersistRegistry, _>(|world, registry| {
        world.resource_scope::<OpaqueComponents, _>(|world, mut opaque| {
            for body in &plan.bodies {
                for file in &body.regions {
                    for (&key, chunk) in &file.chunks {
                        for record in &chunk.entities {
                            let entity = ensure_entity(world, record.stable_entity_id);
                            registry.apply_entity(world, entity, record, &mut opaque)?;
                            // `ChunkAnchor` 是分组键、不在 payload 里；不补的话读档后的
                            // 实体既不会被 `capture` 收集，也不会被 `unload_members` 销毁。
                            world
                                .entity_mut(entity)
                                .insert(ChunkAnchor::new(body.key, key));
                            applied += 1;
                        }
                    }
                }
            }
            Ok::<(), SaveError>(())
        })?;
        Ok::<(), SaveError>(())
    })?;
    Ok(applied)
}

/// 按稳定 ID 找实体，没有就新建并登记索引。
///
/// 区块运行期的实体恢复（`WorldChunkPlugin` 的 `restore_members`）复用同一入口，
/// 保证「启动期批量恢复」与「运行期按 [ChunkEntities](crate::voxel::stream::ChunkEntities)
/// 恢复」对同一个稳定 ID 落到同一个实体上。调用方需保证 [StableEntityIndex] 已初始化。
pub(crate) fn ensure_entity(world: &mut World, id: u64) -> Entity {
    let stable = StableEntityId(id);
    if let Some(entity) = world.resource::<StableEntityIndex>().entity(stable) {
        return entity;
    }
    let entity = world.spawn(stable).id();
    world
        .resource_mut::<StableEntityIndex>()
        .insert(stable, entity);
    entity
}

/// 统计孤儿 region：root 下 <u64> 目录、但 meta 里没有该 body。
///
/// 孤儿是崩溃后的正常残留：不删、不报错。目录里的文件名按宽松规则解析
/// （只认 canonical 的 region_x_y_z / .recovered，跳过 .tmp / .corrupt），
/// 解析不出的文件忽略；同一个 region 的主文件与 .recovered 只算一个。
fn count_orphan_regions(paths: &SavePaths, meta: &WorldMeta) -> Result<usize, SaveError> {
    let root = paths.root();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(io_read_error(root, err)),
    };

    let mut total = 0usize;
    for entry in entries {
        let entry = entry.map_err(|err| io_read_error(root, err))?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|err| io_read_error(&path, err))?;
        if !file_type.is_dir() {
            continue;
        }

        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let Ok(id) = name.parse::<u64>() else {
            continue;
        };
        let body = BodyKey::new(StableEntityId(id));
        if meta.bodies.contains_key(&body) {
            continue;
        }
        total = total.saturating_add(count_region_files_relaxed(&path)?);
    }
    Ok(total)
}

fn count_region_files_relaxed(dir: &Path) -> Result<usize, SaveError> {
    let entries = fs::read_dir(dir).map_err(|err| io_read_error(dir, err))?;
    let mut keys: BTreeSet<ChunkKey> = BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|err| io_read_error(dir, err))?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if name.ends_with(".tmp") || name.ends_with(".corrupt") {
            continue;
        }
        if let Some(key) = parse_canonical_region_name(name) {
            keys.insert(key);
        }
    }
    Ok(keys.len())
}

/// 只接受 store 会写出的 canonical 文件名（含可选的 .recovered）。
fn parse_canonical_region_name(name: &str) -> Option<ChunkKey> {
    let base = name.strip_suffix(".recovered").unwrap_or(name);
    let rest = base.strip_prefix("region_")?;
    let mut parts = rest.split('_');
    let x = parts.next()?.parse::<i32>().ok()?;
    let y = parts.next()?.parse::<i32>().ok()?;
    let z = parts.next()?.parse::<i32>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let key = ChunkKey::new(x, y, z);
    let canonical = format!("region_{}_{}_{}", key.x, key.y, key.z);
    (canonical == base).then_some(key)
}

fn missing_resource(what: &'static str) -> SaveError {
    SaveError::semantic(
        format!("load.resources.{what}"),
        SaveErrorKind::Missing {
            what: "required resource for world load",
            id: 0,
        },
    )
}

fn io_read_error(path: &Path, err: io::Error) -> SaveError {
    SaveError::semantic(
        path.display().to_string(),
        SaveErrorKind::Io {
            stage: "read",
            kind: err.kind(),
            raw: err.raw_os_error(),
        },
    )
}

fn body_error(key: BodyKey, kind: SaveErrorKind) -> SaveError {
    SaveError::semantic(format!("load.body[{}]", key.as_u64()), kind)
}

fn region_error(body: BodyKey, region: ChunkKey, kind: SaveErrorKind) -> SaveError {
    SaveError::semantic(
        format!(
            "load.body[{}].region[{},{},{}]",
            body.as_u64(),
            region.x,
            region.y,
            region.z
        ),
        kind,
    )
}

fn chunk_error(body: BodyKey, key: ChunkKey, kind: SaveErrorKind) -> SaveError {
    SaveError::semantic(
        format!(
            "load.body[{}].chunk[{},{},{}]",
            body.as_u64(),
            key.x,
            key.y,
            key.z
        ),
        kind,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use bevy::prelude::Component;
    use serde::{Deserialize, Serialize};

    use game_engine::identity::StableEntityIndex;
    use game_engine::residency::ChunkAnchor;
    use game_engine::save::{Persist, PersistComponent, StableTypeId};
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

        let (body_entity, island_id) =
            spawn_island_entity(&mut source, Vec3F::ZERO, true, BiomeId(1));
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
}
