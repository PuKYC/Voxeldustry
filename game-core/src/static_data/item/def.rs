use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::static_data::prototype::{prototype_def, PrototypeId};

use super::category::ItemCategory;
use super::subcategory::ItemSubCategoryId;
use super::tag::*;

// ─────────────────────────── 物品类型 ID ───────────────────────────

/// 物品定义 ID（独立命名空间，遵循 game_engine::ids 的分区约定）。
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default, Serialize, Deserialize,
)]
pub struct ItemTypeId(pub u32);

/// 物品放置后的目标（运行时 id 形式）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlacedObject {
    /// 核心 u8 方块调色板。
    Block(u8),
    /// 体素对象（可指向 mod 定义，但未必进入世界调色板）。
    VoxelObject(u32),
    /// 放置出来的实体原型。
    Entity(PrototypeId),
}

/// JSONC 中按稳定名字引用放置目标（加载时解析成 [PlacedObject]）。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlacedObjectRef {
    Block { name: String },
    VoxelObject { name: String },
    Entity { name: String },
}

/// JSONC 中的物品定义（owned 字段；加载后转成 [ItemDef]）。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemDefFile {
    pub id: u32,
    pub name: String,
    pub category: ItemCategory,
    #[serde(default)]
    pub sub_category: u32,
    #[serde(default)]
    pub tags: Vec<u32>,
    /// 单件质量（克）。供堆叠上限与后续负重系统使用。
    pub mass_grams: u32,
    /// 一叠物品可容纳的质量（克），与 mass_grams 共同决定堆叠数量。
    pub stack_capacity_grams: u32,
    pub world_prototype: u32,
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub places: Option<PlacedObjectRef>,
}

fn default_version() -> u32 {
    1
}

/// 由单件质量与一叠容量算出的堆叠数量；调用前必须已校验 `mass > 0` 且
/// `capacity >= mass`。
fn stack_limit(mass_grams: u32, stack_capacity_grams: u32) -> u16 {
    (stack_capacity_grams / mass_grams).min(u16::MAX as u32) as u16
}

impl ItemDefFile {
    pub fn max_stack(&self) -> u16 {
        stack_limit(self.mass_grams, self.stack_capacity_grams)
    }
}

impl crate::static_data::JsoncTable for ItemDefFile {
    type Def = Self;
    const FILE: &'static str = "item_placement.jsonc";
    const ROOT_KEY: &'static str = "items";

    fn key(def: &Self::Def) -> &str {
        &def.name
    }

