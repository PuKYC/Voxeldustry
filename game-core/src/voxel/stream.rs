//! 流式加载器：驻留档位变化的**数据生命周期**（装载 / 卸载 / 等待存档）
//! 与**渲染档落地**（分组实体 + 表现层缓存失效）。
//!
//! ## 职责边界
//!
//! - [Residency::Data]：把 [ResidencyChanged] 的升档请求变成「体素容器里真的
//!   有这棵树」，把降档请求变成「树被真正释放」。
//! - [Residency::Render]：在 Data 档**完成之后**真的产出该块的渲染分组实体
//!   （[VoxChunkGroupRaw]，`lod: 0`，单块单组）：先从容器取树 →
//!   [extract_raw_halo]（34³，[Neighbourhood::none] 让每块自封边，与 terrain
//!   的面邻居缺失策略一致）→ 全空气就记 [RenderGroups] `None`（已到达、
//!   无几何），否则 spawn 分组实体。消息顺序固定：`ChunkReached{Data}` 在前，
//!   `ChunkReached{Render}` 在后。
//!
//! ## 档位语义
//!
//! - `None -> Data | Render`：入队 → 存档命中 / 真生成 / 已在容器，结算后发
//!   `ChunkReached{Data}`；目标档是 Render 时再产出分组实体并发
//!   `ChunkReached{Render}`。
//! - `Render -> Data`：销毁分组实体 + 失效四份表现层缓存（`wrapped`、
//!   `generated.own`、`generated.neighbors`、`neighbors.0`），**保留体素
//!   内容**；不发 [ChunkWillUnload]、不计 `unloaded`。
//! - `Render | Data -> None`：先销毁分组实体（若存在）、把 `(body, key, from)`
//!   记入 [PendingUnloads]；整块内容与 `Persist` 实体落盘后才真正释放（见下）。
//! - 重复消息幂等：同一 `(body, key)` 最多一个分组实体；`Render` 消息只在
//!   首次产出（或首次判定全空气）时发一次。
//!
//! ## 每 tick 的流水线
//!
//! 卸载拆成**同 tick 顺序执行**的三段，保证数据先写出去再释放：
//!
//! 1. [intake_residency_changes]：消费 [ResidencyChanged]。
//!    - 升档：`GenTable::enqueue` + 记入 [StreamWanted]；若块**已经在**
//!      `VoxVolume` 里且生成态还是 `Undetermined`（地形 Startup 已生成 /
//!      上一轮装载留下的数据），直接结算并发 [ChunkReached]，不重生成；
//!    - 降档（`to == None`，或 `Render -> Data`）：只有「请求还在队列里」或
//!      「块真的还在容器里」才动作，因此重复的降档消息既不二次释放也不二次计数。
//!      `to == None` 时只取消生成、发 [ChunkWillUnload]、把 `(pair, from)` 记进
//!      [PendingUnloads]；同一 pair 已在表里就什么都不再做（去重）。
//! 2. [persist_pending_unloads]：每 tick 至多 `capture` 一次实体快照，把每个
//!    待卸载块的整块 content 与该块的 `Persist` 实体组装成
//!    [ChunkRecord](game_engine::streaming::ChunkRecord) 写进
//!    [SaveStore](game_engine::save::SaveStore)。没有存档资源（headless / 单测）
//!    时整个阶段跳过，**不动** [PendingUnloads]。
//! 3. [release_pending_unloads]：取走 [PendingUnloads]，逐块做真正的释放
//!    （四份缓存 `unload_chunk`、`GenTable::reset`、`stats.unloaded`）；
//!    第 2 段写盘失败也照常释放，块不会被卡住。
//! 4. [resolve_chunks]（`Wanted` 键序，每 tick 最多
//!    [VoxelStreamConfig::max_resolve_per_tick] 个）：问
//!    [ChunkPersistence]：`Hit` 且有整块内容 → 装树，生成器零调用；
//!    `Hit` 无内容 / `Miss` → 真正跑 [WorldGenerator]；`Pending` → 留在队列，
//!    下一 tick 再问。
//!
//! ## 实体恢复
//!
//! 存档命中且记录带实体、整块内容装成功时，[resolve_chunks] 发一条
//! [ChunkEntities]（生成路径、空实体、intake 的「块已在容器」快路径都不发——
//! 启动期的实体已由 `load_world` 应用）。真正的重建由 `WorldChunkPlugin` 的
//! Lifecycle 阶段消费这条消息完成，流式层只负责把记录递过去。
//!
//! ## 依赖
//!
//! 插件假定 [GameVoxelPlugin](crate::voxel::GameVoxelPlugin) 已装配，并且
//! [ResidencyChanged] / [ChunkReached] 的消息资源存在（生产装配由
//! `ResidencyPlugin` 注册）——本插件只注册自己的 [ChunkWillUnload]。
//!
//! ## 顺序与确定性
//!
//! 队列是 [BTreeMap]：键序 = `(BodyKey, ChunkKey)` 升序，预算裁剪按同一顺序，
//! 不依赖哈希/墙钟。`GenTable` 只承担调度轴（`enqueue` / `begin` /
//! `finish` / `cancel` / `reset`）；内容态（`Determined`）留给提案/裁决管线，
//! 本轮不写它。

use std::collections::BTreeMap;

use bevy::prelude::*;
use game_engine::body_key::BodyKey;
use game_engine::identity::{StableEntityIndex, StableIdAllocator};
use game_engine::presentation::voxel::{
    extract_raw_halo, raw_halo_has_solid_interior, WrappedBlockCache, RAW_VOXELS,
};
use game_engine::presentation::RenderTransform;
use game_engine::residency::{ChunkReached, GameTickSet, Residency, ResidencyChanged};
use game_engine::save::{
    capture, export_chunk, EntityRecord, OpaqueComponents, PersistRegistry, SaveStore,
};
use game_engine::spatial::Size;
use game_engine::streaming::{ChunkPersistence, ChunkRecord, GenState, GenTable, StoreLookup};
use game_engine::voxel::{
    dec_ref_recursive, ChunkKey, Lod, MeshBlock, Neighbourhood, VoxTree, VoxVolume, VoxelInterner,
};

