//! Greedy rectangle extraction (integer-only).
//!
//! This is the v1 main-line replacement for the 4x Vec3 + add_quad output of
//! vendor/voxelis/src/utils/mesh.rs (MIT OR Apache-2.0).  The merge algorithm
//! is unchanged, but at the point where upstream computed
//! plane / dir / slice / start_row / start_col / width / height (mesh.rs
//! 911-920) we emit a RectInstance directly.
//!
//! ## Ambient occlusion
//!
//! Design decision 21 / section 9.6 option 1: AO is never baked into geometry.
//! Instead a per-face 4-corner AO code is computed and used as an extra
//! constancy predicate in the greedy merge loop: two candidate faces only
//! merge when their 4-corner AO codes are identical.  Convex / fully lit
//! regions have a constant code and merge freely; concave regions fall back to
//! 1x1 rectangles.  The AO code of every emitted rectangle is returned
//! alongside it so the engine can upload it without recomputing voxel
//! occupancy.
//!
//! The AO level formula is the classic voxel one: a corner is fully dark when
//! both edge neighbours occlude, otherwise 3 - (side1 + side2 + corner).

use glam::Vec3;

use crate::mesh::incremental::SliceFaces;
use crate::mesh::occupancy::{
    full_mask, AxisOccupancy, Dir, OccupancyData, Plane, PlaneData, PLANES,
};
use crate::mesh::RectInstance;

/// Per-slice inputs for generate_greedy_faces_for_slice.
///
/// Upstream carried global_offset: Vec3 and voxel_size: f32; both are gone
/// (design 3.2).  The integer material id is added so the emitted
/// RectInstance is complete.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SliceData {
    /// First active row (inclusive).
    pub min_row: usize,
    /// One past the last active row.
    pub max_row: usize,
    /// Plane family.
    pub plane: Plane,
    /// Face direction.
    pub dir: Dir,
    /// Material id (fits in u8 for the v1 palette).
    pub material: u8,
}

/// Returns the contiguous run of set bits of mask starting at start.
///
/// This is the upstream find_contiguous_bits, unchanged.
#[must_use]
#[inline(always)]
pub fn find_contiguous_bits(mask: u64, start: usize) -> u64 {
    if mask == u64::MAX {
        return !0u64 << start;
    }
    let shifted = mask >> start;
    let inverted = !shifted;
    let first_zero = inverted.trailing_zeros() as u64;
    ((1u64 << first_zero) - 1) << start
}

/// AO level from the three classic samples.
#[must_use]
#[inline(always)]
fn ao_level(side1: bool, side2: bool, corner: bool) -> u8 {
    if side1 && side2 {
        0
    } else {
        3 - (side1 as u8 + side2 as u8 + corner as u8)
    }
}

/// True when a face's four corners share one AO value.
///
/// Greedy merging is only allowed over AO-constant faces (design 9.6 option 1):
/// a merged rectangle can only carry four corner values, so a face whose own
/// corners differ must stay 1x1.
#[must_use]
#[inline(always)]
fn ao_is_constant(code: [u8; 4]) -> bool {
    code[0] == code[1] && code[1] == code[2] && code[2] == code[3]
}

/// Reads one occupancy bit at (row, col) of the plane at normal index layer.
#[inline(always)]
fn occupancy_bit(
    occupancy: &OccupancyData,
    plane_offset: usize,
    n: usize,
    layer: i64,
    row: i64,
    col: i64,
) -> bool {
    if layer < 0 || layer >= n as i64 || row < 0 || col < 0 || row >= n as i64 || col >= n as i64 {
        return false;
    }
    (occupancy.global[plane_offset + row as usize * n + col as usize] >> layer) & 1 != 0
}

/// Computes the 4-corner AO code of a single face.
///
/// Corner order is (row_min, col_min), (row_min, col_max),
/// (row_max, col_min), (row_max, col_max).
#[must_use]
pub fn compute_face_ao(
    occupancy: &OccupancyData,
    plane: Plane,
    dir: Dir,
    slice_bit: u32,
    row: usize,
    col: usize,
) -> [u8; 4] {
    let n = occupancy.voxels_per_axis as usize;
    let plane_offset = plane.plane_index() * n * n;
    let layer: i64 = match dir {
        Dir::Pos => slice_bit as i64 + 1,
        Dir::Neg => slice_bit as i64 - 1,
    };
    let r = row as i64;
    let c = col as i64;
    let at = |dr: i64, dc: i64| occupancy_bit(occupancy, plane_offset, n, layer, r + dr, c + dc);

    let neg_row = at(-1, 0);
    let pos_row = at(1, 0);
    let neg_col = at(0, -1);
    let pos_col = at(0, 1);
    let neg_neg = at(-1, -1);
    let neg_pos = at(-1, 1);
    let pos_neg = at(1, -1);
    let pos_pos = at(1, 1);

    [
        ao_level(neg_row, neg_col, neg_neg),
        ao_level(neg_row, pos_col, neg_pos),
        ao_level(pos_row, neg_col, pos_neg),
        ao_level(pos_row, pos_col, pos_pos),
    ]
}

