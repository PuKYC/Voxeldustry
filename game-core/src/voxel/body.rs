//! 世界体（Body）游戏语义。
//!
//! **一岛 = 一个组件**：具体体素数据在引擎的 VoxVolume 组件里，游戏语义挂在
//! 同一实体的 Body / 标签组件上 —— 组合，而不是一个胖 VoxelBody。
//!
//! 本模块只定义类型与标签；v1 不执行 AI、不做功能方块。

use bevy::prelude::*;
use game_engine::identity::{StableEntityId, StableEntityIndex, StableIdAllocator};
use game_engine::math::Vec3F;
use game_engine::voxel::Attachment;

use crate::static_data::voxel::BiomeId;
use crate::world::{Island, IslandId, IslandRegistry};

pub use game_engine::body_key::BodyKey;

/// 体素世界里的一个「体」：岛 / 飞船 / 结构 / 自动机。
///
/// 具体几何（VoxVolume.chunks）由引擎侧组件携带；Body 只描述游戏语义。
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Body {
    pub kind: BodyKind,
}

/// 体的种类。结构与飞船的挂接语义由 Attachment 统一表达。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyKind {
    /// 浮岛：是否锚定（anchored）与生物群系。
    Island { anchored: bool, biome: BiomeId },
    /// 自由体素飞船（可停靠）。
    Ship,
    /// 结构：锚定到世界 / 某个体 / 自由。
    Structure { attach: Attachment },
    /// 小型可建造自动机（v1 仅空壳）。
    Automaton,
}

impl Body {
    pub const fn island(anchored: bool, biome: BiomeId) -> Self {
        Self {
            kind: BodyKind::Island { anchored, biome },
        }
    }

    pub const fn ship() -> Self {
        Self {
            kind: BodyKind::Ship,
        }
    }

    pub const fn structure(attach: Attachment) -> Self {
        Self {
            kind: BodyKind::Structure { attach },
        }
    }

    pub const fn automaton() -> Self {
        Self {
            kind: BodyKind::Automaton,
        }
    }

    pub const fn kind(&self) -> BodyKind {
        self.kind
    }

    pub const fn is_island(&self) -> bool {
        matches!(self.kind, BodyKind::Island { .. })
    }

    pub const fn is_ship(&self) -> bool {
        matches!(self.kind, BodyKind::Ship)
    }

    pub const fn is_structure(&self) -> bool {
        matches!(self.kind, BodyKind::Structure { .. })
    }

    pub const fn is_automaton(&self) -> bool {
        matches!(self.kind, BodyKind::Automaton)
    }
}

/// `IslandId` -> `BodyKey`：岛屿注册表的编号就是岛屿实体的 `StableEntityId`。
///
/// 这是**唯一**的岛到 key 的桥：`IslandId` 没有独立发号器，生成岛屿实体时
/// 请走 [`spawn_island_entity`]；它把返回的 `StableEntityId` 同时用作 `IslandId`，
/// 否则同一个岛会在注册表里有一个编号、在实体上有一个稳定 ID，两套 key 对不上。
impl From<IslandId> for BodyKey {
    fn from(id: IslandId) -> Self {
        BodyKey(StableEntityId(id.0))
    }
}

/// 生成一个岛屿实体并登记到 [`IslandRegistry`]。
///
/// **这是岛实体的唯一生成入口**：其它地方不要手搓
/// `spawn_stable((Body::island(..), IslandTag))`，否则 `IslandId` 与实体
/// `StableEntityId` 会脱钩，`BodyKey` 的两套来源对不上。
///
/// 要求 `IslandRegistry` 已作为资源插入（`WorldPlugin` 会注册）；缺失时 panic，
/// 不静默跳过——静默跳过会让岛从注册表消失。
#[track_caller]
pub fn spawn_island_entity(
    world: &mut World,
    drift: Vec3F,
    anchored: bool,
    biome: BiomeId,
) -> (Entity, IslandId) {
    assert!(
        world.get_resource::<IslandRegistry>().is_some(),
        "spawn_island_entity：缺少 IslandRegistry 资源；请先插入 IslandRegistry 资源"
    );

    let entity = world.spawn((Body::island(anchored, biome), IslandTag)).id();
    let stable_id = attach_island_entity(world, entity, drift, anchored, biome);

    (entity, IslandId(stable_id.0))
}

