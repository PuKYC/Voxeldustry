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
//!   的面邻居缺失策略一致）→ **无条件** spawn 分组实体：每个到达 Render 档的
//!   块恒有一个分组实体，全空气块的 `chunks` 里只有一份全空气 34³ 载荷
//!   （渲染出 0 个矩形）。理由：编辑后要能就地刷新，不能等重新加载。
//!   消息顺序固定：`ChunkReached{Data}` 在前，`ChunkReached{Render}` 在后。
//!
//! ## 档位语义
//!
//! - `None -> Data | Render`：入队 → 存档命中 / 真生成 / 已在容器，结算后发
//!   `ChunkReached{Data}`；目标档是 Render 时再产出分组实体并发
//!   `ChunkReached{Render}`。
//! - `Render -> Data`：销毁分组实体 + 失效四份表现层缓存（`wrapped`、
//!   `generated.own`、`generated.neighbors`、`neighbors.0`），**保留体素
//!   内容**；不发 [ChunkWillUnload]、不计 `unloaded`。
//! - `Render | Data -> None`：先销毁分组实体（若存在）、只把 `(body, key,
//!   from)` 记入 [PendingUnloads]；**不**发 [ChunkWillUnload]、**不**释放任何
//!   东西（树与实体都留在容器里）。整块内容与 `Persist` 实体**确认落盘**后
//!   才发消息并真正释放（见下）。
//! - 卸载未确认期间的升档（复活）：把该 pair 从 [PendingUnloads] 移除，树仍
//!   在容器里，走「已在容器」的快路径直接结算，不查存档、不重生成。
//! - 重复消息幂等：同一 `(body, key)` 最多一个分组实体（全空气块也有实体）；
//!   `Render` 消息只在首次产出时发一次。
//!
//! ## 每 tick 的流水线
//!
//! 卸载拆成**同 tick 顺序执行**的三段，保证数据**确认**写出去之后才释放：
//!
//! 1. [intake_residency_changes]：消费 [ResidencyChanged]。
//!    - 升档：先撤销同 pair 的 [PendingUnloads] 登记（复活；树还在容器里，
//!      绝不释放、绝不再发卸载消息），再 `GenTable::enqueue` + 记入
//!      [StreamWanted]；若块**已经在** `VoxVolume` 里且生成态是
//!      `Undetermined`（地形 Startup 已生成 / 上一轮装载留下的数据），或这是
//!      一次复活，直接结算并发 [ChunkReached]，不重生成、不查存档；
//!    - 降档（`to == None`，或 `Render -> Data`）：只有「请求还在队列里」或
//!      「块真的还在容器里」才动作，因此重复的降档消息既不二次释放也不二次计数。
//!      `to == None` 时只 `GenTable::cancel` 并把 `(pair, PendingUnload)`
//!      记进 [PendingUnloads]（不发 [ChunkWillUnload]、不释放）；同一 pair
//!      已在表里就什么都不再做（去重）。
//! 2. [persist_pending_unloads]：只为 `version == None` 的条目 `capture` 一次
//!    实体快照（同一块只提交一次，不每 tick 重写），把整块 content 与该块的
//!    `Persist` 实体组装成
//!    [ChunkRecord](game_engine::streaming::ChunkRecord) 写进
//!    [SaveStore](game_engine::save::SaveStore)（空块改为 `delete`）；
//!    `Ok(v)` 把版本记进条目，`Err` 只在本块首次失败时 `warn!` 并保留条目，
//!    下一 tick 重试。没有存档资源（headless / 单测）时整个阶段跳过。
//! 3. [release_pending_unloads]：逐条判断，只有「没有 [SaveStore]」、
//!    「store 没有后台线程（永远不会确认落盘）」或「`version` 已确认落盘」才发
//!    [ChunkWillUnload] 并真正释放（四份缓存 `unload_chunk`、
//!    `GenTable::reset`、`stats.unloaded`）；未确认的条目留在表里，下一 tick
//!    再看。
//! 4. [resolve_chunks]（`Wanted` 键序，每 tick 最多
//!    [VoxelStreamConfig::max_resolve_per_tick] 个）：问存档（见下节
//!    「持久化来源」）：`Hit` 且有整块内容且版本匹配 → 装树，生成器零调用；
//!    `Hit` 无内容 / 版本不匹配 / `Miss` → 真正跑 [WorldGenerator]；
//!    `Pending` → 留在队列，下一 tick 再问。
//!
//! ## 实体恢复
//!
//! 存档命中且记录带实体、整块内容装成功时，[resolve_chunks] 发一条
//! [ChunkEntities]（生成路径、空实体、intake 的「块已在容器」快路径都不发——
//! 启动期的实体已由 `load_world` 应用）。真正的重建由 `WorldChunkPlugin` 的
//! Lifecycle 阶段消费这条消息完成，流式层只负责把记录递过去。
//!
//! ## 持久化来源
//!
//! 读路径有两个可能的来源，[resolve_chunks] 按下面的优先级选：
//!
//! 1. [SaveStore](game_engine::save::SaveStore)（生产）：`StreamingWorldPlugin`
//!    在有 `SaveRoot` 时安装 `SavePlugin`，落盘（[persist_pending_unloads]）
//!    与读档（本系统）都走同一个 store。只要这个资源存在就**只用它**；
//! 2. [ChunkPersistence]（注入点）：没有 [SaveStore] 时才查它，默认是
//!    `NoStore`（永远 `Miss`），保证流式层脱离存档也能单独运行。
//!
//! 为什么真实存档优先：`ChunkPersistence` 是测试 / 替换用的注入点，生产装配
//! 里它一直是默认的 `NoStore`。历史上这里只查注入点，卸载时内容确实写进了
//! [SaveStore]，同一会话里重新请求却 `Miss` → 重新生成，玩家编辑被地形覆盖，
//! 下一次卸载又把生成结果写回去，永久盖掉磁盘上的编辑。两者同时存在时以真实
//! 磁盘为准，测试替身就不能挡住生产读路径；要覆盖存档读路径的测试应该直接写
//! [SaveStore]，而不是指望注入点优先。
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
//! 不依赖哈希/墙钟。`GenTable` 的两条轴分工明确：调度轴（`enqueue` / `begin` /
//! `finish` / `cancel` / `reset`）由流式系统维护；内容态（`Unloaded` /
//! `Undetermined` / `Determined`）由 [advance_proposals] 与 [settle_chunks]
//! 推进。[finish_chunk] 是所有「数据已进容器」路径的统一出口，它负责把内容态
//! 定稿成 `Determined`。
//!
//! ## 两段流水线
//!
//! 生成不再「一 tick 一次到位」，而是按文档 §6/§7 分两段推进：
//!
//! 1. [advance_proposals]：只跑地形生成器 + 本块提案，把 `(基础地形树, 提案)`
//!    压进 [StagedChunks]（树**还没**进 [VoxVolume]），内容态推到 `Undetermined`；
//! 2. [settle_chunks]（下一次运行才消费，见 [StagedChunk::observed]）：26 邻提案门
//!    → 全序裁决 → `realize` 落体素 → 装容器 → [finish_chunk] 推 `Determined`。
//!
//! 暂存的树不在容器里，所以降档时**直接丢弃**（见 [demote]），不走
//! [PendingUnloads] 落盘——内容还没定稿，没有可落盘的东西（文档 §8）。
//! `VoxTree` 没有 `Drop`：每条失败/丢弃路径都必须 [release_tree]。

