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
use std::sync::{Arc, Mutex};

use bevy::prelude::{warn, Entity, Resource, World};

use game_engine::body_key::BodyKey;
use game_engine::identity::{StableEntityId, StableEntityIndex, StableIdAllocator};
use game_engine::input::ResolvedActions;
use game_engine::math::{FixedPoint, Vec3F};
use game_engine::modding::{AppliedModSet, ParamRecord, ParamTable};
use game_engine::residency::ChunkAnchor;
use game_engine::save::{
    install_chunk, read_region, read_world_meta, reclaim_orphans, region_of, regions_on_disk,
    AttachTag, BodyKind, BodyRecord, OpaqueComponents, PersistRegistry, ReclaimStats, RegionFile,
    SaveError, SaveErrorKind, SavePaths, WorldMeta, REGION_SIZE,
};
use game_engine::streaming::EntityRecord;
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

/// 读档被拒绝：世界身份（mod / 插件 / 动作表 / 参数 / 版本）与存档不一致。
/// 由流式启动系统写入，Godot 侧读它渲染差异面板。
///
/// 只有 [`SaveErrorKind::is_world_rejection`] 为真的错误才会变成它：档本身是好的，
/// 只是与当前装配不符。损坏 / 截断 / IO 仍然走「降级建新世界」，不会走这条路径。
#[derive(Resource, Clone, Debug, PartialEq, Eq)]
pub struct WorldLoadRejection {
    /// 稳定错误码（[`SaveErrorKind::code`]），客户端按它分支。
    pub code: String,
    /// 出错的字段路径（[`SaveError::path`]），如 `load.meta.plugins`。
    pub path: String,
    /// 完整原因（[`SaveError`] 的 Display，含中文说明）。
    pub message: String,
}

/// 拒绝信息的跨线程槽：后台 Bevy 线程写，Godot 主线程读。
///
/// 与 [`crate::voxel::terrain::VoxelLodHandle`] / `ObjectTablesHandle` 同一种模式
/// （`Arc<Mutex<..>>` 句柄经 `BevyBackendConfig` 双向持有）：后台 App 停掉之后
/// 句柄里的值仍然在，Godot 侧因此能在 `bev_stopped` 之后继续展示差异。
#[derive(Resource, Clone, Debug, Default)]
pub struct WorldLoadRejectionHandle(Arc<Mutex<Option<WorldLoadRejection>>>);

impl WorldLoadRejectionHandle {
    /// 写入拒绝信息（覆盖上一次；启动新会话前由调用方 [`Self::clear`]）。
    pub fn set(&self, rejection: WorldLoadRejection) {
        *self.0.lock().expect("world load rejection lock") = Some(rejection);
    }

    /// 读最近一次拒绝信息；没有拒绝过时为 None。
    #[must_use]
    pub fn get(&self) -> Option<WorldLoadRejection> {
        self.0.lock().expect("world load rejection lock").clone()
    }

    /// 清空（每次启动新会话时调用，避免把上一次的拒绝当成这一次的）。
    pub fn clear(&self) {
        *self.0.lock().expect("world load rejection lock") = None;
    }
}

/// 读档模式。
///
/// - [Normal]：完整校验，身份不符（mod / 插件 / 动作表 / 参数 / 版本）拒绝读档；
/// - [ReadOnly]：只做**结构**校验（键、ID、region 归属、负载可解码……），跳过身份
///   校验，并允许世界参数应用失败。用于「打开旧世界看一眼」：装配本来就和存档不同，
///   按身份拒绝会让只读入口永远打不开。
///
/// 只读模式**不**改变磁盘：写侧由 `SavePaths::read_only` 与只读闸门保证。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LoadMode {
    /// 正常读档（默认）。
    #[default]
    Normal,
    /// 只读查看：放宽身份校验，不写盘。
    ReadOnly,
}

/// 只读打开时，存档身份与当前装配的差异。
///
/// 只读模式跳过身份校验，但「打开了什么、和现在的装配差在哪」必须能看见：
/// 正常读档只会返回**第一个**错误，这里收集全部差异供差异面板逐项展示。
/// 只在 [LoadMode::ReadOnly] 且读档成功时写入 World；正常读档没有这个资源。
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct WorldLoadDiff {
    /// mod 清单差异（缺失 / 多余 / 版本不一致）。
    pub mods: Vec<ModDiff>,
    /// 插件装配差异（集合 + 顺序）。
    pub plugins: PluginDiff,
    /// 动作表差异（缺失 / 多余 / 映射变化）。
    pub actions: Vec<ActionDiff>,
    /// 世界参数差异（缺失 / 多余 / 值或类型变化）。
    pub params: Vec<ParamDiff>,
}

