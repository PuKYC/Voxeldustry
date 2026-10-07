//! 空间索引插件。
//!
//! 本文件提供面向 3D 场景的空间查询基础设施。
//! 它以固定边长的区块作为粗粒度分区单位，并在每个区块内部维护一棵 `RTree`。
//! 当实体的包围盒尺寸超过单个区块时，实体会被移动到全局树中，以避免跨区块注册带来的维护成本。
//!
//! 该插件重点关注查询效率、增量通知和生命周期安全。
//!
//! `SpatialIndex` 对外提供 AABB 查询、半径查询和全量查询。
//! 查询接口会优先根据查询范围定位候选区块，只遍历可能相交的局部树。
//! 当候选区块过多时，查询会退化为遍历现有区块树，避免生成过大的区块坐标集合。
//!
//! `SpatialChanges` 会记录本帧发生过实体变化的区块，以及是否出现全局变化。
//! 上层的 AOI 系统可以只更新受这些区块影响的观察者，而不是每帧扫描全部观察者。
//!
//! `Size` 或 `GlobalTransform` 被移除时，系统会自动清理对应实体在索引中的记录，
//! 防止悬挂引用、脏数据和内存泄漏。
//!
//! 空间锚点用**世界位置** `GlobalTransform`（不是局部 `Transform`）：
//! 逻辑系统写 `Transform`，`sync_global_transforms` 在同帧把它推进到
//! `GlobalTransform`，随后本帧的索引 / AOI 查询看到的就是最新世界位置。
use bevy::{
    math::bounding::Aabb3d,
    platform::collections::{HashMap, HashSet},
    prelude::*,
};
use rstar::{PointDistance, RTree, RTreeObject, AABB};
use std::marker::PhantomData;

/// 空间锚点：提供一个实体的世界坐标。
///
/// 默认以 Bevy 的 `GlobalTransform`（世界位置）为锚点；也允许 game-core
/// 用任意自定义组件实现它。
pub trait SpatialAnchor: Component {
    /// 世界坐标。
    fn anchor_point(&self) -> Vec3;
}

/// 空间尺寸：`Some([w, h, d])` 表示以锚点为中心的 AABB；`None` 表示点实体。
pub trait SpatialExtent: Component {
    fn extents(&self) -> Option<[f32; 3]>;
}

/// 默认区块大小。
pub const DEFAULT_CHUNK_SIZE: f32 = 64.0;

#[derive(SystemSet, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PartitionSystems;

/// 空间索引插件。
///
/// 该插件负责初始化空间索引资源、空间变化记录资源，
/// 并注册每帧用于清理和更新空间索引的系统。
pub struct SpatialPlugin<A = GlobalTransform, X = Size> {
    /// 区块大小。
    ///
    /// 该值决定了空间分区的粒度。
    /// 较小的区块可以减少查询时的候选范围，但会增加区块树数量。
    /// 较大的区块可以减少区块数量，但单次区块内查询成本会更高。
    pub chunk_size: f32,
    _marker: PhantomData<(A, X)>,
}

impl<A, X> SpatialPlugin<A, X> {
    /// 使用指定区块大小构造。
    pub fn new(chunk_size: f32) -> Self {
        Self {
            chunk_size,
            _marker: PhantomData,
        }
    }
}

impl<A, X> Default for SpatialPlugin<A, X> {
    fn default() -> Self {
        Self::new(DEFAULT_CHUNK_SIZE)
    }
}

impl<A: SpatialAnchor, X: SpatialExtent> Plugin for SpatialPlugin<A, X> {
    fn build(&self, app: &mut App) {
        let chunk_size = sanitize_chunk_size(self.chunk_size);
        if chunk_size != self.chunk_size {
            warn!(
                "SpatialPlugin.chunk_size 非法：{:?}，已回退为默认值 {}",
                self.chunk_size, DEFAULT_CHUNK_SIZE
            );
        }

        app.insert_resource(SpatialIndex::new(chunk_size))
            .init_resource::<SpatialChanges>()
            .add_systems(First, clear_spatial_changes.in_set(PartitionSystems))
            .add_systems(
                PreUpdate,
                (cleanup_spatial_index::<X>, cleanup_missing_anchor::<A>)
                    .chain()
                    .in_set(PartitionSystems),
            )
            .add_systems(
                Update,
                (sync_global_transforms, update_spatial_index::<A, X>)
                    .chain()
                    .in_set(PartitionSystems),
            );
    }
}

