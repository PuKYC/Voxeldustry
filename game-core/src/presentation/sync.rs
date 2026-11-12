//! 收集、可见性降级与发布（game-core 业务胶水层）。
//!
//! 引擎提供 SyncBaseline / PendingPresentation / 单槽与命名阶段；
//! 本文件把具体组件（ToPresentation）与原型 / 感知规则接上。

use std::collections::HashMap;
use std::sync::Arc;

use bevy::ecs::system::SystemParam;
use bevy::prelude::*;
use game_engine::presentation::interp::RenderClockState;
use game_engine::presentation::pipeline::{
    PendingPresentation, PresentationRuntime, PresentationVisibility, SyncBaseline,
};

use crate::privacy::{memory_path_component_visible, CoreRequiredPerception};
use game_engine::identity::StableEntityId;

use game_engine::modding::ResolvedModPayloads;

use super::payload::{
    project, project_mod_presentation, CustomPayload, ExtensionBag, ExtensionPayload,
    ExtensionSchema, ModPresentation, PayloadKind, SyncPayload, ToPresentation,
};
use super::{compact_commands, PresentationCommand, PresentationFrame, PresentationSlot};

/// game-core 的同步基线特化。
pub type CoreSyncBaseline = SyncBaseline<SyncPayload>;
/// game-core 的待发布命令特化。
pub type CorePendingPresentation = PendingPresentation<SyncPayload>;

/// 单个组件类型的收集系统（每个 present::<T>() 注册一个实例）。
///
/// 只遍历本帧可见实体，不全世界扫 archetype。
pub fn collect_component<T: ToPresentation>(
    visibility: Res<PresentationVisibility>,
    all: Query<(&StableEntityId, &T)>,
    changed: Query<Entity, Changed<T>>,
    required: Query<&CoreRequiredPerception>,
    mut pending: ResMut<CorePendingPresentation>,
    mut baseline: ResMut<CoreSyncBaseline>,
) {
    for &entity in visibility.entities.iter() {
        let Ok((id, component)) = all.get(entity) else {
            continue;
        };

        // 隐私组件：与网络路径共用同一份真值。
        if T::KIND.is_perception_gated()
            && !memory_path_component_visible(
                visibility.local_perception,
                required.get(entity).ok(),
            )
        {
            continue;
        }

        let is_first = !baseline.has_component(*id, T::KIND);
        if !is_first && !changed.contains(entity) {
            continue;
        }

        let payload = component.to_presentation();
        if is_first {
            baseline.components.insert((id.0, T::KIND));
            pending
                .commands
                .push(PresentationCommand::Add { id: *id, payload });
        } else {
            pending
                .commands
                .push(PresentationCommand::Update { id: *id, payload });
        }
    }
}

/// 表现层同步的写侧资源包：待发布命令与已下发基线。
///
/// collector 与 finalize 都要写这两份资源；折成一个 [SystemParam] 后，
/// 两个系统的参数个数都不再逼近 clippy 阈值。
#[derive(SystemParam)]
pub struct SyncSinks<'w> {
    /// 待发布命令缓冲。
    pending: ResMut<'w, CorePendingPresentation>,
    /// 已下发基线（实体映射 + 组件集合）。
    baseline: ResMut<'w, CoreSyncBaseline>,
}

/// 扩展袋收集的读取侧：可见性、schema、袋 / 稳定 ID 查询与移除事件。
#[derive(SystemParam)]
pub struct ExtensionBagSources<'w, 's> {
    /// 本帧可见实体与观察者感知掩码。
    visibility: Res<'w, PresentationVisibility>,
    /// 扩展字段 schema（Required 位由它自带）。
    schema: Res<'w, ExtensionSchema>,
    /// 带扩展袋的实体。
    all: Query<'w, 's, (&'static StableEntityId, &'static ExtensionBag)>,
    /// 稳定 ID 反查。
    ids: Query<'w, 's, &'static StableEntityId>,
    /// 本帧被移除的扩展袋组件。
    removed_bags: RemovedComponents<'w, 's, ExtensionBag>,
}

