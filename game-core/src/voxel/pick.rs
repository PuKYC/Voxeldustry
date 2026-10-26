//! 体素射线拾取（DDA）。
//!
//! 输入一条**世界坐标**射线（起点 + 单位方向）与最大距离，输出：
//! 命中的 body（实体）、body 局部整数体素坐标、局部轴对齐命中面法线、世界距离。
//!
//! ## 坐标约定
//!
//! - `VoxVolume` 的 chunk / `VoxTree` 都使用 **body 局部整数体素坐标**：
//!   `voxel = ChunkKey * 32 + local`，允许负数（见 `ChunkKey::div_euclid`）。
//! - 射线先用 body 的 [`GlobalTransform`] 逆变换到局部米制空间，再除以
//!   `voxel_size` 变成体素空间；因此平移 / 旋转 / 均匀缩放都成立。
//! - 返回的 `normal` 是**局部**单位轴（进入面朝外的方向），放置时直接
//!   `voxel + normal` 就是目标格，不需要再做旋转。
//! - `distance` 是沿输入单位方向的世界距离（米）。
//!
//! ## 多 body
//!
//! [`pick_nearest`] 对每个 body 各跑一次 DDA，取距离最近者；距离相同按
//! `Entity` 的位模式升序决定，保证不依赖 Query 迭代顺序。
//!
//! ## DDA
//!
//! 经典 Amanatides & Woo 步进：按射线与体素边界的三个 `tMax` 取最小轴前进，
//! 进入新格时法线 = 该轴的负步进方向。射线起点已经在实心格内时按
//! `t = 0` 命中，法线取主方向的反向轴，保证放置目标至少离开当前格。

use bevy::prelude::*;
use game_engine::voxel::{chunk_voxels_per_axis, ChunkKey, VoxInterner, VoxOpsRead, VoxVolume};

/// 拾取射线的默认最大距离（米）。
pub const DEFAULT_PICK_DISTANCE_M: f32 = 6.0;

/// 方向长度小于该值视为退化射线。
const MIN_DIRECTION_LEN: f32 = 1e-6;

/// 命中距离比较容差：正好等于 `max_distance` 也算命中。
const DISTANCE_EPSILON: f32 = 1e-4;

/// DDA 步数上限，防止非有限输入导致死循环。
const MAX_DDA_STEPS: usize = 1 << 20;

/// 世界坐标射线（方向已归一化）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VoxelRay {
    pub origin: Vec3,
    pub direction: Vec3,
}

impl VoxelRay {
    /// 构造归一化射线；零长度 / 非有限输入返回 `None`。
    #[must_use]
    pub fn new(origin: Vec3, direction: Vec3) -> Option<Self> {
        let length = direction.length();
        if !length.is_finite() || length <= MIN_DIRECTION_LEN {
            return None;
        }
        Some(Self {
            origin,
            direction: direction / length,
        })
    }

    /// 从相机（`GlobalTransform`）取眼点 + 前方（Bevy 约定 -Z）。
    #[must_use]
    pub fn from_camera(camera: &GlobalTransform) -> Option<Self> {
        Self::new(camera.translation(), *camera.forward())
    }

    /// 从相机的本地 `Transform` 取射线。
    ///
    /// 相机实体在本项目里是顶层实体，且 `camera_system` 在 FixedUpdate 写的是
    /// `Transform`；直接读它可避免 `GlobalTransform` 晚一帧同步带来的瞄准滞后。
    #[must_use]
    pub fn from_transform(camera: &Transform) -> Option<Self> {
        Self::new(camera.translation, camera.rotation * Vec3::NEG_Z)
    }
}

/// 单个 body 上的命中结果（体素 / 法线均为 body 局部坐标）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VolumeHit {
    /// body 局部整数体素坐标。
    pub voxel: IVec3,
    /// body 局部轴对齐法线（命中面朝外）。
    pub normal: IVec3,
    /// 沿射线方向的世界距离（米）。
    pub distance: f32,
}

/// 多 body 的拾取结果。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VoxelPickHit {
    pub body: Entity,
    /// body 局部整数体素坐标。
    pub voxel: IVec3,
    /// body 局部轴对齐法线。
    pub normal: IVec3,
    /// 世界距离（米）。
    pub distance: f32,
}