/// 把改过的 `Transform` 同步到 `GlobalTransform`（空间锚点用世界位置）。
///
/// Bevy 的 `TransformPlugin` 在 `PostUpdate` 才传播，会让本帧空间 / AOI
/// 读到上一帧的世界位置；headless 之下也未必挂 `TransformPlugin`。这里在
/// `PartitionSystems` 内、`update_spatial_index` 之前做一次同帧同步，
/// 保证逻辑 tick 写入的 `Transform` 当帧即为空间索引所见。
///
/// 只写 `Changed<Transform>` 的实体，避免每帧无谓地标记 `GlobalTransform`。
pub fn sync_global_transforms(
    mut query: Query<(&Transform, &mut GlobalTransform), Changed<Transform>>,
) {
    for (transform, mut global) in &mut query {
        *global = GlobalTransform::from(*transform);
    }
}

/// 将区块大小修正为合法值。
///
/// 区块大小必须是有限正数。
/// 非法值会回退到默认区块大小。
fn sanitize_chunk_size(chunk_size: f32) -> f32 {
    if chunk_size.is_finite() && chunk_size > 0.0 {
        chunk_size
    } else {
        DEFAULT_CHUNK_SIZE
    }
}

/// 本帧空间变化情况。
///
/// 该资源用于 AOI 系统做增量更新。
/// 每帧开始时会被清空，随后由空间索引更新系统写入本帧变化。
#[derive(Resource, Default)]
pub struct SpatialChanges {
    /// 发生变化的区块坐标。
    ///
    /// 当实体进入、离开或在某个区块内发生位置/形状变化时，
    /// 对应区块会被记录到这里。
    pub dirty_chunks: HashSet<IVec3>,

    /// 是否存在全局变化。
    ///
    /// 超大实体移动、进入或离开全局树时，会标记该字段。
    /// 上层系统通常需要在该字段为真时更新所有全局观察者。
    pub global_dirty: bool,
}
impl SpatialChanges {
    /// 清空本帧的变化记录。
    pub fn clear(&mut self) {
        self.dirty_chunks.clear();
        self.global_dirty = false;
    }

    /// 检查本帧是否有任何空间变化。
    pub fn has_changes(&self) -> bool {
        self.global_dirty || !self.dirty_chunks.is_empty()
    }
}

/// 每帧清空空间变化记录。
pub fn clear_spatial_changes(mut changes: ResMut<SpatialChanges>) {
    changes.clear();
}

/// 几何形状。
///
/// 当前支持点形状和轴对齐包围盒两种空间表达。
#[derive(Debug, Clone, Copy)]
pub enum Shape {
    /// 点形状。
    Point {
        /// 三维坐标。
        pos: [f32; 3],
    },

    /// 轴对齐包围盒形状。
    Aabb {
        /// Bevy 提供的三维 AABB。
        aabb: Aabb3d,
    },
}

/// 存储在 R-tree 中的空间实体。
///
/// 该结构同时携带实体句柄和空间形状。
/// 相等性只比较实体句柄，这样同一个实体在索引更新时可以被正确移除。
#[derive(Debug, Clone)]
pub struct SpatialEntity {
    /// Bevy 实体句柄。
    pub entity: Entity,

    /// 实体当前的空间形状。
    pub position: Shape,
}

impl PartialEq for SpatialEntity {
    fn eq(&self, other: &Self) -> bool {
        self.entity == other.entity
    }
}

impl Eq for SpatialEntity {}

impl RTreeObject for SpatialEntity {
    type Envelope = AABB<[f32; 3]>;