use crate::presentation::{PresentedTransform, VoxChunkGroupRaw};
use crate::static_data::voxel::{DEFAULT_BIOME, DEFAULT_VOXEL_SIZE};
use crate::voxel::terrain::blocks::block_origin_meters;
use crate::voxel::terrain::generation::WorldGenerator;
use crate::voxel::terrain::{
    unload_chunk, TerrainConfig, TerrainGeneratedBlocks, TerrainNeighborBlocks,
};
use crate::world::WorldSeed;

/// 流式加载配置。
#[derive(Resource, Clone, Debug, PartialEq, Eq)]
pub struct VoxelStreamConfig {
    /// 每 tick 最多结算多少个排队区块（`Pending` 也占一个名额，
    /// 避免队首被存档卡住时饿死后面的块）。
    pub max_resolve_per_tick: usize,
    /// 生成器版本（按块记录在 [ChunkRecord](game_engine::streaming::ChunkRecord) 里）。
    ///
    /// 它只影响**提案**能否复用（规格 §6.2），不决定整块内容能否安装：
    /// 存档 `content` 永远以存档为准。本轮没有提案管线，所以这个字段只由
    /// 装配方写入、由存档层读取。
    pub generator_version: u32,
}

impl Default for VoxelStreamConfig {
    fn default() -> Self {
        Self {
            max_resolve_per_tick: 8,
            generator_version: 1,
        }
    }
}

/// 流式加载统计（每 tick 由 [resolve_chunks] 刷新）。
#[derive(Resource, Debug, Default, PartialEq, Eq)]
pub struct StreamStats {
    /// 当前等待结算的区块数（[StreamWanted] 长度）。
    pub queued: usize,
    /// 其中本 tick 仍在等存档回包（`StoreLookup::Pending`）的数量。
    pub pending: usize,
    /// 累计从存档装树的块数。
    pub loaded_from_store: u64,
    /// 累计生成器产出的块数。
    pub generated: u64,
    /// 累计真正卸载的块数（重复降档消息不重复计数）。
    pub unloaded: u64,
    /// 当前存活的分组实体数（[RenderGroups] 里 `Some` 的数量），每次增删后刷新。
    pub render_groups: usize,
}

/// 即将卸载一个区块的**数据**（在释放之前发出，监听方还能读到内容）。
///
/// 只有 `to == None` 的降档才发这个信息：`Render -> Data` 只是渲染档清理，
/// 不属于本轮数据生命周期的卸载。
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkWillUnload {
    /// 所属体素容器（body）。
    pub body: BodyKey,
    /// 要被卸载的区块。
    pub key: ChunkKey,
    /// 卸载前的驻留档位。
    pub from: Residency,
    /// 卸载后的驻留档位（恒为 [Residency::None]）。
    pub to: Residency,
}

/// 存档命中装块成功后要恢复的实体记录。
///
/// 由 [resolve_chunks] 在 `Hit(record)` 且 `record.entities` 非空、整块内容
/// `install_chunk` 成功之后发出；生成路径与空实体不发。消费方是
/// `WorldChunkPlugin` 的 Lifecycle 阶段（同一 tick，Streaming 之后）。
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct ChunkEntities {
    /// 所属体素容器（body）。
    pub body: BodyKey,
    /// 实体记录归属的区块。
    pub key: ChunkKey,
    /// 该块的实体记录，保持存档顺序（消费方自行按稳定 ID 排序）。
    pub entities: Vec<EntityRecord>,
}

/// 待结算队列：`(body, key) -> 目标档位`，键序即结算顺序。
#[derive(Resource, Debug, Default)]
pub struct StreamWanted(pub BTreeMap<(BodyKey, ChunkKey), Residency>);

/// 待落盘并释放的区块：`(body, key) -> 卸载前档位`。
///
/// 由 [intake_residency_changes] 的 `to == None` 分支记入，
/// [persist_pending_unloads] 把内容与实体写进存档后，由
/// [release_pending_unloads] 取走并真正释放。键序 = `(BodyKey, ChunkKey)`
/// 升序，落盘与释放都按同一顺序，不依赖哈希。
#[derive(Resource, Debug, Default)]
pub struct PendingUnloads(pub BTreeMap<(BodyKey, ChunkKey), Residency>);

/// 已到达 [Residency::Render] 的块 -> 分组实体。
///
/// 值 `None` 表示「已判定为全空气、没有几何」——它同样代表该块**已经到达**
/// Render 档，因此重复的 Render 请求可以直接忽略，不必重复提取 34³ halo。
/// 只有 `Some(entity)` 才是真正存活的分组实体（`StreamStats::render_groups`
/// 统计的就是它的数量）。
#[derive(Resource, Debug, Default)]
pub struct RenderGroups(pub BTreeMap<(BodyKey, ChunkKey), Option<Entity>>);

/// Data 档流式加载插件。
pub struct VoxelStreamPlugin;

impl Plugin for VoxelStreamPlugin {
    fn build(&self, app: &mut App) {
        assert!(
            app.is_plugin_added::<super::GameVoxelPlugin>(),
            "VoxelStreamPlugin 依赖 GameVoxelPlugin"
        );

        app.init_resource::<VoxelStreamConfig>()
            .init_resource::<StreamStats>()
            .init_resource::<GenTable>()
            .init_resource::<ChunkPersistence>()
            .init_resource::<StreamWanted>()
            .init_resource::<WrappedBlockCache>()
            .init_resource::<TerrainGeneratedBlocks>()
            .init_resource::<TerrainNeighborBlocks>()
            .init_resource::<RenderGroups>()
            .init_resource::<PendingUnloads>()
            .add_message::<ChunkWillUnload>()
            .add_message::<ChunkEntities>()
            .add_systems(
                FixedUpdate,
                (
                    intake_residency_changes,
                    persist_pending_unloads,
                    release_pending_unloads,
                    resolve_chunks,
                )
                    .chain()
                    .in_set(GameTickSet::Streaming),
            );
    }
}