use std::collections::BTreeMap;

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use game_engine::body_key::BodyKey;
use game_engine::identity::{StableEntityIndex, StableIdAllocator};
use game_engine::presentation::voxel::{extract_raw_halo, WrappedBlockCache, RAW_VOXELS};
use game_engine::presentation::RenderTransform;
use game_engine::residency::{ChunkReached, GameTickSet, Residency, ResidencyChanged};
use game_engine::save::{
    capture, export_chunk, EntityRecord, OpaqueComponents, PersistRegistry, SaveStore,
};
use game_engine::spatial::Size;
use game_engine::streaming::{
    ChunkPersistence, ChunkRecord, GenState, GenTable, ProposalRecord, StoreLookup,
};
use game_engine::voxel::{
    dec_ref_recursive, ChunkKey, Lod, MeshBlock, Neighbourhood, VoxTree, VoxVolume, VoxelInterner,
};

use crate::presentation::{PresentedTransform, RenderGroupBody, VoxChunkGroupRaw};
use crate::static_data::voxel::{DEFAULT_BIOME, DEFAULT_VOXEL_SIZE};
use crate::voxel::terrain::blocks::block_origin_meters;
use crate::voxel::terrain::generation::WorldGenerator;
use crate::voxel::terrain::{
    unload_chunk, TerrainConfig, TerrainGeneratedBlocks, TerrainNeighborBlocks,
};
use crate::voxel::{arbitrate, realize_chunk, WorldFeatureRegistry};
use crate::world::WorldSeed;

/// 流式加载配置。
#[derive(Resource, Clone, Debug, PartialEq, Eq)]
pub struct VoxelStreamConfig {
    /// 每 tick 最多结算多少个排队区块（`Pending` 也占一个名额，
    /// 避免队首被存档卡住时饿死后面的块）。
    pub max_resolve_per_tick: usize,
    /// 生成器版本（按块记录在 [ChunkRecord](game_engine::streaming::ChunkRecord) 里）。
    ///
    /// 读路径用它做运行期过滤：`Hit` 记录的版本与这里不一致时，旧整块内容
    /// 不再可信，**不装**、改走生成路径。版本不匹配 = 地形生成器换代，是发布
    /// 新地形时的正常现象，不是错误。
    ///
    /// 它不决定**实体**是否恢复：实体是玩家造的，与地形版本无关。它也仍然
    /// 只影响提案能否复用（规格 §6.2）；本轮没有提案管线，这个字段由装配方
    /// 写入、由存档层读取。
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

/// 流式加载统计（每 tick 由 [advance_proposals] / [settle_chunks] 刷新）。
#[derive(Resource, Debug, Default, PartialEq, Eq)]
pub struct StreamStats {
    /// 当前等待结算的区块数（[StreamWanted] 长度）。
    pub queued: usize,
    /// 其中本 tick 仍在等存档回包（`StoreLookup::Pending`）的数量。
    pub pending: usize,
    /// 累计从存档装树的块数。
    pub loaded_from_store: u64,
    /// 累计生成器产出的块数（进 [StagedChunks] 的那些；存档命中不算）。
    pub generated: u64,
    /// 累计把内容态推进到 `Undetermined` 的块数（[StagedChunks] 的累计入口数，
    /// 卸载后重新暂存会再计一次）。
    pub proposed: u64,
    /// 累计真正卸载的块数（重复降档消息不重复计数）。
    pub unloaded: u64,
    /// 当前还没落盘确认的待卸载块数（[PendingUnloads] 长度），
    /// 每次 [release_pending_unloads] 跑完刷新。
    pub pending_unloads: usize,
    /// 当前存活的分组实体数（[RenderGroups] 的长度），每次增删后刷新。
    pub render_groups: usize,
    /// 当前已算好提案、还没裁决进容器的块数（[StagedChunks] 长度），
    /// 每次 [advance_proposals] / [settle_chunks] 跑完刷新。
    pub staged: usize,
}

/// 即将卸载一个区块的**数据**（在**落盘确认之后、释放之前**发出）。
///
/// 与旧实现的区别：消息不再在登记卸载时发，而是等存档的
/// [SaveStore::is_committed] 确认后才发。**注意**：同一个系统里紧接着就做释放，
/// 所以后续系统读到这条消息时体素树已经从容器里拿走了——它只是一条「这个块的数据
/// 要没了」的通知，不要把容器内容当作仍可读。需要内容的消费方应该读存档记录，
/// 或者自己缓存（`WorldChunkPlugin` 的实体销毁只按 `ChunkAnchor` 走，不读体素）。
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

/// 一条待卸载登记。
///
/// 生命周期：`version == None`（还没提交给存档）→
/// [persist_pending_unloads] 写/删成功得到 `Some(v)` → 等
/// [SaveStore::is_committed] 确认 v 落盘 → [release_pending_unloads] 释放。
/// 写盘失败时 `failed` 置位（同一块只 `warn!` 一次），条目保留、下一 tick 重试。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingUnload {
    /// 卸载前的驻留档位（释放时原样带进 [ChunkWillUnload]）。
    pub from: Residency,
    /// 已提交给存档的版本号；`None` = 还没写。
    pub version: Option<u64>,
    /// 上一次提交是否失败（用来只警告一次）。
    pub failed: bool,
}

