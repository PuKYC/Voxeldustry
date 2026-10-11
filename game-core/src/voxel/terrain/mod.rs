//! 体素地形接入（体素表现通道 = RawVoxelGroups 混合分组）：Startup 生成一圈
//! 地形块，按 (LOD × 空间区域) 混合分组产出带 [VoxChunkGroupRaw] 的分组实体。
//!
//! 体素容器 [VoxVolume] **只作为组件**挂在地形根实体上
//! （「一岛 / 一船 / 一结构 = 一个 VoxVolume 组件」），不作为 Resource；
//! 本模块不持有任何全局体素单例。
//!
//! ## 数据流
//!
//! 1. `Startup`：按 [TerrainConfig] 的半径 / max_lod 用 `WorldGenerator` 生成
//!    基础子块（x/z 多生成一圈 margin，让边缘块的 external 邻块有数据）；
//! 2. 逐块走引擎生产管线：`WrappedBlockCache::get_or_wrap` +
//!    `extract_raw_halo` 得到内部 32³ + 整圈 halo（6 面 + 12 棱 + 8 角，共 34³）
//!    的方块 id 缓冲；
//! 3. 每个分组一个 stable id 实体：`VoxChunkGroupRaw{lod, chunks}` +
//!    `Transform` / `PresentedTransform`（组原点，米）+ `Size(None)`；
//! 4. 体素容器 `VoxVolume` 本地构建、生成完一次性 spawn 成**组件**。
//!
//! 贪婪 meshing 在 Godot 侧完成（调 gdext `mesh_voxel_halo` 现算 39 bit 矩形流）。
//!
//! ## 边界
//!
//! - 块原点（米）= `origin * 32 * voxel_size`，**不乘 2^lod**（见
//!   [blocks::block_origin_meters]）；
//! - 原始体素是块局部、位置无关的数据，LOD 缩放由渲染侧按 payload 的 lod 处理；
//! - 本模块不依赖 godot，也不改 crates/voxel 或 game-engine。
//!
//! ## 运行期流式重划分（observer-relative LOD）
//!
//! 生成仍在 Startup 一次完成（有限区域），但 LOD 分区在运行期按观察者位置重算：
//! 1. 观察者来自 [VoxelLodObserver]；若存在 [VoxelLodHandle] 且其 observer 为
//!    Some，则由 Godot 经 FFI set_voxel_observer 覆写（config 同理）；
//! 2. stream_terrain_lod（Update）用 lod_blocks_for_observer 算出期望分区，
//!    归并成期望 [GroupKey] 集合，与 [TerrainGroups] + 成员快照做组级 diff；
//! 3. 新增 / 成员块集合变化的组才重建（组内逐块 extract_block_raw），消失的组
//!    despawn，**未变化的组完全不动**；统计从成员快照重算，不漂移。
//!
//! 体素表现改走 RawVoxelGroups 分组载荷（每块仍携带 skirt_mask）；本模块只改变
//! 「哪些块、什么 LOD、怎么分组」，不改 crates/voxel 或 game-engine。

pub mod blocks;
pub mod generation;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use game_engine::identity::{StableEntityId, StableEntityIndex, StableIdAllocator};
use game_engine::presentation::voxel::{
    extract_raw_halo_with_faces, raw_halo_has_solid_interior, FaceNeighbor as MeshFaceNeighbor,
    WrappedBlockCache, RAW_VOXELS,
};
use game_engine::presentation::RenderTransform;
use game_engine::spatial::Size;
use game_engine::voxel::{ChunkKey, Lod, MeshBlock, Neighbourhood, VoxVolume, VoxelInterner};

use crate::presentation::voxel_mesh::VoxChunkGroupRaw;
use crate::presentation::PresentedTransform;
use crate::static_data::voxel::{BiomeId, DEFAULT_BIOME, DEFAULT_VOXEL_SIZE};
use crate::voxel::lod::{VoxelLodConfig, MAX_LOD};
use crate::voxel::terrain::generation::WorldGenerator;
use crate::world::WorldSeed;

use blocks::FaceNeighbor as TerrainFaceNeighbor;
pub use blocks::{
    block_origin_meters, block_span, chunk_keys_to_generate, face_neighbors, group_key,
    group_origin, lod_blocks, lod_blocks_for_observer, nearest_axis_distance_m,
    nearest_block_distance_m, neighbor_origins, skirt_mask, y_band_layers, FaceNeighbor, GroupKey,
    TerrainBlock, TerrainPartition, REGION_BLOCKS, TERRAIN_MARGIN_CHUNKS,
};

/// 地形生成参数（体素层资源）。装配方在 `add_plugins(GameVoxelPlugin)` **之前**
/// `insert_resource` 后，[TerrainPlugin] 就会注册地形生成系统；不插入则不生成。
///
/// 地形由独立插件 [TerrainPlugin] 装配（依赖 [GameVoxelPlugin]）。体素本身只以组件存在——
/// `VoxVolume`（容器）+ `VoxChunkGroupRaw`（分组内每块的 32³ + halo 原始体素）；本结构只是
/// 生成参数，不是体素数据。
///
/// `radius_blocks` 是 x/z 上围绕世界原点的基础子块半径（1 个基础子块 = 32
/// 体素 ≈ 14.4 m）。debug 构建生成很慢（实测约 150 ms/子块），所以默认值刻意小：
/// - [TerrainConfig::default]：radius 2 / max_lod 1（7×7×4 ≈ 196 子块；debug ≈ 30 s，
///   release ≈ 2–4 s）；
/// - [TerrainConfig::demo]：radius 0 / max_lod 0（36 子块；debug ≈ 5 s）。
///
/// LOD 合并要半径 ≥ 5 才会越过 32 m 的第一档阈值，所以想看 LOD 切换要用
/// release 构建并调大半径（FFI：`start_bevy_voxel_perf(fixed_hz, runner_hz, radius, max_lod)`）。
///
/// [GameVoxelPlugin]: crate::voxel::GameVoxelPlugin
#[derive(Resource, Clone, Copy, Debug)]
pub struct TerrainConfig {
    /// 水平方向围绕世界原点的 mesh 块半径（基础子块坐标）。
    pub radius_blocks: i32,
    /// 最高 LOD（0..=3，见 `voxel::MAX_LOD`）。
    pub max_lod: u8,
    /// 世界生成种子。
    pub seed: u64,
    /// 生物群系（决定调色板与高度基线）。
    pub biome: BiomeId,
}

impl TerrainConfig {
    /// 默认实体 demo 用的极小地形：1 个 XZ 单元（36 个子块）、无 LOD。
    ///
    /// debug 构建实测约 150 ms/子块（36 子块 ≈ 5 s），所以"默认挂在地形上的
    /// demo"用最小规模；要看更大范围请走 `start_bevy_voxel_perf(..., radius, lod)`。
    #[must_use]
    pub fn demo() -> Self {
        Self {
            radius_blocks: 0,
            max_lod: 0,
            seed: 0xC0FF_EE,
            biome: DEFAULT_BIOME,
        }
    }
}

