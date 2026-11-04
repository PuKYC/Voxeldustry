//! 树：第一个具体的 [`WorldFeature`]（纯数据层）。
//!
//! 提案只依赖 `(seed, owner)`：是否长树、位置与高度全部由
//! [`splitmix64`] 顺序混合派生，没有 `HashMap`、浮点或时间。
//! `realize` 只按提案的几何写体素，不做任何哈希 / 随机。
//!
//! ## 贴地
//!
//! 树根 y 取自地形生成器的 [`WorldGenerator::column_surface`]（同一 seed 下
//! 「第一层空气的 Y」），所以树干正好立在地表上，不靠常量猜高度。
//!
//! 一条提案只由「地表所在的 y 区块」提出（`owner.y == ground / 32`）：
//! 提案校验要求 `origin` 落在自己的包围盒里，而同一列只有地表块满足；
//! 其它 y 块返回空提案，避免同一棵树被上下几个块各提一次。
//!
//! **已知债**：`WorldFeature::propose` 的签名只有 `(seed, owner)`，拿不到
//! 生物群系，所以这里按默认群系算地表高度。非默认群系的岛若换了 base_height /
//! amplitude，树会浮起或埋进地里；等 trait 带上群系再收口。

use game_engine::save::splitmix64;
use game_engine::streaming::ProposalRecord;
use game_engine::voxel::{chunk_voxels_per_axis, ChunkKey};

use crate::voxel::feature::{ChunkSink, ProposalSpec, WorldFeature};
use crate::voxel::terrain::generation::WorldGenerator;

/// 本块长树的百分比阈值：`hash % 100 < TREE_CHANCE_PERCENT` 才长。
const TREE_CHANCE_PERCENT: u64 = 35;

/// 树干方块 ID（木质）。
const TRUNK_BLOCK: u8 = 2;
/// 树叶方块 ID。
const LEAF_BLOCK: u8 = 3;

/// 提案优先级。
const TREE_PRIORITY: i32 = 10;

/// 树冠曼哈顿半径（同时决定包围盒的 x/z 余量）。
const CROWN_RADIUS: i32 = 2;

/// 树：每块最多 1 棵，`TYPE_ID = 100`。
///
/// 几何：
/// - 树干：中心列 `(origin.x, origin.z)`，从 `origin.y` 到 `origin.y + height`（含）；
/// - 树冠：三层（`height - 2 ..= height`）曼哈顿半径 2 的团，中心列保持木质。
///
/// 包围盒单轴跨度最大 5（x/z）/ `height + 2`（y，最大 9），远小于
/// `MAX_PROPOSAL_EXTENT`（96）。
#[derive(Default)]
pub struct TreeFeature;

impl WorldFeature for TreeFeature {
    const TYPE_ID: u16 = 100;

    fn propose(&self, seed: u64, owner: ChunkKey) -> Vec<ProposalSpec> {
        let hash = tree_hash(seed, owner);
        // 35% 的块长树：是否长树与位置 / 高度来自同一个 hash。
        if hash % 100 >= TREE_CHANCE_PERCENT {
            return Vec::new();
        }

        // x/z 取 1..=30：靠近区块边缘时树冠会越过边界（span 仍是 5），
        // 由 ChunkSink 在邻块那一侧只写自己那一段。
        let x = ((hash >> 8) % 30 + 1) as i32;
        let z = ((hash >> 16) % 30 + 1) as i32;
        // 4..=7，`as` 不会截断。
        let height = ((hash >> 24) % 4 + 4) as i64;
        let chunk = i64::from(chunk_voxels_per_axis());
        let world_x = i64::from(owner.x) * chunk + i64::from(x);
        let world_z = i64::from(owner.z) * chunk + i64::from(z);

        // 高度查询收 i32：世界列坐标溢出 i32 的块不种树（这种块也进不了 realize）。
        let (Ok(column_x), Ok(column_z)) = (i32::try_from(world_x), i32::try_from(world_z)) else {
            return Vec::new();
        };
        // column_surface 是「第一层空气的 Y」，树干底面就站在这里（下方是地表方块）。
        let origin_y =
            i64::from(WorldGenerator::from_seed(seed).column_surface(column_x, column_z));

        // 同一列只有地表所在的 y 块提出这棵树：提案要求 origin 落在自己的包围盒内，
        // 其它 y 块的 origin.y 与地表高度对不上，提了也过不了校验。
        if i32::try_from(origin_y.div_euclid(chunk)).ok() != Some(owner.y) {
            return Vec::new();
        }

        // 全程用 i64 算，最后逐轴转 i32：极端区块坐标下 i32 乘法 / 加法会溢出。
        let origin = [world_x, origin_y, world_z];
        let bounds_min = [
            world_x - i64::from(CROWN_RADIUS),
            origin_y - 1,
            world_z - i64::from(CROWN_RADIUS),
        ];
        let bounds_max = [
            world_x + i64::from(CROWN_RADIUS) + 1,
            origin_y + height + 1,
            world_z + i64::from(CROWN_RADIUS) + 1,
        ];
        let (Some(origin), Some(bounds_min), Some(bounds_max)) =
            (to_i32_3(origin), to_i32_3(bounds_min), to_i32_3(bounds_max))
        else {
            // 世界坐标已经超出 i32：这样的块根本进不了 realize_chunk，直接不长树。
            return Vec::new();
        };

        vec![ProposalSpec {
            origin,
            bounds_min,
            bounds_max,
            priority: TREE_PRIORITY,
        }]
    }

