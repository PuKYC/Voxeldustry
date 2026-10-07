//! 表现通道（game-core -> Godot）。
//!
//! 机制（命令/帧/单槽、GPF1、事件队列、插值、可见性刷新）在 game-engine；
//! 本模块提供具体载荷（payload）、组件→载荷同步表（synced_components）、
//! 语义表（semantics）与收集/发布胶水（sync）。
//!
//! 原型表（`Prototype` 等）属于 `static_data`，不在本模块重导出；
//! 需要的调用方请直接 `use crate::static_data::prototype::*`。

pub mod event;
mod synced_components;
pub use synced_components::register_synced_components;
pub mod payload;
pub mod semantics;
pub mod sync;
// 体素 mesh 块的矩形流组件 + rect -> 四边形几何契约（路径 B）。
#[cfg(test)]
mod tests;
pub mod voxel_mesh;

// 引擎机制模块重导出：godot-client-ext 只依赖 game-core，需要 presentation::packed
// 这个模块路径；直接重导出引擎模块，不再放一份 shim 文件。
// 这是**面向 FFI 的有意重导出**，供 Godot 扩展经 `game_core::presentation::*`
// 访问引擎机制，请勿当作冗余 import 清理。
// （`interp` 目前没有调用方走模块路径访问，具体类型走下面的顶层重导出即可，
// 不再额外重导出 `interp` 模块本身。）
pub use game_engine::presentation::packed;

use bevy::prelude::*;
use game_engine::presentation::{
    CollectPresentationSet, EnginePresentationPlugin, FinalizePresentationSet,
};

// game-core 对泛型机制的具化别名（CoreSpec::Payload = SyncPayload）。
pub use game_engine::presentation::command::{coalesce_commands, compact_commands};
pub type PresentationCommand = game_engine::presentation::command::PresentationCommand<SyncPayload>;
pub type PresentationFrame = game_engine::presentation::command::PresentationFrame<SyncPayload>;
pub type PresentationSlot = game_engine::presentation::command::PresentationSlot<SyncPayload>;
pub type SlotFrame = game_engine::presentation::command::SlotFrame<SyncPayload>;

pub use event::{
    AnimId, PresentationEvent, PresentationEventQueue, SoundId, TimedPresentationEvent, VfxId,
    EVENT_QUEUE_CAPACITY,
};
// sample_alpha / monotonic_now_ms 等被 godot-client-ext 直接调用，必须保持 pub。
pub use game_engine::presentation::interp::{
    compute_alpha, monotonic_now_ms, sample_alpha, AlphaSource,
};
// 这几个目前只有 crate 内部（synced_components.rs、tests/mod.rs、tests/packed.rs）在用，收窄为 pub(crate)。
pub(crate) use game_engine::presentation::interp::{PresentedTransform, RenderTransformSample};
pub use game_engine::presentation::pipeline::{PresentationRuntime, PresentationVisibility};
pub use payload::{
    project, ExtError, ExtField, ExtValue, ExtensionBag, ExtensionFieldSchema, ExtensionPayload,
    ExtensionSchema, FieldVisibility, InteractionHint, PayloadKind, PresentationState,
    PresentedHealth, PresentedPrototype, PresentedVisibility, SetOutcome, SyncPayload,
    ToPresentation,
};
pub use semantics::{
    action_state_name, anim_name, domain_table, is_core_domain, locomotion_name, mod_hash_id,
    overlay_tag_name, partition_name, partition_of, sound_name, table_snapshot, vfx_name,
    IdPartition, SemanticEntry, SemanticRegistry, ACTION_STATE_TABLE, ANIM_TABLE, LOCOMOTION_TABLE,
    OVERLAY_TAG_TABLE, SEMANTIC_DOMAINS, SOUND_TABLE, VFX_TABLE,
};
pub use sync::{CorePendingPresentation, CoreSyncBaseline};
pub use voxel_mesh::{rect_corner, rect_normal, rect_scale_meters, VoxelMeshBlock};

/// 表现管线插件（引擎机制 + game-core 的收集/发布胶水）。
pub struct PresentationPlugin;

impl Plugin for PresentationPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(EnginePresentationPlugin::<SyncPayload, PresentationEvent>::default());
        // reconcile 先跑：回收「基线仍记着 Extension 但实体已无袋」的残留，
        // 把 Remove 交回 pending；finalize 后跑并统一整理 / 发布。
        // 首次 Attach 前的映射缺失由 reconcile 跳过（交给 finalize 补映射）。
        app.add_systems(
            Update,
            (
                sync::reconcile_extension_baseline,
                sync::finalize_presentation,
            )
                .chain()
                .in_set(FinalizePresentationSet),
        );
    }
}

/// 每个 PayloadKind 的写入者登记表。
///
/// 不变量：一个载荷 kind 只能有一个写入者 / collector。一旦同一 (id, kind) 出现
/// 两个写入者，compact_commands 的稳定排序就会在相等键上回退到调度 / 哈希顺序，
/// 使 GPF1 字节跨进程不可复现。这里把该不变量变成注册期硬约束（debug / release 都生效）。
#[derive(Resource, Default, Debug)]
pub struct PayloadKindOwners {
    owners: std::collections::HashMap<u8, &'static str>,
}

impl PayloadKindOwners {
    /// 登记一个写入者；同一 kind 换写入者即 panic。
    pub fn claim(&mut self, kind: PayloadKind, owner: &'static str) {
        match self.owners.get(&kind.code()) {
            Some(existing) if *existing != owner => panic!(
                "载荷 kind {} (code={}) 已有写入者 {}，又注册了 {}；同一 kind 的多个写入者会让 compact 排序泄漏调度序，GPF1 不再可复现。",
                kind.as_str(),
                kind.code(),
                existing,
                owner
            ),
            _ => {
                self.owners.insert(kind.code(), owner);
            }
        }
    }
}

/// 注册组件进入表现管线。
pub trait PresentationAppExt {
    /// 注册一个组件类型；它必须实现 ToPresentation。
    ///
    /// 推荐改用 register_synced_components。
    fn present<T: ToPresentation>(&mut self) -> &mut Self;

    /// 为非 present::<T>() 的自定义 collector 声明 kind 所有权。
    fn claim_payload_kind(&mut self, kind: PayloadKind, owner: &'static str) -> &mut Self;
}

impl PresentationAppExt for App {
    fn present<T: ToPresentation>(&mut self) -> &mut Self {
        self.init_resource::<PayloadKindOwners>();
        self.world_mut()
            .resource_mut::<PayloadKindOwners>()
            .claim(T::KIND, std::any::type_name::<T>());
        self.add_systems(
            Update,
            sync::collect_component::<T>.in_set(CollectPresentationSet),
        );
        self
    }

    fn claim_payload_kind(&mut self, kind: PayloadKind, owner: &'static str) -> &mut Self {
        self.init_resource::<PayloadKindOwners>();
        self.world_mut()
            .resource_mut::<PayloadKindOwners>()
            .claim(kind, owner);
        self
    }
}
