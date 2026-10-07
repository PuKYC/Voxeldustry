//! 业务扩展载荷：把一批具名字段装进一个「袋」，整袋快照下发。
//!
//! 设计要点（对应评审结论）：
//! - 字段按 key 升序存在 BTreeMap（中立模块 [crate::logic_ext]），GPF1 字节序与写入顺序无关；
//! - 重发触发由 collector 负责（袋变化 OR 观察者感知变化），见 sync::collect_extension_bag；
//! - 袋空 = 该 kind 缺席：collector / reconcile 主动发 Remove 并清基线；
//! - 逐字段可见性只依赖 schema 自带的 required_bits，求值复用 [perception_allows]
//!   的同一份真值：All 恒可见，ServerOnly 恒隐藏，Required 仅在观察者命中位时可见。
//!
//! ## 为什么不再依赖实体级 CoreRequiredPerception
//!
//! 旧实现把 Required 委托给实体级组件，实体缺组件时引擎语义是 None => true
//! （Fail-Open），于是「标记需要授权的字段」会静默对所有人下发。schema 级
//! required_bits 不再有「缺载体」这一失败模式：Required 且 required_bits != 0 时，
//! 观察者没命中位就必然隐藏（Fail-Closed）。若 required_bits == 0 属于配置错误：
//! debug 下 debug_assert 暴露，release 下按隐藏处理（宁可不发也不泄漏）。
//!
//! 门控点说明：这是相对组件级 replicon filter 的第二个门控点。袋当前只在本地
//! 单视角表现管线使用；一旦走网络（多客户端），服务端序列化前必须做同一投影，
//! 否则两路不一致。

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use game_engine::perception::{perception_allows, PerceptionMask};

// 中立逻辑模块的类型经本模块再导出：既有 presentation::payload::* 路径保持稳定，
// 依赖方向仍是 presentation -> logic_ext（逻辑侧请直接用 crate::logic_ext）。
pub use crate::logic_ext::{ExtValue, ExtensionBag, SetOutcome, MAX_EXT_FIELDS, MAX_EXT_TAGS};

/// 一条扩展字段。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExtField {
    pub key: u16,
    pub value: ExtValue,
}

/// 构造 / 校验扩展载荷时的错误。
///
/// 不复用 PackedError::TooManyTags，让「字段数超限」与「Tags 超限」语义清晰。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtError {
    /// 字段数超过 MAX_EXT_FIELDS。
    TooManyFields,
    /// 单个 Tags 值超过 MAX_EXT_TAGS。
    TooManyTags,
}

impl std::fmt::Display for ExtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExtError::TooManyFields => write!(f, "扩展字段数超过上限 {}", MAX_EXT_FIELDS),
            ExtError::TooManyTags => write!(f, "扩展 Tags 数超过上限 {}", MAX_EXT_TAGS),
        }
    }
}

impl std::error::Error for ExtError {}

/// 整袋载荷（字段按 key 升序）。
///
/// 字段私有：公开构造只能走 [ExtensionPayload::from_fields]（校验上限），
/// 从根上堵住「手搓超限载荷 -> 自产帧被解码端整帧拒收」。
/// 线格式写入侧（put_body）另有 clamp 兜底，见 payload::packed。
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct ExtensionPayload {
    fields: Vec<ExtField>,
}

impl ExtensionPayload {
    /// 由字段列表构造；按 key 升序排序，并校验上限。
    pub fn from_fields(mut fields: Vec<ExtField>) -> Result<Self, ExtError> {
        if fields.len() > MAX_EXT_FIELDS {
            return Err(ExtError::TooManyFields);
        }
        for field in &fields {
            if let ExtValue::Tags(tags) = &field.value {
                if tags.len() > MAX_EXT_TAGS {
                    return Err(ExtError::TooManyTags);
                }
            }
        }
        // 排序保证与写入顺序无关（project 已有序，这是对公开构造路径的兜底）。
        fields.sort_by_key(|field| field.key);
        Ok(Self { fields })
    }

    /// 只读字段视图。
    pub fn fields(&self) -> &[ExtField] {
        &self.fields
    }

    /// 字段数（诊断 / 带宽基线用）。
    pub fn field_count(&self) -> usize {
        self.fields.len()
    }
}

/// 字段可见性策略。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldVisibility {
    /// 总是下发（不需要任何感知位）。
    All,
    /// 需要观察者命中本字段 schema 的 required_bits。
    Required,
    /// 永不下发（仅逻辑侧使用）。
    ServerOnly,
}

/// 字段 schema（原型期用显式表；后续由 business_component! 注册）。
#[derive(Clone, Copy, Debug)]
pub struct ExtensionFieldSchema {
    pub key: u16,
    pub name: &'static str,
    pub vis: FieldVisibility,
    /// FieldVisibility::Required 时生效：观察者需命中的感知位。
    /// 0 是配置错误（debug 自检会暴露，release 按隐藏处理）。
    pub required_bits: u32,
}