/// 系统 1：把 [ResidencyChanged] 折成队列状态、渲染分组与真正的卸载。
#[allow(clippy::too_many_arguments)]
pub fn intake_residency_changes(
    mut commands: Commands,
    mut changes: MessageReader<ResidencyChanged>,
    mut gen: ResMut<GenTable>,
    mut wanted: ResMut<StreamWanted>,
    mut pending: ResMut<PendingUnloads>,
    mut will_unload: MessageWriter<ChunkWillUnload>,
    mut reached: MessageWriter<ChunkReached>,
    mut index: Option<ResMut<StableEntityIndex>>,
    mut allocator: Option<ResMut<StableIdAllocator>>,
    mut volumes: Query<&mut VoxVolume>,
    mut interner: ResMut<VoxelInterner>,
    mut wrapped: ResMut<WrappedBlockCache>,
    mut generated: ResMut<TerrainGeneratedBlocks>,
    mut neighbors: ResMut<TerrainNeighborBlocks>,
    mut groups: ResMut<RenderGroups>,
    mut stats: ResMut<StreamStats>,
) {
    for change in changes.read() {
        let ResidencyChanged { body, key, from, to } = *change;
        let pair = (body, key);

        // 降档：`to == None` 是真卸载；`Render -> Data` 只是渲染档清理
        //（体素内容保留）。两者都交给 demote，由它按 from/to 分流。
        if to == Residency::None || (from == Residency::Render && to != Residency::Render) {
            demote(
                pair,
                from,
                to,
                &mut commands,
                index.as_deref_mut(),
                &mut gen,
                &mut wanted,
                &mut pending,
                &mut will_unload,
                &mut volumes,
                &mut interner,
                &mut wrapped,
                &mut generated,
                &mut neighbors,
                &mut groups,
                &mut stats,
            );
            continue;
        }

        // 幂等：该块已经到过 Render（有分组实体，或已判定全空气）时，
        // 重复的 Render 请求不再入队、不再发第二条 Render 消息。
        if to == Residency::Render && groups.0.contains_key(&pair) {
            continue;
        }

        // 升档：入队（已存在的 Err 不是错误——同一块重复请求是常态），
        // 再用「Wanted 是否新插入」判定这是不是一次新请求，避免重复发
        // ChunkReached。
        if let Err(error) = gen.enqueue(pair) {
            debug!("stream: enqueue({body:?}, {key:?}) 被拒绝：{error:?}");
        }
        let is_new = wanted.0.insert(pair, to).is_none();
        if !is_new {
            continue;
        }

        let Some(entity) = body_entity(index.as_deref(), body) else {
            continue;
        };
        let Ok(volume) = volumes.get(entity) else {
            continue;
        };
        if !volume.contains_chunk(&key) || gen.gen_state(pair) != GenState::Undetermined {
            continue;
        }

        // 数据已在容器里：直接结算，不重生成、不查存档。
        // 快路径同样走 finish_chunk，Render 档才会拿到分组实体。
        let raw = extract_block_raw(&volume, &interner, key, to);
        finish_chunk(
            pair,
            to,
            raw,
            &mut allocator,
            index.as_deref_mut(),
            &mut commands,
            &mut groups,
            &mut reached,
            &mut gen,
            &mut wanted,
            &mut stats,
        );
    }
}

