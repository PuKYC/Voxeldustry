//! mod 自定义表现载荷的声明与 sub_kind 分配。
//!
//! 表现不属于模拟状态，这里只解决**跨端一致**：同一个 mod 的同一个子种类，
//! 在服务端 / 客户端的表里必须是同一个 `sub_kind`。分配只看装配期的确定性
//! 输入（[Mods::apply](crate::modding::Mods::apply) 拓扑排序后的 mod 顺序 +
//! manifest 内的声明顺序），因此与 mod 的注册顺序、机器、遍历顺序都无关。
//!
//! - `sub_kind = 0` 保留给核心（未分配），mod 从 1 起连续分配；
//! - 表本身**不进存档**：mod 清单已由存档冻结，`sub_kind` 是装配期派生的结果，
//!   读档时重建即可得到同一张表；
//! - 表现不跨模拟 / 回放边界，因此这里不承诺位宽之外的兼容性。
//!
//! game-engine 不依赖 game-core，所以可见性枚举在本模块独立定义一份（语义与
//! game-core 表现层的 `FieldVisibility` 一一对应）。

use bevy::prelude::Resource;

/// mod 表现字段的可见性。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModFieldVis {
    /// 对所有观察者可见。
    All,
    /// 观察者的感知掩码命中 [ModFieldSpec::required_bits] 才可见。
    Required,
    /// 恒不跨边界（只在本机逻辑 / 模拟侧使用）。
    ServerOnly,
}

/// 一个表现字段的声明。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModFieldSpec {
    /// 字段 key（在所属子种类内唯一）。
    pub key: u16,
    /// 字段名（Godot 侧按名查表，也是唯一的人类可读标识）。
    pub name: &'static str,
    /// 可见性。
    pub visibility: ModFieldVis,
    /// [ModFieldVis::Required] 时生效：观察者需命中的感知位。
    ///
    /// 0 是配置错误（debug 自检会暴露，release 按隐藏处理，宁可不发也不泄漏）。
    pub required_bits: u32,
}

impl ModFieldSpec {
    /// 总是下发的字段。
    pub const fn all(key: u16, name: &'static str) -> Self {
        Self {
            key,
            name,
            visibility: ModFieldVis::All,
            required_bits: 0,
        }
    }

    /// 需要观察者命中 `bits` 才下发的字段。
    pub const fn required(key: u16, name: &'static str, bits: u32) -> Self {
        Self {
            key,
            name,
            visibility: ModFieldVis::Required,
            required_bits: bits,
        }
    }

    /// 仅逻辑侧使用、永不下发的字段。
    pub const fn server_only(key: u16, name: &'static str) -> Self {
        Self {
            key,
            name,
            visibility: ModFieldVis::ServerOnly,
            required_bits: 0,
        }
    }
}

/// 一个表现子种类的声明：名字 + 若干字段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModPayloadSpec {
    /// 子种类名（mod 内唯一）。
    pub name: &'static str,
    /// 字段声明（顺序只影响 schema 表，不影响 sub_kind）。
    pub fields: &'static [ModFieldSpec],
}

/// sub_kind 的上限（0 保留给核心，mod 可用 1..=u16::MAX）。
pub const MAX_MOD_SUB_KIND: u16 = u16::MAX;

/// 解析后的一个字段（key 升序）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedModPayloadField {
    /// 字段 key。
    pub key: u16,
    /// 字段名。
    pub name: &'static str,
    /// 可见性。
    pub visibility: ModFieldVis,
    /// Required 时的感知位（其余可见性为 0）。
    pub required_bits: u32,
}

/// 解析后的一个子种类。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedModPayloadKind {
    /// 分配到的子种类编号（1 起，装配期派生，不进存档）。
    pub sub_kind: u16,
    /// 声明它的 mod id。
    pub mod_id: &'static str,
    /// 子种类名。
    pub name: &'static str,
    /// 字段（key 升序）。
    pub fields: Vec<ResolvedModPayloadField>,
}

