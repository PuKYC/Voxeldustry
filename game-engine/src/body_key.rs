//! `BodyKey`：体（岛 / 船 / 结构 / 自动机）的稳定键。
//!
//! 流式 / 驻留 / 世界区块三份文档的所有表键都是 `(BodyKey, ChunkKey)`。
//! `BodyKey` 必须同时是**存档键、表键、消息字段**，因此它必须：
//!
//! - **稳定**：不依赖 Bevy `Entity`。读档重建后 `Entity` 会变，稳定 ID 不会；
//! - **可存档**：实现 `serde`，且线格式就是一个 `u64`；
//! - **可排序**：`Copy + Ord`，`BTreeMap<(BodyKey, ChunkKey), _>` 的升序遍历
//!   是确定性（L3）的基础；
//! - **同一 body 永远同一 key**：它只是 `StableEntityId` 的 newtype。
//!
//! ## 为什么只认 `StableEntityId`
//!
//! 仓库里另有一个 `IslandId`（`game-core/src/world/island.rs`），但它是岛屿
//! 注册表自己的局部编号：目前既不指向实体，也没有和 `StableEntityId` 共用一个
//! 发号器。若把 `IslandId` 再做一套 `BodyKey` 来源，同一个岛就会有两个 key
//! （表键一套、存档一套），迟早对不上。因此这里只保留 `StableEntityId` 一套
//! 来源；`IslandId` 只是这个数值在岛屿注册表里的别名，转换见
//! `game-core/src/voxel/body.rs` 的 `From<IslandId> for BodyKey`。
//!
//! ## 硬约束
//!
//! - 不要用 Bevy `Entity` 构造 key；
//! - 不要用 `HashMap<BodyKey, _>` 决定遍历序；一律 `BTreeMap` / `BTreeSet`；
//! - 需要哈希时只用 `splitmix64`，不要用 `DefaultHasher` / `RandomState`。

use std::cmp::Ordering;

use bevy::prelude::{Entity, World};
use serde::{Deserialize, Serialize};

use crate::identity::{StableEntityId, StableEntityIndex};

/// 体（岛 / 船 / 结构 / 自动机）的稳定键。
///
/// 字段就是实体的 `StableEntityId`；构造后不可变。排序按底层 `u64` 数值升序，
/// 与 `StableEntityId` 的分配顺序一致（**不**按 Bevy `Entity` 的 index / generation）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BodyKey(pub StableEntityId);

impl BodyKey {
    /// 保留的无效 key（底层 `StableEntityId(0)`）。
    ///
    /// `0` 不参与发号（`StableIdAllocator` 从 1 开始），表键里出现即 bug；
    /// 只用于“尚未赋值”的占位，例如手写 `ChunkAnchor::default()`。
    pub const INVALID: Self = BodyKey(StableEntityId(0));

    /// 是否为有效 key。`BodyKey::INVALID` 返回 `false`，其余返回 `true`。
    #[must_use]
    pub const fn is_valid(self) -> bool {
        let id = self.0;
        id.0 != 0
    }

    /// 由稳定 ID 构造。
    #[must_use]
    pub const fn new(id: StableEntityId) -> Self {
        Self(id)
    }

    /// 取出底层稳定 ID。
    #[must_use]
    pub const fn stable_id(self) -> StableEntityId {
        self.0
    }

    /// 取出裸 `u64`（存档 / 消息线格式用）。
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        let id = self.0;
        id.0
    }

    /// 从已生成的 body 实体读取 `StableEntityId` 并转成 key。
    ///
    /// debug 构建下实体缺少 `StableEntityId` 会直接 panic（说明生成方漏挂）；
    /// release 构建返回 `None`，调用方应告警并跳过该实体（与驻留文档的约束一致）。
    #[track_caller]
    #[must_use]
    pub fn from_entity(world: &World, entity: Entity) -> Option<Self> {
        let id = world.get::<StableEntityId>(entity).copied();
        debug_assert!(
            id.is_some(),
            "body entity {entity:?} 缺少 StableEntityId；spawn 时必须用 spawn_stable 或手动插入"
        );
        id.map(Self)
    }

    /// 从 `StableEntityIndex` 读取（只拿到索引、没有 `World` 时用）。
    ///
    /// 与 [`Self::from_entity`] 一样：debug 缺失即 panic，release 返回 `None`。
    #[track_caller]
    #[must_use]
    pub fn from_index(index: &StableEntityIndex, entity: Entity) -> Option<Self> {
        let id = index.id(entity);
        debug_assert!(
            id.is_some(),
            "body entity {entity:?} 不在 StableEntityIndex 中；索引未建立或实体未挂 StableEntityId"
        );
        id.map(Self)
    }
}

