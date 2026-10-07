//! Integer-only greedy meshing for crates/voxel.
//!
//! This is the v1 main line ported from vendor/voxelis/src/utils/mesh.rs
//! (voxelis v25.4.0, MIT OR Apache-2.0) with the design 3.2 boundary applied:
//!
//! * output is a list of RectInstance descriptors (39 bits of integer data),
//!   never f32 world vertices;
//! * SliceData lost global_offset / voxel_size and gained an integer material
//!   id; the mesher entry points lost their f32 offset / voxel_size parameters
//!   and slice became u32;
//! * the 64-hard-coded plane scratch is derived from the block's
//!   voxels_per_axis (32 for v1 depth-5 blocks);
//! * AO is carried as a per-face 4-corner code and used as a greedy-merge
//!   constancy predicate (section 9.6 option 1), available to the engine via
//!   extract_block_with_ao;
//! * rects are sorted by (plane, dir, material, slice, row, col) before they
//!   leave the crate, so no map iteration order can influence the result.
//!
//! No Bevy, no Transform, no voxel_size.

pub mod greedy;
pub mod incremental;
pub mod lod;
pub mod occupancy;

use std::collections::BTreeMap;

use glam::{UVec2, UVec3};

use crate::store::{ChunkKey, Lod, MaxDepth, VoxInterner, VoxOpsConfig, VoxTree};

use self::greedy::extract_rects;
use self::occupancy::{
    build_local_block_tree, generate_external_occupancy_mask_fast,
    generate_external_occupancy_mask_generic, generate_occupancy_masks_generic, ExternalPlane,
    InternerAccess, OccupancyData,
};

#[cfg(debug_assertions)]
pub use self::greedy::{add_quad, MeshData};
pub use self::greedy::{
    compute_face_ao, extract_rects as extract_rects_into, find_contiguous_bits, greedy_slice,
};
pub use self::incremental::{IncrementalMesh, SliceFaces, SliceKey, SliceRebuildReport};
pub use self::lod::{voxels_per_axis, wrap_block};
pub use self::occupancy::{
    extract_plane_dir, generate_external_occupancy_mask, generate_external_occupancy_mask_slow,
    generate_occupancy_masks, AxisOccupancy, Dir, ExternalPlane as ExternalPlaneKind,
    OccupancyData as MeshOccupancyData, OccupancyDataBuilder, Plane,
};

/// Plane tag for a YZ face (normal along X), row = y, col = z.
pub const PLANE_YZ: u8 = 0;
/// Plane tag for an XZ face (normal along Y), row = z, col = x.
pub const PLANE_XZ: u8 = 1;
/// Plane tag for an XY face (normal along Z), row = y, col = x.
pub const PLANE_XY: u8 = 2;

/// Direction tag for a positive-normal face.
pub const DIR_POS: u8 = 0;
/// Direction tag for a negative-normal face.
pub const DIR_NEG: u8 = 1;

/// Depth (octree levels) of one base subchunk: 2^5 = 32 voxels per axis.
pub const BASE_DEPTH: u8 = 5;
/// Highest LOD supported by v1 (requires MAX_ALLOWED_DEPTH = 9).
pub const MAX_LOD: u8 = 3;

/// One axis-aligned rectangle of exposed, same-material, AO-constant surface.
///
/// Every field is an integer index in the mesh block's local grid.  The
/// descriptor is 39 bits total and position independent: moving an island only
/// changes its Transform, never this data.
///
/// Field ranges for a 32^3 block:
/// * plane in 0..=2 (PLANE_YZ, PLANE_XZ, PLANE_XY)
/// * dir in 0..=1 (DIR_POS, DIR_NEG)
/// * slice in 0..=32 (includes both boundary planes)
/// * row, col in 0..=31
/// * w, h in 1..=32
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct RectInstance {
    /// Plane family tag.
    pub plane: u8,
    /// Face direction tag.
    pub dir: u8,
    /// Plane coordinate along the normal axis (0..=32).
    pub slice: u8,
    /// First row of the rectangle.
    pub row: u8,
    /// First column of the rectangle.
    pub col: u8,
    /// Width along the column axis (1..=32).
    pub w: u8,
    /// Height along the row axis (1..=32).
    pub h: u8,
    /// Material id.
    pub material: u8,
}

