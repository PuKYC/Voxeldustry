//! 体素层插件（由 game-core 在 WorldPlugin 中注册）。

use bevy::prelude::*;

use super::dirty::VoxelDirtySet;
use super::external::ExternalMaskCache;
use super::incremental::IncrementalMeshCache;
use super::interner::{update_voxel_memory_metric, VoxelInterner, VoxelMemoryMetric};
use super::wrapped::WrappedBlockCache;

/// 注册体素引擎侧资源与指标系统。
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
            .init_resource::<WrappedBlockCache>()
            .init_resource::<ExternalMaskCache>()
            .init_resource::<IncrementalMeshCache>()
            .init_resource::<VoxelMemoryMetric>()
            .add_systems(Update, update_voxel_memory_metric);
    }
}
