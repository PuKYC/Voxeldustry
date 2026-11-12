//! mod 自定义表现载荷（`SyncPayload::Custom`）：一个实体、多个子种类的整包快照。
//!
//! 与 [ExtensionPayload](super::ExtensionPayload) 的区别：扩展袋是「一个实体一份
//! 字段集」，不同写入者共用一套 key 空间；mod 表现载荷是「一个实体按 sub_kind
//! 分组的多份字段集」，不同 mod / 不同表现种类互不覆盖，Godot 侧按 sub_kind
//! 查表决定交给哪个渲染器。
//!
//! ## 线格式（GPF1 body）
//!
//! `u32 group_count`，然后每组 `u16 sub_kind + u32 field_count + 字段编码`；
//! 字段编码与扩展袋逐字节一致（共用 payload::packed 里的 helper）。组按
//! sub_kind 升序、组内字段按 key 升序，因此字节序与 mod 的写入顺序无关。
//!
//! ## 上限
//!
//! - 组数 ≤ [MAX_MOD_PAYLOAD_GROUPS]；
//! - 每组字段数 ≤ [MAX_EXT_FIELDS]；
//! - 单个 Tags 值 ≤ [MAX_EXT_TAGS]。
//!
//! 写入侧（put_body / write_pools）另有 clamp 兜底，公开构造入口
//! （[CustomPayload::from_groups]）负责把「手搓超限载荷」拒在门外，避免自产帧
//! 被解码端整帧拒收。
//!
//! ## sub_kind 从哪来
//!
//! 由 mod 清单在装配期分配（game-engine 的 `modding::presentation`）：按确定性
//! 拓扑序 + manifest 声明顺序从 1 递增，0 保留给核心。表本身**不进存档**：
//! mod 清单已由存档冻结，sub_kind 是装配期派生的确定性结果，读档时重建即可。

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use game_engine::modding::ResolvedModPayloads;
pub use game_engine::modding::{ModFieldVis, ResolvedModPayloadField, ResolvedModPayloadKind};
use game_engine::perception::{perception_allows, PerceptionMask};

use super::{ExtField, ExtValue, SetOutcome, MAX_EXT_FIELDS, MAX_EXT_TAGS};

/// 一个实体最多携带的 mod 表现子种类数（线格式写入侧的 clamp 上限）。
pub const MAX_MOD_PAYLOAD_GROUPS: usize = 32;

/// 一组同一 sub_kind 的字段（组内按 key 升序、key 唯一）。
///
/// 字段公开以便 mod 直接读数；构造整包请走 [CustomPayload::from_groups]，
/// 它负责校验上限并规范化顺序。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModPayloadGroup {
    /// 子种类（0 保留给核心，mod 从 1 起）。
    pub sub_kind: u16,
    /// 字段（按 key 升序）。
    pub fields: Vec<ExtField>,
}

impl ModPayloadGroup {
    /// 组内字段数。
    pub fn field_count(&self) -> usize {
        self.fields.len()
    }

    /// 按 key 查字段。
    pub fn field(&self, key: u16) -> Option<&ExtField> {
        self.fields
            .binary_search_by_key(&key, |field| field.key)
            .ok()
            .map(|index| &self.fields[index])
    }
}

/// 构造 / 校验 mod 表现载荷时的错误。
///
/// 与 [ExtError](super::ExtError) 同风格：结构化变体 + 中文 Display，
/// 让「组数超限」「字段数超限」「key 重复」在日志里一眼可分。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModPayloadError {
    /// 组数超过 [MAX_MOD_PAYLOAD_GROUPS]。
    TooManyGroups,
    /// 同一 sub_kind 出现多组（整包必须一组一个 sub_kind）。
    DuplicateSubKind {
        /// 重复的子种类。
        sub_kind: u16,
    },
    /// 单组字段数超过 [MAX_EXT_FIELDS]。
    TooManyFields {
        /// 超限的子种类。
        sub_kind: u16,
    },
    /// 同一组内出现重复 key。
    DuplicateFieldKey {
        /// 所属子种类。
        sub_kind: u16,
        /// 重复的字段 key。
        key: u16,
    },
    /// 单个 Tags 值超过 [MAX_EXT_TAGS]。
    TooManyTags {
        /// 所属子种类。
        sub_kind: u16,
        /// 超限的字段 key。
        key: u16,
    },
}

impl std::fmt::Display for ModPayloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModPayloadError::TooManyGroups => {
                write!(f, "mod 表现载荷组数超过上限 {}", MAX_MOD_PAYLOAD_GROUPS)
            }
            ModPayloadError::DuplicateSubKind { sub_kind } => {
                write!(f, "mod 表现载荷 sub_kind {sub_kind} 重复")
            }
            ModPayloadError::TooManyFields { sub_kind } => write!(
                f,
                "mod 表现载荷 sub_kind {sub_kind} 的字段数超过上限 {}",
                MAX_EXT_FIELDS
            ),
            ModPayloadError::DuplicateFieldKey { sub_kind, key } => {
                write!(f, "mod 表现载荷 sub_kind {sub_kind} 的字段 key {key} 重复")
            }
            ModPayloadError::TooManyTags { sub_kind, key } => write!(
                f,
                "mod 表现载荷 sub_kind {sub_kind} 字段 key {key} 的 Tags 数超过上限 {}",
                MAX_EXT_TAGS
            ),
        }
    }
}

