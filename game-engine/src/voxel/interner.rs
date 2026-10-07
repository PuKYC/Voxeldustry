//! voxel::store::VoxInterner<u8> 的 Bevy Resource 包装与内存指标。

use bevy::prelude::*;
use voxel::store::VoxInterner;

/// 默认 interner 内存预算（16 MiB）。活动岛预算由 game-core 在启动时提供。
pub const DEFAULT_INTERNER_BUDGET_BYTES: usize = 16 * 1024 * 1024;

/// 全局共享的体素节点池（Bevy Resource）。
///
/// PoolAllocatorLite 无条件 unsafe impl Send/Sync，因此该包装
/// 可直接作 Resource。
#[derive(Resource)]
pub struct VoxelInterner {
    inner: VoxInterner<u8>,
    budget_bytes: usize,
}

impl VoxelInterner {
    /// 按内存预算（字节）预分配节点池。
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            inner: VoxInterner::with_memory_budget(budget_bytes),
            budget_bytes,
        }
    }

    /// new 的别名，语义更明确。
    pub fn from_budget(budget_bytes: usize) -> Self {
        Self::new(budget_bytes)
    }

    pub fn inner(&self) -> &VoxInterner<u8> {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut VoxInterner<u8> {
        &mut self.inner
    }

    /// 启动预算（字节）。
    pub fn budget_bytes(&self) -> usize {
        self.budget_bytes
    }

    /// 单节点 pool 大小（只作单元护栏，不作验收口径）。
    pub fn node_size() -> usize {
        VoxInterner::<u8>::node_size()
    }

    /// 当前容量（节点数）。
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// 实际总内存（pools + patterns + free_indices， 验收口径）。
    pub fn estimated_total_bytes(&self) -> usize {
        self.inner.estimated_total_bytes()
    }

    /// 预留额外 additional_nodes 个节点容量。
    pub fn reserve(&mut self, additional_nodes: usize) -> bool {
        self.inner.reserve(additional_nodes)
    }

    /// 容量用满时倍增（索引稳定，无需 BlockId 重映射）。
    /// 返回是否发生增长。
    pub fn grow(&mut self) -> bool {
        self.inner.grow()
    }
}

impl Default for VoxelInterner {
    fn default() -> Self {
        Self::new(DEFAULT_INTERNER_BUDGET_BYTES)
    }
}

impl std::fmt::Debug for VoxelInterner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoxelInterner")
            .field("budget_bytes", &self.budget_bytes)
            .field("capacity", &self.capacity())
            .field("estimated_total_bytes", &self.estimated_total_bytes())
            .finish()
    }
}

/// estimated_total_bytes() 透出的 Bevy 指标资源。
#[derive(Resource, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoxelMemoryMetric {
    pub estimated_total_bytes: usize,
    pub capacity: usize,
    pub node_size: usize,
}

/// 每帧把 interner 的内存指标写入 VoxelMemoryMetric。
pub fn update_voxel_memory_metric(
    interner: Res<VoxelInterner>,
    mut metric: ResMut<VoxelMemoryMetric>,
) {
    metric.estimated_total_bytes = interner.estimated_total_bytes();
    metric.capacity = interner.capacity();
    metric.node_size = VoxelInterner::node_size();
}

// Resource 要求 Send + Sync；编译期断言。
#[allow(dead_code)]
fn _assert_voxel_interner_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<VoxelInterner>();
}