    fn validate(defs: &[Self::Def]) -> Result<(), String> {
        if defs.is_empty() {
            return Err("物品定义表为空".to_string());
        }
        let mut previous_id = None;
        let mut names = std::collections::HashSet::new();
        for def in defs {
            if let Some(previous) = previous_id {
                if def.id <= previous {
                    return Err(format!(
                        "物品定义必须按 id 严格升序：{}（id {}）出现在 id {} 之后",
                        def.name, def.id, previous
                    ));
                }
            }
            previous_id = Some(def.id);
            if def.id == 0 {
                return Err("物品 id 0 保留".to_string());
            }
            if def.name.is_empty() {
                return Err(format!("id {} 的物品名字为空", def.id));
            }
            if !names.insert(def.name.as_str()) {
                return Err(format!("物品名字重复：{}", def.name));
            }
            if def.version == 0 {
                return Err(format!("{} 的 version 必须 >= 1", def.name));
            }
            if def.name.contains(':') {
                let expected = game_engine::ids::mod_hash_id(&def.name);
                if def.id != expected {
                    return Err(format!(
                        "mod 物品 {} 的 id 必须是 mod_hash_id = {}，实际 {}",
                        def.name, expected, def.id
                    ));
                }
            } else if def.id > game_engine::ids::CORE_ID_MAX {
                return Err(format!(
                    "核心物品 {} 的 id 越界：{}（非命名空间 id 必须 <= {}）",
                    def.name,
                    def.id,
                    game_engine::ids::CORE_ID_MAX
                ));
            }
            if def.mass_grams == 0 {
                return Err(format!("{} 的 mass_grams 不能为 0", def.name));
            }
            if def.stack_capacity_grams < def.mass_grams {
                return Err(format!(
                    "{} 的 stack_capacity_grams({}) 不能小于单件质量({})",
                    def.name, def.stack_capacity_grams, def.mass_grams
                ));
            }
            if u64::from(def.stack_capacity_grams) / u64::from(def.mass_grams) > u64::from(u16::MAX)
            {
                return Err(format!(
                    "{} 的堆叠数量超过 u16 上限（质量 {}，容量 {}）",
                    def.name, def.mass_grams, def.stack_capacity_grams
                ));
            }
            if def.tags.windows(2).any(|window| window[0] >= window[1]) {
                return Err(format!("{} 的 tags 必须严格升序且不重复", def.name));
            }
            for tag in &def.tags {
                if ITEM_TAG_TABLE.iter().all(|entry| entry.id != *tag) {
                    return Err(format!("{} 引用了不存在的标签 id {}", def.name, tag));
                }
            }
            let has_stackable = def.tags.contains(&ITEM_TAG_STACKABLE.0);
            if has_stackable != (def.max_stack() > 1) {
                return Err(format!(
                    "{} 的 stackable 标签与质量/容量算出的堆叠数量矛盾",
                    def.name
                ));
            }
            if def.category == ItemCategory::Quest && !def.tags.contains(&ITEM_TAG_QUEST_ITEM.0) {
                return Err(format!("{} 是 Quest 但没有 quest_item 标签", def.name));
            }
            if def.category == ItemCategory::Consumable
                && !def.tags.contains(&ITEM_TAG_CONSUMABLE.0)
            {
                return Err(format!("{} 是 Consumable 但没有 consumable 标签", def.name));
            }
            let sub_category = ItemSubCategoryId(def.sub_category);
            if sub_category != ItemSubCategoryId::NONE {
                let parent = super::subcategory::item_subcategory_parent(sub_category)
                    .ok_or_else(|| format!("{} 引用了不存在的子品类", def.name))?;
                if parent != def.category {
                    return Err(format!("{} 的 category 与子品类父品类不一致", def.name));
                }
            }
            let is_placeable = def.tags.contains(&ITEM_TAG_PLACEABLE.0);
            if is_placeable != def.places.is_some() {
                return Err(format!(
                    "{} 的 placeable 标签与 places 必须同时存在",
                    def.name
                ));
            }
            // 核心原型引用在这里能查；mod 原型留给 ObjectTables 交叉校验。
            if def.world_prototype <= game_engine::ids::CORE_ID_MAX
                && prototype_def(PrototypeId(def.world_prototype)).is_none()
            {
                return Err(format!(
                    "{} 的 world_prototype({}) 不存在",
                    def.name, def.world_prototype
                ));
            }
        }

        for core in ITEM_TABLE {
            let found = defs
                .iter()
                .find(|def| def.id == core.id)
                .ok_or_else(|| format!("缺少核心物品 id {}（{}）", core.id, core.name))?;
            if found.name != core.name {
                return Err(format!(
                    "核心物品 id {} 名字必须是 {}，实际 {}",
                    core.id, core.name, found.name
                ));
            }
        }
        Ok(())
    }
}

// ─────────────────────────── 物品定义 ItemDef ───────────────────────────

/// 物品静态定义。
///
/// tags 必须按 id 升序（确定性，T5 强制）。分类属于定义，不属于实例。
#[derive(Clone, Debug)]
pub struct ItemDef {
    /// 物品类型 ID（分区 ID）。
    pub id: u32,
    /// 稳定名字（编辑期可读 + 表现层资源映射）。
    pub name: Cow<'static, str>,
    /// 封闭核心品类（mod 物品也必须填核心父品类）。
    pub category: ItemCategory,
    /// 可选子品类，NONE = 无；有值时 category 必须等于其父品类（6.6 / T16）。
    pub sub_category: ItemSubCategoryId,
    /// 开放标签集合，**必须按 id 升序**（确定性，8.1）。
    pub tags: Cow<'static, [ItemTagId]>,
    /// 单件质量（克）。
    pub mass_grams: u32,
    /// 一叠物品可容纳的质量（克）。
    pub stack_capacity_grams: u32,
    /// 作为世界实体（掉落物 / 手持）时使用的原型。
    pub world_prototype: PrototypeId,
    /// 该定义的版本，用于双端一致性校验与存档迁移。
    pub version: u32,
    /// 放下后变成什么；只有 placeable 物品才有。
    pub places: Option<PlacedObject>,
}

impl ItemDef {
    /// 由单件质量与一叠容量算出的堆叠上限；1 表示不可堆叠。
    pub fn max_stack(&self) -> u16 {
        stack_limit(self.mass_grams, self.stack_capacity_grams)
    }
}