fn compute_ao_grid(
    occupancy: &OccupancyData,
    plane: Plane,
    dir: Dir,
    slice_bit: u32,
) -> Vec<[u8; 4]> {
    let n = occupancy.voxels_per_axis as usize;
    let mut grid = vec![[0u8; 4]; n * n];
    for row in 0..n {
        for col in 0..n {
            grid[row * n + col] = compute_face_ao(occupancy, plane, dir, slice_bit, row, col);
        }
    }
    grid
}

/// Computes the global exposed-face masks for one plane family, applying the
/// external-neighbour occlusion exactly like the inline phase 1 of
/// `extract_rects` used to.
///
/// `out_pos` and `out_neg` are length n*n.  They are fully cleared first and
/// then receive the same values the full extractor computes, so callers can
/// reuse one pair of buffers across plane families.
pub(crate) fn global_face_masks_for_plane(
    occupancy: &OccupancyData,
    plane_data: &PlaneData,
    out_pos: &mut [u64],
    out_neg: &mut [u64],
) {
    let n = occupancy.voxels_per_axis as usize;
    out_pos.fill(0);
    out_neg.fill(0);

    let base = plane_data.offset_factor * n * n;
    let ext_pos_start = plane_data.pos as usize * n;
    let external_pos = &occupancy.external[ext_pos_start..ext_pos_start + n];
    let ext_neg_start = plane_data.neg as usize * n;
    let external_neg = &occupancy.external[ext_neg_start..ext_neg_start + n];

    let rows = occupancy.global_active[plane_data.global_active_idx];
    let cols = occupancy.global_active[plane_data.global_active_idx + 1];
    let min_row = rows.min;
    let max_row = rows.max;
    let min_col = cols.min;
    let max_col = cols.max;

    for row in min_row..max_row {
        if (rows.active >> row) & 1 == 0 {
            continue;
        }
        let base_idx = row * n;
        for col in min_col..max_col {
            if (cols.active >> col) & 1 == 0 {
                continue;
            }
            let idx = base_idx + col;
            let mask = occupancy.global[base + idx];
            let mut global_pos = !(mask >> 1) & mask;
            let mut global_neg = !(mask << 1) & mask;
            // Remove faces adjacent to external neighbours.
            global_pos &= !(((external_pos[row] >> col) & 1) << (n - 1));
            global_neg &= !((external_neg[row] >> col) & 1);
            out_pos[idx] = global_pos;
            out_neg[idx] = global_neg;
        }
    }
}

/// Builds a bitmask of the columns of one row whose AO code equals code.
#[inline]
fn ao_row_mask(ao_grid: &[[u8; 4]], n: usize, row: usize, code: [u8; 4], active: u64) -> u64 {
    let mut mask = 0u64;
    let mut bits = active;
    while bits != 0 {
        let col = bits.trailing_zeros() as usize;
        bits &= bits - 1;
        if ao_grid[row * n + col] == code {
            mask |= 1u64 << col;
        }
    }
    mask
}

