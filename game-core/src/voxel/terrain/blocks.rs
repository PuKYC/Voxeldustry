//! 地形块的划分：生成区域、LOD 合并、y 层展开、块原点（纯函数，可单测）。
//!
//! 设计约束：
//! - 每个 mesh 块是 32³ 基础跨度（= 一个 base 子块 × 2^lod）；
//! - 块原点必须对齐到 2^lod 个基础子块（引擎的 wrap_block / MeshBlock 要求）；
//! - 块原点（米）= origin * 32 * voxel_size，**不乘 2^lod**（LOD 缩放由渲染侧
//!   按 RawVoxelPayload.lod 与 rect_scale_meters 处理）。

use std::collections::{BTreeMap, BTreeSet};

use game_engine::voxel::{chunk_key, restricted_quadtree, ChunkKey};

use crate::voxel::lod::{VoxelLodConfig, MAX_LOD};
use crate::voxel::terrain::generation::CHUNK_SIZE;

/// 生成区域在 x/z 上比 mesh 区域多出的子块数。
///
/// 多生成一圈是为了让边缘 mesh 块的 26 邻（6 面 + 12 棱 + 8 角）采样有数据：
/// `extract_raw_halo` 要求传同 LOD 的已包装邻块；缺子块会被 wrap_block 当成
/// 空气。LOD 合并只在完整子块组内发生，半开区域 `[-radius, radius)` 里
/// 邻块原点最多到 `radius`，一圈 margin 足够覆盖 26 邻。
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
    // 半开区域：x/z ∈ [-radius, radius)，radius = 0 退化成原点单块 [0, 1)。
    let extent = radius.max(1);
    let layers = y_band_layers(max_lod);
    let mut out = Vec::new();
    for x in (-radius - margin)..(extent + margin) {
        for z in (-radius - margin)..(extent + margin) {
            for y in 0..layers {
                out.push(chunk_key(x, y, z));
            }
        }
    }
    out
}

/// 静态观察者（世界原点）的 LOD 划分。
///
/// 等价于 [lod_blocks_for_observer] 传观察者 [0.0, 0.0] 与
/// [VoxelLodConfig::default]。保留旧签名，Startup 生成与既有测试不改。
#[must_use]
pub fn lod_blocks(radius_blocks: i32, max_lod: u8, voxel_size_m: f64) -> Vec<TerrainBlock> {
    lod_blocks_for_observer(
        [0.0, 0.0],
        radius_blocks,
        max_lod,
        voxel_size_m,
        &VoxelLodConfig::default(),
    )
}