impl Default for TerrainConfig {
    fn default() -> Self {
        Self {
            radius_blocks: 2,
            max_lod: 1,
            seed: 0xC0FF_EE,
            biome: DEFAULT_BIOME,
        }
    }
}

/// 地形插件（独立）：依赖 [GameVoxelPlugin]；装配方先插入 [TerrainConfig] 才注册系统。
pub struct TerrainPlugin;

impl Plugin for TerrainPlugin {
    fn build(&self, app: &mut App) {
        assert!(
            app.is_plugin_added::<super::GameVoxelPlugin>(),
            "TerrainPlugin 依赖 GameVoxelPlugin"
        );
        if app.world().get_resource::<TerrainConfig>().is_some() {
            register_systems(app);
        }
    }
}

/// 注册地形生成系统与运行期资源：由 [TerrainPlugin]
/// 在 `TerrainConfig` 资源存在时调用。
///
/// 只做两件事：挂 `Startup` 生成系统、初始化实体索引 / 统计资源；体素数据
/// 本身只以组件存在（生成时把 `VoxVolume` spawn 到实体上）。
pub(crate) fn register_systems(app: &mut App) {
    app.init_resource::<TerrainGroups>()
        .init_resource::<TerrainGroupMembers>()
        .init_resource::<TerrainNeighborBlocks>()
        // 表现层网格机制不注册插件；WrappedBlockCache 由唯一消费者 core 侧 init。
        .init_resource::<WrappedBlockCache>()
        .init_resource::<TerrainStats>()
        .init_resource::<VoxelLodConfig>()
        .init_resource::<VoxelLodObserver>()
        .init_resource::<TerrainStreamState>()
        // 观察者写入者在 FixedUpdate；流式重划分放 Update 以便同 tick 看到新位置。
        .add_systems(Startup, build_terrain)
        .add_systems(Update, stream_terrain_lod);
}

/// 已产出的 mesh 分组实体索引：`GroupKey -> Entity`（BTreeMap 键升序 = 确定性）。
///
/// 一个 [GroupKey] 对应一个分组实体（[VoxChunkGroupRaw]），组内所有块同 LOD。
/// F2 起 terrain producer 以**组**为单位 diff / spawn / despawn；stable id 也逐组分配。
#[derive(Resource, Default, Debug)]
pub struct TerrainGroups(BTreeMap<GroupKey, Entity>);

impl TerrainGroups {
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[must_use]
    pub fn get(&self, key: GroupKey) -> Option<Entity> {
        self.0.get(&key).copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = (GroupKey, Entity)> + '_ {
        self.0.iter().map(|(key, entity)| (*key, *entity))
    }
}

/// 组级 diff 的成员快照：组上次构建时覆盖的期望块（**含空块**）与非空块数。
///
/// "未变化的组完全不动" 的判据就是这个集合：组内期望成员集合不变就不 re-extract、
/// 不 spawn。必须记录含空块的成员，否则全空组每次重算分区都会被当成新组重建。
#[derive(Resource, Default, Debug)]
struct TerrainGroupMembers(BTreeMap<GroupKey, GroupMembers>);

/// 一个组上次构建时的成员统计（块级口径）。
#[derive(Clone, Debug, Default)]
struct GroupMembers {
    /// 该组覆盖的 desired 块（块原点；组内同 LOD）。
    blocks: BTreeSet<ChunkKey>,
    /// 其中产出几何（非空）的块数。
    solid: usize,
    /// 上次构建时每个成员块的 6 面 LOD 感知邻居报告。
    ///
    /// 成员集合不变但某邻块 LOD 变化时，本块的 halo 边界采样与 `skirt_mask` 会变，
    /// 必须据此一并触发重建（否则跨 LOD 裙边会在 LOD 环收放后残留）。
    neighbours: BTreeMap<ChunkKey, [FaceNeighbor; 6]>,
}

/// 邻块包装缓存：`extract_raw_halo` 的 26 邻只读借用与它自己的
/// `&mut WrappedBlockCache` 不能来自同一个变量（借用冲突），所以邻块单独一份。
#[derive(Resource, Default)]
pub struct TerrainNeighborBlocks(pub WrappedBlockCache);

/// 地形统计（demo / perf / 测试断言用；不进协议）。
#[derive(Resource, Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerrainStats {
    /// 生成的基础子块数。
    pub chunks: usize,
    /// 当前有几何的 mesh 块数（组内逐块计，块级口径）。
    pub blocks: usize,
    /// 当前产出的分组实体数（一个 [GroupKey] 一个实体）。
    pub groups: usize,
    /// 每个 LOD 的有几何块数（下标 = lod，0..=3）。
    pub lod_blocks: [usize; 4],
    /// 每个 LOD 的分组实体数（下标 = lod，0..=3）。
    pub lod_groups: [usize; 4],
    /// desired 块里因全空气而没产出几何的块数（块级口径）。
    pub skipped_empty: usize,
}

/// 观察者位置（米，世界坐标）。运行期流式系统按它重划分 LOD。
///
/// 默认 [0.0, 0.0, 0.0]；由 crate::dev::perf::move_voxel_observer 或
/// godot-client-ext 经 [VoxelLodHandle] 的 observer 覆写驱动。
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq)]
pub struct VoxelLodObserver {
    pub position: [f32; 3],
}

/// 跨线程（Godot -> Bevy）的 LOD 运行期输入。
///
/// None 表示不覆写：observer 覆写 [VoxelLodObserver]，config 覆写 [VoxelLodConfig]。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct VoxelLodRuntime {
    pub observer: Option<[f32; 3]>,
    pub config: Option<VoxelLodConfig>,
}

/// FFI 句柄：Godot 主线程写，Bevy 后台线程读，内部 Arc<Mutex<..>>。
#[derive(Resource, Clone, Default)]
pub struct VoxelLodHandle(pub Arc<Mutex<VoxelLodRuntime>>);

/// 流式系统的上一次输入（early-out 用）。
#[derive(Resource, Default)]
struct TerrainStreamState {
    last_observer: [f32; 3],
    last_config: Option<VoxelLodConfig>,
}

/// Setup 用体素边长（米，f64 镜像）。
#[must_use]
pub fn voxel_size_meters() -> f64 {
    DEFAULT_VOXEL_SIZE.to_num::<f64>()
}

