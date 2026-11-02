//! 世界区块层：region 是存档批次打包单位，不是实体成员表。
//!
//! ## 坐标
//!
//! 体素块 [ChunkKey]（32³）按各轴 `region_size` 块打包成 [RegionKey]：
//!
//! ```text
//! region.x = key.x.div_euclid(region_size)   // 负数向负无穷取整
//! origin.x = region.x * region_size
//! origin.x <= key.x < origin.x + region_size
//! ```
//!
//! 三个轴各算各的，y 轴本轮不特殊处理（调用方可通过 `region_size` 选择 y 带）。
//! 映射只用整数 `div_euclid` / `rem_euclid`，不用 `f32` / `f64`，负坐标不会
//! 落错格子。
//!
//! ## 只计数，不存成员
//!
//! [RegionIndex] 只记录每个 `(BodyKey, RegionKey)` 里有多少块实际到达
//! `>= Residency::Data`，用于 region 级通知与批次写盘。实体成员一律用
//! `Query<(With<Persist>, &ChunkAnchor)>` 现查，不在这里存第二份。
//!
//! ## 本轮范围与已知偏离
//!
//! - 已实现 [WorldChunkSet::Residency] 的 `apply_residency_changes`、
//!   [WorldChunkSet::Dirty] 的 `mark_dirty`，以及 [WorldChunkSet::Lifecycle]
//!   的两端：`restore_members`（恢复）与 `unload_members`（销毁，发生在
//!   Streaming 落盘之后）。
//! - 运行期恢复只由流式层发来的 [ChunkEntities] 驱动（存档命中、整块内容装成功后
//!   才发）。启动期的批量恢复仍走 `load_world` 的装载流程，不经过本模块，避免同一批
//!   实体被应用两次。
//! - `mark_dirty` **只标实体当前所在的 region**。文档要求新旧两个 region 都标脏，
//!   但 `Changed<ChunkAnchor>` 拿不到变更前的值；要等后续引入组件历史 / 上一帧
//!   镜像后再补旧 region。
//! - [RegionIndex] 比设计稿多一个私有的 `loaded_keys` 集合。`loaded_chunks` 只按
//!   region 计数，单靠它分不清「同一块的重复消息」和「同 region 的另一块」；而
//!   `ChunkReached` / `ChunkWillUnload` 都允许重复投递（必须幂等）。该集合只记
//!   已计数的体素块，不是实体成员表，也不对外暴露。
//!
//! ## 确定性
//!
//! 判定路径只用 [BTreeMap] / [BTreeSet]：region 计数、脏集合、去重集合的遍历
//! 顺序都由键序决定，不依赖哈希或查询顺序。

use std::collections::{BTreeMap, BTreeSet};

use bevy::ecs::message::MessageCursor;
use bevy::prelude::*;
use game_engine::body_key::BodyKey;
use game_engine::identity::{StableEntityId, StableEntityIndex};
use game_engine::residency::{ChunkAnchor, ChunkReached, GameTickSet, Real, Residency};
use game_engine::save::{OpaqueComponents, Persist, PersistRegistry};
use game_engine::voxel::ChunkKey;

use crate::voxel::stream::{ChunkEntities, ChunkWillUnload};
use crate::world::load::ensure_entity;

/// 世界区块配置。
///
/// `region_size` 是每个轴打包多少个 32³ 体素块；必须 `>= 1`，默认 4。
#[derive(Resource, Debug, Clone, Copy)]
pub struct WorldChunkConfig {
    /// 每个轴打包的体素块数（必须非 0）。
    pub region_size: u32,
}

impl Default for WorldChunkConfig {
    fn default() -> Self {
        Self { region_size: 4 }
    }
}

/// 世界区块键：三个轴上的 region 格坐标。
///
/// 与 [ChunkKey] 一样是整数坐标；排序是逐轴字典序，供 `BTreeMap` / `BTreeSet`
/// 给出确定顺序。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RegionKey {
    /// X 轴 region 坐标。
    pub x: i32,
    /// Y 轴 region 坐标。
    pub y: i32,
    /// Z 轴 region 坐标。
    pub z: i32,
}

/// 体素块 -> 世界区块：逐轴 `div_euclid`。
///
/// 负坐标向负无穷取整，例如 `region_size = 4` 时 `-1 -> -1`、`-5 -> -2`。
#[must_use]
pub fn region_of(key: ChunkKey, region_size: u32) -> RegionKey {
    let size = region_size_i32(region_size);
    RegionKey {
        x: key.x.div_euclid(size),
        y: key.y.div_euclid(size),
        z: key.z.div_euclid(size),
    }
}

/// 世界区块 -> 该 region 的体素块原点：逐轴 `region * region_size`。
///
/// 结果满足 `origin <= key < origin + region_size`（逐轴）。
#[must_use]
pub fn region_origin(region: RegionKey, region_size: u32) -> ChunkKey {
    let size = region_size_i32(region_size);
    ChunkKey {
        x: region.x * size,
        y: region.y * size,
        z: region.z * size,
    }
}

/// 把 `u32` 配置转成 `i32` 运算域；0 或超出 `i32` 的值属于配置错误。
#[inline]
fn region_size_i32(region_size: u32) -> i32 {
    debug_assert!(
        region_size > 0 && region_size <= i32::MAX as u32,
        "region_size 必须在 1..=i32::MAX 内，实际为 {region_size}"
    );
    region_size as i32
}