/// Greedy extraction for one (plane, dir, material, slice) grid.
///
/// slice is the integer index along the plane normal (0..=n).  For a
/// positive face it is the boundary above voxel slice - 1; for a negative
/// face it is the boundary below voxel slice.  Emitted rectangles and their
/// 4-corner AO codes are appended to rects / aos in matching order.
pub(crate) fn generate_greedy_faces_for_slice(
    occupancy: &OccupancyData,
    slice_data: &SliceData,
    slice: u32,
    faces_total: usize,
    faces: &[u64],
    rects: &mut Vec<RectInstance>,
    aos: &mut Vec<[u8; 4]>,
) {
    let n = occupancy.voxels_per_axis as usize;
    let mut faces_left = faces_total;
    let mut used = vec![0u64; n];
    let ao_grid = compute_ao_grid(occupancy, slice_data.plane, slice_data.dir, slice);

    let geom_slice = match slice_data.dir {
        Dir::Pos => slice + 1,
        Dir::Neg => slice,
    };
    let plane_tag = slice_data.plane.index();
    let dir_tag = slice_data.dir.index();
    let mut done = false;

    for start_row in slice_data.min_row..slice_data.max_row {
        let row_faces = faces[start_row];
        let mut available = row_faces & !used[start_row];

        while available != 0 {
            let start_col = available.trailing_zeros() as usize;
            let seed_ao = ao_grid[start_row * n + start_col];

            if !ao_is_constant(seed_ao) {
                // AO varies across this face: emit it as 1x1 (never interpolate).
                rects.push(RectInstance {
                    plane: plane_tag,
                    dir: dir_tag,
                    slice: geom_slice as u8,
                    row: start_row as u8,
                    col: start_col as u8,
                    w: 1,
                    h: 1,
                    material: slice_data.material,
                });
                aos.push(seed_ao);
                used[start_row] |= 1u64 << start_col;
                available &= !(1u64 << start_col);
                faces_left = faces_left.saturating_sub(1);
                if faces_left == 0 {
                    done = true;
                    break;
                }
                continue;
            }

            let same = ao_row_mask(&ao_grid, n, start_row, seed_ao, row_faces);
            let avail = available & same;
            if (avail >> start_col) & 1 == 0 {
                // No mergeable neighbour at all; skip this bit so we cannot loop.
                available &= !(1u64 << start_col);
                continue;
            }

            let width_mask = find_contiguous_bits(avail, start_col);
            let width = width_mask.count_ones() as usize;
            let mut height = 1usize;

            for row in (start_row + 1)..slice_data.max_row {
                let candidate = faces[row] & !used[row];
                if (candidate & width_mask) != width_mask {
                    break;
                }
                let same_row = ao_row_mask(&ao_grid, n, row, seed_ao, faces[row]);
                if (same_row & width_mask) != width_mask {
                    break;
                }
                used[row] |= width_mask;
                height += 1;
            }

            rects.push(RectInstance {
                plane: plane_tag,
                dir: dir_tag,
                slice: geom_slice as u8,
                row: start_row as u8,
                col: start_col as u8,
                w: width as u8,
                h: height as u8,
                material: slice_data.material,
            });
            aos.push(seed_ao);

            used[start_row] |= width_mask;
            available &= !width_mask;
            faces_left = faces_left.saturating_sub(width * height);

            if faces_left == 0 {
                done = true;
                break;
            }
        }
        if done {
            break;
        }
    }
}

/// Runs greedy merging for exactly one (plane, dir, material, slice) grid.
///
/// `masks` is the material's face masks for this plane family and direction,
/// already combined with the global face masks and external occlusion by the
/// caller, laid out as `row * n + col`.  `slice_bit` is the slice index along
/// the plane normal in `0..n` (the value fed to
/// `generate_greedy_faces_for_slice`).  Emitted rectangles and their AO codes
/// are appended to `out` in the frozen (row, col) order; the caller reuses a
/// single sink to avoid a heap allocation per slice.
pub fn greedy_slice(
    occupancy: &OccupancyData,
    masks: &[u64],
    plane: Plane,
    dir: Dir,
    material: u8,
    slice_bit: usize,
    out: &mut SliceFaces,
) {
    let n = occupancy.voxels_per_axis as usize;
    // OccupancyDataBuilder guarantees n <= 64, so this stack scratch avoids a
    // heap allocation per slice.
    let mut faces = [0u64; 64];
    let mut faces_total = 0usize;
    let mut active_row = 0u64;

    for (row, slot) in faces.iter_mut().enumerate().take(n) {
        let base_idx = row * n;
        let mut row_faces = 0u64;
        for col in 0..n {
            if ((masks[base_idx + col] >> slice_bit) & 1) != 0 {
                row_faces |= 1u64 << col;
            }
        }
        if row_faces != 0 {
            *slot = row_faces;
            faces_total += row_faces.count_ones() as usize;
            active_row |= 1u64 << row;
        }
    }

    // The per-slice active-row extent drives the merge loop.  It can be
    // narrower than the material's global extent, but empty rows never emit
    // anything, so the output is identical.
    let row_occ = AxisOccupancy::new(active_row);
    let slice_data = SliceData {
        min_row: row_occ.min,
        max_row: row_occ.max,
        plane,
        dir,
        material,
    };

    generate_greedy_faces_for_slice(
        occupancy,
        &slice_data,
        slice_bit as u32,
        faces_total,
        &faces[..n],
        &mut out.rects,
        &mut out.aos,
    );
}

