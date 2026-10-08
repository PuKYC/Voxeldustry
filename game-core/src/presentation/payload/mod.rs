//! 表现载荷实例（game-core 专属内容）。
//!
//! 机制在 game_engine::presentation::payload；这里用 define_payloads! 实例化
//! PayloadKind + SyncPayload，并定义各载荷类型与 ToPresentation。

// packed.rs 持有各载荷的 GPF1 字节布局（实现引擎的 PackedPayload）。
mod extension;
mod packed;

pub use extension::{
    project, ExtError, ExtField, ExtValue, ExtensionBag, ExtensionFieldSchema, ExtensionPayload,
    ExtensionSchema, FieldVisibility, SetOutcome, MAX_EXT_FIELDS, MAX_EXT_TAGS,
};

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use game_engine::presentation::interp::RenderTransformSample;

use crate::input::actions::ActionId;

game_engine::define_payloads! {
    PayloadKind, SyncPayload;
    // 普通载荷：不带 private 子句（不受组件级可见性门控）。
    Transform    = 0, "transform",    payload: RenderTransformSample;
    Presentation = 1, "presentation", payload: PresentationState;
    // 隐私载荷：private: ExactHealth 表示该载荷受 ExactHealth 组件的可见性门控。
    Health       = 2, "health",       private: ExactHealth, payload: PresentedHealth;
    Visibility   = 3, "visibility",   payload: PresentedVisibility;
    Interaction  = 4, "interaction",  payload: InteractionHint;
    // 业务扩展载荷：一批具名字段装进一个「袋」（见 payload/extension.rs）。
    Extension    = 5, "ext",          payload: ExtensionPayload;
    // 原型：曾经的 attach 附加负载，现在是与其它载荷地位平等的普通 i32 载荷。
    Prototype    = 6, "prototype",    payload: PresentedPrototype;
    // code 7 是已删除的 RectList（服务端贪婪矩形流）遗位：**有意留空，禁止复用**
    // （复用会破坏线格式与 Godot ABI）。
    // 体素表现的唯一通道：一个 32³ 块内部 + halo 层（34³）的方块 id 缓冲，
    // 由 Godot 侧自行贪婪 meshing。code 只追加。
    RawVoxels    = 8, "rawvoxels",    payload: RawVoxelPayload;
}

/// L5：精简表现状态。
///
/// locomotion_state 互斥；action_state 独立叠加；overlay_tags 可多个并存。
/// AI 内部 FSM/BT 状态不在这里，也不注册进表现管线（决策层物理隔离）。
#[derive(Component, Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PresentationState {
    pub locomotion_state: u32,
    pub action_state: u32,
    pub overlay_tags: Vec<u32>,
}

impl PresentationState {
    pub fn idle() -> Self {
        Self::default()
    }

    pub fn with_locomotion(mut self, state: u32) -> Self {
        self.locomotion_state = state;
        self
    }
}

/// 经过可见性过滤后的血量。
#[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct PresentedHealth {
    pub current: f32,
    pub max: f32,
}

/// 本地视角下的可见性。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PresentedVisibility {
    pub visible: bool,
}

/// 静态原型 ID 载荷（曾经的 attach 附加负载，现在是普通载荷）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PresentedPrototype(pub u32);

/// 一个 mesh 块的原始体素缓冲（内部 32³ + halo 层，共 34³）。
///
/// 承载 `game_engine::presentation::voxel::extract_raw_halo` 产出的方块 id 字节缓冲
/// （`blocks.len() == RAW_VOXELS == 39304`，值 = 方块 id，0 = 空气）。
///
/// ## 布局（冻结，A / B 两侧逐字节一致）
///
/// - 内部 32³：块局部体素 `(x, y, z) ∈ [0, 32)³` 落在 halo 坐标
///   `(x+1, y+1, z+1)`；
/// - 扁平下标 `i = hx + 34 * (hy + 34 * hz)`，`hx, hy, hz ∈ [0, 34)`；
/// - halo 是 6 个同 LOD 邻块的一层边界，仅用于剔除块边界面；缺失邻块 = 空气；
/// - `lod` 是该 mesh 块的 LOD 级别（0..=3），体素缩放由渲染侧按 lod 处理。
///
/// 这是体素表现的**唯一**通道：同一个 mesh 块只下发本载荷（组件 [crate::presentation::voxel_mesh::VoxChunkRaw]）。
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RawVoxelPayload {
    /// mesh 块的 LOD 级别（0..=3）。
    pub lod: u8,
    /// `extract_raw_halo` 的 34³ 方块 id 缓冲；长度应为 `RAW_VOXELS`。
    pub blocks: Vec<u8>,
}

