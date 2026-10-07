use super::*;
use crate::static_data::prototype::prototype_def;
use game_engine::ids::{detect_id_collisions, CORE_ID_MAX};
use game_engine::ids::{partition_of, IdEntry};

/// T4：物品 id / 名字唯一，且表按 id 升序（item_def 二分查找的前提）。
#[test]
fn item_ids_and_names_are_unique() {
    let mut ids: Vec<u32> = ITEM_TABLE.iter().map(|d| d.id).collect();
    for window in ids.windows(2) {
        assert!(window[0] < window[1], "ITEM_TABLE 必须按 id 升序");
    }
    let before = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(before, ids.len(), "物品 id 重复");

    let mut names: Vec<&str> = ITEM_TABLE.iter().map(|d| d.name).collect();
    let before = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(before, names.len(), "物品名字重复");
}

/// T5：每个 ItemDef.tags 严格升序、无重复。
#[test]
fn item_tags_are_sorted_ascending() {
    for def in ITEM_TABLE {
        for window in def.tags.windows(2) {
            assert!(
                window[0] < window[1],
                "{} 的 tags 未严格升序：{:?}",
                def.name,
                def.tags
            );
        }
    }
}

/// 核心标签表 id / 名字唯一、id <= 999、分区为 core。
#[test]
fn item_tag_ids_and_names_are_unique() {
    assert!(detect_id_collisions(ITEM_TAG_TABLE).is_empty());
    let mut names: Vec<&str> = ITEM_TAG_TABLE.iter().map(|e| e.name).collect();
    let before = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(before, names.len(), "标签名字重复");
    for entry in ITEM_TAG_TABLE {
        assert!(entry.id <= CORE_ID_MAX, "核心标签 id 越界：{}", entry.id);
        assert_eq!(partition_of(entry.id), game_engine::ids::IdPartition::Core);
    }
}

/// T7：品类封闭、repr(u8) 往返、serde 走整数、未知整数落 Other。
#[test]
fn category_is_closed_and_roundtrips() {
    let mut seen: Vec<u8> = Vec::new();
    for category in ItemCategory::ALL {
        let raw = category.as_u8();
        assert!(!seen.contains(&raw), "品类值重复：{raw}");
        seen.push(raw);
        assert_eq!(ItemCategory::from_u8(raw), category, "from_u8 往返失败");
        assert!(!category.as_str().is_empty());
        let json = serde_json::to_string(&category).unwrap();
        assert_eq!(json, raw.to_string(), "serde 必须走整数");
        let back: ItemCategory = serde_json::from_str(&json).unwrap();
        assert_eq!(back, category);
    }
    // 未知整数不报错、落 Other。
    assert_eq!(ItemCategory::from_u8(42), ItemCategory::Other);
    assert_eq!(ItemCategory::from_u8(255), ItemCategory::Other);
    let unknown: ItemCategory = serde_json::from_str("42").unwrap();
    assert_eq!(unknown, ItemCategory::Other);
    assert_eq!(ItemCategory::from_u8(11), ItemCategory::Other);
}

/// T8：未定义 id 返回 None，不 panic。
#[test]
fn unknown_item_lookup_is_none_not_panic() {
    assert!(item_def(ItemTypeId(0)).is_none());
    assert!(item_def(ItemTypeId(999)).is_none());
    assert!(item_name(ItemTypeId(999)).is_none());
    assert!(!item_has_tag(ItemTypeId(999), ITEM_TAG_DROPPABLE));
}

/// T9：标签命中 / 未命中正确（二分查找）。
#[test]
fn item_has_tag_uses_binary_search() {
    assert!(item_has_tag(ItemTypeId(1), ITEM_TAG_STACKABLE));
    assert!(item_has_tag(ItemTypeId(1), ITEM_TAG_DROPPABLE));
    assert!(!item_has_tag(ItemTypeId(1), ITEM_TAG_EQUIPPABLE));
    assert!(!item_has_tag(ItemTypeId(1), ItemTagId(9999)));
}