/// 把一个**已经存在**的实体升级为正式 island body：分配 / 复用 `StableEntityId`
/// 并在 [`IslandRegistry`] 里登记 `IslandId(id.0)`。
///
/// 与 [`spawn_island_entity`] 的关系：后者 = `spawn` + 本函数。需要给由
/// `Commands` 生成、或由引擎 / 存档恢复出来的实体补上「稳定 ID + 注册表条目」
/// 时（例如地形根实体），直接调本函数。
///
/// **幂等**：实体已有 `StableEntityId` 时直接返回它，不再次分配；注册表里
/// 缺条目才补插，已有条目不覆盖（重复调用不会把 anchored / drift / biome 改掉，
/// 也不会重复插入）。`StableEntityIndex` 的双向映射在两条路径上都会保证存在。
///
/// 要求 `IslandRegistry` 已作为资源插入（`WorldPlugin` 会注册）；缺失时 panic，
/// 与 [`spawn_island_entity`] 同样的诊断信息——静默跳过会让岛从注册表消失。
#[track_caller]
pub fn attach_island_entity(
    world: &mut World,
    entity: Entity,
    drift: Vec3F,
    anchored: bool,
    biome: BiomeId,
) -> StableEntityId {
    assert!(
        world.get_resource::<IslandRegistry>().is_some(),
        "attach_island_entity：缺少 IslandRegistry 资源；请先插入 IslandRegistry 资源"
    );

    // ── 幂等路径：已有稳定 ID，只补注册表 / 索引，绝不二次分配 ──
    if let Some(existing) = world.get::<StableEntityId>(entity).copied() {
        let island_id = IslandId(existing.0);
        {
            let mut registry = world.resource_mut::<IslandRegistry>();
            if !registry.contains(island_id) {
                registry.insert(island_id, Island::new(drift, anchored, biome));
            }
        }
        // 索引可能缺失（例如读档后只插了组件、没走 spawn_stable），补齐是幂等的。
        if world.resource::<StableEntityIndex>().id(entity) != Some(existing) {
            world
                .resource_mut::<StableEntityIndex>()
                .insert(existing, entity);
        }
        return existing;
    }

    assert!(
        world.get_resource::<StableIdAllocator>().is_some(),
        "attach_island_entity：缺少 StableIdAllocator 资源；请先插入 StableIdAllocator 资源"
    );
    assert!(
        world.get_resource::<StableEntityIndex>().is_some(),
        "attach_island_entity：缺少 StableEntityIndex 资源；请先插入 StableEntityIndex 资源"
    );

    // ── 分配路径：与 spawn_stable 相同的三步（组件 / 索引 / 注册表）──
    let stable_id = world.resource_mut::<StableIdAllocator>().allocate();
    world.entity_mut(entity).insert(stable_id);
    world
        .resource_mut::<StableEntityIndex>()
        .insert(stable_id, entity);
    world
        .resource_mut::<IslandRegistry>()
        .insert(IslandId(stable_id.0), Island::new(drift, anchored, biome));

    stable_id
}

// ── 标签组件（同一实体上的第二、第三……个语义标记）──────────────────────────
//
// 标签只用于查询筛选（岛屿系统 / 飞船系统 / 结构系统 / 自动机系统各查各的），
// 不复制 BodyKind 的信息，避免两份真相源。

#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IslandTag;

#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShipTag;

#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StructureTag;

#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AutomatonTag;

#[cfg(test)]
mod tests {
    use super::*;
    use game_engine::identity::{StableIdPlugin, StableIdWorldExt};
    use game_engine::math::FixedPoint;

    #[test]
    fn body_kinds_match_their_tags() {
        assert!(Body::island(true, BiomeId(0)).is_island());
        assert!(Body::ship().is_ship());
        assert!(Body::structure(Attachment::World).is_structure());
        assert!(Body::automaton().is_automaton());
    }

    #[test]
    fn structure_attachment_is_preserved() {
        let body = Body::structure(Attachment::Free);
        match body.kind {
            BodyKind::Structure { attach } => assert_eq!(attach, Attachment::Free),
            other => panic!("期望 Structure，得到 {other:?}"),
        }
    }