impl RectInstance {
    /// 3-bit orientation tag: plane * 2 + dir.
    #[must_use]
    pub const fn orientation(&self) -> u8 {
        self.plane * 2 + self.dir
    }

    /// Packs the descriptor into the 39-bit v1 layout:
    ///
    /// bits  0..=2  orientation (plane * 2 + dir)
    /// bits  3..=8  slice
    /// bits  9..=13 row
    /// bits 14..=18 col
    /// bits 19..=24 w
    /// bits 25..=30 h
    /// bits 31..=38 material
    #[must_use]
    pub const fn to_bits(&self) -> u64 {
        (self.orientation() as u64)
            | ((self.slice as u64) << 3)
            | ((self.row as u64) << 9)
            | ((self.col as u64) << 14)
            | ((self.w as u64) << 19)
            | ((self.h as u64) << 25)
            | ((self.material as u64) << 31)
    }

    /// Inverse of RectInstance::to_bits.
    #[must_use]
    pub const fn from_bits(raw: u64) -> Self {
        let orientation = (raw & 0b111) as u8;
        Self {
            plane: orientation >> 1,
            dir: orientation & 1,
            slice: ((raw >> 3) & 0x3F) as u8,
            row: ((raw >> 9) & 0x1F) as u8,
            col: ((raw >> 14) & 0x1F) as u8,
            w: ((raw >> 19) & 0x3F) as u8,
            h: ((raw >> 25) & 0x3F) as u8,
            material: ((raw >> 31) & 0xFF) as u8,
        }
    }

    /// Number of 1x1 faces covered by this rectangle.
    #[must_use]
    pub const fn area(&self) -> u32 {
        self.w as u32 * self.h as u32
    }
}

/// Deterministic ordering key of a rectangle.
#[must_use]
pub fn rect_key(rect: &RectInstance) -> (u8, u8, u8, u8, u8, u8) {
    (
        rect.plane,
        rect.dir,
        rect.material,
        rect.slice,
        rect.row,
        rect.col,
    )
}

/// Sorts a rectangle list by (plane, dir, material, slice, row, col).
pub fn sort_rects(rects: &mut [RectInstance]) {
    rects.sort_by(|a, b| rect_key(a).cmp(&rect_key(b)));
}

/// All rectangles extracted from one mesh block, position independent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RectBatch {
    /// Integer subchunk origin of the mesh block (its low/front/left corner).
    pub origin: ChunkKey,
    /// LOD of the mesh block.
    pub lod: Lod,
    /// Sorted rectangles.
    pub rects: Vec<RectInstance>,
}

/// A RectBatch plus the 4-corner AO code of every rectangle (same order).
///
/// AO is not part of the locked 39-bit RectInstance; it travels in a parallel
/// list so the engine can upload it without recomputing voxel occupancy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AoRectBatch {
    /// Integer subchunk origin of the mesh block.
    pub origin: ChunkKey,
    /// LOD of the mesh block.
    pub lod: Lod,
    /// Sorted rectangles.
    pub rects: Vec<RectInstance>,
    /// AO codes, one per rect.
    pub ao: Vec<[u8; 4]>,
}

impl AoRectBatch {
    /// Drops the AO data.
    #[must_use]
    pub fn into_batch(self) -> RectBatch {
        RectBatch {
            origin: self.origin,
            lod: self.lod,
            rects: self.rects,
        }
    }

    /// Number of rectangles.
    #[must_use]
    pub fn rect_count(&self) -> usize {
        self.rects.len()
    }
}

/// A chunk of work: the block whose lower/front/left corner is origin, at LOD
/// lod.
///
/// origin must be aligned to 2^lod base subchunks.  A block at lod L covers
/// 2^L x 2^L x 2^L base subchunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeshBlock {
    /// Aligned base subchunk origin.
    pub origin: ChunkKey,
    /// Level of detail.
    pub lod: Lod,
}

impl MeshBlock {
    /// Creates a mesh block.
    #[must_use]
    pub const fn new(origin: ChunkKey, lod: Lod) -> Self {
        Self { origin, lod }
    }