/// 系统 2：按队列键序结算，最多 [VoxelStreamConfig::max_resolve_per_tick] 个。
#[allow(clippy::too_many_arguments)]
pub fn resolve_chunks(
    mut commands: Commands,
    config: Res<VoxelStreamConfig>,
    mut gen: ResMut<GenTable>,
    mut wanted: ResMut<StreamWanted>,
    persistence: Res<ChunkPersistence>,
    mut index: Option<ResMut<StableEntityIndex>>,
    mut allocator: Option<ResMut<StableIdAllocator>>,
    mut volumes: Query<&mut VoxVolume>,
    mut interner: ResMut<VoxelInterner>,
    seed: Option<Res<WorldSeed>>,
    terrain: Option<Res<TerrainConfig>>,
    mut reached: MessageWriter<ChunkReached>,
    mut entities_out: MessageWriter<ChunkEntities>,
    mut groups: ResMut<RenderGroups>,
    mut stats: ResMut<StreamStats>,
) {
    let budget = config.max_resolve_per_tick;
    let keys: Vec<((BodyKey, ChunkKey), Residency)> = wanted
        .0
        .iter()
        .take(budget)
        .map(|(pair, level)| (*pair, *level))
        .collect();
    let mut pending = 0usize;

    for (pair, level) in keys {
        let (body, key) = pair;

        let Some(entity) = body_entity(index.as_deref(), body) else {
            // body 没有实体就没有容器：这条请求永远无法完成，记错并出队。
            error!("stream: body {body:?} 找不到实体，放弃区块 {key:?}");
            drop_request(&mut gen, &mut wanted, pair);
            continue;
        };
        let Ok(mut volume) = volumes.get_mut(entity) else {
            error!("stream: 实体 {entity:?} 没有 VoxVolume，放弃区块 {key:?}");
            drop_request(&mut gen, &mut wanted, pair);
            continue;
        };

        if volume.contains_chunk(&key) {
            // 防御：队列里的块被别的路径装进去了（`insert_chunk` 覆盖会
            // debug_assert），这里只补发完成消息。
            let raw = extract_block_raw(&volume, &interner, key, level);
            finish_chunk(
                pair,
                level,
                raw,
                &mut allocator,
                index.as_deref_mut(),
                &mut commands,
                &mut groups,
                &mut reached,
                &mut gen,
                &mut wanted,
                &mut stats,
            );
            continue;
        }

        match persistence.lookup(body, key) {
            StoreLookup::Pending => pending += 1,
            StoreLookup::Hit(record) => {
                let ChunkRecord {
                    content, entities, ..
                } = record;
                match content {
                    Some(bytes) => {
                        match game_engine::save::install_chunk(
                            &mut volume,
                            key,
                            &bytes,
                            interner.inner_mut(),
                        ) {
                            Ok(old) => {
                                release_tree(&mut interner, old);
                                // 装块成功后才发；生成路径与空实体不发。
                                if !entities.is_empty() {
                                    entities_out.write(ChunkEntities {
                                        body,
                                        key,
                                        entities,
                                    });
                                }
                                let raw = extract_block_raw(&volume, &interner, key, level);
                                finish_chunk(
                                    pair,
                                    level,
                                    raw,
                                    &mut allocator,
                                    index.as_deref_mut(),
                                    &mut commands,
                                    &mut groups,
                                    &mut reached,
                                    &mut gen,
                                    &mut wanted,
                                    &mut stats,
                                );
                                stats.loaded_from_store += 1;
                            }
                            Err(error) => {
                                error!("stream: 存档内容装不进区块 {key:?}：{error}");
                                drop_request(&mut gen, &mut wanted, pair);
                            }
                        }
                    }
                    None => {
                        if generate_into(
                            pair,
                            &mut volume,
                            &mut interner,
                            seed.as_deref(),
                            terrain.as_deref(),
                            &mut gen,
                            &mut wanted,
                            &mut stats,
                        ) {
                            let raw = extract_block_raw(&volume, &interner, key, level);
                            finish_chunk(
                                pair,
                                level,
                                raw,
                                &mut allocator,
                                index.as_deref_mut(),
                                &mut commands,
                                &mut groups,
                                &mut reached,
                                &mut gen,
                                &mut wanted,
                                &mut stats,
                            );
                        }
                    }
                }
            }
            StoreLookup::Miss => {
                if generate_into(
                    pair,
                    &mut volume,
                    &mut interner,
                    seed.as_deref(),
                    terrain.as_deref(),
                    &mut gen,
                    &mut wanted,
                    &mut stats,
                ) {
                    let raw = extract_block_raw(&volume, &interner, key, level);
                    finish_chunk(
                        pair,
                        level,
                        raw,
                        &mut allocator,
                        index.as_deref_mut(),
                        &mut commands,
                        &mut groups,
                        &mut reached,
                        &mut gen,
                        &mut wanted,
                        &mut stats,
                    );
                }
            }
        }
    }

    stats.queued = wanted.0.len();
    stats.pending = pending;
}
/// 降档处理：先按需清理渲染档，再把数据卸载**登记**进 [PendingUnloads]。
///
/// **重复消息不二次释放的判据**：第一次降档会把队列项移除、把 `(pair, from)`
/// 记进 [PendingUnloads]；第二次同样的消息在去重检查处直接返回——不发
/// [ChunkWillUnload]、不重复登记。真正的释放（缓存 / `GenTable::reset` /
/// `stats.unloaded`）由 [release_pending_unloads] 在本 tick 稍后统一做，
/// 中间隔着 [persist_pending_unloads] 的落盘，保证数据先写出去。
///
/// `Render -> Data` 只销毁分组实体 + 失效四份表现层缓存，体素内容原样保留，
/// 因此**不**进 [PendingUnloads]、**不**发卸载消息、**不**计 `unloaded`。
#[allow(clippy::too_many_arguments)]
fn demote(
    pair: (BodyKey, ChunkKey),
    from: Residency,
    to: Residency,
    commands: &mut Commands<'_, '_>,
    mut index: Option<&mut StableEntityIndex>,
    gen: &mut GenTable,
    wanted: &mut StreamWanted,
    pending: &mut PendingUnloads,
    will_unload: &mut MessageWriter<ChunkWillUnload>,
    volumes: &mut Query<&mut VoxVolume>,
    interner: &mut VoxelInterner,
    wrapped: &mut WrappedBlockCache,
    generated: &mut TerrainGeneratedBlocks,
    neighbors: &mut TerrainNeighborBlocks,
    groups: &mut RenderGroups,
    stats: &mut StreamStats,
) {
    let (body, key) = pair;

    // 1) 渲染档先退：实体销毁 + 四份表现层缓存失效（体素内容保留）。
    if from == Residency::Render && to != Residency::Render {
        despawn_render_group(pair, commands, groups, index.as_deref_mut(), stats);
        wrapped.invalidate_covered(interner.inner_mut(), key);
        generated.own.invalidate_covered(interner.inner_mut(), key);
        generated.neighbors.invalidate_covered(interner.inner_mut(), key);
        neighbors.0.invalidate_covered(interner.inner_mut(), key);
    }

    // 只是降渲染档（Render -> Data）：数据留在容器里，到此为止。
    if to != Residency::None {
        if wanted.0.remove(&pair).is_some() {
            gen.cancel(pair);
            let _ = gen.reset(pair);
        }
        return;
    }

    let removed = wanted.0.remove(&pair).is_some();
    let entity = body_entity(index.as_deref(), body);
    let present = entity
        .and_then(|entity| volumes.get(entity).ok())
        .is_some_and(|volume| volume.contains_chunk(&key));
    if !removed && !present {
        return;
    }

    // 去重：同一 pair 已经登记过（同一 tick 里的第二次降档）就什么都不再做。
    // 释放要等 [persist_pending_unloads] 写完存档，此时树还在容器里，所以
    // `present` 仍然成立，第二次消息必须在这里被挡住。
    if pending.0.contains_key(&pair) {
        return;
    }

    gen.cancel(pair);
    will_unload.write(ChunkWillUnload { body, key, from, to });
    pending.0.insert(pair, from);
}

