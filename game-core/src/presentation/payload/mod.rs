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
    // 体素网格：一个 body / mesh block 的贪心矩形实例流（设计 9.2 / 9.5）。
    // code 只追加，禁止重排 / 复用。
    RectList     = 7, "rectlist",     payload: RectListPayload;
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

/// 一个 body / mesh block 的贪心矩形实例流（设计 9.2 / 9.5）。
///
/// 承载 `game_engine::voxel::pack_rect_stream` 产出的 8 B/矩形 u64 流，转成
/// **确定的小端字节缓冲**（`rects`，每 8 字节一个 u64）。`lod` 是该 mesh 块
/// 的 LOD 级别（0..=3）。
///
/// ## 39 bit 矩形位布局（小端 u64）
///
/// ```text
/// bit  [0,3)   orientation = plane * 2 + dir   (3 bit, 0..5)
/// bit  [3,9)   slice                           (6 bit, 0..32)
/// bit  [9,14)  row                             (5 bit, 0..31)
/// bit [14,19)  col                             (5 bit, 0..31)
/// bit [19,25)  w                               (6 bit, 1..32)
/// bit [25,31)  h                               (6 bit, 1..32)
/// bit [31,39)  material                        (8 bit)
/// bit [39,47)  ao                              (4 角 × 2 bit, 3=全亮)
/// bit [47,64)  0（保留）
/// ```
///
/// AO（bit [39,47)）：每角 2 bit、0..=3，角序 i = cx | (cy << 1)，与
/// `voxel_mesh::rect_corner` 及着色器 UV 同序。39 bit 描述符本身不变；旧生产端
/// 写 0、旧消费端只读 [0,39)，因此是向后兼容的纯扩展。
///
/// 矩形是**位置无关**的整数描述符：body / island 的变换**不在**载荷里，由
/// Godot 绘制节点（A = `MultiMeshInstance3D.Transform3D`；B = per-draw model
/// matrix / push constant）携带，`voxel_size` 同样不进载荷。
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RectListPayload {
    /// mesh 块的 LOD 级别（0..=3）。
    pub lod: u8,
    /// `pack_rect_stream` 的 u64 流按小端展开的字节缓冲；长度恒为 8 的倍数。
    pub rects: Vec<u8>,
}

impl RectListPayload {
    /// 一个矩形描述符的字节数（u64，路径 B）。
    pub const WORD_BYTES: usize = 8;

    /// 从 `game_engine::voxel::pack_rect_stream` 的 `Vec<u64>` 构造，
    /// 逐字按小端展开成确定性字节缓冲。
    pub fn from_stream(lod: u8, words: &[u64]) -> Self {
        let mut rects = Vec::with_capacity(words.len() * Self::WORD_BYTES);
        for word in words {
            rects.extend_from_slice(&word.to_le_bytes());
        }
        Self { lod, rects }
    }

    /// 矩形数量（字节缓冲长度 / 8）。
    pub fn rect_count(&self) -> usize {
        self.rects.len() / Self::WORD_BYTES
    }

    /// 是否为空矩形列表。
    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    /// 还原成 u64 流；长度不是 8 的倍数的尾部残字节被忽略。
    pub fn to_words(&self) -> Vec<u64> {
        self.rects
            .chunks_exact(Self::WORD_BYTES)
            .map(|chunk| {
                let mut word = [0u8; Self::WORD_BYTES];
                word.copy_from_slice(chunk);
                u64::from_le_bytes(word)
            })
            .collect()
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

    /// 新载荷 code / 名字必须出现在注册表里，且 0..6 保持原样（只追加）。
    #[test]
    fn rect_list_kind_is_registered_append_only() {
        assert_eq!(
            PayloadKind::RectList.code(),
            7,
            "rectlist code 必须固定为 7"
        );
        assert_eq!(PayloadKind::RectList.as_str(), "rectlist");
        assert_eq!(PayloadKind::from_code(7), Some(PayloadKind::RectList));
        assert!(!PayloadKind::RectList.is_perception_gated());
        assert_eq!(
            PayloadKind::ALL
                .iter()
                .copied()
                .filter(|kind| *kind == PayloadKind::RectList)
                .count(),
            1,
            "rectlist 必须恰好出现一次"
        );
        for (code, name) in [
            (0u8, "transform"),
            (1, "presentation"),
            (2, "health"),
            (3, "visibility"),
            (4, "interaction"),
            (5, "ext"),
            (6, "prototype"),
        ] {
            assert_eq!(
                PayloadKind::from_code(code).map(|kind| kind.as_str()),
                Some(name)
            );
        }
    }
}