impl RawVoxelPayload {
    /// 从 `extract_raw_halo` 的缓冲构造。
    pub fn from_halo(lod: u8, blocks: Vec<u8>) -> Self {
        Self { lod, blocks }
    }

    /// 是否为空缓冲。
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

/// 交互提示（状态）。
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionHint {
    pub action: ActionId,
    pub enabled: bool,
}

/// 把逻辑组件翻译成精简表现载荷。
///
/// **不实现这个 trait 的组件 = 不同步**（承载原 ReplicateServerOnly 语义）。
pub trait ToPresentation: Component {
    const KIND: PayloadKind;

    fn to_presentation(&self) -> SyncPayload;
}

#[cfg(test)]
mod tests {
    use super::*;
    use game_engine::presentation::interp::RenderTransform;

    #[test]
    fn payload_kind_strings_are_unique_and_stable() {
        let mut names: Vec<&str> = PayloadKind::ALL.iter().map(|k| k.as_str()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "载荷标签必须唯一");
    }

    #[test]
    fn payload_codes_unique() {
        let mut codes: Vec<u8> = PayloadKind::ALL.iter().map(|k| k.code()).collect();
        codes.sort_unstable();
        let before = codes.len();
        codes.dedup();
        assert_eq!(
            before,
            codes.len(),
            "载荷 code 必须唯一（重排/复用会破坏 ABI）"
        );
    }

    #[test]
    fn code_roundtrip() {
        for &kind in PayloadKind::ALL {
            assert_eq!(PayloadKind::from_code(kind.code()), Some(kind));
        }
        assert_eq!(PayloadKind::from_code(200), None);
        assert_eq!(PayloadKind::from_code(u8::MAX), None);
    }

    #[test]
    fn gated_kinds_stable() {
        let gated: Vec<&str> = PayloadKind::ALL
            .iter()
            .copied()
            .filter(|kind| kind.is_perception_gated())
            .map(|kind| kind.as_str())
            .collect();
        assert_eq!(
            gated,
            vec!["health"],
            "门控载荷清单变了，请同步检查 CorePrivacyScope"
        );
    }

    #[test]
    fn render_transform_is_f32_ready() {
        let translation = Vec3::new(1.5, -1.0, 0.0);
        let rendered = RenderTransform::from_translation(translation);
        assert!((rendered.position[0] - 1.5).abs() < f32::EPSILON);
        assert!((rendered.position[1] + 1.0).abs() < f32::EPSILON);
        assert_eq!(rendered.position[2], 0.0);
    }

    /// raw voxel 载荷 code / 名字只追加，语义为「内部 32³ + halo」。
    #[test]
    fn raw_voxels_kind_is_registered_append_only() {
        assert_eq!(PayloadKind::RawVoxels.code(), 8);
        assert_eq!(PayloadKind::RawVoxels.as_str(), "rawvoxels");
        assert_eq!(PayloadKind::from_code(8), Some(PayloadKind::RawVoxels));
        assert!(!PayloadKind::RawVoxels.is_perception_gated());
        let payload = RawVoxelPayload::from_halo(1, vec![1, 2, 3]);
        assert_eq!(payload.lod, 1);
        assert!(!payload.is_empty());
        assert!(RawVoxelPayload::default().is_empty());
    }

    /// 0..6 与 8 保持固定；code 7 是已删除 RectList 的遗位，必须留空不得复用。
    #[test]
    fn payload_codes_are_append_only_and_code_seven_stays_vacant() {
        assert_eq!(
            PayloadKind::from_code(7),
            None,
            "code 7（旧 RectList）必须留空，禁止复用"
        );
        for (code, name) in [
            (0u8, "transform"),
            (1, "presentation"),
            (2, "health"),
            (3, "visibility"),
            (4, "interaction"),
            (5, "ext"),
            (6, "prototype"),
            (8, "rawvoxels"),
        ] {
            assert_eq!(
                PayloadKind::from_code(code).map(|kind| kind.as_str()),
                Some(name)
            );
        }
    }
}
