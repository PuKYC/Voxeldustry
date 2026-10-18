//! 原始体素（raw voxel）通道：把块内部 32³ + 一整圈 halo 的方块 id 直接编成
//! 一个字节缓冲，供 Godot 侧自行做贪婪 meshing。
//!
//! ## 数据布局（A / B 两侧逐字节一致，冻结）
//!
//! - 内部 32³，halo 34³：`RAW_DIM = 32`、`RAW_HALO = 34`、
//!   `RAW_VOXELS = 34³ = 39304`；
//! - 块局部体素 `(x, y, z) ∈ [0, 32)³` 映射到 halo 坐标 `(x+1, y+1, z+1)`；
//! - 扁平下标 `i = hx + 34 * (hy + 34 * hz)`，`hx, hy, hz ∈ [0, 34)`；
//! - 值 = 方块 id（`u8`，0 = 空气）；缺失的邻块 = 空气。
//!
//! halo 是 26 个同 LOD 邻块的一层边界（6 面 + 12 棱 + 8 角）：
//! `extract_raw_halo` 把整圈都填上；`mesh_raw_halo` 直接把 34³ 当作「内部
//! 32³ + 一层边界」的连续三维数组，用 `OccupancyData::halo` 做面 / 棱 / 角
//! AO 采样，不再建任何树。矩形一律留在内部 32³，不跨缝合并。因此本模块与
//! LOD 无关：`lod` 只决定 `extract_raw_halo` 采样包装树的降采样档，
//! 矩形本身是 LOD 无关的整数单元。
//!
//! 确定性：只使用 BTreeMap / 排序 Vec；三平面掩码按 material id 排序遍历，
//! halo 位按 trailing_zeros 升序展开，均不依赖 HashMap 迭代序。

use bevy::math::IVec3;
use voxel::mesh::{
    build_tree_occupancy_lod, mesh_occupancy_with_ao, mesh_occupancy_with_ao_and_skirts,
    tree_voxel_is_solid, FaceNeighbor, MeshBlock, MeshOccupancyData, Neighbourhood,
    OccupancyDataBuilder, MAX_LOD,
};
use voxel::store::{ChunkKey, Lod, VoxInterner, VoxOpsConfig, VoxTree};

use super::pack::pack_rect_batch_with_ao;

/// 块内部每轴的体素数。
pub const RAW_DIM: usize = 32;
/// halo 后每轴的体素数（内部 32 + 两侧各 1 层）。
pub const RAW_HALO: usize = 34;
/// halo 缓冲的总字节数（34³）。
pub const RAW_VOXELS: usize = RAW_HALO * RAW_HALO * RAW_HALO;

/// halo 坐标 `(hx, hy, hz)` 的扁平下标。
///
/// 调用方负责保证三个坐标都落在 `[0, RAW_HALO)`；本函数不做边界检查
/// （越界坐标会按同一线性公式映射，可能与他处别名）。
#[must_use]
pub const fn raw_halo_index(hx: usize, hy: usize, hz: usize) -> usize {
    hx + RAW_HALO * (hy + RAW_HALO * hz)
}

/// 第 `face` 面邻块占据的 `(row, col)` 单元在 halo 中的坐标。
///
/// `face` 顺序与 `voxel::mesh::ExternalPlane` 一致
/// `[YZ+, YZ-, XZ+, XZ-, XY+, XY-]`；`(row, col)` 与
/// `OccupancyData::external` 的行列约定一致。返回值恒落在 `[0, RAW_HALO)`。
#[must_use]
fn halo_coord_for_face(face: usize, row: usize, col: usize) -> (usize, usize, usize) {
    let far = RAW_HALO - 1;
    match face {
        0 => (far, row + 1, col + 1), // +X：hx=33，行 y、列 z
        1 => (0, row + 1, col + 1),   // -X：hx=0
        2 => (col + 1, far, row + 1), // +Y：hy=33，行 z、列 x
        3 => (col + 1, 0, row + 1),   // -Y：hy=0
        4 => (col + 1, row + 1, far), // +Z：hz=33，行 y、列 x
        _ => (col + 1, row + 1, 0),   // -Z：hz=0
    }
}