/// 扩展袋收集系统。
///
/// **不依赖 `Changed`**：Bevy 里只要可变借用就会标记 Changed（同值写也会被误判），
/// 因此这里对每个可见实体重算投影，用稳定哈希与“上次下发”比较来去抖——
/// 一次性覆盖「无变化」「同值写入」「只改观察者感知」三类需求。
///
/// 空袋 / 投影为空：主动发 `Remove` 并清基线（finalize 对非门控 kind 不发 Remove）。
pub fn collect_extension_bag(
    mut last_sent: Local<HashMap<u64, ExtensionPayload>>,
    mut src: ExtensionBagSources,
    mut out: SyncSinks,
) {
    // 1) 袋组件被移除：实体仍在且未在同帧重新挂回 -> Remove + 清基线。
    for entity in src.removed_bags.read() {
        if src.all.get(entity).is_ok() {
            continue; // 同帧先删后加，视为仍在
        }
        let Ok(id) = src.ids.get(entity) else {
            continue; // 实体已销毁，交给 finalize 的 Despawn 路径
        };
        last_sent.remove(&id.0);
        remove_payload_baseline(
            &mut out.baseline,
            &mut out.pending,
            id.0,
            PayloadKind::Extension,
        );
    }

    // 2) 可见实体：投影 -> 哈希去抖；空投影 -> Remove。
    for &entity in src.visibility.entities.iter() {
        let Ok((id, bag)) = src.all.get(entity) else {
            continue;
        };
        let in_baseline = out.baseline.has_component(*id, PayloadKind::Extension);
        // Required 字段的可见性由 schema 自带 required_bits 决定（观察者掩码单参），
        // 不再读取实体级 CoreRequiredPerception，杜绝「缺载体 -> Fail-Open」。
        let payload = project(bag, src.visibility.local_perception, &src.schema);

        if payload.fields().is_empty() {
            if in_baseline {
                last_sent.remove(&id.0);
                remove_payload_baseline(
                    &mut out.baseline,
                    &mut out.pending,
                    id.0,
                    PayloadKind::Extension,
                );
            }
            continue;
        }

        // 基线缺失 = 首次进入（或 detach 后重进）-> 必须 Add，不能因哈希相同而跳过。
        if !in_baseline {
            last_sent.insert(id.0, payload.clone());
            out.baseline
                .components
                .insert((id.0, PayloadKind::Extension));
            out.pending.commands.push(PresentationCommand::Add {
                id: *id,
                payload: SyncPayload::Extension(payload),
            });
            continue;
        }

        if last_sent.get(&id.0) == Some(&payload) {
            continue; // 投影未变（含同值写入 / 无变化）
        }
        last_sent.insert(id.0, payload.clone());
        out.pending.commands.push(PresentationCommand::Update {
            id: *id,
            payload: SyncPayload::Extension(payload),
        });
    }

    // 3) 反查基线：有 Extension 基线但当前查不到袋 -> 清理。
    //    防 RemovedComponents 事件因跳帧丢失导致 Godot 端永久残留。
    {
        let live: std::collections::HashSet<u64> = src.all.iter().map(|(id, _)| id.0).collect();
        let stale: Vec<u64> = out
            .baseline
            .components
            .iter()
            .filter(|(sid, kind)| *kind == PayloadKind::Extension && !live.contains(sid))
            .map(|(sid, _)| *sid)
            .collect();
        for sid in stale {
            last_sent.remove(&sid);
            remove_payload_baseline(
                &mut out.baseline,
                &mut out.pending,
                sid,
                PayloadKind::Extension,
            );
        }
    }

    // 清理已不在基线的缓存（despawn / detach 后），防无限增长。
    last_sent.retain(|id, _| {
        out.baseline
            .has_component(StableEntityId(*id), PayloadKind::Extension)
    });
}

/// mod 表现收集的读取侧：可见性、sub_kind 表、组件 / 稳定 ID 查询与移除事件。
#[derive(SystemParam)]
pub struct ModPresentationSources<'w, 's> {
    /// 本帧可见实体与观察者感知掩码。
    visibility: Res<'w, PresentationVisibility>,
    /// 装配期分配的 sub_kind 表；未挂 mod（或未装配）时缺失。
    schema: Option<Res<'w, ResolvedModPayloads>>,
    /// 带 mod 表现组件的实体。
    all: Query<'w, 's, (&'static StableEntityId, &'static ModPresentation)>,
    /// 稳定 ID 反查。
    ids: Query<'w, 's, &'static StableEntityId>,
    /// 本帧被移除的 mod 表现组件。
    removed: RemovedComponents<'w, 's, ModPresentation>,
}

