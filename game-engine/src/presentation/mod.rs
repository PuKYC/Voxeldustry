//! 表现管线机制层（泛型于载荷集合 P 与事件集合 E）。
//!
//! 引擎只拥有机制：命令/帧/单槽、GPF1 打包、事件队列、插值、可见性刷新。
//! 具体载荷（define_payloads! 实例）、组件 -> 载荷翻译、原型表属于游戏侧。

pub mod command;
pub mod event;
pub mod interp;
pub mod packed;
pub mod payload;
pub mod pipeline;
/// 体素网格机制（表现层）；依赖世界层 `crate::voxel`，仅 feature = "voxel" 编译。
#[cfg(feature = "voxel")]
pub mod voxel;

use std::marker::PhantomData;

use bevy::prelude::*;

pub use command::{
    coalesce_commands, compact_commands, PresentationCommand, PresentationFrame, PresentationSlot,
    SlotFrame,
};
pub use event::{
    PresentationEventData, PresentationEventQueue, TimedPresentationEvent, EVENT_QUEUE_CAPACITY,
};
pub use interp::{
    compute_alpha, monotonic_now_ms, sample_alpha, AlphaSource, PresentedTransform,
    RenderClockState, RenderTransform, RenderTransformSample,
};
pub use packed::{
    assemble_frame, commands_from_streams, decode_frame, decode_header, decode_streams,
    encode_frame, encode_into, is_packed_frame, FrameStreams, PackedError, PackedHeader,
    PackedPayload, Reader, HEADER_LEN, PACKED_MAGIC, PACKED_VERSION, PAYLOAD_NONE,
};
pub use payload::{PayloadKindTrait, PresentationPayload};
pub use pipeline::{
    CollectPresentationSet, FinalizePresentationSet, PendingPresentation, PresentationRuntime,
    PresentationVisibility, RefreshVisibilitySet, SyncBaseline,
};

/// 引擎表现管线插件（只装与具体载荷 / 事件无关的机制）。
pub struct EnginePresentationPlugin<P: PresentationPayload, E: PresentationEventData>(
    PhantomData<(P, E)>,
);

impl<P: PresentationPayload, E: PresentationEventData> Default for EnginePresentationPlugin<P, E> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<P: PresentationPayload, E: PresentationEventData> Plugin for EnginePresentationPlugin<P, E> {
    fn build(&self, app: &mut App) {
        app.init_resource::<SyncBaseline<P>>()
            .init_resource::<PendingPresentation<P>>()
            .init_resource::<PresentationVisibility>()
            .init_resource::<PresentationRuntime>()
            .init_resource::<PresentationSlot<P>>()
            .init_resource::<PresentationEventQueue<E>>()
            .init_resource::<RenderClockState>()
            .add_systems(
                FixedPostUpdate,
                (interp::record_tick_clock, interp::advance_render_transforms).chain(),
            )
            .configure_sets(
                Update,
                (
                    RefreshVisibilitySet,
                    CollectPresentationSet,
                    FinalizePresentationSet,
                )
                    .chain(),
            )
            .configure_sets(Update, RefreshVisibilitySet.after(crate::aoi::AoiSystems))
            .add_systems(
                Update,
                pipeline::refresh_visibility.in_set(RefreshVisibilitySet),
            );
    }
}