    fn envelope(&self) -> Self::Envelope {
        match self.position {
            Shape::Point { pos } => AABB::from_point(pos),
            Shape::Aabb { aabb } => AABB::from_bounds(aabb.min.to_array(), aabb.max.to_array()),
        }
    }
}

impl PointDistance for SpatialEntity {
    fn distance_2(&self, point: &[f32; 3]) -> f32 {
        match self.position {
            Shape::Point { pos } => {
                let dx = pos[0] - point[0];
                let dy = pos[1] - point[1];
                let dz = pos[2] - point[2];
                dx * dx + dy * dy + dz * dz
            }
            Shape::Aabb { aabb } => {
                let p = Vec3::from_array(*point);
                let min = aabb.min;
                let max = aabb.max;

                let dx = if p.x < min.x {
                    min.x - p.x
                } else if p.x > max.x {
                    p.x - max.x
                } else {
                    0.0
                };

                let dy = if p.y < min.y {
                    min.y - p.y
                } else if p.y > max.y {
                    p.y - max.y
                } else {
                    0.0
                };
                let dz = if p.z < min.z {
                    min.z - p.z
                } else if p.z > max.z {
                    p.z - max.z
                } else {
                    0.0
                };

                dx * dx + dy * dy + dz * dz
            }
        }
    }
}

/// 全局空间索引资源。
///
/// 该资源按区块保存局部实体，并额外维护一棵全局树来保存超大实体。
/// 普通实体根据其位置落入对应区块。
/// 尺寸超过区块大小的实体会进入全局树，避免在多个区块之间重复注册。
#[derive(Resource)]
pub struct SpatialIndex {
    /// 当前区块大小。
    chunk_size: f32,

    /// 区块坐标到局部 R-tree 的映射。
    trees: HashMap<IVec3, RTree<SpatialEntity>>,

    /// 全局 R-tree，用于保存超大实体。
    global_tree: RTree<SpatialEntity>,

    /// 实体当前形状及其所属区块。
    ///
    /// 如果区块为 `None`，表示实体当前位于全局树中。
    positions: HashMap<Entity, (Shape, Option<IVec3>)>,
}

impl SpatialIndex {
    /// 使用指定区块大小创建索引。
    pub fn new(chunk_size: f32) -> Self {
        Self {
            chunk_size: sanitize_chunk_size(chunk_size),
            trees: HashMap::default(),
            global_tree: RTree::new(),
            positions: HashMap::default(),
        }
    }

    /// 获取当前区块大小。
    pub fn chunk_size(&self) -> f32 {
        self.chunk_size
    }

    /// 计算点所在的区块坐标。
    ///
    /// 区块坐标使用向下取整。
    fn chunk_key(&self, pos: [f32; 3]) -> IVec3 {
        let s = self.chunk_size;
        debug_assert!(
            s.is_finite() && s > 0.0,
            "chunk_size must be positive and finite"
        );

        IVec3::new(
            (pos[0] / s).floor() as i32,
            (pos[1] / s).floor() as i32,
            (pos[2] / s).floor() as i32,
        )
    }

    /// AABB 范围查询。
    ///
    /// 返回所有与给定 AABB 相交的实体。
    /// 该接口会分配新的 `Vec`。
    /// 如果调用方已有缓冲区，建议使用 `query_aabb_into`。
    pub fn query_aabb(&self, aabb: Aabb3d) -> Vec<Entity> {
        let mut out = Vec::new();
        self.query_aabb_into(aabb, &mut out);
        out
    }

