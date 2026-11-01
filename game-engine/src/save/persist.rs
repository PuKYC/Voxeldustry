//! 实体组件负载的 postcard 编解码与稳定类型注册表（《存档格式规格》§4）。
//!
//! 分工：`codec` 只认外层框（`u16 component_count | u16 reserved | ComponentRecord[]`），
//! 组件负载在它眼里是不透明字节；本模块负责把**组件负载本身**用 `codec_id = 1`
//! （postcard 1.1.3 默认配置）编解码，并把「实体 <-> EntityRecord」这条边接起来。
//!
//! # 为什么用注册表而不是反射
//!
//! 判定与产出字节都必须只依赖固定顺序：注册表是 `BTreeMap<u32, _>`，按
//! `StableTypeId` 升序遍历，组件顺序由**类型 ID**决定，与注册顺序、`World`
//! 的 archetype 布局、任何哈希种子都无关（§8）。bevy 的反射能省掉手工注册，但
//! 反射遍历顺序不是契约，且组件名 -> 类型 ID 的映射要另立一张表，反而更多不确定性。
//!
//! # 未知类型（§4.2）
//!
//! 未注册的组件不解析、不丢：原字节进 [`OpaqueComponents`]，写回时逐字节原样
//! 输出。未注册的**原型**（整条 `EntityRecord`）由上层建档，不在本模块，因为那需要
//! 区块归属，属于 spawn 阶段的事。
//!
//! # 失败语义（§4.2）
//!
//! - 已注册组件、payload 解不开 -> **整条实体不应用**（先全部解码暂存，全部成功才
//!   逐条 insert），返回错误由上层判整份 region 损坏；
//! - 未注册组件 ID -> 进 `OpaqueComponents`，不报错，只在返回里列出（上层告警一次）；
//! - 注册表里类型 ID 为 0 或重复 -> 注册时即拒绝，不静默覆盖。

use std::collections::BTreeMap;
use std::fmt;

use bevy::prelude::*;
use serde::{de::DeserializeOwned, Serialize};

use crate::identity::StableEntityId;
use crate::ids::{partition_of, IdPartition};
use crate::save::{
    decode_component_table, encode_component_table, ComponentRecord, EntityRecord, SaveError,
    SaveErrorKind,
};

/// 组件的稳定类型 ID（`u32`，分区复用 `crate::ids`）。
///
/// 0 保留给「无」，任何需要写盘的路径都拒绝 0（§4.1）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StableTypeId(u32);

impl StableTypeId {
    /// 保留值 0：不是任何组件。
    pub const INVALID: Self = Self(0);

    /// 由裸 `u32` 构造（调用方保证非 0，或用 [`Self::is_valid`] 校验）。
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// 线格式的裸值。
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// 是否非 0。
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.0 != 0
    }

    /// 所属分区（0 / 核心 / mod / 哈希），见 `crate::ids`。
    #[must_use]
    pub const fn partition(self) -> IdPartition {
        partition_of(self.0)
    }
}

impl fmt::Display for StableTypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 可存档组件：组件 + 可序列化 + 稳定的类型 ID。
///
/// 类型 ID 是**存档契约**，发布后只增不改；改 ID 等于换类型（旧档里的字节会被
/// 当成未注册组件原样保留，不会串到新类型上）。
pub trait PersistComponent: Component + Serialize + DeserializeOwned {
    /// 存档里的稳定类型 ID；必须非 0，且全注册表唯一。
    const STABLE_TYPE_ID: StableTypeId;
}

/// 一条注册项：编码函数 + 解码后延迟插入的闭包工厂。
///
/// 用函数指针而不是 `Box<dyn ...>`，注册表可以 `Send + Sync` 且无生命周期参数。
struct PersistEntry {
    /// `std::any::type_name`，只用于日志与错误信息。
    name: &'static str,
    /// 实体上有这个组件就编出负载，没有就返回 `Ok(None)`。
    encode: fn(&World, Entity) -> Result<Option<Vec<u8>>, SaveError>,
    /// 解码负载，返回「以后往实体上插」的闭包；解码失败不产生闭包。
    stage: fn(&[u8]) -> Result<StagedComponent, SaveError>,
}

/// 已解码、尚未插入的组件（`Box` 擦掉具体类型）。
type StagedComponent = Box<dyn FnOnce(&mut World, Entity) + Send + 'static>;

