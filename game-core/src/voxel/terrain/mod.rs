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
//!
//! ## 子模块
//!
//! - [blocks]：[TerrainBlock] / [TerrainPartition] / [GroupKey] 与原点、掩码等纯计算；
//! - [generation]：世界生成器与块树生成（[generation::WorldGenerator]）；
//! - generated、cache：按 LOD 的块树缓存与失效 / 卸载；
//! - extract：单块的 raw halo 提取；
//! - group_diff：组级 diff 与增量重建；
//! - resources：Terrain* 资源、LOD 句柄与块缓存借用包；
//! - plugin、systems：插件装配面与 Startup / Update 系统。

pub mod blocks;
mod cache;
mod extract;
pub mod generated;
pub mod generation;
mod group_diff;
mod plugin;
mod resources;
mod systems;

#[cfg(test)]
mod tests;

pub use blocks::{
    block_origin_meters, block_span, chunk_keys_to_generate, face_neighbors, group_key,
    group_origin, lod_blocks, lod_blocks_for_observer, lod_mismatch_mask, missing_face_mask,
    nearest_axis_distance_m, nearest_block_distance_m, neighbor_origins, skirt_mask, y_band_layers,
    FaceNeighbor, GroupKey, TerrainBlock, TerrainPartition, REGION_BLOCKS, TERRAIN_MARGIN_CHUNKS,
};
pub use cache::{release_terrain_body, unload_chunk};
pub use generated::GeneratedBlockCache;
pub use plugin::{TerrainConfig, TerrainMode, TerrainPlugin};
pub use resources::{
    TerrainGeneratedBlocks, TerrainGroups, TerrainNeighborBlocks, TerrainStats, VoxelLodHandle,
    VoxelLodObserver, VoxelLodRuntime,
};
pub use systems::voxel_size_meters;

pub(crate) use systems::attach_terrain_body;
