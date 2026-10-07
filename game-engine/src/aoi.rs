//! 基于 `SpatialIndex` 的增量 AOI 管理插件。
//!
//! 本文件负责维护观察者视野，并在视野变化时发送进入和离开消息。
//! 它不会每帧遍历所有观察者，而是根据空间索引提供的脏区块信息，
//! 只更新可能受到影响的观察者。
//!
//! 该插件重点关注增量更新、低分配开销和大范围观察者的稳定性。
//!
//! 脏区块机制让普通观察者的更新成本与局部空间变化相关，而不是与全场实体数量相关。
//! 大半径观察者会被自动标记为全局观察者，避免在区块反向索引中注册过多区块。
//! 视野差集使用排序后的实体列表计算，避免频繁创建和哈希 `HashSet`。
//! 查询缓冲区和事件缓冲区会跨帧复用，以降低每帧堆分配压力。
use bevy::{
    platform::collections::{hash_map::Entry, HashMap, HashSet},
    prelude::*,
};
use std::cmp::Ordering;
use std::marker::PhantomData;
use std::mem;

use crate::spatial::{PartitionSystems, SpatialAnchor, SpatialChanges, SpatialIndex};

/// 单个观察者最多注册的区块数量。
///
/// 当观察者视野覆盖的区块数量超过该值时，
/// 它会被视为全局观察者，而不是继续注册大量区块。
const MAX_OBSERVER_REGISTERED_CHUNKS: usize = 4096;

/// AOI 观察者组件。
///
/// 该组件标记一个实体具备观察周围实体的能力。
/// `radius` 表示观察半径。
#[derive(Component)]
pub struct AoiObserver {
    /// 观察半径。
    ///
    /// `NaN` 和负数会被修正为零。
    /// 正无穷会被保留，并表示全局观察。
    pub radius: f32,
}

impl AoiObserver {
    /// 创建新的观察者组件。
    pub fn new(radius: f32) -> Self {
        Self {
            radius: sanitize_radius(radius),
        }
    }
}

/// 修正非法半径。///
/// `NaN` 和负数会回退为零。
/// 正无穷允许使用，通常会使观察者成为全局观察者。
fn sanitize_radius(radius: f32) -> f32 {
    if radius.is_nan() || radius < 0.0 {
        0.0
    } else {
        radius
    }
}

/// 单个观察者的运行状态。
///
/// 该状态保存观察者当前视野中心、半径、已知目标列表，
/// 以及该观察者注册到了哪些区块。
struct ObserverState {
    /// 观察者中心。
    center: Vec3,

    /// 观察者半径。
    radius: f32,

    /// 当前视野内的目标实体列表。
    ///
    /// 该列表保持排序，便于低开销差集计算。
    targets: Vec<Entity>,

    /// 该观察者注册的区块列表。
    chunks: Vec<IVec3>,

    /// 是否为全局观察者。
    global: bool,
}

/// AOI 管理资源。
///
/// 该资源维护所有观察者状态、区块到观察者的反向索引、
/// 全局观察者集合、脏观察者集合，以及多个可复用缓冲区。
#[derive(Resource)]
pub struct AoIManager {
    /// 观察者状态。
    observers: HashMap<Entity, ObserverState>,

    /// 区块到观察者的反向索引。
    ///
    /// 当某个区块发生变化时，可以通过该索引找到需要重新计算视野的观察者。
    chunk_to_observers: HashMap<IVec3, HashSet<Entity>>,

    /// 全局观察者集合。
    ///    /// 全局观察者会在任何全局空间变化发生时更新。
    global_observers: HashSet<Entity>,

    /// 本帧需要更新的观察者集合。
    dirty_observers: HashSet<Entity>,

    /// 复用查询缓冲区。
    query_buffer: Vec<Entity>,

    /// 复用进入消息缓冲区。
    entered_buffer: Vec<EntityEntered>,

    /// 复用离开消息缓冲区。
    left_buffer: Vec<EntityLeft>,
}