impl fmt::Debug for PersistEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PersistEntry").field("name", &self.name).finish()
    }
}

fn encode_of<T: PersistComponent>(
    world: &World,
    entity: Entity,
) -> Result<Option<Vec<u8>>, SaveError> {
    let Some(value) = world.get::<T>(entity) else {
        return Ok(None);
    };
    let bytes = postcard::to_allocvec(value).map_err(|error| {
        payload_error(
            format!("component[{}].encode", T::STABLE_TYPE_ID),
            "postcard encode",
            error,
        )
    })?;
    Ok(Some(bytes))
}

fn stage_of<T: PersistComponent>(bytes: &[u8]) -> Result<StagedComponent, SaveError> {
    let (value, rest) = postcard::take_from_bytes::<T>(bytes).map_err(|error| {
        payload_error(
            format!("component[{}].decode", T::STABLE_TYPE_ID),
            "postcard decode",
            error,
        )
    })?;
    if !rest.is_empty() {
        return Err(SaveError::new(
            format!("component[{}].payload", T::STABLE_TYPE_ID),
            0,
            SaveErrorKind::TrailingBytes { extra: rest.len() },
        ));
    }
    Ok(Box::new(move |world: &mut World, entity: Entity| {
        world.entity_mut(entity).insert(value);
    }))
}

fn payload_error(path: String, what: &'static str, error: postcard::Error) -> SaveError {
    SaveError::new(
        path,
        0,
        SaveErrorKind::Payload {
            what,
            detail: error.to_string(),
        },
    )
}

/// 组件类型注册表（`Resource`）。
///
/// 启动时注册，运行期只读；`BTreeMap` 保证遍历顺序 = 类型 ID 升序（§8）。
#[derive(Debug, Default, Resource)]
pub struct PersistRegistry {
    entries: BTreeMap<u32, PersistEntry>,
}

impl PersistRegistry {
    /// 空注册表。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个组件类型。
    ///
    /// # 错误
    ///
    /// 类型 ID 为 0，或同一 ID 已注册（不同或相同类型都算，重复注册不是幂等操作）。
    pub fn register<T: PersistComponent>(&mut self) -> Result<(), SaveError> {
        let id = T::STABLE_TYPE_ID;
        let path = format!("registry.{}", std::any::type_name::<T>());
        if !id.is_valid() {
            return Err(SaveError::new(
                path,
                0,
                SaveErrorKind::InvalidValue {
                    what: "component stable_type_id",
                    value: i64::from(id.get()),
                },
            ));
        }
        if let Some(existing) = self.entries.get(&id.get()) {
            return Err(SaveError::new(
                format!("{path}.stable_type_id"),
                0,
                SaveErrorKind::Duplicate {
                    what: existing.name,
                    id: u64::from(id.get()),
                },
            ));
        }
        self.entries.insert(
            id.get(),
            PersistEntry {
                name: std::any::type_name::<T>(),
                encode: encode_of::<T>,
                stage: stage_of::<T>,
            },
        );
        Ok(())
    }

    /// 是否已注册该类型。
    #[must_use]
    pub fn contains(&self, id: StableTypeId) -> bool {
        self.entries.contains_key(&id.get())
    }