impl std::error::Error for ModPayloadError {}

/// mod 表现载荷的整包快照（组按 sub_kind 升序）。
///
/// 组私有：公开构造只能走 [CustomPayload::from_groups]（校验 + 规范化），
/// 从根上堵住「手搓超限载荷 -> 自产帧被拒收」。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CustomPayload {
    groups: Vec<ModPayloadGroup>,
}

impl CustomPayload {
    /// 由分组列表构造：校验上限，组按 sub_kind 升序、组内字段按 key 升序。
    ///
    /// 重复 sub_kind / 重复 key 都是调用方错误（组件写入路径不会产生），
    /// 这里显式拒绝而不是静默合并，避免「两个 mod 抢同一个 sub_kind」被掩盖。
    pub fn from_groups(mut groups: Vec<ModPayloadGroup>) -> Result<Self, ModPayloadError> {
        if groups.len() > MAX_MOD_PAYLOAD_GROUPS {
            return Err(ModPayloadError::TooManyGroups);
        }
        for group in &mut groups {
            if group.fields.len() > MAX_EXT_FIELDS {
                return Err(ModPayloadError::TooManyFields {
                    sub_kind: group.sub_kind,
                });
            }
            for field in &group.fields {
                if let ExtValue::Tags(tags) = &field.value {
                    if tags.len() > MAX_EXT_TAGS {
                        return Err(ModPayloadError::TooManyTags {
                            sub_kind: group.sub_kind,
                            key: field.key,
                        });
                    }
                }
            }
            group.fields.sort_by_key(|field| field.key);
            for pair in group.fields.windows(2) {
                if pair[0].key == pair[1].key {
                    return Err(ModPayloadError::DuplicateFieldKey {
                        sub_kind: group.sub_kind,
                        key: pair[1].key,
                    });
                }
            }
        }
        groups.sort_by_key(|group| group.sub_kind);
        for pair in groups.windows(2) {
            if pair[0].sub_kind == pair[1].sub_kind {
                return Err(ModPayloadError::DuplicateSubKind {
                    sub_kind: pair[1].sub_kind,
                });
            }
        }
        Ok(Self { groups })
    }

    /// 只读分组视图（sub_kind 升序）。
    pub fn groups(&self) -> &[ModPayloadGroup] {
        &self.groups
    }

    /// 组数。
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    /// 是否没有任何组。
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// 按 sub_kind 查组。
    pub fn group_of(&self, sub_kind: u16) -> Option<&ModPayloadGroup> {
        self.groups
            .binary_search_by_key(&sub_kind, |group| group.sub_kind)
            .ok()
            .map(|index| &self.groups[index])
    }
}

/// 实体上的 mod 自定义表现组件（多子种类、可增量写入）。
///
/// mod 在自己的系统里用 [set_field](ModPresentation::set_field) 写它，
/// [collect_mod_presentation](crate::presentation::sync::collect_mod_presentation)
/// 按 schema 投影后下发 `SyncPayload::Custom`。
///
/// 内部不变量与 [CustomPayload] 一致：组按 sub_kind 升序、组内字段按 key 升序，
/// 组数与字段数受上限约束，Tags 写入时归一化（排序去重 + 截断）。
#[derive(Component, Clone, Debug, Default, PartialEq)]
pub struct ModPresentation {
    groups: Vec<ModPayloadGroup>,
}

impl ModPresentation {
    /// 写一个字段；新键 / 新组在满员时返回 [SetOutcome::Rejected]。
    ///
    /// 同值写入返回 [SetOutcome::Unchanged]（语义与 ExtensionBag 一致），
    /// 但 Bevy 仍会因可变借用把组件标记为 Changed —— collector 靠投影哈希去抖。
    pub fn set_field(&mut self, sub_kind: u16, key: u16, value: ExtValue) -> SetOutcome {
        let value = crate::logic_ext::normalize_value(value);
        let index = match self
            .groups
            .binary_search_by_key(&sub_kind, |group| group.sub_kind)
        {
            Ok(index) => index,
            Err(index) => {
                if self.groups.len() >= MAX_MOD_PAYLOAD_GROUPS {
                    return SetOutcome::Rejected;
                }
                self.groups.insert(
                    index,
                    ModPayloadGroup {
                        sub_kind,
                        fields: Vec::new(),
                    },
                );
                index
            }
        };
        let group = &mut self.groups[index];
        match group.fields.binary_search_by_key(&key, |field| field.key) {
            Ok(position) => {
                if group.fields[position].value == value {
                    return SetOutcome::Unchanged;
                }
                group.fields[position].value = value;
                SetOutcome::Changed
            }
            Err(position) => {
                if group.fields.len() >= MAX_EXT_FIELDS {
                    return SetOutcome::Rejected;
                }
                group.fields.insert(position, ExtField { key, value });
                SetOutcome::Changed
            }
        }
    }

