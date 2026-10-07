//! 地形块的划分：生成区域、LOD 合并、y 层展开、块原点（纯函数，可单测）。
//!
//! 设计约束：
//! - 每个 mesh 块是 32³ 基础跨度（= 一个 base 子块 × 2^lod）；
//! - 块原点必须对齐到 2^lod 个基础子块（引擎的 wrap_block / MeshBlock 要求）；
//! - 块原点（米）= origin * 32 * voxel_size，**不乘 2^lod**（LOD 缩放由渲染侧
//!   按 RectListPayload.lod 与 rect_scale_meters 处理）。

use std::collections::{BTreeMap, BTreeSet};

use game_engine::math::FixedPoint;
use game_engine::voxel::{chunk_key, ChunkKey};

use crate::world::generation::CHUNK_SIZE;
use crate::world::lod::{lod_for_distance, MAX_LOD};

/// 生成区域在 x/z 上比 mesh 区域多出的子块数。
///
/// 多生成一圈是为了让边缘 mesh 块的 external 邻块采样有数据（引擎的
/// external 掩码要求传同 LOD 的已包装邻块；缺子块会被 wrap_block 当成空气）。
pub const TERRAIN_MARGIN_CHUNKS: i32 = 1;

/// y 方向层数（从 y=0 起算）。
///
/// 至少 4 层：world 地表在 y ≈ 56..76 体素（base_height 64 ± amplitude ≤ 12），
/// 即子块 y ∈ {1, 2}，所以 band 必须包含 [0, 4) 才能既覆盖地表又有实心基底。
/// 同时必须是 2^max_lod 的倍数，LOD 的 y 块才能对齐平铺。
#[must_use]
pub fn y_band_layers(max_lod: u8) -> i32 {
    1 << u32::from(max_lod.min(MAX_LOD).max(2))
}

/// 一个待网格化的块：原点（对齐到 2^lod 的子块坐标）+ LOD。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TerrainBlock {
    pub origin: ChunkKey,
    pub lod: u8,
}

/// 需要生成的基础子块（含 x/z margin；y 只到 band 内，band 外按空气处理）。
#[must_use]
pub fn chunk_keys_to_generate(radius_blocks: i32, max_lod: u8) -> Vec<ChunkKey> {
    let radius = radius_blocks.max(0);
    let margin = TERRAIN_MARGIN_CHUNKS;
    let layers = y_band_layers(max_lod);
    let mut out = Vec::new();
    for x in (-radius - margin)..=(radius + margin) {
        for z in (-radius - margin)..=(radius + margin) {
            for y in 0..layers {
                out.push(chunk_key(x, y, z));
            }
        }
    }
    out
}

/// XZ 上按到世界原点的距离做 LOD 合并，再按 y 层展开成块列表（确定性升序）。
///
/// 合并规则（自下而上）：
/// 1. level 0 单元 = 区域内每个基础子块 (x, z) ∈ [-radius, radius]²；
/// 2. 若某父块（边长 2^(level+1) 个子块，原点是 2^(level+1) 的倍数）的 4 个
///    level 子块都存在，且父块 XZ AABB 到原点的**最近切比雪夫距离**已经落在
///    `lod_for_distance` 允许的 level+1 档（dist 越大 LOD 越粗），则合并；
/// 3. 每个叶子的 y 层 = 从 0 起每 2^lod 层一个块，铺满 band。
///
/// 不变量（tests 里断言）：区域里每个 (x, z) 基础子块恰好被一个叶子覆盖，
/// 且每个叶子的 y 层恰好覆盖 band 一次。
#[must_use]
pub fn lod_blocks(radius_blocks: i32, max_lod: u8, voxel_size_m: f64) -> Vec<TerrainBlock> {
    let radius = radius_blocks.max(0);
    let max_lod = max_lod.min(MAX_LOD);

    let mut cells: BTreeMap<(i32, i32), u8> = BTreeMap::new();
    for x in -radius..=radius {
        for z in -radius..=radius {
            cells.insert((x, z), 0);
        }
    }

    for level in 0..max_lod {
        let size = 1i32 << level;
        let parent_size = size * 2;
        let mut parents: BTreeSet<(i32, i32)> = BTreeSet::new();
        for (&(x, z), &cell_level) in cells.iter() {
            if cell_level != level {
                continue;
            }
            parents.insert((
                x.div_euclid(parent_size) * parent_size,
                z.div_euclid(parent_size) * parent_size,
            ));
        }

        let mut merges: Vec<(i32, i32)> = Vec::new();
        for &(px, pz) in parents.iter() {
            let children = [
                (px, pz),
                (px + size, pz),
                (px, pz + size),
                (px + size, pz + size),
            ];
            if children
                .iter()
                .any(|child| cells.get(child) != Some(&level))
            {
                continue;
            }
            if !may_coarsen(px, pz, parent_size, voxel_size_m, level + 1) {
                continue;
            }
            merges.push((px, pz));
        }

        for (px, pz) in merges {
            for child in [
                (px, pz),
                (px + size, pz),
                (px, pz + size),
                (px + size, pz + size),
            ] {
                cells.remove(&child);
            }
            cells.insert((px, pz), level + 1);
        }
    }

    let layers = y_band_layers(max_lod);
    let mut out = Vec::new();
    for (&(x, z), &lod) in cells.iter() {
        let step = 1i32 << lod;
        let mut y = 0;
        while y + step <= layers {
            out.push(TerrainBlock {
                origin: chunk_key(x, y, z),
                lod,
            });
            y += step;
        }
    }
    out.sort_unstable();
    out
}