/// mod 表现载荷收集系统。
///
/// 与 [collect_extension_bag] 同一套四步流程（移除事件 / 可见实体投影 / 基线反查 /
/// 缓存清理），差别只在投影：按 sub_kind 查 schema 求值字段可见性，找不到 schema
/// 的组整组丢弃；投影为空（空包 / 无任何可见字段）时主动发 `Remove` 并清基线。
///
/// **不依赖 `Changed`**：与扩展袋同理，同值写入也会被 Bevy 标记为 Changed，
/// 因此对每个可见实体重算投影，用「上次下发」比较来去抖。
pub fn collect_mod_presentation(
    mut last_sent: Local<HashMap<u64, CustomPayload>>,
    mut src: ModPresentationSources,
    mut out: SyncSinks,
) {
    // 1) 组件被移除：实体仍在且未在同帧重新挂回 -> Remove + 清基线。
    for entity in src.removed.read() {
        if src.all.get(entity).is_ok() {
            continue; // 同帧先删后加，视为仍在
        }
        let Ok(id) = src.ids.get(entity) else {
            continue; // 实体已销毁，交给 finalize 的 Despawn 路径
        };
        last_sent.remove(&id.0);
        remove_payload_baseline(
            &mut out.baseline,
            &mut out.pending,
            id.0,
            PayloadKind::Custom,
        );
    }

    // 2) 可见实体：投影 -> 去抖；空投影 -> Remove。
    for &entity in src.visibility.entities.iter() {
        let Ok((id, presentation)) = src.all.get(entity) else {
            continue;
        };
        let in_baseline = out.baseline.has_component(*id, PayloadKind::Custom);
        // schema 缺失（未挂 mod / 未装配）等价于「所有组都没有 schema」：投影为空。
        let payload = match src.schema.as_deref() {
            Some(schema) => {
                project_mod_presentation(presentation, src.visibility.local_perception, schema)
            }
            None => CustomPayload::default(),
        };

        if payload.is_empty() {
            if in_baseline {
                last_sent.remove(&id.0);
                remove_payload_baseline(
                    &mut out.baseline,
                    &mut out.pending,
                    id.0,
                    PayloadKind::Custom,
                );
            }
            continue;
        }

        // 基线缺失 = 首次进入（或 detach 后重进）-> 必须 Add，不能因哈希相同而跳过。
        if !in_baseline {
            last_sent.insert(id.0, payload.clone());
            out.baseline.components.insert((id.0, PayloadKind::Custom));
            out.pending.commands.push(PresentationCommand::Add {
                id: *id,
                payload: SyncPayload::Custom(payload),
            });
            continue;
        }

        if last_sent.get(&id.0) == Some(&payload) {
            continue; // 投影未变（含同值写入 / 无变化）
        }
        last_sent.insert(id.0, payload.clone());
        out.pending.commands.push(PresentationCommand::Update {
            id: *id,
            payload: SyncPayload::Custom(payload),
        });
    }

    // 3) 反查基线：有 Custom 基线但当前查不到组件 -> 清理（防跳帧丢事件留下残影）。
    let live: std::collections::HashSet<u64> = src.all.iter().map(|(id, _)| id.0).collect();
    for sid in stale_baselines(&out.baseline, &live, PayloadKind::Custom) {
        last_sent.remove(&sid);
        remove_payload_baseline(
            &mut out.baseline,
            &mut out.pending,
            sid,
            PayloadKind::Custom,
        );
    }

    // 清理已不在基线的缓存（despawn / detach 后），防无限增长。
    last_sent.retain(|id, _| {
        out.baseline
            .has_component(StableEntityId(*id), PayloadKind::Custom)
    });
}

/// 基线里记着 `kind`、但当前已查不到对应组件的稳定 ID（升序，确定性）。
fn stale_baselines(
    baseline: &CoreSyncBaseline,
    live: &std::collections::HashSet<u64>,
    kind: PayloadKind,
) -> Vec<u64> {
    let mut stale: Vec<u64> = baseline
        .components
        .iter()
        .filter(|(sid, baseline_kind)| *baseline_kind == kind && !live.contains(sid))
        .map(|(sid, _)| *sid)
        .collect();
    stale.sort_unstable();
    stale
}