    fn realize(&self, _seed: u64, proposal: &ProposalRecord, sink: &mut ChunkSink<'_>) {
        let origin = proposal.origin;
        // 高度由包围盒反推：realize 只认提案几何，不重新哈希（逐块结果一致）。
        let height = proposal.bounds_max[1] - origin[1] - 1;

        for dy in 0..=height {
            sink.set_world([origin[0], origin[1] + dy, origin[2]], TRUNK_BLOCK);
        }

        for dy in height - CROWN_RADIUS..=height {
            for dx in -CROWN_RADIUS..=CROWN_RADIUS {
                for dz in -CROWN_RADIUS..=CROWN_RADIUS {
                    // 树干列保持木质；树冠只写曼哈顿半径内的体素。
                    if (dx == 0 && dz == 0) || dx.abs() + dz.abs() > CROWN_RADIUS {
                        continue;
                    }
                    sink.set_world([origin[0] + dx, origin[1] + dy, origin[2] + dz], LEAF_BLOCK);
                }
            }
        }
    }
}

/// `(seed, owner)` -> 64 位哈希：用 [`splitmix64`] 顺序混合，禁止一次 XOR 拼接。
///
/// `owner` 必须进哈希：只混 `seed` 会让所有区块在同一相对位置长同一棵树。
fn tree_hash(seed: u64, owner: ChunkKey) -> u64 {
    let mut hash = splitmix64(seed ^ 0x5452_4545);
    hash = splitmix64(hash ^ i64::from(owner.x) as u64);
    hash = splitmix64(hash ^ i64::from(owner.y) as u64);
    splitmix64(hash ^ i64::from(owner.z) as u64)
}

