//! game-core 表现层回归测试（引擎机制 + 本游戏载荷实例）。

use super::*;

mod command;
mod extension_bag;
mod packed;

/// 契约自检：is_perception_gated() 的载荷类型必须与 CorePrivacyScope 对齐。
#[test]
fn perception_gated_kinds_are_explicitly_listed() {
    let gated: Vec<&str> = PayloadKind::ALL
        .iter()
        .filter(|kind| kind.is_perception_gated())
        .map(|kind| kind.as_str())
        .collect();
    assert_eq!(
        gated,
        vec!["health"],
        "隐私载荷清单变了：请同步检查 CorePrivacyScope 元组"
    );
}

#[test]
fn all_payload_kinds_have_round_trippable_labels() {
    for kind in PayloadKind::ALL.iter().copied() {
        let payload = match kind {
            PayloadKind::Transform => SyncPayload::Transform(RenderTransformSample::default()),
            PayloadKind::Presentation => SyncPayload::Presentation(PresentationState::idle()),
            PayloadKind::Health => SyncPayload::Health(PresentedHealth::default()),
            PayloadKind::Visibility => SyncPayload::Visibility(PresentedVisibility::default()),
            PayloadKind::Interaction => SyncPayload::Interaction(InteractionHint {
                action: crate::input::actions::ActionId(0),
                enabled: false,
            }),
            PayloadKind::Extension => {
                SyncPayload::Extension(crate::presentation::payload::ExtensionPayload::default())
            }
            PayloadKind::Prototype => SyncPayload::Prototype(PresentedPrototype::default()),
            PayloadKind::RawVoxels => SyncPayload::RawVoxels(
                crate::presentation::payload::RawVoxelPayload::from_halo(1, vec![1, 2, 3, 4]),
            ),
        };
        assert_eq!(payload.kind(), kind);
        assert_eq!(payload.type_name(), kind.as_str());
    }
}