/// region 级加载计数与脏标记。
///
/// 字段私有：外部只能通过方法读，不能拿到可变内部。计数按
/// `(BodyKey, RegionKey)`，不按实体；实体成员不在这里存。
#[derive(Resource, Default, Debug)]
pub struct RegionIndex {
    /// `(body, region) -> 实际到达 >= Data 的体素块数`。
    loaded_chunks: BTreeMap<(BodyKey, RegionKey), u32>,
    /// 待批次写盘的 region。
    dirty: BTreeSet<(BodyKey, RegionKey)>,
    /// 已计数的具体体素块：用于把重复消息幂等掉（见模块文档的偏离说明）。
    loaded_keys: BTreeSet<(BodyKey, ChunkKey)>,
}

impl RegionIndex {
    /// 记一块到达 `>= Data`；返回该 region 计数是否发生 `0 -> 1`。
    ///
    /// 同一 `(body, key)` 重复调用是幂等的：计数不重复加、返回 `false`。
    /// 同 region 的另一块会加计数，但只有第一块让返回值变成 `true`。
    pub fn chunk_loaded(&mut self, body: BodyKey, key: ChunkKey, region_size: u32) -> bool {
        if !self.loaded_keys.insert((body, key)) {
            return false;
        }
        let region = region_of(key, region_size);
        let count = self.loaded_chunks.entry((body, region)).or_insert(0);
        *count = count.saturating_add(1);
        *count == 1
    }

    /// 记一块降到 `None`；返回该 region 计数是否发生 `1 -> 0`。
    ///
    /// 计数为 0 时是编程错误：`debug_assert` 报错并保持 0（不降为负）。
    pub fn chunk_unloaded(&mut self, body: BodyKey, key: ChunkKey, region_size: u32) -> bool {
        let pair = (body, key);
        debug_assert!(
            self.loaded_keys.contains(&pair),
            "RegionIndex::chunk_unloaded: ({body:?}, {key:?}) 计数为 0，卸载了未加载的区块"
        );
        if !self.loaded_keys.remove(&pair) {
            return false;
        }

        let region = region_of(key, region_size);
        let region_pair = (body, region);
        let count = self.loaded_chunks.get_mut(&region_pair);
        debug_assert!(
            count.as_deref().is_some_and(|count| *count > 0),
            "RegionIndex::chunk_unloaded: ({body:?}, {region:?}) 计数为 0，卸载了未加载的区块"
        );
        let Some(count) = count else {
            return false;
        };
        let emptied = {
            *count -= 1;
            *count == 0
        };
        if emptied {
            self.loaded_chunks.remove(&region_pair);
        }
        emptied
    }

    /// 把一个 region 标脏（重复标记幂等）。
    pub fn mark_dirty(&mut self, body: BodyKey, region: RegionKey) {
        self.dirty.insert((body, region));
    }

    /// 按 `(BodyKey, RegionKey)` 升序取走全部脏 region，并清空集合。
    #[must_use]
    pub fn take_dirty(&mut self) -> Vec<(BodyKey, RegionKey)> {
        std::mem::take(&mut self.dirty).into_iter().collect()
    }

    /// `(body, region)` 是否已标脏。
    #[must_use]
    pub fn is_dirty(&self, body: BodyKey, region: RegionKey) -> bool {
        self.dirty.contains(&(body, region))
    }

    /// 当前脏 region 数。
    #[must_use]
    pub fn dirty_len(&self) -> usize {
        self.dirty.len()
    }

    /// `(body, region)` 里实际到达 `>= Data` 的块数；没有记录时为 0。
    #[must_use]
    pub fn loaded_count(&self, body: BodyKey, region: RegionKey) -> u32 {
        self.loaded_chunks
            .get(&(body, region))
            .copied()
            .unwrap_or(0)
    }

    /// `(body, region)` 是否至少有一块到达 `>= Data`。
    #[must_use]
    pub fn is_loaded(&self, body: BodyKey, region: RegionKey) -> bool {
        self.loaded_count(body, region) > 0
    }

    /// 某个具体体素块是否已被计为到达。
    #[must_use]
    pub fn is_chunk_loaded(&self, body: BodyKey, key: ChunkKey) -> bool {
        self.loaded_keys.contains(&(body, key))
    }

    /// 有加载块的 region 数（测试 / 统计用）。
    #[must_use]
    pub fn loaded_region_count(&self) -> usize {
        self.loaded_chunks.len()
    }
}

/// region 级加载通知：该 region 的第一块到达 `>= Data`。
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorldChunkLoaded {
    /// 体。
    pub body: BodyKey,
    /// 体局部的世界区块。
    pub region: RegionKey,
}

/// region 级卸载通知：该 region 的最后一块降到 `None`。
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorldChunkUnloading {
    /// 体。
    pub body: BodyKey,
    /// 体局部的世界区块。
    pub region: RegionKey,
}