/// 三轴 `i64 -> i32` 转换；任一轴溢出返回 `None`。
fn to_i32_3(value: [i64; 3]) -> Option<[i32; 3]> {
    Some([
        i32::try_from(value[0]).ok()?,
        i32::try_from(value[1]).ok()?,
        i32::try_from(value[2]).ok()?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::math::IVec3;
    use game_engine::voxel::{
        dec_ref_recursive, MaxDepth, VoxInterner, VoxOpsRead, VoxTree, CHUNK_DEPTH,
    };

    use crate::voxel::feature::{realize_chunk, WorldFeatureRegistry};

    /// 扫描 y 块，找出这一列真正被提案的那棵树。
    ///
    /// 提案只由「地表所在的 y 块」提出，而 x/z 与地表高度都来自 hash（hash 又
    /// 含 owner.y），所以调用方拿不到 ground 之前只能逐 y 试。命中返回
    /// `(owner, 提案)`，否则 `None`。
    fn find_tree(
        registry: &WorldFeatureRegistry,
        seed: u64,
        x: i32,
        z: i32,
    ) -> Option<(ChunkKey, ProposalRecord)> {
        for y in -4..=4 {
            let owner = ChunkKey::new(x, y, z);
            let proposals = registry.propose_chunk(seed, owner).expect("提案必须合法");
            if let Some(proposal) = proposals.first() {
                return Some((owner, *proposal));
            }
        }
        None
    }

    /// 提案的包围盒是否完整落在 `owner` 这一块里（三轴都不越界）。
    fn fits_in_one_chunk(owner: ChunkKey, proposal: &ProposalRecord) -> bool {
        let extent = chunk_voxels_per_axis();
        let base = [owner.x * extent, owner.y * extent, owner.z * extent];
        (0..3).all(|axis| {
            proposal.bounds_min[axis] >= base[axis]
                && proposal.bounds_max[axis] <= base[axis] + extent
        })
    }

    /// 在若干列 × seed 范围里找第一棵，返回 `(seed, owner, 提案)`。
    fn first_tree(
        registry: &WorldFeatureRegistry,
        columns: &[(i32, i32)],
        seeds: std::ops::Range<u64>,
    ) -> (u64, ChunkKey, ProposalRecord) {
        for seed in seeds {
            for &(x, z) in columns {
                if let Some((owner, proposal)) = find_tree(registry, seed, x, z) {
                    return (seed, owner, proposal);
                }
            }
        }
        panic!("前提失效：给定 seed / 列范围内没有任何一棵树");
    }

    fn tree_registry() -> WorldFeatureRegistry {
        let mut registry = WorldFeatureRegistry::default();
        registry
            .register::<TreeFeature>()
            .expect("TreeFeature 必须能注册");
        registry
    }

    fn empty_tree_and_interner() -> (VoxTree<u8>, VoxInterner<u8>) {
        (
            VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH)),
            VoxInterner::<u8>::with_memory_budget(2 * 1024 * 1024),
        )
    }

    /// `VoxTree` 没有 `Drop`：测试结束前显式归还节点引用。
    fn release_tree(tree: &VoxTree<u8>, interner: &mut VoxInterner<u8>) {
        let root = tree.get_root_id();
        dec_ref_recursive(interner, &root);
    }

    fn count_value(tree: &VoxTree<u8>, interner: &VoxInterner<u8>, value: u8) -> usize {
        let extent = chunk_voxels_per_axis();
        let mut count = 0;
        for x in 0..extent {
            for y in 0..extent {
                for z in 0..extent {
                    if tree.get(interner, IVec3::new(x, y, z)).unwrap_or(0) == value {
                        count += 1;
                    }
                }
            }
        }
        count
    }

    fn count_non_air(tree: &VoxTree<u8>, interner: &VoxInterner<u8>) -> usize {
        let extent = chunk_voxels_per_axis();
        let mut count = 0;
        for x in 0..extent {
            for y in 0..extent {
                for z in 0..extent {
                    if tree.get(interner, IVec3::new(x, y, z)).unwrap_or(0) != 0 {
                        count += 1;
                    }
                }
            }
        }
        count
    }

    #[test]
    fn propose_is_deterministic() {
        let registry = tree_registry();
        let columns = [(0, 0), (-1, -2), (3, 5)];
        let mut proposals = 0;
        for seed in 0..8_u64 {
            for &(x, z) in &columns {
                let Some((owner, _)) = find_tree(&registry, seed, x, z) else {
                    continue;
                };
                let first = registry.propose_chunk(seed, owner).expect("提案必须合法");
                let second = registry.propose_chunk(seed, owner).expect("提案必须合法");
                assert_eq!(first, second, "同一 (seed, owner) 必须逐字段相同");
                proposals += first.len();
            }
        }
        assert!(proposals > 0, "这些 seed / 列组合里至少要长出一棵树");
    }

    #[test]
    fn tree_proposal_bounds_contain_origin() {
        let registry = tree_registry();
        let columns = [(0, 0), (-1, -2), (7, 4), (-3, 0)];
        let mut proposals = 0;
        for seed in 0..8_u64 {
            for &(x, z) in &columns {
                let Some((owner, _)) = find_tree(&registry, seed, x, z) else {
                    continue;
                };
                for proposal in registry.propose_chunk(seed, owner).expect("提案必须合法") {
                    for axis in 0..3 {
                        assert!(proposal.bounds_min[axis] < proposal.bounds_max[axis]);
                        assert!(
                            proposal.bounds_min[axis] <= proposal.origin[axis]
                                && proposal.origin[axis] < proposal.bounds_max[axis],
                            "seed {seed} owner {owner:?} 轴 {axis} 的 origin 必须落在 [min, max)"
                        );
                    }
                    proposals += 1;
                }
            }
        }
        assert!(proposals > 0, "这些 seed / 列组合里至少要长出一棵树");
    }

    #[test]
    fn tree_can_be_realized_into_one_chunk() {
        let registry = tree_registry();
        let columns = [(0, 0), (5, 5), (-3, 2)];
        let mut found = None;
        'search: for seed in 0..4096_u64 {
            for &(x, z) in &columns {
                let Some((owner, proposal)) = find_tree(&registry, seed, x, z) else {
                    continue;
                };
                if fits_in_one_chunk(owner, &proposal) {
                    found = Some((seed, owner, proposal));
                    break 'search;
                }
            }
        }
        let (seed, owner, proposal) = found.expect("前提失效：找不到完整落在一块里的树");

        let (mut tree, mut interner) = empty_tree_and_interner();
        let realized = realize_chunk(
            &registry,
            seed,
            owner,
            &[proposal],
            &mut tree,
            &mut interner,
        )
        .expect("realize 必须成功");
        assert!(realized > 0, "至少落地一条提案");

        let trunk = count_value(&tree, &interner, TRUNK_BLOCK);
        let leaves = count_value(&tree, &interner, LEAF_BLOCK);
        assert!(trunk > 0, "必须写出至少 1 个体素 ID {TRUNK_BLOCK}（树干）");
        assert!(leaves > 0, "必须写出至少 1 个体素 ID {LEAF_BLOCK}（树叶）");

        release_tree(&tree, &mut interner);
    }

    #[test]
    fn tree_writes_both_sides_of_chunk_border() {
        let registry = tree_registry();
        let chunk = chunk_voxels_per_axis();

        // 找一棵包围盒越过本块边界的树（x/z 任一侧）。
        let mut found: Option<(u64, ChunkKey, ProposalRecord, usize, i32)> = None;
        'search: for seed in 0..4096_u64 {
            let Some((owner, proposal)) = find_tree(&registry, seed, 0, 0) else {
                continue;
            };
            // 树冠要留在同一个 y 带里：否则越界的那一侧只剩树干、树冠跑到上一块，
            // 「邻块这一侧必须有体素」就取决于 y 而不再是 x/z。
            let band = owner.y * chunk;
            if proposal.bounds_min[1] < band || proposal.bounds_max[1] > band + chunk {
                continue;
            }
            for axis in [0_usize, 2] {
                if proposal.bounds_max[axis] > chunk {
                    found = Some((seed, owner, proposal, axis, 1));
                    break 'search;
                }
                if proposal.bounds_min[axis] < 0 {
                    found = Some((seed, owner, proposal, axis, -1));
                    break 'search;
                }
            }
        }
        let (seed, owner, proposal, axis, side) =
            found.expect("前提失效：seed 0..4096 内没有越过区块边界的树");

        let mut neighbor = owner;
        if axis == 0 {
            neighbor.x += side;
        } else {
            neighbor.z += side;
        }

        let (mut own_tree, mut own_interner) = empty_tree_and_interner();
        assert_eq!(
            realize_chunk(
                &registry,
                seed,
                owner,
                &[proposal],
                &mut own_tree,
                &mut own_interner
            ),
            Ok(1),
            "本块必须与跨界提案相交"
        );

        let (mut other_tree, mut other_interner) = empty_tree_and_interner();
        assert_eq!(
            realize_chunk(
                &registry,
                seed,
                neighbor,
                &[proposal],
                &mut other_tree,
                &mut other_interner
            ),
            Ok(1),
            "邻块必须与跨界提案相交"
        );

        assert!(
            count_non_air(&own_tree, &own_interner) > 0,
            "本块这一侧必须有非空气体素"
        );
        assert!(
            count_non_air(&other_tree, &other_interner) > 0,
            "邻块这一侧必须有非空气体素"
        );

        release_tree(&own_tree, &mut own_interner);
        release_tree(&other_tree, &mut other_interner);
    }

    #[test]
    fn tree_stands_on_terrain_surface() {
        let registry = tree_registry();
        let (seed, _owner, proposal) = first_tree(&registry, &[(0, 0)], 0..64);

        let ground =
            WorldGenerator::from_seed(seed).column_surface(proposal.origin[0], proposal.origin[2]);
        assert_eq!(
            proposal.origin[1], ground,
            "树干底面必须落在第一层空气的 Y（column_surface）上"
        );
        // 树干不悬空：包围盒下界留一格给地表方块。
        assert_eq!(proposal.bounds_min[1], ground - 1);
    }

    #[test]
    fn every_tree_origin_lies_in_its_owner_chunk() {
        // 提案校验要求 origin ∈ 包围盒，而 origin.y 就是地表高度：所以能提出
        // 这棵树的 y 块只可能是「地表所在块」。
        let registry = tree_registry();
        let extent = chunk_voxels_per_axis();
        let mut proposals = 0;

        for seed in 0..16_u64 {
            for y in -4..=4 {
                let owner = ChunkKey::new(0, y, 0);
                for proposal in registry.propose_chunk(seed, owner).expect("提案必须合法") {
                    assert_eq!(
                        proposal.origin[1].div_euclid(extent),
                        y,
                        "seed {seed}：提案只能由地表所在的 y 块提出"
                    );
                    proposals += 1;
                }
            }
        }

        assert!(proposals > 0, "这些 seed / y 块里至少要长出一棵树");
    }
}
