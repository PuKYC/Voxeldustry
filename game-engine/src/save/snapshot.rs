//! 实体快照：把世界里所有可存档实体按 `(BodyKey, ChunkKey)` 分组（《存档格式规格》§4）。
//!
//! 本模块是一个**纯函数层**：输入 `&World` + 注册表 + 未注册组件表，输出
//! [`EntitySnapshot`]。不写盘、不碰 `SaveStore`、不构造完整的 `ChunkRecord`。
//!
//! # 为什么现在还不接 SaveStore
//!
//! `ChunkRecord` 不只有 `entities`，还有 `content`（整块体素内容，`export_tree::<u8>`
//! 的原始字节）与 `proposals`。体素内容的**产出方**（流式层里持有 voxel tree 的那一方）
//! 还没接进来，所以这一层根本拿不到 `content`。如果现在就把分组结果包成
//! `ChunkRecord` 交给 `SaveStore`，只能填 `content = None`；而 §3.1 里
//! `content = None` 的语义是“这块没有整块内容、加载时照常跑生成器”，落盘就会把
//! 已经存好的体素内容覆盖掉，**这是数据丢失**，不是“暂时少存一点”。
//!
//! 所以本层的产物故意只有“实体分组”：调用方（流式层）已经持有那块 `ChunkRecord`
//! 时，用 [`EntitySnapshot::apply_to`] 把实体写进去，`content` / `proposals` /
//! `generator_version` 保持调用方的值不动。等流式层能产出 `content` 之后，
//! 再由它把二者合成一条记录落盘。
//!
//! # 归属块（§4）
//!
//! 实体属于哪个块只由 `ChunkAnchor` 决定，不按坐标反推：`body` 是 `BodyKey`
//! （岛的稳定 ID），`chunk` 是体局部 `ChunkKey`，两者正是 §3 索引项的键。
//! 带 [`Persist`] 却没有 `ChunkAnchor` 的实体**报错**而不是跳过——静默丢实体
//! 比报错糟得多。
//!
//! # 哪些实体进快照
//!
//! 只有带 [`Persist`] 的实体。`AI` / `Real` 这类纯运行时标记永远不带 `Persist`
//! （AI 永不进账本），所以它们不会出现在快照里，也不需要在这里再判一次。
//!
//! [`Persist`] 自身也以类型 ID 1 落盘（见它的 `PersistComponent` 实现），所以
//! 「保存 -> 载入」后的实体天然仍带 `Persist`，下次快照不会漏掉它。
//!
//! # 确定性（§8）
//!
//! `World::iter_entities` 的遍历顺序**不是** API 契约（它按 archetype 布局走），
//! 因此最终输出只由排序决定：
//!
//! - 块按键 `(BodyKey, ChunkKey)` 升序（`BTreeMap` 自带的顺序）；
//! - 块内实体按 `stable_entity_id` 升序；
//! - 同一 `stable_entity_id` 出现两次直接报 `Duplicate`，不靠后写覆盖前写。
//!
//! 实体的组件负载由 [`PersistRegistry::encode_entity_with_opaque`] 按组件
//! `StableTypeId` 升序生成，所以同一世界重复 `capture` 的字节完全相同。
//! 本模块不用 `HashMap` / 浮点 / 随机数。
//!
//! # 未注册组件（§4.2）
//!
//! 未注册的组件不解析、不丢：注册表把它们的原字节从 [`OpaqueComponents`] 里取出来，
//! 原样写进 payload。本模块只负责把 `opaque` 透传下去，不做额外解释。

use std::collections::BTreeMap;

use ::voxel::ChunkKey;
use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::body_key::BodyKey;
use crate::identity::StableEntityId;
use crate::residency::ChunkAnchor;
use crate::save::persist::{OpaqueComponents, PersistComponent, PersistRegistry, StableTypeId};
use crate::save::{ChunkRecord, EntityRecord, SaveError, SaveErrorKind};

/// 可存档标记：只有带这个组件的实体才进存档快照。
///
/// `prototype_type_id` 是**实体原型** ID（`EntityRecord::stable_type_id`），与组件
/// 的 `StableTypeId` 各用一套命名空间，编号撞车没有含义。`0` 是保留值，捕获时拒绝。
///
/// `AI` / `Real` 永远不会带它：AI 不进账本，`Real` 的视觉状态也不需要落盘。
///
/// 这个标记本身也是 `PersistComponent`（类型 ID 1，引擎保留），所以它的
/// `prototype_type_id` 会随实体一起落盘，载入后仍是存档对象。
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Persist {
    /// 实体原型 ID；必须非 0。
    pub prototype_type_id: u32,
}

