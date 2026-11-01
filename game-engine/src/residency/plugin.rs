//! ResidencyPlugin 的装配与每 tick 系统。
//!
//! 顺序固定（文档 §7）：
//! Anchor -> Collect -> Resolve -> Emit -> Gate，整条链挂在
//! GameTickSet::Residency 内，由本插件自己 configure_sets，不 chain 别人的集合。

use std::collections::{BTreeMap, BTreeSet};

use bevy::prelude::*;

use super::Observer as ObserverTag;
use super::{
    body_key_for, chunk_of, observer_chunks, AwaitingChunks, ChunkAnchor, ChunkReached,
    GameTickSet, LogicPos, PathCorridor, PinRequests, Real, RequestSource, Residency,
    ResidencyChanged, ResidencyConfig, ResidencySet, ResidencyStats, ResidencyTable, UnsavedEditClear,
    UnsavedEditMark, WorkSites,
};
use crate::body_key::BodyKey;
use crate::identity::StableEntityId;
use crate::voxel::ChunkKey;

/// 驻留插件。
///
/// config 在 build 时作为资源插入；需要非默认防抖时长时直接构造：
/// ResidencyPlugin { config: ResidencyConfig { demote_grace_ticks: 2, ..default() } }。
#[derive(Debug, Clone, Default)]
pub struct ResidencyPlugin {
    /// 插件配置（作为 ResidencyConfig 资源插入）。
    pub config: ResidencyConfig,
}

impl Plugin for ResidencyPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(self.config.clone())
            .init_resource::<ResidencyTable>()
            .init_resource::<PinRequests>()
            .init_resource::<ResidencyStats>()
            .add_message::<ResidencyChanged>()
            .add_message::<ChunkReached>()
            .add_message::<UnsavedEditMark>()
            .add_message::<UnsavedEditClear>()
            .configure_sets(
                FixedUpdate,
                (
                    ResidencySet::Anchor,
                    ResidencySet::Collect,
                    ResidencySet::Resolve,
                    ResidencySet::Emit,
                    ResidencySet::Gate,
                )
                    .chain()
                    .in_set(GameTickSet::Residency),
            )
            // 标签的**移除 / 销毁**用 observer：此时实体还带着 StableEntityId，
            // 可以按 by_source 精确清理。新增不需要 observer：Collect 的
            // Added<Real> / Added<Observer> 已经覆盖，且同 tick 的 Anchor 先算好了位置。
            .add_observer(on_real_removed)
            .add_observer(on_real_despawned)
            .add_observer(on_observer_removed)
            .add_observer(on_observer_despawned)
            .add_systems(
                FixedUpdate,
                (
                    update_anchors.in_set(ResidencySet::Anchor),
                    (apply_chunk_reached, collect_requests)
                        .chain()
                        .in_set(ResidencySet::Collect),
                    resolve_levels.in_set(ResidencySet::Resolve),
                    emit_changes.in_set(ResidencySet::Emit),
                    gate_real_entities.in_set(ResidencySet::Gate),
                ),
            );
    }
}

// ───────────────────────────── Anchor ─────────────────────────────

/// 按 LogicPos 重算 ChunkAnchor（只在位置变化或锚点刚插入时算）。
///
/// **只用定点整数**：chunk_of 内部是 i64::div_euclid。禁止 f32 参与块归属。
/// 过渡期位置来源是 LogicPos；将来 BodyPose 落地后只改这里。
fn update_anchors(
    mut anchors: Query<
        (Entity, &LogicPos, &mut ChunkAnchor, Option<&StableEntityId>),
        Or<(Changed<LogicPos>, Added<ChunkAnchor>)>,
    >,
) {
    for (entity, pos, mut anchor, id) in &mut anchors {
        let Some(body) = body_key_for(entity, id) else {
            continue;
        };
        let next = ChunkAnchor::new(body, chunk_of(pos.0));
        if *anchor != next {
            *anchor = next;
        }
    }
}

