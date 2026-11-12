//! 装配前参数 schema：把编译期 mod 与目录式包的参数声明合并成一张只读表。
//!
//! ParamTable::resolve 是运行期装配点，只在 CoreGame::build_with_profile 里跑
//! 一次；本模块不参与那个路径，也不复制它的解析行为。它面向 UI：在启动 / 建档
//! 之前，把「有哪些参数、长什么样、默认值是多少」原样交给 Godot 通用面板，并把
//! 面板回传的覆盖值按同一份 schema 先校验一遍。
//!
//! - 编译期参数来自 CoreMods::manifests() 的 ParamSpec；
//! - 运行期参数来自目录式包的 PackageParam；
//! - 两边按全 key <mod_id>.<local_key> 合并，按 key 升序返回。
//!
//! visible_if 统一使用同一 mod 内的 local key：编译期声明本来就是这个语义
//! （VisibleIf 不支持跨 mod），包清单也按同名规则解析。UI 判断联动时先用
//! mod_id + visible_if 拼出全 key，再与 key 比较，避免两个 mod 恰好有同名局部
//! key 时误判。

use std::collections::BTreeMap;

use game_engine::modding::{ParamError, ParamScope, ParamSpec, ParamType};

use crate::mods::CoreMods;
use crate::package::{LoadedPackage, PackageParam, PackageParamScope, PackageParamType};

/// 参数类型的拥有所有权版本。
///
/// 枚举值单独带在 kind 里；数值范围 / 步进 / 定点 scale 放在 ParamSchemaEntry
/// 的独立字段上，方便 FFI 直接平铺。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParamKind {
    Bool,
    Int,
    Fixed,
    Enum { values: Vec<String> },
}

impl ParamKind {
    /// 跨 FFI 的稳定类型标签。
    pub const fn code(&self) -> &'static str {
        match self {
            ParamKind::Bool => "bool",
            ParamKind::Int => "int",
            ParamKind::Fixed => "fixed",
            ParamKind::Enum { .. } => "enum",
        }
    }
}

/// 一条装配前参数声明。
///
/// - key 是全 key（<mod_id>.<local_key>），local_key 是 mod 内声明时写的原名；
/// - visible_if 的 key 是同一 mod 内的 local key；
/// - min / max / step 只对 Int / Fixed 有值，scale 只对 Fixed 有值。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamSchemaEntry {
    pub key: String,
    pub mod_id: String,
    pub local_key: String,
    pub scope: ParamScope,
    pub kind: ParamKind,
    pub default: i64,
    pub min: Option<i64>,
    pub max: Option<i64>,
    pub step: Option<i64>,
    pub scale: Option<u32>,
    pub group: String,
    pub visible_if: Option<(String, i64)>,
}

impl ParamSchemaEntry {
    /// 校验一个落盘整数能否赋给本参数。
    pub fn validate_value(&self, value: i64) -> Result<(), ParamError> {
        match &self.kind {
            ParamKind::Bool => {
                if value == 0 || value == 1 {
                    Ok(())
                } else {
                    Err(ParamError::InvalidBool {
                        key: self.key.clone(),
                        value,
                    })
                }
            }
            ParamKind::Int | ParamKind::Fixed => {
                let min = self.min.unwrap_or(i64::MIN);
                let max = self.max.unwrap_or(i64::MAX);
                if value < min || value > max {
                    Err(ParamError::OutOfRange {
                        key: self.key.clone(),
                        value,
                    })
                } else {
                    Ok(())
                }
            }
            ParamKind::Enum { values } => {
                if value < 0 || value as usize >= values.len() {
                    Err(ParamError::InvalidEnum {
                        key: self.key.clone(),
                        value,
                    })
                } else {
                    Ok(())
                }
            }
        }
    }

    /// 跨 FFI 的稳定作用域标签。
    pub fn scope_code(&self) -> &'static str {
        match self.scope {
            ParamScope::World => "world",
            ParamScope::Debug => "debug",
        }
    }
}

