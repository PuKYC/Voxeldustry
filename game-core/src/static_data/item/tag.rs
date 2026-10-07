use serde::{Deserialize, Serialize};

use game_engine::ids::{partition_name, partition_of, IdEntry};

use super::SOURCE_CORE;

// ─────────────────────────── 标签 Tag ───────────────────────────

/// 开放标签 ID（独立命名空间，遵循分区约定）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct ItemTagId(pub u32);

/// 核心标签表（ID <= 999）。名字只用于表现层。
///
/// 1000..=9999 留给 mod 显式分配；10000+ 用 game_engine::ids::mod_hash_id("mymod:xxx")。
pub const ITEM_TAG_TABLE: &[IdEntry] = &[
    IdEntry {
        id: 1,
        name: "stackable",
    },
    IdEntry {
        id: 2,
        name: "equippable",
    },
    IdEntry {
        id: 3,
        name: "droppable",
    },
    IdEntry {
        id: 4,
        name: "consumable",
    },
    IdEntry {
        id: 5,
        name: "flammable",
    },
    IdEntry {
        id: 6,
        name: "quest_item",
    },
    IdEntry {
        id: 7,
        name: "two_handed",
    },
    IdEntry {
        id: 8,
        name: "magic",
    },
    IdEntry {
        id: 9,
        name: "throwable",
    },
    IdEntry {
        id: 10,
        name: "placeable",
    },
    IdEntry {
        id: 11,
        name: "fuel",
    },
    IdEntry {
        id: 12,
        name: "ingredient",
    },
    IdEntry {
        id: 13,
        name: "indestructible",
    },
    IdEntry {
        id: 14,
        name: "unique",
    },
];

/// 全部核心标签的编译期常量（避免在业务里写裸数字），与 [ITEM_TAG_TABLE] 一一对应，T14 校验。
pub const ITEM_TAG_STACKABLE: ItemTagId = ItemTagId(1);
pub const ITEM_TAG_EQUIPPABLE: ItemTagId = ItemTagId(2);
pub const ITEM_TAG_DROPPABLE: ItemTagId = ItemTagId(3);
pub const ITEM_TAG_CONSUMABLE: ItemTagId = ItemTagId(4);
pub const ITEM_TAG_FLAMMABLE: ItemTagId = ItemTagId(5);
pub const ITEM_TAG_QUEST_ITEM: ItemTagId = ItemTagId(6);
pub const ITEM_TAG_TWO_HANDED: ItemTagId = ItemTagId(7);
pub const ITEM_TAG_MAGIC: ItemTagId = ItemTagId(8);
pub const ITEM_TAG_THROWABLE: ItemTagId = ItemTagId(9);
pub const ITEM_TAG_PLACEABLE: ItemTagId = ItemTagId(10);
pub const ITEM_TAG_FUEL: ItemTagId = ItemTagId(11);
pub const ITEM_TAG_INGREDIENT: ItemTagId = ItemTagId(12);
pub const ITEM_TAG_INDESTRUCTIBLE: ItemTagId = ItemTagId(13);
pub const ITEM_TAG_UNIQUE: ItemTagId = ItemTagId(14);

/// ID -> 名字；未知返回 None（前向兼容）。
pub fn item_tag_name(id: ItemTagId) -> Option<&'static str> {
    ITEM_TAG_TABLE
        .iter()
        .find(|entry| entry.id == id.0)
        .map(|entry| entry.name)
}

/// 标签表快照：(id, name, partition, source)。
pub fn item_tag_table_snapshot() -> Vec<(u32, &'static str, &'static str, &'static str)> {
    ITEM_TAG_TABLE
        .iter()
        .map(|entry| {
            (
                entry.id,
                entry.name,
                partition_name(partition_of(entry.id)),
                SOURCE_CORE,
            )
        })
        .collect()
}