// ───────────────────────────── Collect ─────────────────────────────

/// 把流式的实际到达档位写进表。只改 reached，不碰 target。
///
/// 放在 Collect 且先于 collect_requests：这样 Gate 读到的永远是
/// 「上一 tick 流式汇报的」状态，门控至少滞后一个 tick（见模块文档）。
fn apply_chunk_reached(
    mut table: ResMut<ResidencyTable>,
    mut reached: MessageReader<ChunkReached>,
) {
    // 同一 tick 多条消息时按 (body, key) 稳定排序；同键多条仍保持发送顺序（后者覆盖）。
    let mut messages: Vec<ChunkReached> = reached.read().copied().collect();
    messages.sort_by_key(|m| (m.body, m.key));
    for message in messages {
        table.set_reached(message.body, message.key, message.level);
    }
}

/// 收集本 tick 的全部请求变化，整体替换对应来源的快照。
///
/// 来源：Real 实体、Observer 实体、PinRequests、UnsavedEdit* 消息。
/// 所有 Query 结果先收集进 BTreeMap<RequestSource, _>（键序确定），再应用，
/// 因此发射顺序与 Query 遍历顺序无关。
#[allow(clippy::too_many_arguments)]
fn collect_requests(
    mut table: ResMut<ResidencyTable>,
    mut pins: ResMut<PinRequests>,
    changed_agents: Query<
        (
            Entity,
            Option<&StableEntityId>,
            &ChunkAnchor,
            Option<&WorkSites>,
            Option<&PathCorridor>,
        ),
        (
            With<Real>,
            Or<(
                Added<Real>,
                Changed<ChunkAnchor>,
                Changed<WorkSites>,
                Changed<PathCorridor>,
            )>,
        ),
    >,
    all_agents: Query<
        (
            Entity,
            Option<&StableEntityId>,
            &ChunkAnchor,
            Option<&WorkSites>,
            Option<&PathCorridor>,
        ),
        With<Real>,
    >,
    changed_observers: Query<
        (Entity, Option<&StableEntityId>, &ChunkAnchor, &ObserverTag),
        (
            With<ObserverTag>,
            Or<(Added<ObserverTag>, Changed<ChunkAnchor>, Changed<ObserverTag>)>,
        ),
    >,
    mut removed_work_sites: RemovedComponents<WorkSites>,
    mut removed_corridors: RemovedComponents<PathCorridor>,
    mut unsaved_mark: MessageReader<UnsavedEditMark>,
    mut unsaved_clear: MessageReader<UnsavedEditClear>,
) {
    let tick = table.begin_tick();
    let mut plans: BTreeMap<RequestSource, BTreeMap<(BodyKey, ChunkKey), Residency>> =
        BTreeMap::new();

    // 1) Real 实体：锚点块 ∪ WorkSites ∪ PathCorridor，档位 Data。
    for (entity, id, anchor, sites, corridor) in &changed_agents {
        let Some(body) = body_key_for(entity, id) else {
            continue;
        };
        plans.insert(
            RequestSource::Agent(body.stable_id()),
            agent_snapshot(anchor, sites, corridor),
        );
    }

    // WorkSites / PathCorridor 被整个移除时 Changed 不会再触发，用 RemovedComponents 补一次重建。
    let mut removed_entities: BTreeSet<Entity> = BTreeSet::new();
    removed_entities.extend(removed_work_sites.read());
    removed_entities.extend(removed_corridors.read());
    for entity in removed_entities {
        // 实体已销毁时 observer 已经清理过，这里跳过。
        let Ok((_, id, anchor, sites, corridor)) = all_agents.get(entity) else {
            continue;
        };
        let Some(body) = body_key_for(entity, id) else {
            continue;
        };
        plans.insert(
            RequestSource::Agent(body.stable_id()),
            agent_snapshot(anchor, sites, corridor),
        );
    }

    // 2) Observer 实体：以锚点为中心、半径 radius 的三维球，档位 Render。
    for (entity, id, anchor, observer) in &changed_observers {
        let Some(body) = body_key_for(entity, id) else {
            continue;
        };
        plans.insert(
            RequestSource::Observer(body.stable_id()),
            observer_snapshot(anchor, observer.radius),
        );
    }

    // 3) 系统钉住。
    for (namespace, id) in pins.take_touched() {
        let source = RequestSource::System { namespace, id };
        plans.insert(source, pins.snapshot(namespace, id));
    }

    // 4) 未存档编辑。同一 tick 内 clear 优先于 mark（两条消息流无法还原全局顺序）。
    let mut edits: BTreeMap<(BodyKey, ChunkKey), bool> = BTreeMap::new();
    for message in unsaved_mark.read() {
        edits.insert((message.body, message.key), true);
    }
    for message in unsaved_clear.read() {
        edits.insert((message.body, message.key), false);
    }
    if !edits.is_empty() {
        let mut snapshot: BTreeMap<(BodyKey, ChunkKey), Residency> = table
            .requests_of(RequestSource::UnsavedEdit)
            .iter()
            .map(|&key| (key, Residency::Data))
            .collect();
        for (key, marked) in edits {
            if marked && key.0.is_valid() {
                snapshot.insert(key, Residency::Data);
            } else {
                snapshot.remove(&key);
            }
        }
        plans.insert(RequestSource::UnsavedEdit, snapshot);
    }

    // BTreeMap 键序 = RequestSource 的确定性顺序（乱序插入得出相同结果）。
    for (source, snapshot) in plans {
        table.apply_snapshot(source, snapshot, tick);
    }
}