/// T10：同样输入构建两次，items_with 结果逐位一致且升序。
#[test]
fn tag_index_build_is_deterministic() {
    let a = ItemTagIndex::build(ITEM_TABLE);
    let b = ItemTagIndex::build(ITEM_TABLE);
    for def in ITEM_TABLE {
        for tag in def.tags {
            assert_eq!(a.items_with(*tag), b.items_with(*tag));
            let items = a.items_with(*tag);
            for window in items.windows(2) {
                assert!(window[0] < window[1], "索引结果必须升序");
            }
        }
    }
}

/// T11：每个 world_prototype 都能查到原型（防悬空引用）。
#[test]
fn world_prototype_references_exist() {
    for def in ITEM_TABLE {
        assert!(
            prototype_def(def.world_prototype).is_some(),
            "{} 的 world_prototype({}) 在 PROTOTYPE_TABLE 中不存在",
            def.name,
            def.world_prototype.0
        );
    }
}

/// T12：ID 与 ItemStack serde 往返一致。
#[test]
fn serde_roundtrip_ids_and_stack() {
    let raw = ItemTypeId(3);
    assert_eq!(
        serde_json::from_str::<ItemTypeId>(&serde_json::to_string(&raw).unwrap()).unwrap(),
        raw
    );
    let tag = ITEM_TAG_FLAMMABLE;
    assert_eq!(
        serde_json::from_str::<ItemTagId>(&serde_json::to_string(&tag).unwrap()).unwrap(),
        tag
    );
    let stack = ItemStack {
        item: ItemTypeId(3),
        count: 7,
        instance: ItemInstanceData {
            durability: Some(11),
            flags: 5,
        },
    };
    let json = serde_json::to_string(&stack).unwrap();
    let back: ItemStack = serde_json::from_str(&json).unwrap();
    assert_eq!(back.item, stack.item);
    assert_eq!(back.count, stack.count);
    assert_eq!(back.instance.durability, Some(11));
    assert_eq!(back.instance.flags, 5);
}

/// T14：全部 ITEM_TAG_* 常量与 ITEM_TAG_TABLE 一一对应。
#[test]
fn item_tag_consts_match_table() {
    let consts: [(ItemTagId, u32, &str); 14] = [
        (ITEM_TAG_STACKABLE, 1, "stackable"),
        (ITEM_TAG_EQUIPPABLE, 2, "equippable"),
        (ITEM_TAG_DROPPABLE, 3, "droppable"),
        (ITEM_TAG_CONSUMABLE, 4, "consumable"),
        (ITEM_TAG_FLAMMABLE, 5, "flammable"),
        (ITEM_TAG_QUEST_ITEM, 6, "quest_item"),
        (ITEM_TAG_TWO_HANDED, 7, "two_handed"),
        (ITEM_TAG_MAGIC, 8, "magic"),
        (ITEM_TAG_THROWABLE, 9, "throwable"),
        (ITEM_TAG_PLACEABLE, 10, "placeable"),
        (ITEM_TAG_FUEL, 11, "fuel"),
        (ITEM_TAG_INGREDIENT, 12, "ingredient"),
        (ITEM_TAG_INDESTRUCTIBLE, 13, "indestructible"),
        (ITEM_TAG_UNIQUE, 14, "unique"),
    ];
    assert_eq!(consts.len(), ITEM_TAG_TABLE.len(), "常量数与标签表不一致");
    for (constant, id, name) in consts {
        assert_eq!(constant.0, id);
        assert_eq!(item_tag_name(constant), Some(name));
    }
}

