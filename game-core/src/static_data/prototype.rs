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

        for &(id, name) in CORE_ENTITIES {
            let found = defs
                .iter()
                .find(|def| def.id == id)
                .ok_or_else(|| format!("缺少核心实体 id {id}（{name}）"))?;
            if found.name != name {
                return Err(format!(
                    "核心实体 id {id} 名字必须是 {name}，实际 {}",
                    found.name
                ));
            }
        }
        Ok(())
    }
}

/// 玩家实体原型（`player`）。
pub const PROTOTYPE_PLAYER: u32 = 1;

/// 游荡实体原型（`drifter`）。
pub const PROTOTYPE_DRIFTER: u32 = 2;

/// 带门控的游荡实体原型（`gated_drifter`）。
pub const PROTOTYPE_GATED_DRIFTER: u32 = 3;

/// 游戏运行**必须存在**的核心实体契约：`(id, name)`。
///
/// 只收 Rust 代码里直接按 id 引用的实体，且 id 一律引用上面的常量
/// （`dev/spawn.rs` 用 [`PROTOTYPE_PLAYER`]、`dev/perf.rs` 用
/// [`PROTOTYPE_DRIFTER`]、`presentation/sync.rs` 用 [`PROTOTYPE_GATED_DRIFTER`]）。
/// `version` / `placement` 以及掉落物、放置物等其它实体都只存在于
/// `entity_objects.jsonc`（及其 mod 补丁层），物品对实体的引用由
/// `ObjectTables::from_parts` 的跨表校验负责，不在这里重复登记。
pub const CORE_ENTITIES: &[(u32, &str)] = &[
    (PROTOTYPE_PLAYER, "player"),
    (PROTOTYPE_DRIFTER, "drifter"),
    (PROTOTYPE_GATED_DRIFTER, "gated_drifter"),
];

/// 挂在实体上的原型引用。
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct Prototype(pub PrototypeId);

impl Prototype {
    pub fn new(id: u32) -> Self {
        Self(PrototypeId(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 核心契约本身：id / 名字唯一，且 id 落在核心区间。
    #[test]
    fn core_entities_are_unique_and_in_core_range() {
        let mut ids: Vec<u32> = CORE_ENTITIES.iter().map(|(id, _)| *id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len());
        assert!(ids
            .iter()
            .all(|id| *id != 0 && *id <= game_engine::ids::CORE_ID_MAX));

        let mut names: Vec<&str> = CORE_ENTITIES.iter().map(|(_, name)| *name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len());
    }

    /// 内嵌的 entity_objects.jsonc 必须满足核心契约（走完整加载流水线）。
    #[test]
    fn embedded_entity_table_satisfies_core_contract() {
        let tables =
            crate::static_data::load_object_tables(&Default::default(), &Default::default(), 1)
                .expect("embedded tables load");
        for &(id, name) in CORE_ENTITIES {
            let def = tables.entity(PrototypeId(id)).expect("核心实体存在");
            assert_eq!(def.name, name);
        }
    }

    /// 用户/mod 改掉或删掉核心实体时，校验必须拒绝。
    #[test]
    fn validate_rejects_missing_or_renamed_core_entity() {
        use crate::static_data::JsoncTable;

        let full: Vec<EntityObjectDef> = CORE_ENTITIES
            .iter()
            .map(|&(id, name)| EntityObjectDef {
                id,
                name: name.to_string(),
                version: 1,
                placement: PlacementSpec::default(),
            })
            .collect();
        assert!(EntityObjectDef::validate(&full).is_ok());

        let missing: Vec<EntityObjectDef> = full.iter().skip(1).cloned().collect();
        assert!(EntityObjectDef::validate(&missing)
            .unwrap_err()
            .contains("缺少核心实体"));

        let mut renamed = full.clone();
        renamed[0].name = "hero".to_string();
        assert!(EntityObjectDef::validate(&renamed)
            .unwrap_err()
            .contains("名字必须是"));
    }
}