/// 世界区块插件内部的系统顺序（在 [GameTickSet::WorldChunk] 内 `chain`）。
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorldChunkSet {
    /// 消费驻留消息，维护 region 加载计数。
    Residency,
    /// 按 `Changed<ChunkAnchor>` 标脏 region。
    Dirty,
    /// 实体恢复 / 销毁：恢复消费流式层的 [ChunkEntities]；销毁在 Streaming
    /// 落盘之后。
    Lifecycle,
}

/// 世界区块插件：region 计数、脏标记与 region 级通知。
///
/// 依赖 [ChunkReached]（由 `ResidencyPlugin` 注册）与
/// [ChunkWillUnload](crate::voxel::ChunkWillUnload)（由 `VoxelStreamPlugin`
/// 注册）；缺任何一个都在 `build` 里直接 panic。
pub struct WorldChunkPlugin;

impl Plugin for WorldChunkPlugin {
    fn build(&self, app: &mut App) {
        if app.world().get_resource::<WorldChunkConfig>().is_none() {
            app.insert_resource(WorldChunkConfig::default());
        }
        let region_size = app.world().resource::<WorldChunkConfig>().region_size;
        assert!(
            region_size > 0,
            "WorldChunkConfig::region_size 不能为 0；请设置 >= 1（默认 4）"
        );

        // 先注册本插件自己的消息，再检查依赖的消息是否已由前置插件注册。
        app.init_resource::<RegionIndex>()
            .add_message::<WorldChunkLoaded>()
            .add_message::<WorldChunkUnloading>();

        assert!(
            app.world().contains_resource::<Messages<ChunkReached>>(),
            "WorldChunkPlugin 依赖 ChunkReached 消息；请先 add_plugins(ResidencyPlugin)"
        );
        assert!(
            app.world().contains_resource::<Messages<ChunkWillUnload>>(),
            "WorldChunkPlugin 依赖 ChunkWillUnload 消息；请先 add_plugins(VoxelStreamPlugin)"
        );
        assert!(
            app.world().contains_resource::<Messages<ChunkEntities>>(),
            "WorldChunkPlugin 依赖 ChunkEntities 消息；请先 add_plugins(VoxelStreamPlugin)"
        );

        app.configure_sets(
            FixedUpdate,
            (
                WorldChunkSet::Residency,
                WorldChunkSet::Dirty,
                WorldChunkSet::Lifecycle,
            )
                .chain()
                .in_set(GameTickSet::WorldChunk),
        )
        .add_systems(
            FixedUpdate,
            apply_residency_changes.in_set(WorldChunkSet::Residency),
        )
        .add_systems(FixedUpdate, mark_dirty.in_set(WorldChunkSet::Dirty))
        .add_systems(
            FixedUpdate,
            (restore_members, unload_members)
                .chain()
                .in_set(WorldChunkSet::Lifecycle)
                .in_set(GameTickSet::WorldChunk),
        );
    }
}

/// 系统 1：把 [ChunkReached] / [ChunkWillUnload] 折成 region 加载计数。
///
/// 只认**实际到达**：`level >= Data` 才算加载，`to == None` 才算卸载。重复消息
/// 幂等（同块重复到达不加计数、重复卸载不重复发消息）。
pub fn apply_residency_changes(
    mut reached: MessageReader<ChunkReached>,
    mut will_unload: MessageReader<ChunkWillUnload>,
    config: Res<WorldChunkConfig>,
    mut index: ResMut<RegionIndex>,
    mut loaded: MessageWriter<WorldChunkLoaded>,
    mut unloading: MessageWriter<WorldChunkUnloading>,
) {
    let region_size = config.region_size;

    for message in reached.read() {
        if message.level < Residency::Data {
            continue;
        }
        if index.chunk_loaded(message.body, message.key, region_size) {
            loaded.write(WorldChunkLoaded {
                body: message.body,
                region: region_of(message.key, region_size),
            });
        }
    }

    for message in will_unload.read() {
        if message.to != Residency::None {
            continue;
        }
        // 重复卸载消息直接跳过：chunk_unloaded 把 0 计数视为编程错误。
        if !index.is_chunk_loaded(message.body, message.key) {
            continue;
        }
        if index.chunk_unloaded(message.body, message.key, region_size) {
            unloading.write(WorldChunkUnloading {
                body: message.body,
                region: region_of(message.key, region_size),
            });
        }
    }
}

/// 系统 2：带 [Persist] 的实体换了体素块时，把当前所在 region 标脏。
///
/// **已知偏离**：文档要求新旧两个 region 都标脏，但 `Changed<ChunkAnchor>` 没有
/// 变更前值，本轮只标当前 region；旧 region 留待后续引入组件历史后补。
pub fn mark_dirty(
    changed: Query<(&ChunkAnchor, &Persist), Changed<ChunkAnchor>>,
    config: Res<WorldChunkConfig>,
    mut index: ResMut<RegionIndex>,
) {
    for (anchor, _persist) in &changed {
        if !anchor.is_valid() {
            continue;
        }
        index.mark_dirty(anchor.body, region_of(anchor.chunk, config.region_size));
    }
}

