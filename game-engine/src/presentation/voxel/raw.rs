//! 原始体素（raw voxel）通道：把块内部 32³ + halo 层的方块 id 直接编成一个
//! 字节缓冲，供 Godot 侧自行做贪婪 meshing。
//!
//! ## 数据布局（A / B 两侧逐字节一致，冻结）
//!
//! - 内部 32³，halo 34³：`RAW_DIM = 32`、`RAW_HALO = 34`、
//!   `RAW_VOXELS = 34³ = 39304`；
//! - 块局部体素 `(x, y, z) ∈ [0, 32)³` 映射到 halo 坐标 `(x+1, y+1, z+1)`；
//! - 扁平下标 `i = hx + 34 * (hy + 34 * hz)`，`hx, hy, hz ∈ [0, 34)`；
//! - 值 = 方块 id（`u8`，0 = 空气）；缺失的邻块 = 空气。
//!
//! halo 只是 6 个同 LOD 邻块的一层边界，仅用于剔除块边界面；矩形一律留在内部
//! 32³，不跨缝合并。因此本模块与 LOD 无关：`extract_raw_halo` 先把
//! depth-`(5 + lod)` 的包装树按 mesher 的分辨率降采样成 32³ 的方块 id 网格，
//! `mesh_raw_halo` 再在 depth-5 的树上按 lod 0 网格化，二者输出逐位一致
//! （见本文件 tests 的黄金等价测试）。
//!
//! 确定性：只使用 BTreeMap / 排序 Vec；三平面掩码按 material id 排序遍历，
//! halo 位按 trailing_zeros 升序展开，均不依赖 HashMap 迭代序。

use bevy::math::IVec3;
use voxel::mesh::{
    build_tree_occupancy, extract_block_tree_with_ao, MeshBlock, BASE_DEPTH, MAX_LOD,
};
use voxel::store::{ChunkKey, Lod, MaxDepth, VoxInterner, VoxOpsWrite, VoxTree};

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

/// 第 `face` 面邻块的 `(row, col)` 单元在邻树局部坐标中的体素位置。
#[must_use]
fn thin_neighbor_voxel(face: usize, row: usize, col: usize) -> IVec3 {
    let last = (RAW_DIM - 1) as i32;
    let row = row as i32;
    let col = col as i32;
    match face {
        0 => IVec3::new(0, row, col),    // +X 邻块的 x=0 层
        1 => IVec3::new(last, row, col), // -X 邻块的 x=31 层
        2 => IVec3::new(col, 0, row),    // +Y：行 z、列 x
        3 => IVec3::new(col, last, row), // -Y
        4 => IVec3::new(col, row, 0),    // +Z：行 y、列 x
        _ => IVec3::new(col, row, last), // -Z
    }
}