/// 系统 2：把待卸载区块的整块内容与 `Persist` 实体写进存档（exclusive）。
///
/// 顺序在 [intake_residency_changes] 之后、[release_pending_unloads] 之前：
/// 写盘先于释放，导出 content 时树还在容器里。
///
/// - [PendingUnloads] 为空，或没有 [SaveStore]（headless / 单测）→ 直接返回，
///   不改 pending，释放阶段照常执行；
/// - 每 tick 至多 [capture] 一次实体快照，失败只 `warn!`，本 tick 不写盘，
///   但**不阻断**释放；
/// - 空块（content 与 entities 都空）不写记录，改为删除该块的旧记录：磁盘上可能
///   还留着上一次的 `ChunkRecord`，静默跳过会让旧内容在重载后复活；
/// - [SaveStore::write] / [SaveStore::delete] 的 `Err` 只 `warn!`，同样不阻断释放。
pub fn persist_pending_unloads(world: &mut World) {
    let has_pending = world
        .get_resource::<PendingUnloads>()
        .is_some_and(|pending| !pending.0.is_empty());
    if !has_pending {
        return;
    }
    // 没有存档（headless / 单测）：跳过写盘，但保留 pending，让释放照常发生。
    if world.get_resource::<SaveStore>().is_none() {
        return;
    }

    let world: &World = &*world;
    let snapshot = {
        let (Some(registry), Some(opaque)) = (
            world.get_resource::<PersistRegistry>(),
            world.get_resource::<OpaqueComponents>(),
        ) else {
            warn!("stream: 卸载落盘缺少 PersistRegistry / OpaqueComponents，本 tick 跳过写盘");
            return;
        };
        match capture(world, registry, opaque) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                warn!("stream: 卸载落盘捕获实体快照失败，本 tick 跳过写盘：{error}");
                return;
            }
        }
    };

    let Some(pending) = world.get_resource::<PendingUnloads>() else {
        return;
    };
    let Some(store) = world.get_resource::<SaveStore>() else {
        return;
    };
    let generator_version = world.resource::<VoxelStreamConfig>().generator_version;
    let Some(interner) = world.get_resource::<VoxelInterner>() else {
        warn!("stream: 卸载落盘缺少 VoxelInterner，本 tick 跳过写盘");
        return;
    };
    let index = world.get_resource::<StableEntityIndex>();

    for ((body, key), _from) in &pending.0 {
        let content = body_entity(index, *body)
            .and_then(|entity| world.get::<VoxVolume>(entity))
            .and_then(|volume| export_chunk(volume, *key, interner.inner()));
        let entities = snapshot.for_chunk(*body, *key).to_vec();
        if content.is_none() && entities.is_empty() {
            // 空块：既没有整块内容也没有 Persist 实体，validate 会拒绝全空记录。
            // 不能静默跳过：磁盘上可能还有这块的旧记录，必须显式删除，否则重载后
            // 旧内容会复活。删除与写入一样只在 Flush 落盘，这里只标墓碑。
            if let Err(error) = store.delete(*body, *key) {
                warn!("stream: 卸载落盘删除空区块 {key:?} 失败（不阻断释放）：{error:?}");
            }
            continue;
        }
        let record = ChunkRecord {
            generator_version,
            content,
            proposals: Vec::new(),
            entities,
        };
        if let Err(error) = store.write(*body, *key, record) {
            warn!("stream: 卸载落盘写区块 {key:?} 失败（不阻断释放）：{error:?}");
        }
    }
}

/// 系统 3：真正释放 [PendingUnloads] 里的区块（exclusive）。
///
/// 取走整张表后逐块做旧 demote 的收尾：四份缓存的 [unload_chunk]（顺序与旧
/// 实现一致）、[GenTable::reset]、`stats.unloaded += 1`。落盘阶段失败或根本
/// 没有存档时也照常走到这里，块不会被卡死。
pub fn release_pending_unloads(world: &mut World) {
    let pending = {
        let Some(mut pending) = world.get_resource_mut::<PendingUnloads>() else {
            return;
        };
        if pending.0.is_empty() {
            return;
        }
        std::mem::take(&mut pending.0)
    };

    for ((body, key), from) in pending {
        release_unloaded_chunk(world, body, key, from);
    }
}

/// 释放单个已登记卸载的区块（exclusive 版本，语义与旧 demote 收尾一致）。
fn release_unloaded_chunk(world: &mut World, body: BodyKey, key: ChunkKey, from: Residency) {
    let entity = body_entity(world.get_resource::<StableEntityIndex>(), body);
    let freed = release_chunk_from_volume(world, entity, key);
    if !freed {
        // 正常的「排队中就被撤回」：没有树可释放，缓存已失效。
        debug!("stream: 区块 {key:?}（{from:?} -> None）卸载时容器里没有树（请求被撤回）");
    }
    let _ = world.resource_mut::<GenTable>().reset((body, key));
    world.resource_mut::<StreamStats>().unloaded += 1;
}

