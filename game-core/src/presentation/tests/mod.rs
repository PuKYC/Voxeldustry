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

/// RAWVOXELS 端到端的一段：VoxChunkRaw::skirt_mask 必须原样进入载荷。
#[test]
fn vox_chunk_raw_carries_skirt_mask_into_payload() {
    let component = VoxChunkRaw {
        lod: 2,
        blocks: vec![1, 2, 3, 4],
        skirt_mask: 0b10_1010,
    };
    match component.to_presentation() {
        SyncPayload::RawVoxels(payload) => {
            assert_eq!(payload.lod, 2);
            assert_eq!(payload.skirt_mask, 0b10_1010, "组分掩码必须进入载荷");
            assert_eq!(payload.blocks, vec![1, 2, 3, 4]);
        }
        other => panic!("期望 RawVoxels，得到 {}", other.kind().as_str()),
    }
}

/// RAWVOXELGROUPS 端到端：VoxChunkGroupRaw 的逐块 skirt_mask 必须原样进入载荷。
#[test]
fn vox_chunk_group_carries_skirt_masks_into_payload() {
    use game_engine::voxel::ChunkKey;
    let mut chunks = std::collections::BTreeMap::new();
    chunks.insert(ChunkKey::new(0, 0, 0), (vec![1u8, 2, 3], 0b10_1010));
    chunks.insert(ChunkKey::new(1, 0, 0), (vec![4u8, 5], 0));
    let component = VoxChunkGroupRaw { lod: 2, chunks };
    match component.to_presentation() {
        SyncPayload::RawVoxelGroups(payload) => {
            assert_eq!(payload.lod, 2);
            assert_eq!(
                payload.chunks[&(0, 0, 0)].1,
                0b10_1010,
                "逐块 mask 必须进入载荷"
            );
            assert_eq!(payload.chunks[&(0, 0, 0)].0, vec![1, 2, 3]);
            assert_eq!(payload.chunks[&(1, 0, 0)].1, 0);
        }
        other => panic!("期望 RawVoxelGroups，得到 {}", other.kind().as_str()),
    }
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
            PayloadKind::RawVoxelGroups => SyncPayload::RawVoxelGroups(
                crate::presentation::payload::RawVoxelGroupPayload::from_chunks(
                    1,
                    std::collections::BTreeMap::new(),
                ),
            ),
            PayloadKind::Camera => {
                SyncPayload::Camera(crate::presentation::payload::PresentedCamera::default())
            }
            PayloadKind::InputCaps => SyncPayload::InputCaps(PresentedInputCaps::default()),
        };
        assert_eq!(payload.kind(), kind);
        assert_eq!(payload.type_name(), kind.as_str());
    }
}