/// T15：6.6 的全部一致性规则。
#[test]
fn item_definition_consistency() {
    for def in ITEM_TABLE {
        assert!(def.max_stack >= 1, "{} 的 max_stack 不能为 0", def.name);

        // stackable 标签 <=> max_stack > 1（max_stack 为权威）。
        let has_stackable = def.tags.contains(&ITEM_TAG_STACKABLE);
        assert_eq!(
            has_stackable,
            def.max_stack > 1,
            "{} 的 stackable 标签与 max_stack 矛盾",
            def.name
        );

        // Quest => quest_item（单向）。
        if def.category == ItemCategory::Quest {
            assert!(
                def.tags.contains(&ITEM_TAG_QUEST_ITEM),
                "{} 是 Quest 但没有 quest_item 标签",
                def.name
            );
        }
        // Consumable => consumable（单向）。
        if def.category == ItemCategory::Consumable {
            assert!(
                def.tags.contains(&ITEM_TAG_CONSUMABLE),
                "{} 是 Consumable 但没有 consumable 标签",
                def.name
            );
        }
        // 有子品类（本期为空表）=>
        if def.sub_category != ItemSubCategoryId::NONE {
            let parent =
                item_subcategory_parent(def.sub_category).expect("物品引用了不存在的子品类");
            assert_eq!(
                def.category, parent,
                "{} 的 category 与子品类父品类不一致",
                def.name
            );
        }
    }
}

/// T16：子品类 parent 非 None / Other；ItemDef.category == parent；id 唯一、分区正确。
#[test]
fn subcategory_parent_is_core_and_matches() {
    assert!(detect_id_collisions(
        &ITEM_SUBCATEGORY_TABLE
            .iter()
            .map(|def| IdEntry {
                id: def.id,
                name: def.name
            })
            .collect::<Vec<_>>()
    )
    .is_empty());

    for def in ITEM_SUBCATEGORY_TABLE {
        assert_ne!(def.parent, ItemCategory::None);
        assert_ne!(def.parent, ItemCategory::Other);
        assert_ne!(def.id, 0, "子品类 id 不能为 0");
        assert_ne!(partition_of(def.id), game_engine::ids::IdPartition::None);
    }

    for def in ITEM_TABLE {
        if def.sub_category != ItemSubCategoryId::NONE {
            let sub = item_subcategory_def(def.sub_category).expect("物品引用了不存在的子品类");
            assert_eq!(def.category, sub.parent);
        }
    }
}

/// T17：未知子品类查询返回 None；逻辑仍按 category 工作。
#[test]
fn unknown_subcategory_falls_back() {
    assert!(item_subcategory_def(ItemSubCategoryId(999_999)).is_none());
    assert!(item_subcategory_parent(ItemSubCategoryId(999_999)).is_none());

    let unknown_stack = ItemStack {
        item: ItemTypeId(999_999),
        count: 1,
        instance: ItemInstanceData::default(),
    };
    assert_eq!(unknown_stack.category(), ItemCategory::Other);
    assert!(!unknown_stack.has_tag(ITEM_TAG_DROPPABLE));
    assert!(unknown_stack.def().is_none());

    let known_stack = ItemStack {
        item: ItemTypeId(1),
        count: 3,
        instance: ItemInstanceData::default(),
    };
    assert_eq!(known_stack.category(), ItemCategory::Material);
    assert!(known_stack.has_tag(ITEM_TAG_STACKABLE));
}

/// 附加：FFI 快照与表长度一致，标签输出升序。
#[test]
fn snapshots_match_tables() {
    assert_eq!(
        item_category_table_snapshot().len(),
        ItemCategory::ALL.len()
    );
    assert_eq!(item_tag_table_snapshot().len(), ITEM_TAG_TABLE.len());
    assert_eq!(
        item_subcategory_table_snapshot().len(),
        ITEM_SUBCATEGORY_TABLE.len()
    );
    let rows = item_table_snapshot();
    assert_eq!(rows.len(), ITEM_TABLE.len());
    for row in &rows {
        for window in row.tags.windows(2) {
            assert!(window[0] < window[1]);
        }
    }
}