/// 清除某载荷 kind 的基线并下发 Remove（幂等：基线没有就不发）。
fn remove_payload_baseline(
    baseline: &mut CoreSyncBaseline,
    pending: &mut CorePendingPresentation,
    stable: u64,
    kind: PayloadKind,
) {
    if baseline.components.remove(&(stable, kind)) {
        pending.commands.push(PresentationCommand::Remove {
            id: StableEntityId(stable),
            kind,
        });
    }
}

/// 扩展袋清理的**所有权系统**（跑在 FinalizePresentationSet，与 finalize 同 set、后于它）。
///
/// 非门控 kind 的 Remove 若只由 collector 负责，collector 一旦停用 / 移出 schedule，
/// Godot 端 extension 就会永久残留、基线只增不减。这里把「基线里有 Extension 但
/// 当前实体已无袋」的清理挪到发布侧：只要发布这一帧跑了，清理就跑。
///
/// 分工：collector 负责新增 / 变更；本系统只负责回收「不该再有」的基线。
pub fn reconcile_extension_baseline(
    mut pending: ResMut<CorePendingPresentation>,
    mut baseline: ResMut<CoreSyncBaseline>,
    bags: Query<&ExtensionBag>,
) {
    // 先收集待清理项，避免在遍历 baseline 时触发可变借用。
    let mut stale: Vec<u64> = Vec::new();
    for (stable, kind) in baseline.components.iter() {
        if *kind != PayloadKind::Extension {
            continue;
        }
        match baseline.entities.get(stable) {
            // 实体尚未 Attach：finalize（本 set 内先跑）会补映射，不能在此误删。
            None => continue,
            Some(&entity) => {
                if bags.get(entity).is_err() {
                    stale.push(*stable);
                }
            }
        }
    }
    // 确定性：HashSet/HashMap 迭代顺序随机，先排序再发命令。
    stale.sort_unstable();
    for stable in stale {
        remove_payload_baseline(&mut baseline, &mut pending, stable, PayloadKind::Extension);
    }
}

/// 收尾系统的全部参数：把 10 个参数折成一个 [SystemParam]。
#[derive(SystemParam)]
pub struct FinalizeParams<'w, 's> {
    /// 待发布命令与已下发基线。
    sinks: SyncSinks<'w>,
    /// 本帧可见实体与观察者感知掩码。
    visibility: Res<'w, PresentationVisibility>,
    /// 稳定 ID 查询。
    ids: Query<'w, 's, &'static StableEntityId>,
    /// 实体级必需感知掩码（隐私门控）。
    required: Query<'w, 's, &'static CoreRequiredPerception>,
    /// 单槽发布通道。
    slot: Res<'w, PresentationSlot>,
    /// 发布运行时状态（tick / seq / 计数）。
    runtime: ResMut<'w, PresentationRuntime>,
    /// 当前逻辑 tick。
    tick: Res<'w, crate::input::SimulationTick>,
    /// 渲染时钟。
    clock: Res<'w, RenderClockState>,
    /// 定步长时钟（发布 timestep）。
    time: Res<'w, Time<Fixed>>,
}

