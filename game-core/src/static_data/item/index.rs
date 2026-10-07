use std::collections::HashMap;

use bevy::prelude::*;

use super::def::{ItemDef, ItemTypeId};
use super::tag::ItemTagId;

// ─────────────────────────── 标签反查索引 ───────────────────────────

/// tag -> 该标签下的物品（升序）。
///
/// 只在 Startup 构建一次，供**查找**使用。游戏逻辑禁止遍历 by_tag
/// （HashMap 遍历顺序不确定），只允许调用 [ItemTagIndex::items_with]，
/// 其返回升序切片，遍历安全。
#[derive(Resource, Debug, Default)]
pub struct ItemTagIndex {
    by_tag: HashMap<ItemTagId, Vec<ItemTypeId>>,
}

impl ItemTagIndex {
    /// 从定义列表构建。结果按 tag 分组，组内按物品 id 升序去重（确定性）。
    pub fn build(table: &[ItemDef]) -> Self {
        let mut by_tag: HashMap<ItemTagId, Vec<ItemTypeId>> = HashMap::new();
        for def in table {
            for tag in def.tags {
                by_tag.entry(*tag).or_default().push(ItemTypeId(def.id));
            }
        }
        for items in by_tag.values_mut() {
            items.sort_unstable();
            items.dedup();
        }
        Self { by_tag }
    }

    /// 某标签下的全部物品（升序切片；未知标签返回空切片）。
    pub fn items_with(&self, tag: ItemTagId) -> &[ItemTypeId] {
        self.by_tag.get(&tag).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn tag_count(&self) -> usize {
        self.by_tag.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_tag.is_empty()
    }
}