/// 核心物品表（ID 1..=999，只增不改）。
///
/// world_prototype 指向 PROTOTYPE_TABLE 的掉落物原型（T11 校验悬空引用）。
pub const ITEM_TABLE: &[ItemDef] = &[
    ItemDef {
        id: 1,
        name: Cow::Borrowed("iron_ingot"),
        category: ItemCategory::Material,
        sub_category: ItemSubCategoryId::NONE,
        tags: Cow::Borrowed(&[ITEM_TAG_STACKABLE, ITEM_TAG_DROPPABLE]),
        mass_grams: 100,
        stack_capacity_grams: 6_400,
        world_prototype: PrototypeId(20),
        version: 1,
        places: None,
    },
    ItemDef {
        id: 2,
        name: Cow::Borrowed("wooden_sword"),
        category: ItemCategory::Weapon,
        sub_category: ItemSubCategoryId::NONE,
        tags: Cow::Borrowed(&[ITEM_TAG_EQUIPPABLE, ITEM_TAG_DROPPABLE]),
        mass_grams: 1_500,
        stack_capacity_grams: 1_500,
        world_prototype: PrototypeId(21),
        version: 1,
        places: None,
    },
    ItemDef {
        id: 3,
        name: Cow::Borrowed("health_potion"),
        category: ItemCategory::Consumable,
        sub_category: ItemSubCategoryId::NONE,
        tags: Cow::Borrowed(&[ITEM_TAG_STACKABLE, ITEM_TAG_DROPPABLE, ITEM_TAG_CONSUMABLE]),
        mass_grams: 250,
        stack_capacity_grams: 4_000,
        world_prototype: PrototypeId(22),
        version: 1,
        places: None,
    },
    ItemDef {
        id: 4,
        name: Cow::Borrowed("torch"),
        category: ItemCategory::Tool,
        sub_category: ItemSubCategoryId::NONE,
        // 6.6：stackable 标签 <=> 按质量/容量算出的堆叠数量 > 1。
        tags: Cow::Borrowed(&[
            ITEM_TAG_STACKABLE,
            ITEM_TAG_EQUIPPABLE,
            ITEM_TAG_DROPPABLE,
            ITEM_TAG_PLACEABLE,
            ITEM_TAG_FUEL,
        ]),
        mass_grams: 50,
        stack_capacity_grams: 3_200,
        world_prototype: PrototypeId(23),
        version: 1,
        places: Some(PlacedObject::Entity(PrototypeId(30))),
    },
    ItemDef {
        id: 5,
        name: Cow::Borrowed("ancient_relic"),
        category: ItemCategory::Quest,
        sub_category: ItemSubCategoryId::NONE,
        tags: Cow::Borrowed(&[ITEM_TAG_QUEST_ITEM, ITEM_TAG_UNIQUE]),
        mass_grams: 5_000,
        stack_capacity_grams: 5_000,
        world_prototype: PrototypeId(24),
        version: 1,
        places: None,
    },
];

/// 按 ID 查定义（表按 id 升序，二分查找）。未知返回 None，不 panic。
pub fn item_def(id: ItemTypeId) -> Option<&'static ItemDef> {
    ITEM_TABLE
        .binary_search_by_key(&id.0, |def| def.id)
        .ok()
        .map(|index| &ITEM_TABLE[index])
}

pub fn item_name(id: ItemTypeId) -> Option<&'static str> {
    item_def(id).map(|def| def.name.as_ref())
}

/// 该物品定义是否带某标签（tags 升序，二分查找）。未定义物品视为不带任何标签。
pub fn item_has_tag(id: ItemTypeId, tag: ItemTagId) -> bool {
    item_def(id)
        .map(|def| def.tags.binary_search(&tag).is_ok())
        .unwrap_or(false)
}

/// FFI 用的物品定义行（纯数据，无行为）。
#[derive(Clone, Debug)]
pub struct ItemTableRow {
    pub item_id: u32,
    pub name: Cow<'static, str>,
    pub category: u8,
    pub category_name: &'static str,
    pub sub_category: u32,
    pub tags: Vec<u32>,
    /// 由质量/容量算出的兼容字段（Godot 仍按这个键读堆叠数量）。
    pub max_stack: u16,
    pub mass_grams: u32,
    pub stack_capacity_grams: u32,
    pub prototype_id: u32,
    pub version: u32,
}

/// 物品定义表快照（逻辑层输出统一升序，Godot 只做集合判断）。
pub fn item_table_snapshot() -> Vec<ItemTableRow> {
    ITEM_TABLE
        .iter()
        .map(|def| ItemTableRow {
            item_id: def.id,
            name: def.name.clone(),
            category: def.category.as_u8(),
            category_name: def.category.as_str(),
            sub_category: def.sub_category.0,
            tags: def.tags.iter().map(|tag| tag.0).collect(),
            max_stack: def.max_stack(),
            mass_grams: def.mass_grams,
            stack_capacity_grams: def.stack_capacity_grams,
            prototype_id: def.world_prototype.0,
            version: def.version,
        })
        .collect()
}