    /// 将 AABB 查询结果追加到已有缓冲区。
    ///
    /// 该方法不会清空传入的缓冲区。
    /// 调用方可以根据需要自行复用缓冲区。
    pub fn query_aabb_into(&self, aabb: Aabb3d, out: &mut Vec<Entity>) {
        if !aabb.min.is_finite() || !aabb.max.is_finite() {
            return;
        }

        let min = aabb.min.min(aabb.max);
        let max = aabb.min.max(aabb.max);
        let query_aabb = AABB::from_bounds(min.to_array(), max.to_array());

        let min_chunk = self.chunk_key(min.to_array());
        let max_chunk = self.chunk_key(max.to_array());

        let min_chunk_ext = min_chunk - IVec3::ONE;
        let max_chunk_ext = max_chunk + IVec3::ONE;

        let candidate_chunks = candidate_chunk_count(min_chunk_ext, max_chunk_ext);
        let threshold = 64_i64.max(self.trees.len() as i64);

        if candidate_chunks > threshold {
            for tree in self.trees.values() {
                out.extend(
                    tree.locate_in_envelope_intersecting(query_aabb)
                        .map(|e| e.entity),
                );
            }
        } else {
            for x in min_chunk_ext.x..=max_chunk_ext.x {
                for y in min_chunk_ext.y..=max_chunk_ext.y {
                    for z in min_chunk_ext.z..=max_chunk_ext.z {
                        if let Some(tree) = self.trees.get(&IVec3::new(x, y, z)) {
                            out.extend(
                                tree.locate_in_envelope_intersecting(query_aabb)
                                    .map(|e| e.entity),
                            );
                        }
                    }
                }
            }
        }

        out.extend(
            self.global_tree
                .locate_in_envelope_intersecting(query_aabb)
                .map(|e| e.entity),
        );
    }

    /// 球体范围查询。
    ///
    /// 返回所有与给定球体相交的实体。
    /// 该接口会分配新的 `Vec`。
    /// 如果调用方已有缓冲区，建议使用 `query_radius_into`。
    pub fn query_radius(&self, center: Vec3, radius: f32) -> Vec<Entity> {
        let mut out = Vec::new();
        self.query_radius_into(center, radius, &mut out);
        out
    }

    /// 将球体查询结果追加到已有缓冲区。
    ///
    /// 该方法不会清空传入的缓冲区。
    /// 调用方可以根据需要自行复用缓冲区。
    pub fn query_radius_into(&self, center: Vec3, radius: f32, out: &mut Vec<Entity>) {
        if !center.is_finite() || radius.is_nan() || radius < 0.0 {
            return;
        }
        if radius.is_infinite() {
            self.query_all_into(out);
            return;
        }

        let center_arr = center.to_array();

        // rstar 的 locate_within_distance 使用平方半径。
        let radius_sq = radius * radius;
        if !radius_sq.is_finite() {
            // 半径过大导致平方溢出时，退化为全量查询，避免错误裁剪。
            self.query_all_into(out);
            return;
        }

        let half = Vec3::splat(radius);
        let min = center - half;
        let max = center + half;

        if !min.is_finite() || !max.is_finite() {
            for tree in self.trees.values() {
                out.extend(
                    tree.locate_within_distance(center_arr, radius_sq)
                        .map(|e| e.entity),
                );
            }

            out.extend(
                self.global_tree
                    .locate_within_distance(center_arr, radius_sq)
                    .map(|e| e.entity),
            );
            return;
        }

        let min_chunk = self.chunk_key(min.to_array());
        let max_chunk = self.chunk_key(max.to_array());

        let min_chunk_ext = min_chunk - IVec3::ONE;
        let max_chunk_ext = max_chunk + IVec3::ONE;

        let candidate_chunks = candidate_chunk_count(min_chunk_ext, max_chunk_ext);
        let threshold = 64_i64.max(self.trees.len() as i64);

        if candidate_chunks > threshold {
            for tree in self.trees.values() {
                out.extend(
                    tree.locate_within_distance(center_arr, radius_sq)
                        .map(|e| e.entity),
                );
            }
        } else {
            for x in min_chunk_ext.x..=max_chunk_ext.x {
                for y in min_chunk_ext.y..=max_chunk_ext.y {
                    for z in min_chunk_ext.z..=max_chunk_ext.z {
                        if let Some(tree) = self.trees.get(&IVec3::new(x, y, z)) {
                            out.extend(
                                tree.locate_within_distance(center_arr, radius_sq)
                                    .map(|e| e.entity),
                            );
                        }
                    }
                }
            }
        }

        out.extend(
            self.global_tree
                .locate_within_distance(center_arr, radius_sq)
                .map(|e| e.entity),
        );
    }

