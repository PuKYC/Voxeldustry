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

use std::collections::BTreeMap;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use game_engine::presentation::interp::RenderTransformSample;
use game_engine::voxel::ChunkKey;

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
    // 体素分组：一个组件承载同一 LOD 的多块（每块 34³ halo），按 ChunkKey 升序
    // 序列化，Godot 侧物化「一个 LOD 分组一个节点」。code 只追加。
    RawVoxelGroups = 9, "rawvoxelgroups", payload: RawVoxelGroupPayload;
    // 相机视角（yaw / pitch，弧度）：由带 CameraFlag 的实体下发，Godot 的 CameraRig 消费。
    // code 只追加：9 已被 RawVoxelGroups 占用，故为 10。
    Camera       = 10, "camera",      payload: PresentedCamera;
    // 输入能力：present（能力组件并集）/ available（经禁用削减后的可用位）/
    // source（ControlledBy.source）/ local（是否挂 LocalPlayer）。code 11 是
    // code 7 留空后的下一个可用位，只追加。
    InputCaps    = 11, "inputcaps",   payload: PresentedInputCaps;
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

/// 相机视角载荷（弧度）。
///
/// 约定与 `gameplay::camera` 一致：yaw 绕 +Y（0 = 朝 -Z），pitch 绕 +X（正 = 抬头），
/// 欧拉顺序 YXZ，即 Godot `Node3D.rotation = Vector3(pitch, yaw, 0)`。
/// 相机**位置**不在这里：它走同一实体的 `Transform` 载荷。
#[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct PresentedCamera {
    pub yaw: f32,
    pub pitch: f32,
}

/// 静态原型 ID 载荷（曾经的 attach 附加负载，现在是普通载荷）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PresentedPrototype(pub u32);

/// 输入能力载荷（本地玩家 / 当前被操纵对象的输入能力快照）。
///
/// - `present`：能力组件（MoveChannel / JumpChannel / …）存在 -> 动作位并集；
/// - `available`：`present` 经 `InputDisabled` / `Stunned` / `Rooted` 削减后的
///   当前可用位（对应逻辑侧 `InputAvailability.mask`）；
/// - `source`：`ControlledBy.source.0`（0 = 无控制源哨兵）；
/// - `local`：实体是否挂 `LocalPlayer`（表现侧据此筛选本地玩家）；
/// - `look`：视角轴是否开放（`LookChannel`）。视角不占动作位，所以单独一列；
///   UI 据此决定要不要显示 / 启用视角区。
///
/// 位掩码按 u64 保存，GPF1 里按位写成 i64（S 侧不做真假判断，只做展示）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PresentedInputCaps {
    pub present: u64,
    pub available: u64,
    pub source: u64,
    pub local: bool,
    pub look: bool,
}

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
/// - `lod` 是该 mesh 块的 LOD 级别（0..=3），体素缩放由渲染侧按 lod 处理；
/// - `skirt_mask` 是跨 LOD 接缝裙边掩码（6 bit，ExternalPlane 顺序
///   `[+x, -x, +y, -y, +z, -z]`，0 = 无裙边）。
///
/// 这是体素表现的**唯一**通道：同一个 mesh 块只下发本载荷（组件 [crate::presentation::voxel_mesh::VoxChunkRaw]）。
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RawVoxelPayload {
    /// mesh 块的 LOD 级别（0..=3）。
    pub lod: u8,
    /// 跨 LOD 接缝裙边掩码（6 bit，ExternalPlane 顺序；0 = 无裙边）。
    pub skirt_mask: u8,
    /// `extract_raw_halo` 的 34³ 方块 id 缓冲；长度应为 `RAW_VOXELS`。
    pub blocks: Vec<u8>,
}

impl RawVoxelPayload {
    /// 从 `extract_raw_halo` 的缓冲构造（默认无裙边）。
    pub fn from_halo(lod: u8, blocks: Vec<u8>) -> Self {
        Self::from_halo_with_skirts(lod, blocks, 0)
    }

