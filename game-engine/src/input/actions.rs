//! 语义动作 ↔ 输入通道位的**机制层**。
//!
//! 具体动作表（名字 / id / 通道位）属于游戏内容，由游戏侧通过
//! [`ActionMap`] 提供；引擎只定义形状与查询的默认实现。
//!
//! 两层分离不可混淆：
//!
//! - [`ActionId`]：语义动作标识，**可无限扩展**（mod 用字符串哈希）；
//! - [`ChannelMask`]：输入通道位，**必须定长 64**（性能 + 确定性 + 位运算友好）。
//!
//! 名字只在表现层/跨边界出现；存档 / 协议 / 回滚 / 玩法逻辑一律只看通道位，
//! 因此改键不影响逻辑。

use serde::{Deserialize, Serialize};

/// 语义动作 ID（可扩展；mod 用 FNV1a 哈希生成）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ActionId(pub u32);

/// 输入通道位掩码（定长 64）。
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug, Serialize, Deserialize)]
pub struct ChannelMask(pub u64);

/// 通道位上限。
pub const MAX_INPUT_CHANNELS: u32 = 64;

/// 一条动作定义。
///
/// `name` 同时是 Godot `InputMap` 里的动作名，也是唯一的跨边界标识。
#[derive(Clone, Copy, Debug)]
pub struct ActionDef {
    pub name: &'static str,
    pub action: ActionId,
    pub channel_bit: u32,
}

/// 通道位掩码工具。
impl ChannelMask {
    pub const EMPTY: Self = Self(0);

    /// 第 `index` 位的掩码。越界会被截断到 64 位内（不应发生，调用方先查表）。
    #[inline]
    pub fn bit(index: u32) -> Self {
        debug_assert!(index < MAX_INPUT_CHANNELS, "通道位越界: {index}");
        Self(1u64 << (index % MAX_INPUT_CHANNELS))
    }

    #[inline]
    pub fn insert(&mut self, index: u32) {
        self.0 |= Self::bit(index).0;
    }

    #[inline]
    pub fn remove(&mut self, index: u32) {
        self.0 &= !Self::bit(index).0;
    }

    #[inline]
    pub fn contains(self, index: u32) -> bool {
        self.0 & Self::bit(index).0 != 0
    }

    /// 任意一位命中。
    #[inline]
    pub fn intersects(self, mask: u64) -> bool {
        self.0 & mask != 0
    }

    /// 全部命中。
    #[inline]
    pub fn contains_all(self, mask: u64) -> bool {
        self.0 & mask == mask
    }
}

/// 动作表契约：引擎只通过它把「名字」折成「通道位」。
///
/// 用 ZST + 关联函数，编译期单态化；不是 `dyn`，热路径零开销。
pub trait ActionMap: Send + Sync + 'static {
    /// 单一真相源：动作定义表。
    fn table() -> &'static [ActionDef];

    /// 名字查表（Godot 侧唯一入口）。未知名字返回 `None`（前向兼容，不 panic）。
    ///
    /// 默认线性查找：核心表仅几十条，无需二分。
    fn lookup(name: &str) -> Option<&'static ActionDef> {
        Self::table().iter().find(|def| def.name == name)
    }

    /// 名字 → 通道位序号。
    fn channel_bit_of(name: &str) -> Option<u32> {
        Self::lookup(name).map(|def| def.channel_bit)
    }

    /// 名字 → 单一位掩码。
    fn mask_of_name(name: &str) -> Option<u64> {
        Self::channel_bit_of(name).map(|bit| 1u64 << (bit % MAX_INPUT_CHANNELS))
    }

    /// 批量名字 → 掩码（未知名字被忽略）。
    fn mask_of_names<'a>(names: impl IntoIterator<Item = &'a str>) -> u64 {
        names
            .into_iter()
            .filter_map(Self::mask_of_name)
            .fold(0u64, |acc, mask| acc | mask)
    }

    /// 位序号 → 名字。
    fn name_of_bit(bit: u32) -> Option<&'static str> {
        Self::table()
            .iter()
            .find(|def| def.channel_bit == bit)
            .map(|def| def.name)
    }

    /// 位序号 → 语义动作。
    fn action_of_bit(bit: u32) -> Option<ActionId> {
        Self::table()
            .iter()
            .find(|def| def.channel_bit == bit)
            .map(|def| def.action)
    }

    /// 供 Godot 启动时拉取并校验自己的 `InputMap`：返回 `(name, channel_bit)`。
    fn snapshot() -> Vec<(&'static str, u32)> {
        Self::table()
            .iter()
            .map(|def| (def.name, def.channel_bit))
            .collect()
    }
}

/// 最小空表：不需要任何动作的游戏 / 引擎自测用。
pub struct EmptyActions;

impl ActionMap for EmptyActions {
    fn table() -> &'static [ActionDef] {
        &[]
    }
}