    #[test]
    fn island_id_maps_to_stable_body_key() {
        let key = BodyKey::from(IslandId(7));
        assert_eq!(key, BodyKey::new(StableEntityId(7)));
        assert_eq!(key.as_u64(), 7);
        assert_eq!(BodyKey::from(IslandId(7)), key);
        assert_ne!(BodyKey::from(IslandId(7)), BodyKey::from(IslandId(8)));
    }

    #[test]
    fn spawn_island_entity_registers_and_is_the_single_entry() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StableIdPlugin);
        app.insert_resource(IslandRegistry::new());
        app.finish();

        let world = app.world_mut();
        let drift = Vec3F::new(
            FixedPoint::from_num(1),
            FixedPoint::from_num(2),
            FixedPoint::from_num(-3),
        );
        let (entity, island_id) = spawn_island_entity(world, drift, false, BiomeId(2));

        {
            let registry = world.resource::<IslandRegistry>();
            assert_eq!(registry.len(), 1);
            let island = registry.get(island_id).expect("刚生成的岛必须在注册表里");
            assert_eq!(island.drift, drift);
            assert!(!island.anchored);
            assert_eq!(island.biome, BiomeId(2));
        }
        assert_eq!(
            BodyKey::from(island_id),
            BodyKey::from_entity(world, entity).expect("岛实体必须有稳定 ID")
        );
        // 只分配一个稳定 ID：发号器 1 -> 2，索引恰好一条。
        assert_eq!(world.resource::<StableIdAllocator>().next_id(), 2);
        assert_eq!(world.resource::<StableEntityIndex>().len(), 1);

        let drift2 = Vec3F::new(
            FixedPoint::from_num(-4),
            FixedPoint::from_num(0),
            FixedPoint::from_num(5),
        );
        let (entity2, island_id2) = spawn_island_entity(world, drift2, true, BiomeId(1));

        assert_ne!(island_id, island_id2);
        assert_ne!(
            BodyKey::from_entity(world, entity).expect("岛实体必须有稳定 ID"),
            BodyKey::from_entity(world, entity2).expect("岛实体必须有稳定 ID")
        );
        // 第二次调用再分配一个：发号器 2 -> 3，索引两条，ID 各不相同。
        assert_eq!(world.resource::<StableIdAllocator>().next_id(), 3);
        assert_eq!(world.resource::<StableEntityIndex>().len(), 2);
        let registry = world.resource::<IslandRegistry>();
        assert_eq!(registry.len(), 2);
        let island2 = registry.get(island_id2).expect("第二个岛必须在注册表里");
        assert_eq!(island2.drift, drift2);
        assert!(island2.anchored);
        assert_eq!(island2.biome, BiomeId(1));
    }

    #[test]
    #[should_panic(expected = "缺少 IslandRegistry 资源")]
    fn spawn_island_entity_requires_registry_resource() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StableIdPlugin);
        app.finish();

        let _ = spawn_island_entity(app.world_mut(), Vec3F::ZERO, true, BiomeId(0));
    }

    /// (b) attach_island_entity 幂等：同一实体两次调用 -> 同一 StableEntityId、
    /// 注册表一条、发号器不动、索引双向可查，且已有条目的语义不被覆盖。
    #[test]
    fn attach_island_entity_is_idempotent() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StableIdPlugin);
        app.insert_resource(IslandRegistry::new());
        app.finish();

        let world = app.world_mut();
        let entity = world.spawn(()).id();
        let drift = Vec3F::new(
            FixedPoint::from_num(0),
            FixedPoint::from_num(1),
            FixedPoint::from_num(2),
        );

        let first = attach_island_entity(world, entity, drift, true, BiomeId(3));
        assert_eq!(first, StableEntityId(1), "首次 attach 必须分配下一个稳定 ID");
        assert_eq!(world.get::<StableEntityId>(entity), Some(&first));
        assert_eq!(world.resource::<StableIdAllocator>().next_id(), 2);

        // 同一实体再来一次（故意传不同的语义参数）：仍返回同一 ID，不二次分配。
        let second = attach_island_entity(world, entity, Vec3F::ZERO, false, BiomeId(0));
        assert_eq!(second, first, "重复 attach 必须返回同一个 StableEntityId");
        assert_eq!(
            world.resource::<StableIdAllocator>().next_id(),
            2,
            "幂等路径不得从发号器取号"
        );
        {
            let registry = world.resource::<IslandRegistry>();
            assert_eq!(registry.len(), 1, "同一实体只能有一条注册表记录");
            assert_eq!(
                registry.ids().collect::<Vec<_>>(),
                vec![IslandId(first.0)],
                "注册表键必须是实体稳定 ID"
            );
            let island = registry.get(IslandId(first.0)).expect("注册表必须有对应条目");
            // 已有条目不覆盖：重复 attach 不把锚定岛的 drift / anchored / biome 改掉。
            assert_eq!(island.drift, drift);
            assert!(island.anchored);
            assert_eq!(island.biome, BiomeId(3));
        }
        // StableEntityIndex 双向可查。
        {
            let index = world.resource::<StableEntityIndex>();
            assert_eq!(index.id(entity), Some(first));
            assert_eq!(index.entity(first), Some(entity));
            assert_eq!(index.len(), 1);
        }
    }

    /// 已有 StableEntityId 但注册表条目丢失（模拟读档只恢复组件）：
    /// attach 必须补插条目、复用原 ID、不动发号器。
    #[test]
    fn attach_island_entity_repairs_missing_registry_entry_without_allocating() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StableIdPlugin);
        app.insert_resource(IslandRegistry::new());
        app.finish();

        let world = app.world_mut();
        let entity = world.spawn(()).id();
        let id = attach_island_entity(world, entity, Vec3F::ZERO, true, BiomeId(1));

        world.resource_mut::<IslandRegistry>().remove(IslandId(id.0));
        assert!(world.resource::<IslandRegistry>().is_empty());

        let again = attach_island_entity(world, entity, Vec3F::ZERO, true, BiomeId(1));
        assert_eq!(again, id, "已有 StableEntityId 时不得重新分配");
        assert_eq!(world.resource::<StableIdAllocator>().next_id(), 2);
        assert!(world.resource::<IslandRegistry>().contains(IslandId(id.0)));
        assert_eq!(world.resource::<IslandRegistry>().len(), 1);
        assert_eq!(world.resource::<StableEntityIndex>().id(entity), Some(id));
    }

    /// 与 spawn_island_entity 相同的可诊断信息：缺注册表资源直接 panic。
    #[test]
    #[should_panic(expected = "缺少 IslandRegistry 资源")]
    fn attach_island_entity_requires_registry_resource() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StableIdPlugin);
        app.finish();

        let entity = app.world_mut().spawn(()).id();
        let _ = attach_island_entity(app.world_mut(), entity, Vec3F::ZERO, true, BiomeId(0));
    }

    #[test]
    fn every_spawned_body_carries_a_stable_id_and_key() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StableIdPlugin);
        app.insert_resource(IslandRegistry::new());
        app.finish();

        let world = app.world_mut();
        let (island, island_id) = spawn_island_entity(world, Vec3F::ZERO, true, BiomeId(0));
        let (ship, ship_id) = world.spawn_stable((Body::ship(), ShipTag));
        let (structure, structure_id) =
            world.spawn_stable((Body::structure(Attachment::World), StructureTag));
        let (automaton, automaton_id) = world.spawn_stable((Body::automaton(), AutomatonTag));

        let entities = [island, ship, structure, automaton];
        let ids = [
            BodyKey::from(island_id).stable_id(),
            ship_id,
            structure_id,
            automaton_id,
        ];
        let mut keys = Vec::with_capacity(entities.len());
        for (entity, id) in entities.into_iter().zip(ids) {
            assert_eq!(world.get::<StableEntityId>(entity), Some(&id));
            let key = BodyKey::from_entity(world, entity).expect("所有 body 都必须有稳定 ID");
            assert_eq!(key.stable_id(), id);
            // 同一 body 两次取 key 相同。
            assert_eq!(BodyKey::from_entity(world, entity), Some(key));
            keys.push(key);
        }

        // 不同 body 不同 key。
        let mut sorted = keys.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), keys.len());
    }
}
