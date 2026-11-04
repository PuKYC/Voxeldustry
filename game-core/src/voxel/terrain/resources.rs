//! terrain 的 ECS 资源与块缓存借用包。
//!
//! 只放「状态」：分组索引 [TerrainGroups]、组级成员快照 [TerrainGroupMembers]、
//! 四份块缓存资源、统计与观察者 / FFI 输入。行为（生成、提取、组重建）分别在
//! [`super::systems`]、[`super::extract`]、[`super::group_diff`] 里。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use bevy::prelude::*;
use game_engine::presentation::voxel::WrappedBlockCache;
use game_engine::voxel::{ChunkKey, VoxelInterner};

use crate::voxel::lod::VoxelLodConfig;

use super::blocks::{FaceNeighbor, GroupKey};
use super::generated::GeneratedBlockCache;

/// 已产出的 mesh 分组实体索引：`GroupKey -> Entity`（BTreeMap 键升序 = 确定性）。
///
/// 一个 [GroupKey] 对应一个分组实体（[VoxChunkGroupRaw](crate::presentation::voxel_mesh::VoxChunkGroupRaw)），
/// 组内所有块同 LOD。F2 起 terrain producer 以**组**为单位 diff / spawn / despawn；
/// stable id 也逐组分配。
#[derive(Resource, Default, Debug)]
pub struct TerrainGroups(pub(crate) BTreeMap<GroupKey, Entity>);

impl TerrainGroups {
    /// 当前分组实体数（一个 [GroupKey] 一个实体）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// 是否还没有产出任何分组实体。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// 取某个分组键对应的实体。
    #[must_use]
    pub fn get(&self, key: GroupKey) -> Option<Entity> {
        self.0.get(&key).copied()
    }

    /// 按键升序遍历 `(GroupKey, Entity)`（确定性顺序）。
    pub fn iter(&self) -> impl Iterator<Item = (GroupKey, Entity)> + '_ {
        self.0.iter().map(|(key, entity)| (*key, *entity))
    }
}

/// 组级 diff 的成员快照：组上次构建时覆盖的期望块（**含空块**）与非空块数。
///
/// "未变化的组完全不动" 的判据就是这个集合：组内期望成员集合不变就不 re-extract、
/// 不 spawn。必须记录含空块的成员，否则全空组每次重算分区都会被当成新组重建。
#[derive(Resource, Default, Debug)]
pub(crate) struct TerrainGroupMembers(pub(crate) BTreeMap<GroupKey, GroupMembers>);

/// 一个组上次构建时的成员统计（块级口径）。
#[derive(Clone, Debug, Default)]
pub(crate) struct GroupMembers {
    /// 该组覆盖的 desired 块（块原点；组内同 LOD）。
    pub(crate) blocks: BTreeSet<ChunkKey>,
    /// 其中产出几何（非空）的块数。
    pub(crate) solid: usize,
    /// 上次构建时每个成员块的「同 LOD 面邻居报告 + 跨 LOD 面掩码」。
    ///
    /// 成员集合不变但某邻块 LOD 变化时，本块的同 LOD 邻居集合与 `skirt_mask`
    /// 会变，必须据此一并触发重建（否则跨 LOD 裙边会在 LOD 环收放后残留）。
    pub(crate) neighbours: BTreeMap<ChunkKey, ([FaceNeighbor; 6], u8)>,
}

/// 邻块包装缓存：`extract_raw_halo` 的 26 邻只读借用与它自己的
/// `&mut WrappedBlockCache` 不能来自同一个变量（借用冲突），所以邻块单独一份。
#[derive(Resource, Default)]
pub struct TerrainNeighborBlocks(pub WrappedBlockCache);

/// 按 LOD 直接生成的块树缓存（LOD>0）。
///
/// `own` 放「正在网格化的块」，`neighbors` 放它预热过的 26 邻：两者必须分开，
/// 否则本块的 `&mut` 与邻块的 `&` 会撞在同一份缓存上。两份缓存共享 interner
/// 节点（hash-cons），所以重复生成同一块不会翻倍内存。
#[derive(Resource, Default)]
pub struct TerrainGeneratedBlocks {
    /// 本块（组内逐块）按 LOD 生成的树。
    pub own: GeneratedBlockCache,
    /// 26 邻按 LOD 生成的树。
    pub neighbors: GeneratedBlockCache,
}

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
    /// 观察者世界坐标（米，`[x, y, z]`）。
    pub position: [f32; 3],
}

/// 跨线程（Godot -> Bevy）的 LOD 运行期输入。
///
/// None 表示不覆写：observer 覆写 [VoxelLodObserver]，config 覆写 [VoxelLodConfig]。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct VoxelLodRuntime {
    /// 观察者位置覆写（米，世界坐标）；None = 沿用 Rust 侧资源。
    pub observer: Option<[f32; 3]>,
    /// LOD 配置覆写；None = 沿用 Rust 侧资源。
    pub config: Option<VoxelLodConfig>,
}

/// FFI 句柄：Godot 主线程写，Bevy 后台线程读，内部 Arc<Mutex<..>>。
#[derive(Resource, Clone, Default)]
pub struct VoxelLodHandle(pub Arc<Mutex<VoxelLodRuntime>>);

/// 流式系统的上一次输入（early-out 用）。
#[derive(Resource, Default)]
pub(crate) struct TerrainStreamState {
    /// 上次推进时观察者的位置（米）。
    pub(crate) last_observer: [f32; 3],
    /// 上次推进时夹到合法域后的 LOD 配置。
    pub(crate) last_config: Option<VoxelLodConfig>,
}

/// 一次块提取 / 组重建需要**可变借用**的四份块缓存。
///
/// 四份实例缺一不可：`WrappedBlockCache`、[TerrainNeighborBlocks]、
/// [TerrainGeneratedBlocks::own]、[TerrainGeneratedBlocks::neighbors]
/// （见 [`super::cache::invalidate_chunk_caches`]）。收敛成一个借用包后，
/// [super::extract::extract_block_raw] 与 [super::group_diff::apply_group_diff]
/// 的参数个数才能落在 clippy 阈值（7）以内。
pub(crate) struct TerrainCaches<'a> {
    /// 节点 interner：树的生成 / 包装 / 失效都要它。
    pub(crate) interner: &'a mut VoxelInterner,
    /// 本块（组内逐块）的 LOD0 包装缓存。
    pub(crate) wrapped: &'a mut WrappedBlockCache,
    /// 按 LOD 直接生成的块树缓存（own + neighbors）。
    pub(crate) generated: &'a mut TerrainGeneratedBlocks,
    /// 26 邻的 LOD0 包装缓存。
    pub(crate) neighbors: &'a mut TerrainNeighborBlocks,
}