impl ResolvedModPayloadKind {
    /// 按 key 查字段；未登记返回 None（调用方按 Fail-Closed 处理）。
    pub fn field_of(&self, key: u16) -> Option<&ResolvedModPayloadField> {
        self.fields
            .binary_search_by_key(&key, |field| field.key)
            .ok()
            .map(|index| &self.fields[index])
    }
}

/// 表现载荷表解析错误。
///
/// 与 [ParamError](crate::modding::ParamError) /
/// [ActionResolveError](crate::input::ActionResolveError) 同风格：结构化变体 +
/// 中文 Display，由 [ModError](crate::modding::ModError) 统一包装。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PresentationResolveError {
    /// 同一个 mod 内出现两个同名子种类。
    DuplicateKindName {
        /// 声明它的 mod id。
        mod_id: String,
        /// 重复的子种类名。
        name: &'static str,
    },
    /// 同一个子种类内出现两个相同 key。
    DuplicateFieldKey {
        /// 声明它的 mod id。
        mod_id: String,
        /// 子种类名。
        kind: &'static str,
        /// 重复的字段 key。
        key: u16,
    },
    /// sub_kind 用尽（mod 声明的子种类总数超过 u16::MAX）。
    Exhausted {
        /// 实际声明的总数。
        used: usize,
        /// 可分配的上限。
        max: usize,
    },
}

impl std::fmt::Display for PresentationResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PresentationResolveError::DuplicateKindName { mod_id, name } => {
                write!(f, "mod {mod_id} 内子种类名重复：{name}")
            }
            PresentationResolveError::DuplicateFieldKey { mod_id, kind, key } => {
                write!(f, "mod {mod_id} 的子种类 {kind} 内字段 key 重复：{key}")
            }
            PresentationResolveError::Exhausted { used, max } => {
                write!(f, "mod 表现子种类数 {used} 超过上限 {max}")
            }
        }
    }
}

impl std::error::Error for PresentationResolveError {}

/// 解析后的 mod 表现载荷表（sub_kind 升序）。
///
/// [Mods::apply](crate::modding::Mods::apply) 成功时插入为 Resource；表现 collector
/// 按 `sub_kind` 查 schema 求值字段可见性。
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolvedModPayloads {
    kinds: Vec<ResolvedModPayloadKind>,
}

impl ResolvedModPayloads {
    /// 按「拓扑排序后的 mod 顺序 + manifest 声明顺序」分配 sub_kind。
    ///
    /// `mods` 的顺序就是分配顺序：调用方（[Mods::apply](crate::modding::Mods::apply)）
    /// 已经按确定性拓扑序传入，因此同样的 mod 集合必然得到同一张表。
    ///
    /// 错误场景：mod 内子种类重名、同一子种类内字段 key 重复、sub_kind 用尽。
    pub fn resolve(
        mods: &[(&'static str, &'static [ModPayloadSpec])],
    ) -> Result<Self, PresentationResolveError> {
        let total: usize = mods.iter().map(|(_, specs)| specs.len()).sum();
        let max = usize::from(MAX_MOD_SUB_KIND);
        if total > max {
            return Err(PresentationResolveError::Exhausted { used: total, max });
        }

        let mut kinds: Vec<ResolvedModPayloadKind> = Vec::with_capacity(total);
        for (mod_id, specs) in mods.iter().copied() {
            for (offset, spec) in specs.iter().enumerate() {
                // mod 内重名：只与自己之前声明的比，跨 mod 同名是允许的
                // （sub_kind 才是全局唯一标识）。
                if specs[..offset].iter().any(|prev| prev.name == spec.name) {
                    return Err(PresentationResolveError::DuplicateKindName {
                        mod_id: mod_id.to_string(),
                        name: spec.name,
                    });
                }
                // 0 保留给核心，因此第一条分配 1；上面已校验总数不越界。
                let sub_kind = u16::try_from(kinds.len() + 1).map_err(|_| {
                    PresentationResolveError::Exhausted {
                        used: kinds.len() + 1,
                        max,
                    }
                })?;
                let fields = resolve_fields(mod_id, spec)?;
                kinds.push(ResolvedModPayloadKind {
                    sub_kind,
                    mod_id,
                    name: spec.name,
                    fields,
                });
            }
        }
        Ok(Self { kinds })
    }