    /// 已注册类型数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 已注册类型 ID，升序。
    pub fn registered_ids(&self) -> impl Iterator<Item = StableTypeId> + '_ {
        self.entries.keys().copied().map(StableTypeId::new)
    }

    /// 类型名（`std::any::type_name`），只用于日志。
    #[must_use]
    pub fn name_of(&self, id: StableTypeId) -> Option<&'static str> {
        self.entries.get(&id.get()).map(|entry| entry.name)
    }

    /// 把实体的组件编成一条 `EntityRecord`（不含 `OpaqueComponents`）。
    ///
    /// `stable_type_id` 是**实体原型** ID（与组件 ID 各自独立命名空间），由调用方给出。
    ///
    /// # 错误
    ///
    /// 实体没有 `StableEntityId`、原型 ID 为 0、组件编码失败、组件数超 `u16`。
    pub fn encode_entity(
        &self,
        world: &World,
        entity: Entity,
        stable_type_id: u32,
    ) -> Result<EntityRecord, SaveError> {
        self.encode_entity_inner(world, entity, stable_type_id, None)
    }

    /// 同 [`Self::encode_entity`]，并把该实体名下未注册组件的原字节一起写回（§4.2）。
    pub fn encode_entity_with_opaque(
        &self,
        world: &World,
        entity: Entity,
        stable_type_id: u32,
        opaque: &OpaqueComponents,
    ) -> Result<EntityRecord, SaveError> {
        self.encode_entity_inner(world, entity, stable_type_id, Some(opaque))
    }

    fn encode_entity_inner(
        &self,
        world: &World,
        entity: Entity,
        stable_type_id: u32,
        opaque: Option<&OpaqueComponents>,
    ) -> Result<EntityRecord, SaveError> {
        let stable_entity_id = world.get::<StableEntityId>(entity).map(|id| id.0);
        let Some(stable_entity_id) = stable_entity_id else {
            return Err(SaveError::new(
                "entity.stable_entity_id",
                0,
                SaveErrorKind::Missing {
                    what: "StableEntityId component",
                    id: 0,
                },
            ));
        };
        if stable_type_id == 0 {
            return Err(SaveError::new(
                "entity.stable_type_id",
                0,
                SaveErrorKind::InvalidValue {
                    what: "entity stable_type_id",
                    value: 0,
                },
            ));
        }

        let mut components: Vec<ComponentRecord> = Vec::with_capacity(self.entries.len());
        for (type_id, entry) in &self.entries {
            if let Some(payload) = (entry.encode)(world, entity)? {
                components.push(ComponentRecord {
                    stable_type_id: *type_id,
                    payload,
                });
            } else if let Some(bytes) = opaque.and_then(|o| o.get(stable_entity_id, *type_id)) {
                // 类型现在注册了、但实体上还没有这个组件：把之前原样保存的字节继续
                // 原样写回，不丢数据（spec §4.2 的“未注册组件”升级路径）。
                components.push(ComponentRecord {
                    stable_type_id: *type_id,
                    payload: bytes.to_vec(),
                });
            }
        }

        if let Some(opaque) = opaque {
            for (entity_id, type_id, bytes) in opaque.rows() {
                if entity_id != stable_entity_id {
                    continue;
                }
                let registered = self.entries.contains_key(&type_id);
                let already_written = components
                    .iter()
                    .any(|component| component.stable_type_id == type_id);
                if registered || already_written {
                    continue;
                }
                components.push(ComponentRecord {
                    stable_type_id: type_id,
                    payload: bytes.to_vec(),
                });
            }
        }

        let payload = encode_component_table(&components).map_err(|error| {
            SaveError::new(
                format!("entity[{stable_entity_id}].components"),
                error.offset(),
                error.kind().clone(),
            )
        })?;

        Ok(EntityRecord {
            stable_entity_id,
            stable_type_id,
            payload,
        })
    }

    /// 把一条 `EntityRecord` 应用到已存在的实体上（spawn 由上层负责）。
    ///
    /// 先全部解码暂存、全部成功才插入，所以失败时实体**一个组件都不会变**（§4.2）。
    /// 未注册组件进 `opaque`，并在返回里列出，供上层告警一次。
    ///
    /// # 错误
    ///
    /// payload 外层框损坏（`component_count` / 升序 / 长度）、已注册组件解不开、
    /// 负载有尾随字节。
    pub fn apply_entity(
        &self,
        world: &mut World,
        entity: Entity,
        record: &EntityRecord,
        opaque: &mut OpaqueComponents,
    ) -> Result<ApplyReport, SaveError> {
        let components = decode_component_table(&record.payload).map_err(|error| {
            SaveError::new(
                format!("entity[{}].payload", record.stable_entity_id),
                error.offset(),
                error.kind().clone(),
            )
        })?;

        let mut staged: Vec<(StableTypeId, StagedComponent)> = Vec::with_capacity(components.len());
        let mut unknown: Vec<(u32, Vec<u8>)> = Vec::new();

        for component in &components {
            let type_id = StableTypeId::new(component.stable_type_id);
            match self.entries.get(&component.stable_type_id) {
                Some(entry) => {
                    let insert = (entry.stage)(&component.payload).map_err(|error| {
                        SaveError::new(
                            format!(
                                "entity[{}].component[{}]",
                                record.stable_entity_id, type_id
                            ),
                            0,
                            error.kind().clone(),
                        )
                    })?;
                    staged.push((type_id, insert));
                }
                None => unknown.push((component.stable_type_id, component.payload.clone())),
            }
        }

        for (_, insert) in staged {
            insert(world, entity);
        }
        for (type_id, payload) in &unknown {
            opaque.insert(record.stable_entity_id, *type_id, payload.clone());
        }

        Ok(ApplyReport {
            applied: components.len() - unknown.len(),
            opaque_type_ids: unknown.into_iter().map(|(id, _)| id).collect(),
        })
    }
}