/// 对单个 body 做 DDA。
///
/// `interner` 是未包装的 `voxel::store::VoxInterner<u8>`（引擎
/// `VoxelInterner::inner()`）。`max_distance` 是沿 `ray.direction` 的米数。
#[must_use]
pub fn raycast_volume(
    volume: &VoxVolume,
    interner: &VoxInterner<u8>,
    transform: &GlobalTransform,
    ray: VoxelRay,
    max_distance: f32,
) -> Option<VolumeHit> {
    if volume.is_empty()
        || !max_distance.is_finite()
        || max_distance < 0.0
        || !ray.origin.is_finite()
        || !ray.direction.is_finite()
    {
        return None;
    }

    let voxel_size = volume.voxel_size.to_num::<f32>();
    if !voxel_size.is_finite() || voxel_size <= 0.0 {
        return None;
    }

    let inverse = transform.affine().inverse();
    let local_origin = inverse.transform_point3(ray.origin) / voxel_size;
    let local_direction = inverse.transform_vector3(ray.direction) / voxel_size;

    if !local_origin.is_finite() || !local_direction.is_finite() {
        return None;
    }

    let mut voxel = local_origin.floor().as_ivec3();

    // 起点已在实心格内：立即命中，法线朝射线来向的主轴。
    if is_solid(volume, interner, voxel) {
        return Some(VolumeHit {
            voxel,
            normal: entry_normal(local_direction),
            distance: 0.0,
        });
    }

    let mut step = IVec3::ZERO;
    let mut t_max = [f32::INFINITY; 3];
    let mut t_delta = [f32::INFINITY; 3];

    for axis in 0..3 {
        let d = local_direction[axis];
        let origin = local_origin[axis];
        if d > 0.0 {
            step[axis] = 1;
            t_max[axis] = ((voxel[axis] + 1) as f32 - origin) / d;
            t_delta[axis] = 1.0 / d;
        } else if d < 0.0 {
            step[axis] = -1;
            t_max[axis] = (voxel[axis] as f32 - origin) / d;
            t_delta[axis] = -1.0 / d;
        }
    }

    for _ in 0..MAX_DDA_STEPS {
        // 固定 x -> y -> z 的平局顺序，避免依赖浮点比较的偶然结果。
        let axis = if t_max[0] <= t_max[1] && t_max[0] <= t_max[2] {
            0
        } else if t_max[1] <= t_max[2] {
            1
        } else {
            2
        };

        let distance = t_max[axis];
        if !distance.is_finite() || distance > max_distance + DISTANCE_EPSILON {
            return None;
        }

        voxel[axis] += step[axis];
        t_max[axis] += t_delta[axis];

        if is_solid(volume, interner, voxel) {
            let mut normal = IVec3::ZERO;
            normal[axis] = -step[axis];
            return Some(VolumeHit {
                voxel,
                normal,
                distance: distance.max(0.0),
            });
        }
    }

    None
}

/// 对多个 body 取最近命中。
///
/// 调用方通常直接传 Query 的 `(Entity, &VoxVolume, &GlobalTransform)` 迭代器。
#[must_use]
pub fn pick_nearest<'a>(
    interner: &VoxInterner<u8>,
    ray: VoxelRay,
    max_distance: f32,
    bodies: impl IntoIterator<Item = (Entity, &'a VoxVolume, &'a GlobalTransform)>,
) -> Option<VoxelPickHit> {
    let mut best: Option<VoxelPickHit> = None;

    for (entity, volume, transform) in bodies {
        let Some(hit) = raycast_volume(volume, interner, transform, ray, max_distance) else {
            continue;
        };
        let candidate = VoxelPickHit {
            body: entity,
            voxel: hit.voxel,
            normal: hit.normal,
            distance: hit.distance,
        };
        let replace = match best {
            None => true,
            Some(current) => {
                (candidate.distance, candidate.body.to_bits())
                    < (current.distance, current.body.to_bits())
            }
        };
        if replace {
            best = Some(candidate);
        }
    }

    best
}

/// 把 body 局部体素坐标映射到 chunk / 块内坐标后读树。
fn is_solid(volume: &VoxVolume, interner: &VoxInterner<u8>, voxel: IVec3) -> bool {
    let axis = chunk_voxels_per_axis();
    let key = ChunkKey::new(
        voxel.x.div_euclid(axis),
        voxel.y.div_euclid(axis),
        voxel.z.div_euclid(axis),
    );
    let local = IVec3::new(
        voxel.x.rem_euclid(axis),
        voxel.y.rem_euclid(axis),
        voxel.z.rem_euclid(axis),
    );
    volume
        .get_chunk(&key)
        .and_then(|tree| tree.get(interner, local))
        .is_some_and(|value| value != u8::default())
}