    /// 查询所有已索引实体。
    ///
    /// 该接口会分配新的 `Vec`。
    /// 如果调用方已有缓冲区，建议使用 `query_all_into`。
    pub fn query_all(&self) -> Vec<Entity> {
        let mut out = Vec::new();
        self.query_all_into(&mut out);
        out
    }

    /// 将所有已索引实体追加到已有缓冲区。
    ///
    /// 该方法不会清空传入的缓冲区。
    pub fn query_all_into(&self, out: &mut Vec<Entity>) {
        for tree in self.trees.values() {
            out.extend(tree.iter().map(|e| e.entity));
        }
        out.extend(self.global_tree.iter().map(|e| e.entity));
    }

    /// 计算一个圆形观察范围覆盖的区块列表。
    ///
    /// 如果覆盖区块数量超过 `limit`，返回 `None`。
    /// 调用方通常应将该观察者视为全局观察者，避免维护过大的区块反向索引。
    pub fn chunks_in_radius_limited(
        &self,
        center: Vec3,
        radius: f32,
        limit: usize,
    ) -> Option<Vec<IVec3>> {
        if !center.is_finite() || radius.is_nan() || radius < 0.0 {
            return Some(Vec::new());
        }

        if radius.is_infinite() {
            return None;
        }

        let half = Vec3::splat(radius);
        let min = center - half;
        let max = center + half;

        if !min.is_finite() || !max.is_finite() {
            return None;
        }

        let min_chunk = self.chunk_key(min.to_array()) - IVec3::ONE;
        let max_chunk = self.chunk_key(max.to_array()) + IVec3::ONE;

        let count = candidate_chunk_count(min_chunk, max_chunk);
        if count > limit as i64 {
            return None;
        }

        let mut chunks = Vec::with_capacity(count.max(0) as usize);

        for x in min_chunk.x..=max_chunk.x {
            for y in min_chunk.y..=max_chunk.y {
                for z in min_chunk.z..=max_chunk.z {
                    chunks.push(IVec3::new(x, y, z));
                }
            }
        }

        Some(chunks)
    }
}

/// 计算区块范围包含的区块数量。
///
/// 该函数使用饱和乘法，避免极端范围导致整数溢出。
fn candidate_chunk_count(min_chunk: IVec3, max_chunk: IVec3) -> i64 {
    let x = (max_chunk.x as i64 - min_chunk.x as i64 + 1).max(0);
    let y = (max_chunk.y as i64 - min_chunk.y as i64 + 1).max(0);
    let z = (max_chunk.z as i64 - min_chunk.z as i64 + 1).max(0);
    x.saturating_mul(y).saturating_mul(z)
}

/// 标记实体的空间尺寸。
///
/// `None` 表示点实体。
///
/// `Some([width, height, depth])` 表示以锚点（`GlobalTransform` 的世界位置）为中心的 AABB。
/// 注意：这里传入的是完整尺寸，不是半长；内部会自动乘以 0.5。
#[derive(Component)]
pub struct Size(pub Option<[f32; 3]>);

impl SpatialAnchor for GlobalTransform {
    fn anchor_point(&self) -> Vec3 {
        self.translation()
    }
}

impl SpatialExtent for Size {
    fn extents(&self) -> Option<[f32; 3]> {
        self.0
    }
}

/// 从空间索引中移除一个实体，并标记对应区块或全局状态为脏。
fn remove_spatial_entity(index: &mut SpatialIndex, changes: &mut SpatialChanges, entity: Entity) {
    let Some((shape, chunk_opt)) = index.positions.remove(&entity) else {
        return;
    };

    match chunk_opt {
        Some(chunk) => {
            changes.dirty_chunks.insert(chunk);

            if let Some(tree) = index.trees.get_mut(&chunk) {
                let _ = tree.remove(&SpatialEntity {
                    entity,
                    position: shape,
                });

                if tree.size() == 0 {
                    index.trees.remove(&chunk);
                }
            }
        }
        None => {
            changes.global_dirty = true;

            let _ = index.global_tree.remove(&SpatialEntity {
                entity,
                position: shape,
            });
        }
    }
}