/// 收尾系统：Attach / 可见性降级 Remove / Detach / Despawn -> 整理 -> 发布。
pub fn finalize_presentation(mut p: FinalizeParams) {
    let mut commands = std::mem::take(&mut p.sinks.pending.commands);

    // 1) Attach：本帧可见、基线还没有 -> 首次进入表现层。
    for &entity in p.visibility.entities.iter() {
        let Ok(id) = p.ids.get(entity) else {
            continue;
        };
        if p.sinks.baseline.entities.contains_key(&id.0) {
            continue;
        }
        commands.push(PresentationCommand::Attach { id: *id });
        p.sinks.baseline.entities.insert(id.0, entity);
    }

    // 2) 可见性降级 -> Remove（防隐私残留）。
    let mut demoted: Vec<(u64, PayloadKind)> = Vec::new();
    for &(stable, kind) in p.sinks.baseline.components.iter() {
        if !kind.is_perception_gated() {
            continue;
        }
        if !p.visibility.ids.contains(&stable) {
            continue;
        }
        let Some(&entity) = p.sinks.baseline.entities.get(&stable) else {
            continue;
        };
        if !memory_path_component_visible(
            p.visibility.local_perception,
            p.required.get(entity).ok(),
        ) {
            demoted.push((stable, kind));
        }
    }
    // 确定性：HashSet 迭代顺序随机，发命令前显式按 (id, kind code) 排序。
    demoted.sort_unstable_by_key(|(stable, kind)| (*stable, kind.code()));
    for (stable, kind) in demoted {
        p.sinks.baseline.components.remove(&(stable, kind));
        commands.push(PresentationCommand::Remove {
            id: StableEntityId(stable),
            kind,
        });
    }

    // 3) 离开视野（Detach）与实体消失（Despawn）。
    //
    // 确定性：HashMap 迭代随机，必须先按稳定 ID 排序、再按序 push 命令；
    // 否则 compact_commands 的稳定排序会保留随机序，GPF1 字节不可复现。
    let mut gone: Vec<u64> = p
        .sinks
        .baseline
        .entities
        .iter()
        .filter(|(stable, _)| !p.visibility.ids.contains(stable))
        .map(|(stable, _)| *stable)
        .collect();
    gone.sort_unstable();
    for &stable in &gone {
        let Some(&entity) = p.sinks.baseline.entities.get(&stable) else {
            continue;
        };
        if p.ids.contains(entity) {
            commands.push(PresentationCommand::Detach {
                id: StableEntityId(stable),
            });
        } else {
            commands.push(PresentationCommand::Despawn {
                id: StableEntityId(stable),
            });
        }
    }
    for stable in gone {
        p.sinks.baseline.entities.remove(&stable);
        p.sinks
            .baseline
            .components
            .retain(|(sid, _)| *sid != stable);
    }

    // 4) 整理。
    compact_commands(&mut commands);

    // 5) 空帧不发布。
    if commands.is_empty() {
        return;
    }

    // 6) 发布。
    p.runtime.tick = p.tick.0;
    p.runtime.seq = p.runtime.seq.wrapping_add(1);
    p.runtime.last_commands = commands.len();
    p.runtime.render_clock_ms = p.clock.tick_clock_ms.unwrap_or(0);

    let frame = PresentationFrame {
        session: p.runtime.session,
        seq: p.runtime.seq,
        tick: p.runtime.tick,
        render_clock_ms: p.runtime.render_clock_ms,
        timestep_ms: p.time.timestep().as_secs_f32() * 1000.0,
        commands: commands.into(),
    };

    if p.slot.publish(Arc::new(frame)) {
        p.runtime.frames_overwritten = p.runtime.frames_overwritten.wrapping_add(1);
    }
    p.runtime.frames_published = p.runtime.frames_published.wrapping_add(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_tracks_entities_and_components() {
        let mut baseline = CoreSyncBaseline::default();
        let id = StableEntityId(42);
        baseline.entities.insert(id.0, Entity::from_bits(7));
        baseline.components.insert((id.0, PayloadKind::Transform));

        assert!(baseline.has_component(id, PayloadKind::Transform));
        assert!(!baseline.has_component(id, PayloadKind::Health));
        assert_eq!(baseline.entity_of(id), Some(Entity::from_bits(7)));
        assert_eq!(baseline.synced_entity_count(), 1);
    }

    /// 原型不再是 Attach 的专用字段：它和位置一样，由 collect_component
    /// 走普通 Add 通道下发，并写进组件基线。
    #[test]
    fn prototype_is_collected_as_a_normal_payload() {
        use crate::static_data::prototype::{Prototype, PROTOTYPE_GATED_DRIFTER};

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.init_resource::<CorePendingPresentation>();
        app.init_resource::<CoreSyncBaseline>();
        app.init_resource::<PresentationVisibility>();
        app.add_systems(Update, collect_component::<Prototype>);

        let entity = app
            .world_mut()
            .spawn((StableEntityId(7), Prototype::new(PROTOTYPE_GATED_DRIFTER)))
            .id();
        {
            let mut vis = app.world_mut().resource_mut::<PresentationVisibility>();
            vis.entities = vec![entity];
            vis.ids = [7u64].into_iter().collect();
        }
        app.update();

        let pending = app.world().resource::<CorePendingPresentation>();
        assert_eq!(pending.commands.len(), 1);
        match &pending.commands[0] {
            PresentationCommand::Add {
                id,
                payload: SyncPayload::Prototype(value),
            } => {
                assert_eq!(id.0, 7);
                assert_eq!(value.0, PROTOTYPE_GATED_DRIFTER);
            }
            other => panic!("原型必须作为普通 Add 下发，实际 {other:?}"),
        }
        assert!(
            app.world()
                .resource::<CoreSyncBaseline>()
                .has_component(StableEntityId(7), PayloadKind::Prototype),
            "首次下发后必须写入组件基线"
        );
    }

    #[test]
    fn visibility_membership_is_by_stable_id() {
        let mut visibility = PresentationVisibility::default();
        visibility.ids.insert(3);
        assert!(visibility.is_visible(StableEntityId(3)));
        assert!(!visibility.is_visible(StableEntityId(4)));
        assert!(
            visibility.local_perception == crate::privacy::PerceptionMask::EMPTY,
            "默认必须是 Fail-Closed"
        );
    }

    #[test]
    fn compact_orders_attach_before_its_payloads() {
        let id = StableEntityId(1);
        let mut commands = vec![
            PresentationCommand::Add {
                id,
                payload: SyncPayload::Presentation(super::super::payload::PresentationState::idle()),
            },
            PresentationCommand::Attach { id },
        ];
        compact_commands(&mut commands);
        assert_eq!(commands[0].kind_str(), "attach");
        assert_eq!(commands[1].kind_str(), "add");
    }

    #[test]
    fn finalize_emits_remove_then_detach_and_despawn() {
        use crate::privacy::{CoreRequiredPerception, ExactHealth, PerceptionMask};

        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.init_resource::<crate::input::SimulationTick>();
        app.init_resource::<CorePendingPresentation>();
        app.init_resource::<CoreSyncBaseline>();
        app.init_resource::<PresentationVisibility>();
        app.init_resource::<PresentationRuntime>();
        app.init_resource::<PresentationSlot>();
        app.init_resource::<RenderClockState>();
        app.add_systems(Update, finalize_presentation);

        let entity = app
            .world_mut()
            .spawn((
                StableEntityId(1),
                CoreRequiredPerception::new(0b0100),
                ExactHealth::full(10),
            ))
            .id();

        // 场景 1：隐私组件已同步，但本帧权限被剥夺 -> Remove（防隐私残留）。
        {
            let mut baseline = app.world_mut().resource_mut::<CoreSyncBaseline>();
            baseline.entities.insert(1, entity);
            baseline.components.insert((1, PayloadKind::Health));
        }
        {
            let mut vis = app.world_mut().resource_mut::<PresentationVisibility>();
            vis.local_perception = PerceptionMask(0b0001);
            vis.entities = vec![entity];
            vis.ids = [1u64].into_iter().collect();
        }
        app.update();
        let frame = app.world().resource::<PresentationSlot>().take().unwrap();
        let kinds: Vec<&str> = frame.commands.iter().map(|c| c.kind_str()).collect();
        assert_eq!(kinds, vec!["remove"], "权限被剥夺必须先发 Remove");

        // 场景 2：实体离开视野但逻辑实体还在 -> Detach（不是 Despawn）。
        {
            let mut vis = app.world_mut().resource_mut::<PresentationVisibility>();
            vis.entities = Vec::new();
            vis.ids.clear();
        }
        app.update();
        let frame = app.world().resource::<PresentationSlot>().take().unwrap();
        assert_eq!(frame.commands[0].kind_str(), "detach");

        // 场景 3：实体真的消失 -> Despawn。
        {
            let mut baseline = app.world_mut().resource_mut::<CoreSyncBaseline>();
            baseline.entities.insert(1, entity);
        }
        app.world_mut().despawn(entity);
        app.update();
        let frame = app.world().resource::<PresentationSlot>().take().unwrap();
        assert_eq!(frame.commands[0].kind_str(), "despawn");
    }

    #[test]
    fn empty_frames_are_not_published() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.init_resource::<crate::input::SimulationTick>();
        app.init_resource::<CorePendingPresentation>();
        app.init_resource::<CoreSyncBaseline>();
        app.init_resource::<PresentationVisibility>();
        app.init_resource::<PresentationRuntime>();
        app.init_resource::<PresentationSlot>();
        app.init_resource::<RenderClockState>();
        app.add_systems(Update, finalize_presentation);

        app.update();

        assert!(
            !app.world().resource::<PresentationSlot>().has_pending(),
            "没有任何实体/命令时不得发布空帧"
        );
    }
}