/// [`PersistRegistry::apply_entity`] 的结果：应用了几个、哪些进了 opaque。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApplyReport {
    /// 成功插入的组件数。
    pub applied: usize,
    /// 未注册、原样进 `OpaqueComponents` 的类型 ID（升序）。
    pub opaque_type_ids: Vec<u32>,
}

/// 未注册组件的原字节（实体 -> 类型 -> 负载），§4.2。
///
/// 键是 `(stable_entity_id, stable_type_id)`，`BTreeMap` 保证写回顺序确定。
#[derive(Clone, Debug, Default, PartialEq, Eq, Resource)]
pub struct OpaqueComponents {
    rows: BTreeMap<(u64, u32), Vec<u8>>,
}

impl OpaqueComponents {
    /// 记一条未注册组件的负载（同键覆盖：以最新一次读到的为准）。
    pub fn insert(&mut self, stable_entity_id: u64, stable_type_id: u32, payload: Vec<u8>) {
        self.rows
            .insert((stable_entity_id, stable_type_id), payload);
    }

    /// 取某实体某类型的原字节。
    #[must_use]
    pub fn get(&self, stable_entity_id: u64, stable_type_id: u32) -> Option<&[u8]> {
        self.rows
            .get(&(stable_entity_id, stable_type_id))
            .map(Vec::as_slice)
    }

    /// 丢弃一个实体的全部未注册组件（实体被玩法销毁时调用，§4.2）。
    ///
    /// 返回丢弃条数。
    pub fn remove_entity(&mut self, stable_entity_id: u64) -> usize {
        let keys: Vec<(u64, u32)> = self
            .rows
            .range((stable_entity_id, 0)..=(stable_entity_id, u32::MAX))
            .map(|(key, _)| *key)
            .collect();
        for key in &keys {
            self.rows.remove(key);
        }
        keys.len()
    }

    /// 条数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// 是否没有未注册组件。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// 全部行，按 `(entity, type)` 升序。
    pub fn rows(&self) -> impl Iterator<Item = (u64, u32, &[u8])> + '_ {
        self.rows
            .iter()
            .map(|((entity, type_id), bytes)| (*entity, *type_id, bytes.as_slice()))
    }
}

/// 只注册两个资源；组件类型由游戏侧启动时 `registry.register::<T>()`。
#[derive(Default)]
pub struct PersistPlugin;

impl Plugin for PersistPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PersistRegistry>()
            .init_resource::<OpaqueComponents>();
    }
}

