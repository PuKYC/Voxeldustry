//! 表现命令整理 / 覆盖合并的回归测试（game-core 特化）。

use std::sync::Arc;

use super::payload::{
    ExtField, ExtValue, ExtensionPayload, PayloadKind, PresentationState, PresentedPrototype,
    SyncPayload,
};
use super::RenderTransformSample;
use super::{
    coalesce_commands, compact_commands, PresentationCommand, PresentationFrame, PresentationSlot,
};
use game_engine::identity::StableEntityId;

fn attach(id: u64) -> PresentationCommand {
    PresentationCommand::Attach {
        id: StableEntityId(id),
    }
}

fn update(id: u64) -> PresentationCommand {
    PresentationCommand::Update {
        id: StableEntityId(id),
        payload: SyncPayload::Transform(RenderTransformSample::default()),
    }
}

fn despawn(id: u64) -> PresentationCommand {
    PresentationCommand::Despawn {
        id: StableEntityId(id),
    }
}

fn detach(id: u64) -> PresentationCommand {
    PresentationCommand::Detach {
        id: StableEntityId(id),
    }
}

fn remove(id: u64, kind: PayloadKind) -> PresentationCommand {
    PresentationCommand::Remove {
        id: StableEntityId(id),
        kind,
    }
}

fn add(id: u64, kind: PayloadKind) -> PresentationCommand {
    PresentationCommand::Add {
        id: StableEntityId(id),
        payload: payload_of(kind),
    }
}

fn update_kind(id: u64, kind: PayloadKind) -> PresentationCommand {
    PresentationCommand::Update {
        id: StableEntityId(id),
        payload: payload_of(kind),
    }
}

fn payload_of(kind: PayloadKind) -> SyncPayload {
    match kind {
        PayloadKind::Presentation => SyncPayload::Presentation(PresentationState::idle()),
        PayloadKind::Extension => SyncPayload::Extension(
            ExtensionPayload::from_fields(vec![ExtField {
                key: 1,
                value: ExtValue::I32(1),
            }])
            .unwrap(),
        ),
        PayloadKind::Prototype => SyncPayload::Prototype(PresentedPrototype(1)),
        _ => SyncPayload::Transform(RenderTransformSample::default()),
    }
}

fn frame(seq: u64, commands: Vec<PresentationCommand>) -> Arc<PresentationFrame> {
    Arc::new(PresentationFrame {
        session: 1,
        seq,
        tick: seq as u32,
        render_clock_ms: 0,
        timestep_ms: 0.0,
        commands: commands.into(),
    })
}

#[test]
fn attach_is_sorted_before_update() {
    let mut commands = vec![update(1), attach(1)];
    compact_commands(&mut commands);
    assert_eq!(commands[0].kind_str(), "attach");
    assert_eq!(commands[1].kind_str(), "update");
}

#[test]
fn same_frame_attach_and_despawn_cancel_out() {
    let mut commands = vec![attach(5), update(5), despawn(5)];
    compact_commands(&mut commands);
    assert_eq!(commands.len(), 1, "同帧出生又死亡应只留 Despawn");
    assert_eq!(commands[0].kind_str(), "despawn");
}

#[test]
fn detach_does_not_leak_component_updates() {
    let mut commands = vec![detach(9), update(9), update(10)];
    compact_commands(&mut commands);
    let ids: Vec<(u64, &str)> = commands
        .iter()
        .map(|c| (c.target().0, c.kind_str()))
        .collect();
    assert_eq!(ids, vec![(10, "update"), (9, "detach")]);
}

#[test]
fn slot_keeps_only_latest() {
    let slot = PresentationSlot::new();
    assert!(!slot.publish(frame(1, Vec::new())), "第一次发布没有覆盖");
    assert!(
        slot.publish(frame(2, Vec::new())),
        "第二次发布会覆盖未取走的帧"
    );
    assert_eq!(slot.take().unwrap().seq, 2);
    assert!(slot.take().is_none(), "取走后槽位为空");
}

// ───────────────────── 覆盖合并（bug #1 回归） ─────────────────────

#[test]
fn overwrite_with_attach_is_not_lost() {
    let slot = PresentationSlot::new();
    slot.publish(frame(1, vec![attach(1)]));
    slot.publish(frame(2, Vec::new()));
    let taken = slot.take().unwrap();
    assert_eq!(taken.seq, 2);
    assert_eq!(taken.commands.len(), 1);
    assert_eq!(taken.commands[0].kind_str(), "attach");
}

#[test]
fn overwrite_with_detach_is_not_lost() {
    let slot = PresentationSlot::new();
    slot.publish(frame(1, vec![detach(1)]));
    slot.publish(frame(2, Vec::new()));
    let taken = slot.take().unwrap();
    assert_eq!(taken.commands.len(), 1);
    assert_eq!(taken.commands[0].kind_str(), "detach");
}

#[test]
fn overwrite_with_despawn_is_not_lost() {
    let slot = PresentationSlot::new();
    slot.publish(frame(1, vec![despawn(1)]));
    slot.publish(frame(2, Vec::new()));
    let taken = slot.take().unwrap();
    assert_eq!(taken.commands.len(), 1);
    assert_eq!(taken.commands[0].kind_str(), "despawn");
}