    /// 只读视图（sub_kind 升序）。
    pub fn kinds(&self) -> &[ResolvedModPayloadKind] {
        &self.kinds
    }

    /// 子种类数。
    pub fn len(&self) -> usize {
        self.kinds.len()
    }

    /// 是否没有任何 mod 表现子种类。
    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }

    /// 按 sub_kind 查表；未分配返回 None。
    pub fn kind_of(&self, sub_kind: u16) -> Option<&ResolvedModPayloadKind> {
        self.kinds
            .binary_search_by_key(&sub_kind, |kind| kind.sub_kind)
            .ok()
            .map(|index| &self.kinds[index])
    }
}

/// 校验并规范化一个子种类的字段（key 升序）。
fn resolve_fields(
    mod_id: &'static str,
    spec: &ModPayloadSpec,
) -> Result<Vec<ResolvedModPayloadField>, PresentationResolveError> {
    let mut fields: Vec<ResolvedModPayloadField> = Vec::with_capacity(spec.fields.len());
    for (offset, field) in spec.fields.iter().enumerate() {
        if spec.fields[..offset]
            .iter()
            .any(|prev| prev.key == field.key)
        {
            return Err(PresentationResolveError::DuplicateFieldKey {
                mod_id: mod_id.to_string(),
                kind: spec.name,
                key: field.key,
            });
        }
        fields.push(ResolvedModPayloadField {
            key: field.key,
            name: field.name,
            visibility: field.visibility,
            required_bits: field.required_bits,
        });
    }
    fields.sort_by_key(|field| field.key);
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaked(name: String) -> &'static str {
        Box::leak(name.into_boxed_str())
    }

    fn spec(name: &'static str, fields: &'static [ModFieldSpec]) -> ModPayloadSpec {
        ModPayloadSpec { name, fields }
    }

    /// 测试里把临时切片提升为 'static：生产侧 manifest 本来就是静态表。
    fn leaked_specs(specs: Vec<ModPayloadSpec>) -> &'static [ModPayloadSpec] {
        Box::leak(specs.into_boxed_slice())
    }

    static BASIC_FIELDS: [ModFieldSpec; 2] = [
        ModFieldSpec::all(2, "beta"),
        ModFieldSpec::required(1, "alpha", 0b10),
    ];

    #[test]
    fn sub_kind_allocation_follows_mod_and_declaration_order() {
        let mods: [(&'static str, &'static [ModPayloadSpec]); 2] = [
            (
                "alpha",
                leaked_specs(vec![spec("aura", &BASIC_FIELDS), spec("beam", &[])]),
            ),
            ("zeta", leaked_specs(vec![spec("aura", &[])])),
        ];
        let resolved = ResolvedModPayloads::resolve(&mods).expect("声明合法");

        let triples: Vec<(u16, &str, &str)> = resolved
            .kinds()
            .iter()
            .map(|kind| (kind.sub_kind, kind.mod_id, kind.name))
            .collect();
        assert_eq!(
            triples,
            vec![
                (1, "alpha", "aura"),
                (2, "alpha", "beam"),
                (3, "zeta", "aura"),
            ],
            "sub_kind 从 1 起、按 mod 顺序 + 声明顺序递增"
        );
        assert_eq!(resolved.kind_of(2).map(|kind| kind.name), Some("beam"));
        assert!(resolved.kind_of(0).is_none(), "0 保留给核心");
    }

    #[test]
    fn fields_are_sorted_by_key_and_required_bits_preserved() {
        let mods: [(&'static str, &'static [ModPayloadSpec]); 1] =
            [("alpha", leaked_specs(vec![spec("aura", &BASIC_FIELDS)]))];
        let resolved = ResolvedModPayloads::resolve(&mods).expect("声明合法");
        let kind = resolved.kind_of(1).expect("应已分配");
        let keys: Vec<u16> = kind.fields.iter().map(|field| field.key).collect();
        assert_eq!(keys, vec![1, 2], "字段必须按 key 升序");
        let required = kind.field_of(1).expect("应能按 key 查字段");
        assert_eq!(required.visibility, ModFieldVis::Required);
        assert_eq!(required.required_bits, 0b10);
        assert_eq!(
            kind.field_of(2).map(|field| field.visibility),
            Some(ModFieldVis::All)
        );
        assert!(kind.field_of(9).is_none());
    }

    #[test]
    fn duplicate_kind_name_within_one_mod_is_rejected() {
        let mods: [(&'static str, &'static [ModPayloadSpec]); 1] = [(
            "alpha",
            leaked_specs(vec![spec("aura", &[]), spec("aura", &[])]),
        )];
        assert_eq!(
            ResolvedModPayloads::resolve(&mods),
            Err(PresentationResolveError::DuplicateKindName {
                mod_id: "alpha".to_string(),
                name: "aura",
            })
        );
    }

    #[test]
    fn same_kind_name_across_mods_is_allowed() {
        let mods: [(&'static str, &'static [ModPayloadSpec]); 2] = [
            ("alpha", leaked_specs(vec![spec("aura", &[])])),
            ("zeta", leaked_specs(vec![spec("aura", &[])])),
        ];
        let resolved = ResolvedModPayloads::resolve(&mods).expect("跨 mod 同名合法");
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved.kinds()[1].mod_id, "zeta");
    }

    #[test]
    fn duplicate_field_key_within_one_spec_is_rejected() {
        static DUPLICATED: [ModFieldSpec; 2] =
            [ModFieldSpec::all(7, "x"), ModFieldSpec::server_only(7, "y")];
        let mods: [(&'static str, &'static [ModPayloadSpec]); 1] =
            [("alpha", leaked_specs(vec![spec("aura", &DUPLICATED)]))];
        assert_eq!(
            ResolvedModPayloads::resolve(&mods),
            Err(PresentationResolveError::DuplicateFieldKey {
                mod_id: "alpha".to_string(),
                kind: "aura",
                key: 7,
            })
        );
    }

    #[test]
    fn sub_kind_exhaustion_is_rejected() {
        // u16::MAX 个可用编号对应 u16::MAX 个子种类；再多一个就是溢出。
        let max = usize::from(MAX_MOD_SUB_KIND);
        let fits: Vec<ModPayloadSpec> = (0..max)
            .map(|index| spec(leaked(format!("kind{index}")), &[]))
            .collect();
        let fits: &'static [ModPayloadSpec] = Box::leak(fits.into_boxed_slice());
        let mods: [(&'static str, &'static [ModPayloadSpec]); 1] = [("alpha", fits)];
        let resolved = ResolvedModPayloads::resolve(&mods).expect("上限内应成功");
        assert_eq!(resolved.len(), max);
        assert_eq!(resolved.kinds()[max - 1].sub_kind, MAX_MOD_SUB_KIND);

        let overflow: Vec<ModPayloadSpec> = (0..=max)
            .map(|index| spec(leaked(format!("kind{index}")), &[]))
            .collect();
        let overflow: &'static [ModPayloadSpec] = Box::leak(overflow.into_boxed_slice());
        let mods: [(&'static str, &'static [ModPayloadSpec]); 1] = [("alpha", overflow)];
        assert_eq!(
            ResolvedModPayloads::resolve(&mods),
            Err(PresentationResolveError::Exhausted { used: max + 1, max })
        );
    }
}
