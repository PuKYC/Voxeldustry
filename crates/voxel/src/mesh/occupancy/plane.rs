//! Plane and direction tags plus the plane table.
//!
//! The integer tags are part of the contract with crate::mesh::RectInstance:
//! [`Plane::index`] and [`Dir::index`] are stored verbatim in the emitted
//! rectangles, and [`ExternalPlane`]'s discriminant order is the order of the
//! `external` array in crate::mesh::extract_block.

/// The three axis-aligned plane families.
///
/// * YZ  - normal along X, row = y, col = z
/// * XZ  - normal along Y, row = z, col = x
/// * XY  - normal along Z, row = y, col = x
///
/// The row/col roles match the upstream fill_masks_for_region index
/// arithmetic exactly, so they are part of the integer contract of
/// crate::mesh::RectInstance.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum Plane {
    /// Plane normal to X.
    YZ,
    /// Plane normal to Y.
    XZ,
    /// Plane normal to Z.
    XY,
}

impl Plane {
    /// Stable integer tag used by crate::mesh::RectInstance::plane.
    #[must_use]
    pub const fn index(self) -> u8 {
        match self {
            Plane::YZ => crate::mesh::PLANE_YZ,
            Plane::XZ => crate::mesh::PLANE_XZ,
            Plane::XY => crate::mesh::PLANE_XY,
        }
    }

    /// Base index (in units of one plane) into the global scratch vector.
    #[must_use]
    pub const fn plane_index(self) -> usize {
        match self {
            Plane::YZ => 0,
            Plane::XZ => 1,
            Plane::XY => 2,
        }
    }
}

/// Face orientation along the plane normal.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum Dir {
    /// Face on the positive side of the normal (+X / +Y / +Z).
    Pos,
    /// Face on the negative side of the normal (-X / -Y / -Z).
    Neg,
}

impl Dir {
    /// Stable integer tag used by crate::mesh::RectInstance::dir.
    #[must_use]
    pub const fn index(self) -> u8 {
        match self {
            Dir::Pos => crate::mesh::DIR_POS,
            Dir::Neg => crate::mesh::DIR_NEG,
        }
    }
}

/// The six block faces that can receive external (cross-body) occupancy.
///
/// The discriminant order is the public index order of the
/// external: [Option<&VoxTree<u8>>; 6] parameter of crate::mesh::extract_block:
/// [YZ+, YZ-, XZ+, XZ-, XY+, XY-].
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum ExternalPlane {
    /// Positive X face.
    YZPos,
    /// Negative X face.
    YZNeg,
    /// Positive Y face.
    XZPos,
    /// Negative Y face.
    XZNeg,
    /// Positive Z face.
    XYPos,
    /// Negative Z face.
    XYNeg,
}

impl ExternalPlane {
    /// All six planes in discriminant order.
    pub const ALL: [ExternalPlane; 6] = [
        ExternalPlane::YZPos,
        ExternalPlane::YZNeg,
        ExternalPlane::XZPos,
        ExternalPlane::XZNeg,
        ExternalPlane::XYPos,
        ExternalPlane::XYNeg,
    ];

    /// Converts a raw index in 0..6 into a plane.
    #[must_use]
    pub const fn from_index(index: usize) -> ExternalPlane {
        match index {
            0 => ExternalPlane::YZPos,
            1 => ExternalPlane::YZNeg,
            2 => ExternalPlane::XZPos,
            3 => ExternalPlane::XZNeg,
            4 => ExternalPlane::XYPos,
            _ => ExternalPlane::XYNeg,
        }
    }
}

/// 一个平面族的静态描述，用于把 (Plane, Dir) 换算成掩码下标。
///
/// 每族在 `global` 掩码向量里占 `n * n` 个 u64，起始偏移由
/// `offset_factor * n * n` 给出；两个外部面按 `pos` / `neg` 取出
/// `external` 里的 n 个掩码。
#[derive(Debug, Clone, Copy)]
pub(crate) struct PlaneData {
    /// 平面族。
    pub(crate) plane: Plane,
    /// 该族全局掩码的基址系数（0 / 1 / 2，乘以 n * n 得到起始下标）。
    pub(crate) offset_factor: usize,
    /// 该族在两个激活区间里的起始下标（YZ = 0、XZ = 2、XY = 4）。
    pub(crate) global_active_idx: usize,
    /// 正法线方向对应的外部面。
    pub(crate) pos: ExternalPlane,
    /// 负法线方向对应的外部面。
    pub(crate) neg: ExternalPlane,
}

/// 三个平面族按 YZ / XZ / XY 排列的静态表。
///
/// 索引与 `Plane::plane_index()` 一致，greedy 与 incremental 都按这个顺序
/// 遍历平面，改动顺序会改变矩形输出顺序。
pub(crate) const PLANES: [PlaneData; 3] = [
    PlaneData {
        plane: Plane::YZ,
        offset_factor: 0,
        global_active_idx: 0,
        pos: ExternalPlane::YZPos,
        neg: ExternalPlane::YZNeg,
    },
    PlaneData {
        plane: Plane::XZ,
        offset_factor: 1,
        global_active_idx: 2,
        pos: ExternalPlane::XZPos,
        neg: ExternalPlane::XZNeg,
    },
    PlaneData {
        plane: Plane::XY,
        offset_factor: 2,
        global_active_idx: 4,
        pos: ExternalPlane::XYPos,
        neg: ExternalPlane::XYNeg,
    },
];

/// Splits an ExternalPlane into its plane family and direction.
#[must_use]
#[inline(always)]
pub const fn extract_plane_dir(external_plane: ExternalPlane) -> (Plane, Dir) {
    match external_plane {
        ExternalPlane::YZPos => (Plane::YZ, Dir::Pos),
        ExternalPlane::YZNeg => (Plane::YZ, Dir::Neg),
        ExternalPlane::XZPos => (Plane::XZ, Dir::Pos),
        ExternalPlane::XZNeg => (Plane::XZ, Dir::Neg),
        ExternalPlane::XYPos => (Plane::XY, Dir::Pos),
        ExternalPlane::XYNeg => (Plane::XY, Dir::Neg),
    }
}
