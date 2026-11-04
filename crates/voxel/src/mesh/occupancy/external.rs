//! The six external face masks used to cull seams between two bodies.
//!
//! A face voxel is culled when the voxel directly across it, in the neighbour
//! body on that side, is occupied.  [`generate_external_occupancy_mask`] is the
//! production (fast) entry point; the n^2 `occupied_at` probe is kept as the
//! reference implementation that the equivalence tests compare against.

use glam::{IVec3, UVec2, UVec3};

use crate::store::{BlockId, MaxDepth, VoxInterner, VoxelTrait};

use super::data::OccupancyDataBuilder;
use super::local_tree::{InternerAccess, NodeAccess};
use super::plane::{extract_plane_dir, Dir, ExternalPlane, Plane};
use super::sampling::occupied_at;

/// Generic external-face occupancy extraction from any NodeAccess tree.
pub(crate) fn generate_external_occupancy_mask_generic<T: VoxelTrait, A: NodeAccess<T>>(
    access: &A,
    builder: &mut OccupancyDataBuilder,
    root_id: A::Id,
    max_depth: MaxDepth,
    external_plane: ExternalPlane,
    offset: UVec2,
) {
    let max_depth = max_depth.max();
    let voxels_per_axis = 1usize << max_depth;
    assert_eq!(
        builder.voxels_per_axis as usize, voxels_per_axis,
        "builder resolution does not match max_depth"
    );

    let start_col = offset.x as usize;
    let start_row = offset.y as usize;

    let (plane, dir) = extract_plane_dir(external_plane);

    let offset_start = external_plane as usize * voxels_per_axis;
    builder.external_exists[external_plane as usize] = true;
    let external_plane_masks = &mut builder.external[offset_start..offset_start + voxels_per_axis];

    if access.is_empty(root_id) {
        return;
    }

    let pos_vox = match dir {
        Dir::Pos => 0,
        Dir::Neg => (voxels_per_axis - 1) as i32,
    };

    if !access.is_leaf(root_id) {
        match plane {
            Plane::YZ => {
                for y in 0..voxels_per_axis {
                    let row = start_row + y;
                    for z in 0..voxels_per_axis {
                        if occupied_at(
                            access,
                            root_id,
                            IVec3::new(pos_vox, y as i32, z as i32),
                            max_depth as u32,
                        ) {
                            external_plane_masks[row] |= 1u64 << (start_col + z);
                        }
                    }
                }
            }
            Plane::XZ => {
                for z in 0..voxels_per_axis {
                    let row = start_row + z;
                    for x in 0..voxels_per_axis {
                        if occupied_at(
                            access,
                            root_id,
                            IVec3::new(x as i32, pos_vox, z as i32),
                            max_depth as u32,
                        ) {
                            external_plane_masks[row] |= 1u64 << (start_col + x);
                        }
                    }
                }
            }
            Plane::XY => {
                for y in 0..voxels_per_axis {
                    let row = start_row + y;
                    for x in 0..voxels_per_axis {
                        if occupied_at(
                            access,
                            root_id,
                            IVec3::new(x as i32, y as i32, pos_vox),
                            max_depth as u32,
                        ) {
                            external_plane_masks[row] |= 1u64 << (start_col + x);
                        }
                    }
                }
            }
        }
    } else {
        let bit_mask = if voxels_per_axis == 64 {
            u64::MAX
        } else {
            ((1u64 << voxels_per_axis) - 1) << start_col
        };
        for r in 0..voxels_per_axis {
            external_plane_masks[start_row + r] |= bit_mask;
        }
    }
}

