//! 表现层体素网格机制：几何生产纯机制（pack / raw / incremental / wrapped /
//! external）与 mesh 重建规划（plan）。
//!
//! 本层是**被 core 直接调用的纯函数 / 纯机制库**：不注册任何 Plugin、不
//! add_systems。世界状态（VoxelBox / DirtyChunk / VoxelDirtySet / 编辑应用）
//! 仍在世界层 [crate::voxel]；世界层不反向依赖这里。
//!
//! 会产出字节或决定顺序的容器仍只用 BTreeMap / 排序 Vec。

mod external;
mod incremental;
mod pack;
mod plan;
mod raw;
mod wrapped;

pub use external::ExternalMaskCache;
pub use incremental::{
    mesh_block_incremental, IncrementalMeshCache, DEFAULT_INCREMENTAL_CACHE_BYTES,
};
pub use pack::{
    orientation_of, pack_ao, pack_rect, pack_rect_batch, pack_rect_batch_with_ao, pack_rect_stream,
    pack_rect_stream_with_ao, pack_rect_with_ao, unpack_ao, unpack_rect, AO_BITS, AO_CORNER_BITS,
    AO_SHIFT, COL_BITS, H_BITS, MATERIAL_BITS, ORIENTATION_BITS, RECT_BITS, ROW_BITS, SLICE_BITS,
    WORD_BITS, W_BITS,
};
pub use plan::{floor_to_lod_policy, rebuild_plan, CoverPolicy, FaceMask, MeshBlockDirty};
pub use raw::{
    extract_raw_halo, mesh_raw_halo, raw_halo_has_solid_interior, raw_halo_index, RAW_DIM,
    RAW_HALO, RAW_VOXELS,
};
pub use wrapped::{mesh_block_wrapped, release_body_with_cache, WrappedBlockCache};
