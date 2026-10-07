//! 树 / interner 共用的寻址与转储辅助（移植自 vendored utils/common.rs）。
//!
//! 这里只有整数运算；上游宏被替换为普通 #[inline(always)] 函数，
//! 语义与上游逐位一致。

use glam::IVec3;

use crate::store::{BlockId, MaxDepth, TraversalDepth, VoxInterner, VoxelTrait, MAX_CHILDREN};

/// 返回体素 position 在给定遍历深度下的子节点下标（0..8，Morton 顺序）。
#[must_use]
#[inline(always)]
pub const fn child_index(position: &IVec3, depth: &TraversalDepth) -> usize {
    let shift = depth.max() - depth.current() - 1;

    ((position.x as usize >> shift) & 1)
        | (((position.y as usize >> shift) & 1) << 1)
        | (((position.z as usize >> shift) & 1) << 2)
}

/// 与 child_index 相同，但直接接收 current / max 深度。
#[must_use]
#[inline(always)]
pub const fn child_index2(position: &IVec3, current: usize, max: usize) -> usize {
    let shift = max - current - 1;

    ((position.x as usize >> shift) & 1)
        | (((position.y as usize >> shift) & 1) << 1)
        | (((position.z as usize >> shift) & 1) << 2)
}

/// 把 (x, y, z) 各 10 bit 交织成 30 bit 的 Morton 路径码。
#[must_use]
#[inline(always)]
pub const fn encode_child_index_path(position: &IVec3) -> u32 {
    const MASK_10_BITS: u32 = 0x0000_03FF;
    const MASK_1: u32 = 0x0300_00FF;
    const MASK_2: u32 = 0x0300_F00F;
    const MASK_3: u32 = 0x030C_30C3;
    const MASK_4: u32 = 0x0924_9249;

    let x = (position.x as u32) & MASK_10_BITS;
    let y = (position.y as u32) & MASK_10_BITS;
    let z = (position.z as u32) & MASK_10_BITS;

    let x = (x | (x << 16)) & MASK_1;
    let x = (x | (x << 8)) & MASK_2;
    let x = (x | (x << 4)) & MASK_3;
    let x = (x | (x << 2)) & MASK_4;

    let y = (y | (y << 16)) & MASK_1;
    let y = (y | (y << 8)) & MASK_2;
    let y = (y | (y << 4)) & MASK_3;
    let y = (y | (y << 2)) & MASK_4;

    let z = (z | (z << 16)) & MASK_1;
    let z = (z | (z << 8)) & MASK_2;
    let z = (z | (z << 4)) & MASK_3;
    let z = (z | (z << 2)) & MASK_4;

    x | (y << 1) | (z << 2)
}

/// 从 node_id 沿 position 下降到深度 depth，读出体素值。
#[inline(always)]
pub fn get_at_depth<T: VoxelTrait>(
    interner: &VoxInterner<T>,
    mut node_id: BlockId,
    position: &IVec3,
    depth: &TraversalDepth,
) -> Option<T> {
    let max_depth = depth.max();
    let mut depth = depth.current();

    let default_t = T::default();

    while !node_id.is_empty() {
        if depth >= max_depth {
            let v = interner.get_value(&node_id);
            return if v != &default_t { Some(*v) } else { None };
        }

        if node_id.is_branch() {
            let index = child_index2(position, depth as usize, max_depth as usize);
            node_id = interner.get_child_id(&node_id, index);
            depth += 1;
        } else {
            return Some(*interner.get_value(&node_id));
        }
    }

    None
}

/// 把子树展开为稠密 [T]（voxels_per_axis³，x 最快变）。
pub fn to_vec<T: VoxelTrait>(
    interner: &VoxInterner<T>,
    root_id: &BlockId,
    max_depth: MaxDepth,
) -> Vec<T> {
    let max_depth = max_depth.max() as u32;
    let voxels_per_axis = 1 << max_depth;
    let size = voxels_per_axis * voxels_per_axis * voxels_per_axis;

    if !root_id.is_branch() {
        return vec![*interner.get_value(root_id); size as usize];
    }

    let default_t = T::default();

    let mut data = vec![default_t; size as usize];

    if root_id.is_empty() {
        return data;
    }

    let mut stack: Vec<(BlockId, IVec3, u32)> = Vec::with_capacity(64);
    stack.push((*root_id, IVec3::ZERO, 0));

    while let Some((node_id, pos, depth)) = stack.pop() {
        if node_id.is_branch() && (depth < max_depth) {
            let child_cube_half_side = 1 << (max_depth - depth - 1);
            let childs = interner.get_children(&node_id);
            for i in (0..8).rev() {
                let child_id = childs[i];

                if !child_id.is_empty() {
                    let offset = IVec3::new(
                        (i & 1) as i32 * child_cube_half_side,
                        ((i & 2) >> 1) as i32 * child_cube_half_side,
                        ((i & 4) >> 2) as i32 * child_cube_half_side,
                    );

                    stack.push((child_id, pos + offset, depth + 1));
                }
            }
        } else {
            let value = *interner.get_value(&node_id);
            if value != default_t {
                let cube_side = (1 << (max_depth - depth)) as usize;
                fill_sub_volume(&mut data, pos, cube_side, voxels_per_axis as usize, value);
            }
        }
    }

    data
}