/// Fast, face-descending external-face occupancy extraction.
///
/// 与 slow 版本逐位等价，但只下钻落在采样面上的 4 个子节点，叶节点/到深
/// 分支整块投影，因此复杂度从 n^2 次下钻降到每个面一次遍历。
///
/// Bit-for-bit equivalent to generate_external_occupancy_mask_generic (the
/// reference implementation kept for comparison tests), but instead of probing
/// every one of the n^2 face voxels with occupied_at it walks the octree once
/// and only descends the four children that lie on the sampled face.  A leaf,
/// or a branch reached at max_depth, marks its whole projected square at once.
pub(crate) fn generate_external_occupancy_mask_fast<T: VoxelTrait, A: NodeAccess<T>>(
    access: &A,
    builder: &mut OccupancyDataBuilder,
    root_id: A::Id,
    max_depth: MaxDepth,
    external_plane: ExternalPlane,
    offset: UVec2,
) {
    let max_depth = max_depth.max();
    let voxels_per_axis = 1usize << max_depth;
    assert_eq!(
        builder.voxels_per_axis as usize, voxels_per_axis,
        "builder resolution does not match max_depth"
    );

    let start_col = offset.x as usize;
    let start_row = offset.y as usize;

    let (plane, dir) = extract_plane_dir(external_plane);

    let offset_start = external_plane as usize * voxels_per_axis;
    builder.external_exists[external_plane as usize] = true;
    // Same length-n slice as the generic implementation, so a row offset is
    // honoured (and rejected) identically.
    let external_plane_masks = &mut builder.external[offset_start..offset_start + voxels_per_axis];

    if access.is_empty(root_id) {
        return;
    }

    if access.is_leaf(root_id) {
        // The reference implementation fills the whole sampled plane for a leaf
        // root unconditionally, even when that leaf holds the default value.
        let bit_mask = if voxels_per_axis == 64 {
            u64::MAX
        } else {
            ((1u64 << voxels_per_axis) - 1) << start_col
        };
        for r in 0..voxels_per_axis {
            external_plane_masks[start_row + r] |= bit_mask;
        }
        return;
    }

    // Index bit of the plane normal axis: x = bit 0, y = bit 1, z = bit 2.
    let normal_axis = match plane {
        Plane::YZ => 0usize,
        Plane::XZ => 1usize,
        Plane::XY => 2usize,
    };
    // Dir::Pos samples the neighbour's local coordinate 0, Dir::Neg the last
    // one (see the generic implementation's pos_vox); at octree depth d both
    // correspond to index bit d of the normal axis.
    let dir_bit = match dir {
        Dir::Pos => 0usize,
        Dir::Neg => 1usize,
    };

    let default_t = T::default();
    let mut stack: Vec<(A::Id, UVec3, u32)> = Vec::with_capacity(64);
    stack.push((root_id, UVec3::ZERO, 0u32));

    while let Some((node, cube_min, depth)) = stack.pop() {
        if !access.is_leaf(node) && depth < max_depth as u32 {
            let half = 1u32 << (max_depth as u32 - depth - 1);
            for index in 0..8usize {
                if ((index >> normal_axis) & 1) != dir_bit {
                    continue;
                }
                let child = access.child(node, index);
                if access.is_empty(child) {
                    continue;
                }
                let x = (index & 1) as u32 * half;
                let y = ((index >> 1) & 1) as u32 * half;
                let z = ((index >> 2) & 1) as u32 * half;
                stack.push((child, cube_min + UVec3::new(x, y, z), depth + 1));
            }
        } else {
            // Leaf at any depth, or a branch reached at max_depth: occupied_at
            // resolves both to value != default for every voxel in the cube.
            if access.value(node) == default_t {
                continue;
            }
            let side = (1u32 << (max_depth as u32 - depth)) as usize;
            let (row0, col0) = match plane {
                Plane::YZ => (cube_min.y as usize, cube_min.z as usize),
                Plane::XZ => (cube_min.z as usize, cube_min.x as usize),
                Plane::XY => (cube_min.y as usize, cube_min.x as usize),
            };
            let run_mask = if side >= 64 {
                u64::MAX
            } else {
                ((1u64 << side) - 1) << (start_col + col0)
            };
            for r in 0..side {
                external_plane_masks[start_row + row0 + r] |= run_mask;
            }
        }
    }
}

/// Public interner-backed external occupancy extraction (upstream entry point).
///
/// # 参数
///
/// * `root_id`       - 邻居体在该侧面上的子树根；空根只置位 external_exists。
/// * `max_depth`     - 采样深度，须与 builder 的分辨率一致，否则 panic。
/// * `external_plane`- 六个面之一，决定采样的是哪一面以及行/列的投影方向。
/// * `offset`        - (col, row) 偏移，非零时只填该侧的一部分。
///
/// # 返回
///
/// 无返回值：结果写进 `builder.external` 中该面独占的 n 个掩码。
///
/// The caller feeds a neighbour body's tree for external_plane; faces whose
/// directly-across voxel is occupied are culled from the mesh, so a seam
/// between two bodies does not emit duplicate geometry.
pub fn generate_external_occupancy_mask<T: VoxelTrait>(
    interner: &VoxInterner<T>,
    builder: &mut OccupancyDataBuilder,
    root_id: &BlockId,
    max_depth: MaxDepth,
    external_plane: ExternalPlane,
    offset: UVec2,
) {
    generate_external_occupancy_mask_fast(
        &InternerAccess(interner),
        builder,
        *root_id,
        max_depth,
        external_plane,
        offset,
    );
}

/// Reference (slow) external occupancy extraction, exposed only for
/// benchmarks that compare it against the public fast default.
///
/// This forwards to `generate_external_occupancy_mask_generic`, which is the
/// n^2 `occupied_at` reference kept for equivalence tests.  Production callers
/// must keep using [`generate_external_occupancy_mask`] (fast): this entry
/// point exists so an external bench target can measure the old cost without
/// the crate-internal reference becoming part of the supported API surface.
#[doc(hidden)]
pub fn generate_external_occupancy_mask_slow<T: VoxelTrait>(
    interner: &VoxInterner<T>,
    builder: &mut OccupancyDataBuilder,
    root_id: &BlockId,
    max_depth: MaxDepth,
    external_plane: ExternalPlane,
    offset: UVec2,
) {
    generate_external_occupancy_mask_generic(
        &InternerAccess(interner),
        builder,
        *root_id,
        max_depth,
        external_plane,
        offset,
    );
}
