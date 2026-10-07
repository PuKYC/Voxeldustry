use bevy::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 稳定运行时实体 ID。
///
/// 注意：
/// - 不要直接使用 Bevy 的 `Entity` 作为长期业务 ID；
/// - `StableEntityId` 一旦分配，不应复用。
///
/// 派生 `serde` 是因为它是**跨边界**的实体标识（表现通道、存档、局域网
/// 复制都用它），必须可序列化。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Component, Serialize, Deserialize)]
pub struct StableEntityId(pub u64);

/// 全局稳定 ID 发号器。
///
/// 设计原则：
/// - 单调递增；
/// - 不重复；
/// - 不复用；
/// - 需要随存档持久化。
#[derive(Resource, Debug)]
pub struct StableIdAllocator {
    next: u64,
}

impl Default for StableIdAllocator {
    fn default() -> Self {
        Self {
            // 从 1 开始，0 可保留为无效 ID。
            next: 1,
        }
    }
}

impl StableIdAllocator {
    /// 分配一个新的稳定 ID。
    pub fn allocate(&mut self) -> StableEntityId {
        let id = StableEntityId(self.next);

        self.next = self
            .next
            .checked_add(1)
            .expect("StableEntityId allocator overflow");
        id
    }

    /// 从存档恢复 next_entity_id。
    pub fn restore(&mut self, next_entity_id: u64) {
        self.next = next_entity_id.max(1);
    }

    /// 当前下一个可分配 ID。
    pub fn next_id(&self) -> u64 {
        self.next
    }
}

/// 稳定 ID 与 Bevy Entity 的双向索引。
///
/// 用途：
/// - 通过 `StableEntityId` 查找 Bevy `Entity`；
/// - 通过 Bevy `Entity` 查找 `StableEntityId`；
/// - Godot 侧只保存 `StableEntityId`，再通过该索引访问 ECS 实体。
#[derive(Resource, Default, Debug)]
pub struct StableEntityIndex {
    by_id: HashMap<StableEntityId, Entity>,
    by_entity: HashMap<Entity, StableEntityId>,
}

impl StableEntityIndex {
    /// 插入映射。
    ///
    /// 注意：
    /// 这里只是维护索引，不负责回收或复用旧 ID。
    pub fn insert(&mut self, id: StableEntityId, entity: Entity) {
        if let Some(old_entity) = self.by_id.get(&id).copied() {
            if old_entity != entity {
                warn!(
                    "Duplicate StableEntityId {:?}: old entity {:?}, new entity {:?}",
                    id, old_entity, entity
                );
            }
        }

        if let Some(old_id) = self.by_entity.insert(entity, id) {
            if old_id != id {
                // 该 Entity 原本绑定了另一个 StableEntityId。
                // 这通常意味着运行时错误，需要排查。
                warn!(
                    "Entity {:?} changed StableEntityId from {:?} to {:?}",
                    entity, old_id, id
                );
                if self.by_id.get(&old_id) == Some(&entity) {
                    self.by_id.remove(&old_id);
                }
            }
        }

        self.by_id.insert(id, entity);
    }

    /// 根据 Entity 移除映射。
    ///
    /// 注意：
    /// 这里只是删除索引，不会把 StableEntityId 放回发号器。
    pub fn remove_by_entity(&mut self, entity: Entity) -> Option<StableEntityId> {
        let id = self.by_entity.remove(&entity)?;

        if self.by_id.get(&id) == Some(&entity) {
            self.by_id.remove(&id);
        }

        Some(id)
    }

    /// 通过 StableEntityId 查找 Bevy Entity。
    pub fn entity(&self, id: StableEntityId) -> Option<Entity> {
        self.by_id.get(&id).copied()
    }

    /// 通过 Bevy Entity 查找 StableEntityId。
    pub fn id(&self, entity: Entity) -> Option<StableEntityId> {
        self.by_entity.get(&entity).copied()
    }

    /// 是否包含某个稳定 ID。
    pub fn contains_id(&self, id: StableEntityId) -> bool {
        self.by_id.contains_key(&id)
    }

    /// 清空索引。
    ///
    /// 通常用于读档重建世界。
    pub fn clear(&mut self) {
        self.by_id.clear();
        self.by_entity.clear();
    }

