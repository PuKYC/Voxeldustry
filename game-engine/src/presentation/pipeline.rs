//! 表现层的收集、基线、可见性刷新与发布。
//!
//! 调度（`Update`，显式链式顺序；插值状态在 `FixedPostUpdate` 按 tick 推进）：
//!
//! ```text
//! RefreshVisibilitySet   ── 刷新本地玩家 / 视可见集 / 渲染时钟
//!        ↓
//! CollectPresentationSet ── 每个注册过的组件类型一个泛型系统
//!        ↓
//! FinalizePresentationSet ── Attach / Remove / Detach / Despawn → 整理 → 发布
//! ```

use bevy::prelude::*;
use std::collections::{HashMap, HashSet};

use crate::aoi::AoIManager;
use crate::identity::StableEntityId;
use crate::input::LocalPlayer;
use crate::perception::PerceptionMask;

use super::command::PresentationCommand;
use super::payload::PresentationPayload;

/// 刷新可见性。
#[derive(SystemSet, Clone, Debug, PartialEq, Eq, Hash)]
pub struct RefreshVisibilitySet;

/// 收集组件增量。
#[derive(SystemSet, Clone, Debug, PartialEq, Eq, Hash)]
pub struct CollectPresentationSet;

/// 收尾：Attach / Remove / Detach / Despawn / 整理 / 发布。
#[derive(SystemSet, Clone, Debug, PartialEq, Eq, Hash)]
pub struct FinalizePresentationSet;

/// 表现管线运行时状态（诊断 + 帧号 + 会话号）。
#[derive(Resource, Debug)]
pub struct PresentationRuntime {
    /// 复用 `session`，让 Godot 丢弃重启前的残留帧。
    pub session: u64,
    pub seq: u64,
    pub tick: u32,
    pub render_clock_ms: u64,
    pub frames_published: u64,
    /// 发布时覆盖掉了尚未被 Godot 取走的帧的次数（Godot 掉帧诊断）。
    pub frames_overwritten: u64,
    pub last_commands: usize,
}

impl Default for PresentationRuntime {
    fn default() -> Self {
        Self {
            session: 0,
            seq: 0,
            tick: 0,
            render_clock_ms: 0,
            frames_published: 0,
            frames_overwritten: 0,
            last_commands: 0,
        }
    }
}

impl PresentationRuntime {
    pub fn with_session(session: u64) -> Self {
        Self {
            session,
            ..Self::default()
        }
    }
}

/// 同步基线：区分「首次同步」与「增量同步」，并记录当前已同步的实体集合。
///
/// - `entities`：已 `Attach` 过的 `StableEntityId → Entity`
/// - `components`：已下发过的 `(StableEntityId, PayloadKind)`
#[derive(Resource)]
pub struct SyncBaseline<P: PresentationPayload> {
    pub entities: HashMap<u64, Entity>,
    pub components: HashSet<(u64, P::Kind)>,
}

impl<P: PresentationPayload> Default for SyncBaseline<P> {
    fn default() -> Self {
        Self {
            entities: HashMap::new(),
            components: HashSet::new(),
        }
    }
}

impl<P: PresentationPayload> SyncBaseline<P> {
    pub fn has_component(&self, id: StableEntityId, kind: P::Kind) -> bool {
        self.components.contains(&(id.0, kind))
    }

    pub fn entity_of(&self, id: StableEntityId) -> Option<Entity> {
        self.entities.get(&id.0).copied()
    }

    pub fn synced_entity_count(&self) -> usize {
        self.entities.len()
    }
}

/// 本帧累积的表现命令（每个 `collect_component::<T>` 往里推）。
#[derive(Resource)]
pub struct PendingPresentation<P: PresentationPayload> {
    pub commands: Vec<PresentationCommand<P>>,
}

impl<P: PresentationPayload> Default for PendingPresentation<P> {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
        }
    }
}

/// 本帧「对本地玩家可见」的实体集合与本地感知掩码。
#[derive(Resource, Debug)]
pub struct PresentationVisibility {
    /// 本地玩家的感知掩码；取不到时是 `EMPTY`（Fail-Closed）。
    pub local_perception: PerceptionMask,
    /// 本地玩家实体（也是 AOI 观察者）。
    pub observer: Option<Entity>,
    /// 可见实体列表（有 AOI 时来自 `AoIManager`，否则退化为全部有 `StableEntityId` 的实体）。
    pub entities: Vec<Entity>,
    /// 可见实体的稳定 ID 集合（快速成员判断）。
    pub ids: HashSet<u64>,
}

impl Default for PresentationVisibility {
    fn default() -> Self {
        Self {
            local_perception: PerceptionMask::EMPTY,
            observer: None,
            entities: Vec::new(),
            ids: HashSet::new(),
        }
    }
}

impl PresentationVisibility {
    pub fn is_visible(&self, id: StableEntityId) -> bool {
        self.ids.contains(&id.0)
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

/// 刷新「本地玩家 + 视可见集 + 渲染时钟」。
///
/// 本地玩家通过 `LocalPlayer` **标记组件**定位，而不是从 `AoIManager`
/// 内部状态里反查 —— 否则「谁是本地玩家」和「谁是 AOI 观察者」会被绑死。
#[allow(clippy::too_many_arguments)]
pub fn refresh_visibility(
    mut visibility: ResMut<PresentationVisibility>,
    aoi: Option<Res<AoIManager>>,
    locals: Query<(Entity, Option<&PerceptionMask>), With<LocalPlayer>>,
    ids: Query<(Entity, &StableEntityId)>,
) {
    let mut observer = None;
    let mut perception = PerceptionMask::EMPTY;
    if let Some((entity, mask)) = locals.iter().next() {
        observer = Some(entity);
        if let Some(mask) = mask {
            perception = *mask;
        }
    }
    visibility.observer = observer;
    visibility.local_perception = perception;

    let mut entities: Vec<Entity> = Vec::new();
    let mut used_aoi = false;

    if let (Some(observer), Some(aoi)) = (observer, aoi.as_deref()) {
        if let Some(targets) = aoi.get_visible_targets(observer) {
            entities.extend_from_slice(targets);
            used_aoi = true;
        }
    }

    if !used_aoi {
        // 退化路径：没有 AOI 观察者时，所有带 `StableEntityId` 的实体都算可见。
        // 这样「还没接 AOI」的场景也能直接跑通表现管线。
        entities.extend(ids.iter().map(|(entity, _)| entity));
    }

    visibility.ids.clear();
    for entity in &entities {
        if let Ok((_, id)) = ids.get(*entity) {
            visibility.ids.insert(id.0);
        }
    }
    visibility.entities = entities;
}