/// 从 body 的容器里取树并按旧顺序失效四份缓存，返回是否真的取回了树。
fn release_chunk_from_volume(world: &mut World, entity: Option<Entity>, key: ChunkKey) -> bool {
    let Some(entity) = entity else {
        return false;
    };
    // 实体可能已被别的路径 despawn：先探一下，避免 entity_mut panic。
    if world.get::<VoxVolume>(entity).is_none() {
        return false;
    }
    world.resource_scope::<VoxelInterner, _>(|world, mut interner| {
        world.resource_scope::<WrappedBlockCache, _>(|world, mut wrapped| {
            world.resource_scope::<TerrainGeneratedBlocks, _>(|world, mut generated| {
                world.resource_scope::<TerrainNeighborBlocks, _>(|world, mut neighbors| {
                    let mut body_mut = world.entity_mut(entity);
                    let Some(mut volume) = body_mut.get_mut::<VoxVolume>() else {
                        return false;
                    };
                    // 四份缓存实例缺一不可：只有真正从容器取回树的那次返回 true，
                    // 其余调用只负责失效各自的 LOD 缓存。
                    let mut freed = unload_chunk(
                        &mut interner,
                        &mut volume,
                        &mut wrapped,
                        &mut generated.own,
                        key,
                    );
                    freed |= unload_chunk(
                        &mut interner,
                        &mut volume,
                        &mut neighbors.0,
                        &mut generated.own,
                        key,
                    );
                    freed |= unload_chunk(
                        &mut interner,
                        &mut volume,
                        &mut wrapped,
                        &mut generated.neighbors,
                        key,
                    );
                    freed |= unload_chunk(
                        &mut interner,
                        &mut volume,
                        &mut neighbors.0,
                        &mut generated.neighbors,
                        key,
                    );
                    freed
                })
            })
        })
    })
}

/// 真正跑生成器并把树装进容器（旧树释放引用），返回是否装好。
///
/// 完成消息与渲染分组由调用方统一走 [finish_chunk]，这里只负责数据。
#[allow(clippy::too_many_arguments)]
fn generate_into(
    pair: (BodyKey, ChunkKey),
    volume: &mut VoxVolume,
    interner: &mut VoxelInterner,
    seed: Option<&WorldSeed>,
    terrain: Option<&TerrainConfig>,
    gen: &mut GenTable,
    wanted: &mut StreamWanted,
    stats: &mut StreamStats,
) -> bool {
    let (_, key) = pair;
    let Some(seed) = seed else {
        // 没有种子就没有确定性世界：移除而不是无限重试。
        error!("stream: WorldSeed 资源缺失，无法生成区块 {key:?}");
        drop_request(gen, wanted, pair);
        return false;
    };
    let biome = terrain.map_or(DEFAULT_BIOME, |config| config.biome);
    let tree = WorldGenerator::new(*seed, biome).generate_chunk(interner.inner_mut(), key);
    let old = volume.insert_chunk(key, tree);
    release_tree(interner, old);
    stats.generated += 1;
    true
}

/// Data 档的**统一出口**：先发 `ChunkReached{Data}`，目标档是 Render 时
/// 再产出分组实体、发 `ChunkReached{Render}`。
///
/// 四个成功路径（存档命中 / 真生成 / 已在容器快路径 / resolve 防御分支）
/// 都必须从这里出，否则 Render 档会漏掉分组实体。
#[allow(clippy::too_many_arguments)]
fn finish_chunk(
    pair: (BodyKey, ChunkKey),
    level: Residency,
    raw: Option<Vec<u8>>,
    allocator: &mut Option<ResMut<'_, StableIdAllocator>>,
    index: Option<&mut StableEntityIndex>,
    commands: &mut Commands<'_, '_>,
    groups: &mut RenderGroups,
    reached: &mut MessageWriter<ChunkReached>,
    gen: &mut GenTable,
    wanted: &mut StreamWanted,
    stats: &mut StreamStats,
) {
    let (body, key) = pair;
    // 顺序：Data 消息在前，渲染实体在后，最后才是 Render 消息。
    settle(gen, pair);
    reached.write(ChunkReached {
        body,
        key,
        level: Residency::Data,
    });
    wanted.0.remove(&pair);

    if level != Residency::Render {
        return;
    }
    let Some(raw) = raw else {
        // 树不在容器里（或已产出过）：保持 Data 档完成，不伪造 Render。
        debug!("stream: 区块 {key:?} 没有可提取的树，跳过分组产出");
        return;
    };
    match ensure_render_group(pair, &raw, allocator, index, commands, groups) {
        Ok(true) => {
            refresh_render_groups(groups, stats);
            reached.write(ChunkReached {
                body,
                key,
                level: Residency::Render,
            });
        }
        Ok(false) => debug!("stream: 区块 {key:?} 已有渲染分组，跳过重复产出"),
        Err(error) => error!("stream: 区块 {key:?} 无法产出渲染分组：{error}"),
    }
}

/// 从容器取树并抽 34³ 原始体素；只有目标档是 Render 才需要。
///
/// 无邻块时用 [Neighbourhood::none]：每块自封边，与 terrain 的面邻居缺失
/// 策略一致。
fn extract_block_raw(
    volume: &VoxVolume,
    interner: &VoxelInterner,
    key: ChunkKey,
    level: Residency,
) -> Option<Vec<u8>> {
    if level != Residency::Render {
        return None;
    }
    let tree = volume.get_chunk(&key)?;
    let raw = extract_raw_halo(
        tree,
        interner.inner(),
        MeshBlock::new(key, Lod::new(0)),
        Neighbourhood::none(),
    );
    debug_assert_eq!(raw.len(), RAW_VOXELS, "extract_raw_halo 必须产出 34³");
    Some(raw)
}