impl WorldLoadDiff {
    /// 没有任何差异。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mods.is_empty()
            && self.plugins.is_empty()
            && self.actions.is_empty()
            && self.params.is_empty()
    }
}

/// 一条 mod 清单差异。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModDiff {
    /// 存档里有、当前装配没有。
    Missing { id: String, saved_version: u32 },
    /// 当前装配有、存档没有。
    Extra { id: String, current_version: u32 },
    /// 同 id，版本不同。
    VersionMismatch {
        id: String,
        saved: u32,
        current: u32,
    },
}

/// 插件装配差异。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginDiff {
    /// 存档里有、当前装配没有的插件。
    pub missing: Vec<String>,
    /// 当前装配有、存档没有的插件。
    pub extra: Vec<String>,
    /// 集合相同但顺序不同。
    pub order_changed: bool,
    /// 存档记录的装配顺序（面板展示用）。
    pub saved: Vec<String>,
    /// 当前装配顺序（面板展示用）。
    pub current: Vec<String>,
}

impl PluginDiff {
    /// 没有缺失、多余或顺序变化。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.missing.is_empty() && self.extra.is_empty() && !self.order_changed
    }
}

/// 一条动作表差异。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionDiff {
    /// 存档里有、当前动作表没有。
    Missing {
        name: String,
        action: u32,
        channel_bit: u32,
    },
    /// 当前动作表有、存档没有。
    Extra {
        name: String,
        action: u32,
        channel_bit: u32,
    },
    /// 同名动作的 ID 或通道位变了。
    Changed {
        name: String,
        saved_action: u32,
        saved_channel_bit: u32,
        current_action: u32,
        current_channel_bit: u32,
    },
}

/// 一条世界参数差异。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParamDiff {
    /// 存档里有、当前 schema 没有。
    Missing { record: ParamRecord },
    /// 当前 schema 有、存档没有（用默认值）。
    Extra { record: ParamRecord },
    /// 同一个 key，类型 / scale / 值有变化。
    Changed {
        saved: ParamRecord,
        current: ParamRecord,
    },
}

/// 从 paths 读一个世界；generator_version 是当前生成器的版本号。
///
/// 等价于 [load_world_with_mode] 的 [LoadMode::Normal]。
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
    load_world_with_mode(world, paths, generator_version, LoadMode::Normal)
}

