//! `ToPresentation` 的落地实现 + 组件→载荷同步表（synced_components）。
//!
//! **不实现 `ToPresentation` 的组件 = 不下发**。AI 黑板 / FSM / BT 节点
//! 一律不实现，于是「决策层状态永不外泄」是编译期保证而不是运行期约定。
//!
//! ## 组件注册表（B 表）
//!
//! `synced_components!` 把「组件 -> 载荷」与「注册进表现管线」收成一张表：
//! 加一行就同时得到 `impl ToPresentation` 与 `register_synced_components`。
//! 转换用「调用方自带参数的闭包」表达，宏只把 `self` 交给它，避免宏卫生问题。
//!
//! **本层不改善的失败模式**：组件没写进本表就不会被同步，且**无编译错误** ——
//! 这与现状「忘了 `present::<T>()`」相同。列进表就一定注册，转换由编译器约束。

use bevy::prelude::*;

use crate::privacy::ExactHealth;

use super::payload::{
    InteractionHint, PayloadKind, PresentationState, PresentedHealth, PresentedPrototype,
    RectListPayload, SyncPayload, ToPresentation,
};
use super::voxel_mesh::VoxelMeshBlock;
use super::PresentationAppExt;
use super::PresentedTransform;
use super::RenderTransformSample;

/// 声明「哪些玩法组件同步、各自翻译成哪个载荷」，并生成注册函数。
///
/// 转换用闭包表达（调用方自带参数绑定），宏只把 `self` 交给闭包，
/// 避免宏卫生问题。
macro_rules! synced_components {
    ($( $kind:ident => $component:ty, $to:expr; )*) => {
        $(
            impl ToPresentation for $component {
                const KIND: PayloadKind = PayloadKind::$kind;

                fn to_presentation(&self) -> SyncPayload {
                    SyncPayload::$kind(($to)(self))
                }
            }
        )*

        /// 注册全部同步玩法组件（**不注册 = 不同步**）。
        ///
        /// 单一调用点：`bevy_backend.rs` 与测试都调它，不再各抄一份清单。
        pub fn register_synced_components(app: &mut bevy::prelude::App) {
            $( app.present::<$component>(); )*
            // 扩展袋：走自定义 collector（感知变化重发 + 空袋移除），不走 collect_component。
            app.init_resource::<crate::presentation::payload::ExtensionSchema>();
            // P3：Extension kind 的唯一写入者是 collect_extension_bag，注册期防重复。
            app.claim_payload_kind(
                crate::presentation::payload::PayloadKind::Extension,
                "collect_extension_bag",
            );
            app.add_systems(
                Update,
                crate::presentation::sync::collect_extension_bag
                    .in_set(game_engine::presentation::CollectPresentationSet),
            );
            // Debug 自检：schema 里 Required 字段必须声明非零 required_bits。
            #[cfg(debug_assertions)]
            app.add_systems(
                ::bevy::prelude::Startup,
                |schema: ::bevy::prelude::Res<crate::presentation::payload::ExtensionSchema>| {
                    let _ = schema.required_bits_violations();
                },
            );
        }
    };
}

synced_components! {
    // 位置用表现侧 `PresentedTransform`（插值端点，Godot 在渲染时刻采样），
    // 而不是逻辑侧 Bevy `Transform`：两者物理分离，回滚重放不污染插值状态。
    Transform => PresentedTransform,
        |c: &PresentedTransform| RenderTransformSample { prev: c.prev, curr: c.curr };
    // L5 精简表现状态。
    Presentation => PresentationState,
        |c: &PresentationState| c.clone();
    // 交互提示是**状态**：走 Add/Update/Remove，而不是会丢的事件队列。
    Interaction => InteractionHint,
        |c: &InteractionHint| *c;
    // 血量（唯一的逻辑真值，也是隐私门控载体）：注册后仍受
    // `CoreRequiredPerception` 的组件级可见性约束，判断真值与网络路径
    // 共用 `memory_path_component_visible`。逻辑侧整数 -> 线格式 f32。
    Health => ExactHealth,
        |c: &ExactHealth| PresentedHealth {
            current: c.current as f32,
            max: c.max as f32,
        };
    // 原型：普通整数组件，走 PROTOTYPE 载荷（不再由 Attach 携带）。
    Prototype => crate::static_data::prototype::Prototype,
        |c: &crate::static_data::prototype::Prototype| PresentedPrototype(c.0 .0);
    // 体素 mesh 块：一个 32³ 块的 39 bit 矩形流，走 RECTLIST 载荷（路径 B）。
    // 块原点由同一实体上的 Transform 载荷（PresentedTransform）携带。
    RectList => VoxelMeshBlock,
        |c: &VoxelMeshBlock| RectListPayload::from_stream(c.lod, &c.words);
}

// 逻辑侧 `Transform` **不直出**：真正注册的是表现侧 `PresentedTransform`
// （由 `advance_render_transforms` 每 tick 推进 prev/curr）。