#[test]
fn overwrite_with_remove_is_not_lost() {
    let slot = PresentationSlot::new();
    slot.publish(frame(1, vec![remove(1, PayloadKind::Health)]));
    slot.publish(frame(2, Vec::new()));
    let taken = slot.take().unwrap();
    assert_eq!(taken.commands.len(), 1);
    assert_eq!(taken.commands[0].kind_str(), "remove");
}

#[test]
fn coalesce_remove_then_add_keeps_add() {
    let merged = coalesce_commands(
        &[remove(1, PayloadKind::Presentation)],
        &[add(1, PayloadKind::Presentation)],
    );
    let kinds: Vec<(&str, u64)> = merged
        .iter()
        .map(|c| (c.kind_str(), c.target().0))
        .collect();
    assert_eq!(kinds, vec![("add", 1)]);
}

#[test]
fn coalesce_detach_then_reenter_keeps_attach() {
    let merged = coalesce_commands(
        &[detach(1)],
        &[attach(1), add(1, PayloadKind::Presentation)],
    );
    let kinds: Vec<&str> = merged.iter().map(|c| c.kind_str()).collect();
    assert_eq!(kinds, vec!["attach", "add"]);
}

#[test]
fn coalesce_attach_then_detach_keeps_detach_only() {
    let merged = coalesce_commands(
        &[attach(1), add(1, PayloadKind::Presentation)],
        &[detach(1)],
    );
    let kinds: Vec<&str> = merged.iter().map(|c| c.kind_str()).collect();
    assert_eq!(kinds, vec!["detach"]);
}

#[test]
fn coalesce_attach_then_despawn_keeps_despawn_only() {
    let merged = coalesce_commands(&[attach(1)], &[despawn(1)]);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].kind_str(), "despawn");
}

#[test]
fn coalesce_last_payload_write_wins() {
    let old = add(1, PayloadKind::Presentation);
    let new = update_kind(1, PayloadKind::Presentation);
    let merged = coalesce_commands(&[attach(1), old], &[new]);
    let kinds: Vec<&str> = merged.iter().map(|c| c.kind_str()).collect();
    assert_eq!(kinds, vec!["attach", "update"]);
}

#[test]
fn coalesce_keep_distinct_kinds() {
    // Presentation / Health / Transform 三种载荷互不覆盖，必须全部保留。
    let merged = coalesce_commands(
        &[attach(1), add(1, PayloadKind::Presentation)],
        &[remove(1, PayloadKind::Health), update(1)],
    );
    let mut kinds: Vec<&str> = merged.iter().map(|c| c.kind_str()).collect();
    kinds.sort_unstable();
    assert_eq!(kinds, vec!["add", "attach", "remove", "update"]);
}

#[test]
fn coalesce_repeated_publish_is_stable() {
    let slot = PresentationSlot::new();
    slot.publish(frame(1, vec![attach(1)]));
    slot.publish(frame(2, vec![add(1, PayloadKind::Presentation)]));
    slot.publish(frame(3, vec![update_kind(1, PayloadKind::Presentation)]));
    slot.publish(frame(4, Vec::new()));
    let taken = slot.take().unwrap();
    let kinds: Vec<&str> = taken.commands.iter().map(|c| c.kind_str()).collect();
    assert_eq!(kinds, vec!["attach", "update"]);
    assert!(slot.take().is_none());
}

#[test]
fn coalesce_does_not_duplicate_other_entities() {
    let merged = coalesce_commands(&[attach(1), attach(2)], &[update(1), update(2)]);
    assert_eq!(merged.len(), 4);
    let attaches = merged.iter().filter(|c| c.kind_str() == "attach").count();
    let updates = merged.iter().filter(|c| c.kind_str() == "update").count();
    assert_eq!((attaches, updates), (2, 2));
}

/// P3 回归：同 rank 的命令必须按 (实体 ID, 载荷 kind code) 确定全序，
/// 不能依赖 HashMap / 系统注册顺序，否则 GPF1 字节跨进程不可复现。
/// 覆盖 Add/Remove Extension 与普通载荷混排。
#[test]
fn same_rank_commands_sorted_by_entity_then_kind() {
    let mut commands = vec![
        remove(2, PayloadKind::Extension),
        update_kind(2, PayloadKind::Presentation),
        add(1, PayloadKind::Extension),
        update_kind(1, PayloadKind::Transform),
        update_kind(2, PayloadKind::Transform),
        update_kind(1, PayloadKind::Presentation),
    ];
    compact_commands(&mut commands);
    let keys: Vec<(u8, u64, u8)> = commands
        .iter()
        .map(|c| {
            let kind = match c {
                PresentationCommand::Add { payload, .. }
                | PresentationCommand::Update { payload, .. } => payload.kind().code(),
                PresentationCommand::Remove { kind, .. } => kind.code(),
                _ => 0,
            };
            (c.rank(), c.target().0, kind)
        })
        .collect();
    assert_eq!(
        keys,
        vec![
            (1, 1, 5),
            (2, 1, 0),
            (2, 1, 1),
            (2, 2, 0),
            (2, 2, 1),
            (3, 2, 5),
        ],
        "(rank, id, kind) 必须构成确定全序"
    );
}
