//! 树操作 trait 集（移植自 vendored spatial/voxops.rs，裁掉 mesh / chunk 依赖）。
//!
//! 硬边界：这里只出现整数坐标；f32 世界坐标 / VoxChunk / MeshData
//! 相关的 trait 全部不移植。

use glam::{IVec3, UVec3};

use crate::store::{Batch, Lod, MaxDepth, VoxInterner, VoxelTrait};

/// 读取单个体素。
pub trait VoxOpsRead<T: VoxelTrait> {
    /// 取得 position 处的体素；默认值 / 缺失返回 None。
    fn get(&self, interner: &VoxInterner<T>, position: IVec3) -> Option<T>;
}

/// 写入单个体素。
pub trait VoxOpsWrite<T: VoxelTrait> {
    /// 设置 position 处的体素；返回结构是否发生变化。
    fn set(&mut self, interner: &mut VoxInterner<T>, position: IVec3, voxel: T) -> bool;
}

/// 批量填充 / 清空。
pub trait VoxOpsBulkWrite<T: VoxelTrait> {
    /// 用同一个值填充整棵树（塌陷为单个叶节点）。
    fn fill(&mut self, interner: &mut VoxInterner<T>, value: T);

    /// 清空整棵树。
    fn clear(&mut self, interner: &mut VoxInterner<T>);
}

/// Batch 相关操作。
pub trait VoxOpsBatch<T: VoxelTrait> {
    /// 为这棵树创建一个 Batch。
    fn create_batch(&self) -> Batch<T>;

    /// 应用一个 Batch；返回结构是否发生变化。
    fn apply_batch(&mut self, interner: &mut VoxInterner<T>, batch: &Batch<T>) -> bool;
}

/// 配置查询（max_depth / voxels_per_axis）。
pub trait VoxOpsConfig {
    /// 返回指定 LOD 下的最大深度。
    fn max_depth(&self, lod: Lod) -> MaxDepth;

    /// 返回指定 LOD 下每个轴的体素数。
    fn voxels_per_axis(&self, lod: Lod) -> u32;
}

/// 状态查询。
pub trait VoxOpsState {
    /// 树是否为空。
    fn is_empty(&self) -> bool;

    /// 根是否为叶节点。
    fn is_leaf(&self) -> bool;
}

/// 脏标记。
pub trait VoxOpsDirty {
    /// 是否被标记为脏。
    fn is_dirty(&self) -> bool;

    /// 标记为脏。
    fn mark_dirty(&mut self);

    /// 清除脏标记。
    fn clear_dirty(&mut self);
}

/// 体素树的完整操作集。
pub trait VoxOps<T: VoxelTrait>:
    VoxOpsRead<T> + VoxOpsWrite<T> + VoxOpsConfig + VoxOpsState + VoxOpsDirty
{
}

impl<T: VoxelTrait, U> VoxOps<T> for U where
    U: VoxOpsRead<T> + VoxOpsWrite<T> + VoxOpsConfig + VoxOpsState + VoxOpsDirty
{
}

/// 局部 / 世界整数坐标换算（无 f32）。
pub trait VoxOpsConvertPositions {
    /// 局部坐标转世界坐标。
    fn local_to_world(&self, position: UVec3) -> IVec3;

    /// 世界坐标转局部坐标。
    fn world_to_local(&self, position: IVec3) -> UVec3;
}
