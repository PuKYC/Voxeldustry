//! 物品静态数据。
//!
//! 三类身份各自独立编号，互不比较、互不合并：
//!
//! | 概念 | 类型 | 性质 |
//! |---|---|---|
//! | 物品定义 | [ItemTypeId] | 这是哪种物品（分区 ID） |
//! | 品类 | [ItemCategory] | 封闭核心枚举，代码 match |
//! | 子品类 | [ItemSubCategoryId] | mod 的 UI 分组（可选，必带核心父品类） |
//! | 标签 | [ItemTagId] | 开放标签，集合查询 |
//!
//! **分类只在定义上，实例只存差异**：ItemDef 持有品类 / 子品类 / 标签 /
//! 堆叠上限 / 掉落原型；ItemStack 只存物品 id、数量与实例差异。
//!
//! 命名只在表现层：逻辑 / 存档 / 网络只认 ID。

/// 核心静态数据的 source 标签（运行时 mod 注册随 ItemRegistry 后置，见 D8）。
pub(crate) const SOURCE_CORE: &str = "core";

mod category;
mod def;
mod index;
mod stack;
mod subcategory;
mod tag;

pub use category::*;
pub use def::*;
pub use index::*;
pub use stack::*;
pub use subcategory::*;
pub use tag::*;

#[cfg(test)]
mod tests;
