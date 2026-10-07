use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use super::category::ItemCategory;
use super::def::{item_def, item_has_tag, ItemDef, ItemTypeId};
use super::tag::ItemTagId;

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
    /// 品类来自定义；未定义（mod 卸载）落入 Other，不 panic。
    pub fn category(&self) -> ItemCategory {
        item_def(self.item)
            .map(|def| def.category)
            .unwrap_or(ItemCategory::Other)
    }

    /// 标签查询走定义；未定义物品一律为 false。
    pub fn has_tag(&self, tag: ItemTagId) -> bool {
        item_has_tag(self.item, tag)
    }

    /// 定义的借用视图（热路径可缓存它，避免每帧查表）。
    pub fn def(&self) -> Option<&'static ItemDef> {
        item_def(self.item)
    }
}
