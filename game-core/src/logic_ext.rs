//! 中立扩展字段袋（**逻辑侧**）。
//!
//! ## 分层边界
//!
//! 本模块**不依赖** `presentation` / 隐私 / Godot：它只是「逻辑系统可写的具名值
//! 容器」。表现层（`presentation::payload::extension`）单向消费它，再投影成线载荷。
//! 这样 gameplay 系统写袋时不会形成「逻辑 -> 表现」的依赖倒挂（先例：
//! `game-engine/src/ids.rs` 的中立模块）。`presentation::payload` 会再导出本模块
//! 的类型以保持既有路径稳定，但依赖方向只有 presentation -> logic_ext。
//!
//! ## 上限
//!
//! `MAX_EXT_FIELDS` / `MAX_EXT_TAGS` 是编解码两侧共用的硬上限：写入侧
//! （`set_field`）与线格式侧（`put_body` / `read_body`）都必须执行，
//! 保证自产帧永远不会被解码端整帧拒收。`Tags` 值在写入时即排序去重并截断，
//! 与写入来源无关，GPF1 字节可复现。

use std::collections::BTreeMap;

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

/// 单个扩展袋最多字段数（编解码两侧共用上限）。
pub const MAX_EXT_FIELDS: usize = 1024;
/// 单个 `Tags` 字段最多标签数。
pub const MAX_EXT_TAGS: usize = 4096;

/// 单个扩展字段的值（原型期：整数 / 布尔 / 标签集合）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ExtValue {
    I32(i32),
    Bool(bool),
    Tags(Vec<u32>),
}

/// [ExtensionBag::set_field] 的结果。
///
/// 显式区分「值真的变了」「同值写入」与「被上限拒绝」，避免旧的 `bool`
/// 把「未变」与「拒绝」混为一谈。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOutcome {
    /// 值确实变化并写入。
    Changed,
    /// 键已存在且值相同：未写入（但调用方仍可能因可变借用触发 Bevy `Changed`）。
    Unchanged,
    /// 新键超出 [MAX_EXT_FIELDS]，被拒绝。
    Rejected,
}

/// 规范化字段值：`Tags` 排序去重并截断到 [MAX_EXT_TAGS]，保证字节可复现。
pub(crate) fn normalize_value(value: ExtValue) -> ExtValue {
    match value {
        ExtValue::Tags(mut tags) => {
            tags.sort_unstable();
            tags.dedup();
            if tags.len() > MAX_EXT_TAGS {
                tags.truncate(MAX_EXT_TAGS);
            }
            ExtValue::Tags(tags)
        }
        other => other,
    }
}

/// 逻辑侧扩展字段袋：一个实体一个，只由框架的写入路径修改。
///
/// 业务代码不要直接拿 `&mut ExtensionBag`；写入口标 `#[doc(hidden)]`，
/// 后续由 `business_component!` 生成的系统调用。
#[derive(Component, Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExtensionBag {
    fields: BTreeMap<u16, ExtValue>,
}

impl ExtensionBag {
    /// 写入字段。
    ///
    /// **上限判断在规范化之前**：满袋时新键直接 [SetOutcome::Rejected]，
    /// 不会先做无谓的 Tags 排序/截断。
    #[doc(hidden)]
    pub fn set_field(&mut self, key: u16, value: ExtValue) -> SetOutcome {
        if !self.fields.contains_key(&key) && self.fields.len() >= MAX_EXT_FIELDS {
            // 编码上限：超出的新字段直接拒绝，避免自产帧被解码端整帧拒收。
            return SetOutcome::Rejected;
        }
        let value = normalize_value(value);
        match self.fields.get(&key) {
            Some(existing) if *existing == value => SetOutcome::Unchanged,
            _ => {
                self.fields.insert(key, value);
                SetOutcome::Changed
            }
        }
    }

    /// 清除字段；仅在原本存在时返回 true。
    #[doc(hidden)]
    pub fn clear_field(&mut self, key: u16) -> bool {
        self.fields.remove(&key).is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    pub fn field_count(&self) -> usize {
        self.fields.len()
    }

    /// 按 key 升序的字段视图（克隆，投影 / 测试用）。
    pub fn sorted_fields(&self) -> Vec<(u16, ExtValue)> {
        self.fields.iter().map(|(k, v)| (*k, v.clone())).collect()
    }
}