impl Default for AoIManager {
    fn default() -> Self {
        Self {
            observers: HashMap::default(),
            chunk_to_observers: HashMap::default(),
            global_observers: HashSet::default(),
            dirty_observers: HashSet::default(),
            query_buffer: Vec::with_capacity(1024),
            entered_buffer: Vec::with_capacity(256),
            left_buffer: Vec::with_capacity(256),
        }
    }
}

impl AoIManager {
    /// 指定观察者当前视野内的目标实体列表（只读接口，不破坏封装）。
    ///
    /// 表现层收集**只遍历这个列表**，而不是全世界扫 archetype ——
    /// 地图另一端的几万实体既不会被翻译，也不会被推给 Godot。
    ///
    /// 返回 `None` 表示该观察者不存在或已失效（还没挂 `AoiObserver`、
    /// 或已失去锚点 `GlobalTransform`），调用方应退化到「全部可见」或直接跳过。
    ///
    /// 列表由 `update_aoi` 维护，已按 `Entity::to_bits` 排序并去重。
    pub fn get_visible_targets(&self, observer: Entity) -> Option<&[Entity]> {
        self.observers
            .get(&observer)
            .map(|state| state.targets.as_slice())
    }

    /// 当前登记的观察者数量（诊断用）。
    pub fn observer_count(&self) -> usize {
        self.observers.len()
    }
}

/// 实体进入观察者视野消息。
///
/// 使用 Bevy 的无状态 `Message` 机制，
/// 以降低高频帧级通知的开销。
#[derive(Message)]
pub struct EntityEntered {
    /// 观察者实体。
    pub observer: Entity,

    /// 进入视野的目标实体。
    pub entity: Entity,
}

/// 实体离开观察者视野消息。
#[derive(Message)]
pub struct EntityLeft {
    /// 观察者实体。
    pub observer: Entity,

    /// 离开视野的目标实体。    
    pub entity: Entity,
}

// 公开的系统集
#[derive(SystemSet, Clone, Debug, PartialEq, Eq, Hash)]
pub struct AoiSystems;

/// AOI 管理插件。
///
/// 该插件初始化 `AoIManager`，注册进入和离开消息，
/// 并安排 AOI 更新系统在空间索引更新之后运行。
pub struct AoIPlugin<A: SpatialAnchor = GlobalTransform>(PhantomData<A>);

impl<A: SpatialAnchor> Default for AoIPlugin<A> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<A: SpatialAnchor> Plugin for AoIPlugin<A> {
    fn build(&self, app: &mut App) {
        app.init_resource::<AoIManager>()
            .add_message::<EntityEntered>()
            .add_message::<EntityLeft>()
            .add_systems(
                Update,
                update_aoi::<A>.after(PartitionSystems).in_set(AoiSystems),
            );
    }
}