    /// 从 `extract_raw_halo` 的缓冲 + 跨 LOD 裙边掩码构造。
    pub fn from_halo_with_skirts(lod: u8, blocks: Vec<u8>, skirt_mask: u8) -> Self {
        Self {
            lod,
            skirt_mask,
            blocks,
        }
    }

    /// 是否为空缓冲。
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

/// 一组同一 LOD 的 mesh 块原始体素缓冲（每块内部 32³ + halo，共 34³）。
///
/// 每块的字节布局与 [RawVoxelPayload] 的 `blocks` 一致
/// （`blocks.len() == RAW_VOXELS`，值 = 方块 id，0 = 空气）；元组第二项是该块的
/// 跨 LOD 裙边掩码（6 bit，ExternalPlane 顺序；0 = 无裙边）。`chunks` 的键是块的
/// 原点坐标，用 BTreeMap 保证升序遍历，于是**同一组内容无论插入顺序如何，
/// [RawVoxelGroupPayload::put_body] 的输出逐字节相同**（确定性铁律）。
///
/// `ChunkKey` 在 `crates/voxel` 未派生 serde，载荷侧改用可序列化的 `(x, y, z)`
/// 三元组键；组件 [crate::presentation::voxel_mesh::VoxChunkGroupRaw] 仍用
/// `ChunkKey`，两者经 [RawVoxelGroupPayload::from_chunks] 转换。
///
/// ## 线格式（小端，载荷体**不含** kind 码）
///
/// ```text
/// [0]              lod        u8
/// [1,5)            count      u32 LE
/// 每条记录（count 条，按 (x, y, z) 升序）：
///   x, y, z        u32 LE × 3（i32 的补码位模式）
///   byte_len       u32 LE
///   skirt_mask     u8（6 bit，ExternalPlane 顺序；0 = 无裙边）
///   bytes          byte_len 个原始体素字节
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RawVoxelGroupPayload {
    /// 组内所有块的 LOD 级别（0..=3）。
    pub lod: u8,
    /// 块原点 `(x, y, z)` -> (34³ 方块 id 缓冲, 该块跨 LOD 裙边掩码)。
    pub chunks: BTreeMap<(i32, i32, i32), (Vec<u8>, u8)>,
}

impl RawVoxelGroupPayload {
    /// 从组件的 `ChunkKey` 表构造；BTreeMap 的升序语义原样保留。
    pub fn from_chunks(lod: u8, chunks: BTreeMap<ChunkKey, (Vec<u8>, u8)>) -> Self {
        Self {
            lod,
            chunks: chunks
                .into_iter()
                .map(|(key, (blocks, mask))| ((key.x, key.y, key.z), (blocks, mask)))
                .collect(),
        }
    }

    /// 是否为空组。
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
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

    /// raw voxel group 载荷 code / 名字只追加，语义为「一组 34³ halo」。
    #[test]
    fn raw_voxel_groups_kind_is_registered_append_only() {
        assert_eq!(PayloadKind::RawVoxelGroups.code(), 9);
        assert_eq!(PayloadKind::RawVoxelGroups.as_str(), "rawvoxelgroups");
        assert_eq!(PayloadKind::from_code(9), Some(PayloadKind::RawVoxelGroups));
        assert!(!PayloadKind::RawVoxelGroups.is_perception_gated());
        let payload = RawVoxelGroupPayload::from_chunks(1, BTreeMap::new());
        assert_eq!(payload.lod, 1);
        assert!(payload.is_empty());
        assert!(RawVoxelGroupPayload::default().is_empty());
    }

    /// 0..6、8..=11 保持固定；code 7 是已删除 RectList 的遗位，必须留空不得复用。
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
            (9, "rawvoxelgroups"),
            (10, "camera"),
            (11, "inputcaps"),
        ] {
            assert_eq!(
                PayloadKind::from_code(code).map(|kind| kind.as_str()),
                Some(name)
            );
        }
    }
}