    /// 当前索引数量。
    pub fn len(&self) -> usize {
        self.by_id.len()
    }
    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

/// 稳定 ID 插件。
///
/// 该插件负责：
/// - 注册 `StableIdAllocator`；
/// - 注册 `StableEntityIndex`；
/// - 自动维护新增 / 移除的 `StableEntityId` 索引。
pub struct StableIdPlugin;

impl Plugin for StableIdPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<StableIdAllocator>()
            .init_resource::<StableEntityIndex>()
            .add_systems(
                PostUpdate,
                (register_added_stable_ids, unregister_removed_stable_ids).chain(),
            );
    }
}

/// 自动登记新增的 `StableEntityId`。
///
/// 适用场景：
/// - 读档时手动插入旧 `StableEntityId`；
/// - 某些系统绕过 `spawn_stable` 手动插入 `StableEntityId`；
/// - 插件自身通过 `spawn_stable` 创建实体后，这里也会幂等地维护索引。
fn register_added_stable_ids(
    mut allocator: ResMut<StableIdAllocator>,
    mut index: ResMut<StableEntityIndex>,
    query: Query<(Entity, &StableEntityId), Added<StableEntityId>>,
) {
    for (entity, id) in &query {
        // 如果读档或外部系统插入了已有 ID，
        // 需要保证发号器不会重新分配到这些旧 ID。
        if allocator.next <= id.0 {
            allocator.next = id.0.saturating_add(1);
        }

        index.insert(*id, entity);
    }
}

/// 自动移除已经销毁或移除 `StableEntityId` 的实体索引。
///
/// 注意：
/// 这里不会回收 ID。
fn unregister_removed_stable_ids(
    mut index: ResMut<StableEntityIndex>,
    mut removed: RemovedComponents<StableEntityId>,
) {
    for entity in removed.read() {
        index.remove_by_entity(entity);
    }
}

/// `World` 扩展：用于创建稳定 ID 实体、恢复存档、重建索引。
pub trait StableIdWorldExt {
    /// 分配一个新的 StableEntityId。
    fn allocate_stable_id(&mut self) -> StableEntityId;

    /// 创建一个带有稳定 ID 的实体。
    ///
    /// 注意：
    /// 传入的 bundle 中不要再包含 `StableEntityId`。
    fn spawn_stable<B: Bundle>(&mut self, bundle: B) -> (Entity, StableEntityId);

    /// 从当前世界中重建索引。
    ///
    /// 适合读档后调用：
    /// 1. 恢复所有带 `StableEntityId` 的实体；
    /// 2. 调用该函数；
    /// 3. 发号器会自动被修正到所有已有 ID 之后。
    fn rebuild_stable_index(&mut self);

    /// 从存档恢复发号器。
    fn restore_stable_allocator(&mut self, next_entity_id: u64);
}

impl StableIdWorldExt for World {
    fn allocate_stable_id(&mut self) -> StableEntityId {
        self.resource_mut::<StableIdAllocator>().allocate()
    }

    fn spawn_stable<B: Bundle>(&mut self, bundle: B) -> (Entity, StableEntityId) {
        let id = self.allocate_stable_id();

        let entity = self.spawn(id).insert(bundle).id();
        self.resource_mut::<StableEntityIndex>().insert(id, entity);

        (entity, id)
    }

    fn rebuild_stable_index(&mut self) {
        let mut query = self.query::<(Entity, &StableEntityId)>();

        let entries: Vec<(Entity, StableEntityId)> =
            query.iter(self).map(|(entity, id)| (entity, *id)).collect();

        let mut next_id = self.resource::<StableIdAllocator>().next_id();

        {
            let mut index = self.resource_mut::<StableEntityIndex>();
            index.clear();

            for (entity, id) in entries {
                index.insert(id, entity);
                next_id = next_id.max(id.0.saturating_add(1));
            }
        }

        self.resource_mut::<StableIdAllocator>().restore(next_id);
    }

    fn restore_stable_allocator(&mut self, next_entity_id: u64) {
        self.resource_mut::<StableIdAllocator>()
            .restore(next_entity_id);
    }
}