/// 每帧增量更新 AOI。
///
/// 该系统先清理失效观察者，再根据空间变化和观察者自身变化标记脏观察者。
/// 随后只重新查询脏观察者的视野，并通过排序差集生成进入和离开消息。
/// 不排除观察者自身
#[allow(clippy::too_many_arguments)]
pub fn update_aoi<A: SpatialAnchor>(
    mut manager: ResMut<AoIManager>,
    index: Res<SpatialIndex>,
    changes: Res<SpatialChanges>,
    observers: Query<(Entity, &A, &AoiObserver)>,
    changed_observers: Query<
        Entity,
        (
            With<AoiObserver>,
            Or<(
                Added<A>,
                Changed<A>,
                Added<AoiObserver>,
                Changed<AoiObserver>,
            )>,
        ),
    >,
    invalid_observers: Query<Entity, (With<AoiObserver>, Without<A>)>,
    mut removed_observers: RemovedComponents<AoiObserver>,
    mut ev_entered: MessageWriter<EntityEntered>,
    mut ev_left: MessageWriter<EntityLeft>,
) {
    let m = &mut *manager;

    m.entered_buffer.clear();
    m.left_buffer.clear();
    // 先清理已经移除 AoiObserver 的观察者。
    for entity in removed_observers.read() {
        remove_observer(m, entity);
    }

    // 再清理仍然有 AoiObserver，但已经失去 GlobalTransform 的观察者。
    for entity in invalid_observers.iter() {
        remove_observer(m, entity);
    }

    if changes.global_dirty {
        m.dirty_observers.extend(m.observers.keys().copied());
    } else if changes.has_changes() {
        m.dirty_observers.extend(m.global_observers.iter().copied());

        for chunk in changes.dirty_chunks.iter() {
            if let Some(observers_in_chunk) = m.chunk_to_observers.get(chunk) {
                m.dirty_observers.extend(observers_in_chunk.iter().copied());
            }
        }
    }

    for entity in changed_observers.iter() {
        m.dirty_observers.insert(entity);
    }

    let mut buffer = mem::take(&mut m.query_buffer);
    let mut dirty = mem::take(&mut m.dirty_observers);

    for observer in dirty.drain() {
        let Ok((_, anchor, obs)) = observers.get(observer) else {
            continue;
        };

        let center = anchor.anchor_point();
        let radius = sanitize_radius(obs.radius);

        buffer.clear();
        index.query_radius_into(center, radius, &mut buffer);

        buffer.sort_unstable_by(|a, b| a.to_bits().cmp(&b.to_bits()));
        buffer.dedup();

        // 只有观察者自身空间状态变化时才重新注册区块。
        // 如果仅目标变化，不需要反复注销/注册 chunk_to_observers。
        let needs_reregister = match m.observers.get(&observer) {
            Some(state) => state.center != center || state.radius != radius,
            None => true,
        };

        let mut new_registration: Option<(bool, Vec<IVec3>)> = None;

        if needs_reregister {
            // 先取出旧注册信息，避免后续同时借用 observers 和 chunk_to_observers。
            let old_data = m.observers.get_mut(&observer).map(|state| {
                let old_global = state.global;
                let old_chunks = mem::take(&mut state.chunks);
                (old_global, old_chunks)
            });

            if let Some((old_global, old_chunks)) = old_data {
                if old_global {
                    m.global_observers.remove(&observer);
                }

                for chunk in old_chunks {
                    remove_observer_from_chunk(&mut m.chunk_to_observers, chunk, observer);
                }
            }

            let chunks_opt =
                index.chunks_in_radius_limited(center, radius, MAX_OBSERVER_REGISTERED_CHUNKS);

            let is_global = chunks_opt.is_none();
            let chunks_vec = chunks_opt.unwrap_or_default();

            if is_global {
                m.global_observers.insert(observer);
            } else {
                for chunk in &chunks_vec {
                    m.chunk_to_observers
                        .entry(*chunk)
                        .or_default()
                        .insert(observer);
                }
            }

            new_registration = Some((is_global, chunks_vec));
        }

        match m.observers.entry(observer) {
            Entry::Occupied(mut occupied) => {
                let state = occupied.get_mut();
                if let Some((is_global, chunks_vec)) = new_registration {
                    state.center = center;
                    state.radius = radius;
                    state.global = is_global;
                    state.chunks = chunks_vec;
                }

                diff_sorted(
                    &state.targets,
                    &buffer,
                    observer,
                    &mut m.entered_buffer,
                    &mut m.left_buffer,
                );

                state.targets.clear();
                state.targets.extend_from_slice(&buffer);
            }
            Entry::Vacant(vacant) => {
                let (is_global, chunks_vec) = new_registration.unwrap_or_default();

                for &entity in buffer.iter() {
                    m.entered_buffer.push(EntityEntered { observer, entity });
                }

                vacant.insert(ObserverState {
                    center,
                    radius,
                    targets: buffer.clone(),
                    chunks: chunks_vec,
                    global: is_global,
                });
            }
        }
    }

    m.dirty_observers = dirty;
    m.query_buffer = buffer;

    if !m.entered_buffer.is_empty() {
        ev_entered.write_batch(m.entered_buffer.drain(..));
    }

    if !m.left_buffer.is_empty() {
        ev_left.write_batch(m.left_buffer.drain(..));
    }
}