    /// Number of base subchunks per axis covered by this block (2^lod).
    #[must_use]
    pub const fn subchunk_span(&self) -> i32 {
        1 << self.lod.lod()
    }
}

fn finish(origin: ChunkKey, lod: Lod, mut pairs: Vec<(RectInstance, [u8; 4])>) -> AoRectBatch {
    pairs.sort_by(|a, b| rect_key(&a.0).cmp(&rect_key(&b.0)));
    let mut rects = Vec::with_capacity(pairs.len());
    let mut ao = Vec::with_capacity(pairs.len());
    for (rect, code) in pairs {
        rects.push(rect);
        ao.push(code);
    }
    AoRectBatch {
        origin,
        lod,
        rects,
        ao,
    }
}

fn run(occupancy: &OccupancyData, origin: ChunkKey, lod: Lod) -> AoRectBatch {
    let mut rects = Vec::new();
    let mut ao = Vec::new();
    extract_rects(occupancy, &mut rects, &mut ao);
    debug_assert_eq!(rects.len(), ao.len());
    let pairs: Vec<(RectInstance, [u8; 4])> = rects.into_iter().zip(ao).collect();
    finish(origin, lod, pairs)
}

fn build_local_occupancy(
    chunks: &BTreeMap<ChunkKey, VoxTree<u8>>,
    interner: &VoxInterner<u8>,
    block: MeshBlock,
    external: [Option<&VoxTree<u8>>; 6],
    clear_external_planes: [bool; 6],
) -> OccupancyData {
    let n = 1u32 << BASE_DEPTH;
    let mesh_depth = MaxDepth::new(BASE_DEPTH);
    let mut builder = OccupancyDataBuilder::new(n);

    for plane in ExternalPlane::ALL {
        if clear_external_planes[plane as usize] {
            builder.fill_external_side(plane);
        }
    }

    let local = build_local_block_tree(chunks, interner, block.origin, block.lod.lod());
    generate_occupancy_masks_generic(&local, local.root(), &mut builder, mesh_depth, UVec3::ZERO);

    let access = InternerAccess(interner);
    for plane in ExternalPlane::ALL {
        if let Some(tree) = external[plane as usize] {
            let root = tree.get_root_id();
            if !root.is_empty() {
                generate_external_occupancy_mask_generic(
                    &access,
                    &mut builder,
                    root,
                    mesh_depth,
                    plane,
                    UVec2::ZERO,
                );
            }
        }
    }

    builder.build()
}

/// Builds the occupancy grid of an already-wrapped tree.
///
/// Shared by extract_block_tree_with_ao and the engine incremental mesher
/// (spec 5 T6): the latter needs the same occupancy construction to diff
/// against its cached copy, so this is deliberately public rather than
/// duplicated.  tree has depth BASE_DEPTH + lod, so it is sampled at
/// tree.max_depth(lod) (always 32^3).
///
/// external carries the six neighbour trees in
/// [YZ+, YZ-, XZ+, XZ-, XY+, XY-] order; clear_external_planes[i] == true
/// first marks side i fully occupied (culling every face on it).
#[must_use]
pub fn build_tree_occupancy(
    tree: &VoxTree<u8>,
    interner: &VoxInterner<u8>,
    lod: Lod,
    external: [Option<&VoxTree<u8>>; 6],
    clear_external_planes: [bool; 6],
) -> OccupancyData {
    // A wrapped tree has depth BASE_DEPTH + lod, so max_depth(lod) is 5 (32^3).
    let mesh_depth = tree.max_depth(lod);
    let n = 1u32 << mesh_depth.max();
    let mut builder = OccupancyDataBuilder::new(n);

    for plane in ExternalPlane::ALL {
        if clear_external_planes[plane as usize] {
            builder.fill_external_side(plane);
        }
    }

    let access = InternerAccess(interner);
    generate_occupancy_masks_generic(
        &access,
        tree.get_root_id(),
        &mut builder,
        mesh_depth,
        UVec3::ZERO,
    );

    for plane in ExternalPlane::ALL {
        if let Some(neighbour) = external[plane as usize] {
            let root = neighbour.get_root_id();
            if !root.is_empty() {
                // Fast face-descending extractor (T3): bit-identical to the
                // generic reference but avoids n^2 occupied_at probes.
                generate_external_occupancy_mask_fast(
                    &access,
                    &mut builder,
                    root,
                    mesh_depth,
                    plane,
                    UVec2::ZERO,
                );
            }
        }
    }

    builder.build()
}