/// 自动更新空间索引，并记录脏区块。
///
/// 该系统监听 `GlobalTransform` 和 `Size` 的变化。
/// 当实体位置或尺寸发生变化时，它会先移除旧的空间记录，再插入新的空间记录。
///
/// 无论实体是否跨越区块，只要空间状态发生变化，对应区块都会被标记为脏。
/// 这样可以保证 AOI 系统能检测到同一区块内部的进入/离开变化。
pub fn update_spatial_index<A: SpatialAnchor, X: SpatialExtent>(
    mut index: ResMut<SpatialIndex>,
    mut changes: ResMut<SpatialChanges>,
    query: Query<(Entity, &A, &X), Or<(Changed<A>, Changed<X>)>>,
) {
    let chunk_size = index.chunk_size;

    for (entity, anchor, extent) in query.iter() {
        let pos = anchor.anchor_point();

        if !pos.is_finite() {
            // 坐标非法时，必须移除旧记录，否则会留下幽灵实体。
            remove_spatial_entity(&mut index, &mut changes, entity);
            continue;
        }

        let new_shape = match extent.extents() {
            None => Shape::Point {
                pos: pos.to_array(),
            },
            Some(dim) => {
                let half = Vec3::from_array(dim.map(|v| {
                    if v.is_finite() && v > 0.0 {
                        v * 0.5
                    } else {
                        0.0
                    }
                }));

                Shape::Aabb {
                    aabb: Aabb3d::new(pos, half),
                }
            }
        };

        let is_oversized = match new_shape {
            Shape::Aabb { aabb } => {
                let size_vec = aabb.max - aabb.min;
                size_vec.x > chunk_size || size_vec.y > chunk_size || size_vec.z > chunk_size
            }
            _ => false,
        };
        // 先移除旧记录，并标记旧区块/全局状态为脏。
        remove_spatial_entity(&mut index, &mut changes, entity);

        let new_entry = SpatialEntity {
            entity,
            position: new_shape,
        };

        if is_oversized {
            changes.global_dirty = true;
            index.global_tree.insert(new_entry);
            index.positions.insert(entity, (new_shape, None));
        } else {
            let new_chunk = index.chunk_key(pos.to_array());

            // 即使新区块和旧区块相同，也必须标记为脏。
            changes.dirty_chunks.insert(new_chunk);

            let tree = index.trees.entry(new_chunk).or_default();
            tree.insert(new_entry);
            index.positions.insert(entity, (new_shape, Some(new_chunk)));
        }
    }
}

/// 清理被销毁或失去 `Size` 组件实体的空间索引条目，并记录脏区块。
pub fn cleanup_spatial_index<X: SpatialExtent>(
    mut index: ResMut<SpatialIndex>,
    mut removed: RemovedComponents<X>,
    mut changes: ResMut<SpatialChanges>,
) {
    for entity in removed.read() {
        remove_spatial_entity(&mut index, &mut changes, entity);
    }
}