/// 待落盘并释放的区块：`(body, key) -> PendingUnload`。
///
/// 由 [intake_residency_changes] 的 `to == None` 分支记入（此时**不释放**
/// 任何东西），[persist_pending_unloads] 把内容与实体提交给存档，由
/// [release_pending_unloads] 在确认落盘后取走并真正释放。键序 =
/// `(BodyKey, ChunkKey)` 升序，落盘与释放都按同一顺序，不依赖哈希。
#[derive(Resource, Debug, Default)]
pub struct PendingUnloads(pub BTreeMap<(BodyKey, ChunkKey), PendingUnload>);

/// 已到达 [Residency::Render] 的块 -> 分组实体。
///
/// **每个到达 Render 档的块恒有一个分组实体**，全空气块也不例外：它的
/// `chunks` 里只有一份全空气 34³ 载荷（渲染出 0 个矩形）。这样编辑后可以就地
/// 刷新 `VoxChunkGroupRaw`，不必等块被卸载重载——否则「空块里放第一块方块」
/// 要等下一次驻留变化才可见。
///
/// 键存在即代表该块**已经到达** Render 档，重复的 Render 请求可以直接忽略，
/// 不必重复提取 34³ halo。`StreamStats::render_groups` 统计的就是这里的长度。
#[derive(Resource, Debug, Default)]
pub struct RenderGroups(pub BTreeMap<(BodyKey, ChunkKey), Entity>);

/// 已算好提案、尚未裁决进容器的区块：基础地形树 + 本块提案。
///
/// 由 [advance_proposals] 写入，由 [settle_chunks] 取走并裁决。树在这里**还没**
/// 进 [VoxVolume]，所以降档时直接丢弃（见 [demote]），不进 [PendingUnloads]、
/// 也不落盘——内容还没定稿（文档 §8）。
#[derive(Resource, Debug, Default)]
pub struct StagedChunks(pub BTreeMap<(BodyKey, ChunkKey), StagedChunk>);

/// 一个已暂存、待裁决的区块。
pub struct StagedChunk {
    /// 基础地形树（生成器产出，feature 还没写进来），尚未进 [VoxVolume]。
    pub tree: VoxTree<u8>,
    /// 已盖章、已排序的本块提案（[WorldFeatureRegistry::propose_chunk] 的结果）。
    pub proposals: Vec<ProposalRecord>,
    /// [settle_chunks] 是否已经「看见」过这个暂存块。第一次看见只置位、不裁决，
    /// 下一次运行才真正结算：同一条 chain 里 advance 在前、settle 在后，若没有
    /// 这一步，本 tick 刚暂存的块会被同 tick 立刻结算，`Undetermined` 这个中间态
    /// 就永远观察不到，降档/取消也就没有可取消的窗口。
    pub observed: bool,
}

impl std::fmt::Debug for StagedChunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StagedChunk")
            .field("proposals", &self.proposals.len())
            .field("observed", &self.observed)
            .finish_non_exhaustive()
    }
}

/// 提案缓存：邻居门的输入。纯函数、可随时补算。
///
/// 键是 `(body, chunk)`，值是 [WorldFeatureRegistry::propose_chunk] 为该块产出的
/// 提案（已按 `(feature_type, seq)` 排序，可直接当 union 的稳定顺序用）。
/// [advance_proposals] 写自己的块，[settle_chunks] 现算 26 邻并补齐；降档与真正
/// 卸载时清掉对应键。
#[derive(Resource, Debug, Default)]
pub struct ProposalCache(pub BTreeMap<(BodyKey, ChunkKey), Vec<ProposalRecord>>);

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
            .init_resource::<StagedChunks>()
            .init_resource::<ProposalCache>()
            .add_message::<ChunkWillUnload>()
            .add_message::<ChunkEntities>()
            .add_systems(
                FixedUpdate,
                (
                    intake_residency_changes,
                    persist_pending_unloads,
                    release_pending_unloads,
                    advance_proposals,
                    settle_chunks,
                )
                    .chain()
                    .in_set(GameTickSet::Streaming),
            );
    }
}

/// 暂存侧资源的打包。
///
/// Bevy 0.19 的系统参数上限是 16 个，[advance_proposals] / [settle_chunks] 需要的
/// 资源超过这个数，所以把总是成对出现的 [StagedChunks] / [ProposalCache] 折成
/// 一个参数。
#[derive(SystemParam)]
pub struct Staging<'w> {
    /// 已生成、待裁决的块。
    pub staged: ResMut<'w, StagedChunks>,
    /// 提案缓存（邻居门的输入）。
    pub cache: ResMut<'w, ProposalCache>,
}

/// 世界侧的只读输入：种子、地形配置、feature 注册表。
///
/// 三者都是可选的：缺 [WorldSeed] 时生成路径报错并放弃请求；缺 [TerrainConfig]
/// 用 [DEFAULT_BIOME]；缺 [WorldFeatureRegistry] 表示「世界里没有物体」，提案恒为
/// 空——这也是本轮不注册任何具体 feature 的装配方式。
#[derive(SystemParam)]
pub struct WorldInputs<'w> {
    /// 世界种子（区块内容与提案都从它派生）。
    pub seed: Option<Res<'w, WorldSeed>>,
    /// 地形配置（只有 `biome` 参与生成）。
    pub terrain: Option<Res<'w, TerrainConfig>>,
    /// 物体注册表；缺失 = 没有物体 = 提案为空、`realize` 跳过。
    pub registry: Option<Res<'w, WorldFeatureRegistry>>,
}