/// Real 实体的请求集合：锚点所在块 ∪ WorkSites ∪ PathCorridor。
fn agent_snapshot(
    anchor: &ChunkAnchor,
    sites: Option<&WorkSites>,
    corridor: Option<&PathCorridor>,
) -> BTreeMap<(BodyKey, ChunkKey), Residency> {
    let mut out = BTreeMap::new();
    if anchor.body.is_valid() {
        out.insert((anchor.body, anchor.chunk), Residency::Data);
    }
    if let Some(sites) = sites {
        for &(body, key) in &sites.0 {
            push_site(&mut out, body, key, "WorkSites");
        }
    }
    if let Some(corridor) = corridor {
        for &(body, key) in &corridor.0 {
            push_site(&mut out, body, key, "PathCorridor");
        }
    }
    out
}

fn push_site(
    out: &mut BTreeMap<(BodyKey, ChunkKey), Residency>,
    body: BodyKey,
    key: ChunkKey,
    what: &str,
) {
    if body.is_valid() {
        out.insert((body, key), Residency::Data);
    } else {
        debug_assert!(false, "residency: {what} 含 BodyKey::INVALID");
        warn!("residency: {what} 含无效 BodyKey，已忽略（chunk={key:?}）");
    }
}

/// Observer 实体的请求集合：以锚点为中心、半径 radius 的三维球，档位 Render。
fn observer_snapshot(
    anchor: &ChunkAnchor,
    radius: u8,
) -> BTreeMap<(BodyKey, ChunkKey), Residency> {
    let mut out = BTreeMap::new();
    if !anchor.body.is_valid() {
        return out;
    }
    for key in observer_chunks(anchor.chunk, radius) {
        out.insert((anchor.body, key), Residency::Render);
    }
    out
}

// ───────────────────────────── Resolve ─────────────────────────────

