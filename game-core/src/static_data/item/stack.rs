use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use super::category::ItemCategory;
use super::def::{ItemDef, ItemTypeId};
use super::tag::ItemTagId;
use crate::static_data::ObjectTables;

// ─────────────────────────── 物品实例 ───────────────────────────

/// 每实例差异数据。分类相关字段一律不放这里（D10）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ItemInstanceData {
    pub durability: Option<u16>,
    /// 附魔 / 词缀等扩展位，后续按需补。
    pub flags: u32,
}

/// 背包 / 槽位里的一叠物品（普通组件，按 Rust 类型注册 replicon）。
///
/// 只存物品 id、数量与实例差异；品类 / 标签 / 堆叠上限等定义属性不复制。
#[derive(Component, Clone, Debug, Serialize, Deserialize)]
pub struct ItemStack {
    pub item: ItemTypeId,
    pub count: u16,
    pub instance: ItemInstanceData,
}

impl ItemStack {
    /// 品类来自运行期定义表；未定义（mod 卸载）落入 Other，不 panic。
    pub fn category(&self, tables: &ObjectTables) -> ItemCategory {
        tables
            .item(self.item)
            .map_or(ItemCategory::Other, |def| def.category)
    }

    /// 标签查询走运行期定义表；未定义物品一律为 false。
    pub fn has_tag(&self, tag: ItemTagId, tables: &ObjectTables) -> bool {
        tables
            .item(self.item)
            .is_some_and(|def| def.tags.binary_search(&tag).is_ok())
    }

    /// 定义的借用视图（热路径可缓存它，避免每帧查表）。
    pub fn def<'a>(&self, tables: &'a ObjectTables) -> Option<&'a ItemDef> {
        tables.item(self.item)
    }

    /// 这一叠的总质量（克）；未定义物品返回 None。
    pub fn total_mass_grams(&self, tables: &ObjectTables) -> Option<u64> {
        self.def(tables)
            .map(|def| u64::from(def.mass_grams) * u64::from(self.count))
    }
}