/// 系统 1：把 [ResidencyChanged] 折成队列状态、渲染分组与真正的卸载。
#[allow(clippy::too_many_arguments)]
pub fn intake_residency_changes(
    mut commands: Commands,
    mut changes: MessageReader<ResidencyChanged>,
    mut gen: ResMut<GenTable>,
    mut wanted: ResMut<StreamWanted>,
    mut pending: ResMut<PendingUnloads>,
    mut reached: MessageWriter<ChunkReached>,
    mut index: Option<ResMut<StableEntityIndex>>,
    mut allocator: Option<ResMut<StableIdAllocator>>,
    mut volumes: Query<&mut VoxVolume>,
    mut interner: ResMut<VoxelInterner>,
    mut wrapped: ResMut<WrappedBlockCache>,
    mut generated: ResMut<TerrainGeneratedBlocks>,
    mut neighbors: ResMut<TerrainNeighborBlocks>,
    mut groups: ResMut<RenderGroups>,
    mut staging: Staging,
    mut stats: ResMut<StreamStats>,
) {
    for change in changes.read() {
        let ResidencyChanged {
            body,
            key,
            from,
            to,
        } = *change;
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
                &mut volumes,
                &mut interner,
                &mut wrapped,
                &mut generated,
                &mut neighbors,
                &mut groups,
                &mut staging.staged,
                &mut staging.cache,
                &mut stats,
            );
            continue;
        }

        // 复活：上一轮降档还挂在 [PendingUnloads]（写盘未确认）。树还在容器里，
        // 直接撤销登记即可——绝不能释放，也不能再发 ChunkWillUnload。
        // 这一步必须早于下面的 Render 幂等检查，否则一次「Render -> None 未确认
        // 就重新升 Render」的请求会被当成重复 Render 丢掉。
        let revived = pending.0.remove(&pair).is_some();

        // 幂等：该块已经到过 Render（恒有分组实体）时，重复的 Render 请求
        // 不再入队、不再发第二条 Render 消息。
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
        // 快路径条件：树在容器里，且生成态是 Undetermined（地形 Startup /
        // 上一轮装载留下的数据）**或者**这是一次复活（demote 已把 work 置为
        // Cancelled，内容态可能是任意值；树就在容器里，不该再查存档/重生成）。
        if !volume.contains_chunk(&key)
            || (gen.gen_state(pair) != GenState::Undetermined && !revived)
        {
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

    stats.staged = staging.staged.0.len();
}

/// 系统 4：把排队块推进到「未确定」——生成基础地形 + 本块提案，暂存起来。
///
/// 这一段**不**进容器、不发 [ChunkReached]、也不给 `wanted` 出队：块只是从
/// 「什么都没有」变成「地形有了、提案盖章了」，内容态推进到
/// [GenState::Undetermined]，真正的定稿留给下一次运行的 [settle_chunks]。
///
/// 存档命中且带整块内容的记录仍然一步到位（装块 → [finish_chunk] →
/// `Determined`）：磁盘上的内容本来就是定稿，不必再走提案环。
#[allow(clippy::too_many_arguments)]
pub fn advance_proposals(
    mut commands: Commands,
    config: Res<VoxelStreamConfig>,
    mut gen: ResMut<GenTable>,
    mut wanted: ResMut<StreamWanted>,
    save_store: Option<Res<SaveStore>>,
    persistence: Res<ChunkPersistence>,
    mut index: Option<ResMut<StableEntityIndex>>,
    mut allocator: Option<ResMut<StableIdAllocator>>,
    mut volumes: Query<&mut VoxVolume>,
    mut interner: ResMut<VoxelInterner>,
    world: WorldInputs,
    mut reached: MessageWriter<ChunkReached>,
    mut entities_out: MessageWriter<ChunkEntities>,
    mut groups: ResMut<RenderGroups>,
    mut staging: Staging,
    mut stats: ResMut<StreamStats>,
) {
    let budget = config.max_resolve_per_tick;
    // 跳过已经暂存的块：它们的树与提案已在 [StagedChunks] 里，再生成一次只会
    // 白烧一棵树、还会把 `observed` 重置成 false（`wanted` 要到 settle 才出队，
    // 不跳过就永远结算不了）。
    let keys: Vec<((BodyKey, ChunkKey), Residency)> = wanted
        .0
        .iter()
        .filter(|(pair, _)| !staging.staged.0.contains_key(pair))
        .take(budget)
        .map(|(pair, level)| (*pair, *level))
        .collect();
    let mut pending = 0usize;

    // 生产装配里落盘就是 SaveStore，ChunkPersistence 是测试 / 替换用的注入点；
    // 两者同时存在时以真实磁盘为准，免得测试替身把生产读路径挡住（见模块文档
    // 「持久化来源」）。SaveStore::lookup 是固有方法，优先于同名的 trait 方法，
    // 不会递归。
    let lookup = |body: BodyKey, key: ChunkKey| match save_store.as_deref() {
        Some(store) => store.lookup(body, key),
        None => persistence.lookup(body, key),
    };

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

        match lookup(body, key) {
            StoreLookup::Pending => pending += 1,
            StoreLookup::Hit(record) => {
                // 运行期版本过滤。`load_world` 只在启动读档时按
                // `generator_version` 丢弃旧块；这里从磁盘 / 内存缓冲读回来的
                // 记录没有经过那一步，必须在装块之前再比一次，否则旧地形会
                // 盖掉当前版本的地表。
                //
                // 只丢**内容**、不丢**实体**：实体是玩家造的，与地形生成器
                // 版本无关。启动读档时实体已经恢复过，这里再发 ChunkEntities
                // 会让消费方二次 apply；所以 stale 分支既不 install_chunk，
                // 也不发 ChunkEntities，直接按「没有可用内容」重新生成。
                // 版本不匹配是发布新地形时的正常现象，不是错误，只记 debug。
                if record.generator_version != config.generator_version {
                    debug!(
                        "stream: 区块 {key:?} 存档版本 {} 与当前生成器版本 {} 不一致，重新生成",
                        record.generator_version, config.generator_version
                    );
                    stage_chunk(
                        pair,
                        &mut gen,
                        &mut wanted,
                        &mut staging.staged,
                        &mut staging.cache,
                        &mut interner,
                        &world,
                        &mut stats,
                    );
                    continue;
                }
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
                    // 记录存在但没有整块内容（旧版本只写了实体 / 内容被清掉）：
                    // 与 Miss 同样走生成 + 提案，不装空块。
                    None => stage_chunk(
                        pair,
                        &mut gen,
                        &mut wanted,
                        &mut staging.staged,
                        &mut staging.cache,
                        &mut interner,
                        &world,
                        &mut stats,
                    ),
                }
            }
            // 没有可用内容：本地生成基础地形 + 本块提案，压进暂存区。
            // 内容态推到 `Undetermined`，容器与 `ChunkReached` 都不动。
            StoreLookup::Miss => stage_chunk(
                pair,
                &mut gen,
                &mut wanted,
                &mut staging.staged,
                &mut staging.cache,
                &mut interner,
                &world,
                &mut stats,
            ),
        }
    }

    stats.queued = wanted.0.len();
    stats.pending = pending;
    stats.staged = staging.staged.0.len();
}

