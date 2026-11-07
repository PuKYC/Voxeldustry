//! 流式 / 驻留诊断快照：把 Bevy 侧资源抄成纯数据，供 FFI 适配层读取。
//!
//! 为什么需要快照：Godot 与 Bevy 不在同一线程，Godot 不能读 World。
//! 本结构由 game-engine 的诊断采集系统在每帧末尾（`Last` 调度）从 World 里
//! 抄一份，写进 `ClientBridge` 的共享槽；Godot 主线程随时读取，
//! 读到的永远是「上一帧结束时的状态」。
//!
//! 采集只是拷贝，不参与任何玩法逻辑；字段都是 `Option`，用于区分
//! 「没装配流式层」与「恰好为 0」——后者是合法值。

use bevy::prelude::World;
use game_engine::spec::GameDiagnostics;
use game_engine::voxel::VoxelDirtySet;

// 面向 FFI 的引擎驻留类型重导出：godot-client-ext 只依赖 game-core，
// 需要经 game_core::diagnostics::* 取到这些机制类型。
pub use game_engine::residency::{MemoryPressure, ResidencyStats};

use crate::voxel::StreamStats;
use crate::world::streaming::WorldLoadReport;

/// 流式世界的一帧诊断快照。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StreamingDiagnostics {
    /// 体素流式统计（由 `VoxelStreamPlugin` 维护）。
    pub stream: Option<StreamStats>,
    /// 驻留统计（由 `ResidencyPlugin` 维护）。
    pub residency: Option<ResidencyStats>,
    /// 体素内存压力（由 `VoxelStreamPlugin` 每 tick 覆盖写）。
    pub memory: Option<MemoryPressure>,
    /// 启动引导的读档结果（由 `StreamingWorldPlugin` 的 bootstrap 写）。
    ///
    /// 重进世界要看这里：`load_world` 直接把存档块装进容器，那些块不算流的
    /// store 命中，所以 `stream.loaded_from_store` 在刚读档启动时恒为 0。
    pub world: Option<WorldLoadReport>,
    /// 已编辑块数（`VoxelDirtySet::edited_len`）：卸载时「要不要把内容写进存档」
    /// 的唯一判据。测试用它确认编辑真的落进了体素容器，再断言存档行为。
    pub edited_chunks: Option<usize>,
}

impl GameDiagnostics for StreamingDiagnostics {
    fn capture(world: &mut World) -> Self {
        Self {
            stream: world.get_resource::<StreamStats>().cloned(),
            residency: world.get_resource::<ResidencyStats>().copied(),
            memory: world.get_resource::<MemoryPressure>().copied(),
            world: world.get_resource::<WorldLoadReport>().copied(),
            edited_chunks: world
                .get_resource::<VoxelDirtySet>()
                .map(VoxelDirtySet::edited_len),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::prelude::*;

    /// game-core 的客户端桥特化：诊断类型必须与 `CoreSpec` 一致。
    type Bridge = crate::spec::ClientBridge;

    #[test]
    fn bridge_starts_with_empty_snapshot() {
        let bridge = Bridge::new(1);
        let snapshot = bridge.diagnostics();
        assert_eq!(snapshot, StreamingDiagnostics::default());
        assert!(snapshot.stream.is_none());
        assert!(snapshot.residency.is_none());
        assert!(snapshot.memory.is_none());
    }

    #[test]
    fn install_diagnostics_copy_stream_stats_out_of_the_world() {
        let mut app = App::new();
        app.insert_resource(StreamStats {
            queued: 3,
            pending: 2,
            loaded_from_store: 7,
            generated: 11,
            proposed: 5,
            unloaded: 4,
            pending_unloads: 1,
            render_groups: 6,
            staged: 8,
        });
        app.insert_resource(ResidencyStats {
            render_chunks: 9,
            ..ResidencyStats::default()
        });

        let snapshot = StreamingDiagnostics::capture(app.world_mut());
        let stream = snapshot.stream.expect("StreamStats 已插入，应采集到");
        assert_eq!(stream.queued, 3);
        assert_eq!(stream.generated, 11);
        assert_eq!(stream.render_groups, 6);
        let residency = snapshot.residency.expect("ResidencyStats 已插入，应采集到");
        assert_eq!(residency.render_chunks, 9);
        // 没插 MemoryPressure 时必须是 None，而不是伪造的全 0。
        assert!(snapshot.memory.is_none());
    }
}
