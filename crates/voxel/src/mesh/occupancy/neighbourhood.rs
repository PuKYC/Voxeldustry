//! The 26-neighbour sampler used for per-vertex ambient occlusion.
//!
//! A meshed block samples the voxels just outside each of its faces, edges and
//! corners; [`Neighbourhood`] owns those neighbour trees so the AO pass can
//! read them without knowing how the halo is produced.

use crate::store::VoxTree;

/// The 26 neighbours of a meshed block, indexed by signed axis triple.
///
/// Slot index is `(dx + 1) + 3 * (dy + 1) + 9 * (dz + 1)` for every
/// `(dx, dy, dz)` in `{-1, 0, 1}^3`; the centre `(0, 0, 0)` is the meshed
/// block itself, so slot 13 is always `None`.  A `None` neighbour reads as
/// air (missing block).
///
/// The raw-halo producer (`extract_raw_halo`) walks the halo shell one cell at
/// a time, maps each cell to the neighbour that owns it and samples that
/// neighbour directly, so no per-edge / per-corner mask table is needed.
#[derive(Clone, Copy, Default)]
pub struct Neighbourhood<'a> {
    slots: [Option<&'a VoxTree<u8>>; 27],
}

/// Slot index of a sign triple in [`Neighbourhood`]'s 3x3x3 layout.
#[must_use]
#[inline(always)]
pub const fn neighbour_slot(dx: i8, dy: i8, dz: i8) -> usize {
    (dx + 1) as usize + 3 * ((dy + 1) as usize) + 9 * ((dz + 1) as usize)
}

/// Sign triple of the face at `face` in [`crate::mesh::occupancy::ExternalPlane`] order
/// `[+x, -x, +y, -y, +z, -z]`.
#[must_use]
#[inline(always)]
pub const fn face_sign(face: usize) -> (i8, i8, i8) {
    match face {
        0 => (1, 0, 0),
        1 => (-1, 0, 0),
        2 => (0, 1, 0),
        3 => (0, -1, 0),
        4 => (0, 0, 1),
        _ => (0, 0, -1),
    }
}

impl<'a> Neighbourhood<'a> {
    /// All 26 neighbours absent (read as air).
    #[must_use]
    pub const fn none() -> Self {
        Self { slots: [None; 27] }
    }

    /// Builds a neighbourhood from the six face neighbours only.
    ///
    /// `faces` is in [`crate::mesh::occupancy::ExternalPlane`] order `[+x, -x, +y, -y, +z, -z]`; the
    /// twelve edge and eight corner slots stay absent, which reproduces the
    /// face-only AO of the tree and incremental paths.
    #[must_use]
    pub fn from_faces(faces: [Option<&'a VoxTree<u8>>; 6]) -> Self {
        let mut out = Self::none();
        for (face, tree) in faces.into_iter().enumerate() {
            if let Some(tree) = tree {
                let (dx, dy, dz) = face_sign(face);
                out.slots[neighbour_slot(dx, dy, dz)] = Some(tree);
            }
        }
        out
    }

    /// Reads one neighbour by sign triple.
    #[must_use]
    #[inline(always)]
    pub const fn get(&self, dx: i8, dy: i8, dz: i8) -> Option<&'a VoxTree<u8>> {
        self.slots[neighbour_slot(dx, dy, dz)]
    }

    /// Stores one neighbour by sign triple.
    #[inline(always)]
    pub fn set(&mut self, dx: i8, dy: i8, dz: i8, tree: &'a VoxTree<u8>) {
        self.slots[neighbour_slot(dx, dy, dz)] = Some(tree);
    }

    /// The six face neighbours in [`crate::mesh::occupancy::ExternalPlane`] order.
    #[must_use]
    pub fn faces(&self) -> [Option<&'a VoxTree<u8>>; 6] {
        let mut out = [None; 6];
        for (face, slot) in out.iter_mut().enumerate() {
            let (dx, dy, dz) = face_sign(face);
            *slot = self.get(dx, dy, dz);
        }
        out
    }
}

/// 单个面方向上的邻居。
///
/// 与 [`Neighbourhood`] 的分工：`Neighbourhood` 按 26 邻逐格提供 AO 的
/// 面 / 棱 / 角采样；本枚举只描述 6 个面，供外部面掩码（剔除）和面 AO 使用。
///
/// **只看同 LOD**：跨 LOD 面一律报 [`FaceNeighbor::Missing`]（按空气处理），
/// 每块自己封边（godot_voxel 的块自闭合）。相邻 LOD 的可见表现是台阶墙，
/// 不是穿透洞；网格侧裙边只在面外残留实心 halo 时作为安全网生效（当前
/// extract 路径下是 no-op）。不靠跨 LOD 剔除 —— 那条路径会产生「两侧都剔」
/// 的穿透洞。
#[derive(Clone, Copy)]
pub enum FaceNeighbor<'a> {
    /// 面外无块（按空气处理，不遮挡）。
    Missing,
    /// 同 LOD 邻居。
    Same(&'a VoxTree<u8>),
}

impl<'a> FaceNeighbor<'a> {
    /// 同 LOD 面邻居（`None` = 缺失）。
    #[must_use]
    pub fn from_optional(tree: Option<&'a VoxTree<u8>>) -> Self {
        match tree {
            Some(tree) => FaceNeighbor::Same(tree),
            None => FaceNeighbor::Missing,
        }
    }
}