/// 采集装配前参数 schema。
///
/// 编译期 manifest 与包清单的参数按全 key 合并；重复 key 直接报
/// ParamError::DuplicateKey（不静默覆盖）。返回项按全 key 升序。
///
/// 包侧在这里再校验一遍声明（min <= max、step > 0、scale > 0、默认值在范围内、
/// Enum 默认下标合法、visible_if 引用的 local key 在本 mod 内已声明），即使
/// scan_package_root 已经校验过：包也可能来自别的入口。
pub fn collect_param_schema(
    mods: &CoreMods,
    packages: &[LoadedPackage],
) -> Result<Vec<ParamSchemaEntry>, ParamError> {
    let mut entries: BTreeMap<String, ParamSchemaEntry> = BTreeMap::new();

    for manifest in mods.manifests() {
        for spec in manifest.params {
            let entry = entry_from_spec(manifest.id, spec)?;
            insert_entry(&mut entries, entry)?;
        }
    }

    for package in packages {
        for param in &package.manifest.params {
            let entry = entry_from_package(package.id(), param)?;
            insert_entry(&mut entries, entry)?;
        }
    }

    // visible_if 只能引用同一 mod 内已声明的 local key；整个 mod 的参数都已
    // 收集完再检查，因此声明顺序不影响结果（允许前向引用）。
    for entry in entries.values() {
        if let Some((local_key, equals)) = &entry.visible_if {
            let target = full_key(&entry.mod_id, local_key);
            let Some(target_entry) = entries.get(&target) else {
                return Err(ParamError::InvalidSpec {
                    key: entry.key.clone(),
                    what: "visible_if 引用了本 mod 未声明的 key",
                });
            };
            // 条件值也必须落在目标参数的类型范围内，否则面板会永远不显示
            // 这一行，且错误在运行期很难定位。
            if target_entry.validate_value(*equals).is_err() {
                return Err(ParamError::InvalidSpec {
                    key: entry.key.clone(),
                    what: "visible_if.equals 与目标参数的类型 / 范围不符",
                });
            }
        }
    }

    Ok(entries.into_values().collect())
}

/// 校验一份覆盖表能否套用到给定 schema，不修改任何值。
///
/// 未知 key、Bool 非 0 / 1、Int / Fixed 越界、Enum 下标越界都拒绝；不按作用域
/// 拒绝——面板可以同时显示 World 与 Debug 参数，冻结与否由运行期决定。
pub fn validate_param_overrides(
    entries: &[ParamSchemaEntry],
    overrides: &BTreeMap<String, i64>,
) -> Result<(), ParamError> {
    for (key, value) in overrides {
        let entry = entries
            .iter()
            .find(|entry| entry.key == *key)
            .ok_or_else(|| ParamError::UnknownKey { key: key.clone() })?;
        entry.validate_value(*value)?;
    }
    Ok(())
}

/// 把参数错误映射成跨 FFI 的稳定错误码（param.* 前缀）。
///
/// 采集与覆盖校验只会产生前七种错误；其余变体来自运行期存档路径，这里给出
/// 保守的 param.invalid_spec，调用方仍应保留 message 原文。
pub fn param_error_code(error: &ParamError) -> &'static str {
    match error {
        ParamError::DuplicateKey { .. } => "param.duplicate_key",
        ParamError::UnknownKey { .. } => "param.unknown_key",
        ParamError::InvalidSpec { .. } => "param.invalid_spec",
        ParamError::InvalidDefault { .. } => "param.invalid_default",
        ParamError::OutOfRange { .. } => "param.out_of_range",
        ParamError::InvalidBool { .. } => "param.invalid_bool",
        ParamError::InvalidEnum { .. } => "param.invalid_enum",
        _ => "param.invalid_spec",
    }
}

fn full_key(mod_id: &str, local_key: &str) -> String {
    format!("{mod_id}.{local_key}")
}