#[inline(always)]
fn fill_sub_volume<T: VoxelTrait>(
    data: &mut [T],
    pos: IVec3,
    cube_side: usize,
    voxels_per_axis: usize,
    value: T,
) {
    let pos_x = pos.x as usize;
    let pos_y = pos.y as usize;
    let pos_z = pos.z as usize;

    let stride_y = voxels_per_axis * voxels_per_axis;
    let stride_z = voxels_per_axis;

    for y in pos_y..(pos_y + cube_side) {
        let base_y = y * stride_y;
        for z in pos_z..(pos_z + cube_side) {
            let base_z = base_y + z * stride_z;
            let start_index = base_z + pos_x;
            let end_index = start_index + cube_side;
            data[start_index..end_index].fill(value);
        }
    }
}

/// 以人类可读形式转储以 root_id 为根的整棵树。
pub fn dump_structure<T: VoxelTrait>(
    interner: &VoxInterner<T>,
    root_id: BlockId,
    max_depth: usize,
) {
    println!("\n=== Octree Structure Dump ===");
    println!("Max depth: {max_depth}");

    if !root_id.is_empty() {
        interner.dump_node(root_id, 0, "  ");
    } else {
        println!("Empty octree (no root)");
    }
    println!("=== End of Structure Dump ===\n");
}

/// 转储根节点。
pub fn dump_root<T: VoxelTrait>(interner: &VoxInterner<T>, root_id: BlockId) {
    println!("\n=== Octree Root Dump ===");
    if !root_id.is_empty() {
        interner.dump_node(root_id, 0, "");
    } else {
        println!("Empty octree (no root)");
    }
    println!("=== End of Root Dump ===\n");
}

#[derive(Default)]
struct OctreeStats {
    total_nodes: usize,
    branch_nodes: usize,
    leaf_nodes: usize,
    max_depth_reached: u8,
    nodes_by_depth: Vec<usize>,
}

/// 收集并打印以 root_id 为根的树的节点统计。
pub fn dump_statistics<T: VoxelTrait>(interner: &VoxInterner<T>, root_id: BlockId) {
    println!("\n=== Octree Statistics ===");
    if !root_id.is_empty() {
        let mut stats = OctreeStats::default();
        collect_stats(interner, root_id, 0, &mut stats);
        println!("Total nodes: {}", stats.total_nodes);
        println!("Branch nodes: {}", stats.branch_nodes);
        println!("Leaf nodes: {}", stats.leaf_nodes);
        println!("Max depth reached: {}", stats.max_depth_reached);
        println!("Nodes by depth:");
        for (depth, count) in stats.nodes_by_depth.iter().enumerate() {
            println!("  Depth {depth}: {count} nodes");
        }
    } else {
        println!("Empty octree (no statistics available)");
    }
    println!("=== End of Statistics ===\n");
}

fn collect_stats<T: VoxelTrait>(
    interner: &VoxInterner<T>,
    node_id: BlockId,
    depth: u8,
    stats: &mut OctreeStats,
) {
    stats.total_nodes += 1;

    while stats.nodes_by_depth.len() <= depth as usize {
        stats.nodes_by_depth.push(0);
    }
    stats.nodes_by_depth[depth as usize] += 1;
    stats.max_depth_reached = stats.max_depth_reached.max(depth);

    if node_id.is_leaf() {
        stats.leaf_nodes += 1;
    } else {
        stats.branch_nodes += 1;
        let children = interner.get_children(&node_id);
        for child in children.iter() {
            if !child.is_empty() {
                collect_stats(interner, *child, depth + 1, stats);
            }
        }
    }
}

/// 每个节点最多 8 个子节点（编译期断言）。
const _: () = assert!(MAX_CHILDREN == 8);
