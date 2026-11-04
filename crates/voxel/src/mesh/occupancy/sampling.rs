//! Single-voxel and material queries.
//!
//! These read one logical occupancy cell (or one material id) rather than
//! building a whole mask, and are used by the halo producer and by the
//! material lookup of the greedy extractor.

use glam::IVec3;

use crate::store::{MaxDepth, VoxInterner, VoxTree, VoxelTrait};

use super::data::OccupancyData;
use super::local_tree::{InternerAccess, NodeAccess};

/// Returns true when the voxel at position (in the tree's own depth frame) is
/// non-default.
pub(super) fn occupied_at<T: VoxelTrait, A: NodeAccess<T>>(
    access: &A,
    root: A::Id,
    position: IVec3,
    max_depth: u32,
) -> bool {
    let mut node = root;
    let mut depth = 0u32;
    while !access.is_empty(node) {
        if !access.is_leaf(node) {
            if depth >= max_depth {
                return access.value(node) != T::default();
            }
            let shift = (max_depth - depth - 1) as usize;
            let index = ((position.x as usize >> shift) & 1)
                | (((position.y as usize >> shift) & 1) << 1)
                | (((position.z as usize >> shift) & 1) << 2);
            node = access.child(node, index);
            depth += 1;
        } else {
            return access.value(node) != T::default();
        }
    }
    false
}

/// Samples whether `tree` holds a non-default voxel at `position`.
///
/// # 参数
///
/// * `position`  - 树自身坐标系下的整数体素坐标（可越界，越界读作空）。
/// * `max_depth` - 读到第几层；到层后按节点缓存的 LOD 平均值判定。
///
/// `position` is in the tree's own frame and `max_depth` is the traversal
/// depth to read at: for a wrapped depth-`(5 + lod)` block tree meshed at
/// `lod`, pass `tree.max_depth(lod)` and coordinates in `0..32` to get the
/// same lod-resolution value the occupancy extractors see.  This wraps the
/// private [`occupied_at`] so the raw-halo producer can sample a neighbour
/// tree without duplicating octree traversal.
#[must_use]
pub fn tree_voxel_is_solid(
    tree: &VoxTree<u8>,
    interner: &VoxInterner<u8>,
    position: IVec3,
    max_depth: MaxDepth,
) -> bool {
    let root = tree.get_root_id();
    if root.is_empty() {
        return false;
    }
    occupied_at(
        &InternerAccess(interner),
        root,
        position,
        max_depth.max() as u32,
    )
}

/// 读取 occupancy 网格中某个格的材料 id（0 = 空气，越界同样返回 0）。
///
/// 三平面掩码同时置位才算该材料；多材料同时命中时按 `materials`（材料 id 升序）
/// 取第一个，与 greedy mesher 的材质选择保持一致。
#[must_use]
pub fn voxel_material(occupancy: &OccupancyData, x: usize, y: usize, z: usize) -> u8 {
    let n = occupancy.voxels_per_axis as usize;
    if x >= n || y >= n || z >= n {
        return 0;
    }
    let plane_len = n * n;
    let bit_x = 1u64 << x;
    let bit_y = 1u64 << y;
    let bit_z = 1u64 << z;
    for (mi, (material, _)) in occupancy.materials.iter().enumerate() {
        let Some(masks) = occupancy.per_material.get(mi) else {
            continue;
        };
        if masks.len() < 3 * plane_len {
            continue;
        }
        if (masks[y * n + z] & bit_x) != 0
            && (masks[plane_len + z * n + x] & bit_y) != 0
            && (masks[2 * plane_len + y * n + x] & bit_z) != 0
        {
            return *material as u8;
        }
    }
    0
}