fn insert_entry(
    entries: &mut BTreeMap<String, ParamSchemaEntry>,
    entry: ParamSchemaEntry,
) -> Result<(), ParamError> {
    if entries.contains_key(&entry.key) {
        return Err(ParamError::DuplicateKey { key: entry.key });
    }
    entries.insert(entry.key.clone(), entry);
    Ok(())
}

fn entry_from_spec(
    mod_id: &'static str,
    spec: &'static ParamSpec,
) -> Result<ParamSchemaEntry, ParamError> {
    if spec.key.is_empty() {
        return Err(ParamError::InvalidSpec {
            key: format!("{mod_id}."),
            what: "key 不能为空",
        });
    }
    let key = full_key(mod_id, spec.key);
    spec.validate(&key)?;
    let (kind, min, max, step, scale) = match spec.ty {
        ParamType::Bool => (ParamKind::Bool, None, None, None, None),
        ParamType::Int { min, max, step } => {
            (ParamKind::Int, Some(min), Some(max), Some(step), None)
        }
        ParamType::Fixed {
            scale,
            min,
            max,
            step,
        } => (
            ParamKind::Fixed,
            Some(min),
            Some(max),
            Some(step),
            Some(scale),
        ),
        ParamType::Enum { values } => (
            ParamKind::Enum {
                values: values.iter().map(|value| (*value).to_string()).collect(),
            },
            None,
            None,
            None,
            None,
        ),
    };
    Ok(ParamSchemaEntry {
        key,
        mod_id: mod_id.to_string(),
        local_key: spec.key.to_string(),
        scope: spec.scope,
        kind,
        default: spec.default,
        min,
        max,
        step,
        scale,
        group: spec.group.to_string(),
        visible_if: spec
            .visible_if
            .map(|visible| (visible.key.to_string(), visible.equals)),
    })
}

fn entry_from_package(mod_id: &str, param: &PackageParam) -> Result<ParamSchemaEntry, ParamError> {
    if param.key.is_empty() {
        return Err(ParamError::InvalidSpec {
            key: format!("{mod_id}."),
            what: "key 不能为空",
        });
    }
    let key = full_key(mod_id, &param.key);
    let (kind, min, max, step, scale) = match &param.ty {
        PackageParamType::Bool => (ParamKind::Bool, None, None, None, None),
        PackageParamType::Int { min, max, step } => {
            (ParamKind::Int, Some(*min), Some(*max), Some(*step), None)
        }
        PackageParamType::Fixed {
            scale,
            min,
            max,
            step,
        } => (
            ParamKind::Fixed,
            Some(*min),
            Some(*max),
            Some(*step),
            Some(*scale),
        ),
        PackageParamType::Enum { values } => (
            ParamKind::Enum {
                values: values.clone(),
            },
            None,
            None,
            None,
            None,
        ),
    };
    let entry = ParamSchemaEntry {
        key,
        mod_id: mod_id.to_string(),
        local_key: param.key.clone(),
        scope: match param.scope {
            PackageParamScope::World => ParamScope::World,
            PackageParamScope::Debug => ParamScope::Debug,
        },
        kind,
        default: param.default,
        min,
        max,
        step,
        scale,
        group: param.group.clone(),
        visible_if: param.visible_if.clone(),
    };
    validate_package_entry(&entry)?;
    Ok(entry)
}

