//! 体素流式插件的核心类型与生成状态机。
//!
//! 本模块只做**数据模型**：生成状态（`GenState` / `WorkState` / `GenTable`）、
//! 区块存档记录（`ChunkRecord`）、读档查询（`StoreLookup` / `ChunkStore`）。
//! 不做系统、不注册 `Plugin`、不碰渲染 / 网格 / 体素存储。
//!
//! 权威文档：
//!
//! - 《体素流式_实现文档》§4（生成状态）、§6（状态机与裁决）、§7（读档接口）；
//! - 《存档格式规格》§3（`ChunkRecord` 字段与语义）、§7（`lookup` 映射）。
//!
//! 确定性硬约束：表与集合一律 `BTreeMap` / `BTreeSet`（本模块只有 `BTreeMap`），
//! 判定路径没有 f32/f64，也没有 `HashMap`。
//!
//! 编译门：整个模块由 `#[cfg(feature = "voxel")]` 门控（`ChunkKey` 来自可选依赖 `voxel`）。

pub mod gen_table;
pub mod types;

pub use gen_table::{GenError, GenState, GenTable, Slot, WorkState};
pub use types::{
    ChunkRecord, ChunkStore, EntityRecord, NoStore, ProposalRecord, RecordError, StoreLookup,
    MAX_PROPOSAL_EXTENT,
};
