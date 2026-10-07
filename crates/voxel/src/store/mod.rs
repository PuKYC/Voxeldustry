//! 存储层：`BlockId` / `ChunkKey` / `VoxTree` / `VoxInterner`（WP1）。
//!
//! 设计边界：**只用整数体素坐标**，不依赖 Bevy，不认识 `voxel_size`，
//! 不碰 `Transform`。本模块是 `crate::mesh` 与 `game-engine` 的唯一存储入口。
//!
//! 目录来源：`vendor/voxelis` 的 `core/` + `spatial/`，按「复用 / 重写」清单改造：
//!
//! - **复用（最小改动）**：`block_id` / `lod` / `max_depth` / `traversal_depth` /
//!   `voxel` / `batch` / `voxtree` / `voxops` / `aabb2d` / `common`。
//! - **重写**：`interner`（SoA + `child_mask` + u32 子索引 + u32 结构化哈希 +
//!   碰撞 bucket）、`memory`（增长路径）。
//!
//! 关键不变量：
//! 1. `MAX_ALLOWED_DEPTH = 9`，可用 depth 8。
//! 2. 节点池倍增时**索引稳定**，因此 `BlockId` / patterns 无需重映射。
//! 3. patterns 只按 hash 查表并**结构校验**，绝不迭代它产生字节或哈希。
//! 4. `content_hash` 是结构哈希，与 interner 节点索引无关。

pub mod aabb2d;
pub mod batch;
pub mod block_id;
pub mod chunk_key;
pub mod common;
pub mod consts;
pub mod interner;
pub mod lod;
pub mod max_depth;
pub mod memory;
pub mod traversal_depth;
pub mod voxel;
pub mod voxops;
pub mod voxtree;

pub use aabb2d::Aabb2d;
pub use batch::Batch;
pub use block_id::BlockId;
pub use chunk_key::ChunkKey;
pub use common::{
    child_index, child_index2, dump_root, dump_statistics, dump_structure, encode_child_index_path,
    get_at_depth, to_vec,
};
pub use consts::{
    CHILD_ABSENT, CHUNK_DEPTH, EMPTY_CHILD, MAX_ALLOWED_DEPTH, MAX_CHILDREN, MAX_VOXELS_PER_AXIS,
    NODE_TYPE_BRANCH, NODE_TYPE_LEAF, PATTERNS_TYPE_BRANCH, PATTERNS_TYPE_LEAF,
    PREALLOCATED_STACK_SIZE,
};
#[cfg(feature = "memory_stats")]
pub use interner::InternerStats;
pub use interner::{Bucket, Children, PatternsHashmap, VoxInterner};
pub use lod::Lod;
pub use max_depth::MaxDepth;
pub use memory::PoolAllocatorLite;
pub use traversal_depth::TraversalDepth;
pub use voxel::{ByteConversion, VoxelTrait};
pub use voxops::{
    VoxOps, VoxOpsBatch, VoxOpsBulkWrite, VoxOpsConfig, VoxOpsConvertPositions, VoxOpsDirty,
    VoxOpsRead, VoxOpsState, VoxOpsWrite,
};
pub use voxtree::VoxTree;