/// 按 [LoadMode] 读一个世界（见 [LoadMode] 的语义）。
///
/// 阶段一 / 阶段二的划分与 [load_world] 相同；只读模式只在阶段一放宽身份校验、
/// 在阶段二把世界参数应用失败降级为 warn。
pub fn load_world_with_mode(
    world: &mut World,
    paths: &SavePaths,
    generator_version: u32,
    mode: LoadMode,
) -> Result<Option<LoadedWorld>, SaveError> {
    let Some(meta) = read_world_meta(paths)? else {
        return Ok(None);
    };

    // 阶段一：只读文件 + 全部校验；这一步不碰 world。
    let plan = build_plan(world, paths, meta, generator_version, mode)?;

    // 阶段二：先确认所有必需资源都在，再开始写世界。
    require_resources(world)?;
    if mode == LoadMode::Normal {
        world.remove_resource::<WorldLoadDiff>();
        apply_meta_params(world, &plan.meta)?;
    } else {
        if let Err(error) = apply_meta_params(world, &plan.meta) {
            // 只读查看的目标是地形和实体，不是玩法数值；参数表对不上不该挡住打开。
            warn!("world: 只读会话未能应用世界参数（继续查看）：{error}");
        }
        // 差异在参数应用之后收集：能应用的参数已经写回，面板展示的是真正没能
        // 对上的 mod / 插件 / 动作 / 参数，而不是「存档值不等于默认值」。
        let diff = compute_load_diff(world, &plan.meta);
        world.insert_resource(diff);
    }
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

/// 阶段一的总编排：校验 meta -> 按 body / region 读入并校验 -> 校验序号与负载。
///
/// 顺序即语义：任何错误都在返回前出现，且世界一次都没有被改过。
fn build_plan(
    world: &World,
    paths: &SavePaths,
    meta: WorldMeta,
    generator_version: u32,
    mode: LoadMode,
) -> Result<Plan, SaveError> {
    validate_meta(&meta)?;
    if mode == LoadMode::Normal {
        // 身份校验：装配必须与存档逐项一致。只读查看刻意跳过——装配不同正是
        // 它要展示的东西。
        validate_meta_mods(world, &meta)?;
        validate_meta_actions(world, &meta)?;
        validate_meta_plugins(world, &meta)?;
        validate_meta_params(world, &meta)?;
    }

    let mut seen_entities: BTreeSet<u64> = BTreeSet::new();
    let mut bodies: Vec<BodyPlan> = Vec::with_capacity(meta.bodies.len());
    let mut chunk_count = 0usize;

    for (body_key, record) in &meta.bodies {
        let (regions, chunks) = load_body_regions(paths, *body_key, &mut seen_entities)?;
        chunk_count += chunks;
        bodies.push(BodyPlan {
            key: *body_key,
            record: *record,
            regions,
        });
    }

    validate_next_stable_id(&meta, &seen_entities)?;

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

/// 按 region 键升序读出一个 body 的全部 region 文件，并逐块校验。
///
/// 返回 (region 文件, 块数)；块数包含 content = None 的记录。
fn load_body_regions(
    paths: &SavePaths,
    body_key: BodyKey,
    seen_entities: &mut BTreeSet<u64>,
) -> Result<(Vec<RegionFile>, usize), SaveError> {
    let mut regions: Vec<RegionFile> = Vec::new();
    let mut chunk_count = 0usize;
    for region_key in regions_on_disk(paths, body_key)? {
        let file = read_region_file(paths, body_key, region_key)?;
        chunk_count += validate_region_chunks(body_key, region_key, &file, seen_entities)?;
        regions.push(file);
    }
    Ok((regions, chunk_count))
}

/// 读一个 region 文件；meta 列出的 body 缺文件是坏档，不是"跳过"。
fn read_region_file(
    paths: &SavePaths,
    body_key: BodyKey,
    region_key: ChunkKey,
) -> Result<RegionFile, SaveError> {
    let anchor = region_anchor(body_key, region_key)?;
    read_region(paths, body_key, anchor)?.ok_or_else(|| {
        region_error(
            body_key,
            region_key,
            SaveErrorKind::Missing {
                what: "region file (meta body has no readable region)",
                id: 0,
            },
        )
    })
}

/// 校验一个 region 文件里的块：键必须落在本 region，实体 ID 必须非零且全局唯一。
///
/// 返回该文件的块数（含 content = None 的记录）。
fn validate_region_chunks(
    body_key: BodyKey,
    region_key: ChunkKey,
    file: &RegionFile,
    seen_entities: &mut BTreeSet<u64>,
) -> Result<usize, SaveError> {
    let mut chunk_count = 0usize;
    for (chunk_key, chunk) in &file.chunks {
        if region_of(*chunk_key) != region_key {
            return Err(chunk_error(
                body_key,
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
        validate_chunk_entities(body_key, *chunk_key, &chunk.entities, seen_entities)?;
        chunk_count += 1;
    }
    Ok(chunk_count)
}

/// 校验单个块里的实体记录：ID 非零，且跨 region 不能重复。
fn validate_chunk_entities(
    body_key: BodyKey,
    chunk_key: ChunkKey,
    entities: &[EntityRecord],
    seen_entities: &mut BTreeSet<u64>,
) -> Result<(), SaveError> {
    for entity in entities {
        let id = entity.stable_entity_id;
        if id == 0 {
            return Err(chunk_error(
                body_key,
                chunk_key,
                SaveErrorKind::InvalidValue {
                    what: "entity stable_entity_id",
                    value: 0,
                },
            ));
        }
        if !seen_entities.insert(id) {
            return Err(chunk_error(
                body_key,
                chunk_key,
                SaveErrorKind::Duplicate {
                    what: "entity stable_entity_id across regions",
                    id,
                },
            ));
        }
    }
    Ok(())
}

/// next_stable_entity_id 必须大于所有已经出现的稳定 ID（body 与实体）。
/// 空表时 next = 0 也是坏档：发号器从 1 开始。
fn validate_next_stable_id(
    meta: &WorldMeta,
    seen_entities: &BTreeSet<u64>,
) -> Result<(), SaveError> {
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
    Ok(())
}

/// 阶段一：按当前插件 schema 校验存档里的世界参数。
///
/// 空参数段不要求 `ParamTable`（旧存档 / 无参数装配）；非空时缺表即报错，
/// 未知 key、类型 / scale 不符、越界都由 [ParamTable::validate_saved] 拒绝。
fn validate_meta_params(world: &World, meta: &WorldMeta) -> Result<(), SaveError> {
    if meta.params.is_empty() {
        return Ok(());
    }
    let table = world
        .get_resource::<ParamTable>()
        .ok_or_else(|| missing_resource("ParamTable"))?;
    table.validate_saved(&meta.params).map_err(param_error)
}

/// 阶段一：存档里的 mod 清单必须与本次装配逐项一致（数量 / id / 版本）。
///
/// 两边都按 id 升序；空清单且世界没有 `AppliedModSet` 时放行（老存档 / 老测试）。
fn validate_meta_mods(world: &World, meta: &WorldMeta) -> Result<(), SaveError> {
    let Some(applied) = world.get_resource::<AppliedModSet>() else {
        return if meta.mods.is_empty() {
            Ok(())
        } else {
            Err(missing_resource("AppliedModSet"))
        };
    };
    let saved = &meta.mods;
    let current = &applied.mods;
    let common = saved.len().min(current.len());
    for index in 0..common {
        let (saved_mod, current_mod) = (&saved[index], &current[index]);
        if saved_mod.id != current_mod.id {
            return Err(if saved_mod.id < current_mod.id {
                mod_error(SaveErrorKind::MissingMod {
                    id: saved_mod.id.clone(),
                })
            } else {
                mod_error(SaveErrorKind::UnexpectedMod {
                    id: current_mod.id.clone(),
                })
            });
        }
        if saved_mod.version != current_mod.version {
            return Err(mod_error(SaveErrorKind::ModVersionMismatch {
                id: saved_mod.id.clone(),
                saved: saved_mod.version,
                current: current_mod.version,
            }));
        }
    }
    if let Some(extra) = saved.get(common) {
        return Err(mod_error(SaveErrorKind::MissingMod {
            id: extra.id.clone(),
        }));
    }
    if let Some(extra) = current.get(common) {
        return Err(mod_error(SaveErrorKind::UnexpectedMod {
            id: extra.id.clone(),
        }));
    }
    Ok(())
}

/// 阶段一：存档冻结的动作表必须与当前 `ResolvedActions` 完全一致。
///
/// `meta.actions` 按 name 升序，动作表本身按「core 在前、mod 按分配顺序」排列，
/// 所以先按 name 归一化再逐项比对（name / action / channel_bit 全部敏感）。
/// 未发布前不做旧档兼容：缺动作表段（动作表写进 meta 之前的 v2）一律拒绝。
fn validate_meta_actions(world: &World, meta: &WorldMeta) -> Result<(), SaveError> {
    let Some(resolved) = world.get_resource::<ResolvedActions>() else {
        return if meta.actions.is_empty() {
            Ok(())
        } else {
            Err(missing_resource("ResolvedActions"))
        };
    };
    let mut current: Vec<(&str, u32, u32)> = resolved
        .table()
        .iter()
        .map(|def| (def.name, def.action.0, def.channel_bit))
        .collect();
    current.sort_by(|a, b| a.0.cmp(b.0));
    if current.len() != meta.actions.len() {
        return Err(action_error(SaveErrorKind::ActionCountMismatch {
            expected: meta.actions.len(),
            found: current.len(),
        }));
    }
    for (saved, (name, action, channel_bit)) in meta.actions.iter().zip(current) {
        if saved.name != name || saved.action != action || saved.channel_bit != channel_bit {
            return Err(action_error(SaveErrorKind::ActionTableMismatch {
                name: saved.name.clone(),
            }));
        }
    }
    Ok(())
}

/// 阶段一：存档冻结的插件清单必须与本次装配完全一致（集合 + 顺序）。
///
/// 为什么连顺序都比：插件装配顺序是确定性拓扑排序的结果，也是 world 的一部分。
/// 只比集合的话，tie-break 变化会悄悄改变系统调度顺序，而存档看起来「能读」。
/// 空清单 + 当前装配没有 `ActivePlugins` 时放行（直接搭 World 的单测）。
fn validate_meta_plugins(world: &World, meta: &WorldMeta) -> Result<(), SaveError> {
    let Some(active) = world.get_resource::<super::ActivePlugins>() else {
        return if meta.plugins.is_empty() {
            Ok(())
        } else {
            Err(missing_resource("ActivePlugins"))
        };
    };
    let saved: BTreeSet<&str> = meta.plugins.iter().map(String::as_str).collect();
    let current: BTreeSet<&str> = active.ids().iter().copied().collect();
    if let Some(id) = saved.difference(&current).next() {
        return Err(plugin_error(SaveErrorKind::MissingPlugin {
            id: (*id).to_string(),
        }));
    }
    if let Some(id) = current.difference(&saved).next() {
        return Err(plugin_error(SaveErrorKind::UnexpectedPlugin {
            id: (*id).to_string(),
        }));
    }
    for (saved_id, current_id) in meta.plugins.iter().zip(active.ids()) {
        if saved_id.as_str() != *current_id {
            return Err(plugin_error(SaveErrorKind::PluginOrderMismatch {
                saved: saved_id.clone(),
                current: (*current_id).to_string(),
            }));
        }
    }
    Ok(())
}

/// 收集只读打开时存档身份与当前装配的全部差异。
///
/// 与 validate_meta_mods / validate_meta_actions / validate_meta_plugins /
/// validate_meta_params 使用同一组事实来源，但不短路：正常读档只需要第一个
/// 错误，只读面板需要知道差在哪几项。身份校验只在 LoadMode::Normal 运行，
/// 本函数只在 LoadMode::ReadOnly 运行，两者不会同时生效。
///
/// 参数差异在 load_world_with_mode 尝试应用存档值之后计算：能应用的值
/// 已经写回 ParamTable，因此这里报出的是「没能应用」的 key（schema 缺失 /
/// 类型变化 / 值非法），不是「存档值本来就和默认值不同」。
#[must_use]
pub fn compute_load_diff(world: &World, meta: &WorldMeta) -> WorldLoadDiff {
    let mut diff = WorldLoadDiff::default();

    // ── mod 清单 ────────────────────────────────────────────────
    let current_mods: BTreeMap<&str, u32> = world
        .get_resource::<AppliedModSet>()
        .map(|applied| {
            applied
                .mods
                .iter()
                .map(|mod_ref| (mod_ref.id.as_str(), mod_ref.version))
                .collect()
        })
        .unwrap_or_default();
    let saved_mods: BTreeMap<&str, u32> = meta
        .mods
        .iter()
        .map(|record| (record.id.as_str(), record.version))
        .collect();
    let mod_ids: BTreeSet<&str> = saved_mods
        .keys()
        .chain(current_mods.keys())
        .copied()
        .collect();
    for id in mod_ids {
        match (saved_mods.get(id), current_mods.get(id)) {
            (Some(saved), Some(current)) if saved != current => {
                diff.mods.push(ModDiff::VersionMismatch {
                    id: id.to_string(),
                    saved: *saved,
                    current: *current,
                });
            }
            (Some(saved), None) => diff.mods.push(ModDiff::Missing {
                id: id.to_string(),
                saved_version: *saved,
            }),
            (None, Some(current)) => diff.mods.push(ModDiff::Extra {
                id: id.to_string(),
                current_version: *current,
            }),
            _ => {}
        }
    }

    // ── 插件序列 ────────────────────────────────────────────────
    let current_plugins: Vec<&str> = world
        .get_resource::<super::ActivePlugins>()
        .map(|active| active.ids().to_vec())
        .unwrap_or_default();
    let saved_plugins: Vec<&str> = meta.plugins.iter().map(String::as_str).collect();
    let saved_set: BTreeSet<&str> = saved_plugins.iter().copied().collect();
    let current_set: BTreeSet<&str> = current_plugins.iter().copied().collect();
    // 只读 profile 会主动剔除 gameplay / movement 等。那些插件「存档有、当前没有」
    // 是只读的刻意裁剪，不是身份漂移；从缺失与顺序比较里都排除掉。
    let omitted: BTreeSet<&str> = super::profile::readonly_excluded_plugins()
        .iter()
        .copied()
        .filter(|id| !current_set.contains(id))
        .collect();
    diff.plugins.missing = saved_set
        .difference(&current_set)
        .filter(|id| !omitted.contains(*id))
        .map(|id| (*id).to_string())
        .collect();
    diff.plugins.extra = current_set
        .difference(&saved_set)
        .map(|id| (*id).to_string())
        .collect();
    let saved_visible: Vec<&str> = saved_plugins
        .iter()
        .copied()
        .filter(|id| !omitted.contains(id))
        .collect();
    diff.plugins.order_changed = diff.plugins.missing.is_empty()
        && diff.plugins.extra.is_empty()
        && saved_visible != current_plugins;
    if diff.plugins.order_changed {
        diff.plugins.saved = saved_visible.iter().map(|id| (*id).to_string()).collect();
        diff.plugins.current = current_plugins.iter().map(|id| (*id).to_string()).collect();
    }

    // ── 动作表 ─────────────────────────────────────────────────
    let current_actions: BTreeMap<&str, (u32, u32)> = world
        .get_resource::<ResolvedActions>()
        .map(|resolved| {
            resolved
                .table()
                .iter()
                .map(|def| (def.name, (def.action.0, def.channel_bit)))
                .collect()
        })
        .unwrap_or_default();
    let saved_actions: BTreeMap<&str, (u32, u32)> = meta
        .actions
        .iter()
        .map(|record| (record.name.as_str(), (record.action, record.channel_bit)))
        .collect();
    let action_names: BTreeSet<&str> = saved_actions
        .keys()
        .chain(current_actions.keys())
        .copied()
        .collect();
    for name in action_names {
        match (saved_actions.get(name), current_actions.get(name)) {
            (Some(saved), Some(current)) if saved != current => {
                diff.actions.push(ActionDiff::Changed {
                    name: name.to_string(),
                    saved_action: saved.0,
                    saved_channel_bit: saved.1,
                    current_action: current.0,
                    current_channel_bit: current.1,
                });
            }
            (Some(saved), None) => diff.actions.push(ActionDiff::Missing {
                name: name.to_string(),
                action: saved.0,
                channel_bit: saved.1,
            }),
            (None, Some(current)) => diff.actions.push(ActionDiff::Extra {
                name: name.to_string(),
                action: current.0,
                channel_bit: current.1,
            }),
            _ => {}
        }
    }

    // ── 世界参数 ────────────────────────────────────────────────
    let current_params_vec: Vec<ParamRecord> = world
        .get_resource::<ParamTable>()
        .map(ParamTable::world_values)
        .unwrap_or_default();
    let current_params: BTreeMap<&str, &ParamRecord> = current_params_vec
        .iter()
        .map(|record| (record.key.as_str(), record))
        .collect();
    let saved_params: BTreeMap<&str, &ParamRecord> = meta
        .params
        .iter()
        .map(|record| (record.key.as_str(), record))
        .collect();
    let param_keys: BTreeSet<&str> = saved_params
        .keys()
        .chain(current_params.keys())
        .copied()
        .collect();
    for key in param_keys {
        match (saved_params.get(key), current_params.get(key)) {
            (Some(saved), Some(current)) if saved != current => {
                diff.params.push(ParamDiff::Changed {
                    saved: (*saved).clone(),
                    current: (*current).clone(),
                });
            }
            (Some(saved), None) => diff.params.push(ParamDiff::Missing {
                record: (*saved).clone(),
            }),
            (None, Some(current)) => diff.params.push(ParamDiff::Extra {
                record: (*current).clone(),
            }),
            _ => {}
        }
    }

    diff
}

fn plugin_error(kind: SaveErrorKind) -> SaveError {
    SaveError::semantic("load.meta.plugins", kind)
}

fn mod_error(kind: SaveErrorKind) -> SaveError {
    SaveError::semantic("load.meta.mods", kind)
}

fn action_error(kind: SaveErrorKind) -> SaveError {
    SaveError::semantic("load.meta.actions", kind)
}

/// 阶段二：把已校验的世界参数写回 `ParamTable`（必须在 `WorldParamsFrozen` 之前）。
fn apply_meta_params(world: &mut World, meta: &WorldMeta) -> Result<(), SaveError> {
    if meta.params.is_empty() {
        return Ok(());
    }
    let Some(mut table) = world.get_resource_mut::<ParamTable>() else {
        return Err(missing_resource("ParamTable"));
    };
    table.apply_saved(&meta.params).map_err(param_error)
}

fn param_error(error: game_engine::modding::ParamError) -> SaveError {
    SaveError::semantic(
        "load.meta.params",
        SaveErrorKind::Param {
            detail: error.to_string(),
        },
    )
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
#[path = "load/tests.rs"]
mod tests;