/// 系统 5：结算暂存块——26 邻提案门 → 全序裁决 → realize 落体素 → 装容器 → 定稿。
///
/// 只处理**上一次运行**就已经暂存的块（[StagedChunk::observed]）：同一条 chain
/// 里 [advance_proposals] 在前、本系统在后，没有这一步的话，本 tick 刚暂存的块会
/// 被同 tick 立刻结算，`Undetermined` 这个中间态就没人观察得到，降档/取消也就
/// 没有可撤销的窗口。
///
/// 邻居门是「提案环」的惰性实现：提案是纯函数，现算即满足「所有可能与本块重叠
/// 的提案都已存在」，本轮不排 tick 等待。
#[allow(clippy::too_many_arguments)]
pub fn settle_chunks(
    mut commands: Commands,
    config: Res<VoxelStreamConfig>,
    mut gen: ResMut<GenTable>,
    mut wanted: ResMut<StreamWanted>,
    mut index: Option<ResMut<StableEntityIndex>>,
    mut allocator: Option<ResMut<StableIdAllocator>>,
    mut volumes: Query<&mut VoxVolume>,
    mut interner: ResMut<VoxelInterner>,
    world: WorldInputs,
    mut groups: ResMut<RenderGroups>,
    mut reached: MessageWriter<ChunkReached>,
    mut staging: Staging,
    mut stats: ResMut<StreamStats>,
) {
    let budget = config.max_resolve_per_tick;

    // 先把所有暂存块标成「看见过」，再挑出上一次就已标记的来结算。
    let mut ready: Vec<(BodyKey, ChunkKey)> = Vec::new();
    for (pair, chunk) in staging.staged.0.iter_mut() {
        if chunk.observed {
            ready.push(*pair);
        } else {
            chunk.observed = true;
        }
    }
    ready.truncate(budget);

    'settle: for pair in ready {
        let (body, key) = pair;
        let Some(staged_chunk) = staging.staged.0.remove(&pair) else {
            continue;
        };
        let StagedChunk {
            tree, proposals, ..
        } = staged_chunk;

        let Some(entity) = body_entity(index.as_deref(), body) else {
            error!("stream: body {body:?} 找不到实体，放弃区块 {key:?}");
            fail_staged(
                pair,
                tree,
                &mut interner,
                &mut gen,
                &mut wanted,
                &mut staging,
            );
            continue 'settle;
        };
        let Ok(mut volume) = volumes.get_mut(entity) else {
            error!("stream: 实体 {entity:?} 没有 VoxVolume，放弃区块 {key:?}");
            fail_staged(
                pair,
                tree,
                &mut interner,
                &mut gen,
                &mut wanted,
                &mut staging,
            );
            continue 'settle;
        };

        // 邻居门：26 个邻块，逐轴 -1/0/+1，(0,0,0) 跳过，按邻键升序拼。
        let mut neighbours: Vec<ChunkKey> = Vec::with_capacity(26);
        for dx in -1..=1 {
            for dy in -1..=1 {
                for dz in -1..=1 {
                    if dx == 0 && dy == 0 && dz == 0 {
                        continue;
                    }
                    neighbours.push(ChunkKey::new(key.x + dx, key.y + dy, key.z + dz));
                }
            }
        }
        neighbours.sort();

        let mut union = proposals;
        for neighbour in neighbours {
            let neighbour_pair = (body, neighbour);
            if !staging.cache.0.contains_key(&neighbour_pair) {
                let computed = match (world.registry.as_deref(), world.seed.as_deref()) {
                    (Some(registry), Some(seed)) => {
                        match registry.propose_chunk(seed.0, neighbour) {
                            Ok(computed) => computed,
                            Err(error) => {
                                error!(
                                    "stream: 邻块 {neighbour:?} 提案失败，放弃区块 {key:?}：{error}"
                                );
                                fail_staged(
                                    pair,
                                    tree,
                                    &mut interner,
                                    &mut gen,
                                    &mut wanted,
                                    &mut staging,
                                );
                                continue 'settle;
                            }
                        }
                    }
                    // 有注册表却没有种子：提案不可复现，按错误放弃。
                    (Some(_), None) => {
                        error!("stream: WorldSeed 资源缺失，算不出邻块 {neighbour:?} 的提案");
                        fail_staged(
                            pair,
                            tree,
                            &mut interner,
                            &mut gen,
                            &mut wanted,
                            &mut staging,
                        );
                        continue 'settle;
                    }
                    // 没有注册表 = 没有物体：邻块提案是空，
                    // cache 里也记成空，省掉后续重复判断。
                    (None, _) => Vec::new(),
                };
                staging.cache.0.insert(neighbour_pair, computed);
            }
            if let Some(cached) = staging.cache.0.get(&neighbour_pair) {
                union.extend(cached.iter().copied());
            }
        }

        let winners = arbitrate(&union);

        // 没有注册表 = 没有物体：winners 必为空，realize 直接跳过。
        let mut tree = tree;
        if let Some(registry) = world.registry.as_deref() {
            let Some(seed) = world.seed.as_deref() else {
                error!("stream: WorldSeed 资源缺失，无法 realize 区块 {key:?}");
                fail_staged(
                    pair,
                    tree,
                    &mut interner,
                    &mut gen,
                    &mut wanted,
                    &mut staging,
                );
                continue 'settle;
            };
            if let Err(error) = realize_chunk(
                registry,
                seed.0,
                key,
                &winners,
                &mut tree,
                interner.inner_mut(),
            ) {
                error!("stream: 区块 {key:?} realize 失败，放弃：{error}");
                fail_staged(
                    pair,
                    tree,
                    &mut interner,
                    &mut gen,
                    &mut wanted,
                    &mut staging,
                );
                continue 'settle;
            }
        }

        // 装容器：防御式先取旧树（insert_chunk 覆盖会 debug_assert!）。
        let old = volume.take_chunk_raw(&key);
        volume.insert_chunk(key, tree);
        release_tree(&mut interner, old);

        let level = wanted.0.get(&pair).copied().unwrap_or(Residency::Data);
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

    stats.queued = wanted.0.len();
    stats.staged = staging.staged.0.len();
}