impl PersistComponent for Persist {
    /// 1 是引擎保留的 Persist 类型 ID，业务组件不得占用。
    ///
    /// 快照准入标记必须自己也在存档里，否则「保存 -> 载入 -> 再保存」会丢掉
    /// `Persist`，实体下次不再被快照收集。
    const STABLE_TYPE_ID: StableTypeId = StableTypeId::new(1);
}

/// 一次捕获得到的实体分组：`(BodyKey, ChunkKey)` -> 该块实体（按 `stable_entity_id` 升序）。
///
/// `BTreeMap` 保证块的遍历顺序 = `(BodyKey, ChunkKey)` 升序，与实体在 `World` 里的
/// 存储顺序无关（§8）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EntitySnapshot {
    chunks: BTreeMap<(BodyKey, ChunkKey), Vec<EntityRecord>>,
}

impl EntitySnapshot {
    /// 是否一个块都没有（空世界 / 没有带 [`Persist`] 的实体）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// 块数（不是实体数；实体数见 [`Self::entity_count`]）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    /// 实体总数。
    #[must_use]
    pub fn entity_count(&self) -> usize {
        self.chunks.values().map(Vec::len).sum()
    }

    /// 按 `(BodyKey, ChunkKey)` 升序迭代 `(块键, 该块实体)`。
    pub fn chunks(&self) -> impl Iterator<Item = ((BodyKey, ChunkKey), &[EntityRecord])> + '_ {
        self.chunks
            .iter()
            .map(|(key, records)| (*key, records.as_slice()))
    }

    /// 取某块的实体；没有这个块时返回空切片（不是 `Option`，调用方少一层分支）。
    #[must_use]
    pub fn for_chunk(&self, body: BodyKey, chunk: ChunkKey) -> &[EntityRecord] {
        self.chunks
            .get(&(body, chunk))
            .map_or(&[], Vec::as_slice)
    }

    /// 把某块的实体写进调用方持有的 `ChunkRecord`，**只动 `entities`**。
    ///
    /// 语义是“快照权威”：没有这个块时把 `entities` 清空（说明这块的实体已经全部
    /// 不在世界里了）。`content` / `proposals` / `generator_version` 一律不动——
    /// 那些字段的产出方是流式层，不是本模块（见模块文档里“不接 SaveStore”的理由）。
    pub fn apply_to(&self, body: BodyKey, chunk: ChunkKey, record: &mut ChunkRecord) {
        record.entities.clear();
        record
            .entities
            .extend_from_slice(self.for_chunk(body, chunk));
    }

    /// 出现过实体的 `BodyKey`，去重后升序。
    #[must_use]
    pub fn bodies(&self) -> Vec<BodyKey> {
        let mut bodies: Vec<BodyKey> = Vec::new();
        for (body, _) in self.chunks.keys() {
            // 键已经按 (body, chunk) 升序，只需和上一个比。
            if bodies.last() != Some(body) {
                bodies.push(*body);
            }
        }
        bodies
    }
}

