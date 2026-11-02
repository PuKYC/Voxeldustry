//! Data 档流式加载器：驻留档位变化的**数据生命周期**（装载 / 卸载 / 等待存档）。
//!
//! ## 职责边界
//!
//! 本模块只负责 [Residency::Data]：把 [ResidencyChanged] 的升档请求变成
//! 「体素容器里真的有这棵树」，把降档请求变成「树被真正释放」。渲染档
//! （[Residency::Render]：网格 / 碰撞 / 表现层缓存）不在本轮范围内：
//! 对 `to == Render` 的请求同样只装数据并发 [ChunkReached] `level: Data`，
//! 不触碰任何渲染资源，也不发假消息。
//!
//! ## 每 tick 的两步
//!
//! 1. [intake_residency_changes]：消费 [ResidencyChanged]。
//!    - 升档：`GenTable::enqueue` + 记入 [StreamWanted]；若块**已经在**
//!      `VoxVolume` 里且生成态还是 `Undetermined`（地形 Startup 已生成 /
//!      上一轮装载留下的数据），直接结算并发 [ChunkReached]，不重生成；
//!    - 降档（`to == None`）：只有「请求还在队列里」或「块真的还在容器里」
//!      才动作，因此重复的降档消息既不二次释放也不二次计数。
//! 2. [resolve_chunks]（`Wanted` 键序，每 tick 最多
//!    [VoxelStreamConfig::max_resolve_per_tick] 个）：问
//!    [ChunkPersistence]：`Hit` 且有整块内容 → 装树，生成器零调用；
//!    `Hit` 无内容 / `Miss` → 真正跑 [WorldGenerator]；`Pending` → 留在队列，
//!    下一 tick 再问。
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
use game_engine::identity::StableEntityIndex;
use game_engine::presentation::voxel::WrappedBlockCache;
use game_engine::residency::{ChunkReached, GameTickSet, Residency, ResidencyChanged};
use game_engine::streaming::{ChunkPersistence, GenState, GenTable, StoreLookup};
use game_engine::voxel::{dec_ref_recursive, ChunkKey, VoxTree, VoxVolume, VoxelInterner};

use crate::static_data::voxel::DEFAULT_BIOME;
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

/// 待结算队列：`(body, key) -> 目标档位`，键序即结算顺序。
#[derive(Resource, Debug, Default)]
pub struct StreamWanted(pub BTreeMap<(BodyKey, ChunkKey), Residency>);

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
            .add_message::<ChunkWillUnload>()
            .add_systems(
                FixedUpdate,
                (intake_residency_changes, resolve_chunks)
                    .chain()
                    .in_set(GameTickSet::Streaming),
            );
    }
}

