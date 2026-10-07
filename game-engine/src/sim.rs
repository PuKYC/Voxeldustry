//! 引擎调度阶段。
//!
//! EngineSet 是 mod 排序的稳定锚点：mod 只能相对这些命名集合排序。
//! 稳定性承诺：变体位置 / 名字是 mod API 的一部分，只增不改；
//! 新增阶段只能追加，不能重排既有集合的相对顺序。

use bevy::prelude::*;

/// 默认逻辑定步频率（Hz）。
pub const DEFAULT_FIXED_HZ: u32 = 60;

/// 逻辑 tick（FixedUpdate）与表现（Update）的稳定命名阶段。
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EngineSet {
    // ── 逻辑 tick（FixedUpdate）──
    /// 采集 / 折叠输入（元数据阶段；输入细链见 input::InputSet）。
    Input,
    /// 能力求解（game-core 在此插入）。
    Availability,
    /// AI / 决策。
    Decision,
    /// 位移。
    Motion,
    /// 结算。
    Combat,
    /// 判定（如死亡检查）。
    Reaction,
    /// 清理。
    Cleanup,
    // ── 表现（Update）──
    /// 可见性刷新。
    Refresh,
    /// 组件 -> 载荷。
    Collect,
    /// Attach / Remove / Detach / Despawn -> 发布。
    Finalize,
}

/// 只声明 EngineSet 的逻辑 tick 顺序，不注册任何系统。
///
/// 表现侧（Refresh / Collect / Finalize）由表现管线自行 configure，
/// 这里不重复声明以避免与既有 Update 集合冲突。
pub struct EngineSetPlugin;

impl Plugin for EngineSetPlugin {
    fn build(&self, app: &mut App) {
        app.configure_sets(
            FixedUpdate,
            (
                EngineSet::Input,
                EngineSet::Availability,
                EngineSet::Decision,
                EngineSet::Motion,
                EngineSet::Combat,
                EngineSet::Reaction,
                EngineSet::Cleanup,
            )
                .chain(),
        );
    }
}