/// 从 AOI 管理器中移除观察者。
///
/// 移除观察者时，会同步清理它注册的区块，
/// 并向其当前视野内的所有目标发送离开消息。
fn remove_observer(m: &mut AoIManager, observer: Entity) {
    let Some(state) = m.observers.remove(&observer) else {
        return;
    };

    m.dirty_observers.remove(&observer);

    if state.global {
        m.global_observers.remove(&observer);
    }

    for target in state.targets {
        m.left_buffer.push(EntityLeft {
            observer,
            entity: target,
        });
    }
    for chunk in state.chunks {
        remove_observer_from_chunk(&mut m.chunk_to_observers, chunk, observer);
    }
}

/// 从区块反向索引中移除观察者。
///
/// 当某个区块中的观察者集合为空时，该区块索引会被移除，
/// 避免长期持有空集合。
fn remove_observer_from_chunk(
    chunk_to_observers: &mut HashMap<IVec3, HashSet<Entity>>,
    chunk: IVec3,
    observer: Entity,
) {
    if let Some(set) = chunk_to_observers.get_mut(&chunk) {
        set.remove(&observer);

        if set.is_empty() {
            chunk_to_observers.remove(&chunk);
        }
    }
}

/// 对两个已排序的实体列表做差集。
///
/// `new - old` 会生成进入消息。
/// `old - new` 会生成离开消息。
fn diff_sorted(
    old: &[Entity],
    new: &[Entity],
    observer: Entity,
    entered: &mut Vec<EntityEntered>,
    left: &mut Vec<EntityLeft>,
) {
    let mut i = 0;
    let mut j = 0;

    while i < old.len() && j < new.len() {
        match old[i].to_bits().cmp(&new[j].to_bits()) {
            Ordering::Less => {
                left.push(EntityLeft {
                    observer,
                    entity: old[i],
                });
                i += 1;
            }
            Ordering::Greater => {
                entered.push(EntityEntered {
                    observer,
                    entity: new[j],
                });
                j += 1;
            }
            Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }

    while i < old.len() {
        left.push(EntityLeft {
            observer,
            entity: old[i],
        });
        i += 1;
    }

    while j < new.len() {
        entered.push(EntityEntered {
            observer,
            entity: new[j],
        });
        j += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spatial::{Size, SpatialPlugin};
    use bevy::app::App;
    use bevy::math::Vec3;

    fn test_app() -> App {
        let mut app = App::new();
        app.add_plugins(SpatialPlugin::<GlobalTransform, Size>::new(10.0))
            .add_plugins(AoIPlugin::<GlobalTransform>::default());
        app
    }

    #[test]
    fn test_enter_and_leave_events() {
        let mut app = test_app();

        let observer;
        let target;

        {
            let world = app.world_mut();
            observer = world
                .spawn((
                    Transform::from_translation(Vec3::ZERO),
                    AoiObserver::new(10.0),
                ))
                .id();

            target = world
                .spawn((
                    Transform::from_translation(Vec3::new(5.0, 0.0, 0.0)),
                    Size(None),
                ))
                .id();
        }

        let mut enter_reader = app
            .world()
            .resource::<Messages<EntityEntered>>()
            .get_cursor();
        let mut left_reader = app.world().resource::<Messages<EntityLeft>>().get_cursor();

        app.update();

        let messages = app.world().resource::<Messages<EntityEntered>>();
        let entered: Vec<_> = enter_reader.read(messages).collect();

        assert_eq!(entered.len(), 1);
        assert_eq!(entered[0].observer, observer);
        assert_eq!(entered[0].entity, target);

        app.world_mut()
            .entity_mut(target)
            .insert(Transform::from_translation(Vec3::new(50.0, 0.0, 0.0)));

        app.update();

        let left_messages = app.world().resource::<Messages<EntityLeft>>();
        let left: Vec<_> = left_reader.read(left_messages).collect();

        assert!(left
            .iter()
            .any(|e| e.observer == observer && e.entity == target));
    }

    #[test]
    fn test_leave_within_same_chunk() {
        let mut app = test_app();

        let observer;
        let target;

        {
            let world = app.world_mut();

            observer = world
                .spawn((
                    Transform::from_translation(Vec3::ZERO),
                    AoiObserver::new(2.0),
                ))
                .id();

            target = world
                .spawn((
                    Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)),
                    Size(None),
                ))
                .id();
        }

        let mut enter_reader = app
            .world()
            .resource::<Messages<EntityEntered>>()
            .get_cursor();

        app.update();

        let messages = app.world().resource::<Messages<EntityEntered>>();
        let entered: Vec<_> = enter_reader.read(messages).collect();

        assert_eq!(entered.len(), 1);
        assert_eq!(entered[0].observer, observer);
        assert_eq!(entered[0].entity, target);

        let mut left_reader = app.world().resource::<Messages<EntityLeft>>().get_cursor();

        // 1.0 -> 3.0 仍然在同一个 10.0 区块内，但已经离开半径 2.0。
        app.world_mut()
            .entity_mut(target)
            .insert(Transform::from_translation(Vec3::new(3.0, 0.0, 0.0)));

        app.update();

        let left_messages = app.world().resource::<Messages<EntityLeft>>();
        let left: Vec<_> = left_reader.read(left_messages).collect();

        assert!(
            left.iter()
                .any(|e| e.observer == observer && e.entity == target),
            "同区块内移动也必须触发 Leave 事件"
        );
    }

    #[test]
    fn test_non_finite_target_sends_leave() {
        let mut app = test_app();

        let observer;
        let target;

        {
            let world = app.world_mut();

            observer = world
                .spawn((
                    Transform::from_translation(Vec3::ZERO),
                    AoiObserver::new(10.0),
                ))
                .id();

            target = world
                .spawn((
                    Transform::from_translation(Vec3::new(5.0, 0.0, 0.0)),
                    Size(None),
                ))
                .id();
        }

        app.update();

        let mut left_reader = app.world().resource::<Messages<EntityLeft>>().get_cursor();

        app.world_mut()
            .entity_mut(target)
            .insert(Transform::from_translation(Vec3::new(f32::NAN, 0.0, 0.0)));

        app.update();

        let left_messages = app.world().resource::<Messages<EntityLeft>>();
        let left: Vec<_> = left_reader.read(left_messages).collect();

        assert!(
            left.iter()
                .any(|e| e.observer == observer && e.entity == target),
            "目标坐标变为非有限值后应触发 Leave 事件"
        );
    }

    #[test]
    fn test_observer_missing_transform_sends_leave() {
        let mut app = test_app();

        let observer;
        let target;

        {
            let world = app.world_mut();

            observer = world
                .spawn((
                    Transform::from_translation(Vec3::ZERO),
                    AoiObserver::new(10.0),
                ))
                .id();

            target = world
                .spawn((
                    Transform::from_translation(Vec3::new(5.0, 0.0, 0.0)),
                    Size(None),
                ))
                .id();
        }

        app.update();

        let mut left_reader = app.world().resource::<Messages<EntityLeft>>().get_cursor();

        app.world_mut()
            .entity_mut(observer)
            .remove::<GlobalTransform>();

        app.update();

        let left_messages = app.world().resource::<Messages<EntityLeft>>();
        let left: Vec<_> = left_reader.read(left_messages).collect();

        assert!(
            left.iter()
                .any(|e| e.observer == observer && e.entity == target),
            "观察者失去 GlobalTransform 后应触发 Leave 事件"
        );
    }

    #[test]
    fn test_incremental_update_isolation() {
        let mut app = test_app();

        let obs_a;
        let obs_b;
        let target;

        {
            let world = app.world_mut();

            obs_a = world
                .spawn((
                    Transform::from_translation(Vec3::ZERO),
                    AoiObserver::new(10.0),
                ))
                .id();

            obs_b = world
                .spawn((
                    Transform::from_translation(Vec3::new(100.0, 0.0, 0.0)),
                    AoiObserver::new(10.0),
                ))
                .id();

            target = world
                .spawn((
                    Transform::from_translation(Vec3::new(5.0, 0.0, 0.0)),
                    Size(None),
                ))
                .id();
        }

        let mut enter_reader = app
            .world()
            .resource::<Messages<EntityEntered>>()
            .get_cursor();

        app.update();

        let messages = app.world().resource::<Messages<EntityEntered>>();
        let entered: Vec<_> = enter_reader.read(messages).collect();

        assert_eq!(entered.len(), 1);
        assert_eq!(entered[0].observer, obs_a);
        app.world_mut()
            .entity_mut(target)
            .insert(Transform::from_translation(Vec3::new(105.0, 0.0, 0.0)));

        let mut left_reader = app.world().resource::<Messages<EntityLeft>>().get_cursor();
        let mut enter_reader_2 = app
            .world()
            .resource::<Messages<EntityEntered>>()
            .get_cursor();

        app.update();

        let left_msgs = app.world().resource::<Messages<EntityLeft>>();
        let left: Vec<_> = left_reader.read(left_msgs).collect();

        assert!(left
            .iter()
            .any(|e| e.observer == obs_a && e.entity == target));
        assert!(!left.iter().any(|e| e.observer == obs_b));

        let enter_msgs = app.world().resource::<Messages<EntityEntered>>();
        let entered_2: Vec<_> = enter_reader_2.read(enter_msgs).collect();

        assert!(entered_2
            .iter()
            .any(|e| e.observer == obs_b && e.entity == target));
    }

    #[test]
    fn test_observer_radius_change() {
        let mut app = App::new();
        app.add_plugins(SpatialPlugin::<GlobalTransform, Size>::new(10.0))
            .add_plugins(AoIPlugin::<GlobalTransform>::default());

        let observer;
        let target_near;
        let target_far;

        {
            let world = app.world_mut();

            observer = world
                .spawn((
                    Transform::from_translation(Vec3::ZERO),
                    AoiObserver::new(20.0),
                ))
                .id();

            target_near = world
                .spawn((
                    Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)),
                    Size(None),
                ))
                .id();

            target_far = world
                .spawn((
                    Transform::from_translation(Vec3::new(50.0, 0.0, 0.0)),
                    Size(None),
                ))
                .id();
        }

        let mut enter_reader = app
            .world()
            .resource::<Messages<EntityEntered>>()
            .get_cursor();

        app.update();

        let msgs = app.world().resource::<Messages<EntityEntered>>();
        let entered: Vec<_> = enter_reader.read(msgs).collect();

        assert!(entered.iter().any(|e| e.entity == target_near));
        assert!(!entered.iter().any(|e| e.entity == target_far));

        app.world_mut()
            .entity_mut(observer)
            .insert(AoiObserver::new(100.0));

        let mut enter_reader_2 = app
            .world()
            .resource::<Messages<EntityEntered>>()
            .get_cursor();

        app.update();

        let msgs2 = app.world().resource::<Messages<EntityEntered>>();
        let entered2: Vec<_> = enter_reader_2.read(msgs2).collect();

        assert!(
            entered2.iter().any(|e| e.entity == target_far),
            "半径扩大后应看到 far"
        );

        app.world_mut()
            .entity_mut(observer)
            .insert(AoiObserver::new(5.0));

        let mut left_reader = app.world().resource::<Messages<EntityLeft>>().get_cursor();

        app.update();

        let left_msgs = app.world().resource::<Messages<EntityLeft>>();
        let left: Vec<_> = left_reader.read(left_msgs).collect();

        assert!(
            left.iter().any(|e| e.entity == target_near),
            "半径缩小后应丢失 near"
        );
    }

    #[test]
    fn test_observer_despawn_sends_leave_events() {
        let mut app = App::new();
        app.add_plugins(SpatialPlugin::<GlobalTransform, Size>::new(10.0))
            .add_plugins(AoIPlugin::<GlobalTransform>::default());

        let observer;
        let target;

        {
            let world = app.world_mut();

            observer = world
                .spawn((
                    Transform::from_translation(Vec3::ZERO),
                    AoiObserver::new(20.0),
                ))
                .id();

            target = world
                .spawn((
                    Transform::from_translation(Vec3::new(5.0, 0.0, 0.0)),
                    Size(None),
                ))
                .id();
        }

        app.update();

        let mut left_reader = app.world().resource::<Messages<EntityLeft>>().get_cursor();

        app.world_mut().despawn(observer);

        app.update();

        let left_msgs = app.world().resource::<Messages<EntityLeft>>();
        let left: Vec<_> = left_reader.read(left_msgs).collect();

        assert!(
            left.iter()
                .any(|e| e.observer == observer && e.entity == target),
            "观察者销毁应触发 Leave 事件"
        );
    }

    #[test]
    fn bench_large_scale_aoi() {
        use crate::spatial::{Size, SpatialPlugin};
        use bevy::app::App;
        use bevy::math::Vec3;
        use std::time::Instant;

        let mut app = App::new();
        app.add_plugins(SpatialPlugin::<GlobalTransform, Size>::new(32.0))
            .add_plugins(AoIPlugin::<GlobalTransform>::default());

        let num_targets = 100_000;
        let num_observers = 100;

        {
            let world = app.world_mut();

            for i in 0..num_targets {
                let x = (i % 100) as f32 * 3.0 - 150.0;
                let y = ((i / 100) % 100) as f32 * 3.0 - 150.0;
                let z = (i / 10000) as f32 * 3.0 - 150.0;

                world.spawn((Transform::from_translation(Vec3::new(x, y, z)), Size(None)));
            }

            for i in 0..num_observers {
                let x = (i % 50) as f32 * 6.0 - 150.0;
                let y = ((i / 50) % 50) as f32 * 6.0 - 150.0;
                let z = (i / 2500) as f32 * 6.0 - 150.0;

                world.spawn((
                    Transform::from_translation(Vec3::new(x, y, z)),
                    AoiObserver::new(25.0),
                ));
            }
        }

        let start = Instant::now();
        app.update();
        let init_time = start.elapsed();
        println!("🚀 [Init] 100k targets, 100 observers: {:?}", init_time);

        let start = Instant::now();
        app.update();
        let idle_time = start.elapsed();
        println!("💤 [Idle] No changes: {:?}", idle_time);

        let mut entities = Vec::new();
        {
            let world = app.world_mut();
            let mut query = world.query_filtered::<Entity, With<Size>>();
            entities.extend(query.iter(world));
        }

        let partial_count = (entities.len() / 10).max(1);
        {
            let world = app.world_mut();

            for &entity in entities.iter().take(partial_count) {
                let mut e = world.entity_mut(entity);
                let mut t = *e.get::<Transform>().unwrap();
                t.translation.x += 33.0;
                e.insert(t);
            }
        }

        let start = Instant::now();
        app.update();
        let partial_time = start.elapsed();
        println!("🏃 [Partial] 10% targets moved: {:?}", partial_time);

        {
            let world = app.world_mut();

            for &entity in entities.iter() {
                let mut e = world.entity_mut(entity);
                let mut t = *e.get::<Transform>().unwrap();
                t.translation.y += 33.0;
                e.insert(t);
            }
        }

        let start = Instant::now();
        app.update();
        let full_time = start.elapsed();
        println!("🌪️ [Full] 100% targets moved: {:?}", full_time);
    }
}
