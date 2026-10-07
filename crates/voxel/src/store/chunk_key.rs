//! 子块寻址：`ChunkKey`。
//!
//! 以 `CHUNK_DEPTH`（32 体素）为单位的整数子块坐标：
//! `global_voxel = chunk_key * 32 + local_xyz`。
//! `ChunkKey` 是**岛上局部坐标**，与世界位置解耦（浮岛会漂移）。

/// 子块坐标（以 32 体素为单位）。
///
/// `Copy + Ord` 是刻意为之：`BTreeMap<ChunkKey, _>` 的升序遍历天然确定，
/// 是导出 / 哈希 / 网格遍历的确定性基础。
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct ChunkKey {
    /// X 轴子块坐标。
    pub x: i32,
    /// Y 轴子块坐标。
    pub y: i32,
    /// Z 轴子块坐标。
    pub z: i32,
}

impl ChunkKey {
    /// 原点子块 `(0, 0, 0)`。
    pub const ZERO: Self = Self { x: 0, y: 0, z: 0 };

    /// 用三个分量构造一个子块坐标。
    #[must_use]
    #[inline(always)]
    pub const fn new(x: i32, y: i32, z: i32) -> Self {
        Self { x, y, z }
    }

    /// 用同一个值填充三个分量。
    #[must_use]
    #[inline(always)]
    pub const fn splat(v: i32) -> Self {
        Self { x: v, y: v, z: v }
    }

    /// 分量求和（用于哈希 / LOD 块寻址等）。
    #[must_use]
    #[inline(always)]
    pub const fn sum(self) -> i32 {
        self.x.wrapping_add(self.y).wrapping_add(self.z)
    }

    /// 逐分量加法。
    #[must_use]
    #[inline(always)]
    pub const fn add(self, other: Self) -> Self {
        Self {
            x: self.x + other.x,
            y: self.y + other.y,
            z: self.z + other.z,
        }
    }

    /// 逐分量减法。
    #[must_use]
    #[inline(always)]
    pub const fn sub(self, other: Self) -> Self {
        Self {
            x: self.x - other.x,
            y: self.y - other.y,
            z: self.z - other.z,
        }
    }

    /// 逐分量乘以标量。
    #[must_use]
    #[inline(always)]
    pub const fn mul(self, scalar: i32) -> Self {
        Self {
            x: self.x * scalar,
            y: self.y * scalar,
            z: self.z * scalar,
        }
    }

    /// 转为数组 `[x, y, z]`。
    #[must_use]
    #[inline(always)]
    pub const fn to_array(self) -> [i32; 3] {
        [self.x, self.y, self.z]
    }
}

impl std::ops::Add for ChunkKey {
    type Output = Self;
    #[inline(always)]
    fn add(self, rhs: Self) -> Self {
        ChunkKey::add(self, rhs)
    }
}

impl std::ops::Sub for ChunkKey {
    type Output = Self;
    #[inline(always)]
    fn sub(self, rhs: Self) -> Self {
        ChunkKey::sub(self, rhs)
    }
}

impl std::ops::Mul<i32> for ChunkKey {
    type Output = Self;
    #[inline(always)]
    fn mul(self, rhs: i32) -> Self {
        ChunkKey::mul(self, rhs)
    }
}

impl From<[i32; 3]> for ChunkKey {
    #[inline(always)]
    fn from(v: [i32; 3]) -> Self {
        Self::new(v[0], v[1], v[2])
    }
}

impl From<ChunkKey> for [i32; 3] {
    #[inline(always)]
    fn from(k: ChunkKey) -> Self {
        k.to_array()
    }
}

impl From<(i32, i32, i32)> for ChunkKey {
    #[inline(always)]
    fn from(v: (i32, i32, i32)) -> Self {
        Self::new(v.0, v.1, v.2)
    }
}

impl From<ChunkKey> for (i32, i32, i32) {
    #[inline(always)]
    fn from(k: ChunkKey) -> Self {
        (k.x, k.y, k.z)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_and_orders_are_stable() {
        let a = ChunkKey::new(1, -2, 3);
        assert_eq!(a.to_array(), [1, -2, 3]);
        assert_eq!(ChunkKey::from([1, -2, 3]), a);
        assert_eq!(ChunkKey::from((1, -2, 3)), a);
        assert_eq!(a + ChunkKey::ZERO, a);
        assert_eq!(a.add(ChunkKey::splat(1)).sub(ChunkKey::splat(1)), a);
        assert_eq!(a.mul(2).to_array(), [2, -4, 6]);
        assert_eq!(ChunkKey::default(), ChunkKey::ZERO);
    }

    #[test]
    fn ord_matches_coordinate_lexicographic() {
        let mut keys = vec![
            ChunkKey::new(0, 1, 0),
            ChunkKey::new(0, 0, 5),
            ChunkKey::new(-1, 0, 0),
            ChunkKey::new(0, 0, 4),
        ];
        keys.sort();
        assert_eq!(
            keys,
            vec![
                ChunkKey::new(-1, 0, 0),
                ChunkKey::new(0, 0, 4),
                ChunkKey::new(0, 0, 5),
                ChunkKey::new(0, 1, 0),
            ]
        );
    }
}