/// 从一块已 wrap 的树提取「内部 32³ + 整圈 halo（6 面 + 12 棱 + 8 角）」。
///
/// `tree` 是已经 wrap 好的块树（深度 `5 + block.lod`，mesher 采样到 32³），
/// `neighbors` 是同 LOD 的 26 邻树（缺失 = 空气，顺序见 [`Neighbourhood`]）。
///
/// - **内部 32³**：由 `build_tree_occupancy` 的 `per_material`（按 material id
///   排序的平面掩码）反推每个单元的材料。平面索引约定见
///   `crates/voxel/src/mesh/occupancy.rs`：
///   `plane0 = YZ`（法线 x）`index 0*n*n + y*n + z`，位索引 x；
///   `plane1 = XZ`（法线 y）`index 1*n*n + z*n + x`，位索引 y；
///   `plane2 = XY`（法线 z）`index 2*n*n + y*n + x`，位索引 z。
///   三个平面掩码对应位同时为 1 的 material 即该单元的材料，找不到 = 空气 0。
/// - **6 个面**：直接取 `occupancy.external`（位 = 邻块该面占据），非 0 即填
///   `1`（halo 值只参与 AO / 剔除，不参与几何材质）。
/// - **12 棱 + 8 角**：无表化实现——对每个 halo 边界单元，按它落在哪几侧推出
///   所属邻树的符号三元组与局部坐标，直接采样该邻树是否实心。
///
/// 输出长度恒为 [`RAW_VOXELS`]。
#[must_use]
pub fn extract_raw_halo(
    tree: &VoxTree<u8>,
    interner: &VoxInterner<u8>,
    block: MeshBlock,
    neighbors: Neighbourhood<'_>,
) -> Vec<u8> {
    let faces = neighbors.faces().map(FaceNeighbor::from_optional);
    extract_raw_halo_with_faces(tree, interner, block, faces, neighbors)
}