/// 清理失去锚点组件（默认 `GlobalTransform`）但仍保留 `Size` 的实体。
///
/// 如果实体被销毁，`anchors.get(entity)` 会失败，此时也会清理索引。
/// 如果实体只是短暂移除锚点并在清理前重新加回，则跳过。
pub fn cleanup_missing_anchor<A: SpatialAnchor>(
    mut index: ResMut<SpatialIndex>,
    mut removed: RemovedComponents<A>,
    mut changes: ResMut<SpatialChanges>,
    anchors: Query<(), With<A>>,
) {
    for entity in removed.read() {
        if anchors.get(entity).is_ok() {
            continue;
        }

        remove_spatial_entity(&mut index, &mut changes, entity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::app::App;
    use bevy::math::Vec3;

    fn test_app() -> App {
        let mut app = App::new();
        app.add_plugins(SpatialPlugin::<GlobalTransform, Size>::new(10.0));
        app
    }

    #[test]
    fn test_point_and_aabb_query() {
        let mut app = test_app();

        let e1;
        let e2;

        {
            let world = app.world_mut();

            e1 = world
                .spawn((Transform::from_translation(Vec3::ZERO), Size(None)))
                .id();

            e2 = world
                .spawn((
                    Transform::from_translation(Vec3::new(5.0, 0.0, 0.0)),
                    Size(Some([2.0, 2.0, 2.0])),
                ))
                .id();
        }

        app.update();

        let index = app.world().resource::<SpatialIndex>();

        let aabb = Aabb3d::new(Vec3::ZERO, Vec3::splat(10.0));
        let mut res = Vec::new();
        index.query_aabb_into(aabb, &mut res);

        assert!(res.contains(&e1));
        assert!(res.contains(&e2));

        let mut res_rad = Vec::new();
        index.query_radius_into(Vec3::ZERO, 1.0, &mut res_rad);

        assert!(res_rad.contains(&e1));
        assert!(!res_rad.contains(&e2));
    }

    #[test]
    fn test_oversized_entity_global_tree() {
        let mut app = test_app();

        {
            let world = app.world_mut();
            world.spawn((
                Transform::from_translation(Vec3::ZERO),
                Size(Some([20.0, 20.0, 20.0])),
            ));
        }

        app.update();

        let index = app.world().resource::<SpatialIndex>();

        assert_eq!(index.trees.len(), 0);
        assert_eq!(index.global_tree.size(), 1);
    }

    #[test]
    fn test_chunk_boundary_dirty_tracking() {
        let mut app = App::new();
        app.add_plugins(SpatialPlugin::<GlobalTransform, Size>::new(10.0));

        let entity;
        {
            let world = app.world_mut();
            entity = world
                .spawn((
                    Transform::from_translation(Vec3::new(9.0, 0.0, 0.0)),
                    Size(None),
                ))
                .id();
        }

        app.update();

        app.world_mut().resource_mut::<SpatialChanges>().clear();
        app.world_mut()
            .entity_mut(entity)
            .insert(Transform::from_translation(Vec3::new(11.0, 0.0, 0.0)));

        app.update();

        let changes = app.world().resource::<SpatialChanges>();

        assert!(
            changes.dirty_chunks.contains(&IVec3::new(0, 0, 0)),
            "旧区块必须被标记为脏"
        );
        assert!(
            changes.dirty_chunks.contains(&IVec3::new(1, 0, 0)),
            "新区块必须被标记为脏"
        );
    }

    #[test]
    fn test_same_chunk_movement_marks_dirty() {
        let mut app = App::new();
        app.add_plugins(SpatialPlugin::<GlobalTransform, Size>::new(10.0));

        let entity;
        {
            let world = app.world_mut();
            entity = world
                .spawn((
                    Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)),
                    Size(None),
                ))
                .id();
        }

        app.update();

        app.world_mut().resource_mut::<SpatialChanges>().clear();

        app.world_mut()
            .entity_mut(entity)
            .insert(Transform::from_translation(Vec3::new(3.0, 0.0, 0.0)));

        app.update();

        let changes = app.world().resource::<SpatialChanges>();

        assert!(
            changes.dirty_chunks.contains(&IVec3::ZERO),
            "同区块内移动也必须标记区块为脏"
        );
    }

    #[test]
    fn test_non_finite_transform_removes_entity() {
        let mut app = App::new();
        app.add_plugins(SpatialPlugin::<GlobalTransform, Size>::new(10.0));

        let entity;
        {
            let world = app.world_mut();
            entity = world
                .spawn((
                    Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)),
                    Size(None),
                ))
                .id();
        }

        app.update();
        assert_eq!(app.world().resource::<SpatialIndex>().query_all().len(), 1);

        app.world_mut().resource_mut::<SpatialChanges>().clear();

        app.world_mut()
            .entity_mut(entity)
            .insert(Transform::from_translation(Vec3::new(f32::NAN, 0.0, 0.0)));

        app.update();

        let index = app.world().resource::<SpatialIndex>();
        let changes = app.world().resource::<SpatialChanges>();

        assert_eq!(index.query_all().len(), 0, "非有限坐标实体应从索引中移除");
        assert!(
            changes.dirty_chunks.contains(&IVec3::ZERO),
            "移除非有限坐标实体时应标记原区块为脏"
        );
    }

    #[test]
    fn test_global_transform_removal_cleanup() {
        let mut app = App::new();
        app.add_plugins(SpatialPlugin::<GlobalTransform, Size>::new(10.0));

        let entity;
        {
            let world = app.world_mut();
            entity = world
                .spawn((
                    Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)),
                    Size(None),
                ))
                .id();
        }

        app.update();
        assert_eq!(app.world().resource::<SpatialIndex>().query_all().len(), 1);

        app.world_mut().resource_mut::<SpatialChanges>().clear();

        app.world_mut()
            .entity_mut(entity)
            .remove::<GlobalTransform>();

        app.update();

        let index = app.world().resource::<SpatialIndex>();
        let changes = app.world().resource::<SpatialChanges>();

        assert_eq!(
            index.query_all().len(),
            0,
            "失去 GlobalTransform 的实体应从索引中移除"
        );
        assert!(
            changes.dirty_chunks.contains(&IVec3::ZERO),
            "失去 GlobalTransform 时应标记原区块为脏"
        );
    }

    #[test]
    fn test_size_mutation_transitions() {
        let mut app = App::new();
        app.add_plugins(SpatialPlugin::<GlobalTransform, Size>::new(10.0));

        let entity;
        {
            let world = app.world_mut();
            entity = world
                .spawn((
                    Transform::from_translation(Vec3::ZERO),
                    Size(Some([5.0, 5.0, 5.0])),
                ))
                .id();
        }

        app.update();

        {
            let index = app.world().resource::<SpatialIndex>();
            assert_eq!(index.global_tree.size(), 0);
            assert_eq!(index.trees.len(), 1);
        }

        app.world_mut()
            .entity_mut(entity)
            .insert(Size(Some([20.0, 20.0, 20.0])));

        app.update();

        {
            let index = app.world().resource::<SpatialIndex>();
            let changes = app.world().resource::<SpatialChanges>();

            assert_eq!(index.global_tree.size(), 1, "应移入全局树");
            assert_eq!(index.trees.len(), 0, "应离开局部树");
            assert!(changes.global_dirty, "应触发全局脏标记");
        }

        app.world_mut()
            .entity_mut(entity)
            .insert(Size(Some([2.0, 2.0, 2.0])));

        app.update();

        let index = app.world().resource::<SpatialIndex>();

        assert_eq!(index.global_tree.size(), 0, "应离开全局树");
        assert_eq!(index.trees.len(), 1, "应移回局部树");
    }

    #[test]
    fn test_component_removal_cleanup() {
        let mut app = App::new();
        app.add_plugins(SpatialPlugin::<GlobalTransform, Size>::new(10.0));

        let entity;
        {
            let world = app.world_mut();
            entity = world
                .spawn((Transform::from_translation(Vec3::ZERO), Size(None)))
                .id();
        }

        app.update();

        assert_eq!(app.world().resource::<SpatialIndex>().query_all().len(), 1);

        app.world_mut().entity_mut(entity).remove::<Size>();

        app.update();

        let index = app.world().resource::<SpatialIndex>();
        let changes = app.world().resource::<SpatialChanges>();

        assert_eq!(index.query_all().len(), 0, "实体应从索引中彻底清除");
        assert!(
            changes.dirty_chunks.contains(&IVec3::ZERO),
            "所在区块应被标记为脏"
        );
    }
}
