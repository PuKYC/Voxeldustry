//! 静态原型表。
//!
//! `Attach` 只带 id；原型走普通 `PROTOTYPE` 载荷（不再挂在 Attach 上）。Godot 用
//! `prototype_id` 去查自己的 `name → PackedScene` 表 —— **名字由 `game-core` 决定，
//! `.tscn` 路径由 Godot 决定**。
//!
//! `version` 用于双端一致性校验：版本不匹配时必须拒绝或强制拉取最新配置，
//! 否则「模板 + 差异」的差异基线就不一致了。
//!
//! **落位**：本模块属于逻辑层静态数据，表现层只**消费**类型、不**拥有**类型。

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

/// 静态原型 ID（独立命名空间，遵循 `game_engine::ids` 的分区约定）。
///
/// **必须带 `Ord`**：`binary_search` 与排序构建都依赖它。
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default, Serialize, Deserialize,
)]
pub struct PrototypeId(pub u32);

#[derive(Clone, Copy, Debug)]
pub struct PrototypeDef {
    pub id: u32,
    pub name: &'static str,
    pub version: u32,
}

/// 原型表。
///
/// **这是静态配置，不是每实例状态**：100 个同类野怪共享同一份默认值，
/// `Attach` 只需要传「跟默认值不同的部分」。
///
/// - `1..=3`：核心实体原型；
/// - `20..=24`：`static_data::item` 的掉落物 / 手持原型（`ItemDef::world_prototype`）。
///   **物品 -> 原型是单向引用**：实体原型不需要知道自己是哪种物品。
pub const PROTOTYPE_TABLE: &[PrototypeDef] = &[
    PrototypeDef {
        id: 1,
        name: "player",
        version: 1,
    },
    PrototypeDef {
        id: 2,
        name: "drifter",
        version: 1,
    },
    PrototypeDef {
        id: 3,
        name: "gated_drifter",
        version: 1,
    },
    // ── 物品掉落物 / 手持原型（与 ITEM_TABLE.world_prototype 对齐，T11 校验）──
    PrototypeDef {
        id: 20,
        name: "iron_ingot_drop",
        version: 1,
    },
    PrototypeDef {
        id: 21,
        name: "wooden_sword_drop",
        version: 1,
    },
    PrototypeDef {
        id: 22,
        name: "health_potion_drop",
        version: 1,
    },
    PrototypeDef {
        id: 23,
        name: "torch_drop",
        version: 1,
    },
    PrototypeDef {
        id: 24,
        name: "ancient_relic_drop",
        version: 1,
    },
];

/// 挂在实体上的原型引用。
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Prototype(pub PrototypeId);

impl Prototype {
    pub fn new(id: u32) -> Self {
        Self(PrototypeId(id))
    }
}

pub fn prototype_def(id: PrototypeId) -> Option<&'static PrototypeDef> {
    PROTOTYPE_TABLE.iter().find(|def| def.id == id.0)
}

pub fn prototype_name(id: PrototypeId) -> Option<&'static str> {
    prototype_def(id).map(|def| def.name)
}

/// 供 Godot 启动时拉取并校验自己的场景表。
///
/// 返回 `(prototype_id, name, version)`。
pub fn prototype_table_snapshot() -> Vec<(u32, &'static str, u32)> {
    PROTOTYPE_TABLE
        .iter()
        .map(|def| (def.id, def.name, def.version))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T3：原型 id / 名字唯一。
    #[test]
    fn prototype_ids_and_names_are_unique() {
        let mut ids: Vec<u32> = PROTOTYPE_TABLE.iter().map(|d| d.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len());

        let mut names: Vec<&str> = PROTOTYPE_TABLE.iter().map(|d| d.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len());
    }

    #[test]
    fn snapshot_roundtrips() {
        assert_eq!(prototype_name(PrototypeId(1)), Some("player"));
        assert!(prototype_name(PrototypeId(999)).is_none());
        assert_eq!(prototype_table_snapshot().len(), PROTOTYPE_TABLE.len());
    }
}
