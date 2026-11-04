//! The three-plane global occupancy masks.
//!
//! Each plane family gets n * n u64 masks, one per in-plane cell, whose bits
//! index the normal axis.  [`super::data::OccupancyData`] also keeps active
//! row/column extents so the greedy pass can skip empty slices.

use glam::UVec3;

use crate::store::{BlockId, MaxDepth, VoxInterner, VoxelTrait};

use super::data::OccupancyDataBuilder;
use super::full_mask;
use super::local_tree::{InternerAccess, NodeAccess};

/// 把一个立方体区域（边长 `side`，最小角 `region_offset`）写进三层掩码。
///
/// # 参数
///
/// * `builder`      - 目标构建器，分辨率 n 必须等于 1 << max_depth。
/// * `region_offset`- 区域最小角在网格内的坐标（0..n）。
/// * `side`         - 区域边长；等于 n 表示整块都是该材料。
/// * `material_id`  - 写入的材料 id，同时累加体素数。
///
/// # 不变量
///
/// `side == n` 时整层写满并一次性置位全部 global_active；否则按位写入
/// YZ / XZ / XY 三个平面，并把区域覆盖的行列合并进 global_active。
/// `_access` 目前不参与计算，保留它是为了让所有提取器共用同一签名。
fn fill_masks_for_region<T: VoxelTrait, A: NodeAccess<T>>(
    _access: &A,
    builder: &mut OccupancyDataBuilder,
    region_offset: UVec3,
    side: u32,
    material_id: usize,
) {
    let n = builder.voxels_per_axis as usize;
    let side = side as usize;
    let volume = side * side * side;

    builder
        .materials
        .entry(material_id)
        .and_modify(|count| *count += volume)
        .or_insert(volume);

    if side != n {
        let plane_len = 3 * n * n;
        let occupancy = builder
            .per_material
            .entry(material_id)
            .or_insert_with(|| vec![0u64; plane_len]);

        let run_mask = (1u64 << side) - 1;

        let start_x = region_offset.x as usize;
        let x_mask = run_mask << start_x;
        let start_y = region_offset.y as usize;
        let y_mask = run_mask << start_y;
        let start_z = region_offset.z as usize;
        let z_mask = run_mask << start_z;

        // YZ masks
        builder.global_active[0] |= y_mask; // rows
        builder.global_active[1] |= z_mask; // cols
                                            // XZ masks
        builder.global_active[2] |= z_mask; // rows
        builder.global_active[3] |= x_mask; // cols
                                            // XY masks
        builder.global_active[4] |= y_mask; // rows
        builder.global_active[5] |= x_mask; // cols

        for i in 0..side {
            let z = start_z + i;
            let y = start_y + i;

            let index_base_y = n * n + z * n + start_x;
            let index_base_z = 2 * n * n + y * n + start_x;
            let index_base_x = y * n + start_z;

            for j in 0..side {
                let index_y = index_base_y + j;
                builder.global[index_y] |= y_mask;
                occupancy[index_y] |= y_mask;

                let index_z = index_base_z + j;
                builder.global[index_z] |= z_mask;
                occupancy[index_z] |= z_mask;

                let index_x = index_base_x + j;
                builder.global[index_x] |= x_mask;
                occupancy[index_x] |= x_mask;
            }
        }
    } else {
        let plane_len = 3 * n * n;
        let full = full_mask(n);
        builder
            .per_material
            .insert(material_id, vec![full; plane_len]);
        builder.global.fill(full);
        builder.global_active = [full; 6];
    }
}

/// Generic occupancy extraction from any NodeAccess tree.
///
/// 深度优先遍历整棵八叉树：非叶且未到 max_depth 的节点压栈，其余节点
/// （叶，或已到采样深度的分支）整体作为一个边长为 `1 << (max_depth - depth)`
/// 的立方体交给 fill_masks_for_region。空节点整棵跳过。
///
/// # 参数
///
/// `root` 必须是 `access` 的合法句柄；`offset` 是子树最小角在目标网格中的
/// 偏移，用于把 wrap 起来的多子块组拼进一块占位网格。
pub(crate) fn generate_occupancy_masks_generic<T: VoxelTrait, A: NodeAccess<T>>(
    access: &A,
    root: A::Id,
    builder: &mut OccupancyDataBuilder,
    max_depth: MaxDepth,
    offset: UVec3,
) {
    if access.is_empty(root) {
        return;
    }

    let max_depth = max_depth.max();
    assert_eq!(
        builder.voxels_per_axis,
        1u32 << max_depth,
        "builder resolution does not match max_depth"
    );

    let default_t = T::default();

    if access.is_leaf(root) {
        let value = access.value(root);
        if value != default_t {
            fill_masks_for_region(
                access,
                builder,
                offset,
                1u32 << max_depth,
                value.material_id(),
            );
        }
        return;
    }

    let mut stack: Vec<(A::Id, UVec3, u32)> = Vec::with_capacity(64);
    stack.push((root, UVec3::ZERO, 0));

    while let Some((node, pos, depth)) = stack.pop() {
        if !access.is_leaf(node) && depth < max_depth as u32 {
            let child_cube_half_side = 1u32 << (max_depth as u32 - depth - 1);
            for index in 0..8usize {
                let child = access.child(node, index);
                if !access.is_empty(child) {
                    let x = (index & 1) as u32 * child_cube_half_side;
                    let y = ((index >> 1) & 1) as u32 * child_cube_half_side;
                    let z = ((index >> 2) & 1) as u32 * child_cube_half_side;
                    stack.push((child, pos + UVec3::new(x, y, z), depth + 1));
                }
            }
        } else {
            let value = access.value(node);
            if value != default_t {
                let cube_side = 1u32 << (max_depth as u32 - depth);
                fill_masks_for_region(
                    access,
                    builder,
                    offset + pos,
                    cube_side,
                    value.material_id(),
                );
            }
        }
    }
}

/// Public interner-backed occupancy extraction (upstream entry point).
///
/// # 参数
///
/// * `root_id`   - 待提取子树的根；空根直接返回，不写任何掩码。
/// * `max_depth` - 采样深度，决定网格分辨率 `1 << max_depth`（须与 builder 一致）。
/// * `offset`    - 子树在网格中的偏移，非零时表示这是更大块的一部分。
///
/// # Panics
///
/// `max_depth` 与 `builder.voxels_per_axis` 不一致时 panic。
pub fn generate_occupancy_masks<T: VoxelTrait>(
    interner: &VoxInterner<T>,
    builder: &mut OccupancyDataBuilder,
    root_id: &BlockId,
    max_depth: MaxDepth,
    offset: UVec3,
) {
    generate_occupancy_masks_generic(
        &InternerAccess(interner),
        *root_id,
        builder,
        max_depth,
        offset,
    );
}