/// 系统 3：按流式层发来的 [ChunkEntities] 恢复块内实体（exclusive）。
///
/// 顺序由 [GameTickSet] 决定（`Residency -> Streaming -> WorldChunk`）：Streaming
/// 的 `resolve_chunks` 在存档命中且整块内容装成功后写消息，本系统在同一 tick 的
/// [WorldChunkSet::Lifecycle] 读到。运行期恢复**只**由这条消息驱动；启动期的批量
/// 恢复走 `load_world`，不经过这里。
///
/// - 逐条消息按 `(stable_entity_id, 记录顺序)` 排序后再应用，不依赖查询/哈希顺序；
/// - 复用 `load_world` 的 `ensure_entity`：稳定 ID 已存在就更新同一个实体，所以重复
///   投递幂等，[Persist] 等组件按记录覆盖；
/// - 单个实体应用失败只 `error!` 并跳过，不回滚整块、不 panic；
/// - 缺 [PersistRegistry] / [OpaqueComponents]（headless 装配）时整条消息 warn 后跳过。
pub fn restore_members(world: &mut World, mut cursor: Local<MessageCursor<ChunkEntities>>) {
    // 先把消息复制出来并释放 `Messages` 借用，后面才能继续改世界。
    let batches: Vec<ChunkEntities> = {
        let messages = world.resource::<Messages<ChunkEntities>>();
        cursor.read(messages).cloned().collect()
    };

    for batch in batches {
        if batch.entities.is_empty() {
            continue;
        }
        if !world.contains_resource::<PersistRegistry>()
            || !world.contains_resource::<OpaqueComponents>()
        {
            warn!(
                "world chunk: 缺少 PersistRegistry/OpaqueComponents，跳过 body {:?} 区块 {:?} 的 {} 个实体恢复",
                batch.body,
                batch.key,
                batch.entities.len()
            );
            continue;
        }

        // 先排好序再应用：稳定 ID 优先，同一 ID 保持记录顺序。
        let mut order: Vec<usize> = (0..batch.entities.len()).collect();
        order.sort_by_key(|&i| (batch.entities[i].stable_entity_id, i));

        world.resource_scope::<PersistRegistry, _>(|world, registry| {
            world.resource_scope::<OpaqueComponents, _>(|world, mut opaque| {
                for &i in &order {
                    let record = &batch.entities[i];
                    let entity = ensure_entity(world, record.stable_entity_id);
                    match registry.apply_entity(world, entity, record, &mut opaque) {
                        Ok(_) => {
                            // `ChunkAnchor` 是分组键、不在 payload 里：恢复时必须按消息补上，
                            // 否则该实体下次 `capture` / `unload_members` 都找不到。
                            world
                                .entity_mut(entity)
                                .insert(ChunkAnchor::new(batch.body, batch.key));
                        }
                        Err(error) => error!(
                            "world chunk: 恢复实体 {}（body {:?}，区块 {:?}）失败：{error}",
                            record.stable_entity_id, batch.body, batch.key
                        ),
                    }
                }
            });
        });
    }
}