/// 结算失败的统一出口：清缓存、释放暂存树、放弃请求。
///
/// `VoxTree` 没有 `Drop`，所有「暂存块不能落地」的路径都必须走这里，
/// 否则每失败一次就泄漏一棵树的节点。
fn fail_staged(
    pair: (BodyKey, ChunkKey),
    tree: VoxTree<u8>,
    interner: &mut VoxelInterner,
    gen: &mut GenTable,
    wanted: &mut StreamWanted,
    staging: &mut Staging,
) {
    staging.cache.0.remove(&pair);
    release_tree(interner, Some(tree));
    drop_request(gen, wanted, pair);
}

/// 降档处理：先按需清理渲染档，再把数据卸载**登记**进 [PendingUnloads]。
///
/// **重复消息不二次释放的判据**：第一次降档会把队列项移除、把
/// `(pair, PendingUnload { from, version: None, failed: false })` 记进
/// [PendingUnloads]；第二次同样的消息在去重检查处直接返回——不重复登记。
/// 此时**不发** [ChunkWillUnload]、**不**释放任何东西（树与实体都留着）。
/// 真正的释放（缓存 / `GenTable::reset` / `stats.unloaded`）由
/// [release_pending_unloads] 在 [persist_pending_unloads] 确认落盘之后做。
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
    volumes: &mut Query<&mut VoxVolume>,
    interner: &mut VoxelInterner,
    wrapped: &mut WrappedBlockCache,
    generated: &mut TerrainGeneratedBlocks,
    neighbors: &mut TerrainNeighborBlocks,
    groups: &mut RenderGroups,
    staged: &mut StagedChunks,
    cache: &mut ProposalCache,
    stats: &mut StreamStats,
) {
    let (body, key) = pair;

    // 1) 渲染档先退：实体销毁 + 四份表现层缓存失效（体素内容保留）。
    if from == Residency::Render && to != Residency::Render {
        despawn_render_group(pair, commands, groups, index.as_deref_mut(), stats);
        wrapped.invalidate_covered(interner.inner_mut(), key);
        generated.own.invalidate_covered(interner.inner_mut(), key);
        generated
            .neighbors
            .invalidate_covered(interner.inner_mut(), key);
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

    // 暂存块还没进容器、内容也没定稿：直接丢弃，**不**进 [PendingUnloads]、
    // 不落盘（文档 §8：降档时半成品不能进容器）。
    if let Some(chunk) = staged.0.remove(&pair) {
        release_tree(interner, Some(chunk.tree));
        cache.0.remove(&pair);
        drop_request(gen, wanted, pair);
        return;
    }
    cache.0.remove(&pair);

    let removed = wanted.0.remove(&pair).is_some();
    let entity = body_entity(index.as_deref(), body);
    let present = entity
        .and_then(|entity| volumes.get(entity).ok())
        .is_some_and(|volume| volume.contains_chunk(&key));
    if !removed && !present {
        return;
    }

    // 去重：同一 pair 已经登记过（同一 tick 里的第二次降档）就什么都不再做。
    // 释放要等 [persist_pending_unloads] 确认落盘，此时树还在容器里，所以
    // `present` 仍然成立，第二次消息必须在这里被挡住。
    if pending.0.contains_key(&pair) {
        return;
    }

    // 只取消生成并登记；消息与释放都推到确认落盘之后
    // （[release_pending_unloads]）。
    gen.cancel(pair);
    pending.0.insert(
        pair,
        PendingUnload {
            from,
            version: None,
            failed: false,
        },
    );
}

/// 系统 2：把待卸载区块的整块内容与 `Persist` 实体提交给存档（exclusive）。
///
/// 顺序在 [intake_residency_changes] 之后、[release_pending_unloads] 之前：
/// 提交先于释放，导出 content 时树还在容器里。
///
/// - 只处理 `version == None` 的条目：同一块只提交一次，不每 tick 重写；
/// - 没有待提交条目，或没有 [SaveStore]（headless / 单测）→ 直接返回；
/// - 只在存在待提交条目时 [capture] 一次实体快照，失败只 `warn!`，本 tick
///   不写盘（条目保持 `version == None`，下一 tick 重试）；
/// - 空块（content 与 entities 都空）不写记录，改为删除该块的旧记录：磁盘上可能
///   还留着上一次的 `ChunkRecord`，静默跳过会让旧内容在重载后复活；
/// - [SaveStore::write] / [SaveStore::delete] 的 `Ok(v)` 记进条目的
///   `version`；`Err` 只在该块**第一次**失败时 `warn!`，置 `failed` 并保留
///   条目，下一 tick 重试。
pub fn persist_pending_unloads(world: &mut World) {
    let needs_write = world
        .get_resource::<PendingUnloads>()
        .is_some_and(|pending| pending.0.values().any(|entry| entry.version.is_none()));
    if !needs_write {
        return;
    }
    // 没有存档（headless / 单测）：跳过写盘，释放阶段会立即释放。
    if world.get_resource::<SaveStore>().is_none() {
        return;
    }

    let snapshot = {
        let world: &World = &*world;
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

    if world.get_resource::<VoxelInterner>().is_none() {
        warn!("stream: 卸载落盘缺少 VoxelInterner，本 tick 跳过写盘");
        return;
    }
    let generator_version = world.resource::<VoxelStreamConfig>().generator_version;

    world.resource_scope::<PendingUnloads, _>(|world, mut pending| {
        let world: &World = &*world;
        let Some(store) = world.get_resource::<SaveStore>() else {
            return;
        };
        let Some(interner) = world.get_resource::<VoxelInterner>() else {
            return;
        };
        let index = world.get_resource::<StableEntityIndex>();

        for ((body, key), entry) in pending.0.iter_mut() {
            if entry.version.is_some() {
                // 已经提交过：等确认，不要每 tick 重写。
                continue;
            }
            let content = body_entity(index, *body)
                .and_then(|entity| world.get::<VoxVolume>(entity))
                .and_then(|volume| export_chunk(volume, *key, interner.inner()));
            let entities = snapshot.for_chunk(*body, *key).to_vec();
            let empty = content.is_none() && entities.is_empty();
            let result = if empty {
                // 空块：既没有整块内容也没有 Persist 实体，validate 会拒绝全空记录。
                // 不能静默跳过：磁盘上可能还有这块的旧记录，必须显式删除，否则重载后
                // 旧内容会复活。删除与写入一样只在 Flush 落盘，这里只标墓碑。
                store.delete(*body, *key)
            } else {
                store.write(
                    *body,
                    *key,
                    ChunkRecord {
                        generator_version,
                        content,
                        proposals: Vec::new(),
                        entities,
                    },
                )
            };
            match result {
                Ok(version) => {
                    entry.version = Some(version);
                    entry.failed = false;
                }
                Err(error) => {
                    if !entry.failed {
                        if empty {
                            warn!("stream: 卸载落盘删除空区块 {key:?} 失败（下一 tick 重试）：{error:?}");
                        } else {
                            warn!("stream: 卸载落盘写区块 {key:?} 失败（下一 tick 重试）：{error:?}");
                        }
                        entry.failed = true;
                    }
                }
            }
        }
    });
}

/// 系统 3：真正释放 [PendingUnloads] 里**已确认落盘**的区块（exclusive）。
///
/// 逐条判断释放条件（满足其一即可）：
///
/// - 没有 [SaveStore]（headless / 单测）：立即释放；
/// - `store.has_worker() == false`：没有后台线程就永远不会有确认（`flush` 直接
///   返回 0），等了也是泄漏；这只可能出现在测试替身或 `shutdown_worker` 之后，
///   此时释放是唯一能收敛的选择；
/// - `version == Some(v)` 且 `store.is_committed(body, key, v)`：已落盘。
///
/// **有意的取舍**：后台持续写失败（磁盘满、权限）时条目会一直留在表里，块不释放、
/// 内存不回收，但数据不会静默丢失；`StreamStats::pending_unloads` 是这条积压的
/// 观测量，排查时看它是否持续增长。
///
/// 释放顺序固定：先发 [ChunkWillUnload]（纯通知，见该消息的文档）→ 四份缓存的
/// [unload_chunk]（顺序与旧实现一致）→ [GenTable::reset] →
/// `stats.unloaded += 1` → 从表里移除。未确认的条目留在表里，下一 tick 再看。
/// 每次运行后把 `stats.pending_unloads` 刷成剩余条目数。
pub fn release_pending_unloads(world: &mut World) {
    let pending = {
        let Some(mut table) = world.get_resource_mut::<PendingUnloads>() else {
            return;
        };
        std::mem::take(&mut table.0)
    };
    if pending.is_empty() {
        world.resource_mut::<StreamStats>().pending_unloads = 0;
        return;
    }

    let mut ready = Vec::new();
    let mut kept = BTreeMap::new();
    {
        let store = world.get_resource::<SaveStore>();
        let has_worker = store.is_some_and(|store| store.has_worker());
        for (pair, entry) in pending {
            let release = match store {
                // 没有存档 / 没有后台线程：永远不会确认，不能无限等。
                None => true,
                Some(_) if !has_worker => true,
                Some(store) => entry
                    .version
                    .is_some_and(|version| store.is_committed(pair.0, pair.1, version)),
            };
            if release {
                ready.push((pair, entry));
            } else {
                kept.insert(pair, entry);
            }
        }
    }

    // 先写回未确认的条目，再释放（释放会可变借用 world）。
    world.resource_mut::<PendingUnloads>().0 = kept;
    for ((body, key), entry) in ready {
        // 消息紧贴在释放之前发出；监听方读到它时树已经被取走（见类型文档）。
        world.write_message(ChunkWillUnload {
            body,
            key,
            from: entry.from,
            to: Residency::None,
        });
        release_unloaded_chunk(world, body, key, entry.from);
    }
    let remaining = world.resource::<PendingUnloads>().0.len();
    world.resource_mut::<StreamStats>().pending_unloads = remaining;
}

/// 释放单个已登记卸载的区块（exclusive 版本，语义与旧 demote 收尾一致）。
fn release_unloaded_chunk(world: &mut World, body: BodyKey, key: ChunkKey, from: Residency) {
    // 防御：暂存块在 demote 时已经丢弃，这里再清一次，避免异常路径残留树。
    let pair = (body, key);
    let orphan = world
        .get_resource_mut::<StagedChunks>()
        .and_then(|mut staged| staged.0.remove(&pair));
    if let Some(chunk) = orphan {
        let mut interner = world.resource_mut::<VoxelInterner>();
        release_tree(&mut interner, Some(chunk.tree));
    }
    if let Some(mut cache) = world.get_resource_mut::<ProposalCache>() {
        cache.0.remove(&pair);
    }

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

/// 生成基础地形 + 本块提案，压进暂存区，并把内容态推到 `Undetermined`。
///
/// 树**不进**容器（那是 [settle_chunks] 的事），也不发 [ChunkReached]。失败
///（缺 [WorldSeed]、提案出错）时立刻 [release_tree] + [drop_request]：`VoxTree`
/// 没有 `Drop`，漏一条路径就是泄漏节点。
#[allow(clippy::too_many_arguments)]
fn stage_chunk(
    pair: (BodyKey, ChunkKey),
    gen: &mut GenTable,
    wanted: &mut StreamWanted,
    staged: &mut StagedChunks,
    cache: &mut ProposalCache,
    interner: &mut VoxelInterner,
    world: &WorldInputs,
    stats: &mut StreamStats,
) {
    let (body, key) = pair;
    let Some(seed) = world.seed.as_deref() else {
        // 没有种子就没有确定性世界：移除而不是无限重试。
        error!("stream: WorldSeed 资源缺失，无法生成区块 {key:?}");
        drop_request(gen, wanted, pair);
        return;
    };
    let biome = world
        .terrain
        .as_deref()
        .map_or(DEFAULT_BIOME, |config| config.biome);
    let tree = WorldGenerator::new(*seed, biome).generate_chunk(interner.inner_mut(), key);
    stats.generated += 1;

    let proposals = match world.registry.as_deref() {
        Some(registry) => match registry.propose_chunk(seed.0, key) {
            Ok(proposals) => proposals,
            Err(error) => {
                error!("stream: 区块 {key:?} 提案失败，放弃：{error}");
                release_tree(interner, Some(tree));
                drop_request(gen, wanted, pair);
                return;
            }
        },
        // 没有注册表 = 世界里没有物体 = 提案为空（本轮就是这种装配）。
        None => Vec::new(),
    };
    // 缓存是邻居门的输入：先写自己的，等 settle 现算 26 邻时再补齐邻键。
    cache.0.insert(pair, proposals.clone());
    staged.0.insert(
        pair,
        StagedChunk {
            tree,
            proposals,
            observed: false,
        },
    );
    if let Err(error) = gen.advance(pair, GenState::Undetermined) {
        debug!("stream: advance({body:?}, {key:?}) 被拒绝：{error:?}");
    }
    // 调度态收敛回 Idle：真正的工作已经做完，别让后续系统看到一个没关的 Running。
    let _ = gen.begin(pair);
    let _ = gen.finish(pair);
    stats.proposed += 1;
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
/// - 已在 [RenderGroups] 里 → `Ok(false)`，幂等。
/// - 否则 spawn 分组实体并登记进 [StableEntityIndex]，`Ok(true)`。
///
/// **全空气块也 spawn**：`chunks` 里放一份全空气 34³ 载荷（渲染 0 个矩形），
/// 并挂 [RenderGroupBody] 标出所属 body；之后在空块里放方块时，刷新系统能
/// 直接就地更新这个实体，不必等块重新加载。
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
    let (body, key) = pair;
    debug_assert_eq!(raw.len(), RAW_VOXELS, "extract_raw_halo 必须产出 34³");

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
            RenderGroupBody(body),
        ))
        .id();
    index.insert(id, entity);
    groups.0.insert(pair, entity);
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
    if let Some(entity) = groups.0.remove(&pair) {
        if let Some(index) = index {
            index.remove_by_entity(entity);
        }
        commands.entity(entity).despawn();
    }
    refresh_render_groups(groups, stats);
}

/// 把 `stats.render_groups` 刷成存活分组实体数。
fn refresh_render_groups(groups: &RenderGroups, stats: &mut StreamStats) {
    stats.render_groups = groups.0.len();
}

/// 单个体素边长（米），与 terrain 分组使用的尺度一致。
fn voxel_size_f32() -> f32 {
    DEFAULT_VOXEL_SIZE.to_num::<f32>()
}
/// 把 `GenTable` 从任意工作态干净地结算回 `Idle`，并把内容态**定稿**。
///
/// `finish` 只接受 `Running`，所以固定先 `begin` 再 `finish`；
/// 两个结果都容忍（`Idle` / `Running` 都能收敛）。
///
/// 内容态在这里推进到 [GenState::Determined]：所有「数据已进容器」的路径都从
/// [finish_chunk] 出去，所以这是内容定稿的唯一出口。`Unloaded -> Determined`
/// 与 `Undetermined -> Determined` 都是文档允许的跳级；已经是 `Determined`
/// 时 `advance` 返回 `Ok(false)`，忽略即可。
fn settle(gen: &mut GenTable, pair: (BodyKey, ChunkKey)) {
    let _ = gen.begin(pair);
    let _ = gen.finish(pair);
    let _ = gen.advance(pair, GenState::Determined);
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
    use game_engine::presentation::voxel::raw_halo_has_solid_interior;
    use game_engine::residency::ChunkAnchor;
    use game_engine::save::{
        export_chunk, read_region, EntityRecord, OpaqueComponents, Persist, PersistComponent,
        PersistRegistry, SaveConfig, SavePaths, SaveStore, StableTypeId,
    };
    use game_engine::streaming::{ChunkRecord, ChunkStore, StoreLookup};
    use game_engine::voxel::{
        chunk_content_hash, chunk_key, MaxDepth, VoxOpsBulkWrite, VoxOpsRead, CHUNK_DEPTH,
    };
    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::static_data::voxel::DEFAULT_VOXEL_SIZE;
    use crate::voxel::{ChunkSink, ProposalSpec, WorldFeature};

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
        assert!(harness
            .app
            .world()
            .resource::<ProposalCache>()
            .0
            .get(&pair)
            .is_none());
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
}