    /// 清空一个子种类；返回是否真的移除了组。
    pub fn clear_group(&mut self, sub_kind: u16) -> bool {
        match self
            .groups
            .binary_search_by_key(&sub_kind, |group| group.sub_kind)
        {
            Ok(index) => {
                self.groups.remove(index);
                true
            }
            Err(_) => false,
        }
    }

    /// 只读分组视图（sub_kind 升序）。
    pub fn groups(&self) -> &[ModPayloadGroup] {
        &self.groups
    }

    /// 是否没有任何组。
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// 转成整包快照（collector 与手动构造下发共用同一入口）。
    pub fn to_payload(&self) -> CustomPayload {
        CustomPayload::from_groups(self.groups.clone())
            .expect("组件写入受上限 / 排序约束，快照必然合法")
    }
}

/// 已记录过「无 schema」日志的 sub_kind（每个 sub_kind 只记一次，避免刷屏）。
static UNKNOWN_SUB_KIND_LOGGED: RwLock<Option<BTreeSet<u16>>> = RwLock::new(None);

fn note_unknown_sub_kind(sub_kind: u16) {
    let mut guard = match UNKNOWN_SUB_KIND_LOGGED.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let logged = guard.get_or_insert_with(BTreeSet::new);
    if logged.insert(sub_kind) {
        debug!("mod 表现载荷缺少 sub_kind {sub_kind} 的 schema，整组丢弃");
    }
}

/// 纯函数：按 sub_kind 的 schema 把组件投影成下发载荷。
///
/// 每个组单独求值：`All` 恒可见、`ServerOnly` 恒隐藏、`Required` 需观察者命中
/// `required_bits`（Fail-Closed：`required_bits == 0` 时隐藏而非放行，未在
/// schema 登记的字段同样隐藏）。找不到 schema 的组整组丢弃，并按 sub_kind
/// 只记一次 debug 日志。
///
/// 结果里有字段的组才保留；没有可见字段时返回空载荷，collector 据此发 Remove。
pub fn project_mod_presentation(
    presentation: &ModPresentation,
    observer: PerceptionMask,
    schema: &ResolvedModPayloads,
) -> CustomPayload {
    let mut groups: Vec<ModPayloadGroup> = Vec::with_capacity(presentation.groups.len());
    for group in &presentation.groups {
        let Some(kind) = schema.kind_of(group.sub_kind) else {
            note_unknown_sub_kind(group.sub_kind);
            continue;
        };
        let mut fields: Vec<ExtField> = Vec::with_capacity(group.fields.len());
        for field in &group.fields {
            let visible = match kind.field_of(field.key) {
                // 未登记 -> Fail-Closed（防漏登记泄漏）。
                None => false,
                Some(spec) => match spec.visibility {
                    ModFieldVis::All => true,
                    ModFieldVis::ServerOnly => false,
                    ModFieldVis::Required => {
                        debug_assert!(
                            spec.required_bits != 0,
                            "mod 表现字段 {} 标记 Required 但 required_bits == 0（配置错误）",
                            spec.name
                        );
                        // release 下同样 Fail-Closed：0 位视为隐藏。
                        spec.required_bits != 0 && perception_allows(observer, spec.required_bits)
                    }
                },
            };
            if visible {
                fields.push(field.clone());
            }
        }
        if !fields.is_empty() {
            groups.push(ModPayloadGroup {
                sub_kind: group.sub_kind,
                fields,
            });
        }
    }
    CustomPayload::from_groups(groups).expect("组件写入受上限约束，投影必然合法")
}

/// 全局 mod 表现载荷表快照（Godot 线程读；Arc 共享，避免每帧拷贝）。
///
/// 与 [crate::input::actions] 的动作表快照同一模式：装配期发布一次，运行期只读；
/// 未发布（未挂 mod）时返回空表。
static MOD_PAYLOAD_TABLE: RwLock<Option<Arc<[ResolvedModPayloadKind]>>> = RwLock::new(None);

/// 发布装配期解析出的 mod 表现载荷表（由 bevy_backend 在装配成功后调用）。
pub fn publish_mod_payloads(resolved: &ResolvedModPayloads) {
    let table: Arc<[ResolvedModPayloadKind]> = Arc::from(resolved.kinds());
    match MOD_PAYLOAD_TABLE.write() {
        Ok(mut slot) => *slot = Some(table),
        Err(poisoned) => *poisoned.into_inner() = Some(table),
    }
}

/// 读取全局快照；未发布时返回空表。
pub fn mod_payload_table() -> Arc<[ResolvedModPayloadKind]> {
    let published = match MOD_PAYLOAD_TABLE.read() {
        Ok(slot) => (*slot).clone(),
        Err(poisoned) => (*poisoned.into_inner()).clone(),
    };
    published.unwrap_or_else(|| Arc::from(Vec::new()))
}
