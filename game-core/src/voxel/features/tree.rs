//! 树：第一个具体的 [`WorldFeature`]（纯数据层）。
//!
//! 提案只依赖 `(seed, owner)`：是否长树、位置与高度全部由
//! [`splitmix64`] 顺序混合派生，没有 `HashMap`、浮点或时间。
//! `realize` 只按提案的几何写体素，不做任何哈希 / 随机。
//!
//! ## 阶段限制
//!
//! 地表高度还没有接入生成器查询，本阶段用固定常量 [`TREE_GROUND_Y`]；
//! 「贴地」需要按 `(seed, owner)` 查地形高度，属于后续接生成器的任务。

use game_engine::save::splitmix64;
use game_engine::streaming::ProposalRecord;
use game_engine::voxel::{chunk_voxels_per_axis, ChunkKey};

use crate::voxel::feature::{ChunkSink, ProposalSpec, WorldFeature};

/// 本阶段树根所在的世界 y（临时常量）。
///
/// 纯数据层不调用生成器、也不读 `VoxVolume`，所以暂时固定在地表附近；
/// 真正的「贴地」需要按 `(seed, owner)` 查询地形高度（后续接生成器）。
pub const TREE_GROUND_Y: i32 = 64;

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
        let x = (hash >> 8) % 30 + 1;
        let z = (hash >> 16) % 30 + 1;
        // 4..=7，`as` 不会截断。
        let height = ((hash >> 24) % 4 + 4) as i64;
        let origin_y = i64::from(TREE_GROUND_Y);
        let chunk = i64::from(chunk_voxels_per_axis());
        let world_x = i64::from(owner.x) * chunk + x as i64;
        let world_z = i64::from(owner.z) * chunk + z as i64;

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

    /// 树根固定在世界 y = 64，落在地面区块 y = 2（64..96）。
    const GROUND_CHUNK: i32 = TREE_GROUND_Y / 32;

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

    /// 在 `seeds` 里找第一棵对 `owner` 长出的树；找不到说明测试前提失效。
    fn first_tree(
        registry: &WorldFeatureRegistry,
        owner: ChunkKey,
        seeds: std::ops::Range<u64>,
    ) -> (u64, ProposalRecord) {
        for seed in seeds {
            let proposals = registry.propose_chunk(seed, owner).expect("提案必须合法");
            if let Some(proposal) = proposals.first() {
                return (seed, *proposal);
            }
        }
        panic!("前提失效：给定 seed 范围内没有任何一棵树");
    }

    #[test]
    fn propose_is_deterministic() {
        let registry = tree_registry();
        let owners = [
            ChunkKey::new(0, GROUND_CHUNK, 0),
            ChunkKey::new(-1, GROUND_CHUNK, -2),
            ChunkKey::new(3, GROUND_CHUNK, 5),
        ];
        let mut proposals = 0;
        for seed in 0..8_u64 {
            for owner in owners {
                let first = registry.propose_chunk(seed, owner).expect("提案必须合法");
                let second = registry.propose_chunk(seed, owner).expect("提案必须合法");
                assert_eq!(first, second, "同一 (seed, owner) 必须逐字段相同");
                proposals += first.len();
            }
        }
        assert!(proposals > 0, "这些 seed / owner 组合里至少要长出一棵树");
    }

    #[test]
    fn tree_proposal_bounds_contain_origin() {
        let registry = tree_registry();
        let owners = [
            ChunkKey::new(0, GROUND_CHUNK, 0),
            ChunkKey::new(-1, GROUND_CHUNK, -2),
            ChunkKey::new(7, GROUND_CHUNK, 4),
            ChunkKey::new(-3, GROUND_CHUNK, 0),
        ];
        let mut proposals = 0;
        for seed in 0..8_u64 {
            for owner in owners {
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
        assert!(proposals > 0, "这些 seed / owner 组合里至少要长出一棵树");
    }

    #[test]
    fn tree_can_be_realized_into_one_chunk() {
        let registry = tree_registry();
        let owner = ChunkKey::new(0, GROUND_CHUNK, 0);
        let (seed, proposal) = first_tree(&registry, owner, 0..64);

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
        let owner = ChunkKey::new(0, GROUND_CHUNK, 0);
        let chunk = chunk_voxels_per_axis();

        // 找一棵包围盒越过本块边界的树（x/z 任一侧）。
        let mut found: Option<(u64, ProposalRecord, usize, i32)> = None;
        'search: for seed in 0..1024_u64 {
            for proposal in registry.propose_chunk(seed, owner).expect("提案必须合法") {
                for axis in [0_usize, 2] {
                    if proposal.bounds_max[axis] > chunk {
                        found = Some((seed, proposal, axis, 1));
                        break 'search;
                    }
                    if proposal.bounds_min[axis] < 0 {
                        found = Some((seed, proposal, axis, -1));
                        break 'search;
                    }
                }
            }
        }
        let (seed, proposal, axis, side) =
            found.expect("前提失效：seed 0..1024 内没有越过区块边界的树");

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
}
