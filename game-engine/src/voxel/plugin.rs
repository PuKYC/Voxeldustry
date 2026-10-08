//! 体素世界层插件（由 game-core 在 GameVoxelPlugin 中注册）。
//!
//! 注册世界层资源与变更系统：VoxelInterner（缺失则按 budget 插入）、
//! VoxelMemoryMetric 指标、VoxelDirtySet 脏集，以及 apply_voxel_changes 的
//! Apply/Mesh 阶段链。表现层的网格机制（pack / plan / 各种缓存）是被 core
//! 直接调用的纯函数库，其缓存资源由 core 侧按需 init。

use bevy::prelude::*;

use super::change::{apply_voxel_changes, VoxelSet};
use super::dirty::VoxelDirtySet;
use super::interner::{update_voxel_memory_metric, VoxelInterner, VoxelMemoryMetric};

/// 注册体素世界层资源、指标与变更系统。
///
/// budget_bytes 只在 VoxelInterner 资源尚未存在时使用；game-core 可先自行
/// insert_resource(VoxelInterner::new(real_budget)) 再注册本插件。
pub struct VoxelPlugin {
    pub budget_bytes: usize,
}

impl VoxelPlugin {
    pub fn new(budget_bytes: usize) -> Self {
        Self { budget_bytes }
    }
}

impl Default for VoxelPlugin {
    fn default() -> Self {
        Self::new(super::interner::DEFAULT_INTERNER_BUDGET_BYTES)
    }
}

impl Plugin for VoxelPlugin {
    fn build(&self, app: &mut App) {
        if app.world().get_resource::<VoxelInterner>().is_none() {
            app.insert_resource(VoxelInterner::new(self.budget_bytes));
        }
        app.init_resource::<VoxelDirtySet>()
            .init_resource::<VoxelMemoryMetric>()
            .add_systems(Update, update_voxel_memory_metric);
        app.add_systems(FixedUpdate, apply_voxel_changes.in_set(VoxelSet::Apply))
            .configure_sets(FixedUpdate, (VoxelSet::Apply, VoxelSet::Mesh).chain());
    }
}
