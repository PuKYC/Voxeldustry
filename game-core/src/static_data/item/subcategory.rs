use serde::{Deserialize, Serialize};

use game_engine::ids::{partition_name, partition_of};

use super::category::ItemCategory;
use super::SOURCE_CORE;

// ─────────────────────────── 子品类 SubCategory（mod 接口，D11） ───────────────────────────

/// mod 子品类 ID：开放分区 ID，必须声明核心父品类。
///
/// 核心逻辑只 match [ItemCategory]，禁止 match 子品类；子品类只用于 UI 分组 / 排序，
/// 以及 mod 自己的逻辑。卸载回退：查不到的子品类视为 [ItemSubCategoryId::NONE]，
/// 物品仍按 category 正常参与逻辑。
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default, Serialize, Deserialize,
)]
pub struct ItemSubCategoryId(pub u32);

impl ItemSubCategoryId {
    /// 0 = 无子品类（绝大多数核心物品）。
    pub const NONE: Self = Self(0);
}

#[derive(Clone, Copy, Debug)]
pub struct ItemSubCategoryDef {
    pub id: u32,
    pub name: &'static str,
    /// 必填，且只能是核心品类（不得为 None / Other）。
    pub parent: ItemCategory,
}

/// 本期核心表为空，只固定数据形状；mod 注册入口随 ItemRegistry（D8）后置。
pub const ITEM_SUBCATEGORY_TABLE: &[ItemSubCategoryDef] = &[];

pub fn item_subcategory_def(id: ItemSubCategoryId) -> Option<&'static ItemSubCategoryDef> {
    ITEM_SUBCATEGORY_TABLE.iter().find(|def| def.id == id.0)
}

/// 物品引用的子品类是否有效；未知（mod 卸载）返回 None，调用方回退到 NONE。
pub fn item_subcategory_parent(id: ItemSubCategoryId) -> Option<ItemCategory> {
    item_subcategory_def(id).map(|def| def.parent)
}

/// 子品类表快照：(id, name, parent_u8, parent_name, partition, source)。
pub fn item_subcategory_table_snapshot() -> Vec<(
    u32,
    &'static str,
    u8,
    &'static str,
    &'static str,
    &'static str,
)> {
    ITEM_SUBCATEGORY_TABLE
        .iter()
        .map(|def| {
            (
                def.id,
                def.name,
                def.parent.as_u8(),
                def.parent.as_str(),
                partition_name(partition_of(def.id)),
                SOURCE_CORE,
            )
        })
        .collect()
}