/// 同 LOD 的 6 邻块原点，顺序 = neighbors6 = [+x, -x, +y, -y, +z, -z]。
#[must_use]
pub fn neighbor_origins(origin: ChunkKey, lod: u8) -> [ChunkKey; 6] {
    let span = 1i32 << lod;
    [
        chunk_key(origin.x + span, origin.y, origin.z),
        chunk_key(origin.x - span, origin.y, origin.z),
        chunk_key(origin.x, origin.y + span, origin.z),
        chunk_key(origin.x, origin.y - span, origin.z),
        chunk_key(origin.x, origin.y, origin.z + span),
        chunk_key(origin.x, origin.y, origin.z - span),
    ]
}

/// 块原点（米）= origin * CHUNK_SIZE * voxel_size。
///
/// **不乘 2^lod**：块内矩形单位由渲染侧按 voxel_size * 2^lod 缩放。
#[must_use]
pub fn block_origin_meters(origin: ChunkKey, voxel_size_m: f32) -> [f32; 3] {
    let scale = CHUNK_SIZE as f32 * voxel_size_m;
    [
        origin.x as f32 * scale,
        origin.y as f32 * scale,
        origin.z as f32 * scale,
    ]
}

/// 该块覆盖的基础子块数量（每轴 2^lod）。
#[must_use]
pub fn block_span(lod: u8) -> i32 {
    1i32 << lod
}

/// 父块 XZ AABB 到原点的最近距离（米，切比雪夫 = 两轴取大）是否已经允许
/// 粗到 `level`。
fn may_coarsen(px: i32, pz: i32, size: i32, voxel_size_m: f64, level: u8) -> bool {
    let dist_m = nearest_axis_m(px, size, voxel_size_m).max(nearest_axis_m(pz, size, voxel_size_m));
    lod_for_distance(FixedPoint::from_num(dist_m)).lod() >= level
}

