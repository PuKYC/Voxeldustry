//! 2D 轴对齐包围盒（移植自 vendored spatial/aabb2d.rs）。
//!
//! 注意：这是纯几何查询辅助，使用 f32 glam 类型；它不在任何产生字节 /
//! 哈希的路径上（例外仅限“立即量化的 mesh scratch”，本类型
//! 不参与网格输出）。

use glam::Vec2;

/// 2D 轴对齐包围盒。
#[derive(Debug, Clone, Copy)]
pub struct Aabb2d {
    pub min: Vec2,
    pub max: Vec2,
}

impl Aabb2d {
    pub const fn with_min_max(min: Vec2, max: Vec2) -> Self {
        Self { min, max }
    }

    pub fn with_position_and_size(position: Vec2, size: Vec2) -> Self {
        Self {
            min: position,
            max: position + size,
        }
    }

    pub fn union(&self, other: &Self) -> Self {
        Self {
            min: self.min.min(other.min),
            max: self.max.max(other.max),
        }
    }

    pub fn size(&self) -> Vec2 {
        self.max - self.min
    }

    pub const fn contains(&self, point: Vec2) -> bool {
        point.x >= self.min.x
            && point.x <= self.max.x
            && point.y >= self.min.y
            && point.y <= self.max.y
    }

    pub const fn intersects(&self, other: &Self) -> bool {
        self.min.x <= other.max.x
            && self.max.x >= other.min.x
            && self.min.y <= other.max.y
            && self.max.y >= other.min.y
    }
}