/// 系统 4：区块降到 `None` 时销毁块内要存档的实体（exclusive）。
///
/// 顺序由 [GameTickSet] 决定（`Residency -> Streaming -> WorldChunk -> ...`），
/// 本系统在 [WorldChunkSet::Lifecycle]，所以这些块的内容与实体**已经**由
/// Streaming 的 `persist_pending_unloads` 写进存档，这里只做销毁。恢复端由
/// 同一 chain 里的 [restore_members] 负责。
///
/// - 只认 `to == Residency::None`：`Render -> Data` 是渲染档清理，实体不销毁；
/// - 只销毁 `(body, chunk)` 与消息完全一致的 [Persist] 实体，同 body 的其它块、
///   同块的其它 body 都不受影响；
/// - `Without<Real>`：[Real]（AI / 真实模拟）**永不进账本**，即使误挂了 [Persist]
///   也不在这里销毁——这是该约束的第二道保险（第一道在驻留层，不给它发 `None`）；
/// - 幂等：消息游标保证每条只处理一次；先 `get_entity` 探活再 `despawn`，实体被
///   别的路径先销毁时不会 panic；索引用 `remove_by_entity`，重复调用无副作用；
/// - 索引资源缺失（headless 装配）时跳过摘除：`StableIdPlugin` 的
///   `unregister_removed_stable_ids` 会在 PostUpdate 按 `RemovedComponents` 兜底；
/// - 确定性：先按 `(StableEntityId, Entity)` 排序再逐个销毁，不依赖查询顺序。
pub fn unload_members(world: &mut World, mut cursor: Local<MessageCursor<ChunkWillUnload>>) {
    // 同一块可能在同 tick 收到重复消息：按块键去重，只处理一次。
    let mut targets: BTreeSet<(BodyKey, ChunkKey)> = BTreeSet::new();
    {
        let messages = world.resource::<Messages<ChunkWillUnload>>();
        for message in cursor.read(messages) {
            if message.to == Residency::None {
                targets.insert((message.body, message.key));
            }
        }
    }
    if targets.is_empty() {
        return;
    }

    // 收集 + 排序放在同一个只读阶段：销毁会改 archetype，不能边查边改。
    let mut victims: Vec<(u64, Entity)> = Vec::new();
    {
        let mut query =
            world.query_filtered::<(Entity, &ChunkAnchor), (With<Persist>, Without<Real>)>();
        for (entity, anchor) in query.iter(world) {
            if !targets.contains(&(anchor.body, anchor.chunk)) {
                continue;
            }
            // 缺 StableEntityId 的实体同样要销毁（存档层会拒绝它），排序键用 MAX 兜底。
            let stable = world
                .get::<StableEntityId>(entity)
                .map_or(u64::MAX, |id| id.0);
            victims.push((stable, entity));
        }
    }
    victims.sort_unstable();

    for (_stable, entity) in victims {
        // 已被别的路径销毁（例如读档重建）：跳过，别再 despawn 一次。
        if world.get_entity(entity).is_err() {
            continue;
        }
        if let Some(mut index) = world.get_resource_mut::<StableEntityIndex>() {
            index.remove_by_entity(entity);
        }
        world.despawn(entity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use game_engine::identity::StableEntityId;
    use game_engine::save::{capture, EntityRecord, PersistComponent, StableTypeId};
    use serde::{Deserialize, Serialize};

    fn body(id: u64) -> BodyKey {
        BodyKey::new(StableEntityId(id))
    }

    /// 只补两条依赖消息的最小 App；生产装配由驻留 / 流式插件注册它们。
    fn test_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_message::<ChunkReached>()
            .add_message::<ChunkWillUnload>()
            .add_message::<ChunkEntities>()
            .init_resource::<StableEntityIndex>()
            .add_plugins(WorldChunkPlugin);
        app
    }

    fn drain_loaded(app: &mut App) -> Vec<WorldChunkLoaded> {
        app.world_mut()
            .resource_mut::<Messages<WorldChunkLoaded>>()
            .drain()
            .collect()
    }

    fn drain_unloading(app: &mut App) -> Vec<WorldChunkUnloading> {
        app.world_mut()
            .resource_mut::<Messages<WorldChunkUnloading>>()
            .drain()
            .collect()
    }

    /// 测试组件：证明销毁带走的是整个实体，而不只是索引条目。
    #[derive(Component, Debug)]
    struct Cargo;

    /// spawn 一个「要存档」的实体，并按生产装配那样登记进 [StableEntityIndex]。
    fn spawn_persisted(app: &mut App, b: BodyKey, key: ChunkKey, id: u64) -> Entity {
        let entity = app
            .world_mut()
            .spawn((
                StableEntityId(id),
                Persist {
                    prototype_type_id: 7,
                },
                ChunkAnchor::new(b, key),
                Cargo,
            ))
            .id();
        app.world_mut()
            .resource_mut::<StableEntityIndex>()
            .insert(StableEntityId(id), entity);
        entity
    }

    fn write_unload(app: &mut App, b: BodyKey, key: ChunkKey, to: Residency) {
        app.world_mut().write_message(ChunkWillUnload {
            body: b,
            key,
            from: Residency::Data,
            to,
        });
    }

    #[test]
    fn region_of_handles_negative_coordinates() {
        // -1 落在 [-4, 0) 这一格，不能和 0 / 正数混在一起。
        assert_eq!(
            region_of(ChunkKey::new(-1, -1, -1), 4),
            RegionKey {
                x: -1,
                y: -1,
                z: -1
            }
        );
        assert_eq!(
            region_origin(
                RegionKey {
                    x: -1,
                    y: -1,
                    z: -1
                },
                4
            ),
            ChunkKey::new(-4, -4, -4)
        );
        assert_eq!(
            region_of(ChunkKey::new(-4, -4, -4), 4),
            RegionKey {
                x: -1,
                y: -1,
                z: -1
            }
        );
        assert_eq!(
            region_of(ChunkKey::new(-5, -5, -5), 4),
            RegionKey {
                x: -2,
                y: -2,
                z: -2
            }
        );
        assert_eq!(
            region_of(ChunkKey::new(0, 3, 4), 4),
            RegionKey { x: 0, y: 0, z: 1 }
        );
    }

    #[test]
    fn region_origin_contains_key() {
        for region_size in 1..=7u32 {
            let size = region_size as i32;
            for x in -9..=9 {
                for y in -9..=9 {
                    for z in -9..=9 {
                        let key = ChunkKey::new(x, y, z);
                        let region = region_of(key, region_size);
                        let origin = region_origin(region, region_size);
                        assert!(origin.x <= key.x && key.x < origin.x + size);
                        assert!(origin.y <= key.y && key.y < origin.y + size);
                        assert!(origin.z <= key.z && key.z < origin.z + size);
                        assert_eq!(region_of(origin, region_size), region);
                    }
                }
            }
        }
    }

    #[test]
    fn region_index_counts_and_dedups_chunks() {
        let mut index = RegionIndex::default();
        let b = body(1);
        let k0 = ChunkKey::new(0, 0, 0);
        let k1 = ChunkKey::new(1, 0, 0);
        let other = ChunkKey::new(4, 0, 0);
        let region = region_of(k0, 4);
        let other_region = region_of(other, 4);

        assert!(index.chunk_loaded(b, k0, 4));
        // 同一块的重复到达：不重复加计数。
        assert!(!index.chunk_loaded(b, k0, 4));
        assert_eq!(index.loaded_count(b, region), 1);

        // 同 region 的另一块：加计数，但不再触发 region 的 0 -> 1。
        assert!(!index.chunk_loaded(b, k1, 4));
        assert_eq!(index.loaded_count(b, region), 2);
        assert!(index.is_loaded(b, region));

        // 另一个 region 的第一块：重新触发 0 -> 1。
        assert!(index.chunk_loaded(b, other, 4));
        assert_eq!(index.loaded_count(b, other_region), 1);
        assert_eq!(index.loaded_region_count(), 2);

        assert!(!index.chunk_unloaded(b, k0, 4));
        assert_eq!(index.loaded_count(b, region), 1);
        assert!(index.chunk_unloaded(b, k1, 4));
        assert_eq!(index.loaded_count(b, region), 0);
        assert!(!index.is_loaded(b, region));
        assert!(!index.is_chunk_loaded(b, k1));
        assert_eq!(index.loaded_region_count(), 1);
    }

    #[test]
    fn dirty_regions_drain_in_key_order() {
        let mut index = RegionIndex::default();
        let a = body(2);
        let b = body(1);
        let r0 = RegionKey { x: 0, y: 1, z: 0 };
        let r1 = RegionKey { x: 1, y: 0, z: 0 };
        let r2 = RegionKey { x: 0, y: 0, z: 0 };

        index.mark_dirty(a, r2);
        index.mark_dirty(b, r1);
        index.mark_dirty(b, r0);
        index.mark_dirty(a, r2); // 重复标记幂等

        assert_eq!(index.dirty_len(), 3);
        assert!(index.is_dirty(a, r2));
        assert_eq!(
            index.take_dirty(),
            vec![(b, r0), (b, r1), (a, r2)],
            "take_dirty 必须按 (BodyKey, RegionKey) 升序"
        );
        assert_eq!(index.dirty_len(), 0);
        assert!(index.take_dirty().is_empty());
        assert!(!index.is_dirty(a, r2));
    }

    #[test]
    fn residency_messages_update_region_index_idempotently() {
        let mut app = test_app();
        let b = body(1);
        let key = ChunkKey::new(-1, 2, -3);
        let region = region_of(key, 4);

        // 同一条 Data 到达消息投递两次。
        app.world_mut().write_message(ChunkReached {
            body: b,
            key,
            level: Residency::Data,
        });
        app.world_mut().write_message(ChunkReached {
            body: b,
            key,
            level: Residency::Data,
        });
        app.world_mut().run_schedule(FixedUpdate);

        assert_eq!(
            app.world()
                .resource::<RegionIndex>()
                .loaded_count(b, region),
            1
        );
        assert_eq!(
            drain_loaded(&mut app),
            vec![WorldChunkLoaded { body: b, region }]
        );

        // 升到 Render 仍是同一块，不重复计数、不重复发。
        app.world_mut().write_message(ChunkReached {
            body: b,
            key,
            level: Residency::Render,
        });
        app.world_mut().run_schedule(FixedUpdate);
        assert_eq!(
            app.world()
                .resource::<RegionIndex>()
                .loaded_count(b, region),
            1
        );
        assert!(drain_loaded(&mut app).is_empty());

        // None 不算到达。
        app.world_mut().write_message(ChunkReached {
            body: b,
            key,
            level: Residency::None,
        });
        app.world_mut().run_schedule(FixedUpdate);
        assert_eq!(
            app.world()
                .resource::<RegionIndex>()
                .loaded_count(b, region),
            1
        );

        // 同一条卸载消息投递两次。
        app.world_mut().write_message(ChunkWillUnload {
            body: b,
            key,
            from: Residency::Data,
            to: Residency::None,
        });
        app.world_mut().write_message(ChunkWillUnload {
            body: b,
            key,
            from: Residency::Data,
            to: Residency::None,
        });
        app.world_mut().run_schedule(FixedUpdate);

        assert_eq!(
            app.world()
                .resource::<RegionIndex>()
                .loaded_count(b, region),
            0
        );
        assert_eq!(
            drain_unloading(&mut app),
            vec![WorldChunkUnloading { body: b, region }]
        );
    }

    #[test]
    fn mark_dirty_marks_current_region_for_persisted_anchor() {
        let mut app = test_app();
        let b = body(3);
        let key = ChunkKey::new(-1, 0, 5);
        let entity = app
            .world_mut()
            .spawn((
                ChunkAnchor::new(b, key),
                Persist {
                    prototype_type_id: 7,
                },
            ))
            .id();

        app.world_mut().run_schedule(FixedUpdate);
        let old_region = region_of(key, 4);
        {
            let index = app.world().resource::<RegionIndex>();
            assert!(index.is_dirty(b, old_region));
            assert_eq!(index.dirty_len(), 1);
        }

        // 没有 Persist 的实体不参与标脏。
        app.world_mut()
            .spawn(ChunkAnchor::new(b, ChunkKey::new(8, 0, 0)));

        let new_key = ChunkKey::new(8, 0, 0);
        app.world_mut()
            .entity_mut(entity)
            .insert(ChunkAnchor::new(b, new_key));
        let _ = app.world_mut().resource_mut::<RegionIndex>().take_dirty();
        app.world_mut().run_schedule(FixedUpdate);

        let new_region = region_of(new_key, 4);
        assert_eq!(
            app.world_mut().resource_mut::<RegionIndex>().take_dirty(),
            vec![(b, new_region)],
            "本轮只标实体当前所在 region"
        );
    }

    #[test]
    #[should_panic(expected = "ChunkReached")]
    fn plugin_panics_without_residency_messages() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(WorldChunkPlugin);
    }

    #[test]
    #[should_panic(expected = "ChunkWillUnload")]
    fn plugin_panics_without_stream_messages() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_message::<ChunkReached>();
        app.add_plugins(WorldChunkPlugin);
    }

    #[test]
    #[should_panic(expected = "region_size 不能为 0")]
    fn plugin_panics_on_zero_region_size() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_message::<ChunkReached>();
        app.add_message::<ChunkWillUnload>();
        app.insert_resource(WorldChunkConfig { region_size: 0 });
        app.add_plugins(WorldChunkPlugin);
    }

    #[test]
    fn unload_members_despawns_persisted_entities() {
        let mut app = test_app();
        let b = body(11);
        let key = ChunkKey::new(2, -1, 0);
        let entity = spawn_persisted(&mut app, b, key, 41);
        assert_eq!(
            app.world()
                .resource::<StableEntityIndex>()
                .entity(StableEntityId(41)),
            Some(entity)
        );

        write_unload(&mut app, b, key, Residency::None);
        app.world_mut().run_schedule(FixedUpdate);

        assert!(
            app.world().get_entity(entity).is_err(),
            "降到 None 后块内 Persist 实体必须被销毁"
        );
        let index = app.world().resource::<StableEntityIndex>();
        assert_eq!(index.entity(StableEntityId(41)), None);
        assert_eq!(index.id(entity), None);

        // 消息已消费：再跑一帧不重复处理、不 panic。
        app.world_mut().run_schedule(FixedUpdate);
        assert!(app.world().get_entity(entity).is_err());
    }

    #[test]
    fn unload_members_keeps_real_entities() {
        let mut app = test_app();
        let b = body(12);
        let key = ChunkKey::new(0, 0, 0);
        let persisted = spawn_persisted(&mut app, b, key, 51);
        // 同时挂 Persist：排除必须来自 Real，而不是「没有 Persist 所以不管」。
        let real = app
            .world_mut()
            .spawn((
                StableEntityId(52),
                Persist {
                    prototype_type_id: 9,
                },
                Real,
                ChunkAnchor::new(b, key),
                Cargo,
            ))
            .id();

        write_unload(&mut app, b, key, Residency::None);
        app.world_mut().run_schedule(FixedUpdate);

        assert!(app.world().get_entity(persisted).is_err());
        assert!(
            app.world().get_entity(real).is_ok(),
            "带 Real 的实体永不进账本，即使误挂 Persist 也不能销毁"
        );
    }

    #[test]
    fn unload_members_ignores_render_demotion() {
        let mut app = test_app();
        let b = body(13);
        let key = ChunkKey::new(1, 1, 1);
        let entity = spawn_persisted(&mut app, b, key, 61);

        // Render -> Data 只是渲染档清理，不属于数据生命周期卸载。
        write_unload(&mut app, b, key, Residency::Data);
        app.world_mut().run_schedule(FixedUpdate);

        assert!(app.world().get_entity(entity).is_ok());
        assert_eq!(
            app.world()
                .resource::<StableEntityIndex>()
                .entity(StableEntityId(61)),
            Some(entity)
        );
    }

    #[test]
    fn unload_members_only_touches_matching_chunk() {
        let mut app = test_app();
        let b = body(14);
        let other_body = body(15);
        let key = ChunkKey::new(3, 0, -2);
        let same_body_other_chunk = ChunkKey::new(3, 0, -1);
        let victim = spawn_persisted(&mut app, b, key, 71);
        let sibling_chunk = spawn_persisted(&mut app, b, same_body_other_chunk, 72);
        // 同块、不同 body（例如另一条船）。
        let other_body_same_chunk = spawn_persisted(&mut app, other_body, key, 73);

        write_unload(&mut app, b, key, Residency::None);
        app.world_mut().run_schedule(FixedUpdate);

        assert!(app.world().get_entity(victim).is_err());
        assert!(
            app.world().get_entity(sibling_chunk).is_ok(),
            "同 body 的另一块不受影响"
        );
        assert!(
            app.world().get_entity(other_body_same_chunk).is_ok(),
            "同块的另一个 body 不受影响"
        );
        let index = app.world().resource::<StableEntityIndex>();
        assert_eq!(index.entity(StableEntityId(71)), None);
        assert_eq!(index.entity(StableEntityId(72)), Some(sibling_chunk));
        assert_eq!(
            index.entity(StableEntityId(73)),
            Some(other_body_same_chunk)
        );
    }

    /// 测试用可存档组件：值要能跨 `capture` -> `apply_entity` 原样回来。
    #[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct RestoreMarker(u32);

    impl PersistComponent for RestoreMarker {
        const STABLE_TYPE_ID: StableTypeId = StableTypeId::new(21);
    }

    /// 给 App 装恢复所需的资源：注册 [RestoreMarker] 与 [Persist]。
    fn attach_restore_registry(app: &mut App) {
        let mut registry = PersistRegistry::new();
        registry.register::<RestoreMarker>().expect("注册测试组件");
        registry.register::<Persist>().expect("注册 Persist");
        app.insert_resource(registry);
        app.insert_resource(OpaqueComponents::default());
    }

    /// 用真实 `capture` 从源世界造一条记录，保证负载与存档路径一致。
    fn capture_records(b: BodyKey, key: ChunkKey, id: u64, value: u32) -> Vec<EntityRecord> {
        let mut world = World::new();
        world.spawn((
            StableEntityId(id),
            Persist {
                prototype_type_id: 9,
            },
            ChunkAnchor::new(b, key),
            RestoreMarker(value),
        ));

        let mut registry = PersistRegistry::new();
        registry.register::<RestoreMarker>().expect("注册测试组件");
        registry.register::<Persist>().expect("注册 Persist");
        let snapshot = capture(&world, &registry, &OpaqueComponents::default()).expect("capture");
        snapshot.for_chunk(b, key).to_vec()
    }

    fn write_entities(app: &mut App, b: BodyKey, key: ChunkKey, entities: Vec<EntityRecord>) {
        app.world_mut().write_message(ChunkEntities {
            body: b,
            key,
            entities,
        });
    }

    #[test]
    fn restore_members_spawns_from_record() {
        let mut app = test_app();
        attach_restore_registry(&mut app);
        let b = body(21);
        let key = ChunkKey::new(-2, 1, 3);
        let records = capture_records(b, key, 88, 5);
        assert_eq!(records.len(), 1, "capture 应产出一条实体记录");

        write_entities(&mut app, b, key, records);
        app.world_mut().run_schedule(FixedUpdate);

        // 记录里只有组件表负载；ChunkAnchor 是分组键，不在 payload 里。
        let mut query = app
            .world_mut()
            .query::<(&StableEntityId, &RestoreMarker, &Persist)>();
        let restored: Vec<_> = query
            .iter(app.world())
            .map(|(id, marker, persist)| (*id, *marker, *persist))
            .collect();
        assert_eq!(restored.len(), 1, "一条记录只应恢复一个实体");
        let (id, marker, persist) = restored[0];
        assert_eq!(id, StableEntityId(88));
        assert_eq!(marker, RestoreMarker(5));
        assert_eq!(persist.prototype_type_id, 9);

        let index = app.world().resource::<StableEntityIndex>();
        let indexed = index
            .entity(StableEntityId(88))
            .expect("索引应登记恢复的实体");
        assert!(app.world().get::<RestoreMarker>(indexed).is_some());
        assert_eq!(
            app.world().get::<ChunkAnchor>(indexed),
            Some(&ChunkAnchor::new(b, key)),
            "ChunkAnchor 不在 payload 里，恢复时必须按消息的分组键补回"
        );
    }

    #[test]
    fn restored_entity_is_destroyed_by_later_unload() {
        let mut app = test_app();
        attach_restore_registry(&mut app);
        let b = body(24);
        let key = ChunkKey::new(-1, 0, 2);
        write_entities(&mut app, b, key, capture_records(b, key, 91, 8));
        app.world_mut().run_schedule(FixedUpdate);

        // 恢复出来的实体必须带 anchor，否则降到 None 时 unload_members 找不到它。
        write_unload(&mut app, b, key, Residency::None);
        app.world_mut().run_schedule(FixedUpdate);

        let mut query = app
            .world_mut()
            .query_filtered::<Entity, With<StableEntityId>>();
        assert_eq!(
            query.iter(app.world()).count(),
            0,
            "恢复后的实体再次卸载时必须被销毁"
        );
    }

    #[test]
    fn restore_members_is_idempotent() {
        let mut app = test_app();
        attach_restore_registry(&mut app);
        let b = body(22);
        let key = ChunkKey::new(1, 1, 1);
        let records = capture_records(b, key, 89, 6);

        // 同一条记录投递两次：第二次必须更新同一个实体，而不是再 spawn 一个。
        for _ in 0..2 {
            write_entities(&mut app, b, key, records.clone());
            app.world_mut().run_schedule(FixedUpdate);
        }

        let mut query = app
            .world_mut()
            .query_filtered::<Entity, With<StableEntityId>>();
        let restored: Vec<Entity> = query.iter(app.world()).collect();
        assert_eq!(restored.len(), 1, "重复投递不应产生第二个实体");
        assert_eq!(
            app.world().get::<RestoreMarker>(restored[0]),
            Some(&RestoreMarker(6))
        );
        let index = app.world().resource::<StableEntityIndex>();
        assert_eq!(index.entity(StableEntityId(89)), Some(restored[0]));
    }

    #[test]
    fn restore_members_skips_missing_registry() {
        let mut app = test_app();
        // 不插入 PersistRegistry / OpaqueComponents：模拟 headless 装配。
        let b = body(23);
        let key = ChunkKey::new(0, 0, 0);
        let records = capture_records(b, key, 90, 7);
        write_entities(&mut app, b, key, records);

        app.world_mut().run_schedule(FixedUpdate);

        assert!(app.world().get_resource::<PersistRegistry>().is_none());
        let mut query = app
            .world_mut()
            .query_filtered::<Entity, With<StableEntityId>>();
        assert_eq!(query.iter(app.world()).count(), 0, "缺注册表时应整条跳过");
    }
}