/// 一维 AABB [origin*chunk, (origin+size)*chunk] 到 0 的最近距离（米）。
fn nearest_axis_m(origin: i32, size: i32, voxel_size_m: f64) -> f64 {
    let chunk_m = f64::from(CHUNK_SIZE) * voxel_size_m;
    let lo = f64::from(origin) * chunk_m;
    let hi = lo + f64::from(size) * chunk_m;
    if lo > 0.0 {
        lo
    } else if hi < 0.0 {
        -hi
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::static_data::voxel::VOXEL_SIZE_METERS;

    /// 区域里每个 (x, y, z) 基础子块被叶子恰好覆盖一次（3D 划分不重叠、无空洞）。
    fn assert_partition(radius_blocks: i32, max_lod: u8) {
        let blocks = lod_blocks(radius_blocks, max_lod, VOXEL_SIZE_METERS);
        assert!(!blocks.is_empty());
        let layers = y_band_layers(max_lod);

        let mut covered: BTreeMap<(i32, i32, i32), usize> = BTreeMap::new();
        for block in blocks.iter() {
            let span = block_span(block.lod);
            assert!(block.lod <= max_lod);
            assert_eq!(block.origin.x.rem_euclid(span), 0, "x 未对齐到 2^lod");
            assert_eq!(block.origin.y.rem_euclid(span), 0, "y 未对齐到 2^lod");
            assert_eq!(block.origin.z.rem_euclid(span), 0, "z 未对齐到 2^lod");
            assert!(block.origin.y >= 0 && block.origin.y + span <= layers);
            for dx in 0..span {
                for dy in 0..span {
                    for dz in 0..span {
                        *covered
                            .entry((
                                block.origin.x + dx,
                                block.origin.y + dy,
                                block.origin.z + dz,
                            ))
                            .or_insert(0) += 1;
                    }
                }
            }
        }

        // 覆盖 = 区域 × band 恰好一次，不越界、不重叠。
        let mut expected = Vec::new();
        for x in -radius_blocks..=radius_blocks {
            for y in 0..layers {
                for z in -radius_blocks..=radius_blocks {
                    expected.push((x, y, z));
                }
            }
        }
        assert_eq!(
            covered.len(),
            expected.len(),
            "覆盖集合大小不符（越界或空洞）"
        );
        for cell in expected {
            assert_eq!(covered.get(&cell), Some(&1), "({cell:?}) 覆盖数不为 1");
        }
    }

    #[test]
    fn small_region_is_all_lod0() {
        let blocks = lod_blocks(2, 1, VOXEL_SIZE_METERS);
        assert!(blocks.iter().all(|block| block.lod == 0));
        // x/z = 5 × 5 个单元，y band = 4 层。
        assert_eq!(blocks.len(), 25 * 4);
        assert_partition(2, 1);
        assert_partition(2, 0);
    }

    #[test]
    fn far_region_has_coarse_lods_and_still_partitions() {
        // 32/64/128 m 三档阈值换算到 LOD 合并的"父块最近距离"：
        // LOD1 要 ≥ 32 m（父块 origin ≥ 3 子块），LOD2 要 ≥ 64 m（origin ≥ 5），
        // LOD3 要 ≥ 128 m（origin ≥ 9）；半径 24 才能把三档都覆盖到。
        let blocks = lod_blocks(24, 3, VOXEL_SIZE_METERS);
        let mut lods = [0usize; 4];
        for block in blocks.iter() {
            lods[usize::from(block.lod)] += 1;
        }
        assert!(lods[0] > 0, "近处必须是 LOD0");
        assert!(lods[1] > 0, "中距必须出现 LOD1");
        assert!(lods[2] > 0, "远处必须出现 LOD2");
        assert!(lods[3] > 0, "最远处必须出现 LOD3");
        assert_partition(24, 3);

        // 半径 8 只够到 LOD1（LOD2 的父块最近距离仍 < 64 m）。
        let near = lod_blocks(8, 3, VOXEL_SIZE_METERS);
        assert!(near.iter().all(|block| block.lod <= 1));
        assert_partition(8, 3);
    }

    #[test]
    fn partition_is_deterministic() {
        let a = lod_blocks(4, 3, VOXEL_SIZE_METERS);
        let b = lod_blocks(4, 3, VOXEL_SIZE_METERS);
        assert_eq!(a, b);
    }

    #[test]
    fn generated_chunks_cover_every_block() {
        let radius = 2;
        let max_lod = 1;
        let generated: BTreeSet<ChunkKey> = chunk_keys_to_generate(radius, max_lod)
            .into_iter()
            .collect();
        for block in lod_blocks(radius, max_lod, VOXEL_SIZE_METERS) {
            let span = block_span(block.lod);
            for dx in 0..span {
                for dy in 0..span {
                    for dz in 0..span {
                        let key = chunk_key(
                            block.origin.x + dx,
                            block.origin.y + dy,
                            block.origin.z + dz,
                        );
                        assert!(
                            generated.contains(&key),
                            "块 {block:?} 的子块 {key:?} 未生成"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn block_origin_is_base_scale_not_lod_scaled() {
        // 同一个原点：LOD 只影响块跨度，不影响起点的米坐标。
        let origin = chunk_key(2, 1, -3);
        let meters = block_origin_meters(origin, 0.45);
        assert!((meters[0] - 2.0 * 32.0 * 0.45).abs() < 1e-4);
        assert!((meters[1] - 1.0 * 32.0 * 0.45).abs() < 1e-4);
        assert!((meters[2] - (-3.0) * 32.0 * 0.45).abs() < 1e-4);
    }

    #[test]
    fn neighbor_order_matches_neighbors6() {
        let origin = chunk_key(0, 0, 0);
        let n = neighbor_origins(origin, 0);
        assert_eq!(n[0], chunk_key(1, 0, 0));
        assert_eq!(n[1], chunk_key(-1, 0, 0));
        assert_eq!(n[2], chunk_key(0, 1, 0));
        assert_eq!(n[3], chunk_key(0, -1, 0));
        assert_eq!(n[4], chunk_key(0, 0, 1));
        assert_eq!(n[5], chunk_key(0, 0, -1));
        let n1 = neighbor_origins(origin, 1);
        assert_eq!(n1[0], chunk_key(2, 0, 0));
    }

    #[test]
    fn y_band_covers_surface_chunks() {
        // 地表在子块 y ∈ {1, 2}；band 必须包含它们。
        for max_lod in 0..=3u8 {
            let layers = y_band_layers(max_lod);
            assert!(layers >= 4);
            assert!(layers.rem_euclid(1i32 << max_lod) == 0);
        }
    }
}
