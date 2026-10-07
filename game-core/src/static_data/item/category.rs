use serde::{Deserialize, Deserializer, Serialize, Serializer};

// ─────────────────────────── 品类 Category ───────────────────────────

/// 封闭核心品类。
///
/// 代码会写 match 做分支，分支需要穷举、稳定、可静态检查，所以品类封闭：
/// mod 新增物品必须选一个核心品类，分组需求走子品类（见 [ItemSubCategoryId](crate::static_data::item::ItemSubCategoryId)），
/// 行为差异用标签表达。
///
/// **不要 derive Serialize / Deserialize**：derive 会序列化成变体名字符串，
/// 且遇到未知值直接报错。这里手写为 repr(u8) 整数，未知值回落 [ItemCategory::Other]。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum ItemCategory {
    None = 0,
    Material = 1,
    Weapon = 2,
    Armor = 3,
    Consumable = 4,
    Tool = 5,
    Ammo = 6,
    Quest = 7,
    Currency = 8,
    Container = 9,
    Block = 10,
    /// 前向兼容：解码到未知 u8 时落到这里，绝不 panic。
    Other = 255,
}

impl ItemCategory {
    /// 全部取值（含 [ItemCategory::Other]），供表快照与单测遍历。
    pub const ALL: [ItemCategory; 12] = [
        ItemCategory::None,
        ItemCategory::Material,
        ItemCategory::Weapon,
        ItemCategory::Armor,
        ItemCategory::Consumable,
        ItemCategory::Tool,
        ItemCategory::Ammo,
        ItemCategory::Quest,
        ItemCategory::Currency,
        ItemCategory::Container,
        ItemCategory::Block,
        ItemCategory::Other,
    ];

    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// 未知 u8（含 255）一律落到 [ItemCategory::Other]，不 panic。
    pub const fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::None,
            1 => Self::Material,
            2 => Self::Weapon,
            3 => Self::Armor,
            4 => Self::Consumable,
            5 => Self::Tool,
            6 => Self::Ammo,
            7 => Self::Quest,
            8 => Self::Currency,
            9 => Self::Container,
            10 => Self::Block,
            _ => Self::Other,
        }
    }

    /// 表现层名字（Godot 图标 / 分组映射用）。
    pub fn as_str(self) -> &'static str {
        match self {
            ItemCategory::None => "none",
            ItemCategory::Material => "material",
            ItemCategory::Weapon => "weapon",
            ItemCategory::Armor => "armor",
            ItemCategory::Consumable => "consumable",
            ItemCategory::Tool => "tool",
            ItemCategory::Ammo => "ammo",
            ItemCategory::Quest => "quest",
            ItemCategory::Currency => "currency",
            ItemCategory::Container => "container",
            ItemCategory::Block => "block",
            ItemCategory::Other => "other",
        }
    }
}

impl Serialize for ItemCategory {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(self.as_u8())
    }
}

impl<'de> Deserialize<'de> for ItemCategory {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::from_u8(u8::deserialize(deserializer)?))
    }
}

/// 品类表快照：(category_u8, name)。
pub fn item_category_table_snapshot() -> Vec<(u8, &'static str)> {
    ItemCategory::ALL
        .iter()
        .map(|category| (category.as_u8(), category.as_str()))
        .collect()
}
