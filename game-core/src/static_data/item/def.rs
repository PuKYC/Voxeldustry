use serde::{Deserialize, Serialize};

use crate::static_data::prototype::PrototypeId;

use super::category::ItemCategory;
use super::subcategory::ItemSubCategoryId;
use super::tag::*;

// ─────────────────────────── 物品类型 ID ───────────────────────────

/// 物品定义 ID（独立命名空间，遵循 game_engine::ids 的分区约定）。
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default, Serialize, Deserialize,
)]
pub struct ItemTypeId(pub u32);

// ─────────────────────────── 物品定义 ItemDef ───────────────────────────

/// 物品静态定义。
///
/// tags 必须按 id 升序（确定性，T5 强制）。分类属于定义，不属于实例。
#[derive(Clone, Copy, Debug)]
pub struct ItemDef {
    /// 物品类型 ID（分区 ID）。
    pub id: u32,
    /// 稳定名字（编辑期可读 + 表现层资源映射）。
    pub name: &'static str,
    /// 封闭核心品类（mod 物品也必须填核心父品类）。
    pub category: ItemCategory,
    /// 可选子品类，NONE = 无；有值时 category 必须等于其父品类（6.6 / T16）。
    pub sub_category: ItemSubCategoryId,
    /// 开放标签集合，**必须按 id 升序**（确定性，8.1）。
    pub tags: &'static [ItemTagId],
    /// 堆叠上限；1 表示不可堆叠。
    pub max_stack: u16,
    /// 作为世界实体（掉落物 / 手持）时使用的原型。
    pub world_prototype: PrototypeId,
    /// 该定义的版本，用于双端一致性校验与存档迁移。
    pub version: u32,
}

/// 核心物品表（ID 1..=999，只增不改）。
///
/// world_prototype 指向 PROTOTYPE_TABLE 的掉落物原型（T11 校验悬空引用）。
pub const ITEM_TABLE: &[ItemDef] = &[
    ItemDef {
        id: 1,
        name: "iron_ingot",
        category: ItemCategory::Material,
        sub_category: ItemSubCategoryId::NONE,
        tags: &[ITEM_TAG_STACKABLE, ITEM_TAG_DROPPABLE],
        max_stack: 64,
        world_prototype: PrototypeId(20),
        version: 1,
    },
    ItemDef {
        id: 2,
        name: "wooden_sword",
        category: ItemCategory::Weapon,
        sub_category: ItemSubCategoryId::NONE,
        tags: &[ITEM_TAG_EQUIPPABLE, ITEM_TAG_DROPPABLE],
        max_stack: 1,
        world_prototype: PrototypeId(21),
        version: 1,
    },
    ItemDef {
        id: 3,
        name: "health_potion",
        category: ItemCategory::Consumable,
        sub_category: ItemSubCategoryId::NONE,
        tags: &[ITEM_TAG_STACKABLE, ITEM_TAG_DROPPABLE, ITEM_TAG_CONSUMABLE],
        max_stack: 16,
        world_prototype: PrototypeId(22),
        version: 1,
    },
    ItemDef {
        id: 4,
        name: "torch",
        category: ItemCategory::Tool,
        sub_category: ItemSubCategoryId::NONE,
        // 6.6：stackable 标签 <=> max_stack > 1，max_stack 为权威，故此处必须带 stackable。
        tags: &[
            ITEM_TAG_STACKABLE,
            ITEM_TAG_EQUIPPABLE,
            ITEM_TAG_DROPPABLE,
            ITEM_TAG_PLACEABLE,
            ITEM_TAG_FUEL,
        ],
        max_stack: 64,
        world_prototype: PrototypeId(23),
        version: 1,
    },
    ItemDef {
        id: 5,
        name: "ancient_relic",
        category: ItemCategory::Quest,
        sub_category: ItemSubCategoryId::NONE,
        tags: &[ITEM_TAG_QUEST_ITEM, ITEM_TAG_UNIQUE],
        max_stack: 1,
        world_prototype: PrototypeId(24),
        version: 1,
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
    item_def(id).map(|def| def.name)
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
    pub name: &'static str,
    pub category: u8,
    pub category_name: &'static str,
    pub sub_category: u32,
    pub tags: Vec<u32>,
    pub max_stack: u16,
    pub prototype_id: u32,
    pub version: u32,
}

/// 物品定义表快照（逻辑层输出统一升序，Godot 只做集合判断）。
pub fn item_table_snapshot() -> Vec<ItemTableRow> {
    ITEM_TABLE
        .iter()
        .map(|def| ItemTableRow {
            item_id: def.id,
            name: def.name,
            category: def.category.as_u8(),
            category_name: def.category.as_str(),
            sub_category: def.sub_category.0,
            tags: def.tags.iter().map(|tag| tag.0).collect(),
            max_stack: def.max_stack,
            prototype_id: def.world_prototype.0,
            version: def.version,
        })
        .collect()
}
