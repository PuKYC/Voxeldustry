//! 世界体（Body）游戏语义。
//!
//! **一岛 = 一个组件**：具体体素数据在引擎的 VoxVolume 组件里，游戏语义挂在
//! 同一实体的 Body / 标签组件上 —— 组合，而不是一个胖 VoxelBody。
//!
//! 本模块只定义类型与标签；v1 不执行 AI、不做功能方块。

use bevy::prelude::*;
use game_engine::voxel::Attachment;

use crate::static_data::voxel::BiomeId;

/// 体素世界里的一个「体」：岛 / 飞船 / 结构 / 自动机。
///
/// 具体几何（VoxVolume.chunks）由引擎侧组件携带；Body 只描述游戏语义。
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Body {
    pub kind: BodyKind,
}

/// 体的种类。结构与飞船的挂接语义由 Attachment 统一表达。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyKind {
    /// 浮岛：是否锚定（anchored）与生物群系。
    Island { anchored: bool, biome: BiomeId },
    /// 自由体素飞船（可停靠）。
    Ship,
    /// 结构：锚定到世界 / 某个体 / 自由。
    Structure { attach: Attachment },
    /// 小型可建造自动机（v1 仅空壳）。
    Automaton,
}

impl Body {
    pub const fn island(anchored: bool, biome: BiomeId) -> Self {
        Self {
            kind: BodyKind::Island { anchored, biome },
        }
    }

    pub const fn ship() -> Self {
        Self {
            kind: BodyKind::Ship,
        }
    }

    pub const fn structure(attach: Attachment) -> Self {
        Self {
            kind: BodyKind::Structure { attach },
        }
    }

    pub const fn automaton() -> Self {
        Self {
            kind: BodyKind::Automaton,
        }
    }

    pub const fn kind(&self) -> BodyKind {
        self.kind
    }

    pub const fn is_island(&self) -> bool {
        matches!(self.kind, BodyKind::Island { .. })
    }

    pub const fn is_ship(&self) -> bool {
        matches!(self.kind, BodyKind::Ship)
    }

    pub const fn is_structure(&self) -> bool {
        matches!(self.kind, BodyKind::Structure { .. })
    }

    pub const fn is_automaton(&self) -> bool {
        matches!(self.kind, BodyKind::Automaton)
    }
}

// ── 标签组件（同一实体上的第二、第三……个语义标记）──────────────────────────
//
// 标签只用于查询筛选（岛屿系统 / 飞船系统 / 结构系统 / 自动机系统各查各的），
// 不复制 BodyKind 的信息，避免两份真相源。

#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IslandTag;

#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShipTag;

#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StructureTag;

#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AutomatonTag;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_kinds_match_their_tags() {
        assert!(Body::island(true, BiomeId(0)).is_island());
        assert!(Body::ship().is_ship());
        assert!(Body::structure(Attachment::World).is_structure());
        assert!(Body::automaton().is_automaton());
    }

    #[test]
    fn structure_attachment_is_preserved() {
        let body = Body::structure(Attachment::Free);
        match body.kind {
            BodyKind::Structure { attach } => assert_eq!(attach, Attachment::Free),
            other => panic!("期望 Structure，得到 {other:?}"),
        }
    }
}