/// Extracts the rectangles of one mesh block.
///
/// chunks maps aligned base subchunk keys to their depth-BASE_DEPTH trees.
/// For lod > 0 the 2^lod-wide group is wrapped into a temporary depth-(5+lod)
/// tree; missing base subchunks are empty.  This signature takes an immutable
/// interner (as specified by design 3.2); use wrap_block when you want the
/// wrapped tree interned and shared.
///
/// external carries the six neighbour trees in
/// [YZ+, YZ-, XZ+, XZ-, XY+, XY-] order for cross-body seam culling.  A
/// neighbour is sampled at this block's resolution, so pass a wrapped tree of
/// the same LOD for a clean seam.
///
/// LOD seam policy: adjacent LOD blocks are meshed independently.  Both sides
/// place faces on the same integer plane coordinate and both use a face size
/// of 2^lod base voxels, so a shared boundary is geometrically watertight with
/// no skirts or vertex snapping.  A face that is covered by external occupancy
/// is culled, which is how a seam between two bodies avoids duplicate
/// triangles.  T-junctions between a large rectangle and several smaller ones
/// are accepted for v1 (invisible under opaque flat shading), matching design
/// section 9.5.
#[must_use]
pub fn extract_block(
    chunks: &BTreeMap<ChunkKey, VoxTree<u8>>,
    interner: &VoxInterner<u8>,
    block: MeshBlock,
    external: [Option<&VoxTree<u8>>; 6],
) -> RectBatch {
    extract_block_with_ao(chunks, interner, block, external).into_batch()
}

/// Like extract_block but keeps the per-rect AO codes.
#[must_use]
pub fn extract_block_with_ao(
    chunks: &BTreeMap<ChunkKey, VoxTree<u8>>,
    interner: &VoxInterner<u8>,
    block: MeshBlock,
    external: [Option<&VoxTree<u8>>; 6],
) -> AoRectBatch {
    let occupancy = build_local_occupancy(chunks, interner, block, external, [false; 6]);
    run(&occupancy, block.origin, block.lod)
}

/// Meshes an already-wrapped tree (the interned counterpart of extract_block).
#[must_use]
pub fn extract_block_tree(
    tree: &VoxTree<u8>,
    interner: &VoxInterner<u8>,
    block: MeshBlock,
    external: [Option<&VoxTree<u8>>; 6],
) -> RectBatch {
    extract_block_tree_ext(tree, interner, block, external, [false; 6])
}

/// Like extract_block_tree but keeps the per-rect AO codes.
#[must_use]
pub fn extract_block_tree_with_ao(
    tree: &VoxTree<u8>,
    interner: &VoxInterner<u8>,
    block: MeshBlock,
    external: [Option<&VoxTree<u8>>; 6],
) -> AoRectBatch {
    let occupancy = build_tree_occupancy(tree, interner, block.lod, external, [false; 6]);
    run(&occupancy, block.origin, block.lod)
}

/// The chunk_generate_greedy_mesh_arrays_ext equivalent: meshes an
/// already-wrapped tree, optionally forcing whole external sides closed.
///
/// clear_external_planes[i] == true marks side i as fully occupied, culling
/// every face on it (see OccupancyDataBuilder::fill_external_side).
#[must_use]
pub fn extract_block_tree_ext(
    tree: &VoxTree<u8>,
    interner: &VoxInterner<u8>,
    block: MeshBlock,
    external: [Option<&VoxTree<u8>>; 6],
    clear_external_planes: [bool; 6],
) -> RectBatch {
    let occupancy =
        build_tree_occupancy(tree, interner, block.lod, external, clear_external_planes);
    run(&occupancy, block.origin, block.lod).into_batch()
}

#[cfg(test)]
mod tests;
