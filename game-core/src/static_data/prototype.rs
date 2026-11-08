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
    /// 放置规格（数据表可覆盖；核心常量给默认值）。
    pub placement: PlacementSpec,
}

/// 实体落位锚点。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementAnchor {
    /// 脚底贴地（默认）。
    #[default]
    Feet,
    /// 包围盒中心。
    Center,
    /// 占据一个体素（火把等薄物体）。
    Voxel,
}

/// 实体放置规格（方块面 / 地面）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementSpec {
    #[serde(default)]
    pub anchor: PlacementAnchor,
    /// 占位 footprint（体素，三轴均 >= 1）。
    #[serde(default = "unit_footprint")]
    pub footprint: [u8; 3],
    /// 相对锚点的偏移（体素）。
    #[serde(default)]
    pub offset: [i32; 3],
    /// 是否必须有支撑面。
    #[serde(default)]
    pub requires_support: bool,
}

fn unit_footprint() -> [u8; 3] {
    [1, 1, 1]
}

impl Default for PlacementSpec {
    fn default() -> Self {
        Self {
            anchor: PlacementAnchor::Feet,
            footprint: unit_footprint(),
            offset: [0, 0, 0],
            requires_support: false,
        }
    }
}

impl PlacementSpec {
    pub const DEFAULT: Self = Self {
        anchor: PlacementAnchor::Feet,
        footprint: [1, 1, 1],
        offset: [0, 0, 0],
        requires_support: false,
    };

    fn validate(&self) -> Result<(), String> {
        if self.footprint.iter().any(|side| *side == 0) {
            return Err("placement.footprint 的三个分量都必须 >= 1".to_string());
        }
        if self.offset.iter().any(|value| value.unsigned_abs() > 1024) {
            return Err("placement.offset 超出 ±1024 体素".to_string());
        }
        Ok(())
    }
}

/// JSONC 中的实体对象定义。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityObjectDef {
    pub id: u32,
    pub name: String,
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub placement: PlacementSpec,
}

fn default_version() -> u32 {
    1
}

impl super::JsoncTable for EntityObjectDef {
    type Def = Self;
    const FILE: &'static str = "entity_objects.jsonc";
    const ROOT_KEY: &'static str = "prototypes";

    fn key(def: &Self::Def) -> &str {
        &def.name
    }

    fn validate(defs: &[Self::Def]) -> Result<(), String> {
        if defs.is_empty() {
            return Err("实体对象表为空".to_string());
        }
        let mut previous_id = None;
        let mut names = std::collections::HashSet::new();
        for def in defs {
            if let Some(previous) = previous_id {
                if def.id <= previous {
                    return Err(format!(
                        "实体对象必须按 id 严格升序：{}（id {}）出现在 id {} 之后",
                        def.name, def.id, previous
                    ));
                }
            }
            previous_id = Some(def.id);
            if def.id == 0 {
                return Err("实体对象 id 0 保留".to_string());
            }
            if def.name.is_empty() {
                return Err(format!("id {} 的实体名字为空", def.id));
            }
            if !names.insert(def.name.as_str()) {
                return Err(format!("实体名字重复：{}", def.name));
            }
            if def.version == 0 {
                return Err(format!("{} 的 version 必须 >= 1", def.name));
            }
            if def.name.contains(':') {
                let expected = game_engine::ids::mod_hash_id(&def.name);
                if def.id != expected {
                    return Err(format!(
                        "mod 实体 {} 的 id 必须是 mod_hash_id = {}，实际 {}",
                        def.name, expected, def.id
                    ));
                }
            } else if def.id > game_engine::ids::CORE_ID_MAX {
                return Err(format!(
                    "核心实体 {} 的 id 越界：{}（非命名空间 id 必须 <= {}）",
                    def.name,
                    def.id,
                    game_engine::ids::CORE_ID_MAX
                ));
            }
            def.placement
                .validate()
                .map_err(|error| format!("{}: {error}", def.name))?;
        }

        for prototype in PROTOTYPE_TABLE {
            let found = defs
                .iter()
                .find(|def| def.id == prototype.id)
                .ok_or_else(|| format!("缺少核心实体 id {}（{}）", prototype.id, prototype.name))?;
            if found.name != prototype.name {
                return Err(format!(
                    "核心实体 id {} 名字必须是 {}，实际 {}",
                    prototype.id, prototype.name, found.name
                ));
            }
        }
        Ok(())
    }
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
        placement: PlacementSpec::DEFAULT,
    },
    PrototypeDef {
        id: 2,
        name: "drifter",
        version: 1,
        placement: PlacementSpec::DEFAULT,
    },
    PrototypeDef {
        id: 3,
        name: "gated_drifter",
        version: 1,
        placement: PlacementSpec::DEFAULT,
    },
    // ── 物品掉落物 / 手持原型（与 ITEM_TABLE.world_prototype 对齐，T11 校验）──
    PrototypeDef {
        id: 20,
        name: "iron_ingot_drop",
        version: 1,
        placement: PlacementSpec::DEFAULT,
    },
    PrototypeDef {
        id: 21,
        name: "wooden_sword_drop",
        version: 1,
        placement: PlacementSpec::DEFAULT,
    },
    PrototypeDef {
        id: 22,
        name: "health_potion_drop",
        version: 1,
        placement: PlacementSpec::DEFAULT,
    },
    PrototypeDef {
        id: 23,
        name: "torch_drop",
        version: 1,
        placement: PlacementSpec::DEFAULT,
    },
    PrototypeDef {
        id: 24,
        name: "ancient_relic_drop",
        version: 1,
        placement: PlacementSpec::DEFAULT,
    },
    // ── 放置出来的实体（物品 places 指向这里）────────────────────────────
    PrototypeDef {
        id: 30,
        name: "torch_placed",
        version: 1,
        placement: PlacementSpec {
            anchor: PlacementAnchor::Voxel,
            footprint: [1, 1, 1],
            offset: [0, 0, 0],
            requires_support: true,
        },
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