/// 起点在实心格内时，取射线主方向的反向轴作为进入面法线。
fn entry_normal(direction: Vec3) -> IVec3 {
    let abs = direction.abs();
    let axis = if abs.x >= abs.y && abs.x >= abs.z {
        0
    } else if abs.y >= abs.z {
        1
    } else {
        2
    };
    let mut normal = IVec3::ZERO;
    normal[axis] = if direction[axis] >= 0.0 { -1 } else { 1 };
    normal
}

#[cfg(test)]
mod tests {
    use super::*;
    use game_engine::math::FixedPoint;
    use game_engine::voxel::{MaxDepth, VoxOpsWrite, VoxTree, CHUNK_DEPTH};
    use std::f32::consts::FRAC_PI_2;

    fn test_interner() -> VoxInterner<u8> {
        VoxInterner::with_memory_budget(4 * 1024 * 1024)
    }

    fn insert_voxel(volume: &mut VoxVolume, interner: &mut VoxInterner<u8>, point: IVec3) {
        let axis = chunk_voxels_per_axis();
        let key = ChunkKey::new(
            point.x.div_euclid(axis),
            point.y.div_euclid(axis),
            point.z.div_euclid(axis),
        );
        let local = IVec3::new(
            point.x.rem_euclid(axis),
            point.y.rem_euclid(axis),
            point.z.rem_euclid(axis),
        );
        if volume.get_chunk(&key).is_none() {
            volume.insert_chunk(key, VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH)));
        }
        let _ = volume
            .get_chunk_mut(&key)
            .expect("chunk just inserted")
            .set(interner, local, 1);
    }

    fn volume_with_voxels(points: &[IVec3]) -> (VoxVolume, VoxInterner<u8>) {
        let mut interner = test_interner();
        let mut volume = VoxVolume::new(FixedPoint::from_num(1));
        for point in points {
            insert_voxel(&mut volume, &mut interner, *point);
        }
        (volume, interner)
    }

    fn identity() -> GlobalTransform {
        GlobalTransform::IDENTITY
    }

    fn ray(origin: [f32; 3], direction: [f32; 3]) -> VoxelRay {
        VoxelRay::new(Vec3::from(origin), Vec3::from(direction)).expect("valid ray")
    }

    #[test]
    fn front_face_hit_reports_voxel_and_normal() {
        let (volume, interner) = volume_with_voxels(&[IVec3::new(2, 2, 2)]);
        let hit = raycast_volume(
            &volume,
            &interner,
            &identity(),
            ray([2.5, 2.5, 0.5], [0.0, 0.0, 1.0]),
            10.0,
        )
        .expect("must hit front face");
        assert_eq!(hit.voxel, IVec3::new(2, 2, 2));
        assert_eq!(hit.normal, IVec3::new(0, 0, -1));
        assert!(
            (hit.distance - 1.5).abs() < 1e-5,
            "distance={}",
            hit.distance
        );
    }

    #[test]
    fn side_face_hit_reports_minus_x_normal() {
        let (volume, interner) = volume_with_voxels(&[IVec3::new(2, 2, 2)]);
        let hit = raycast_volume(
            &volume,
            &interner,
            &identity(),
            ray([0.5, 2.5, 2.5], [1.0, 0.0, 0.0]),
            10.0,
        )
        .expect("must hit side face");
        assert_eq!(hit.voxel, IVec3::new(2, 2, 2));
        assert_eq!(hit.normal, IVec3::new(-1, 0, 0));
        assert!((hit.distance - 1.5).abs() < 1e-5);
    }

    #[test]
    fn grazing_boundary_hits_but_just_outside_misses() {
        let (volume, interner) = volume_with_voxels(&[IVec3::new(2, 2, 2)]);

        // z 恰好落在体素下边界：擦边命中同一格（d_z = 0）。
        let hit = raycast_volume(
            &volume,
            &interner,
            &identity(),
            ray([0.5, 2.5, 2.0], [1.0, 0.0, 0.0]),
            10.0,
        )
        .expect("grazing the boundary must still hit");
        assert_eq!(hit.voxel, IVec3::new(2, 2, 2));

        // 只差 0.001 格：必须完全错过。
        let miss = raycast_volume(
            &volume,
            &interner,
            &identity(),
            ray([0.5, 2.5, 1.999], [1.0, 0.0, 0.0]),
            10.0,
        );
        assert_eq!(miss, None, "just outside the face must miss");
    }

    #[test]
    fn max_distance_is_inclusive_but_further_is_miss() {
        let (volume, interner) = volume_with_voxels(&[IVec3::new(2, 2, 2)]);
        let origin = [0.5, 2.5, 2.5];
        let direction = [1.0, 0.0, 0.0];

        assert!(
            raycast_volume(&volume, &interner, &identity(), ray(origin, direction), 1.5).is_some(),
            "exact reach must hit"
        );
        assert_eq!(
            raycast_volume(
                &volume,
                &interner,
                &identity(),
                ray(origin, direction),
                1.499
            ),
            None,
            "out of range must miss"
        );
    }

    #[test]
    fn empty_world_and_away_ray_return_none() {
        let (volume, interner) = volume_with_voxels(&[IVec3::new(2, 2, 2)]);
        assert_eq!(
            raycast_volume(
                &volume,
                &interner,
                &identity(),
                ray([0.5, 0.5, 0.5], [0.0, 1.0, 0.0]),
                10.0
            ),
            None
        );

        let empty = VoxVolume::new(FixedPoint::from_num(1));
        assert_eq!(
            raycast_volume(
                &empty,
                &interner,
                &identity(),
                ray([0.5, 0.5, 0.5], [1.0, 0.0, 0.0]),
                10.0
            ),
            None
        );
    }

    #[test]
    fn nearest_of_multiple_bodies_wins_and_uses_body_local_voxel() {
        let mut interner = test_interner();
        let mut near = VoxVolume::new(FixedPoint::from_num(1));
        let mut far = VoxVolume::new(FixedPoint::from_num(1));
        insert_voxel(&mut near, &mut interner, IVec3::new(5, 0, 0));
        insert_voxel(&mut far, &mut interner, IVec3::new(2, 0, 0));

        let near_entity = Entity::from_bits(1);
        let far_entity = Entity::from_bits(2);
        let near_transform = GlobalTransform::IDENTITY;
        let far_transform = GlobalTransform::from_xyz(10.0, 0.0, 0.0);

        let hit = pick_nearest(
            &interner,
            ray([0.5, 0.5, 0.5], [1.0, 0.0, 0.0]),
            30.0,
            [
                (near_entity, &near, &near_transform),
                (far_entity, &far, &far_transform),
            ],
        )
        .expect("must hit the near body");
        assert_eq!(hit.body, near_entity);
        assert_eq!(hit.voxel, IVec3::new(5, 0, 0));
        assert!((hit.distance - 4.5).abs() < 1e-5);

        let miss = pick_nearest(
            &interner,
            ray([0.5, 0.5, 0.5], [1.0, 0.0, 0.0]),
            4.0,
            [
                (near_entity, &near, &near_transform),
                (far_entity, &far, &far_transform),
            ],
        );
        assert_eq!(miss, None, "neither body is within 4 m");
    }

    #[test]
    fn rotated_body_converts_ray_and_normal_to_local_space() {
        let (volume, interner) = volume_with_voxels(&[IVec3::new(0, 0, 0)]);
        let transform = GlobalTransform::from(
            Transform::from_xyz(10.0, 0.0, 0.0).with_rotation(Quat::from_rotation_y(FRAC_PI_2)),
        );

        // 旋转后局部 +X 指向世界 -Z，局部体素 (0,0,0) 的 +Z 面朝世界 +Z。
        let hit = raycast_volume(
            &volume,
            &interner,
            &transform,
            ray([10.5, 0.5, 10.0], [0.0, 0.0, -1.0]),
            20.0,
        )
        .expect("rotated body must be hit");
        assert_eq!(hit.voxel, IVec3::new(0, 0, 0));
        assert_eq!(hit.normal, IVec3::new(-1, 0, 0));
        assert!(
            (hit.distance - 10.0).abs() < 1e-4,
            "distance={}",
            hit.distance
        );
    }

    #[test]
    fn starting_inside_solid_returns_immediate_hit() {
        let (volume, interner) = volume_with_voxels(&[IVec3::new(2, 2, 2)]);
        let hit = raycast_volume(
            &volume,
            &interner,
            &identity(),
            ray([2.5, 2.5, 2.5], [1.0, 0.0, 0.0]),
            10.0,
        )
        .expect("starting inside must hit");
        assert_eq!(hit.distance, 0.0);
        assert_eq!(hit.voxel, IVec3::new(2, 2, 2));
        assert_eq!(hit.normal, IVec3::new(-1, 0, 0));
    }

    #[test]
    fn zero_direction_is_rejected() {
        assert!(VoxelRay::new(Vec3::ZERO, Vec3::ZERO).is_none());
        assert!(VoxelRay::new(Vec3::ZERO, Vec3::new(f32::NAN, 0.0, 0.0)).is_none());
    }
}
