//! 存储层编译期常量。
//!
//! 这些值决定实例位宽与 occupancy scratch 的展开，**不做运行期可配**。
//! 硬边界：本层只认识整数体素坐标。

/// 基础子块（chunk）的边长指数：`32³`。
///
/// `voxels_per_axis = 1 << CHUNK_DEPTH = 32`。
pub const CHUNK_DEPTH: u8 = 5;

/// 单个轴的最大体素数（基础子块）：`1 << CHUNK_DEPTH == 32`。
pub const MAX_VOXELS_PER_AXIS: u32 = 1 << CHUNK_DEPTH;

/// 允许的最大深度上限；**可用深度 = 本值 - 1**。
///
/// 由上游的 7 提升到 9（可用 depth 8），以支持 base 32³ + LOD 3
/// `MaxDepth::new(d)` 断言 `d < MAX_ALLOWED_DEPTH`。
pub const MAX_ALLOWED_DEPTH: u8 = 9;

/// 一个分支节点的最大子节点数（八叉树 → 8）。
pub const MAX_CHILDREN: usize = 8;

/// 分支节点类型标签（用于结构化哈希的域分隔）。
pub const NODE_TYPE_BRANCH: u8 = 0;

/// 叶节点类型标签（用于结构化哈希的域分隔）。
pub const NODE_TYPE_LEAF: u8 = 1;

/// patterns 表中的分支桶下标。
pub const PATTERNS_TYPE_BRANCH: usize = 0;

/// patterns 表中的叶桶下标。
pub const PATTERNS_TYPE_LEAF: usize = 1;

/// 空子节点标记。children 索引列中的 `0` 同时是空分支哨兵的下标。
pub const CHILD_ABSENT: u8 = 0;

/// 供公开 API 使用的“无子节点”数组。
pub const EMPTY_CHILD: [super::BlockId; MAX_CHILDREN] = [super::BlockId::EMPTY; MAX_CHILDREN];

/// `dec_ref_recursive` 复用缓冲区的初始容量（元素个数，非字节）。
///
/// 释放路径按需增长，绝不越界。
pub const PREALLOCATED_STACK_SIZE: usize = 32768;