/// Startup：生成子块 -> 逐块网格化 -> 产出 stable id 实体。
///
/// 体素容器在本地构建，生成完成后作为 `VoxVolume` **组件** spawn 到地形
/// 根实体上，不写进任何 Resource。
///
/// 用普通 system（多 `ResMut`）而不是 exclusive system，是为了让
/// interner / wrapped / 邻块缓存 / 增量缓存的借用天然分离；实体 id 由
/// `StableIdAllocator` 立即分配并登记进 `StableEntityIndex`（等价
/// `spawn_stable` 的当帧语义）。
/// 提取一个块的原始体素 halo（34³）与跨 LOD 裙边掩码；内部无实心体素返回 None。
///
/// 返回 (halo 字节, skirt_mask)：掩码由 face_neighbors 报告经 [skirt_mask] 求得，
/// 只有面外邻居 LOD 不同（更粗 / 更细）的面置位，0 = 无裙边。
///
/// 共享给 Startup 生成与运行期流式重划分，避免两条路径分叉。邻块缓存与本块
/// 包装缓存必须是两个资源（external 只读借用与 &mut 本块缓存冲突）。
#[allow(clippy::too_many_arguments)]
fn extract_block_raw(
    block: TerrainBlock,
    volume: &VoxVolume,
    _voxel_size_m: f32,
    partition: &TerrainPartition,
    interner: &mut VoxelInterner,
    wrapped: &mut WrappedBlockCache,
    neighbors: &mut TerrainNeighborBlocks,
) -> Option<(Vec<u8>, u8)> {
    let lod = Lod::new(block.lod);
    let mesh_block = MeshBlock::new(block.origin, lod);
    let span = 1i32 << block.lod;
    // 面邻居报告（纯原点 / LOD）：2:1 平衡保证只有同 LOD / 更粗 / 更细三种。
    let plan = face_neighbors(&block, partition);
    // 跨 LOD 接缝裙边掩码：只有 Coarser / Finer 面置位，随 raw 一起交给消费侧。
    let skirt = skirt_mask(&plan);

    // 预热同 LOD 的 26 邻（6 面 + 12 棱 + 8 角），随后只读取回。对角邻块让
    // halo 的棱 / 角也能被填充，从而给出真正的 3D AO；缺失 = 空气。
    for dx in -1i32..=1 {
        for dy in -1i32..=1 {
            for dz in -1i32..=1 {
                if dx == 0 && dy == 0 && dz == 0 {
                    continue;
                }
                let neighbor = ChunkKey::new(
                    mesh_block.origin.x + dx * span,
                    mesh_block.origin.y + dy * span,
                    mesh_block.origin.z + dz * span,
                );
                neighbors.0.get_or_wrap(
                    &volume.chunks,
                    interner.inner_mut(),
                    MeshBlock::new(neighbor, lod),
                );
            }
        }
    }

    // 预热跨 LOD 面邻居（粗 / 细各按自己的 lod wrap；同 LOD 面上面已预热）。
    for plan_face in plan.iter() {
        match plan_face {
            TerrainFaceNeighbor::Missing => {}
            TerrainFaceNeighbor::Same(origin) => {
                neighbors.0.get_or_wrap(
                    &volume.chunks,
                    interner.inner_mut(),
                    MeshBlock::new(*origin, lod),
                );
            }
            TerrainFaceNeighbor::Coarser { origin, .. } => {
                neighbors.0.get_or_wrap(
                    &volume.chunks,
                    interner.inner_mut(),
                    MeshBlock::new(*origin, Lod::new(block.lod + 1)),
                );
            }
            TerrainFaceNeighbor::Finer(origins) => {
                let neighbour_lod = Lod::new(block.lod.saturating_sub(1));
                for origin in origins.iter() {
                    neighbors.0.get_or_wrap(
                        &volume.chunks,
                        interner.inner_mut(),
                        MeshBlock::new(*origin, neighbour_lod),
                    );
                }
            }
        }
    }
    let neighbourhood = {
        let neighbor_cache = &neighbors.0;
        let mut out = Neighbourhood::none();
        for dx in -1i8..=1 {
            for dy in -1i8..=1 {
                for dz in -1i8..=1 {
                    if dx == 0 && dy == 0 && dz == 0 {
                        continue;
                    }
                    let neighbor = ChunkKey::new(
                        mesh_block.origin.x + i32::from(dx) * span,
                        mesh_block.origin.y + i32::from(dy) * span,
                        mesh_block.origin.z + i32::from(dz) * span,
                    );
                    if let Some(tree) = neighbor_cache.get(neighbor, lod) {
                        out.set(dx, dy, dz, tree);
                    }
                }
            }
        }
        out
    };

    // 把面邻居报告解析成 voxel 层描述符（借用上面已预热的树）。
    let mesh_faces = {
        let cache = &neighbors.0;
        let mut out = [MeshFaceNeighbor::Missing; 6];
        for (face, plan_face) in plan.iter().enumerate() {
            out[face] = match plan_face {
                TerrainFaceNeighbor::Missing => MeshFaceNeighbor::Missing,
                TerrainFaceNeighbor::Same(origin) => match cache.get(*origin, lod) {
                    Some(tree) => MeshFaceNeighbor::Same(tree),
                    None => MeshFaceNeighbor::Missing,
                },
                TerrainFaceNeighbor::Coarser { origin, offset } => {
                    let neighbour_lod = Lod::new(block.lod + 1);
                    match cache.get(*origin, neighbour_lod) {
                        Some(tree) => MeshFaceNeighbor::Coarser {
                            tree,
                            offset: *offset,
                        },
                        None => MeshFaceNeighbor::Missing,
                    }
                }
                TerrainFaceNeighbor::Finer(origins) => {
                    let neighbour_lod = Lod::new(block.lod.saturating_sub(1));
                    match (
                        cache.get(origins[0], neighbour_lod),
                        cache.get(origins[1], neighbour_lod),
                        cache.get(origins[2], neighbour_lod),
                        cache.get(origins[3], neighbour_lod),
                    ) {
                        (Some(a), Some(b), Some(c), Some(d)) => {
                            MeshFaceNeighbor::Finer([a, b, c, d])
                        }
                        _ => MeshFaceNeighbor::Missing,
                    }
                }
            };
        }
        out
    };

    // 用已 wrap 的块树 + LOD 感知面邻居 + 26 邻树提取原始体素
    //（内部 32³ + 整圈 halo）。
    let tree = wrapped.get_or_wrap(&volume.chunks, interner.inner_mut(), mesh_block);
    let raw = extract_raw_halo_with_faces(
        tree,
        interner.inner(),
        mesh_block,
        mesh_faces,
        neighbourhood,
    );
    debug_assert_eq!(raw.len(), RAW_VOXELS, "extract_raw_halo 必须产出 34³");
    if raw_halo_has_solid_interior(&raw) {
        Some((raw, skirt))
    } else {
        None
    }
}

/// 构造一个 mesh 分组实体 bundle（stable id + 逐块 raw/mask + 组原点变换 + Size）。
#[allow(clippy::type_complexity)]
fn group_bundle(
    id: StableEntityId,
    key: GroupKey,
    chunks: BTreeMap<ChunkKey, (Vec<u8>, u8)>,
    voxel_size_m: f32,
) -> (
    StableEntityId,
    VoxChunkGroupRaw,
    Transform,
    Size,
    PresentedTransform,
) {
    // 组原点（米，块基准尺度）= group_origin * 32 * voxel_size；仅影响 culling，
    // VoxelGroupNode 的子节点用各自块的绝对世界坐标，与组原点无关。
    let position = Vec3::from_array(block_origin_meters(group_origin(key), voxel_size_m));
    let mut presented = PresentedTransform::default();
    presented.set_sampled(
        RenderTransform::from_translation(position),
        RenderTransform::from_translation(position),
    );
    (
        id,
        VoxChunkGroupRaw {
            lod: key.lod,
            chunks,
        },
        Transform::from_translation(position),
        Size(None),
        presented,
    )
}