/// 确保该块有渲染分组实体；返回「本次是否新产出了这个块」。
///
/// - 已在 [RenderGroups] 里（实体或全空气判定）→ `Ok(false)`，幂等。
/// - 内层 32³ 全空气 → 记 `None`（已到达、无几何），`Ok(true)`。
/// - 否则 spawn 分组实体并登记进 [StableEntityIndex]，`Ok(true)`。
fn ensure_render_group(
    pair: (BodyKey, ChunkKey),
    raw: &[u8],
    allocator: &mut Option<ResMut<'_, StableIdAllocator>>,
    index: Option<&mut StableEntityIndex>,
    commands: &mut Commands<'_, '_>,
    groups: &mut RenderGroups,
) -> Result<bool, &'static str> {
    if groups.0.contains_key(&pair) {
        return Ok(false);
    }
    let (_, key) = pair;
    debug_assert_eq!(raw.len(), RAW_VOXELS, "extract_raw_halo 必须产出 34³");

    // 全空气块：没有几何可渲染，但仍算「已到达 Render」，防止重复提取。
    if !raw_halo_has_solid_interior(raw) {
        groups.0.insert(pair, None);
        return Ok(true);
    }

    let (Some(allocator), Some(index)) = (allocator.as_deref_mut(), index) else {
        return Err("缺少 StableIdAllocator / StableEntityIndex 资源");
    };
    let id = allocator.allocate();
    let position = Vec3::from_array(block_origin_meters(key, voxel_size_f32()));
    let mut presented = PresentedTransform::default();
    presented.set_sampled(
        RenderTransform::from_translation(position),
        RenderTransform::from_translation(position),
    );
    // 单块单组：lod 0，自封边；掩码 0（无跨 LOD / 无邻块的裙边需求）。
    let mut chunks = BTreeMap::new();
    chunks.insert(key, (raw.to_vec(), 0u8));
    let entity = commands
        .spawn((
            id,
            VoxChunkGroupRaw { lod: 0, chunks },
            Transform::from_translation(position),
            Size(None),
            presented,
        ))
        .id();
    index.insert(id, entity);
    groups.0.insert(pair, Some(entity));
    Ok(true)
}

/// 销毁该块的分组实体并清掉 [RenderGroups] 里的记录（幂等）。
fn despawn_render_group(
    pair: (BodyKey, ChunkKey),
    commands: &mut Commands<'_, '_>,
    groups: &mut RenderGroups,
    index: Option<&mut StableEntityIndex>,
    stats: &mut StreamStats,
) {
    if let Some(Some(entity)) = groups.0.remove(&pair) {
        if let Some(index) = index {
            index.remove_by_entity(entity);
        }
        commands.entity(entity).despawn();
    }
    refresh_render_groups(groups, stats);
}

/// 把 `stats.render_groups` 刷成存活分组实体数。
fn refresh_render_groups(groups: &RenderGroups, stats: &mut StreamStats) {
    stats.render_groups = groups.0.values().filter(|slot| slot.is_some()).count();
}

/// 单个体素边长（米），与 terrain 分组使用的尺度一致。
fn voxel_size_f32() -> f32 {
    DEFAULT_VOXEL_SIZE.to_num::<f32>()
}
/// 把 `GenTable` 从任意工作态干净地结算回 `Idle`。
///
/// `finish` 只接受 `Running`，所以固定先 `begin` 再 `finish`；
/// 两个结果都容忍（`Idle` / `Running` 都能收敛）。
fn settle(gen: &mut GenTable, pair: (BodyKey, ChunkKey)) {
    let _ = gen.begin(pair);
    let _ = gen.finish(pair);
}

/// 放弃一个请求：取消 → 复位调度态 → 出队（失败路径统一走这里，不重试）。
fn drop_request(gen: &mut GenTable, wanted: &mut StreamWanted, pair: (BodyKey, ChunkKey)) {
    gen.cancel(pair);
    let _ = gen.reset(pair);
    wanted.0.remove(&pair);
}

/// 释放一棵被替换下来的旧树的引用（`VoxTree` 没有 `Drop`）。
fn release_tree(interner: &mut VoxelInterner, tree: Option<VoxTree<u8>>) {
    if let Some(tree) = tree {
        let root = tree.get_root_id();
        dec_ref_recursive(interner.inner_mut(), &root);
    }
}

