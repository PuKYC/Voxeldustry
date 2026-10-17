//! 受限四叉树（Restricted Quadtree）：XZ 平面上的 2:1 平衡 LOD 划分。
//!
//! 把基础子块区域 `[-radius, radius)` 划分成若干**对齐方块叶子**（边长
//! `2^lod` 个基础子块）；区域是半开的（每轴 `2 * radius` 格），`radius = 0`
//! 退化成原点单个基础子块 `[0, 1)²`，保证半径 0 也有非空地形。保证：
//!
//! 1. **精确划分**：区域内每个基础子块恰好被一个叶子覆盖，不越界、不重叠；
//! 2. **2:1 平衡**：共享一条边的两个叶子，`lod` 差 ≤ 1
//!    （体素接缝修复与跨 LOD 邻居采样都依赖这条不变量）；
//! 3. **确定性**：输出只由输入闭包决定，按 `(x, z, lod)` 升序；
//! 4. **上限**：叶子 `lod ≤ max_lod`。
//!
//! 这是纯整数算法，不依赖 Bevy / 米制坐标（本 crate 的边界铁律）。
//! 设计参考 godot_voxel 的 `LodOctree`（受限分裂）与经典受限四叉树
//! （restricted quadtree）平衡：自下而上合并得到初始划分，再反复把
//! 相邻差 ≥ 2 的**较粗**叶子分裂成 4 个 `lod-1` 子块，直到不动点。
//!
//! 与 godot_voxel 的差别：godot_voxel 在八叉树里按观察者距离自顶向下分裂，
//! 我们保留自下而上的历史合并语义（逐块与旧实现一致），只叠加平衡修正。

use std::collections::{BTreeMap, BTreeSet};

/// `lod` 上限：`1 << 15 = 32768` 个基础子块跨度，足够覆盖任何地形半径，
/// 同时保证 `1i32 << lod` 不溢出。
pub const MAX_QUAD_LOD: u8 = 15;

/// 一个 XZ 叶子：原点（基础子块坐标，对齐到 `2^lod`）+ LOD。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct QuadLeaf {
    pub x: i32,
    pub z: i32,
    pub lod: u8,
}

impl QuadLeaf {
    /// 该叶子每轴覆盖的基础子块数（`2^lod`）。
    #[must_use]
    pub fn span(self) -> i32 {
        1i32 << self.lod
    }
}

/// 生成 `[-radius, radius)` 上满足 2:1 平衡的受限四叉树叶子（升序）。
///
/// 区域半开：每轴 `2 * radius` 格；`radius = 0` 时退化为原点单块
/// `[0, 1)²`（恰好一个 `QuadLeaf { x: 0, z: 0, lod: 0 }`）。
///
/// `may_coarsen(px, pz, size, level)` 判断**对齐到 `size`、原点 `(px, pz)` 的
/// 父块**是否允许成为 `level` 的叶子（`size` = `2^level`，单位为 1 个基础子块）。
/// 语义与历史 `may_coarsen_for_observer` 一致：只有 4 个子块都在区域内且
/// 距离允许时才合并。
#[must_use]
pub fn restricted_quadtree<F>(radius: i32, max_lod: u8, may_coarsen: F) -> Vec<QuadLeaf>
where
    F: Fn(i32, i32, i32, u8) -> bool,
{
    let radius = radius.max(0);
    let max_lod = max_lod.min(MAX_QUAD_LOD);
    let mut cells = base_merge(radius, max_lod, &may_coarsen);
    balance_2_to_1(&mut cells, radius, max_lod);
    cells
        .into_iter()
        .map(|((x, z), lod)| QuadLeaf { x, z, lod })
        .collect()
}