/// 跨 LOD 版本的 [`extract_raw_halo`]：六个面由 [`FaceNeighbor`] 描述。
///
/// 与同 LOD 版本的区别只在 external 面掩码：粗邻居（LOD 大 1）按 2x2 保守
/// 聚合，细邻居（LOD 小 1）按 2x2 任一实心剔除，因此 LOD 接缝两侧的剔除一致。
/// 棱 / 角仍由 `neighbors` 的 26 邻提供（同 LOD 的逐格采样）。
#[must_use]
pub fn extract_raw_halo_with_faces(
    tree: &VoxTree<u8>,
    interner: &VoxInterner<u8>,
    block: MeshBlock,
    faces: [FaceNeighbor<'_>; 6],
    neighbors: Neighbourhood<'_>,
) -> Vec<u8> {
    let occupancy = build_tree_occupancy_lod(tree, interner, block.lod, faces, [false; 6]);
    let n = occupancy.voxels_per_axis as usize;
    let mut out = vec![0u8; RAW_VOXELS];

    // ── 内部 32³：从按 material id 排序的 per_material 掩码反推单元材料 ──
    let plane_len = n.saturating_mul(n);
    for (mi, masks) in occupancy.per_material.iter().enumerate() {
        let Some((material, _)) = occupancy.materials.get(mi) else {
            continue;
        };
        if *material == 0 || masks.len() < 3 * plane_len {
            continue;
        }
        let material = *material as u8;
        let p0 = &masks[0..plane_len];
        let p1 = &masks[plane_len..2 * plane_len];
        let p2 = &masks[2 * plane_len..3 * plane_len];
        let nx = n.min(RAW_DIM);
        for x in 0..nx {
            let bit_x = 1u64 << x;
            for y in 0..nx {
                let bit_y = 1u64 << y;
                let row0 = y * n;
                for z in 0..nx {
                    let bit_z = 1u64 << z;
                    if (p0[row0 + z] & bit_x) != 0
                        && (p1[z * n + x] & bit_y) != 0
                        && (p2[row0 + x] & bit_z) != 0
                    {
                        out[raw_halo_index(x + 1, y + 1, z + 1)] = material;
                    }
                }
            }
        }
    }

    // ── 6 个面：直接取 external 掩码，非 0 即可 ──
    for face in 0..6usize {
        let base = face * n;
        for row in 0..n.min(RAW_DIM) {
            let mut bits = *occupancy.external.get(base + row).unwrap_or(&0);
            while bits != 0 {
                let col = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                if col >= RAW_DIM {
                    continue;
                }
                let (hx, hy, hz) = halo_coord_for_face(face, row, col);
                out[raw_halo_index(hx, hy, hz)] = 1;
            }
        }
    }

    // ── 12 棱 + 8 角：无表化，按符号三元组分侧采样 ──
    //
    // 一个 halo 单元只要有两个及以上坐标落在 -1 / 32，就属于某个棱 / 角邻块。
    // 越界轴固定在该邻块的 0 或 31 层，自由轴沿邻块边界线扫 0..32。
    let mesh_depth = tree.max_depth(block.lod);
    for dx in -1i8..=1 {
        for dy in -1i8..=1 {
            for dz in -1i8..=1 {
                let non_zero = u8::from(dx != 0) + u8::from(dy != 0) + u8::from(dz != 0);
                if non_zero < 2 {
                    continue;
                }
                let Some(neighbor) = neighbors.get(dx, dy, dz) else {
                    continue;
                };
                // 越界轴固定层：局部 0（正侧）/ 31（负侧），halo 33 / 0；
                // 自由轴（该轴符号为 0）从 halo 1 / 局部 0 起扫 0..RAW_DIM。
                let lx = if dx < 0 { (RAW_DIM - 1) as i32 } else { 0 };
                let ly = if dy < 0 { (RAW_DIM - 1) as i32 } else { 0 };
                let lz = if dz < 0 { (RAW_DIM - 1) as i32 } else { 0 };
                let hx = if dx < 0 {
                    0
                } else if dx > 0 {
                    RAW_HALO - 1
                } else {
                    1
                };
                let hy = if dy < 0 {
                    0
                } else if dy > 0 {
                    RAW_HALO - 1
                } else {
                    1
                };
                let hz = if dz < 0 {
                    0
                } else if dz > 0 {
                    RAW_HALO - 1
                } else {
                    1
                };
                let solid = |p: IVec3| tree_voxel_is_solid(neighbor, interner, p, mesh_depth);
                if dx == 0 {
                    // 沿 x 的棱（dy、dz 越界）。
                    for x in 0..RAW_DIM as i32 {
                        if solid(IVec3::new(x, ly, lz)) {
                            out[raw_halo_index(x as usize + 1, hy, hz)] = 1;
                        }
                    }
                } else if dy == 0 {
                    // 沿 y 的棱。
                    for y in 0..RAW_DIM as i32 {
                        if solid(IVec3::new(lx, y, lz)) {
                            out[raw_halo_index(hx, y as usize + 1, hz)] = 1;
                        }
                    }
                } else if dz == 0 {
                    // 沿 z 的棱。
                    for z in 0..RAW_DIM as i32 {
                        if solid(IVec3::new(lx, ly, z)) {
                            out[raw_halo_index(hx, hy, z as usize + 1)] = 1;
                        }
                    }
                } else if solid(IVec3::new(lx, ly, lz)) {
                    // 角：单个单元。
                    out[raw_halo_index(hx, hy, hz)] = 1;
                }
            }
        }
    }

    out
}

/// 内部 32³ 是否存在非空体素（供 build_terrain 跳过全空气块）。
///
/// `blocks.len() != RAW_VOXELS` 时返回 false（不 panic）。
#[must_use]
pub fn raw_halo_has_solid_interior(blocks: &[u8]) -> bool {
    if blocks.len() != RAW_VOXELS {
        return false;
    }
    for hz in 1..RAW_HALO - 1 {
        for hy in 1..RAW_HALO - 1 {
            let start = raw_halo_index(1, hy, hz);
            let end = raw_halo_index(RAW_HALO - 1, hy, hz);
            if blocks[start..end].iter().any(|value| *value != 0) {
                return true;
            }
        }
    }
    false
}

/// 从 halo 缓冲直接做贪婪 meshing（生产 mesher：消费侧现算 39 bit + AO）。
///
/// 直接把 34³ 当作「内部 32³ + 一层边界」的连续三维数组：
/// - 内部 32³ 正向编码进 `global` / `per_material` / `global_active`；
/// - 6 张 external 面掩码从 halo 的 6 个边界面直接读；
/// - `halo = Some(blocks)`，AO 采样走该连续数组（面 / 棱 / 角统一）；
/// - 不再建任何 32³ 树或 6 棵薄邻树，最后走 [`mesh_occupancy_with_ao`]。
///
/// `lod` **不参与**网格化（矩形是 LOD 无关的整数单元，缩放由载荷的 lod 在渲染
/// 侧做），仅为调用方语义保留。`blocks.len() != RAW_VOXELS` 时返回空 Vec
/// （不 panic）。
#[must_use]
pub fn mesh_raw_halo(lod: u8, blocks: &[u8]) -> Vec<u64> {
    debug_assert!(lod <= MAX_LOD, "lod exceeds MAX_LOD");
    let Some(occupancy) = raw_halo_occupancy(blocks) else {
        return Vec::new();
    };
    // 原点只影响 RectBatch.origin（单 batch 排序键的前 3 项为常量），
    // 取规范原点 0 即可保证输出与直接 mesher 逐位一致。
    let block = MeshBlock::new(ChunkKey::new(0, 0, 0), Lod::new(0));
    pack_rect_batch_with_ao(&mesh_occupancy_with_ao(&occupancy, block.origin, block.lod))
}

/// 与 [`mesh_raw_halo`] 相同，额外在 `skirt_mask` 置位的面上追加跨 LOD 接缝
/// 裙边（AO 满亮）。`skirt_mask == 0` 时逐位等于 [`mesh_raw_halo`]。
///
/// bit `face` 对应 [`voxel::mesh::ExternalPlane`] 顺序 `[+x, -x, +y, -y, +z, -z]`；
/// 只有跨 LOD 面才应置位（由调用方按 2:1 面邻居报告决定）。
#[must_use]
pub fn mesh_raw_halo_with_skirts(lod: u8, blocks: &[u8], skirt_mask: u8) -> Vec<u64> {
    debug_assert!(lod <= MAX_LOD, "lod exceeds MAX_LOD");
    let Some(occupancy) = raw_halo_occupancy(blocks) else {
        return Vec::new();
    };
    // 原点只影响 RectBatch.origin（单 batch 排序键的前 3 项为常量），
    // 取规范原点 0 即可保证输出与直接 mesher 逐位一致。
    let block = MeshBlock::new(ChunkKey::new(0, 0, 0), Lod::new(0));
    pack_rect_batch_with_ao(&mesh_occupancy_with_ao_and_skirts(
        &occupancy,
        block.origin,
        block.lod,
        skirt_mask,
    ))
}

/// 从 34³ halo 缓冲构造 [`OccupancyData`]（内部 32³ + 6 external + padded halo）。
#[must_use]
fn raw_halo_occupancy(blocks: &[u8]) -> Option<MeshOccupancyData> {
    if blocks.len() != RAW_VOXELS {
        return None;
    }
    let n = RAW_DIM;
    let plane_len = n * n;
    let mut builder = OccupancyDataBuilder::new(n as u32);

    // ── 内部 32³：正向编码 global / per_material / global_active ──
    for z in 0..n {
        for y in 0..n {
            let row = y * n;
            for x in 0..n {
                let material = blocks[raw_halo_index(x + 1, y + 1, z + 1)];
                if material == 0 {
                    continue;
                }
                let xb = 1u64 << x;
                let yb = 1u64 << y;
                let zb = 1u64 << z;
                // YZ：法线 x，行 y，列 z。
                builder.global[row + z] |= xb;
                // XZ：法线 y，行 z，列 x。
                builder.global[plane_len + z * n + x] |= yb;
                // XY：法线 z，行 y，列 x。
                builder.global[2 * plane_len + row + x] |= zb;
                builder.global_active[0] |= yb; // YZ rows
                builder.global_active[1] |= zb; // YZ cols
                builder.global_active[2] |= zb; // XZ rows
                builder.global_active[3] |= xb; // XZ cols
                builder.global_active[4] |= yb; // XY rows
                builder.global_active[5] |= xb; // XY cols

                let per = builder
                    .per_material
                    .entry(material as usize)
                    .or_insert_with(|| vec![0u64; 3 * plane_len]);
                per[row + z] |= xb;
                per[plane_len + z * n + x] |= yb;
                per[2 * plane_len + row + x] |= zb;
                *builder.materials.entry(material as usize).or_insert(0) += 1;
            }
        }
    }

    // ── 6 张 external 面掩码：从 halo 边界层直接读 ──
    for face in 0..6usize {
        let base = face * n;
        for row in 0..n {
            let mut bits = 0u64;
            for col in 0..n {
                let (hx, hy, hz) = halo_coord_for_face(face, row, col);
                if blocks[raw_halo_index(hx, hy, hz)] != 0 {
                    bits |= 1u64 << col;
                }
            }
            builder.external[base + row] = bits;
        }
        builder.external_exists[face] = true;
    }

    // ── padded halo：直接搬 34³（一次 39 KiB 分配，取代旧路径的整棵建树）──
    builder.halo = Some(blocks.to_vec().into_boxed_slice());

    Some(builder.build())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::presentation::voxel::{unpack_ao, unpack_rect};
    use voxel::mesh::{extract_block_tree_with_ao, wrap_block, BASE_DEPTH};
    use voxel::store::{MaxDepth, VoxOpsBulkWrite, VoxOpsWrite};

    fn key(x: i32, y: i32, z: i32) -> ChunkKey {
        ChunkKey::new(x, y, z)
    }

    fn new_interner() -> VoxInterner<u8> {
        VoxInterner::with_memory_budget(16 * 1024 * 1024)
    }

    /// 确定性 LCG，替代测试中的 rand 依赖。
    fn lcg(state: &mut u64) -> u32 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (*state >> 33) as u32
    }

    /// 用一个稀疏的确定性伪随机图案构建一个 base 子块（约 1/6 实心）。
    fn random_chunk(interner: &mut VoxInterner<u8>, seed: u64) -> VoxTree<u8> {
        let mut state = seed | 1;
        let mut tree = VoxTree::<u8>::new(MaxDepth::new(BASE_DEPTH));
        for z in 0..RAW_DIM as i32 {
            for y in 0..RAW_DIM as i32 {
                for x in 0..RAW_DIM as i32 {
                    let r = lcg(&mut state);
                    if r % 6 == 0 {
                        let material = 1 + (r % 3) as u8;
                        let _ = tree.set(interner, IVec3::new(x, y, z), material);
                    }
                }
            }
        }
        tree
    }

    /// 生成并 wrap 一个 lod 块；lod 越大越可能整块子块留空，控制测试成本。
    fn wrap_random_block(interner: &mut VoxInterner<u8>, lod: u8, seed: u64) -> VoxTree<u8> {
        let span = 1i32 << lod;
        let mut chunks: BTreeMap<ChunkKey, VoxTree<u8>> = BTreeMap::new();
        for cy in 0..span {
            for cz in 0..span {
                for cx in 0..span {
                    let idx = (cx + cy * span + cz * span * span) as u64;
                    if lod >= 2 && idx % 3 != 0 {
                        continue;
                    }
                    let chunk_seed = seed
                        .wrapping_mul(131)
                        .wrapping_add((idx + 1).wrapping_mul(2654435761));
                    chunks.insert(key(cx, cy, cz), random_chunk(interner, chunk_seed | 1));
                }
            }
        }
        wrap_block(
            &chunks,
            interner,
            MeshBlock::new(key(0, 0, 0), Lod::new(lod)),
        )
    }

    /// 生成并 wrap 一个邻块组。
    fn wrap_random_neighbor(
        interner: &mut VoxInterner<u8>,
        origin: ChunkKey,
        lod: u8,
        seed: u64,
    ) -> VoxTree<u8> {
        let span = 1i32 << lod;
        let mut chunks: BTreeMap<ChunkKey, VoxTree<u8>> = BTreeMap::new();
        for cy in 0..span {
            for cz in 0..span {
                for cx in 0..span {
                    let idx = (cx + cy * span + cz * span * span) as u64;
                    let chunk_seed = seed
                        .wrapping_mul(17)
                        .wrapping_add((idx + 1).wrapping_mul(40503));
                    chunks.insert(
                        key(origin.x + cx, origin.y + cy, origin.z + cz),
                        random_chunk(interner, chunk_seed | 1),
                    );
                }
            }
        }
        wrap_block(&chunks, interner, MeshBlock::new(origin, Lod::new(lod)))
    }

    #[test]
    fn raw_halo_index_layout_and_bounds() {
        assert_eq!(RAW_DIM, 32);
        assert_eq!(RAW_HALO, 34);
        assert_eq!(RAW_VOXELS, 34 * 34 * 34);
        assert_eq!(raw_halo_index(0, 0, 0), 0);
        assert_eq!(raw_halo_index(1, 0, 0), 1);
        assert_eq!(raw_halo_index(0, 1, 0), 34);
        assert_eq!(raw_halo_index(0, 0, 1), 34 * 34);
        assert_eq!(raw_halo_index(33, 33, 33), RAW_VOXELS - 1);
        // 内部角映射：局部 (0,0,0) -> halo (1,1,1)。
        assert_eq!(raw_halo_index(1, 1, 1), 1 + 34 + 34 * 34);
        // 尺寸不符（截断 / 空 / 超长）一律退化为空，而不是 panic。
        assert!(mesh_raw_halo(0, &[0u8; RAW_VOXELS - 1]).is_empty());
        assert!(mesh_raw_halo(0, &[]).is_empty());
        assert!(mesh_raw_halo(0, &vec![0u8; RAW_VOXELS + 1]).is_empty());
    }

    #[test]
    fn raw_halo_has_solid_interior_only_scans_interior() {
        let mut blocks = vec![0u8; RAW_VOXELS];
        assert!(!raw_halo_has_solid_interior(&blocks));
        // 只在 halo 层（hx=0）放实心：内部仍为空。
        blocks[raw_halo_index(0, 1, 1)] = 1;
        assert!(!raw_halo_has_solid_interior(&blocks));
        // 内部放一个实心。
        blocks[raw_halo_index(1, 1, 1)] = 1;
        assert!(raw_halo_has_solid_interior(&blocks));
        assert!(!raw_halo_has_solid_interior(&[0u8; 3]));
    }

    #[test]
    fn all_air_meshes_empty() {
        let blocks = vec![0u8; RAW_VOXELS];
        assert!(mesh_raw_halo(0, &blocks).is_empty());
    }

    #[test]
    fn single_voxel_has_six_faces() {
        let mut blocks = vec![0u8; RAW_VOXELS];
        blocks[raw_halo_index(1, 1, 1)] = 7;
        let words = mesh_raw_halo(0, &blocks);
        assert_eq!(words.len(), 6, "单个体素应产生 6 个 1x1 面");
        assert!(words.iter().all(|w| unpack_rect(*w).material == 7));
    }

    #[test]
    fn solid_block_has_only_six_boundary_faces() {
        let mut blocks = vec![0u8; RAW_VOXELS];
        for hz in 1..RAW_HALO - 1 {
            for hy in 1..RAW_HALO - 1 {
                for hx in 1..RAW_HALO - 1 {
                    blocks[raw_halo_index(hx, hy, hz)] = 3;
                }
            }
        }
        let words = mesh_raw_halo(0, &blocks);
        assert_eq!(words.len(), 6, "满实心块只应有 6 个边界面，无内部面");
        assert!(words.iter().all(|w| unpack_rect(*w).material == 3));
    }

    /// 黄金等价（最重要）：raw 通道与直接 mesher 逐位一致。
    ///
    /// 覆盖若干 LOD 的随机树（含降采样），以及 lod 0 的 6 邻 seam。
    #[test]
    fn raw_halo_equivalence_matches_direct_mesher() {
        for lod in 0..=2u8 {
            let mut interner = new_interner();
            let tree = wrap_random_block(&mut interner, lod, 0x1234 + u64::from(lod));
            let block = MeshBlock::new(key(0, 0, 0), Lod::new(lod));
            let external: [Option<&VoxTree<u8>>; 6] = [None; 6];

            let halo =
                extract_raw_halo(&tree, &interner, block, Neighbourhood::from_faces(external));
            assert_eq!(halo.len(), RAW_VOXELS, "lod {lod}: halo 长度");
            let via_raw = mesh_raw_halo(lod, &halo);
            let via_ref = pack_rect_batch_with_ao(&extract_block_tree_with_ao(
                &tree, &interner, block, external,
            ));
            assert_eq!(
                via_raw, via_ref,
                "lod {lod}: 从 halo 反推的 mesh 必须与直接 mesher 逐位一致"
            );
        }
    }

    /// 6 邻 seam 的 halo 映射与直接 mesher 一致（覆盖 lod>0 的降采样邻树）。
    #[test]
    fn raw_halo_external_seam_matches_direct_mesher() {
        for lod in 0..=1u8 {
            let span = 1i32 << lod;
            let mut interner = new_interner();
            let tree = wrap_random_block(&mut interner, lod, 0xABCD + u64::from(lod));
            let offline = [
                key(span, 0, 0),
                key(-span, 0, 0),
                key(0, span, 0),
                key(0, -span, 0),
                key(0, 0, span),
                key(0, 0, -span),
            ];
            let neighbor_trees: Vec<VoxTree<u8>> = offline
                .iter()
                .enumerate()
                .map(|(i, origin)| {
                    wrap_random_neighbor(
                        &mut interner,
                        *origin,
                        lod,
                        0x500 + i as u64 + u64::from(lod),
                    )
                })
                .collect();
            let external: [Option<&VoxTree<u8>>; 6] = [
                Some(&neighbor_trees[0]),
                Some(&neighbor_trees[1]),
                Some(&neighbor_trees[2]),
                Some(&neighbor_trees[3]),
                Some(&neighbor_trees[4]),
                Some(&neighbor_trees[5]),
            ];

            let block = MeshBlock::new(key(0, 0, 0), Lod::new(lod));
            let halo =
                extract_raw_halo(&tree, &interner, block, Neighbourhood::from_faces(external));
            let via_raw = mesh_raw_halo(lod, &halo);
            let via_ref = pack_rect_batch_with_ao(&extract_block_tree_with_ao(
                &tree, &interner, block, external,
            ));
            assert_eq!(
                via_raw, via_ref,
                "lod {lod}: 带 6 邻 seam 时 raw 通道必须逐位一致"
            );
        }
    }

    /// 一个只含单个体素的同 LOD 邻树（用于棱 / 角邻居测试）。
    fn single_voxel_neighbor(
        interner: &mut VoxInterner<u8>,
        origin: ChunkKey,
        voxel: IVec3,
        material: u8,
    ) -> VoxTree<u8> {
        let mut chunk = VoxTree::<u8>::new(MaxDepth::new(BASE_DEPTH));
        let _ = chunk.set(interner, voxel, material);
        let mut chunks: BTreeMap<ChunkKey, VoxTree<u8>> = BTreeMap::new();
        chunks.insert(origin, chunk);
        wrap_block(&chunks, interner, MeshBlock::new(origin, Lod::new(0)))
    }

    /// `extract_raw_halo` 必须把 12 棱 / 8 角的 halo 单元也填上。
    #[test]
    fn extract_raw_halo_fills_edges_and_corners_from_diagonal_neighbours() {
        let mut interner = new_interner();
        let mut own = VoxTree::<u8>::new(MaxDepth::new(BASE_DEPTH));
        let _ = own.set(&mut interner, IVec3::new(0, 0, 0), 1);
        let block = MeshBlock::new(key(0, 0, 0), Lod::new(0));

        // +X+Y 棱邻块：它本地 (0,0,5) 实心 -> 本块 halo (33,33,6)。
        let edge = single_voxel_neighbor(&mut interner, key(1, 1, 0), IVec3::new(0, 0, 5), 2);
        // -X-Y-Z 角邻块：它本地 (31,31,31) 实心 -> 本块 halo (0,0,0)。
        let corner =
            single_voxel_neighbor(&mut interner, key(-1, -1, -1), IVec3::new(31, 31, 31), 3);

        let mut nb = Neighbourhood::none();
        nb.set(1, 1, 0, &edge);
        nb.set(-1, -1, -1, &corner);
        let halo = extract_raw_halo(&own, &interner, block, nb);
        assert_ne!(
            halo[raw_halo_index(RAW_HALO - 1, RAW_HALO - 1, 6)],
            0,
            "+X+Y 棱邻块必须填 halo 棱单元"
        );
        assert_ne!(
            halo[raw_halo_index(0, 0, 0)],
            0,
            "-X-Y-Z 角邻块必须填 halo 角单元"
        );

        // 只有 6 面（无对角）时，棱 / 角仍是空气。
        let faces_only = extract_raw_halo(&own, &interner, block, Neighbourhood::none());
        assert_eq!(faces_only[raw_halo_index(RAW_HALO - 1, RAW_HALO - 1, 6)], 0);
        assert_eq!(faces_only[raw_halo_index(0, 0, 0)], 0);
    }

    /// 独立暴力参考：直接从 34³ halo 数组按 MC 公式算某条 1x1 面的四角 AO。
    ///
    /// 刻意不调用被测 mesher 的任何函数；坐标映射独立重写一遍。
    fn brute_face_ao(blocks: &[u8], plane: usize, row: i32, col: i32, layer: i32) -> [u8; 4] {
        let sample = |r: i32, c: i32| -> bool {
            let (x, y, z) = match plane {
                0 => (layer, r, c), // YZ：行 y、列 z
                1 => (c, layer, r), // XZ：行 z、列 x
                _ => (c, r, layer), // XY：行 y、列 x
            };
            if x < -1
                || x > RAW_DIM as i32
                || y < -1
                || y > RAW_DIM as i32
                || z < -1
                || z > RAW_DIM as i32
            {
                return false;
            }
            blocks[raw_halo_index((x + 1) as usize, (y + 1) as usize, (z + 1) as usize)] != 0
        };
        let level = |a: bool, b: bool, c: bool| {
            if a && b {
                0
            } else {
                3 - (a as u8 + b as u8 + c as u8)
            }
        };
        let at = |dr: i32, dc: i32| sample(row + dr, col + dc);
        [
            level(at(-1, 0), at(0, -1), at(-1, -1)),
            level(at(-1, 0), at(0, 1), at(-1, 1)),
            level(at(1, 0), at(0, -1), at(1, -1)),
            level(at(1, 0), at(0, 1), at(1, 1)),
        ]
    }

    /// 棱 / 角处有实心邻居时，`mesh_raw_halo` 的边界 AO 必须与暴力参考逐位一致。
    #[test]
    fn edge_corner_ao_matches_brute_force() {
        let mut blocks = vec![0u8; RAW_VOXELS];
        // 块角 (0,0,0) 放一个体素 -> halo (1,1,1)。
        blocks[raw_halo_index(1, 1, 1)] = 1;
        // 在 -X/-Y/-Z 侧放 3 个棱邻居 + 1 个角邻居（都是对角，不剔除任何面）。
        for &(hx, hy, hz) in &[(0usize, 0usize, 1usize), (0, 1, 0), (1, 0, 0), (0, 0, 0)] {
            blocks[raw_halo_index(hx, hy, hz)] = 2;
        }

        let words = mesh_raw_halo(0, &blocks);

        // (plane, dir, slice, row, col, dir_pos, ao_layer)
        let cases = [
            (0usize, 1u8, 0u8, 0usize, 0usize, false, -1i32), // -X
            (1, 1, 0, 0, 0, false, -1),                       // -Y
            (2, 1, 0, 0, 0, false, -1),                       // -Z
            (0, 0, 1, 0, 0, true, 1),                         // +X
            (1, 0, 1, 0, 0, true, 1),                         // +Y
            (2, 0, 1, 0, 0, true, 1),                         // +Z
        ];
        for (plane, dir, slice, row, col, dir_pos, layer) in cases {
            let expected = brute_face_ao(&blocks, plane, row as i32, col as i32, layer);
            let word = words
                .iter()
                .find(|w| {
                    let r = unpack_rect(**w);
                    r.plane == plane as u8
                        && r.dir == dir
                        && r.slice == slice
                        && r.row as usize == row
                        && r.col as usize == col
                        && r.w == 1
                        && r.h == 1
                })
                .unwrap_or_else(|| panic!("face not found: plane={plane} dir={dir} slice={slice}"));
            assert_eq!(
                unpack_ao(*word),
                expected,
                "AO mismatch plane={plane} dir={dir}"
            );
        }

        // 确认本用例真的让 -X 面读到了棱 / 角邻居（否则测试没覆盖新行为）。
        let neg_x = words
            .iter()
            .find(|w| {
                let r = unpack_rect(**w);
                r.plane == 0 && r.dir == 1 && r.slice == 0
            })
            .expect("-X 面必须存在");
        assert_ne!(unpack_ao(*neg_x), [3, 3, 3, 3], "-X 面应被棱 / 角邻居压暗");
    }

    /// 生成一个 origin/lod 的实心邻居块（所有基础子块填 material）。
    fn solid_block(
        interner: &mut VoxInterner<u8>,
        origin: ChunkKey,
        lod: u8,
        material: u8,
    ) -> VoxTree<u8> {
        let span = 1i32 << lod;
        let mut chunks: BTreeMap<ChunkKey, VoxTree<u8>> = BTreeMap::new();
        for cy in 0..span {
            for cz in 0..span {
                for cx in 0..span {
                    let mut chunk = VoxTree::<u8>::new(MaxDepth::new(BASE_DEPTH));
                    chunk.fill(interner, material);
                    chunks.insert(key(origin.x + cx, origin.y + cy, origin.z + cz), chunk);
                }
            }
        }
        wrap_block(&chunks, interner, MeshBlock::new(origin, Lod::new(lod)))
    }

    /// 跨 LOD 裙边：只在 `skirt_mask` 置位的面出现，AO 满亮；mask=0 时与不开
    /// 裙边逐字相等（保护黄金测试）。
    #[test]
    fn skirts_only_on_masked_faces_and_ao_full_bright() {
        let mut blocks = vec![0u8; RAW_VOXELS];
        // 体素 (31,10,10)：+X 面被邻居 (halo hx=33) 遮挡，其余面暴露。
        blocks[raw_halo_index(32, 11, 11)] = 5;
        blocks[raw_halo_index(RAW_HALO - 1, 11, 11)] = 7;

        let baseline = mesh_raw_halo(0, &blocks);
        assert!(
            !baseline.iter().any(|w| {
                let r = unpack_rect(*w);
                r.plane == 0 && r.dir == 0
            }),
            "主 mesher 已剔除 +X 面"
        );

        // +X（bit 0）置位：补一条满亮裙边。
        let with = mesh_raw_halo_with_skirts(0, &blocks, 1 << 0);
        assert_eq!(with.len(), baseline.len() + 1, "只应多一条裙边 rect");
        let skirt = with
            .iter()
            .find(|w| {
                let r = unpack_rect(**w);
                r.plane == 0 && r.dir == 0 && r.slice == 32 && r.row == 10 && r.col == 10
            })
            .expect("+X 裙边必须存在");
        let rect = unpack_rect(*skirt);
        assert_eq!((rect.w, rect.h, rect.material), (1, 1, 5));
        assert_eq!(unpack_ao(*skirt), [3, 3, 3, 3], "裙边 AO 必须满亮");

        // 只在指定面出现：置 -Z（bit 5）不应产生 +X 裙边。
        let other = mesh_raw_halo_with_skirts(0, &blocks, 1 << 5);
        assert!(
            !other.iter().any(|w| {
                let r = unpack_rect(*w);
                r.plane == 0 && r.dir == 0 && r.slice == 32
            }),
            "未置位的面不得出现裙边"
        );

        // mask = 0 必须与旧输出逐字相等。
        assert_eq!(mesh_raw_halo_with_skirts(0, &blocks, 0), baseline);
    }
}