impl From<StableEntityId> for BodyKey {
    #[inline]
    fn from(id: StableEntityId) -> Self {
        Self(id)
    }
}

impl From<BodyKey> for StableEntityId {
    #[inline]
    fn from(key: BodyKey) -> Self {
        key.0
    }
}

impl PartialOrd for BodyKey {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BodyKey {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        let this = self.0;
        let that = other.0;
        this.0.cmp(&that.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::{StableIdAllocator, StableIdWorldExt};

    fn test_world() -> World {
        let mut world = World::new();
        world.insert_resource(StableIdAllocator::default());
        world.insert_resource(StableEntityIndex::default());
        world
    }

    #[test]
    fn same_body_yields_same_key_twice() {
        let mut world = test_world();
        let (entity, id) = world.spawn_stable(());

        let first = BodyKey::from_entity(&world, entity).expect("刚 spawn 的实体必须有稳定 ID");
        let second = BodyKey::from_entity(&world, entity).expect("同一实体第二次取 key");
        assert_eq!(first, second);
        assert_eq!(first, BodyKey::new(id));
        assert_eq!(first.stable_id(), id);

        let index = world.resource::<StableEntityIndex>();
        assert_eq!(BodyKey::from_index(index, entity), Some(first));
    }

    #[test]
    fn different_bodies_have_different_keys() {
        let mut world = test_world();
        let (a, a_id) = world.spawn_stable(());
        let (b, b_id) = world.spawn_stable(());

        assert_ne!(a_id, b_id);
        let a_key = BodyKey::from_entity(&world, a).expect("a 有稳定 ID");
        let b_key = BodyKey::from_entity(&world, b).expect("b 有稳定 ID");
        assert_ne!(a_key, b_key);
    }

    #[test]
    fn allocator_recreation_with_same_id_keeps_key() {
        let mut world = test_world();
        let (entity, id) = world.spawn_stable(());
        let key = BodyKey::from_entity(&world, entity).expect("有稳定 ID");
        drop(world);

        // 模拟读档：新世界把发号器恢复到存档里的 next，再分配出同一个 id。
        let mut reloaded = test_world();
        reloaded.resource_mut::<StableIdAllocator>().restore(id.0);
        let (new_entity, new_id) = reloaded.spawn_stable(());

        assert_eq!(new_id, id);
        assert_eq!(BodyKey::from_entity(&reloaded, new_entity), Some(key));
    }

    #[test]
    fn invalid_key_is_not_valid() {
        assert_eq!(BodyKey::INVALID, BodyKey(StableEntityId(0)));
        assert!(!BodyKey::INVALID.is_valid());
        assert!(BodyKey::new(StableEntityId(1)).is_valid());
    }

    #[test]
    fn key_is_serde_transparent_u64() {
        let key = BodyKey(StableEntityId(42));
        let json = serde_json::to_string(&key).expect("BodyKey 必须可序列化");
        assert_eq!(json, "42");

        let back: BodyKey = serde_json::from_str(&json).expect("BodyKey 必须可反序列化");
        assert_eq!(back, key);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "缺少 StableEntityId")]
    fn missing_stable_id_panics_in_debug() {
        let mut world = test_world();
        let entity = world.spawn(()).id();
        let _ = BodyKey::from_entity(&world, entity);
    }

    #[test]
    fn ord_is_numeric_stable_id_order() {
        let mut keys = [
            BodyKey(StableEntityId(30)),
            BodyKey(StableEntityId(2)),
            BodyKey(StableEntityId(100)),
        ];
        keys.sort();
        assert_eq!(
            keys,
            [
                BodyKey(StableEntityId(2)),
                BodyKey(StableEntityId(30)),
                BodyKey(StableEntityId(100)),
            ]
        );
    }
}