/// 自下而上合并出初始划分（历史 `lod_blocks_for_observer` 的 XZ 部分，逐块不变）。
///
/// 每层把「4 个 `level` 子块都在区域内且 `may_coarsen` 放行」的对齐父块合并成
/// `level + 1`。结果一定是一个精确划分，但**不保证** 2:1 平衡。
fn base_merge<F>(radius: i32, max_lod: u8, may_coarsen: &F) -> BTreeMap<(i32, i32), u8>
where
    F: Fn(i32, i32, i32, u8) -> bool,
{
    let extent = radius.max(1);
    let mut cells: BTreeMap<(i32, i32), u8> = BTreeMap::new();
    for x in -radius..extent {
        for z in -radius..extent {
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
            if !may_coarsen(px, pz, parent_size, level + 1) {
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

    cells
}

/// 2:1 松弛：反复把「与边相邻叶子 lod 差 ≥ 2」中较粗的那个分裂成 4 个子块。
///
/// 分裂只细化、不粗化，且叶子数严格增加（上界 = 全 LOD0 的基础子块数），
/// 所以一定在有限步内到达不动点；同时分裂保持对齐与精确划分，因此不变量
/// 在每一步之后都成立。
fn balance_2_to_1(cells: &mut BTreeMap<(i32, i32), u8>, radius: i32, max_lod: u8) {
    let extent = radius.max(1);
    loop {
        let mut to_split: BTreeSet<(i32, i32)> = BTreeSet::new();

        for x in -radius..extent {
            for z in -radius..extent {
                let Some(a_lod) = leaf_lod(cells, x, z, max_lod) else {
                    continue;
                };
                for (nx, nz) in [(x + 1, z), (x, z + 1)] {
                    if nx >= extent || nz >= extent {
                        continue;
                    }
                    let Some(b_lod) = leaf_lod(cells, nx, nz, max_lod) else {
                        continue;
                    };
                    if a_lod >= b_lod + 2 {
                        to_split.insert((aligned_origin(x, a_lod), aligned_origin(z, a_lod)));
                    } else if b_lod >= a_lod + 2 {
                        to_split.insert((aligned_origin(nx, b_lod), aligned_origin(nz, b_lod)));
                    }
                }
            }
        }

        if to_split.is_empty() {
            return;
        }

        for (px, pz) in to_split {
            let lod = cells.remove(&(px, pz)).expect("待分裂的原点必须是一个叶子");
            debug_assert!(lod >= 1, "LOD0 不可能比邻居粗 2 档");
            let child = lod - 1;
            let half = 1i32 << child;
            cells.insert((px, pz), child);
            cells.insert((px + half, pz), child);
            cells.insert((px, pz + half), child);
            cells.insert((px + half, pz + half), child);
        }
    }
}

/// 坐标 `coord` 对齐到 `2^lod` 后的格点（欧几里得取模，负数也正确）。
fn aligned_origin(coord: i32, lod: u8) -> i32 {
    let span = 1i32 << lod;
    coord.div_euclid(span) * span
}

/// 基础子块 `(x, z)` 所属叶子的 lod。
///
/// 从最粗到最细尝试：叶子的原点必须在**它自己的层级**上对齐，所以
/// `(aligned_origin(x, lod), aligned_origin(z, lod))` 恰好等于该叶子原点时才命中。
fn leaf_lod(cells: &BTreeMap<(i32, i32), u8>, x: i32, z: i32, max_lod: u8) -> Option<u8> {
    for lod in (0..=max_lod).rev() {
        let ox = aligned_origin(x, lod);
        let oz = aligned_origin(z, lod);
        if cells.get(&(ox, oz)) == Some(&lod) {
            return Some(lod);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 覆盖 = 区域 × 恰好一次，且每个叶子对齐、不越界、不超 LOD 上限。
    fn assert_valid_partition(leaves: &[QuadLeaf], radius: i32, max_lod: u8) {
        assert!(!leaves.is_empty(), "非空区域必须至少有一个叶子");
        let extent = radius.max(1);
        let mut covered: BTreeMap<(i32, i32), usize> = BTreeMap::new();
        for leaf in leaves {
            assert!(leaf.lod <= max_lod, "lod 超上限: {leaf:?}");
            assert_eq!(leaf.x.rem_euclid(leaf.span()), 0, "x 未对齐: {leaf:?}");
            assert_eq!(leaf.z.rem_euclid(leaf.span()), 0, "z 未对齐: {leaf:?}");
            assert!(
                leaf.x >= -radius && leaf.x + leaf.span() <= extent,
                "x 越界: {leaf:?}"
            );
            assert!(
                leaf.z >= -radius && leaf.z + leaf.span() <= extent,
                "z 越界: {leaf:?}"
            );
            for dx in 0..leaf.span() {
                for dz in 0..leaf.span() {
                    *covered.entry((leaf.x + dx, leaf.z + dz)).or_insert(0) += 1;
                }
            }
        }
        let mut expected = 0usize;
        for x in -radius..extent {
            for z in -radius..extent {
                assert_eq!(covered.get(&(x, z)), Some(&1), "({x},{z}) 覆盖数不为 1");
                expected += 1;
            }
        }
        assert_eq!(covered.len(), expected, "覆盖集合大小不符（越界或空洞）");
    }

    /// 统计违反 2:1 的相邻基础子块对数量（0 = 平衡）。
    fn two_to_one_violations(leaves: &[QuadLeaf], radius: i32, max_lod: u8) -> usize {
        let cells: BTreeMap<(i32, i32), u8> = leaves
            .iter()
            .map(|leaf| ((leaf.x, leaf.z), leaf.lod))
            .collect();
        let extent = radius.max(1);
        let mut count = 0usize;
        for x in -radius..extent {
            for z in -radius..extent {
                let Some(a) = leaf_lod(&cells, x, z, max_lod) else {
                    continue;
                };
                for (nx, nz) in [(x + 1, z), (x, z + 1)] {
                    if nx >= extent || nz >= extent {
                        continue;
                    }
                    if let Some(b) = leaf_lod(&cells, nx, nz, max_lod) {
                        if a.abs_diff(b) > 1 {
                            count += 1;
                        }
                    }
                }
            }
        }
        count
    }

    #[test]
    fn single_cell_is_one_leaf() {
        let leaves = restricted_quadtree(0, 3, |_, _, _, _| true);
        assert_eq!(leaves, vec![QuadLeaf { x: 0, z: 0, lod: 0 }]);
        assert_valid_partition(&leaves, 0, 3);
    }

    #[test]
    fn never_coarsen_keeps_every_cell_at_lod0() {
        let radius = 3;
        let leaves = restricted_quadtree(radius, 3, |_, _, _, _| false);
        assert_eq!(leaves.len(), 36);
        assert!(leaves.iter().all(|leaf| leaf.lod == 0));
        assert_valid_partition(&leaves, radius, 3);
        assert_eq!(two_to_one_violations(&leaves, radius, 3), 0);
    }

    #[test]
    fn always_coarsen_merges_aligned_blocks() {
        // 半径 2 半开 = 每轴 4 格，正好是 4 个对齐的 LOD1 父块：
        // 全部合并，不残留任何 LOD0（闭区间会剩 x=2 列 + z=2 行）。
        let radius = 2;
        let leaves = restricted_quadtree(radius, 1, |_, _, _, _| true);
        assert_valid_partition(&leaves, radius, 1);
        assert_eq!(two_to_one_violations(&leaves, radius, 1), 0);
        assert_eq!(leaves.iter().filter(|leaf| leaf.lod == 1).count(), 4);
        assert_eq!(leaves.iter().filter(|leaf| leaf.lod == 0).count(), 0);
    }

    #[test]
    fn always_coarsen_eats_aligned_half_open_region() {
        // 半开区域被对齐的粗块整除时，不允许留下细带（闭区间总会剩 1 格）。
        // R=4 半开 = 8 格 = 2×2 个对齐的 LOD2 块（[-4,4) 不是 8 的倍数）。
        let leaves = restricted_quadtree(4, 3, |_, _, _, _| true);
        assert_eq!(
            leaves,
            vec![
                QuadLeaf {
                    x: -4,
                    z: -4,
                    lod: 2
                },
                QuadLeaf {
                    x: -4,
                    z: 0,
                    lod: 2
                },
                QuadLeaf {
                    x: 0,
                    z: -4,
                    lod: 2
                },
                QuadLeaf { x: 0, z: 0, lod: 2 },
            ]
        );
        assert_valid_partition(&leaves, 4, 3);

        // R=8 半开 = 16 格 = 4 个对齐的 LOD3 块（2×2，无残留）。
        let leaves = restricted_quadtree(8, 3, |_, _, _, _| true);
        assert_eq!(leaves.len(), 4);
        assert!(leaves.iter().all(|leaf| leaf.lod == 3), "{leaves:?}");
        assert_valid_partition(&leaves, 8, 3);
        assert_eq!(two_to_one_violations(&leaves, 8, 3), 0);
    }

    #[test]
    fn half_open_radius_12_has_no_constant_lod0_band() {
        // 闭区间 [-12, 12] 每轴 25 格（奇数），对齐的 4 子块合并永远差一格，
        // x=+12 整列 / z=+12 整行恒为 LOD0；半开 [-12, 12) 共 24 格后消失。
        let radius = 12;
        let max_lod = 3;
        let leaves = restricted_quadtree(radius, max_lod, |_, _, _, _| true);
        assert_valid_partition(&leaves, radius, max_lod);
        assert_eq!(two_to_one_violations(&leaves, radius, max_lod), 0);
        assert!(
            leaves.iter().all(|leaf| leaf.lod > 0),
            "半开区域不允许出现 LOD0 残留列/行: {leaves:?}"
        );

        // 四角最粗只能到 LOD2：-12 不是 8 的倍数，凑不齐对齐的 LOD3 父块。
        let cells: BTreeMap<(i32, i32), u8> = leaves
            .iter()
            .map(|leaf| ((leaf.x, leaf.z), leaf.lod))
            .collect();
        for (cx, cz) in [(-12, -12), (-12, 11), (11, -12), (11, 11)] {
            assert_eq!(
                leaf_lod(&cells, cx, cz, max_lod),
                Some(2),
                "({cx},{cz}) 最粗只能到 LOD2"
            );
        }
    }

    #[test]
    fn balancing_repairs_a_2_to_1_violation() {
        // 只有靠近小 z 的父块能合并，于是远端会出现 LOD2/LOD3 紧贴 LOD0。
        let radius = 8;
        let max_lod = 3;
        let may = |_px: i32, pz: i32, size: i32, _level: u8| pz + size <= 4;

        let raw: Vec<QuadLeaf> = base_merge(radius, max_lod, &may)
            .into_iter()
            .map(|((x, z), lod)| QuadLeaf { x, z, lod })
            .collect();
        assert!(
            two_to_one_violations(&raw, radius, max_lod) > 0,
            "构造的输入必须真的违反 2:1，否则本测试没有意义"
        );

        let leaves = restricted_quadtree(radius, max_lod, may);
        assert_valid_partition(&leaves, radius, max_lod);
        assert_eq!(
            two_to_one_violations(&leaves, radius, max_lod),
            0,
            "平衡后不允许任何 2:1 违反"
        );
        // 允许区域仍保留粗叶子，近处仍保留 LOD0。
        assert!(leaves.iter().any(|leaf| leaf.lod == 2));
        assert!(leaves.iter().any(|leaf| leaf.lod == 0));
    }

    #[test]
    fn lod_is_capped_and_clamped() {
        let leaves = restricted_quadtree(4, 9, |_, _, _, _| true);
        assert!(leaves.iter().all(|leaf| leaf.lod <= 9));
        assert_valid_partition(&leaves, 4, 9);
        assert_eq!(two_to_one_violations(&leaves, 4, 9), 0);

        // 超过 MAX_QUAD_LOD 的上限被夹住，而不是溢出移位。
        let clamped = restricted_quadtree(0, 200, |_, _, _, _| true);
        assert_valid_partition(&clamped, 0, MAX_QUAD_LOD);
    }

    #[test]
    fn output_is_sorted_and_deterministic() {
        let may = |_px: i32, pz: i32, size: i32, _level: u8| pz + size <= 4;
        let a = restricted_quadtree(8, 3, may);
        let b = restricted_quadtree(8, 3, may);
        assert_eq!(a, b, "同样输入必须逐块一致");
        let mut sorted = a.clone();
        sorted.sort_unstable();
        assert_eq!(a, sorted, "输出必须按 (x, z, lod) 升序");
    }
}