/// 从一块已 wrap 的树提取「内部 32³ + 6 面一层 halo」的原始方块 id。
///
/// `tree` 是已经 wrap 好的块树（深度 `5 + block.lod`，mesher 采样到 32³），
/// `external` 是 6 个同 LOD 邻树，顺序
/// `[YZ+, YZ-, XZ+, XZ-, XY+, XY-]`。
///
/// - **内部 32³**：由 `build_tree_occupancy` 的 `per_material`（按 material id
///   排序的平面掩码）反推每个单元的材料。平面索引约定见
///   `crates/voxel/src/mesh/occupancy.rs`：
///   `plane0 = YZ`（法线 x）`index 0*n*n + y*n + z`，位索引 x；
///   `plane1 = XZ`（法线 y）`index 1*n*n + z*n + x`，位索引 y；
///   `plane2 = XY`（法线 z）`index 2*n*n + y*n + x`，位索引 z。
///   三个平面掩码对应位同时为 1 的 material 即该单元的材料，找不到 = 空气 0。
/// - **halo 层**：直接取 `occupancy.external`（位 = 邻块该面占据），非 0 即可，
///   这里统一填 `1`（halo 只用于剔除，值本身不参与几何）。
///
/// 输出长度恒为 [`RAW_VOXELS`]。
#[must_use]
pub fn extract_raw_halo(
    tree: &VoxTree<u8>,
    interner: &VoxInterner<u8>,
    block: MeshBlock,
    external: [Option<&VoxTree<u8>>; 6],
) -> Vec<u8> {
    let occupancy = build_tree_occupancy(tree, interner, block.lod, external, [false; 6]);
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

    // ── halo 层：直接取 external 掩码，非 0 即可 ──
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

/// 从 halo 缓冲直接做贪婪 meshing（把 meshing 放到消费侧的参考 / 校验入口）。
///
/// 内部 32³ 的非 0 单元重建一棵 depth-5 的体素树；halo 六面各重建一棵只含相邻
/// 一层的「薄」邻树；再调用
/// `extract_block_tree_with_ao(&tree, &interner, MeshBlock::new(origin, Lod::new(0)), external)`
/// 并 `pack_rect_batch_with_ao` 成 39 bit + AO 的 u64 流。
///
/// `lod` **不参与**网格化（矩形是 LOD 无关的整数单元，缩放由载荷的 lod 在渲染
/// 侧做），仅为调用方语义保留。`blocks.len() != RAW_VOXELS` 时返回空 Vec
/// （不 panic）。
#[must_use]
pub fn mesh_raw_halo(lod: u8, blocks: &[u8]) -> Vec<u64> {
    debug_assert!(lod <= MAX_LOD, "lod exceeds MAX_LOD");
    if blocks.len() != RAW_VOXELS {
        return Vec::new();
    }

    // 局部 interner：容量会按需自动增长，4 MiB 起步足够一棵 32³ + 6 薄邻树。
    let mut interner = VoxInterner::<u8>::with_memory_budget(4 * 1024 * 1024);
    let mut tree = VoxTree::<u8>::new(MaxDepth::new(BASE_DEPTH));

    for z in 0..RAW_DIM {
        for y in 0..RAW_DIM {
            for x in 0..RAW_DIM {
                let value = blocks[raw_halo_index(x + 1, y + 1, z + 1)];
                if value != 0 {
                    let _ = tree.set(
                        &mut interner,
                        IVec3::new(x as i32, y as i32, z as i32),
                        value,
                    );
                }
            }
        }
    }

    // 6 棵薄邻树：只填与本块相邻的那一层 halo 值。
    let mut thin: [VoxTree<u8>; 6] =
        std::array::from_fn(|_| VoxTree::<u8>::new(MaxDepth::new(BASE_DEPTH)));
    for (face, neighbor) in thin.iter_mut().enumerate() {
        for row in 0..RAW_DIM {
            for col in 0..RAW_DIM {
                let (hx, hy, hz) = halo_coord_for_face(face, row, col);
                let value = blocks[raw_halo_index(hx, hy, hz)];
                if value != 0 {
                    let _ = neighbor.set(&mut interner, thin_neighbor_voxel(face, row, col), value);
                }
            }
        }
    }

    let external: [Option<&VoxTree<u8>>; 6] = [
        Some(&thin[0]),
        Some(&thin[1]),
        Some(&thin[2]),
        Some(&thin[3]),
        Some(&thin[4]),
        Some(&thin[5]),
    ];

    // 原点只影响 RectBatch.origin（单 batch 排序键的前 3 项为常量），
    // 取规范原点 0 即可保证输出与直接 mesher 逐位一致。
    let block = MeshBlock::new(ChunkKey::new(0, 0, 0), Lod::new(0));
    let batch = extract_block_tree_with_ao(&tree, &interner, block, external);
    pack_rect_batch_with_ao(&batch)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::presentation::voxel::unpack_rect;
    use voxel::mesh::wrap_block;

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

            let halo = extract_raw_halo(&tree, &interner, block, external);
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
            let halo = extract_raw_halo(&tree, &interner, block, external);
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
}
