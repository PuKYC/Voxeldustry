//! `crates/voxel`：确定性 SVO-DAG 体素层。
//!
//! **边界铁律**：本 crate 只使用**整数体素坐标**，不依赖 Bevy，
//! 不知道 `voxel_size`，不碰 `Transform`。世界换算全部发生在 `game-engine`。
//! 源码源自 voxelis v25.4.0（MIT OR Apache-2.0）。
//!
//! 模块：
//! - `store`：`BlockId` / `ChunkKey` / `VoxTree` / `VoxInterner` / 增长路径 / 碰撞 bucket。
//! - `mesh`：greedy 合并 + occupancy + LOD 块包装 + 整数 `RectInstance` 提取。
//! - `lod_quadtree`：XZ 受限四叉树划分（2:1 平衡），供地形 LOD 选块使用。
//! - `io`：确定性存档（结构序遍历，禁用 map 迭代序）。

pub mod io;
pub mod lod_quadtree;
pub mod mesh;
pub mod store;

pub use lod_quadtree::{restricted_quadtree, QuadLeaf, MAX_QUAD_LOD};
pub use mesh::{extract_block, AoRectBatch, MeshBlock, RectBatch, RectInstance};
pub use store::{
    Aabb2d, Batch, BlockId, Bucket, ByteConversion, Children, ChunkKey, Lod, MaxDepth,
    PatternsHashmap, TraversalDepth, VoxInterner, VoxOps, VoxOpsBatch, VoxOpsBulkWrite,
    VoxOpsConfig, VoxOpsConvertPositions, VoxOpsDirty, VoxOpsRead, VoxOpsState, VoxOpsWrite,
    VoxTree, VoxelTrait, CHUNK_DEPTH, MAX_ALLOWED_DEPTH, MAX_CHILDREN, MAX_VOXELS_PER_AXIS,
};