fn validate_package_entry(entry: &ParamSchemaEntry) -> Result<(), ParamError> {
    let invalid = |what: &'static str| ParamError::InvalidSpec {
        key: entry.key.clone(),
        what,
    };
    match &entry.kind {
        ParamKind::Bool => {
            if entry.default != 0 && entry.default != 1 {
                return Err(ParamError::InvalidDefault {
                    key: entry.key.clone(),
                    value: entry.default,
                });
            }
        }
        ParamKind::Int | ParamKind::Fixed => {
            if entry.kind == ParamKind::Fixed && entry.scale == Some(0) {
                return Err(invalid("scale 必须大于 0"));
            }
            let (min, max, step) = match (entry.min, entry.max, entry.step) {
                (Some(min), Some(max), Some(step)) => (min, max, step),
                _ => return Err(invalid("数值类型缺少 min / max / step")),
            };
            if min > max {
                return Err(invalid("min 不能大于 max"));
            }
            if step <= 0 {
                return Err(invalid("step 必须大于 0"));
            }
            if entry.default < min || entry.default > max {
                return Err(ParamError::InvalidDefault {
                    key: entry.key.clone(),
                    value: entry.default,
                });
            }
        }
        ParamKind::Enum { values } => {
            if values.is_empty() {
                return Err(invalid("enum 至少需要一个值"));
            }
            if values.iter().any(|value| value.is_empty()) {
                return Err(invalid("enum 的值不能为空"));
            }
            if entry.default < 0 || entry.default as usize >= values.len() {
                return Err(ParamError::InvalidDefault {
                    key: entry.key.clone(),
                    value: entry.default,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use game_engine::modding::{Mod, ModManifest, VisibleIf};

    use crate::package::{
        LoadedPackage, PackageManifest, PackageParam, PackageParamScope, PackageParamType,
    };
    use crate::spec::CoreSpec;

    static ALPHA: ModManifest = ModManifest {
        id: "test:alpha",
        version: 1,
        engine_api: 1,
        load_after: &[],
        load_after_optional: &[],
        conflicts: &[],
        exclusive_groups: &[],
        params: &[
            ParamSpec {
                key: "speed",
                ty: ParamType::Int {
                    min: 0,
                    max: 100,
                    step: 5,
                },
                default: 50,
                scope: ParamScope::World,
                group: "移动",
                visible_if: None,
            },
            ParamSpec {
                key: "mode",
                ty: ParamType::Enum {
                    values: &["walk", "run"],
                },
                default: 0,
                scope: ParamScope::Debug,
                group: "",
                visible_if: None,
            },
        ],
        actions: &[],
        presentation: &[],
    };

    static BETA: ModManifest = ModManifest {
        id: "test:beta",
        version: 1,
        engine_api: 1,
        load_after: &[],
        load_after_optional: &[],
        conflicts: &[],
        exclusive_groups: &[],
        params: &[
            // 前向引用：enabled 在后面声明，整表收集完再校验应通过。
            ParamSpec {
                key: "detail",
                ty: ParamType::Bool,
                default: 1,
                scope: ParamScope::World,
                group: "视图",
                visible_if: Some(VisibleIf {
                    key: "enabled",
                    equals: 1,
                }),
            },
            ParamSpec {
                key: "enabled",
                ty: ParamType::Bool,
                default: 0,
                scope: ParamScope::World,
                group: "视图",
                visible_if: None,
            },
        ],
        actions: &[],
        presentation: &[],
    };

    static BAD_DEFAULT: ModManifest = ModManifest {
        id: "test:bad",
        version: 1,
        engine_api: 1,
        load_after: &[],
        load_after_optional: &[],
        conflicts: &[],
        exclusive_groups: &[],
        params: &[ParamSpec {
            key: "too_big",
            ty: ParamType::Int {
                min: 0,
                max: 10,
                step: 1,
            },
            default: 99,
            scope: ParamScope::World,
            group: "",
            visible_if: None,
        }],
        actions: &[],
        presentation: &[],
    };

    struct AlphaMod;
    impl Mod<CoreSpec> for AlphaMod {
        fn manifest(&self) -> &'static ModManifest {
            &ALPHA
        }
    }

    struct BetaMod;
    impl Mod<CoreSpec> for BetaMod {
        fn manifest(&self) -> &'static ModManifest {
            &BETA
        }
    }

    struct BadDefaultMod;
    impl Mod<CoreSpec> for BadDefaultMod {
        fn manifest(&self) -> &'static ModManifest {
            &BAD_DEFAULT
        }
    }

    fn package(id: &str, params: Vec<PackageParam>) -> LoadedPackage {
        LoadedPackage {
            source: PathBuf::from(id),
            manifest: PackageManifest {
                id: id.to_string(),
                version: 1,
                engine_api: 1,
                load_after: Vec::new(),
                load_after_optional: Vec::new(),
                conflicts: Vec::new(),
                exclusive_groups: Vec::new(),
                actions: Vec::new(),
                params,
                presentation: Vec::new(),
                data: Vec::new(),
                scripts: Vec::new(),
            },
            data: Vec::new(),
            scripts: Vec::new(),
        }
    }

    fn param(
        key: &str,
        ty: PackageParamType,
        default: i64,
        scope: PackageParamScope,
        group: &str,
        visible_if: Option<(&str, i64)>,
    ) -> PackageParam {
        PackageParam {
            key: key.to_string(),
            ty,
            default,
            scope,
            group: group.to_string(),
            visible_if: visible_if.map(|(key, equals)| (key.to_string(), equals)),
        }
    }

    fn find<'a>(entries: &'a [ParamSchemaEntry], key: &str) -> &'a ParamSchemaEntry {
        entries
            .iter()
            .find(|entry| entry.key == key)
            .unwrap_or_else(|| panic!("缺少参数 {key}"))
    }

    #[test]
    fn merges_compiled_and_package_params_in_key_order() {
        // push 顺序与最终 key 顺序相反，验证排序不依赖声明 / 装配顺序。
        let mods = CoreMods::new().push(BetaMod).push(AlphaMod);
        let packages = vec![package(
            "test:gamma",
            vec![
                param(
                    "zoom",
                    PackageParamType::Fixed {
                        scale: 100,
                        min: 100,
                        max: 300,
                        step: 10,
                    },
                    150,
                    PackageParamScope::World,
                    "视图",
                    Some(("enabled", 1)),
                ),
                param(
                    "enabled",
                    PackageParamType::Bool,
                    1,
                    PackageParamScope::Debug,
                    "",
                    None,
                ),
            ],
        )];

        let entries = collect_param_schema(&mods, &packages).expect("schema 应通过");
        let keys: Vec<&str> = entries.iter().map(|entry| entry.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "test:alpha.mode",
                "test:alpha.speed",
                "test:beta.detail",
                "test:beta.enabled",
                "test:gamma.enabled",
                "test:gamma.zoom",
            ]
        );

        let speed = find(&entries, "test:alpha.speed");
        assert_eq!(speed.mod_id, "test:alpha");
        assert_eq!(speed.local_key, "speed");
        assert_eq!(speed.scope, ParamScope::World);
        assert_eq!(speed.kind, ParamKind::Int);
        assert_eq!(
            (speed.min, speed.max, speed.step),
            (Some(0), Some(100), Some(5))
        );
        assert_eq!(speed.scale, None);
        assert_eq!(speed.group, "移动");

        let mode = find(&entries, "test:alpha.mode");
        assert_eq!(
            mode.kind,
            ParamKind::Enum {
                values: vec!["walk".to_string(), "run".to_string()]
            }
        );
        assert_eq!(mode.scope, ParamScope::Debug);

        let zoom = find(&entries, "test:gamma.zoom");
        assert_eq!(zoom.kind, ParamKind::Fixed);
        assert_eq!(zoom.scale, Some(100));
        assert_eq!(zoom.visible_if, Some(("enabled".to_string(), 1)));

        // 重复采集结果逐项相等：BTreeMap 保证稳定序。
        let again = collect_param_schema(&mods, &packages).expect("schema 应通过");
        assert_eq!(entries, again);
    }

    #[test]
    fn duplicate_key_between_compiled_and_package_is_rejected() {
        let mods = CoreMods::new().push(AlphaMod);
        let packages = vec![package(
            "test:alpha",
            vec![param(
                "speed",
                PackageParamType::Int {
                    min: 0,
                    max: 100,
                    step: 1,
                },
                10,
                PackageParamScope::World,
                "",
                None,
            )],
        )];
        let error = collect_param_schema(&mods, &packages).expect_err("重复 key 必须拒绝");
        assert_eq!(
            error,
            ParamError::DuplicateKey {
                key: "test:alpha.speed".to_string()
            }
        );
        assert_eq!(param_error_code(&error), "param.duplicate_key");
    }

    #[test]
    fn package_invalid_default_is_rejected() {
        let cases = vec![
            (
                param(
                    "amount",
                    PackageParamType::Int {
                        min: 0,
                        max: 10,
                        step: 1,
                    },
                    11,
                    PackageParamScope::World,
                    "",
                    None,
                ),
                "test:pkg.amount",
            ),
            (
                param(
                    "ratio",
                    PackageParamType::Fixed {
                        scale: 10,
                        min: 0,
                        max: 100,
                        step: 1,
                    },
                    -1,
                    PackageParamScope::World,
                    "",
                    None,
                ),
                "test:pkg.ratio",
            ),
            (
                param(
                    "flag",
                    PackageParamType::Bool,
                    2,
                    PackageParamScope::World,
                    "",
                    None,
                ),
                "test:pkg.flag",
            ),
            (
                param(
                    "mode",
                    PackageParamType::Enum {
                        values: vec!["a".to_string(), "b".to_string()],
                    },
                    2,
                    PackageParamScope::World,
                    "",
                    None,
                ),
                "test:pkg.mode",
            ),
        ];
        for (param, key) in cases {
            let error = collect_param_schema(&CoreMods::new(), &[package("test:pkg", vec![param])])
                .expect_err("非法默认值必须拒绝");
            assert!(
                matches!(error, ParamError::InvalidDefault { .. }),
                "期望 InvalidDefault，实际 {error:?}"
            );
            assert_eq!(param_error_code(&error), "param.invalid_default");
            assert!(error.to_string().contains(key));
        }
    }

    #[test]
    fn package_invalid_spec_is_rejected() {
        let cases = vec![
            PackageParamType::Int {
                min: 10,
                max: 0,
                step: 1,
            },
            PackageParamType::Int {
                min: 0,
                max: 10,
                step: 0,
            },
            PackageParamType::Fixed {
                scale: 0,
                min: 0,
                max: 10,
                step: 1,
            },
            PackageParamType::Enum { values: Vec::new() },
        ];
        for ty in cases {
            let error = collect_param_schema(
                &CoreMods::new(),
                &[package(
                    "test:pkg",
                    vec![param("bad", ty, 0, PackageParamScope::World, "", None)],
                )],
            )
            .expect_err("非法声明必须拒绝");
            assert!(
                matches!(error, ParamError::InvalidSpec { .. }),
                "期望 InvalidSpec，实际 {error:?}"
            );
            assert_eq!(param_error_code(&error), "param.invalid_spec");
        }
    }

    #[test]
    fn compiled_manifest_invalid_default_is_rejected() {
        let error = collect_param_schema(&CoreMods::new().push(BadDefaultMod), &[])
            .expect_err("编译期默认值也必须校验");
        assert_eq!(
            error,
            ParamError::InvalidDefault {
                key: "test:bad.too_big".to_string(),
                value: 99,
            }
        );
    }

    #[test]
    fn visible_if_must_reference_a_declared_local_key() {
        // 同 mod 内的 local key 可以前向引用。
        let entries =
            collect_param_schema(&CoreMods::new().push(BetaMod), &[]).expect("前向引用应通过");
        assert_eq!(
            find(&entries, "test:beta.detail").visible_if,
            Some(("enabled".to_string(), 1))
        );

        // 引用不存在的 local key：拒绝。
        let error = collect_param_schema(
            &CoreMods::new(),
            &[package(
                "test:delta",
                vec![
                    param(
                        "enabled",
                        PackageParamType::Bool,
                        1,
                        PackageParamScope::World,
                        "",
                        None,
                    ),
                    param(
                        "level",
                        PackageParamType::Int {
                            min: 0,
                            max: 3,
                            step: 1,
                        },
                        1,
                        PackageParamScope::World,
                        "",
                        Some(("missing", 1)),
                    ),
                ],
            )],
        )
        .expect_err("未声明的 visible_if 必须拒绝");
        assert!(matches!(error, ParamError::InvalidSpec { .. }));
        assert_eq!(param_error_code(&error), "param.invalid_spec");

        // 跨 mod 同名 local key 不算声明：本包没有 enabled，引用必须失败。
        let cross_mod = collect_param_schema(
            &CoreMods::new(),
            &[
                package(
                    "test:epsilon",
                    vec![param(
                        "enabled",
                        PackageParamType::Bool,
                        1,
                        PackageParamScope::World,
                        "",
                        None,
                    )],
                ),
                package(
                    "test:zeta",
                    vec![param(
                        "level",
                        PackageParamType::Int {
                            min: 0,
                            max: 3,
                            step: 1,
                        },
                        1,
                        PackageParamScope::World,
                        "",
                        Some(("enabled", 1)),
                    )],
                ),
            ],
        );
        assert!(cross_mod.is_err(), "visible_if 不支持跨 mod 引用");

        // 条件值本身也必须合法：Bool 目标只接受 0 / 1。
        let bad_equals = collect_param_schema(
            &CoreMods::new(),
            &[package(
                "test:eta",
                vec![
                    param(
                        "enabled",
                        PackageParamType::Bool,
                        1,
                        PackageParamScope::World,
                        "",
                        None,
                    ),
                    param(
                        "level",
                        PackageParamType::Int {
                            min: 0,
                            max: 3,
                            step: 1,
                        },
                        1,
                        PackageParamScope::World,
                        "",
                        Some(("enabled", 2)),
                    ),
                ],
            )],
        );
        assert!(bad_equals.is_err(), "visible_if.equals 越界必须拒绝");
    }

    #[test]
    fn overrides_validate_values_and_accept_both_scopes() {
        let entries = collect_param_schema(&CoreMods::new().push(AlphaMod).push(BetaMod), &[])
            .expect("schema 应通过");

        let mut valid = BTreeMap::new();
        valid.insert("test:alpha.speed".to_string(), 80);
        valid.insert("test:alpha.mode".to_string(), 1);
        valid.insert("test:beta.enabled".to_string(), 1);
        validate_param_overrides(&entries, &valid).expect("World / Debug 都应接受");

        let mut unknown = BTreeMap::new();
        unknown.insert("test:alpha.nope".to_string(), 1);
        let error = validate_param_overrides(&entries, &unknown).expect_err("未知 key 必须拒绝");
        assert_eq!(
            error,
            ParamError::UnknownKey {
                key: "test:alpha.nope".to_string()
            }
        );
        assert_eq!(param_error_code(&error), "param.unknown_key");

        let cases = vec![
            ("test:alpha.speed", 101, "param.out_of_range", "OutOfRange"),
            ("test:alpha.speed", -1, "param.out_of_range", "OutOfRange"),
            ("test:beta.enabled", 2, "param.invalid_bool", "InvalidBool"),
            ("test:alpha.mode", 2, "param.invalid_enum", "InvalidEnum"),
            ("test:alpha.mode", -1, "param.invalid_enum", "InvalidEnum"),
        ];
        for (key, value, code, label) in cases {
            let mut overrides = BTreeMap::new();
            overrides.insert(key.to_string(), value);
            let error =
                validate_param_overrides(&entries, &overrides).expect_err("非法覆盖值必须拒绝");
            assert_eq!(param_error_code(&error), code, "{label} 的错误码不对");
            match label {
                "OutOfRange" => assert!(matches!(error, ParamError::OutOfRange { .. })),
                "InvalidBool" => assert!(matches!(error, ParamError::InvalidBool { .. })),
                "InvalidEnum" => assert!(matches!(error, ParamError::InvalidEnum { .. })),
                _ => unreachable!(),
            }
        }
    }
}