/// Turns an OccupancyData into greedy rectangles plus per-rect AO codes.
///
/// The output order is the deterministic upstream traversal order
/// (plane, material id, direction, slice, row, col); callers that need the
/// frozen public order should sort with crate::mesh::sort_rects.
pub fn extract_rects(
    occupancy: &OccupancyData,
    rects: &mut Vec<RectInstance>,
    aos: &mut Vec<[u8; 4]>,
) {
    let n = occupancy.voxels_per_axis as usize;
    let full = full_mask(n);

    if occupancy.external.iter().all(|&mask| mask == full) {
        return;
    }

    let mut global_face_masks_pos = vec![0u64; n * n];
    let mut global_face_masks_neg = vec![0u64; n * n];
    let mut material_face_masks_pos = vec![0u64; n * n];
    let mut material_face_masks_neg = vec![0u64; n * n];
    let mut scratch = SliceFaces::default();

    for plane_data in &PLANES {
        let base = plane_data.offset_factor * n * n;

        global_face_masks_for_plane(
            occupancy,
            plane_data,
            &mut global_face_masks_pos,
            &mut global_face_masks_neg,
        );

        let rows = occupancy.global_active[plane_data.global_active_idx];
        let cols = occupancy.global_active[plane_data.global_active_idx + 1];
        let min_row = rows.min;
        let max_row = rows.max;
        let min_col = cols.min;
        let max_col = cols.max;

        // Phase 2: per material, per direction.
        for (material_idx, (material_id, _)) in occupancy.materials.iter().enumerate() {
            let occupancy_per_material = &occupancy.per_material[material_idx];

            // Clear the per-material scratch: greedy_slice scans the whole n*n
            // grid, so every cell must hold this material's combined mask.
            material_face_masks_pos.fill(0);
            material_face_masks_neg.fill(0);

            let mut active_depth_pos = 0u64;
            let mut count_pos = 0usize;
            let mut active_depth_neg = 0u64;
            let mut count_neg = 0usize;

            for row in min_row..max_row {
                let base_idx = row * n;
                for col in min_col..max_col {
                    let idx = base_idx + col;
                    let mask = occupancy_per_material[base + idx];

                    let mask_pos = mask & global_face_masks_pos[idx];
                    active_depth_pos |= mask_pos;
                    material_face_masks_pos[idx] = mask_pos;
                    count_pos += mask_pos.count_ones() as usize;

                    let mask_neg = mask & global_face_masks_neg[idx];
                    active_depth_neg |= mask_neg;
                    material_face_masks_neg[idx] = mask_neg;
                    count_neg += mask_neg.count_ones() as usize;
                }
            }

            if count_pos == 0 && count_neg == 0 {
                continue;
            }

            let material = *material_id as u8;
            let dirs = [
                (Dir::Pos, count_pos, active_depth_pos, true),
                (Dir::Neg, count_neg, active_depth_neg, false),
            ];

            for (dir, total, active_depth, is_pos) in dirs {
                if total == 0 {
                    continue;
                }
                let masks: &[u64] = if is_pos {
                    &material_face_masks_pos
                } else {
                    &material_face_masks_neg
                };
                let depth_occ = AxisOccupancy::new(active_depth);
                for slice_bit in depth_occ.min..depth_occ.max {
                    if (depth_occ.active >> slice_bit) & 1 == 0 {
                        continue;
                    }
                    greedy_slice(
                        occupancy,
                        masks,
                        plane_data.plane,
                        dir,
                        material,
                        slice_bit,
                        &mut scratch,
                    );
                    rects.append(&mut scratch.rects);
                    aos.append(&mut scratch.aos);
                }
            }
        }
    }
}

/// Debug-only vertex path.  The production extractor never calls this.
#[cfg(debug_assertions)]
#[derive(Default, Debug, Clone)]
pub struct MeshData {
    /// Debug vertices.
    pub vertices: Vec<Vec3>,
    /// Debug normals.
    pub normals: Vec<Vec3>,
    /// Debug triangle indices.
    pub indices: Vec<u32>,
}

/// Debug-only quad writer, kept from upstream for debugging tools.
#[cfg(debug_assertions)]
pub fn add_quad(mesh_data: &mut MeshData, quad: [Vec3; 4], normal: &Vec3) {
    let index = mesh_data.vertices.len() as u32;
    mesh_data.vertices.extend(quad);
    mesh_data
        .normals
        .extend([*normal, *normal, *normal, *normal]);
    mesh_data
        .indices
        .extend([index + 2, index + 1, index, index + 3, index, index + 1]);
}