impl ExtensionFieldSchema {
    /// 总是下发。
    pub const fn all(key: u16, name: &'static str) -> Self {
        Self {
            key,
            name,
            vis: FieldVisibility::All,
            required_bits: 0,
        }
    }

    /// 需要观察者命中 bits。
    pub const fn required(key: u16, name: &'static str, bits: u32) -> Self {
        Self {
            key,
            name,
            vis: FieldVisibility::Required,
            required_bits: bits,
        }
    }

    /// 仅逻辑侧使用，永不下发。
    pub const fn server_only(key: u16, name: &'static str) -> Self {
        Self {
            key,
            name,
            vis: FieldVisibility::ServerOnly,
            required_bits: 0,
        }
    }
}

fn vis_rank(vis: FieldVisibility) -> u8 {
    match vis {
        FieldVisibility::All => 0,
        FieldVisibility::Required => 1,
        FieldVisibility::ServerOnly => 2,
    }
}

/// 扩展字段 schema 资源（按 key 升序）。
#[derive(Resource, Clone, Debug, Default)]
pub struct ExtensionSchema {
    pub fields: Vec<ExtensionFieldSchema>,
}

impl ExtensionSchema {
    pub fn new(mut fields: Vec<ExtensionFieldSchema>) -> Self {
        // 同 key 重复时取“最严”的可见性，避免歧义导致 Fail-Open；
        // 同级 Required 取感知位并集（任一命中即可见）。
        fields.sort_by_key(|f| f.key);
        let mut merged: Vec<ExtensionFieldSchema> = Vec::with_capacity(fields.len());
        for field in fields {
            match merged.last_mut() {
                Some(last) if last.key == field.key => {
                    if vis_rank(field.vis) > vis_rank(last.vis) {
                        last.vis = field.vis;
                        last.required_bits = field.required_bits;
                    } else if field.vis == FieldVisibility::Required {
                        last.required_bits |= field.required_bits;
                    }
                }
                _ => merged.push(field),
            }
        }
        Self { fields: merged }
    }

    /// 查 schema 条目；未登记返回 None。
    pub fn field_of(&self, key: u16) -> Option<&ExtensionFieldSchema> {
        self.fields.iter().find(|f| f.key == key)
    }

    pub fn vis_of(&self, key: u16) -> FieldVisibility {
        self.field_of(key)
            .map(|f| f.vis)
            // 默认 Fail-Closed：未在 schema 登记的字段不下发（防漏登记泄漏）。
            .unwrap_or(FieldVisibility::ServerOnly)
    }

    /// Debug 自检：统计「标记 Required 却没有 required_bits」的配置错误条目。
    ///
    /// 对齐 privacy.rs 的 VisibilityViolations 模式：属于契约违规，逐条 error!。
    pub fn required_bits_violations(&self) -> usize {
        let mut violations = 0;
        for field in &self.fields {
            if field.vis == FieldVisibility::Required && field.required_bits == 0 {
                error!(
                    "【扩展可见性违规】字段 {} (key={}) 标记为 Required 但 required_bits == 0；这会按 Fail-Closed 隐藏（release）或触发 debug 断言。请显式声明所需感知位。",
                    field.name, field.key
                );
                violations += 1;
            }
        }
        violations
    }
}

/// 纯函数：把袋投影成下发载荷。
///
/// observer 是参数（不读全局），便于按观察者分别投影与单测。Required 字段委托
/// [perception_allows] 单一真值，不再依赖实体级组件，因此不存在「实体缺授权载体
/// -> Fail-Open」的失败模式。
pub fn project(
    bag: &ExtensionBag,
    observer: PerceptionMask,
    schema: &ExtensionSchema,
) -> ExtensionPayload {
    let mut fields = Vec::new();
    for (key, value) in bag.sorted_fields() {
        let visible = match schema.field_of(key) {
            // 未登记 -> Fail-Closed。
            None => false,
            Some(field) => match field.vis {
                FieldVisibility::All => true,
                FieldVisibility::ServerOnly => false,
                FieldVisibility::Required => {
                    debug_assert!(
                        field.required_bits != 0,
                        "扩展字段 {} 标记 Required 但 required_bits == 0（配置错误）",
                        field.name
                    );
                    // release 下也 Fail-Closed：required_bits==0 时隐藏而非放行。
                    field.required_bits != 0 && perception_allows(observer, field.required_bits)
                }
            },
        };
        if visible {
            fields.push(ExtField { key, value });
        }
    }
    // 袋已在写入时受 MAX_EXT_FIELDS / MAX_EXT_TAGS 约束，此处结构上必然合法。
    ExtensionPayload::from_fields(fields).expect("袋内字段受写入上限约束，投影必然合法")
}