/// body 的 stable id -> 实体（索引缺失或未登记都返回 `None`）。
fn body_entity(index: Option<&StableEntityIndex>, body: BodyKey) -> Option<Entity> {
    index?.entity(body.stable_id())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    use bevy::prelude::*;
    use game_engine::identity::{StableIdPlugin, StableIdWorldExt};
    use game_engine::residency::ChunkAnchor;
    use game_engine::save::{
        export_chunk, read_region, EntityRecord, OpaqueComponents, Persist, PersistComponent,
        PersistRegistry, SaveConfig, SavePaths, SaveStore, StableTypeId,
    };
    use game_engine::streaming::{ChunkRecord, ChunkStore, StoreLookup};
    use game_engine::voxel::{chunk_content_hash, chunk_key};
    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::static_data::voxel::DEFAULT_VOXEL_SIZE;

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
        let (entity, id) =
            app.world_mut()
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

    impl Harness {
        fn tick(&mut self) {
            self.app.world_mut().run_schedule(FixedUpdate);
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
        harness.tick();

        let reached = harness.reached();
        assert_eq!(reached.len(), 1);
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
        for expected in [first, second, third] {
            harness.tick();
            let reached = harness.reached();
            assert_eq!(reached.len(), 1, "每 tick 恰好推进一个块");
            assert_eq!(reached[0].key, expected, "按队列键序结算");
        }

        assert_eq!(harness.stats().queued, 0);
        assert_eq!(harness.stats().generated, 3);
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
        harness.tick();
        let first_hash = harness.content_hash(&key);
        assert_eq!(first_hash, expected);

        harness.demote(key);
        harness.tick();
        assert!(!harness.volume().contains_chunk(&key));

        harness.upgrade(key);
        harness.tick();
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
        assert_eq!(harness.content_hash(&key), expected, "Data 档必须真的装上存档内容");

        let entities = harness.group_entities();
        assert_eq!(entities.len(), 1, "必须恰好产出一个分组实体");
        let raw = harness.group_payload(entities[0]);
        assert_eq!(raw.lod, 0);
        assert_eq!(raw.chunks.len(), 1, "单块单组：分组里只能有该块");
        let (payload, mask) = raw.chunks.get(&key).expect("分组必须恰含该 key");
        assert_eq!(payload.len(), RAW_VOXELS);
        assert!(raw_halo_has_solid_interior(payload), "有几何的块才该 spawn");
        assert_eq!(*mask, 0, "自封边、无跨 LOD 裙边，掩码为 0");
        assert_eq!(harness.stats().render_groups, 1);
    }

    #[test]
    fn data_only_request_creates_no_render_entity() {
        let key = chunk_key(11, 0, 1);
        let mut harness = harness(vec![Answer::Miss]);

        harness.upgrade(key);
        harness.tick();

        assert_eq!(
            harness.reached(),
            vec![ChunkReached {
                body: harness.body,
                key,
                level: Residency::Data,
            }]
        );
        assert!(harness.group_entities().is_empty(), "Data 档不得 spawn 分组实体");
        assert!(harness.render_groups().0.is_empty());
        assert_eq!(harness.stats().render_groups, 0);
    }

    #[test]
    fn render_is_idempotent() {
        let key = chunk_key(12, 0, 1);
        let mut harness = harness(vec![Answer::Miss]);

        harness.upgrade_to_render(key);
        harness.tick();
        assert_eq!(harness.group_entities().len(), 1);
        assert_eq!(harness.render_messages().len(), 1);

        // 第二条相同的 Render 消息：既不 spawn 第二个实体，也不重发 Render。
        harness.upgrade_to_render(key);
        harness.tick();
        assert_eq!(harness.group_entities().len(), 1, "同一块只能有一个分组实体");
        assert!(harness.render_messages().is_empty(), "重复消息不得重复发 Render");
        assert_eq!(harness.stats().render_groups, 1);
        assert_eq!(harness.stats().generated, 1, "幂等请求不得触发第二次生成");
    }

    #[test]
    fn render_to_data_keeps_voxels_and_drops_entity() {
        let key = chunk_key(13, 0, 1);
        let (_, expected) = generated_bytes(key);
        let mut harness = harness(vec![Answer::Miss]);

        harness.upgrade_to_render(key);
        harness.tick();
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
        harness.tick();
        assert!(harness.alive_nodes() > baseline, "Render 档必须真的装进体素树");
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
        harness.tick();
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
        harness.tick();
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
        harness.tick();
        assert!(harness.volume().contains_chunk(&key));

        harness.demote(key);
        harness.tick();

        assert!(
            !harness.volume().contains_chunk(&key),
            "落盘与释放必须在同一 tick 内完成"
        );
        assert_eq!(harness.stats().unloaded, 1);
        assert_eq!(
            harness.app.world().resource::<SaveStore>().stats().buffered,
            1,
            "卸载必须把记录写进内存缓冲"
        );

        flush_store(&harness);
        let record = read_back(&harness, dir.path(), key).expect("卸载后存档里必须留下该块");
        assert_eq!(
            record.content.as_deref(),
            Some(expected.as_slice()),
            "content 必须与导出字节逐字节相等"
        );
        assert_eq!(record.generator_version, 1);
        assert!(record.entities.is_empty());
        assert!(record.proposals.is_empty());
        assert_eq!(harness.app.world().resource::<SaveStore>().stats().buffered, 0);
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
        harness.tick();
        harness.demote(key);
        harness.tick();

        flush_store(&harness);
        let record = read_back(&harness, dir.path(), key).expect("卸载后存档里必须留下该块");
        assert!(record.content.is_some(), "内容与实体必须落在同一条记录");
        assert_eq!(record.entities.len(), 1, "该块只应有一条实体记录");
        assert_eq!(record.entities[0].stable_entity_id, entity_id.0);
        assert_eq!(record.entities[0].stable_type_id, 7);
    }

    #[test]
    fn unload_without_save_store_still_releases() {
        let key = chunk_key(22, 0, 22);
        let mut harness = harness(vec![Answer::Miss]);
        let baseline = harness.alive_nodes();

        harness.upgrade(key);
        harness.tick();
        assert!(harness.alive_nodes() > baseline, "请求必须先真的装进体素树");

        harness.demote(key);
        harness.tick();

        assert!(!harness.volume().contains_chunk(&key), "块必须被卸载");
        assert_eq!(harness.alive_nodes(), baseline, "节点引用必须回基线");
        assert_eq!(harness.stats().unloaded, 1);
        assert!(
            harness.app.world().resource::<PendingUnloads>().0.is_empty(),
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
        harness.tick();
        let _ = harness.reached();

        harness.demote(key);
        harness.demote(key);
        harness.tick();

        assert_eq!(
            harness.will_unload().len(),
            1,
            "重复降档只许发一条 ChunkWillUnload"
        );
        assert_eq!(harness.stats().unloaded, 1, "重复降档只许释放一次");
        assert!(!harness.volume().contains_chunk(&key));
        assert_eq!(
            harness.app.world().resource::<SaveStore>().stats().buffered,
            1,
            "重复降档只许写一条记录"
        );

        flush_store(&harness);
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

        assert_eq!(harness.stats().unloaded, 1);
        assert_eq!(
            harness.app.world().resource::<SaveStore>().stats().buffered,
            0,
            "无内容无实体的块不得写记录"
        );
        flush_store(&harness);
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

        assert_eq!(harness.stats().unloaded, 1);
        {
            let store = harness.app.world().resource::<SaveStore>();
            assert_eq!(store.stats().buffered, 0, "空块不得写记录");
            assert_eq!(store.stats().tombstones, 1, "空块必须标删除");
            assert_eq!(store.lookup(harness.body, key), StoreLookup::Miss);
        }

        flush_store(&harness);
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
        harness.tick();

        assert!(harness.volume().contains_chunk(&key));
        assert_eq!(harness.stats().generated, 1);
        assert!(
            harness.chunk_entities().is_empty(),
            "生成路径不得发 ChunkEntities"
        );
    }
}