/// 汇总请求为 target：升级立即生效，降级走 demote_grace_ticks 防抖，每次只降一档。
///
/// 防抖计时锚点是 last_requested_tick：只要「当前 target 档位仍被请求」
/// （desired >= target）就每 tick 刷新；降级成功后再重置为当 tick，
/// 于是 Render -> Data -> None 每一档都重新计时。
fn resolve_levels(
    mut table: ResMut<ResidencyTable>,
    config: Res<ResidencyConfig>,
    mut stats: ResMut<ResidencyStats>,
) {
    let tick = table.tick;

    // 1) 仍在被需求的槽位刷新防抖计时（O(槽位数)，不做全量请求扫描）。
    for slot in table.slots.values_mut() {
        let desired = slot
            .requests
            .values()
            .copied()
            .max()
            .unwrap_or(Residency::None);
        if desired >= slot.target {
            slot.last_requested_tick = tick;
        }
    }

    // 2) 只处理被改动过的槽位；BTreeSet 升序 = ResidencyChanged 的发射顺序。
    let dirty: Vec<(BodyKey, ChunkKey)> = std::mem::take(&mut table.dirty).into_iter().collect();
    for key in dirty {
        let mut change = None;
        // 降级没走完（防抖未到期，或只降了一档还不够）时，下一 tick 继续看这个槽位。
        let mut pending_demotion = false;
        {
            let Some(slot) = table.slots.get_mut(&key) else {
                continue;
            };
            let desired = slot
                .requests
                .values()
                .copied()
                .max()
                .unwrap_or(Residency::None);

            if desired > slot.target {
                // 升级：立即兑现。内存压力接入后在这里改为「只记录、不兑现」。
                change = Some((slot.target, desired));
                slot.target = desired;
                slot.last_requested_tick = tick;
            } else if desired < slot.target {
                let grace = config.demote_grace_ticks;
                if tick.saturating_sub(slot.last_requested_tick) >= grace {
                    let next = match slot.target {
                        Residency::Render => Residency::Data,
                        Residency::Data => Residency::None,
                        Residency::None => Residency::None,
                    };
                    change = Some((slot.target, next));
                    slot.target = next;
                    // 每一档重新计时。
                    slot.last_requested_tick = tick;
                    pending_demotion = next > desired;
                } else {
                    pending_demotion = true;
                }
            }
        }
        if pending_demotion {
            table.dirty.insert(key);
        }
        if let Some((from, to)) = change {
            table.pending_changes.push(ResidencyChanged {
                body: key.0,
                key: key.1,
                from,
                to,
            });
        }
    }

    // 3) 清掉彻底空闲的槽位，避免长会话里槽位只增不减。
    table.slots.retain(|_, slot| {
        !(slot.requests.is_empty()
            && slot.target == Residency::None
            && slot.reached == Residency::None)
    });

    // 4) 统计（目标档位区块数）。
    let mut none_chunks = 0;
    let mut data_chunks = 0;
    let mut render_chunks = 0;
    for slot in table.slots.values() {
        match slot.target {
            Residency::None => none_chunks += 1,
            Residency::Data => data_chunks += 1,
            Residency::Render => render_chunks += 1,
        }
    }
    stats.none_chunks = none_chunks;
    stats.data_chunks = data_chunks;
    stats.render_chunks = render_chunks;
}

// ───────────────────────────── Emit ─────────────────────────────

/// 按键序写出 ResidencyChanged（Resolve 已经按 BTreeSet 升序排好）。
fn emit_changes(mut table: ResMut<ResidencyTable>, mut writer: MessageWriter<ResidencyChanged>) {
    for change in table.pending_changes.drain(..) {
        writer.write(change);
    }
}

// ───────────────────────────── Gate ─────────────────────────────