// ---------------------------------------------------------------------------
// 测试（§4.1–§4.3、§8）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! 只用真实 `World`（`World::new` / `spawn` / `entity_mut().insert`），不 mock。
    //!
    //! 覆盖：往返、注册顺序无关、重复 / 0 类型 ID 拒绝、未注册组件原样保留与写回、
    //! 负载损坏时实体不变、尾随字节、opaque 行清理、空组件实体、缺
    //! `StableEntityId`、原型 ID 为 0、分区与注册表访问器。

    use super::*;
    use serde::Deserialize;

    // -- 测试用组件类型（ID 刻意分散在核心 / mod 区，且互不相同）--------------

    /// 已注册组件 A（ID 11，核心区）。
    #[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct Health {
        current: i32,
        max: i32,
    }

    impl PersistComponent for Health {
        const STABLE_TYPE_ID: StableTypeId = StableTypeId::new(11);
    }

    /// 已注册组件 B（ID 22）。
    #[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct Position {
        x: i32,
        y: i32,
        z: i32,
    }

    impl PersistComponent for Position {
        const STABLE_TYPE_ID: StableTypeId = StableTypeId::new(22);
    }

    /// 已注册但**不参与** round_trip 的组件（ID 33）。
    #[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct Score {
        value: u32,
    }

    impl PersistComponent for Score {
        const STABLE_TYPE_ID: StableTypeId = StableTypeId::new(33);
    }

    /// 损坏负载用的组件：只有一个 `u32`，所以 5 字节全 0xFF 的 varint 必然溢出。
    #[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct Counter {
        value: u32,
    }

    impl PersistComponent for Counter {
        const STABLE_TYPE_ID: StableTypeId = StableTypeId::new(44);
    }

    /// 与 [`DupB`] 抢同一个类型 ID（55）的组件。
    #[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct DupA {
        value: u32,
    }

    impl PersistComponent for DupA {
        const STABLE_TYPE_ID: StableTypeId = StableTypeId::new(55);
    }

    /// 与 [`DupA`] 抢同一个类型 ID（55）的组件。
    #[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct DupB {
        value: u32,
    }

    impl PersistComponent for DupB {
        const STABLE_TYPE_ID: StableTypeId = StableTypeId::new(55);
    }

    /// 类型 ID 为 0（[`StableTypeId::INVALID`]）的组件。
    #[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct ZeroId {
        value: u32,
    }

    impl PersistComponent for ZeroId {
        const STABLE_TYPE_ID: StableTypeId = StableTypeId::INVALID;
    }

    /// `u32` 的 postcard varint 最多 5 字节，第 5 字节只允许低 4 位；全 0xFF 必然溢出。
    const OVERFLOW_VARINT: [u8; 5] = [0xFF; 5];

    /// 注册两个已注册组件的空注册表（Health = 11、Position = 22）。
    fn registry_with_two() -> PersistRegistry {
        let mut registry = PersistRegistry::new();
        registry.register::<Health>().expect("register Health");
        registry.register::<Position>().expect("register Position");
        registry
    }

    /// 用组件表造一条 `EntityRecord`（外层框交给 `encode_component_table`）。
    fn record_of(
        stable_entity_id: u64,
        stable_type_id: u32,
        components: &[ComponentRecord],
    ) -> EntityRecord {
        EntityRecord {
            stable_entity_id,
            stable_type_id,
            payload: encode_component_table(components).expect("encode component table"),
        }
    }

    /// T1：编码 -> 应用到新实体 -> 逐字段相等，`applied == 2`。
    #[test]
    fn round_trip() {
        let registry = registry_with_two();
        assert_eq!(registry.len(), 2);

        let mut world = World::new();
        let source = world
            .spawn((
                StableEntityId(1),
                Health {
                    current: 7,
                    max: 10,
                },
                Position { x: -3, y: 4, z: 5 },
                // 未注册组件不参与编码。
                Score { value: 99 },
            ))
            .id();

        let record = registry.encode_entity(&world, source, 3).expect("encode");
        assert_eq!(record.stable_entity_id, 1);
        assert_eq!(record.stable_type_id, 3);

        let target = world.spawn(StableEntityId(2)).id();
        let mut opaque = OpaqueComponents::default();
        let report = registry
            .apply_entity(&mut world, target, &record, &mut opaque)
            .expect("apply");

        assert_eq!(report.applied, 2);
        assert!(report.opaque_type_ids.is_empty());
        assert!(opaque.is_empty(), "没有未注册组件，opaque 不应有行");

        let health = world.get::<Health>(target).expect("Health 已插入");
        assert_eq!(health.current, 7);
        assert_eq!(health.max, 10);

        let position = world.get::<Position>(target).expect("Position 已插入");
        assert_eq!(position.x, -3);
        assert_eq!(position.y, 4);
        assert_eq!(position.z, 5);

        assert!(
            world.get::<Score>(target).is_none(),
            "未注册的组件不应被应用"
        );
    }

    /// T2：注册顺序不影响字节：组件按类型 ID 升序写出（§8）。
    #[test]
    fn registration_order_independent() {
        let mut health_first = PersistRegistry::new();
        health_first.register::<Health>().expect("register Health");
        health_first
            .register::<Position>()
            .expect("register Position");

        let mut position_first = PersistRegistry::new();
        position_first
            .register::<Position>()
            .expect("register Position");
        position_first
            .register::<Health>()
            .expect("register Health");

        let mut world = World::new();
        let entity = world
            .spawn((
                StableEntityId(9),
                Health { current: 1, max: 2 },
                Position { x: 3, y: 4, z: 5 },
            ))
            .id();

        let first = health_first
            .encode_entity(&world, entity, 1)
            .expect("encode");
        let second = position_first
            .encode_entity(&world, entity, 1)
            .expect("encode");

        assert_eq!(first.payload, second.payload, "注册顺序不得影响字节");
        assert_eq!(first, second);

        // 字节里的顺序 = 类型 ID 升序（11 在 22 之前），而不是注册顺序。
        let components = decode_component_table(&first.payload).expect("decode");
        let ids: Vec<u32> = components
            .iter()
            .map(|component| component.stable_type_id)
            .collect();
        assert_eq!(ids, vec![11, 22]);
    }

    /// T3：同 ID 第二次注册被拒绝，且不覆盖已有项。
    #[test]
    fn duplicate_type_id_rejected() {
        let mut registry = PersistRegistry::new();
        registry.register::<DupA>().expect("register DupA");

        let error = registry
            .register::<DupB>()
            .expect_err("同类型 ID 第二次注册必须报错");
        match error.kind() {
            SaveErrorKind::Duplicate { id, .. } => assert_eq!(*id, 55),
            other => panic!("期望 Duplicate，实际 {other:?}"),
        }
        assert_eq!(registry.len(), 1, "拒绝后长度必须仍是 1（不静默覆盖）");
        assert_eq!(
            registry.name_of(StableTypeId::new(55)),
            Some(std::any::type_name::<DupA>()),
            "已注册项必须仍是最早那个类型"
        );
    }

    /// T4：类型 ID = 0 的注册被拒绝。
    #[test]
    fn zero_type_id_rejected() {
        let mut registry = PersistRegistry::new();
        let error = registry
            .register::<ZeroId>()
            .expect_err("stable_type_id = 0 必须报错");
        match error.kind() {
            SaveErrorKind::InvalidValue { what, value } => {
                assert_eq!(*what, "component stable_type_id");
                assert_eq!(*value, 0);
            }
            other => panic!("期望 InvalidValue，实际 {other:?}"),
        }
        assert!(registry.is_empty());
    }

    /// T5：未注册类型的字节进 opaque，再用 `encode_entity_with_opaque` 原样写回。
    #[test]
    fn unknown_component_preserved() {
        let mut registry = PersistRegistry::new();
        registry.register::<Health>().expect("register Health");

        let known_payload =
            postcard::to_allocvec(&Health { current: 3, max: 8 }).expect("postcard encode");
        let opaque_payload: Vec<u8> = vec![0xAB, 0xCD, 0x00, 0x7F, 0xEE];
        let record = record_of(
            42,
            5,
            &[
                ComponentRecord {
                    stable_type_id: 11,
                    payload: known_payload,
                },
                ComponentRecord {
                    stable_type_id: 999,
                    payload: opaque_payload.clone(),
                },
            ],
        );

        let mut world = World::new();
        let entity = world.spawn(StableEntityId(42)).id();
        let mut opaque = OpaqueComponents::default();
        let report = registry
            .apply_entity(&mut world, entity, &record, &mut opaque)
            .expect("apply");

        assert_eq!(report.applied, 1, "只有已注册的 11 被应用");
        assert_eq!(report.opaque_type_ids, vec![999]);
        assert_eq!(
            opaque.get(42, 999),
            Some(opaque_payload.as_slice()),
            "未注册组件的原字节必须逐字节保留"
        );

        let health = world.get::<Health>(entity).expect("Health 已应用");
        assert_eq!((health.current, health.max), (3, 8));

        let written = registry
            .encode_entity_with_opaque(&world, entity, 5, &opaque)
            .expect("encode with opaque");
        let components = decode_component_table(&written.payload).expect("decode");
        let unknown = components
            .iter()
            .find(|component| component.stable_type_id == 999)
            .expect("未注册组件必须写回");
        assert_eq!(unknown.payload, opaque_payload, "写回字节必须逐字节相同");
    }

    /// T6：已注册组件负载解不开 -> 报错，且实体上一个组件都不插入。
    #[test]
    fn corrupt_payload_leaves_entity_unchanged() {
        let mut world = World::new();
        let source = world
            .spawn((
                StableEntityId(1),
                Health { current: 1, max: 2 },
                Counter { value: 3 },
            ))
            .id();

        // 先确认 Counter 已注册，才能走“已注册组件解不开”这条路径。
        let mut registry = registry_with_two();
        registry.register::<Counter>().expect("register Counter");
        let record = registry.encode_entity(&world, source, 1).expect("encode");

        // 把第二条（Counter，ID 44）的负载换成非法 postcard。
        let mut components = decode_component_table(&record.payload).expect("decode");
        assert_eq!(components.len(), 2);
        let second = components
            .iter_mut()
            .find(|component| component.stable_type_id == 44)
            .expect("第二条必须是 Counter");
        second.payload = OVERFLOW_VARINT.to_vec();

        let corrupt = record_of(7, 1, &components);

        let target = world.spawn(StableEntityId(7)).id();
        let mut opaque = OpaqueComponents::default();
        let error = registry
            .apply_entity(&mut world, target, &corrupt, &mut opaque)
            .expect_err("负载解不开必须报错");
        match error.kind() {
            SaveErrorKind::Payload { what, .. } => assert_eq!(*what, "postcard decode"),
            other => panic!("期望 Payload，实际 {other:?}"),
        }
        assert!(
            error.path().contains("component[44]"),
            "错误路径应指到解不开的组件，实际 {}",
            error.path()
        );

        assert!(
            world.get::<Health>(target).is_none(),
            "先全部解码暂存，失败时第一个组件也不得插入"
        );
        assert!(world.get::<Counter>(target).is_none());
    }

    /// T7：组件负载合法但多一个字节 -> `TrailingBytes`，且实体不变。
    #[test]
    fn trailing_bytes_rejected() {
        let registry = registry_with_two();
        let mut world = World::new();
        let source = world
            .spawn((StableEntityId(1), Health { current: 1, max: 2 }))
            .id();
        let record = registry.encode_entity(&world, source, 1).expect("encode");

        let mut components = decode_component_table(&record.payload).expect("decode");
        assert_eq!(components.len(), 1);
        components[0].payload.push(0x00);
        let trailing = record_of(2, 1, &components);

        let target = world.spawn(StableEntityId(2)).id();
        let mut opaque = OpaqueComponents::default();
        let error = registry
            .apply_entity(&mut world, target, &trailing, &mut opaque)
            .expect_err("尾随字节必须报错");
        match error.kind() {
            SaveErrorKind::TrailingBytes { extra } => assert_eq!(*extra, 1),
            other => panic!("期望 TrailingBytes，实际 {other:?}"),
        }
        assert!(world.get::<Health>(target).is_none());
    }

    /// T8：`remove_entity` 只删指定实体的行，返回删除条数。
    #[test]
    fn remove_entity_drops_rows() {
        let mut opaque = OpaqueComponents::default();
        opaque.insert(1, 999, vec![1, 2]);
        opaque.insert(1, 1000, vec![3]);
        opaque.insert(2, 999, vec![4, 5, 6]);
        assert_eq!(opaque.len(), 3);

        assert_eq!(opaque.remove_entity(1), 2);
        assert_eq!(opaque.len(), 1);
        assert_eq!(opaque.get(1, 999), None);
        assert_eq!(opaque.get(1, 1000), None);
        assert_eq!(
            opaque.get(2, 999),
            Some([4u8, 5, 6].as_slice()),
            "其它实体的行必须还在"
        );

        // 再删一次、删不存在的实体都是 0，不 panic。
        assert_eq!(opaque.remove_entity(1), 0);
        assert_eq!(opaque.remove_entity(3), 0);
    }

    /// T9：只有 `StableEntityId` 的实体编码出 4 字节空表，应用成功且 `applied == 0`。
    #[test]
    fn entity_without_components() {
        let registry = registry_with_two();
        let mut world = World::new();
        let source = world.spawn(StableEntityId(5)).id();

        let record = registry.encode_entity(&world, source, 1).expect("encode");
        assert_eq!(
            record.payload,
            vec![0, 0, 0, 0],
            "component_count = 0 + reserved = 0 共 4 字节"
        );

        let target = world.spawn(StableEntityId(6)).id();
        let mut opaque = OpaqueComponents::default();
        let report = registry
            .apply_entity(&mut world, target, &record, &mut opaque)
            .expect("apply");
        assert_eq!(report.applied, 0);
        assert!(report.opaque_type_ids.is_empty());
        assert!(opaque.is_empty());
    }

    /// T10：实体没有 `StableEntityId` -> `Missing`。
    #[test]
    fn missing_stable_entity_id() {
        let registry = registry_with_two();
        let mut world = World::new();
        let entity = world.spawn(Health { current: 1, max: 2 }).id();

        let error = registry
            .encode_entity(&world, entity, 1)
            .expect_err("缺 StableEntityId 必须报错");
        match error.kind() {
            SaveErrorKind::Missing { what, id } => {
                assert_eq!(*what, "StableEntityId component");
                assert_eq!(*id, 0);
            }
            other => panic!("期望 Missing，实际 {other:?}"),
        }
    }

    /// T11：原型 ID 为 0 -> `InvalidValue`。
    #[test]
    fn zero_entity_type_id_rejected() {
        let registry = registry_with_two();
        let mut world = World::new();
        let entity = world
            .spawn((StableEntityId(1), Health { current: 1, max: 2 }))
            .id();

        let error = registry
            .encode_entity(&world, entity, 0)
            .expect_err("原型 ID 为 0 必须报错");
        match error.kind() {
            SaveErrorKind::InvalidValue { what, value } => {
                assert_eq!(*what, "entity stable_type_id");
                assert_eq!(*value, 0);
            }
            other => panic!("期望 InvalidValue，实际 {other:?}"),
        }
    }

    /// T12：分区边界 + 注册表访问器（contains / len / name_of / registered_ids）。
    #[test]
    fn partition_and_registry_accessors() {
        assert_eq!(StableTypeId::new(11).partition(), IdPartition::Core);
        assert_eq!(StableTypeId::new(0).partition(), IdPartition::None);
        assert_eq!(StableTypeId::new(999).partition(), IdPartition::Core);
        assert_eq!(StableTypeId::new(1000).partition(), IdPartition::Mod);
        assert_eq!(StableTypeId::new(9999).partition(), IdPartition::Mod);
        assert_eq!(StableTypeId::new(10000).partition(), IdPartition::Hashed);

        let mut registry = PersistRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(!registry.contains(StableTypeId::new(11)));
        assert_eq!(registry.name_of(StableTypeId::new(11)), None);
        assert_eq!(registry.registered_ids().count(), 0);

        // 故意逆序注册：`registered_ids` 仍按 ID 升序。
        registry.register::<Position>().expect("register Position");
        registry.register::<Health>().expect("register Health");

        assert_eq!(registry.len(), 2);
        assert!(!registry.is_empty());
        assert!(registry.contains(StableTypeId::new(11)));
        assert!(registry.contains(StableTypeId::new(22)));
        assert!(!registry.contains(StableTypeId::new(33)));
        assert!(!registry.contains(StableTypeId::INVALID));

        assert_eq!(
            registry.name_of(StableTypeId::new(11)),
            Some(std::any::type_name::<Health>())
        );
        assert_eq!(
            registry.name_of(StableTypeId::new(22)),
            Some(std::any::type_name::<Position>())
        );
        assert_eq!(registry.name_of(StableTypeId::new(33)), None);

        let ids: Vec<u32> = registry.registered_ids().map(StableTypeId::get).collect();
        assert_eq!(ids, vec![11, 22]);
    }

    /// T13：类型已注册、实体上没有该组件，但 opaque 里有该 `(entity, type)` 行
    /// -> `encode_entity_with_opaque` 必须原样写回（不丢数据）。
    #[test]
    fn registered_but_absent_keeps_opaque() {
        let mut registry = PersistRegistry::new();
        registry.register::<Health>().expect("register Health");

        let mut world = World::new();
        let entity = world.spawn(StableEntityId(7)).id();

        // 读档时类型还没注册，字节原样存进 opaque；之后 mod 升级把类型注册上了。
        let original: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let mut opaque = OpaqueComponents::default();
        opaque.insert(7, 11, original.clone());

        let record = registry
            .encode_entity_with_opaque(&world, entity, 2, &opaque)
            .expect("encode with opaque");
        let components = decode_component_table(&record.payload).expect("decode");
        assert_eq!(components.len(), 1);
        assert_eq!(components[0].stable_type_id, 11);
        assert_eq!(
            components[0].payload, original,
            "已注册但实体上缺失的组件必须原样写回"
        );

        // 对照：不带 opaque 时这个组件不写（§4.2「仍缺失就仍不写」）。
        let plain = registry.encode_entity(&world, entity, 2).expect("encode");
        assert_eq!(plain.payload, vec![0, 0, 0, 0]);
    }
}