/// 遍历世界，把带 [`Persist`] 的实体按 `(BodyKey, ChunkKey)` 分组。
///
/// 只读 `World`（`iter_entities` 在 `&World` 上就能用），不修改任何实体。
///
/// # 错误
///
/// 逐实体按固定顺序检查：原型 ID 为 0、缺 `ChunkAnchor`、`anchor.body` 为
/// `BodyKey::INVALID`；然后交给注册表编码（缺 `StableEntityId`、组件编码失败的
/// 错误**原样向上返回**，不吞成空 payload）。全部编码完成后按 `stable_entity_id`
/// 排序并查重，重复返回 `Duplicate`。
///
/// 候选实体先按 `(stable_entity_id, Entity)` 排序再校验，所以同一个世界上的错误
/// 选择也不依赖 `iter_entities` 的顺序。
pub fn capture(
    world: &World,
    registry: &PersistRegistry,
    opaque: &OpaqueComponents,
) -> Result<EntitySnapshot, SaveError> {
    let mut candidates: Vec<(Entity, Persist)> = world
        .iter_entities()
        .filter_map(|entity| {
            entity
                .get::<Persist>()
                .map(|persist| (entity.id(), *persist))
        })
        .collect();
    // 先排序再校验：iter_entities 的顺序不是契约，错误也不能随 archetype 布局漂移。
    candidates.sort_by_key(|(entity, _)| {
        let stable = world.get::<StableEntityId>(*entity).map_or(u64::MAX, |id| id.0);
        (stable, *entity)
    });

    let mut encoded: Vec<((BodyKey, ChunkKey), EntityRecord)> = Vec::with_capacity(candidates.len());
    for (entity, persist) in candidates {
        if persist.prototype_type_id == 0 {
            return Err(SaveError::new(
                "entity.persist.prototype_type_id",
                0,
                SaveErrorKind::InvalidValue {
                    what: "persist prototype_type_id",
                    value: 0,
                },
            ));
        }

        let stable_entity_id = world.get::<StableEntityId>(entity).map_or(0, |id| id.0);
        let Some(anchor) = world.get::<ChunkAnchor>(entity) else {
            return Err(SaveError::new(
                "entity.chunk_anchor",
                0,
                SaveErrorKind::Missing {
                    what: "ChunkAnchor for Persist entity",
                    id: stable_entity_id,
                },
            ));
        };
        if !anchor.body.is_valid() {
            return Err(SaveError::new(
                "entity.chunk_anchor.body",
                0,
                SaveErrorKind::InvalidValue {
                    what: "persist anchor body",
                    value: 0,
                },
            ));
        }

        // 缺 StableEntityId / 组件编码失败都原样返回，不在这一层抹平成空负载。
        let record =
            registry.encode_entity_with_opaque(world, entity, persist.prototype_type_id, opaque)?;
        encoded.push(((anchor.body, anchor.chunk), record));
    }

    // 全局按 stable_entity_id 升序：块内顺序与块键顺序都由这一步 + BTreeMap 决定。
    encoded.sort_by_key(|(_, record)| record.stable_entity_id);
    for pair in encoded.windows(2) {
        if pair[0].1.stable_entity_id == pair[1].1.stable_entity_id {
            return Err(SaveError::new(
                "snapshot.entities",
                0,
                SaveErrorKind::Duplicate {
                    what: "snapshot stable_entity_id",
                    id: pair[0].1.stable_entity_id,
                },
            ));
        }
    }

    let mut chunks: BTreeMap<(BodyKey, ChunkKey), Vec<EntityRecord>> = BTreeMap::new();
    for (key, record) in encoded {
        chunks.entry(key).or_default().push(record);
    }
    Ok(EntitySnapshot { chunks })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    use crate::save::decode_component_table;
    use crate::save::persist::{PersistComponent, PersistPlugin, StableTypeId};

    /// 测试用可存档组件：只关心“注册后能被编进 payload”。
    #[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct Health(u32);

    impl PersistComponent for Health {
        const STABLE_TYPE_ID: StableTypeId = StableTypeId::new(3);
    }

    fn registry() -> PersistRegistry {
        let mut registry = PersistRegistry::new();
        registry.register::<Health>().expect("注册 Health");
        registry
    }

    /// Health + Persist（引擎保留 ID 1）。
    fn registry_with_persist() -> PersistRegistry {
        let mut registry = registry();
        registry.register::<Persist>().expect("注册 Persist");
        registry
    }

    fn body(id: u64) -> BodyKey {
        BodyKey::new(StableEntityId(id))
    }

    fn chunk(x: i32, y: i32, z: i32) -> ChunkKey {
        ChunkKey::new(x, y, z)
    }

    fn anchor(body_id: u64, chunk_key: ChunkKey) -> ChunkAnchor {
        ChunkAnchor::new(body(body_id), chunk_key)
    }

    /// spawn 一个带 Persist + StableEntityId + ChunkAnchor 的实体。
    fn spawn_persist(world: &mut World, stable: u64, prototype: u32, at: ChunkAnchor) -> Entity {
        world
            .spawn((
                StableEntityId(stable),
                Persist {
                    prototype_type_id: prototype,
                },
                at,
            ))
            .id()
    }

    fn empty_world() -> World {
        World::new()
    }

    fn ids(records: &[EntityRecord]) -> Vec<u64> {
        records.iter().map(|record| record.stable_entity_id).collect()
    }

    /// 块键按 (BodyKey, ChunkKey) 升序、块内实体按 stable_entity_id 升序。
    #[test]
    fn capture_groups_entities_by_body_and_chunk_in_sorted_order() {
        let mut world = empty_world();
        // 故意按乱序 spawn：输出顺序不能跟着 world 的存储顺序走。
        spawn_persist(&mut world, 30, 7, anchor(2, chunk(1, 0, 0)));
        spawn_persist(&mut world, 10, 7, anchor(2, chunk(1, 0, 0)));
        spawn_persist(&mut world, 50, 7, anchor(1, chunk(0, 0, 1)));
        spawn_persist(&mut world, 20, 7, anchor(1, chunk(0, 0, 0)));

        let snapshot = capture(&world, &registry(), &OpaqueComponents::default()).expect("捕获成功");

        assert_eq!(snapshot.len(), 3);
        assert_eq!(snapshot.entity_count(), 4);
        assert_eq!(snapshot.bodies(), vec![body(1), body(2)]);

        let keys: Vec<(BodyKey, ChunkKey)> = snapshot.chunks().map(|(key, _)| key).collect();
        assert_eq!(
            keys,
            vec![
                (body(1), chunk(0, 0, 0)),
                (body(1), chunk(0, 0, 1)),
                (body(2), chunk(1, 0, 0)),
            ],
            "块键必须按 (BodyKey, ChunkKey) 升序"
        );

        assert_eq!(ids(snapshot.for_chunk(body(1), chunk(0, 0, 0))), vec![20]);
        assert_eq!(ids(snapshot.for_chunk(body(1), chunk(0, 0, 1))), vec![50]);
        assert_eq!(
            ids(snapshot.for_chunk(body(2), chunk(1, 0, 0))),
            vec![10, 30],
            "块内实体必须按 stable_entity_id 升序"
        );
        assert!(snapshot.for_chunk(body(9), chunk(0, 0, 0)).is_empty());
    }

    /// 同一世界重复捕获必须逐字节相等（确定性）。
    #[test]
    fn capture_is_deterministic_for_the_same_world() {
        let mut world = empty_world();
        spawn_persist(&mut world, 4, 9, anchor(1, chunk(2, 0, 0)));
        spawn_persist(&mut world, 2, 9, anchor(1, chunk(0, 0, 0)));
        spawn_persist(&mut world, 7, 9, anchor(3, chunk(0, 0, 0)));

        let registry = registry();
        let opaque = OpaqueComponents::default();
        let first = capture(&world, &registry, &opaque).expect("第一次捕获成功");
        let second = capture(&world, &registry, &opaque).expect("第二次捕获成功");

        assert_eq!(first, second);
        let first_bytes: Vec<_> = first.chunks().collect();
        let second_bytes: Vec<_> = second.chunks().collect();
        assert_eq!(first_bytes, second_bytes, "块与实体顺序也必须一致");
    }

    /// 未注册原型 ID 原样进 stable_type_id，未注册组件经 opaque 原样进 payload。
    #[test]
    fn capture_keeps_unregistered_prototype_and_opaque_components() {
        let mut world = empty_world();
        let entity = spawn_persist(&mut world, 7, 42, anchor(5, chunk(0, 0, 0)));
        // 注册过的组件照常编码。
        world.entity_mut(entity).insert(Health(9));

        let mut opaque = OpaqueComponents::default();
        opaque.insert(7, 999, vec![0xAB, 0xCD, 0x00]);

        let snapshot = capture(&world, &registry(), &opaque).expect("捕获成功");
        let records = snapshot.for_chunk(body(5), chunk(0, 0, 0));
        assert_eq!(records.len(), 1);

        let record = &records[0];
        assert_eq!(record.stable_entity_id, 7);
        assert_eq!(record.stable_type_id, 42, "未注册的原型 ID 必须原样保留");

        let components = decode_component_table(&record.payload).expect("payload 外层框合法");
        let type_ids: Vec<u32> = components.iter().map(|c| c.stable_type_id).collect();
        assert_eq!(type_ids, vec![3, 999], "组件按 StableTypeId 升序");

        let opaque_row = components
            .iter()
            .find(|c| c.stable_type_id == 999)
            .expect("opaque 行必须被带出");
        assert_eq!(opaque_row.payload, vec![0xAB, 0xCD, 0x00], "opaque 字节必须原样");

        let health_row = components
            .iter()
            .find(|c| c.stable_type_id == 3)
            .expect("已注册组件必须有负载");
        assert_eq!(
            postcard::from_bytes::<Health>(&health_row.payload).expect("postcard 编码"),
            Health(9)
        );
    }

    /// 带 Persist 却缺 ChunkAnchor：报错，不静默跳过。
    #[test]
    fn capture_rejects_persist_entity_without_chunk_anchor() {
        let mut world = empty_world();
        world.spawn((
            StableEntityId(5),
            Persist {
                prototype_type_id: 1,
            },
        ));

        let error = capture(&world, &registry(), &OpaqueComponents::default())
            .expect_err("缺 ChunkAnchor 必须报错");
        assert_eq!(
            error.kind(),
            &SaveErrorKind::Missing {
                what: "ChunkAnchor for Persist entity",
                id: 5,
            }
        );
    }

    /// anchor.body = BodyKey::INVALID 视为未锚定，报 InvalidValue。
    #[test]
    fn capture_rejects_invalid_anchor_body() {
        let mut world = empty_world();
        spawn_persist(&mut world, 5, 1, ChunkAnchor::default());

        let error = capture(&world, &registry(), &OpaqueComponents::default())
            .expect_err("BodyKey::INVALID 必须报错");
        assert_eq!(
            error.kind(),
            &SaveErrorKind::InvalidValue {
                what: "persist anchor body",
                value: 0,
            }
        );
    }

    /// prototype_type_id = 0 是保留值，捕获时拒绝。
    #[test]
    fn capture_rejects_zero_prototype_type_id() {
        let mut world = empty_world();
        spawn_persist(&mut world, 5, 0, anchor(1, chunk(0, 0, 0)));

        let error = capture(&world, &registry(), &OpaqueComponents::default())
            .expect_err("原型 ID 0 必须报错");
        assert_eq!(
            error.kind(),
            &SaveErrorKind::InvalidValue {
                what: "persist prototype_type_id",
                value: 0,
            }
        );
    }

    /// 两个实体用同一个 stable_entity_id（不同块也算重复）：报 Duplicate。
    #[test]
    fn capture_rejects_duplicate_stable_entity_id() {
        let mut world = empty_world();
        spawn_persist(&mut world, 9, 1, anchor(1, chunk(0, 0, 0)));
        spawn_persist(&mut world, 9, 1, anchor(1, chunk(1, 0, 0)));

        let error = capture(&world, &registry(), &OpaqueComponents::default())
            .expect_err("重复 stable_entity_id 必须报错");
        assert_eq!(
            error.kind(),
            &SaveErrorKind::Duplicate {
                what: "snapshot stable_entity_id",
                id: 9,
            }
        );
    }

    /// 不带 Persist 的普通实体不进快照；空世界返回空快照。
    #[test]
    fn capture_skips_entities_without_persist_and_handles_empty_world() {
        let mut world = empty_world();
        world.spawn((
            StableEntityId(1),
            anchor(1, chunk(0, 0, 0)),
            Health(1),
        ));
        world.spawn((StableEntityId(2), anchor(1, chunk(0, 0, 0))));
        spawn_persist(&mut world, 3, 5, anchor(1, chunk(0, 0, 0)));

        let snapshot = capture(&world, &registry(), &OpaqueComponents::default()).expect("捕获成功");
        assert_eq!(snapshot.entity_count(), 1);
        assert_eq!(ids(snapshot.for_chunk(body(1), chunk(0, 0, 0))), vec![3]);

        let empty = capture(&empty_world(), &registry(), &OpaqueComponents::default())
            .expect("空世界也能捕获");
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        assert_eq!(empty.entity_count(), 0);
        assert!(empty.bodies().is_empty());
        assert_eq!(empty.chunks().count(), 0);
    }

    /// apply_to 只动 entities：覆盖已有实体；没有该块时清空；content/proposals 不动。
    #[test]
    fn apply_to_overwrites_entities_and_clears_absent_chunk() {
        let mut world = empty_world();
        spawn_persist(&mut world, 11, 2, anchor(1, chunk(0, 0, 0)));
        spawn_persist(&mut world, 12, 2, anchor(1, chunk(0, 0, 0)));

        let snapshot = capture(&world, &registry(), &OpaqueComponents::default()).expect("捕获成功");

        let mut record = ChunkRecord {
            generator_version: 7,
            content: Some(vec![0xDE, 0xAD]),
            proposals: Vec::new(),
            entities: vec![EntityRecord {
                stable_entity_id: 999,
                stable_type_id: 1,
                payload: vec![0],
            }],
        };

        snapshot.apply_to(body(1), chunk(0, 0, 0), &mut record);
        assert_eq!(ids(&record.entities), vec![11, 12], "快照权威：旧实体被覆盖");
        assert_eq!(record.content, Some(vec![0xDE, 0xAD]), "content 不得被改");
        assert_eq!(record.generator_version, 7);

        // 没有这个块：entities 清空，其余字段仍不动。
        snapshot.apply_to(body(1), chunk(9, 9, 9), &mut record);
        assert!(record.entities.is_empty(), "快照里没有该块时必须清空");
        assert_eq!(record.content, Some(vec![0xDE, 0xAD]));
        assert_eq!(record.generator_version, 7);
    }

    /// Persist 自身进 payload：capture -> apply 到空实体后标记仍在。
    #[test]
    fn persist_component_round_trips() {
        let mut world = empty_world();
        let source = spawn_persist(&mut world, 1, 7, anchor(4, chunk(0, 0, 0)));
        world.entity_mut(source).insert(Health(5));

        let registry = registry_with_persist();
        let snapshot = capture(&world, &registry, &OpaqueComponents::default()).expect("捕获成功");
        let records = snapshot.for_chunk(body(4), chunk(0, 0, 0));
        assert_eq!(records.len(), 1);

        let mut target_world = World::new();
        let target = target_world.spawn_empty().id();
        let mut opaque = OpaqueComponents::default();
        let report = registry
            .apply_entity(&mut target_world, target, &records[0], &mut opaque)
            .expect("应用成功");

        assert_eq!(report.applied, 2, "Persist 与 Health 都要应用");
        assert!(report.opaque_type_ids.is_empty());
        assert!(opaque.is_empty());

        let persist = target_world
            .get::<Persist>(target)
            .expect("Persist 必须随存档恢复");
        assert_eq!(persist.prototype_type_id, 7);
        assert_eq!(target_world.get::<Health>(target), Some(&Health(5)));
    }

    /// 最小 App 装 PersistPlugin：引擎保留 ID 1 已登记，名字指向 Persist。
    #[test]
    fn persist_registered_by_plugin() {
        let mut app = App::new();
        app.add_plugins(PersistPlugin);

        let registry = app.world().resource::<PersistRegistry>();
        assert!(
            registry.contains(StableTypeId::new(1)),
            "PersistPlugin 必须注册引擎保留 ID 1"
        );
        assert_eq!(
            registry.name_of(StableTypeId::new(1)),
            Some(std::any::type_name::<Persist>())
        );
        assert_eq!(registry.len(), 1, "最小 App 里只有 PersistPlugin 这一项");
    }

    /// 保存 -> 载入 -> 再保存：两条 EntityRecord 逐字节相等（Persist 不丢）。
    #[test]
    fn resave_after_apply_matches() {
        let registry = registry_with_persist();

        let mut first_world = empty_world();
        let source = spawn_persist(&mut first_world, 21, 7, anchor(6, chunk(0, 0, 0)));
        first_world.entity_mut(source).insert(Health(11));
        let first = capture(&first_world, &registry, &OpaqueComponents::default()).expect("第一次捕获");
        let first_record = first.for_chunk(body(6), chunk(0, 0, 0))[0].clone();

        // 载入：空世界里先放好 StableEntityId / ChunkAnchor，再把组件表应用上去。
        let mut second_world = empty_world();
        let target = second_world
            .spawn((StableEntityId(21), anchor(6, chunk(0, 0, 0))))
            .id();
        let mut opaque = OpaqueComponents::default();
        registry
            .apply_entity(&mut second_world, target, &first_record, &mut opaque)
            .expect("应用成功");
        assert!(opaque.is_empty());

        let second =
            capture(&second_world, &registry, &OpaqueComponents::default()).expect("第二次捕获");
        assert_eq!(second.entity_count(), 1, "载入后的实体必须仍带 Persist");
        let second_record = &second.for_chunk(body(6), chunk(0, 0, 0))[0];

        assert_eq!(first_record, *second_record, "再保存必须逐字节相同");
        assert_eq!(first_record.payload, second_record.payload);
    }
}