/// 把 desired 块列表归并成 `GroupKey -> 成员块`（BTreeMap，键升序确定）。
fn desired_groups(desired: &[TerrainBlock]) -> BTreeMap<GroupKey, BTreeSet<ChunkKey>> {
    let mut out: BTreeMap<GroupKey, BTreeSet<ChunkKey>> = BTreeMap::new();
    for block in desired {
        out.entry(group_key(block.origin, block.lod))
            .or_default()
            .insert(block.origin);
    }
    out
}

/// 组成员块的 6 面 LOD 感知邻居签名（块原点升序 = 确定）。
///
/// 把「成员集合相同、但邻块 LOD 变化」也纳入重建判据：邻块 LOD 影响本块
/// `extract_raw_halo_with_faces` 的边界采样与 [`skirt_mask`]，只比成员集合会漏重建。
fn group_neighbour_signature(
    blocks: &BTreeSet<ChunkKey>,
    lod: u8,
    partition: &TerrainPartition,
) -> BTreeMap<ChunkKey, [FaceNeighbor; 6]> {
    blocks
        .iter()
        .map(|origin| {
            let block = TerrainBlock {
                origin: *origin,
                lod,
            };
            (*origin, face_neighbors(&block, partition))
        })
        .collect()
}

/// 组重建 / 消失时按块失效 C/D 的包装缓存（口径不变：逐块 invalidate_covered）。
fn invalidate_group(
    blocks: &BTreeSet<ChunkKey>,
    interner: &mut VoxelInterner,
    wrapped: &mut WrappedBlockCache,
    neighbors: &mut TerrainNeighborBlocks,
) {
    for origin in blocks {
        wrapped.invalidate_covered(interner.inner_mut(), *origin);
        neighbors.0.invalidate_covered(interner.inner_mut(), *origin);
    }
}