/// XZ 上按到**观察者**的距离做 LOD 合并，再按 y 层展开成块列表（确定性升序）。
///
/// 划分规则（XZ 部分委托给 `voxel::restricted_quadtree`）：
/// 1. level 0 单元 = 区域内每个基础子块 (x, z) ∈ [-radius, radius)²（半开，与观察者无关）；
/// 2. 自下而上合并：若某对齐父块（边长 2^(level+1) 个子块）的 4 个 level 子块
///    都存在，且父块 XZ AABB 到观察者的**最近切比雪夫距离**已经落在
///    [VoxelLodConfig::lod_for_distance_m] 允许的 level+1 档，则合并；
/// 3. **2:1 平衡**：把与边相邻叶子 lod 差 ≥ 2 的较粗叶子继续分裂，保证任意
///    两个边相邻块至多差一档（跨 LOD 邻居采样 / 接缝修复的前提）；
/// 4. 每个叶子的 y 层 = 从 0 起每 2^lod 层一个块，铺满 band。
///
/// max_lod 同时决定 y band 层数（必须与 Startup 生成时的 max_lod 一致）；
/// 观察者只改变 XZ 合并，不改变覆盖范围。
///
/// 不变量（tests 里断言）：区域里每个 (x, z) 基础子块恰好被一个叶子覆盖，
/// 每个叶子的 y 层恰好覆盖 band 一次，且任意两个边相邻叶子的 lod 差 ≤ 1。
#[must_use]
pub fn lod_blocks_for_observer(
    observer_xz_m: [f64; 2],
    radius_blocks: i32,
    max_lod: u8,
    voxel_size_m: f64,
    config: &VoxelLodConfig,
) -> Vec<TerrainBlock> {
    let radius = radius_blocks.max(0);
    let max_lod = max_lod.min(MAX_LOD);

    // XZ 划分委托给 `crates/voxel` 的受限四叉树：先自下而上合并，再做 2:1 平衡。
    // 闭包语义 = 历史 `may_coarsen_for_observer`（同一对齐父块、同一 level）。
    let leaves = restricted_quadtree(radius, max_lod, |px, pz, size, level| {
        may_coarsen_for_observer(observer_xz_m, px, pz, size, voxel_size_m, level, config)
    });

    let layers = y_band_layers(max_lod);
    let mut out = Vec::new();
    for leaf in leaves {
        let step = leaf.span();
        let mut y = 0;
        while y + step <= layers {
            out.push(TerrainBlock {
                origin: chunk_key(leaf.x, y, leaf.z),
                lod: leaf.lod,
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

/// 单个面方向外侧的邻居报告（纯原点 / LOD，不含包装树）。
///
/// 顺序与 [`neighbor_origins`] 一致：`[+x, -x, +y, -y, +z, -z]`。
/// 2:1 平衡（XZ）保证相邻叶子 LOD 差 ≤ 1，所以同一面的外侧只有三种情况：
/// - `Same`：同 LOD 的单块；
/// - `Coarser`：邻居 LOD 大 1。本块面正好落在粗邻居面的一个 16x16 子块内，
///   `offset` = 子块首格在粗邻居面内的坐标（每个切向轴 0 或 16）；
/// - `Finer`：邻居 LOD 小 1。本块面被 4 个细邻居按切向 2x2 象限覆盖，
///   行优先 `[q00, q01, q10, q11]`（行 = 面的 row，列 = 面的 col）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FaceNeighbor {
    /// 面外无块（按空气处理）。
    Missing,
    /// 同 LOD 邻居的原点。
    Same(ChunkKey),
    /// 更粗的邻居（LOD 大 1）。
    Coarser {
        /// 粗邻居的块原点。
        origin: ChunkKey,
        /// 本块面在粗邻居面内的首格（行、列；每轴 0 或 16）。
        offset: [u8; 2],
    },
    /// 更细的邻居（LOD 小 1），切向 2x2 象限行优先。
    Finer([ChunkKey; 4]),
}

/// 划分的 XZ 索引 + 块集合，供 [`face_neighbors`] 跨块复用（只建一次）。
#[derive(Clone, Debug, Default)]
pub struct TerrainPartition {
    blocks: BTreeSet<(ChunkKey, u8)>,
    xz: BTreeMap<(i32, i32), u8>,
}

impl TerrainPartition {
    /// 由块列表构建（同一 XZ 基础格的 lod 必须一致）。
    #[must_use]
    pub fn new(blocks: &[TerrainBlock]) -> Self {
        let mut set = BTreeSet::new();
        let mut xz: BTreeMap<(i32, i32), u8> = BTreeMap::new();
        for block in blocks {
            set.insert((block.origin, block.lod));
            let span = block_span(block.lod);
            for dx in 0..span {
                for dz in 0..span {
                    let cell = (block.origin.x + dx, block.origin.z + dz);
                    if let Some(previous) = xz.insert(cell, block.lod) {
                        debug_assert_eq!(previous, block.lod, "({cell:?}) 的 XZ 叶子 lod 冲突");
                    }
                }
            }
        }
        Self { blocks: set, xz }
    }

    /// 块集合中是否存在该 (原点, lod)。
    #[must_use]
    pub fn contains(&self, origin: ChunkKey, lod: u8) -> bool {
        self.blocks.contains(&(origin, lod))
    }

    /// 某个 XZ 基础格所属叶子的 lod。
    #[must_use]
    pub fn xz_lod(&self, x: i32, z: i32) -> Option<u8> {
        self.xz.get(&(x, z)).copied()
    }

    /// 某个 XZ 基础格所属叶子的 (x, z, lod)。
    fn leaf_origin(&self, x: i32, z: i32) -> Option<(i32, i32, u8)> {
        let lod = self.xz_lod(x, z)?;
        let span = 1i32 << lod;
        Some((x.div_euclid(span) * span, z.div_euclid(span) * span, lod))
    }
}

/// 给定块与其划分，返回 6 个面的**完整 LOD 报告**（内部用）。
///
/// 顺序 = [`neighbor_origins`] = `[+x, -x, +y, -y, +z, -z]`。
/// 竖直（±y）面永远是同一 XZ 叶子的上/下 y 块（同 LOD）；水平（±x / ±z）
/// 面按 2:1 平衡只会出现 `Same` / `Coarser` / `Finer` 三种之一。
///
/// 这份报告只用于「这面是否跨 LOD」（裙边掩码 / 组重建签名），
/// **不再驱动网格剔除**：跨 LOD 面在 [face_neighbors] 里一律映射成 `Missing`。
#[must_use]
fn neighbour_lod_report(block: &TerrainBlock, partition: &TerrainPartition) -> [FaceNeighbor; 6] {
    let span = block_span(block.lod);
    [
        horizontal_neighbor(partition, block.origin, block.lod, span, 1, 0),
        horizontal_neighbor(partition, block.origin, block.lod, span, -1, 0),
        vertical_neighbor(partition, block.origin, block.lod, span, 1),
        vertical_neighbor(partition, block.origin, block.lod, span, -1),
        horizontal_neighbor(partition, block.origin, block.lod, span, 0, 1),
        horizontal_neighbor(partition, block.origin, block.lod, span, 0, -1),
    ]
}

/// 给定块与其划分，返回 6 个面的**同 LOD** 邻居报告。
///
/// 跨 LOD 面（`Coarser` / `Finer`）一律报 `Missing`：块只看同 LOD 邻居，
/// 缺失按空气处理 —— 每块自己封边（godot_voxel 的块自闭合）。
/// 这样不会出现「粗侧按 2x2 聚合剔除、细侧按粗格剔除」两侧都剔的穿透洞。
///
/// LOD 错位信息不外传：裙边掩码用 [`lod_mismatch_mask`]，重建签名用它 + 本报告。
#[must_use]
pub fn face_neighbors(block: &TerrainBlock, partition: &TerrainPartition) -> [FaceNeighbor; 6] {
    let mut report = neighbour_lod_report(block, partition);
    for neighbour in &mut report {
        if matches!(
            neighbour,
            FaceNeighbor::Coarser { .. } | FaceNeighbor::Finer(_)
        ) {
            *neighbour = FaceNeighbor::Missing;
        }
    }
    report
}

/// 跨 LOD 面掩码（bit `face` 对应 `[+x, -x, +y, -y, +z, -z]`）。
///
/// 只有 `Coarser` / `Finer` 面置位；`Same` / `Missing` 为 0。
#[must_use]
pub fn lod_mismatch_mask(block: &TerrainBlock, partition: &TerrainPartition) -> u8 {
    skirt_mask(&neighbour_lod_report(block, partition))
}

/// 由面邻居报告得到跨 LOD 面掩码（bit `face` 对应 [`face_neighbors`] 顺序
/// `[+x, -x, +y, -y, +z, -z]`）。
///
/// 只有 `Coarser` / `Finer` 面置位；`Same` / `Missing` 为 0（同 LOD 面不加裙边，
/// 避免多余几何 / z-fighting）。
#[must_use]
pub fn skirt_mask(neighbours: &[FaceNeighbor; 6]) -> u8 {
    let mut mask = 0u8;
    for (face, neighbour) in neighbours.iter().enumerate() {
        if matches!(
            neighbour,
            FaceNeighbor::Coarser { .. } | FaceNeighbor::Finer(_)
        ) {
            mask |= 1 << face;
        }
    }
    mask
}

/// 竖直面（±y）：同一 XZ 叶子的相邻 y 块，同 LOD。
fn vertical_neighbor(
    partition: &TerrainPartition,
    origin: ChunkKey,
    lod: u8,
    span: i32,
    sign: i32,
) -> FaceNeighbor {
    let neighbour = chunk_key(origin.x, origin.y + sign * span, origin.z);
    if partition.contains(neighbour, lod) {
        FaceNeighbor::Same(neighbour)
    } else {
        FaceNeighbor::Missing
    }
}

/// 水平面（±x / ±z）：面外第一层基础格按切向扫描，收集所属 XZ 叶子。
fn horizontal_neighbor(
    partition: &TerrainPartition,
    origin: ChunkKey,
    lod: u8,
    span: i32,
    dir_x: i32,
    dir_z: i32,
) -> FaceNeighbor {
    let nx = if dir_x > 0 {
        origin.x + span
    } else if dir_x < 0 {
        origin.x - 1
    } else {
        origin.x
    };
    let nz = if dir_z > 0 {
        origin.z + span
    } else if dir_z < 0 {
        origin.z - 1
    } else {
        origin.z
    };
    let (tan_start, normal_is_x) = if dir_x != 0 {
        (origin.z, true)
    } else {
        (origin.x, false)
    };

    // 切向坐标 -> 所属叶子。同一面下最多两个不同叶子（更细时）。
    let mut leaves: BTreeMap<i32, (i32, i32, u8)> = BTreeMap::new();
    for t in tan_start..tan_start + span {
        let (x, z) = if normal_is_x { (nx, t) } else { (t, nz) };
        if let Some(leaf) = partition.leaf_origin(x, z) {
            leaves.insert(t, leaf);
        }
    }
    if leaves.is_empty() {
        return FaceNeighbor::Missing;
    }

    let lods: BTreeSet<u8> = leaves.values().map(|(_, _, l)| *l).collect();
    if lods.len() != 1 {
        // 2:1 对齐下同一面的邻居 LOD 必然一致；不一致只可能是外部传入了
        // 非划分集合，保守按空气处理。
        debug_assert!(false, "同一面的邻居 LOD 不一致: {lods:?}");
        return FaceNeighbor::Missing;
    }
    let neighbour_lod = *lods.iter().next().expect("非空");
    let (&_, &(leaf_x, leaf_z, _)) = leaves.iter().next().expect("非空");

    if neighbour_lod == lod {
        let neighbour = chunk_key(leaf_x, origin.y, leaf_z);
        debug_assert!(partition.contains(neighbour, lod));
        return FaceNeighbor::Same(neighbour);
    }

    if neighbour_lod == lod + 1 {
        // 邻居更粗：一个粗叶子覆盖整个切向范围，本块面是其 16x16 子块。
        let coarse_span = 1i32 << neighbour_lod;
        let y_origin = origin.y.div_euclid(coarse_span) * coarse_span;
        let neighbour = chunk_key(leaf_x, y_origin, leaf_z);
        debug_assert!(partition.contains(neighbour, neighbour_lod));
        // 32 / 2^(lod+1) = 2^(4-lod)：粗格数 = 细格数 / 2。
        let shift = 4 - i32::from(lod);
        let row_offset = ((origin.y - y_origin) << shift) as u8;
        let col_offset = if normal_is_x {
            ((origin.z - leaf_z) << shift) as u8
        } else {
            ((origin.x - leaf_x) << shift) as u8
        };
        return FaceNeighbor::Coarser {
            origin: neighbour,
            offset: [row_offset, col_offset],
        };
    }

    if neighbour_lod + 1 == lod {
        // 邻居更细：切向每半区一个细叶子，竖直方向每半区一个 y 块。
        let fine_span = 1i32 << neighbour_lod;
        let mut by_half: [Option<(i32, i32)>; 2] = [None; 2];
        for (_, &(lx, lz, _)) in leaves.iter() {
            let leaf_tan = if normal_is_x { lz } else { lx };
            let half = usize::from(leaf_tan >= tan_start + fine_span);
            by_half[half] = Some((lx, lz));
        }
        let mut out = [chunk_key(0, 0, 0); 4];
        for row_half in 0..2i32 {
            for col_half in 0..2usize {
                let Some((lx, lz)) = by_half[col_half] else {
                    debug_assert!(false, "更细邻居缺少切向半区 {col_half}");
                    return FaceNeighbor::Missing;
                };
                let y = origin.y + row_half * fine_span;
                let neighbour = chunk_key(lx, y, lz);
                debug_assert!(partition.contains(neighbour, neighbour_lod));
                out[(row_half as usize) * 2 + col_half] = neighbour;
            }
        }
        return FaceNeighbor::Finer(out);
    }

    debug_assert!(false, "LOD 差超过 1: {} -> {}", lod, neighbour_lod);
    FaceNeighbor::Missing
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

/// 每个空间区域每轴覆盖的 mesh 块数（可调）。
///
/// 混合分组键 = LOD × 空间区域：LOD L 的单个区域边长 = `REGION_BLOCKS * 2^L`
/// 基础子块，所以**组内所有块同 LOD**，wire 的单 `lod` 字段仍然够用。
/// 默认 2 -> 每组至多 `2×2×2 = 8` 块，载荷有上界（见 docs §5.1）。
pub const REGION_BLOCKS: i32 = 2;

/// 混合分组键：LOD + 空间区域网格坐标（每轴 `REGION_BLOCKS` 个块）。
///
/// `Ord` 派生保证 `BTreeMap<GroupKey, _>` 的遍历是确定的键升序，stable id 的
/// 分配顺序因此可复现。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GroupKey {
    /// 组内所有块的 LOD 级别（0..=3）。
    pub lod: u8,
    /// 空间区域 X 坐标（单位 = `REGION_BLOCKS << lod` 块）。
    pub rx: i32,
    /// 空间区域 Y 坐标（单位 = `REGION_BLOCKS << lod` 块）。
    pub ry: i32,
    /// 空间区域 Z 坐标（单位 = `REGION_BLOCKS << lod` 块）。
    pub rz: i32,
}

/// mesh 块 -> 分组键：`r* = origin.*.div_euclid(REGION_BLOCKS << lod)`。
///
/// 必须用欧几里得取整：负坐标下 `/` 会向零截断，导致区域边界错位 / 组重叠。
#[must_use]
pub fn group_key(origin: ChunkKey, lod: u8) -> GroupKey {
    let region = REGION_BLOCKS << lod;
    GroupKey {
        lod,
        rx: origin.x.div_euclid(region),
        ry: origin.y.div_euclid(region),
        rz: origin.z.div_euclid(region),
    }
}

/// 分组键对应的区域原点（块坐标，对齐到 `REGION_BLOCKS << lod`）。
#[must_use]
pub fn group_origin(key: GroupKey) -> ChunkKey {
    let region = REGION_BLOCKS << key.lod;
    chunk_key(key.rx * region, key.ry * region, key.rz * region)
}

/// 父块 XZ AABB 到观察者的最近距离（米，切比雪夫 = 两轴取大）是否已经允许
/// 粗到 level。
fn may_coarsen_for_observer(
    observer_xz_m: [f64; 2],
    px: i32,
    pz: i32,
    size: i32,
    voxel_size_m: f64,
    level: u8,
    config: &VoxelLodConfig,
) -> bool {
    let chunk_m = f64::from(CHUNK_SIZE) * voxel_size_m;
    let dx = nearest_axis_distance_m(px, size, chunk_m, observer_xz_m[0]);
    let dz = nearest_axis_distance_m(pz, size, chunk_m, observer_xz_m[1]);
    config.lod_for_distance_m(dx.max(dz)) >= level
}

/// 一维闭区间 [origin*chunk_m, (origin+size)*chunk_m] 到观察者坐标的最近距离（米）。
#[must_use]
pub fn nearest_axis_distance_m(origin: i32, size: i32, chunk_m: f64, observer_axis_m: f64) -> f64 {
    let lo = f64::from(origin) * chunk_m;
    axis_interval_distance(lo, lo + f64::from(size) * chunk_m, observer_axis_m)
}

/// 块（米原点 + 边长）到观察者（米）的最近距离，只取 X / Z 两轴的切比雪夫最大值。
///
/// Y 被刻意忽略：地形 LOD 是水平距离策略。这是 FFI voxel_lod_for_block 的
/// 唯一真相源，与 [lod_blocks_for_observer] 的合并距离同源。
#[must_use]
pub fn nearest_block_distance_m(
    observer_m: [f64; 3],
    block_origin_m: [f64; 3],
    span_m: f64,
) -> f64 {
    let span = span_m.max(0.0);
    let dx = axis_interval_distance(block_origin_m[0], block_origin_m[0] + span, observer_m[0]);
    let dz = axis_interval_distance(block_origin_m[2], block_origin_m[2] + span, observer_m[2]);
    dx.max(dz)
}

/// 一维闭区间 [lo, hi] 到 point 的最近距离（点在区间内为 0）。
fn axis_interval_distance(lo: f64, hi: f64, point: f64) -> f64 {
    if point < lo {
        lo - point
    } else if point > hi {
        point - hi
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::static_data::voxel::VOXEL_SIZE_METERS;

    /// 区域里每个 (x, y, z) 基础子块被叶子恰好覆盖一次（3D 划分不重叠、无空洞）。
    fn assert_partition_of(blocks: &[TerrainBlock], radius_blocks: i32, max_lod: u8) {
        assert!(!blocks.is_empty());
        let extent = radius_blocks.max(1);
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
        for x in -radius_blocks..extent {
            for y in 0..layers {
                for z in -radius_blocks..extent {
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

        assert_two_to_one_xz(blocks, radius_blocks);
    }

    /// 2:1 平衡：任意两个 XZ 边相邻的叶子，lod 差 ≤ 1。
    ///
    /// 这是跨 LOD 邻居采样 / 接缝修复的前提，缺失时会出现裂缝与未剔除面。
    fn assert_two_to_one_xz(blocks: &[TerrainBlock], radius_blocks: i32) {
        let extent = radius_blocks.max(1);
        let mut lod_at: BTreeMap<(i32, i32), u8> = BTreeMap::new();
        for block in blocks {
            let span = block_span(block.lod);
            for dx in 0..span {
                for dz in 0..span {
                    let cell = (block.origin.x + dx, block.origin.z + dz);
                    // 同一个 XZ 叶子的多个 y 块会重复命中同一格：lod 必须相同。
                    if let Some(previous) = lod_at.insert(cell, block.lod) {
                        assert_eq!(previous, block.lod, "({cell:?}) 的 XZ 叶子 lod 冲突");
                    }
                }
            }
        }

        for x in -radius_blocks..extent {
            for z in -radius_blocks..extent {
                let a = lod_at[&(x, z)];
                for (nx, nz) in [(x + 1, z), (x, z + 1)] {
                    if nx >= extent || nz >= extent {
                        continue;
                    }
                    let b = lod_at[&(nx, nz)];
                    assert!(
                        a.abs_diff(b) <= 1,
                        "({x},{z}) lod {a} 与 ({nx},{nz}) lod {b} 违反 2:1 平衡"
                    );
                }
            }
        }
    }

    /// 旧签名包装：静态观察者（世界原点）的划分。
    fn assert_partition(radius_blocks: i32, max_lod: u8) {
        assert_partition_of(
            &lod_blocks(radius_blocks, max_lod, VOXEL_SIZE_METERS),
            radius_blocks,
            max_lod,
        );
    }

    #[test]
    fn small_region_is_all_lod0() {
        let blocks = lod_blocks(2, 1, VOXEL_SIZE_METERS);
        assert!(blocks.iter().all(|block| block.lod == 0));
        // 半开 [-2, 2)：x/z = 4 × 4 个单元，y band = 4 层。
        assert_eq!(blocks.len(), 16 * 4);
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

    /// 分组键必须用欧几里得取整（负坐标），且随 LOD 放大区域步长。
    #[test]
    fn group_key_uses_euclidean_regions_and_lod_shift() {
        // LOD0: 区域边长 = REGION_BLOCKS = 2 块。
        assert_eq!(
            group_key(chunk_key(0, 0, 0), 0),
            GroupKey {
                lod: 0,
                rx: 0,
                ry: 0,
                rz: 0
            }
        );
        assert_eq!(
            group_key(chunk_key(1, 0, 0), 0),
            GroupKey {
                lod: 0,
                rx: 0,
                ry: 0,
                rz: 0
            }
        );
        assert_eq!(
            group_key(chunk_key(2, 0, 0), 0),
            GroupKey {
                lod: 0,
                rx: 1,
                ry: 0,
                rz: 0
            }
        );
        // 负坐标：-1/-2 -> -1，-3 -> -2（div_euclid 而非向零截断）。
        assert_eq!(group_key(chunk_key(-1, 0, 0), 0).rx, -1);
        assert_eq!(group_key(chunk_key(-2, 0, 0), 0).rx, -1);
        assert_eq!(group_key(chunk_key(-3, 0, 0), 0).rx, -2);
        // LOD1: 区域边长 = REGION_BLOCKS << 1 = 4 块。
        assert_eq!(group_key(chunk_key(3, 0, 0), 1).rx, 0);
        assert_eq!(group_key(chunk_key(4, 0, 0), 1).rx, 1);
        // group_origin 反推区域左下角（块坐标）。
        assert_eq!(
            group_origin(group_key(chunk_key(-3, 5, 7), 0)),
            chunk_key(-4, 4, 6)
        );
        // 同一个组内的块必须映射回同一个键，且 group_origin 落在区域起点。
        for origin in [
            chunk_key(0, 0, 0),
            chunk_key(1, 1, 1),
            chunk_key(-1, -1, -1),
        ] {
            let key = group_key(origin, 0);
            let start = group_origin(key);
            assert!(origin.x - start.x >= 0 && origin.x - start.x < REGION_BLOCKS);
            assert!(origin.y - start.y >= 0 && origin.y - start.y < REGION_BLOCKS);
            assert!(origin.z - start.z >= 0 && origin.z - start.z < REGION_BLOCKS);
            assert_eq!(group_key(start, 0), key);
        }
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

    #[test]
    fn observer_partition_matches_static_partition_at_origin() {
        for (radius, max_lod) in [(2, 1), (8, 3), (24, 3)] {
            assert_eq!(
                lod_blocks(radius, max_lod, VOXEL_SIZE_METERS),
                lod_blocks_for_observer(
                    [0.0, 0.0],
                    radius,
                    max_lod,
                    VOXEL_SIZE_METERS,
                    &VoxelLodConfig::default(),
                ),
                "观察者在原点 + 默认配置必须逐块等于旧 lod_blocks（radius {radius}）"
            );
        }
    }

    #[test]
    fn far_observer_coarsens_far_side_and_keeps_near_lod0() {
        // 观察者置于区域 +X 侧（x=100 m 落在第 8 个子块 AABB 内）。
        let observer = [100.0f64, 0.0];
        let blocks = lod_blocks_for_observer(
            observer,
            8,
            3,
            VOXEL_SIZE_METERS,
            &VoxelLodConfig::default(),
        );
        assert_partition_of(&blocks, 8, 3);

        // 观察者附近（半开区域 +X 边缘 x = 7）必须保持 LOD0。
        assert!(
            blocks
                .iter()
                .any(|block| block.origin.x == 7 && block.lod == 0),
            "观察者附近必须保持 LOD0"
        );
        // 远离观察者（x <= -4）必须出现粗于 LOD0 的块。
        let far_max = blocks
            .iter()
            .filter(|block| block.origin.x <= -4)
            .map(|block| block.lod)
            .max()
            .expect("远端必须有块");
        assert!(far_max >= 1, "远端必须变粗，实测最粗 {far_max}");
    }

    /// R=12 半开 + 默认配置 + 观察者在原点：闭区间下 x=+12 整列 / z=+12 整行
    /// 恒为 LOD0（可见的细带），半开后远角必须变粗，且 LOD0 核心四边对称。
    #[test]
    fn observer_at_origin_has_symmetric_lod0_core() {
        let radius = 12;
        let max_lod = 3;
        let blocks = lod_blocks(radius, max_lod, VOXEL_SIZE_METERS);
        assert_partition_of(&blocks, radius, max_lod);
        let partition = TerrainPartition::new(&blocks);

        // 远角 (11, 11)：闭区间下是 LOD0，半开后必须至少 LOD1。
        let corner = partition.xz_lod(11, 11).expect("(11,11) 必须被覆盖");
        assert!(corner >= 1, "远角 (11,11) 必须粗于 LOD0，实测 LOD{corner}");

        // LOD0 叶子覆盖集合：实际分区里是以原点为中心的 8×8 方块 [-4, 4)²。
        let lod0: BTreeSet<(i32, i32)> = blocks
            .iter()
            .filter(|block| block.lod == 0)
            .flat_map(|block| {
                let span = block_span(block.lod);
                (0..span).flat_map(move |dx| {
                    (0..span).map(move |dz| (block.origin.x + dx, block.origin.z + dz))
                })
            })
            .collect();
        let want: BTreeSet<(i32, i32)> =
            (-4..4).flat_map(|x| (-4..4).map(move |z| (x, z))).collect();
        assert_eq!(lod0, want, "LOD0 覆盖必须是中心对称的 [-4,4)²");

        // 四边对称：半开网格关于 x -> -1-x（z 同理）镜像，lod 必须一致。
        for x in -radius..radius {
            for z in -radius..radius {
                assert_eq!(
                    partition.xz_lod(x, z),
                    partition.xz_lod(-1 - x, -1 - z),
                    "({x},{z}) 与镜像格 lod 不一致"
                );
            }
        }
    }

    #[test]
    fn observer_partition_is_deterministic() {
        let first = lod_blocks_for_observer(
            [37.5, -12.25],
            8,
            3,
            VOXEL_SIZE_METERS,
            &VoxelLodConfig::default(),
        );
        let second = lod_blocks_for_observer(
            [37.5, -12.25],
            8,
            3,
            VOXEL_SIZE_METERS,
            &VoxelLodConfig::default(),
        );
        assert_eq!(first, second);
    }

    #[test]
    fn nearest_axis_distance_matches_closed_interval() {
        assert_eq!(nearest_axis_distance_m(0, 2, 10.0, 5.0), 0.0);
        assert_eq!(nearest_axis_distance_m(0, 2, 10.0, -3.0), 3.0);
        assert_eq!(nearest_axis_distance_m(0, 2, 10.0, 25.0), 5.0);
    }

    #[test]
    fn nearest_block_distance_ignores_y() {
        // Y 极远也必须被忽略：块 [0,10]³，观察者在 +Z 外 5 m。
        let distance = nearest_block_distance_m([5.0, 1000.0, 15.0], [0.0, 0.0, 0.0], 10.0);
        assert!((distance - 5.0).abs() < 1e-9);
        // 观察者在块内 -> 0。
        assert_eq!(
            nearest_block_distance_m([5.0, 5.0, 5.0], [0.0, 0.0, 0.0], 10.0),
            0.0
        );
        // X 更远时取两轴最大值。
        let distance = nearest_block_distance_m([-30.0, 0.0, 12.0], [0.0, 0.0, 0.0], 10.0);
        assert!((distance - 30.0).abs() < 1e-9);
    }

    // ─────────────────── 跨 LOD 面邻居报告 ───────────────────

    fn block(x: i32, y: i32, z: i32, lod: u8) -> TerrainBlock {
        TerrainBlock {
            origin: chunk_key(x, y, z),
            lod,
        }
    }

    /// 粗块面对细邻居：4 个细块按切向 2x2 象限行优先报告。
    #[test]
    fn face_neighbors_reports_finer_for_coarse_block() {
        let coarse = block(2, 0, 0, 1);
        let fine = [
            block(1, 0, 0, 0),
            block(1, 1, 0, 0),
            block(1, 0, 1, 0),
            block(1, 1, 1, 0),
        ];
        let mut blocks = vec![coarse];
        blocks.extend_from_slice(&fine);
        let partition = TerrainPartition::new(&blocks);
        let neighbours = neighbour_lod_report(&coarse, &partition);
        // -X 面（face 1）：行 = y、列 = z，行优先。
        assert_eq!(
            neighbours[1],
            FaceNeighbor::Finer([
                chunk_key(1, 0, 0),
                chunk_key(1, 0, 1),
                chunk_key(1, 1, 0),
                chunk_key(1, 1, 1),
            ])
        );
        assert_eq!(neighbours[0], FaceNeighbor::Missing);
        assert_eq!(neighbours[4], FaceNeighbor::Missing);
    }

    /// 细块面对粗邻居：单块 + 2x2 子块偏移（每轴 0 或 16）。
    #[test]
    fn face_neighbors_reports_coarser_with_offset() {
        let coarse = block(2, 0, 0, 1);
        let fine = [
            block(1, 0, 0, 0),
            block(1, 1, 0, 0),
            block(1, 0, 1, 0),
            block(1, 1, 1, 0),
        ];
        let mut blocks = vec![coarse];
        blocks.extend_from_slice(&fine);
        let partition = TerrainPartition::new(&blocks);

        assert_eq!(
            neighbour_lod_report(&block(1, 0, 0, 0), &partition)[0],
            FaceNeighbor::Coarser {
                origin: chunk_key(2, 0, 0),
                offset: [0, 0],
            }
        );
        assert_eq!(
            neighbour_lod_report(&block(1, 0, 1, 0), &partition)[0],
            FaceNeighbor::Coarser {
                origin: chunk_key(2, 0, 0),
                offset: [0, 16],
            }
        );
        assert_eq!(
            neighbour_lod_report(&block(1, 1, 0, 0), &partition)[0],
            FaceNeighbor::Coarser {
                origin: chunk_key(2, 0, 0),
                offset: [16, 0],
            }
        );
        // 同 XZ 叶子的 +Y 邻块是同 LOD。
        assert_eq!(
            neighbour_lod_report(&block(1, 0, 0, 0), &partition)[2],
            FaceNeighbor::Same(chunk_key(1, 1, 0))
        );
    }

    /// 真实划分（含多档 LOD）上报告必须自洽，且不触发 LOD 混面断言。
    #[test]
    fn face_neighbors_are_consistent_for_real_partition() {
        let blocks = lod_blocks(8, 3, VOXEL_SIZE_METERS);
        let partition = TerrainPartition::new(&blocks);
        for block in &blocks {
            for neighbour in neighbour_lod_report(block, &partition) {
                match neighbour {
                    FaceNeighbor::Missing => {}
                    FaceNeighbor::Same(origin) => {
                        assert!(partition.contains(origin, block.lod), "Same 邻块必须存在");
                    }
                    FaceNeighbor::Coarser { origin, offset } => {
                        assert!(
                            partition.contains(origin, block.lod + 1),
                            "Coarser 邻块必须以 lod+1 存在"
                        );
                        assert!(offset[0] == 0 || offset[0] == 16);
                        assert!(offset[1] == 0 || offset[1] == 16);
                    }
                    FaceNeighbor::Finer(origins) => {
                        assert!(block.lod >= 1);
                        for origin in origins {
                            assert!(
                                partition.contains(origin, block.lod - 1),
                                "Finer 邻块必须以 lod-1 存在"
                            );
                        }
                    }
                }
            }
        }
    }

    /// 裙边掩码：只有跨 LOD（Coarser / Finer）面置位。
    #[test]
    fn skirt_mask_marks_only_cross_lod_faces() {
        let coarse = block(2, 0, 0, 1);
        let fine = [
            block(1, 0, 0, 0),
            block(1, 1, 0, 0),
            block(1, 0, 1, 0),
            block(1, 1, 1, 0),
        ];
        let mut blocks = vec![coarse];
        blocks.extend_from_slice(&fine);
        let partition = TerrainPartition::new(&blocks);
        // 粗块 -X 面是 Finer -> bit 1。
        assert_eq!(
            skirt_mask(&neighbour_lod_report(&coarse, &partition)),
            1 << 1
        );
        // 细块 +X 面是 Coarser -> bit 0；+Z 面是 Same -> 不置位。
        assert_eq!(
            skirt_mask(&neighbour_lod_report(&block(1, 0, 0, 0), &partition)),
            1 << 0
        );
    }

    /// 块自闭合：`face_neighbors` 对跨 LOD 面一律报 `Missing`（网格不再做
    /// 跨 LOD 剔除），LOD 错位信息只经 `lod_mismatch_mask` 流出。
    #[test]
    fn face_neighbors_masks_cross_lod_as_missing() {
        let coarse = block(2, 0, 0, 1);
        let fine = [
            block(1, 0, 0, 0),
            block(1, 1, 0, 0),
            block(1, 0, 1, 0),
            block(1, 1, 1, 0),
        ];
        let mut blocks = vec![coarse];
        blocks.extend_from_slice(&fine);
        let partition = TerrainPartition::new(&blocks);

        let fine_faces = face_neighbors(&block(1, 0, 0, 0), &partition);
        assert_eq!(
            fine_faces[0],
            FaceNeighbor::Missing,
            "+X 邻粗块必须报 Missing（块自闭合）"
        );
        assert_eq!(lod_mismatch_mask(&block(1, 0, 0, 0), &partition), 1 << 0);
        assert_eq!(
            fine_faces[2],
            FaceNeighbor::Same(chunk_key(1, 1, 0)),
            "同 LOD 面仍是 Same"
        );

        let coarse_faces = face_neighbors(&coarse, &partition);
        assert_eq!(
            coarse_faces[1],
            FaceNeighbor::Missing,
            "-X 邻细块必须报 Missing（块自闭合）"
        );
        assert_eq!(lod_mismatch_mask(&coarse, &partition), 1 << 1);
    }

    /// ±Z 面的行列轴是 (y, x)：粗邻居的切向 x 偏移必须落在 col。
    #[test]
    fn face_neighbors_coarser_uses_face_row_col_axes_for_z() {
        let coarse = block(0, 0, 2, 1);
        let fine = [
            block(0, 0, 1, 0),
            block(1, 0, 1, 0),
            block(0, 1, 1, 0),
            block(1, 1, 1, 0),
        ];
        let mut blocks = vec![coarse];
        blocks.extend_from_slice(&fine);
        let partition = TerrainPartition::new(&blocks);
        // 粗块 -Z 面（face 5）：行 = y、列 = x，行优先。
        assert_eq!(
            neighbour_lod_report(&coarse, &partition)[5],
            FaceNeighbor::Finer([
                chunk_key(0, 0, 1),
                chunk_key(1, 0, 1),
                chunk_key(0, 1, 1),
                chunk_key(1, 1, 1),
            ])
        );
        // 细块 (1,0,1) 的 +Z 面（face 4）：x 偏移 16 应落在 col。
        assert_eq!(
            neighbour_lod_report(&block(1, 0, 1, 0), &partition)[4],
            FaceNeighbor::Coarser {
                origin: chunk_key(0, 0, 2),
                offset: [0, 16],
            }
        );
    }
}