/// 系统 1：把 [ResidencyChanged] 折成队列状态与真正的卸载。
#[allow(clippy::too_many_arguments)]
pub fn intake_residency_changes(
    mut changes: MessageReader<ResidencyChanged>,
    mut gen: ResMut<GenTable>,
    mut wanted: ResMut<StreamWanted>,
    mut will_unload: MessageWriter<ChunkWillUnload>,
    mut reached: MessageWriter<ChunkReached>,
    index: Option<Res<StableEntityIndex>>,
    mut volumes: Query<&mut VoxVolume>,
    mut interner: ResMut<VoxelInterner>,
    mut wrapped: ResMut<WrappedBlockCache>,
    mut generated: ResMut<TerrainGeneratedBlocks>,
    mut neighbors: ResMut<TerrainNeighborBlocks>,
    mut stats: ResMut<StreamStats>,
) {
    let index = index.as_deref();

    for change in changes.read() {
        let ResidencyChanged { body, key, from, to } = *change;
        let pair = (body, key);

        if to == Residency::None {
            demote(
                pair, from, to, index, &mut gen, &mut wanted, &mut will_unload, &mut volumes,
                &mut interner, &mut wrapped, &mut generated, &mut neighbors, &mut stats,
            );
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

        let Some(entity) = body_entity(index, body) else {
            continue;
        };
        let Ok(volume) = volumes.get(entity) else {
            continue;
        };
        if !volume.contains_chunk(&key) || gen.gen_state(pair) != GenState::Undetermined {
            continue;
        }

        // 数据已在容器里：直接结算，不重生成、不查存档。
        settle(&mut gen, pair);
        reached.write(ChunkReached {
            body,
            key,
            level: Residency::Data,
        });
        wanted.0.remove(&pair);
    }
}

/// 系统 2：按队列键序结算，最多 [VoxelStreamConfig::max_resolve_per_tick] 个。
#[allow(clippy::too_many_arguments)]
pub fn resolve_chunks(
    config: Res<VoxelStreamConfig>,
    mut gen: ResMut<GenTable>,
    mut wanted: ResMut<StreamWanted>,
    persistence: Res<ChunkPersistence>,
    index: Option<Res<StableEntityIndex>>,
    mut volumes: Query<&mut VoxVolume>,
    mut interner: ResMut<VoxelInterner>,
    seed: Option<Res<WorldSeed>>,
    terrain: Option<Res<TerrainConfig>>,
    mut reached: MessageWriter<ChunkReached>,
    mut stats: ResMut<StreamStats>,
) {
    let index = index.as_deref();
    let budget = config.max_resolve_per_tick;
    let keys: Vec<(BodyKey, ChunkKey)> = wanted.0.keys().copied().take(budget).collect();
    let mut pending = 0usize;

    for pair in keys {
        let (body, key) = pair;

        let Some(entity) = body_entity(index, body) else {
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
            settle(&mut gen, pair);
            reached.write(ChunkReached {
                body,
                key,
                level: Residency::Data,
            });
            wanted.0.remove(&pair);
            continue;
        }

        match persistence.lookup(body, key) {
            StoreLookup::Pending => pending += 1,
            StoreLookup::Hit(record) => match record.content {
                Some(bytes) => {
                    match game_engine::save::install_chunk(
                        &mut volume,
                        key,
                        &bytes,
                        interner.inner_mut(),
                    ) {
                        Ok(old) => {
                            release_tree(&mut interner, old);
                            settle(&mut gen, pair);
                            reached.write(ChunkReached {
                                body,
                                key,
                                level: Residency::Data,
                            });
                            wanted.0.remove(&pair);
                            stats.loaded_from_store += 1;
                        }
                        Err(error) => {
                            error!("stream: 存档内容装不进区块 {key:?}：{error}");
                            drop_request(&mut gen, &mut wanted, pair);
                        }
                    }
                }
                None => {
                    generate_into(
                        pair, &mut volume, &mut interner, seed.as_deref(), terrain.as_deref(),
                        &mut gen, &mut wanted, &mut reached, &mut stats,
                    );
                }
            },
            StoreLookup::Miss => {
                generate_into(
                    pair, &mut volume, &mut interner, seed.as_deref(), terrain.as_deref(),
                    &mut gen, &mut wanted, &mut reached, &mut stats,
                );
            }
        }
    }

    stats.queued = wanted.0.len();
    stats.pending = pending;
}

/// 降档处理：只有「请求还在队列里」或「块还在容器里」才动作。
///
/// **重复消息不二次释放的判据**就在这两条：第一次降档会把队列项移除、
/// 把树从容器里取走；第二次同样的消息两条都不成立，直接返回——不发
/// [ChunkWillUnload]、不碰缓存、`stats.unloaded` 不涨。
#[allow(clippy::too_many_arguments)]
fn demote(
    pair: (BodyKey, ChunkKey),
    from: Residency,
    to: Residency,
    index: Option<&StableEntityIndex>,
    gen: &mut GenTable,
    wanted: &mut StreamWanted,
    will_unload: &mut MessageWriter<ChunkWillUnload>,
    volumes: &mut Query<&mut VoxVolume>,
    interner: &mut VoxelInterner,
    wrapped: &mut WrappedBlockCache,
    generated: &mut TerrainGeneratedBlocks,
    neighbors: &mut TerrainNeighborBlocks,
    stats: &mut StreamStats,
) {
    let (body, key) = pair;
    let removed = wanted.0.remove(&pair).is_some();
    let entity = body_entity(index, body);
    let present = entity
        .and_then(|entity| volumes.get(entity).ok())
        .is_some_and(|volume| volume.contains_chunk(&key));
    if !removed && !present {
        return;
    }

    gen.cancel(pair);
    will_unload.write(ChunkWillUnload { body, key, from, to });

    let freed = match entity {
        Some(entity) => match volumes.get_mut(entity) {
            Ok(mut volume) => {
                // 四份缓存实例缺一不可：只有真正从容器取回树的那次返回 true，
                // 其余调用只负责失效各自的 LOD 缓存。
                let mut freed = unload_chunk(interner, &mut volume, wrapped, &mut generated.own, key);
                freed |= unload_chunk(
                    interner,
                    &mut volume,
                    &mut neighbors.0,
                    &mut generated.own,
                    key,
                );
                freed |= unload_chunk(
                    interner,
                    &mut volume,
                    wrapped,
                    &mut generated.neighbors,
                    key,
                );
                freed |= unload_chunk(
                    interner,
                    &mut volume,
                    &mut neighbors.0,
                    &mut generated.neighbors,
                    key,
                );
                freed
            }
            Err(_) => false,
        },
        None => false,
    };

    if !freed {
        // 正常的「排队中就被撤回」：没有树可释放，队列项/缓存已处理。
        debug!("stream: 区块 {key:?} 卸载时容器里没有树（请求被撤回）");
    }
    let _ = gen.reset(pair);
    stats.unloaded += 1;
}

/// 真正跑生成器并把树装进容器（旧树释放引用）。
#[allow(clippy::too_many_arguments)]
fn generate_into(
    pair: (BodyKey, ChunkKey),
    volume: &mut VoxVolume,
    interner: &mut VoxelInterner,
    seed: Option<&WorldSeed>,
    terrain: Option<&TerrainConfig>,
    gen: &mut GenTable,
    wanted: &mut StreamWanted,
    reached: &mut MessageWriter<ChunkReached>,
    stats: &mut StreamStats,
) {
    let (body, key) = pair;
    let Some(seed) = seed else {
        // 没有种子就没有确定性世界：移除而不是无限重试。
        error!("stream: WorldSeed 资源缺失，无法生成区块 {key:?}");
        drop_request(gen, wanted, pair);
        return;
    };
    let biome = terrain.map_or(DEFAULT_BIOME, |config| config.biome);
    let tree = WorldGenerator::new(*seed, biome).generate_chunk(interner.inner_mut(), key);
    let old = volume.insert_chunk(key, tree);
    release_tree(interner, old);
    settle(gen, pair);
    reached.write(ChunkReached {
        body,
        key,
        level: Residency::Data,
    });
    wanted.0.remove(&pair);
    stats.generated += 1;
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
    use std::sync::Mutex;

    use bevy::prelude::*;
    use game_engine::identity::{StableIdPlugin, StableIdWorldExt};
    use game_engine::save::export_chunk;
    use game_engine::streaming::{ChunkRecord, ChunkStore, StoreLookup};
    use game_engine::voxel::{chunk_content_hash, chunk_key};

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
}