/// 组级 diff + 重建：Startup 与运行期流式共用，避免两条路径分叉。
///
/// 硬约束：只有**新增组 / 成员块集合变化的组**才对该组内每块跑 extract_block_raw；
/// 未变化的组完全不动（不 re-extract、不 spawn）。组内全空则不 spawn，但成员快照
/// 仍记录，避免下次重算分区时把全空组当成新组反复重建。
/// stable id 逐组分配、按 GroupKey 升序，分配顺序确定。
#[allow(clippy::too_many_arguments)]
fn apply_group_diff(
    commands: &mut Commands,
    volume: &VoxVolume,
    desired: &[TerrainBlock],
    partition: &TerrainPartition,
    voxel_size_f32: f32,
    interner: &mut VoxelInterner,
    wrapped: &mut WrappedBlockCache,
    neighbors: &mut TerrainNeighborBlocks,
    groups: &mut TerrainGroups,
    members: &mut TerrainGroupMembers,
    stats: &mut TerrainStats,
    allocator: &mut StableIdAllocator,
    index: &mut StableEntityIndex,
) {
    let desired_by_group = desired_groups(desired);

    // ── 1. 消失的组（快照里有、期望里没有）：despawn + 逐块失效 ──
    let gone: Vec<GroupKey> = members
        .0
        .keys()
        .filter(|key| !desired_by_group.contains_key(*key))
        .copied()
        .collect();
    for key in gone {
        if let Some(entity) = groups.0.remove(&key) {
            index.remove_by_entity(entity);
            commands.entity(entity).despawn();
        }
        if let Some(old) = members.0.remove(&key) {
            invalidate_group(&old.blocks, interner, wrapped, neighbors);
        }
    }

    // ── 2. 新增 / 成员集变化的组：重建；未变化的组直接跳过 ──
    for (key, desired_blocks) in &desired_by_group {
        // 重建判据 = 成员集合 + 每成员块的 6 面 LOD 邻居签名。邻块 LOD 变化时
        // 成员集合可能完全不变，但本块 halo 采样与 skirt_mask 会变，必须一并重建。
        let neighbours = group_neighbour_signature(desired_blocks, key.lod, partition);
        let unchanged = members
            .0
            .get(key)
            .is_some_and(|old| &old.blocks == desired_blocks && old.neighbours == neighbours);
        if unchanged {
            continue;
        }
        // 旧实体换新（stable id 重分配），旧 id 先注销。
        if let Some(entity) = groups.0.remove(key) {
            index.remove_by_entity(entity);
            commands.entity(entity).despawn();
        }
        // 失效口径仍按块：旧成员 + 新成员逐个 invalidate_covered。
        if let Some(old) = members.0.get(key) {
            invalidate_group(&old.blocks, interner, wrapped, neighbors);
        }
        invalidate_group(desired_blocks, interner, wrapped, neighbors);

        let mut chunks: BTreeMap<ChunkKey, (Vec<u8>, u8)> = BTreeMap::new();
        for origin in desired_blocks {
            let block = TerrainBlock {
                origin: *origin,
                lod: key.lod,
            };
            if let Some((raw, mask)) = extract_block_raw(
                block,
                volume,
                voxel_size_f32,
                partition,
                interner,
                wrapped,
                neighbors,
            ) {
                chunks.insert(*origin, (raw, mask));
            }
        }
        let solid = chunks.len();
        if !chunks.is_empty() {
            let id = allocator.allocate();
            let entity = commands
                .spawn(group_bundle(id, *key, chunks, voxel_size_f32))
                .id();
            index.insert(id, entity);
            groups.0.insert(*key, entity);
        }
        members.0.insert(
            *key,
            GroupMembers {
                blocks: desired_blocks.clone(),
                solid,
                neighbours,
            },
        );
    }

    // ── 3. 统计从成员快照重算，避免漂移（chunks 是生成期口径，不动）──
    stats.groups = groups.0.len();
    stats.blocks = 0;
    stats.skipped_empty = 0;
    stats.lod_blocks = [0; 4];
    stats.lod_groups = [0; 4];
    for (key, group) in &members.0 {
        let lod = usize::from(key.lod);
        stats.blocks += group.solid;
        stats.skipped_empty += group.blocks.len() - group.solid;
        stats.lod_blocks[lod] += group.solid;
        if groups.0.contains_key(key) {
            stats.lod_groups[lod] += 1;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_terrain(
    mut commands: Commands,
    config: Res<TerrainConfig>,
    mut interner: ResMut<VoxelInterner>,
    mut wrapped: ResMut<WrappedBlockCache>,
    mut neighbors: ResMut<TerrainNeighborBlocks>,
    mut groups: ResMut<TerrainGroups>,
    mut members: ResMut<TerrainGroupMembers>,
    mut stats: ResMut<TerrainStats>,
    mut allocator: ResMut<StableIdAllocator>,
    mut index: ResMut<StableEntityIndex>,
) {
    let voxel_size_m = voxel_size_meters();
    let voxel_size_f32 = DEFAULT_VOXEL_SIZE.to_num::<f32>();
    let radius_blocks = config.radius_blocks.max(0);
    let max_lod = config.max_lod.min(MAX_LOD);

    // ── 1. 生成基础子块（本地 VoxVolume，最后作为组件挂到地形根实体）──
    let mut volume = VoxVolume::new(DEFAULT_VOXEL_SIZE);
    let generator = WorldGenerator::new(WorldSeed(config.seed), config.biome);
    for key in chunk_keys_to_generate(radius_blocks, max_lod) {
        let tree = generator.generate_chunk(interner.inner_mut(), key);
        volume.insert_chunk(key, tree);
    }
    stats.chunks = volume.chunk_count();

    // ── 2. 混合分组 + 组内逐块网格化 + 产出实体（分区 = 静态观察者）──
    let planned_blocks = lod_blocks(radius_blocks, max_lod, voxel_size_m);
    let partition = TerrainPartition::new(&planned_blocks);
    apply_group_diff(
        &mut commands,
        &volume,
        &planned_blocks,
        &partition,
        voxel_size_f32,
        &mut interner,
        &mut wrapped,
        &mut neighbors,
        &mut groups,
        &mut members,
        &mut stats,
        &mut allocator,
        &mut index,
    );

    // 地形根实体：体素容器以组件形式挂载
    //（「一岛 / 一船 / 一结构 = 一个 VoxVolume 组件」）；生成完成后一次性 attach。
    // physics：同时挂 VoxelCollider，让地形派生 avian 碰撞体（Transform 由
    // #[require(Transform)] 补为原点，正好等于 body 局部原点）。
    #[cfg(feature = "physics")]
    commands.spawn((volume, game_engine::voxel::VoxelCollider));
    #[cfg(not(feature = "physics"))]
    commands.spawn(volume);

    info!(
        "terrain: chunks={} blocks={} lod={:?} seed={:#x}",
        stats.chunks, stats.blocks, stats.lod_blocks, config.seed
    );
}

/// 运行期流式重划分：按观察者把 LOD 分区 diff 到 [TerrainGroups]，增删分组实体。
///
/// 输入优先级：若存在 [VoxelLodHandle] 且 observer / config 为 Some，则覆写
/// [VoxelLodObserver] / [VoxelLodConfig]（Godot 经 FFI 驱动）；否则用 Rust 侧资源
/// （crate::dev::perf 的移动观察者）。
///
/// 确定性：desired / current 都用 BTreeSet，增删按排序顺序处理，因此
/// StableIdAllocator 的分配顺序可复现。观察者只动一点时分区往往不变，diff 为空；
/// 只有 (observer, config) 变化才重算分区。
#[allow(clippy::too_many_arguments)]
fn stream_terrain_lod(
    mut commands: Commands,
    config: Res<TerrainConfig>,
    lod_handle: Option<Res<VoxelLodHandle>>,
    mut lod_config: ResMut<VoxelLodConfig>,
    mut observer: ResMut<VoxelLodObserver>,
    mut stream: ResMut<TerrainStreamState>,
    mut interner: ResMut<VoxelInterner>,
    mut wrapped: ResMut<WrappedBlockCache>,
    mut neighbors: ResMut<TerrainNeighborBlocks>,
    mut groups: ResMut<TerrainGroups>,
    mut members: ResMut<TerrainGroupMembers>,
    mut stats: ResMut<TerrainStats>,
    mut allocator: ResMut<StableIdAllocator>,
    mut index: ResMut<StableEntityIndex>,
    volume_query: Query<&VoxVolume>,
) {
    // ── 1. 解析 FFI 覆写，并把配置夹到合法域 ──
    if let Some(handle) = lod_handle.as_deref() {
        if let Ok(runtime) = handle.0.lock() {
            if let Some(position) = runtime.observer {
                observer.position = position;
            }
            if let Some(runtime_config) = runtime.config {
                let sanitized = runtime_config.sanitized();
                if *lod_config != sanitized {
                    *lod_config = sanitized;
                }
            }
        }
    }
    let effective = lod_config.sanitized();
    if effective != *lod_config {
        *lod_config = effective;
    }
    let observer_position = observer.position;

    // ── 2. early-out：观察者与配置都没变则不动 ──
    if stream.last_config == Some(effective) && stream.last_observer == observer_position {
        return;
    }

    let Some(volume) = volume_query.iter().next() else {
        // 体素容器尚未生成：不推进状态，下一 tick 重试。
        return;
    };
    stream.last_config = Some(effective);
    stream.last_observer = observer_position;

    let voxel_size_m = voxel_size_meters();
    let voxel_size_f32 = DEFAULT_VOXEL_SIZE.to_num::<f32>();
    let radius_blocks = config.radius_blocks.max(0);
    let max_lod = config.max_lod.min(MAX_LOD);

    // ── 3. 期望分区（与 Startup 相同的 base cell 集合，只改 LOD 合并基准）──
    let desired = lod_blocks_for_observer(
        [
            f64::from(observer_position[0]),
            f64::from(observer_position[2]),
        ],
        radius_blocks,
        max_lod,
        voxel_size_m,
        &effective,
    );
    let partition = TerrainPartition::new(&desired);

    // ── 4. 组级 diff + 重建（BTreeMap 键升序 = 确定性分配顺序）──
    apply_group_diff(
        &mut commands,
        volume,
        &desired,
        &partition,
        voxel_size_f32,
        &mut interner,
        &mut wrapped,
        &mut neighbors,
        &mut groups,
        &mut members,
        &mut stats,
        &mut allocator,
        &mut index,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use game_engine::identity::{StableEntityId, StableIdPlugin};
    use game_engine::voxel::{
        chunk_key, count_body_nodes, release_body, MaxDepth, VoxOpsBulkWrite, VoxTree, CHUNK_DEPTH,
    };

    /// 半径 0（1 个 XZ 单元）：debug 下生成最便宜，单元测试够用。
    ///
    /// 装配方插入 [TerrainConfig] 后由 [TerrainPlugin] 注册生成系统。
    fn terrain_app(radius_blocks: i32, max_lod: u8) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StableIdPlugin);
        app.insert_resource(TerrainConfig {
            radius_blocks,
            max_lod,
            ..TerrainConfig::default()
        });
        app.add_plugins(crate::voxel::GameVoxelPlugin);
        app.add_plugins(TerrainPlugin);
        // 只用 app.update() 的测试必须显式 finish，Avian 在 Plugin::finish 注册诊断资源。
        app.finish();
        app
    }

    /// 地形根实体：体素容器必须是**组件**，不是 Resource。
    fn volume_entity(world: &mut World) -> Entity {
        let mut query = world.query_filtered::<Entity, With<VoxVolume>>();
        query
            .iter(world)
            .next()
            .expect("地形根实体必须带 VoxVolume 组件")
    }

    /// 锁定边界：体素容器只以组件存在，不能退回全局 Resource。
    #[test]
    fn terrain_volume_is_a_component_not_a_resource() {
        let mut app = terrain_app(0, 0);
        app.update();
        let entity = volume_entity(app.world_mut());
        assert!(
            app.world().entity(entity).get::<VoxVolume>().is_some(),
            "体素容器必须是实体上的 VoxVolume 组件"
        );
    }

    #[test]
    fn build_terrain_produces_group_entities() {
        let mut app = terrain_app(0, 0);
        app.update();

        let stats = *app.world().resource::<TerrainStats>();
        let layers = y_band_layers(0) as usize;
        // 半径 0 → 1 个 XZ 单元 × y band；天空层（全空气）被跳过。
        assert_eq!(stats.chunks, 3 * 3 * layers, "半径 0 + margin 1");
        // 块级口径：非空块 + 全空块 = 期望块总数。
        assert_eq!(stats.blocks + stats.skipped_empty, layers);
        assert!(stats.blocks > 0, "地表块必须产出几何");
        assert!(stats.skipped_empty > 0, "天空层必须被跳过");
        assert_eq!(stats.lod_blocks[0], stats.blocks);

        let groups = app.world().resource::<TerrainGroups>();
        assert_eq!(groups.len(), stats.groups);
        assert!(stats.groups > 0 && stats.groups <= layers, "组数应少于块数");
        assert_eq!(stats.lod_groups[0], stats.groups);

        let mut total_chunks = 0usize;
        for (key, entity) in groups.iter() {
            assert_eq!(key.lod, 0);
            let entity_ref = app.world().entity(entity);
            assert!(entity_ref.get::<StableEntityId>().is_some());
            assert!(entity_ref.get::<Size>().is_some());
            assert!(entity_ref.get::<PresentedTransform>().is_some());

            let raw = entity_ref
                .get::<VoxChunkGroupRaw>()
                .expect("mesh 分组实体必须带 VoxChunkGroupRaw");
            assert_eq!(raw.lod, 0);
            assert!(!raw.chunks.is_empty(), "只有含非空块才会 spawn");
            for (origin, (blocks, mask)) in &raw.chunks {
                assert_eq!(group_key(*origin, 0), key, "成员块必须属于该组");
                assert_eq!(blocks.len(), RAW_VOXELS);
                assert!(raw_halo_has_solid_interior(blocks));
                // LOD0 + 全 LOD0 划分：任何面都不跨 LOD -> mask = 0。
                assert_eq!(*mask, 0);
                total_chunks += 1;
            }

            // Transform 米坐标 = 组原点 * 32 * voxel_size（不乘 2^lod）。
            let transform = entity_ref
                .get::<Transform>()
                .expect("分组实体必须带 Transform");
            let want = block_origin_meters(group_origin(key), DEFAULT_VOXEL_SIZE.to_num::<f32>());
            assert!((transform.translation.x - want[0]).abs() < 1e-3);
            assert!((transform.translation.y - want[1]).abs() < 1e-3);
            assert!((transform.translation.z - want[2]).abs() < 1e-3);
        }
        assert_eq!(total_chunks, stats.blocks, "组内块数总和 = 非空块数");
    }

    /// 手搭含跨 LOD 面的划分：extract_block_raw 的 skirt_mask 必须等于
    /// skirt_mask(face_neighbors(...))，且只有跨 LOD 面置位。
    #[test]
    fn extract_block_raw_reports_cross_lod_skirt_mask() {
        // 被测块 (1,0,0) LOD0；+X 面外是 LOD1 粗块 (2,0,0)（跨 LOD），
        // +Y 面是同 LOD 块 (1,1,0)（不应置位）。
        let block = TerrainBlock {
            origin: chunk_key(1, 0, 0),
            lod: 0,
        };
        let coarse = TerrainBlock {
            origin: chunk_key(2, 0, 0),
            lod: 1,
        };
        let same_y = TerrainBlock {
            origin: chunk_key(1, 1, 0),
            lod: 0,
        };
        let partition = TerrainPartition::new(&[block, coarse, same_y]);
        let expected = skirt_mask(&face_neighbors(&block, &partition));
        assert_eq!(expected, 1 << 0, "只有 +X 面跨 LOD");

        // 只放被测块自己的实心 base 子块；缺失邻块按空气处理，不影响掩码。
        let mut interner = VoxelInterner::new(4 * 1024 * 1024);
        let mut volume = VoxVolume::new(DEFAULT_VOXEL_SIZE);
        let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH as u8));
        tree.fill(interner.inner_mut(), 1);
        volume.insert_chunk(block.origin, tree);

        let mut wrapped = WrappedBlockCache::default();
        let mut neighbors = TerrainNeighborBlocks::default();
        let (raw, mask) = extract_block_raw(
            block,
            &volume,
            DEFAULT_VOXEL_SIZE.to_num::<f32>(),
            &partition,
            &mut interner,
            &mut wrapped,
            &mut neighbors,
        )
        .expect("实心块必须产出 raw halo");

        assert_eq!(raw.len(), RAW_VOXELS);
        assert!(raw_halo_has_solid_interior(&raw));
        assert_eq!(
            mask,
            skirt_mask(&face_neighbors(&block, &partition)),
            "extract_block_raw 的掩码必须与纯函数报告一致"
        );
        assert_eq!(mask, expected);
    }

    #[test]
    fn build_terrain_is_deterministic() {
        let mut first = terrain_app(0, 0);
        first.update();
        let mut second = terrain_app(0, 0);
        second.update();

        // 确定性快照：组键 + 组内逐块 (原点, halo, mask)，均按键升序。
        let group_snapshot = |app: &App| -> Vec<(GroupKey, Vec<(ChunkKey, Vec<u8>, u8)>)> {
            let groups = app.world().resource::<TerrainGroups>();
            let mut out = Vec::new();
            for (key, entity) in groups.iter() {
                let raw = app
                    .world()
                    .entity(entity)
                    .get::<VoxChunkGroupRaw>()
                    .expect("VoxChunkGroupRaw");
                let chunks: Vec<(ChunkKey, Vec<u8>, u8)> = raw
                    .chunks
                    .iter()
                    .map(|(origin, (blocks, mask))| (*origin, blocks.clone(), *mask))
                    .collect();
                out.push((key, chunks));
            }
            out
        };
        assert_eq!(group_snapshot(&first), group_snapshot(&second));
        assert_eq!(
            *first.world().resource::<TerrainStats>(),
            *second.world().resource::<TerrainStats>()
        );
    }

    #[test]
    fn release_terrain_returns_interner_to_baseline() {
        let mut app = terrain_app(0, 0);
        // 基线：GameVoxelPlugin 刚建好 interner、还没生成任何子块
        //（空哨兵本身可能算一个 node，所以不能断言 0）。
        let baseline = app
            .world()
            .resource::<VoxelInterner>()
            .inner()
            .alive_nodes();

        app.update();

        let alive_before = app
            .world()
            .resource::<VoxelInterner>()
            .inner()
            .alive_nodes();
        assert!(alive_before > baseline, "生成后必须有可达节点");
        let entity = volume_entity(app.world_mut());
        let volume = app
            .world()
            .entity(entity)
            .get::<VoxVolume>()
            .expect("地形根实体必须带 VoxVolume 组件");
        let volume_nodes =
            count_body_nodes(app.world().resource::<VoxelInterner>().inner(), volume);
        assert!(volume_nodes > 0);

        // 先清两个包装缓存（否则 root 引用把节点钉住），再 release_body。
        let world = app.world_mut();
        world.resource_scope::<VoxelInterner, _>(|world, mut interner| {
            {
                let mut wrapped = world.resource_mut::<WrappedBlockCache>();
                wrapped.clear(interner.inner_mut());
            }
            {
                let mut neighbors = world.resource_mut::<TerrainNeighborBlocks>();
                neighbors.0.clear(interner.inner_mut());
            }
            let entity = volume_entity(world);
            let mut entity_mut = world.entity_mut(entity);
            let mut volume = entity_mut
                .get_mut::<VoxVolume>()
                .expect("地形根实体必须带 VoxVolume 组件");
            let released = release_body(interner.inner_mut(), &mut volume);
            assert!(released > 0);
            assert_eq!(count_body_nodes(interner.inner(), &volume), 0);
            assert_eq!(
                interner.inner().alive_nodes(),
                baseline,
                "包装缓存与 body 释放后可达节点必须回到基线"
            );
        });
    }

    /// 大半径 LOD 场景：debug 下生成太慢，只在需要时手动跑。
    #[test]
    #[ignore = "半径 8 / LOD3 的生成在 debug 下很慢；release 手动运行"]
    fn terrain_with_lods_builds_coarse_blocks() {
        let mut app = terrain_app(8, 3);
        app.update();
        let stats = *app.world().resource::<TerrainStats>();
        assert!(stats.lod_blocks[1] > 0 || stats.lod_blocks[2] > 0);
    }

    // ─────────────── 运行期流式重划分（合成体素，不跑 worldgen）───────────────

    /// 合成实心体素 + 只挂 key 资源的 App：跳过 worldgen，测试快且完全确定。
    ///
    /// 直接调用私有系统 stream_terrain_lod，等于把流式逻辑从生成里隔离出来验证。
    fn streaming_app(radius: i32, max_lod: u8) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.add_plugins(StableIdPlugin);
        app.add_plugins(crate::voxel::GameVoxelPlugin);
        app.init_resource::<WrappedBlockCache>()
            .init_resource::<TerrainNeighborBlocks>()
            .init_resource::<TerrainGroups>()
            .init_resource::<TerrainGroupMembers>()
            .init_resource::<TerrainStats>()
            .init_resource::<VoxelLodObserver>()
            .init_resource::<VoxelLodConfig>()
            .init_resource::<TerrainStreamState>();
        app.insert_resource(TerrainConfig {
            radius_blocks: radius,
            max_lod,
            ..TerrainConfig::default()
        });
        app.add_systems(Update, stream_terrain_lod);
        // 只用 app.update() 的测试必须显式 finish，Avian 在 Plugin::finish 注册诊断资源。
        app.finish();

        let world = app.world_mut();
        let mut volume = VoxVolume::new(DEFAULT_VOXEL_SIZE);
        world.resource_scope::<VoxelInterner, _>(|world, mut interner| {
            for key in chunk_keys_to_generate(radius, max_lod) {
                let mut tree = VoxTree::<u8>::new(MaxDepth::new(CHUNK_DEPTH as u8));
                tree.fill(interner.inner_mut(), 1);
                volume.insert_chunk(key, tree);
            }
            world.spawn(volume);
        });
        app
    }

    fn set_stream_observer(app: &mut App, position: [f32; 3]) {
        app.world_mut().resource_mut::<VoxelLodObserver>().position = position;
    }

    fn set_stream_config(app: &mut App, config: VoxelLodConfig) {
        *app.world_mut().resource_mut::<VoxelLodConfig>() = config;
    }

    /// 当前分组键（BTreeMap 升序 -> 直接是确定性顺序）。
    fn stream_group_keys(app: &App) -> Vec<GroupKey> {
        app.world()
            .resource::<TerrainGroups>()
            .iter()
            .map(|(key, _)| key)
            .collect()
    }

    #[test]
    fn stream_terrain_lod_restreams_around_observer() {
        let mut app = streaming_app(1, 1);
        set_stream_config(
            &mut app,
            VoxelLodConfig {
                max_lod: 1,
                thresholds_m: [1.0, 2.0, 3.0],
            },
        );

        // 第一帧：观察者在原点 -> 母块 AABB 都含原点，距离 0，全部 LOD0。
        set_stream_observer(&mut app, [0.0, 0.0, 0.0]);
        app.update();
        let before = *app.world().resource::<TerrainStats>();
        assert!(before.blocks > 0);
        assert_eq!(before.lod_blocks[0], before.blocks, "近观察者必须全 LOD0");
        let stale = app
            .world()
            .resource::<TerrainGroups>()
            .get(group_key(chunk_key(0, 0, 0), 0))
            .expect("(0,0,0) 所在的 LOD0 组必须存在");

        // 观察者移到 +X 极远 -> 完整母块合并成 LOD1。
        set_stream_observer(&mut app, [10_000.0, 0.0, 0.0]);
        app.update();

        let stats = *app.world().resource::<TerrainStats>();
        assert!(
            stats.lod_blocks[1] > 0,
            "远端必须出现 LOD1：{:?}",
            stats.lod_blocks
        );
        assert_eq!(stats.groups, app.world().resource::<TerrainGroups>().len());
        assert_eq!(
            app.world().resource::<StableEntityIndex>().len(),
            stats.groups,
            "实体索引必须与 TerrainGroups 一致"
        );
        assert!(
            app.world().get_entity(stale).is_err(),
            "离开分区的旧组实体必须被 despawn"
        );
        for (key, entity) in app.world().resource::<TerrainGroups>().iter() {
            let entity_ref = app
                .world()
                .get_entity(entity)
                .unwrap_or_else(|_| panic!("分组实体 {key:?} 必须存在"));
            assert!(entity_ref.get::<VoxChunkGroupRaw>().is_some());
        }
    }

    fn run_observer_sequence() -> (Vec<GroupKey>, TerrainStats) {
        let mut app = streaming_app(1, 1);
        set_stream_config(
            &mut app,
            VoxelLodConfig {
                max_lod: 1,
                thresholds_m: [1.0, 2.0, 3.0],
            },
        );
        for position in [
            [0.0, 0.0, 0.0],
            [100.0, 0.0, 0.0],
            [100.0, 0.0, 200.0],
            [-100.0, 0.0, -50.0],
        ] {
            set_stream_observer(&mut app, position);
            app.update();
        }
        (
            stream_group_keys(&app),
            *app.world().resource::<TerrainStats>(),
        )
    }

    #[test]
    fn stream_terrain_lod_is_deterministic() {
        let first = run_observer_sequence();
        let second = run_observer_sequence();
        assert_eq!(first.0, second.0, "同一观察者序列必须产生相同分区键");
        assert_eq!(first.1, second.1, "统计必须一致");
    }

    #[test]
    fn stream_terrain_lod_keeps_registered_state_consistent() {
        // 走真实 TerrainPlugin 注册路径（radius 0 的 worldgen 便宜）。
        let mut app = terrain_app(0, 0);
        app.update();
        let before = *app.world().resource::<TerrainStats>();

        // radius 0 没有可合并的母块：换阈值 + 远观察者也不改分区。
        *app.world_mut().resource_mut::<VoxelLodConfig>() = VoxelLodConfig {
            max_lod: 0,
            thresholds_m: [1.0, 2.0, 3.0],
        };
        app.world_mut().resource_mut::<VoxelLodObserver>().position = [5_000.0, 0.0, 0.0];
        app.update();

        let after = *app.world().resource::<TerrainStats>();
        assert_eq!(before.blocks, after.blocks);
        assert_eq!(before.groups, after.groups);
        assert_eq!(after.groups, app.world().resource::<TerrainGroups>().len());
        assert_eq!(
            app.world().resource::<StableEntityIndex>().len(),
            after.groups
        );
    }

    /// (a) 合成全实心地形：每个分组实体的块集合 == 该 GroupKey 覆盖的 desired 块。
    ///
    /// 用固定的 thresholds/config 调一次与系统相同的 `lod_blocks_for_observer`，
    /// 直接比对，避免依赖具体分区形状。
    #[test]
    fn group_members_match_desired_blocks() {
        let config = VoxelLodConfig {
            max_lod: 1,
            thresholds_m: [1.0, 2.0, 3.0],
        };
        let mut app = streaming_app(1, 1);
        set_stream_config(&mut app, config);
        set_stream_observer(&mut app, [0.0, 0.0, 0.0]);
        app.update();

        let desired = lod_blocks_for_observer([0.0, 0.0], 1, 1, voxel_size_meters(), &config);
        let mut want: BTreeMap<GroupKey, BTreeSet<ChunkKey>> = BTreeMap::new();
        for block in &desired {
            want.entry(group_key(block.origin, block.lod))
                .or_default()
                .insert(block.origin);
        }

        let groups = app.world().resource::<TerrainGroups>();
        let have: BTreeMap<GroupKey, BTreeSet<ChunkKey>> = groups
            .iter()
            .map(|(key, entity)| {
                let raw = app
                    .world()
                    .entity(entity)
                    .get::<VoxChunkGroupRaw>()
                    .expect("VoxChunkGroupRaw");
                assert!(raw.chunks.values().all(|(b, _)| b.len() == RAW_VOXELS));
                (key, raw.chunks.keys().copied().collect())
            })
            .collect();
        // 合成体素全实心 -> 没有空块，实体块集合应逐组等于 desired。
        assert_eq!(have, want, "组内块集合必须等于该 GroupKey 覆盖的 desired 块");
        assert!(!want.is_empty());
    }

    /// (b) 只改动一个组的成员集，只重建该组：其它组实体 id 不变，旧实体被 despawn。
    ///
    /// 直接篡改受害者组的成员快照模拟「该组内一块变化」，并把 `last_observer`
    /// 置成 NaN 绕过 early-out（观察者/配置本身不变，期望分区完全相同）。
    #[test]
    fn changing_one_group_membership_only_rebuilds_that_group() {
        let mut app = streaming_app(1, 1);
        set_stream_config(
            &mut app,
            VoxelLodConfig {
                max_lod: 1,
                thresholds_m: [1.0, 2.0, 3.0],
            },
        );
        set_stream_observer(&mut app, [0.0, 0.0, 0.0]);
        app.update();

        let before: BTreeMap<GroupKey, Entity> = app
            .world()
            .resource::<TerrainGroups>()
            .iter()
            .collect();
        assert!(
            before.len() >= 2,
            "测试需要至少两个组：{:?}",
            before.keys().collect::<Vec<_>>()
        );
        let victim = *before.keys().next().expect("非空");

        // 篡改受害者组的成员快照（模拟该组内一块变化），并强制重跑 diff。
        {
            let mut members = app.world_mut().resource_mut::<TerrainGroupMembers>();
            let snapshot = members.0.get_mut(&victim).expect("受害者组必须在快照里");
            snapshot.blocks.insert(chunk_key(12_345, 0, 0));
        }
        app.world_mut()
            .resource_mut::<TerrainStreamState>()
            .last_observer = [f32::NAN, 0.0, 0.0];

        app.update();

        let groups = app.world().resource::<TerrainGroups>();
        let rebuilt = groups.get(victim).expect("受害者组必须重建");
        assert_ne!(rebuilt, before[&victim], "受害者组必须换新实体");
        assert!(
            app.world().get_entity(before[&victim]).is_err(),
            "受害者组旧实体必须 despawn"
        );
        for (key, old_entity) in &before {
            if *key == victim {
                continue;
            }
            assert_eq!(
                groups.get(*key),
                Some(*old_entity),
                "未变化组 {key:?} 的实体 id 不得改变"
            );
        }
        // 篡改只在快照层：重建后组内容回到 desired，不含伪块。
        let raw = app
            .world()
            .entity(rebuilt)
            .get::<VoxChunkGroupRaw>()
            .expect("VoxChunkGroupRaw");
        assert!(
            !raw.chunks.contains_key(&chunk_key(12_345, 0, 0)),
            "重建必须回到 desired 成员"
        );
    }

    /// (c) 邻块 LOD 变化、但本组**成员集合不变**时也必须重建：
    /// 本块 halo 采样与 skirt_mask 依赖 6 面邻居的 LOD，只比成员集合会残留旧裙边。
    #[test]
    fn neighbour_lod_change_rebuilds_group_without_membership_change() {
        let member = chunk_key(1, 0, 0);
        let members: BTreeSet<ChunkKey> = [member].into_iter().collect();

        // 邻居 (+x) 更粗：本块 +x 面 = Coarser。
        let coarser = [
            TerrainBlock {
                origin: member,
                lod: 0,
            },
            TerrainBlock {
                origin: chunk_key(2, 0, 0),
                lod: 1,
            },
        ];
        let sig_coarser = group_neighbour_signature(&members, 0, &TerrainPartition::new(&coarser));

        // 邻居换成同 LOD：本块 +x 面 = Same，成员集合完全不变。
        let same = [
            TerrainBlock {
                origin: member,
                lod: 0,
            },
            TerrainBlock {
                origin: chunk_key(2, 0, 0),
                lod: 0,
            },
        ];
        let sig_same = group_neighbour_signature(&members, 0, &TerrainPartition::new(&same));

        assert!(matches!(
            sig_coarser[&member][0],
            FaceNeighbor::Coarser { .. }
        ));
        assert!(matches!(sig_same[&member][0], FaceNeighbor::Same(_)));
        assert_ne!(sig_coarser, sig_same, "邻块 LOD 变化必须改变组重建签名");
    }
}