/// 给「所需区块还没全部到达 Data」的 Real 实体挂 AwaitingChunks。
///
/// **只读 slot.reached**：同 tick 里流式还没跑（Streaming 在 Residency 之后），
/// 所以门控最多滞后一个 tick，且同一 tick 内所有系统看到一致的门控。
/// Observer 不门控。
fn gate_real_entities(
    mut commands: Commands,
    agents: Query<
        (
            Entity,
            &ChunkAnchor,
            Option<&WorkSites>,
            Option<&PathCorridor>,
            Option<&AwaitingChunks>,
        ),
        With<Real>,
    >,
    table: Res<ResidencyTable>,
    mut stats: ResMut<ResidencyStats>,
) {
    let mut awaiting = 0;
    for (entity, anchor, sites, corridor, marker) in &agents {
        if real_ready(&table, anchor, sites, corridor) {
            if marker.is_some() {
                commands.entity(entity).remove::<AwaitingChunks>();
            }
        } else {
            awaiting += 1;
            if marker.is_none() {
                commands.entity(entity).try_insert(AwaitingChunks);
            }
        }
    }
    stats.awaiting_entities = awaiting;
}

/// Real 实体的放行条件：锚点块 ∪ WorkSites ∪ PathCorridor 全部 reached >= Data。
fn real_ready(
    table: &ResidencyTable,
    anchor: &ChunkAnchor,
    sites: Option<&WorkSites>,
    corridor: Option<&PathCorridor>,
) -> bool {
    if !anchor.is_valid() {
        // 还没锚定（没有 LogicPos 或没算出来）：不能让 AI 先跑起来。
        return false;
    }
    if table.reached(anchor.body, anchor.chunk) < Residency::Data {
        return false;
    }
    let sites = sites.map_or(&[][..], |sites| sites.0.as_slice());
    if !all_reached(table, sites) {
        return false;
    }
    let corridor = corridor.map_or(&[][..], |corridor| corridor.0.as_slice());
    all_reached(table, corridor)
}

fn all_reached(table: &ResidencyTable, entries: &[(BodyKey, ChunkKey)]) -> bool {
    for &(body, key) in entries {
        if !body.is_valid() {
            debug_assert!(false, "residency: 门控遇到 BodyKey::INVALID");
            return false;
        }
        if table.reached(body, key) < Residency::Data {
            return false;
        }
    }
    true
}

// ───────────────────────────── 标签生命周期 ─────────────────────────────

fn on_real_removed(
    removed: On<Remove, Real>,
    ids: Query<&StableEntityId>,
    mut table: ResMut<ResidencyTable>,
) {
    clear_agent(&mut table, removed.entity, &ids);
}

fn on_real_despawned(
    despawned: On<Despawn, Real>,
    ids: Query<&StableEntityId>,
    mut table: ResMut<ResidencyTable>,
) {
    clear_agent(&mut table, despawned.entity, &ids);
}

fn on_observer_removed(
    removed: On<Remove, ObserverTag>,
    ids: Query<&StableEntityId>,
    mut table: ResMut<ResidencyTable>,
) {
    clear_observer(&mut table, removed.entity, &ids);
}

fn on_observer_despawned(
    despawned: On<Despawn, ObserverTag>,
    ids: Query<&StableEntityId>,
    mut table: ResMut<ResidencyTable>,
) {
    clear_observer(&mut table, despawned.entity, &ids);
}

fn clear_agent(table: &mut ResidencyTable, entity: Entity, ids: &Query<&StableEntityId>) {
    match ids.get(entity) {
        Ok(id) => table.clear_source(RequestSource::Agent(*id)),
        Err(_) => {
            debug_assert!(false, "residency: {entity:?} 移除 Real 时读不到 StableEntityId");
            warn!("residency: {entity:?} 移除 Real 时读不到 StableEntityId，请求无法清理");
        }
    }
}

fn clear_observer(table: &mut ResidencyTable, entity: Entity, ids: &Query<&StableEntityId>) {
    match ids.get(entity) {
        Ok(id) => table.clear_source(RequestSource::Observer(*id)),
        Err(_) => {
            debug_assert!(false, "residency: {entity:?} 移除 Observer 时读不到 StableEntityId");
            warn!("residency: {entity:?} 移除 Observer 时读不到 StableEntityId，请求无法清理");
        }
    }
}
