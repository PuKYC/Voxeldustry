//! 整数体素 AABB 与射线 / 重叠查询。
//!
//! 输入全是整数体素坐标；只有射线参数在边界处用 FixedPoint 表达，不用 f32。

use crate::math::FixedPoint;

/// 轴对齐整数体素盒，半开区间 [min, max)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VoxelAabb {
    pub min: [i32; 3],
    pub max: [i32; 3],
}

impl VoxelAabb {
    #[inline]
    pub const fn new(min: [i32; 3], max: [i32; 3]) -> Self {
        Self { min, max }
    }

    #[inline]
    pub fn from_min_size(min: [i32; 3], size: [i32; 3]) -> Self {
        Self {
            min,
            max: [min[0] + size[0], min[1] + size[1], min[2] + size[2]],
        }
    }

    #[inline]
    pub fn size(&self) -> [i32; 3] {
        [
            self.max[0] - self.min[0],
            self.max[1] - self.min[1],
            self.max[2] - self.min[2],
        ]
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.max[0] <= self.min[0] || self.max[1] <= self.min[1] || self.max[2] <= self.min[2]
    }

    #[inline]
    pub fn contains(&self, p: [i32; 3]) -> bool {
        p[0] >= self.min[0]
            && p[0] < self.max[0]
            && p[1] >= self.min[1]
            && p[1] < self.max[1]
            && p[2] >= self.min[2]
            && p[2] < self.max[2]
    }

    #[inline]
    pub fn intersects(&self, other: &Self) -> bool {
        self.min[0] < other.max[0]
            && other.min[0] < self.max[0]
            && self.min[1] < other.max[1]
            && other.min[1] < self.max[1]
            && self.min[2] < other.max[2]
            && other.min[2] < self.max[2]
    }

    pub fn intersection(&self, other: &Self) -> Option<Self> {
        let out = Self {
            min: [
                self.min[0].max(other.min[0]),
                self.min[1].max(other.min[1]),
                self.min[2].max(other.min[2]),
            ],
            max: [
                self.max[0].min(other.max[0]),
                self.max[1].min(other.max[1]),
                self.max[2].min(other.max[2]),
            ],
        };
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    pub fn union(&self, other: &Self) -> Self {
        Self {
            min: [
                self.min[0].min(other.min[0]),
                self.min[1].min(other.min[1]),
                self.min[2].min(other.min[2]),
            ],
            max: [
                self.max[0].max(other.max[0]),
                self.max[1].max(other.max[1]),
                self.max[2].max(other.max[2]),
            ],
        }
    }

    /// 射线与 AABB 的最近正向交点参数 t（origin + t * dir），无交返回 None。
    ///
    /// 用 slab 方法。输入全是整数；仅在边界处用 FixedPoint 计算 t。
    pub fn ray_intersection(&self, origin: [i32; 3], dir: [i32; 3]) -> Option<FixedPoint> {
        let zero = FixedPoint::from_num(0);
        let mut tmin = zero;
        let mut tmax: Option<FixedPoint> = None;

        for axis in 0..3usize {
            let o = FixedPoint::from_num(origin[axis]);
            let d = FixedPoint::from_num(dir[axis]);
            let lo = FixedPoint::from_num(self.min[axis]);
            let hi = FixedPoint::from_num(self.max[axis]);

            if d == zero {
                if o < lo || o >= hi {
                    return None;
                }
                continue;
            }

            let mut t1 = (lo - o) / d;
            let mut t2 = (hi - o) / d;
            if t1 > t2 {
                std::mem::swap(&mut t1, &mut t2);
            }
            if t1 > tmin {
                tmin = t1;
            }
            tmax = Some(match tmax {
                None => t2,
                Some(t) => {
                    if t2 < t {
                        t2
                    } else {
                        t
                    }
                }
            });
            if let Some(t) = tmax {
                if tmin > t {
                    return None;
                }
            }
        }

        Some(tmin)
    }

    #[inline]
    pub fn ray_intersects(&self, origin: [i32; 3], dir: [i32; 3]) -> bool {
        self.ray_intersection(origin, dir).is_some()
    }
}

/// 子块在体素空间中的整数包围盒（「子块级剔除」）。
///
/// `min = chunk_key * 32`，`max = min + 32`（体素坐标，轴对齐，半开区间）。
#[must_use]
pub fn chunk_aabb(key: ::voxel::ChunkKey) -> VoxelAabb {
    let s = super::key::chunk_voxels_per_axis();
    VoxelAabb::from_min_size([key.x * s, key.y * s, key.z * s], [s, s, s])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::FixedPoint;

    #[test]
    fn chunk_aabb_matches_chunk_bounding_box() {
        let a = chunk_aabb(::voxel::ChunkKey::new(2, -1, 0));
        let s = super::super::key::chunk_voxels_per_axis();
        assert_eq!(a.min, [2 * s, -1 * s, 0]);
        assert_eq!(a.size(), [s, s, s]);
        assert!(a.contains([2 * s, -s, 0]));
        assert!(!a.contains([2 * s - 1, -s, 0]));
        assert!(!a.contains([2 * s, -s, s]));
    }

    #[test]
    fn overlap_basics() {
        let a = VoxelAabb::new([0, 0, 0], [4, 4, 4]);
        assert!(a.contains([0, 0, 0]));
        assert!(a.contains([3, 3, 3]));
        assert!(!a.contains([4, 0, 0]));

        assert!(a.intersects(&VoxelAabb::new([3, 3, 3], [5, 5, 5])));
        assert!(!a.intersects(&VoxelAabb::new([4, 0, 0], [5, 1, 1])));

        assert_eq!(
            a.intersection(&VoxelAabb::new([2, 2, 2], [6, 6, 6])),
            Some(VoxelAabb::new([2, 2, 2], [4, 4, 4]))
        );
        assert_eq!(
            a.intersection(&VoxelAabb::new([9, 9, 9], [10, 10, 10])),
            None
        );
        assert_eq!(
            a.union(&VoxelAabb::new([8, 8, 8], [9, 9, 9])),
            VoxelAabb::new([0, 0, 0], [9, 9, 9])
        );
    }

    #[test]
    fn ray_basics() {
        let a = VoxelAabb::new([0, 0, 0], [4, 4, 4]);

        // 从 -x 射向 +x，命中 x=0 面 -> t=10。
        assert_eq!(
            a.ray_intersection([-10, 2, 2], [1, 0, 0]),
            Some(FixedPoint::from_num(10))
        );
        // 背离 -> 无交。
        assert_eq!(a.ray_intersection([10, 2, 2], [1, 0, 0]), None);
        // 平行且在范围外 -> 无交。
        assert_eq!(a.ray_intersection([-10, 9, 2], [1, 0, 0]), None);
        // 起点在盒内 -> t=0。
        assert_eq!(
            a.ray_intersection([2, 2, 2], [0, 1, 0]),
            Some(FixedPoint::from_num(0))
        );
        assert!(a.ray_intersects([-1, 2, 2], [1, 0, 0]));
        assert!(!a.ray_intersects([-1, 9, 2], [1, 0, 0]));
    }
}
