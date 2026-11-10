//! 插件参数清单与运行期参数表。
//!
//! 参数 schema 由 mod 在 `ModManifest::params` 里用 `const` 声明；key 在 mod 内
//! 局部唯一，落盘 / 桥接时拼成 `<mod_id>.<key>`。参数只分两类作用域：
//!
//! - [`ParamScope::World`]：建档时确定，进入世界后冻结，必须随存档保存；
//! - [`ParamScope::Debug`]：仅调试期可改，不进存档。
//!
//! 客户端本地参数（画质、按键等）不进入本表：Rust 模拟层读不到它们，也就不该
//! 由 Rust 声明。这条判据避免把「表现设置」和「模拟参数」混在同一张表里。

use std::collections::BTreeMap;

use bevy::prelude::*;

use super::ModManifest;

/// 参数类型（整数或定点；显示精度由 `scale` 决定）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamType {
    /// 布尔：落盘整数只能是 0 / 1。
    Bool,
    /// 整数：`min..=max`，`step` 只约束 UI 步进。
    Int { min: i64, max: i64, step: i64 },
    /// 定点：落盘整数 = 显示值 × `scale`，范围按落盘整数给出。
    Fixed {
        scale: u32,
        min: i64,
        max: i64,
        step: i64,
    },
    /// 枚举：落盘整数是 `values` 的下标。
    Enum { values: &'static [&'static str] },
}

impl ParamType {
    /// 跨 FFI / 存档的类型标签。
    pub const fn tag(self) -> u8 {
        match self {
            ParamType::Bool => 0,
            ParamType::Int { .. } => 1,
            ParamType::Fixed { .. } => 2,
            ParamType::Enum { .. } => 3,
        }
    }

    /// 校验 schema 本身是否合法（默认值校验由 [`ParamSpec::validate`] 做）。
    pub fn validate_spec(self) -> Result<(), &'static str> {
        match self {
            ParamType::Bool => Ok(()),
            ParamType::Int { min, max, step } => {
                if min > max {
                    return Err("min 不能大于 max");
                }
                if step <= 0 {
                    return Err("step 必须大于 0");
                }
                Ok(())
            }
            ParamType::Fixed {
                scale,
                min,
                max,
                step,
            } => {
                if scale == 0 {
                    return Err("scale 必须大于 0");
                }
                if min > max {
                    return Err("min 不能大于 max");
                }
                if step <= 0 {
                    return Err("step 必须大于 0");
                }
                Ok(())
            }
            ParamType::Enum { values } => {
                if values.is_empty() {
                    return Err("enum 至少需要一个值");
                }
                Ok(())
            }
        }
    }

    /// 校验一个落盘整数是否落在类型允许的范围内。
    fn validate_value(self, value: i64) -> Result<(), ValueError> {
        match self {
            ParamType::Bool => {
                if value == 0 || value == 1 {
                    Ok(())
                } else {
                    Err(ValueError::InvalidBool)
                }
            }
            ParamType::Int { min, max, .. } | ParamType::Fixed { min, max, .. } => {
                if value < min || value > max {
                    Err(ValueError::OutOfRange)
                } else {
                    Ok(())
                }
            }
            ParamType::Enum { values } => {
                if value < 0 || value as usize >= values.len() {
                    Err(ValueError::InvalidEnum)
                } else {
                    Ok(())
                }
            }
        }
    }
}

/// 参数作用域。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamScope {
    /// 世界参数：建档时确定，进入世界后冻结，随存档保存。
    World,
    /// 调试参数：仅开发期可改，不进存档。
    Debug,
}

impl ParamScope {
    /// 跨 FFI / 存档的作用域标签。
    pub const fn tag(self) -> u8 {
        match self {
            ParamScope::World => 0,
            ParamScope::Debug => 1,
        }
    }
}

/// 参数联动的最简形式：当另一个参数等于某个值时，本参数才可见。
///
/// `key` 是同一 mod 内的局部 key；跨 mod 引用不做支持。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VisibleIf {
    pub key: &'static str,
    pub equals: i64,
}

/// 一条插件参数声明。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParamSpec {
    /// mod 内局部 key；落盘 / 桥接时拼成 `<mod_id>.<key>`。
    pub key: &'static str,
    pub ty: ParamType,
    /// 默认值，必须是落盘整数。
    pub default: i64,
    pub scope: ParamScope,
    /// UI 分组；空串表示不分组。
    pub group: &'static str,
    /// 可选的可见性条件。
    pub visible_if: Option<VisibleIf>,
}

impl ParamSpec {
    /// 校验 schema 与默认值。
    pub fn validate(&self, full_key: &str) -> Result<(), ParamError> {
        self.ty
            .validate_spec()
            .map_err(|what| ParamError::InvalidSpec {
                key: full_key.to_string(),
                what,
            })?;
        self.ty
            .validate_value(self.default)
            .map_err(|_| ParamError::InvalidDefault {
                key: full_key.to_string(),
                value: self.default,
            })
    }
}

/// 参数表里的一条已解析项。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParamEntry {
    pub spec: &'static ParamSpec,
    pub value: i64,
}

/// 参数错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamError {
    /// 两个 mod（或同一 mod 的两条声明）拼出了同一个全 key。
    DuplicateKey { key: String },
    /// 覆盖 / 写入引用了未声明的 key。
    UnknownKey { key: String },
    /// schema 本身不合法。
    InvalidSpec { key: String, what: &'static str },
    /// 默认值不合法。
    InvalidDefault { key: String, value: i64 },
    /// 整数值超出 min..=max。
    OutOfRange { key: String, value: i64 },
    /// 布尔参数不是 0 / 1。
    InvalidBool { key: String, value: i64 },
    /// 枚举下标越界。
    InvalidEnum { key: String, value: i64 },
    /// 世界参数在冻结后被修改。
    Frozen { key: String },
}

impl std::fmt::Display for ParamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParamError::DuplicateKey { key } => write!(f, "参数 {key} 重复声明"),
            ParamError::UnknownKey { key } => write!(f, "未知参数：{key}"),
            ParamError::InvalidSpec { key, what } => {
                write!(f, "参数 {key} 的声明不合法：{what}")
            }
            ParamError::InvalidDefault { key, value } => {
                write!(f, "参数 {key} 的默认值 {value} 不合法")
            }
            ParamError::OutOfRange { key, value } => {
                write!(f, "参数 {key} 的值 {value} 超出范围")
            }
            ParamError::InvalidBool { key, value } => {
                write!(f, "参数 {key} 是布尔，值 {value} 只能是 0 或 1")
            }
            ParamError::InvalidEnum { key, value } => {
                write!(f, "参数 {key} 的枚举下标 {value} 越界")
            }
            ParamError::Frozen { key } => {
                write!(f, "世界参数 {key} 已冻结，不能再改")
            }
        }
    }
}

impl std::error::Error for ParamError {}

/// 运行期参数表：全 key → 声明 + 当前值，按 key 字典序稳定遍历。
#[derive(Resource, Default, Clone, Debug)]
pub struct ParamTable {
    entries: BTreeMap<String, ParamEntry>,
}

impl ParamTable {
    /// 从 mod 清单解析参数表。
    ///
    /// `overrides` 的 key 必须是全 key（`<mod_id>.<key>`）；缺失的 key 用默认值，
    /// 未知 key 直接拒绝。**不静默忽略**：拼错的覆盖如果被吞掉，用户会以为
    /// 设置生效了。
    pub fn resolve(
        manifests: &[&'static ModManifest],
        overrides: &BTreeMap<String, i64>,
    ) -> Result<Self, ParamError> {
        let mut entries: BTreeMap<String, ParamEntry> = BTreeMap::new();

        for manifest in manifests {
            for spec in manifest.params {
                if spec.key.is_empty() {
                    return Err(ParamError::InvalidSpec {
                        key: format!("{}.", manifest.id),
                        what: "key 不能为空",
                    });
                }
                let key = format!("{}.{}", manifest.id, spec.key);
                if entries.contains_key(&key) {
                    return Err(ParamError::DuplicateKey { key });
                }
                spec.validate(&key)?;
                let value = overrides.get(&key).copied().unwrap_or(spec.default);
                spec.ty
                    .validate_value(value)
                    .map_err(|error| error.into_param(key.clone(), value))?;
                entries.insert(key, ParamEntry { spec, value });
            }
        }

        for key in overrides.keys() {
            if !entries.contains_key(key) {
                return Err(ParamError::UnknownKey { key: key.clone() });
            }
        }

        Ok(Self { entries })
    }

    /// 按全 key 取一项。
    pub fn get(&self, key: &str) -> Option<ParamEntry> {
        self.entries.get(key).copied()
    }

    /// 按 mod id + 局部 key 取一项。
    pub fn get_mod(&self, mod_id: &str, key: &str) -> Option<ParamEntry> {
        self.get(&format!("{mod_id}.{key}"))
    }

    /// 按全 key 写一项。
    ///
    /// `frozen` 为真时拒绝修改 [`ParamScope::World`] 参数；[ParamScope::Debug]
    /// 不受影响。冻结判定在 Rust 侧完成，桥接层只负责把当前冻结状态传进来。
    pub fn set(&mut self, key: &str, value: i64, frozen: bool) -> Result<(), ParamError> {
        let spec = self
            .entries
            .get(key)
            .map(|entry| entry.spec)
            .ok_or_else(|| ParamError::UnknownKey {
                key: key.to_string(),
            })?;
        if frozen && spec.scope == ParamScope::World {
            return Err(ParamError::Frozen {
                key: key.to_string(),
            });
        }
        spec.ty
            .validate_value(value)
            .map_err(|error| error.into_param(key.to_string(), value))?;
        let entry = self.entries.get_mut(key).expect("key 已在上面确认存在");
        entry.value = value;
        Ok(())
    }

    /// 全部项（按全 key 字典序）。
    pub fn iter(&self) -> impl Iterator<Item = (&str, ParamEntry)> + '_ {
        self.entries
            .iter()
            .map(|(key, entry)| (key.as_str(), *entry))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 参数值哈希：覆盖全部 key + 类型标签 + 当前值。
    ///
    /// 用于存档 / 联机握手的一致性诊断；不替代逐 key 落盘。
    pub fn hash(&self) -> u64 {
        let mut hash = FNV64_OFFSET;
        for (key, entry) in &self.entries {
            hash = fnv1a64(hash, key.as_bytes());
            hash = fnv1a64(hash, &[entry.spec.ty.tag()]);
            hash = fnv1a64(hash, &entry.value.to_le_bytes());
        }
        hash
    }

    /// schema 哈希：只覆盖声明形状（key / 类型 / 范围 / 分组 / 作用域），不含值。
    ///
    /// 用于检测「存档时的 schema 和当前加载的 mod 不一致」。
    pub fn schema_hash(&self) -> u64 {
        let mut hash = FNV64_OFFSET;
        for (key, entry) in &self.entries {
            let spec = entry.spec;
            hash = fnv1a64(hash, key.as_bytes());
            hash = fnv1a64(hash, &[spec.ty.tag(), spec.scope.tag()]);
            hash = fnv1a64(hash, spec.group.as_bytes());
            hash = fnv1a64(hash, &spec.default.to_le_bytes());
            match spec.ty {
                ParamType::Bool => {}
                ParamType::Int { min, max, step } => {
                    hash = fnv1a64(hash, &min.to_le_bytes());
                    hash = fnv1a64(hash, &max.to_le_bytes());
                    hash = fnv1a64(hash, &step.to_le_bytes());
                }
                ParamType::Fixed {
                    scale,
                    min,
                    max,
                    step,
                } => {
                    hash = fnv1a64(hash, &scale.to_le_bytes());
                    hash = fnv1a64(hash, &min.to_le_bytes());
                    hash = fnv1a64(hash, &max.to_le_bytes());
                    hash = fnv1a64(hash, &step.to_le_bytes());
                }
                ParamType::Enum { values } => {
                    for value in values {
                        hash = fnv1a64(hash, value.as_bytes());
                    }
                }
            }
            if let Some(visible_if) = spec.visible_if {
                hash = fnv1a64(hash, visible_if.key.as_bytes());
                hash = fnv1a64(hash, &visible_if.equals.to_le_bytes());
            }
        }
        hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static DEMO: ModManifest = ModManifest {
        id: "test:params",
        version: 1,
        engine_api: 1,
        load_after: &[],
        load_after_optional: &[],
        conflicts: &[],
        exclusive_groups: &[],
        params: &[
            ParamSpec {
                key: "enabled",
                ty: ParamType::Bool,
                default: 1,
                scope: ParamScope::World,
                group: "main",
                visible_if: None,
            },
            ParamSpec {
                key: "speed",
                ty: ParamType::Int {
                    min: 0,
                    max: 10,
                    step: 1,
                },
                default: 5,
                scope: ParamScope::World,
                group: "main",
                visible_if: Some(VisibleIf {
                    key: "enabled",
                    equals: 1,
                }),
            },
            ParamSpec {
                key: "debug_draw",
                ty: ParamType::Bool,
                default: 0,
                scope: ParamScope::Debug,
                group: "",
                visible_if: None,
            },
            ParamSpec {
                key: "mode",
                ty: ParamType::Enum {
                    values: &["a", "b"],
                },
                default: 1,
                scope: ParamScope::World,
                group: "",
                visible_if: None,
            },
            ParamSpec {
                key: "ratio",
                ty: ParamType::Fixed {
                    scale: 100,
                    min: 0,
                    max: 1_000,
                    step: 10,
                },
                default: 500,
                scope: ParamScope::World,
                group: "",
                visible_if: None,
            },
        ],
    };

    #[test]
    fn resolve_uses_defaults_and_overrides() {
        let mut overrides = BTreeMap::new();
        overrides.insert("test:params.speed".to_string(), 7);
        let table = ParamTable::resolve(&[&DEMO], &overrides).expect("参数表应解析成功");

        assert_eq!(table.len(), 5);
        assert_eq!(table.get_mod("test:params", "speed").unwrap().value, 7);
        assert_eq!(table.get_mod("test:params", "enabled").unwrap().value, 1);
        assert_eq!(table.get("test:params.mode").unwrap().value, 1);
        assert_eq!(table.get("test:params.ratio").unwrap().value, 500);
    }

    #[test]
    fn unknown_override_is_rejected() {
        let mut overrides = BTreeMap::new();
        overrides.insert("test:params.nope".to_string(), 1);
        let error = ParamTable::resolve(&[&DEMO], &overrides).unwrap_err();
        assert_eq!(
            error,
            ParamError::UnknownKey {
                key: "test:params.nope".to_string()
            }
        );
    }

    #[test]
    fn duplicate_key_across_manifests_is_rejected() {
        let error = ParamTable::resolve(&[&DEMO, &DEMO], &BTreeMap::new()).unwrap_err();
        assert_eq!(
            error,
            ParamError::DuplicateKey {
                key: "test:params.enabled".to_string()
            }
        );
    }

    #[test]
    fn invalid_default_is_rejected() {
        static BAD: ModManifest = ModManifest {
            id: "test:bad",
            version: 1,
            engine_api: 1,
            load_after: &[],
            load_after_optional: &[],
            conflicts: &[],
            exclusive_groups: &[],
            params: &[ParamSpec {
                key: "flag",
                ty: ParamType::Bool,
                default: 2,
                scope: ParamScope::World,
                group: "",
                visible_if: None,
            }],
        };
        let error = ParamTable::resolve(&[&BAD], &BTreeMap::new()).unwrap_err();
        assert_eq!(
            error,
            ParamError::InvalidDefault {
                key: "test:bad.flag".to_string(),
                value: 2
            }
        );
    }

    #[test]
    fn set_validates_range_bool_and_enum() {
        let mut table = ParamTable::resolve(&[&DEMO], &BTreeMap::new()).unwrap();

        assert_eq!(
            table.set("test:params.speed", 11, false),
            Err(ParamError::OutOfRange {
                key: "test:params.speed".to_string(),
                value: 11
            })
        );
        assert_eq!(
            table.set("test:params.enabled", 2, false),
            Err(ParamError::InvalidBool {
                key: "test:params.enabled".to_string(),
                value: 2
            })
        );
        assert_eq!(
            table.set("test:params.mode", 2, false),
            Err(ParamError::InvalidEnum {
                key: "test:params.mode".to_string(),
                value: 2
            })
        );
        assert!(table.set("test:params.speed", 6, false).is_ok());
        assert_eq!(table.get_mod("test:params", "speed").unwrap().value, 6);
    }

    #[test]
    fn frozen_blocks_world_but_not_debug() {
        let mut table = ParamTable::resolve(&[&DEMO], &BTreeMap::new()).unwrap();

        assert_eq!(
            table.set("test:params.speed", 6, true),
            Err(ParamError::Frozen {
                key: "test:params.speed".to_string()
            })
        );
        assert!(table.set("test:params.debug_draw", 1, true).is_ok());
        assert_eq!(table.get_mod("test:params", "debug_draw").unwrap().value, 1);
    }

    #[test]
    fn hash_tracks_values_while_schema_hash_does_not() {
        let mut table = ParamTable::resolve(&[&DEMO], &BTreeMap::new()).unwrap();
        let value_hash = table.hash();
        let schema_hash = table.schema_hash();

        table.set("test:params.speed", 6, false).unwrap();
        assert_ne!(table.hash(), value_hash);
        assert_eq!(table.schema_hash(), schema_hash);
    }

    #[test]
    fn freeze_plugin_marks_world_params_frozen() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let table = ParamTable::resolve(&[&DEMO], &BTreeMap::new()).unwrap();
        app.insert_resource(table);
        app.add_plugins(ModParamsPlugin);
        app.update();

        assert!(app.world().resource::<WorldParamsFrozen>().0);
    }
}

/// 世界参数是否已经冻结（启动完成后为 `true`）。
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WorldParamsFrozen(pub bool);

fn freeze_world_params(mut frozen: ResMut<WorldParamsFrozen>) {
    frozen.0 = true;
}

/// 参数冻结插件：在 `PostStartup` 把 [`WorldParamsFrozen`] 置为 `true`。
///
/// 装配方必须在 mod `install` 之前插入 [`ParamTable`]，否则 mod 读不到自己的
/// 参数。插件名带 `Mod` 前缀，避免和未来其它参数系统混淆。
pub struct ModParamsPlugin;

impl Plugin for ModParamsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<WorldParamsFrozen>();
        app.add_systems(PostStartup, freeze_world_params);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ValueError {
    OutOfRange,
    InvalidBool,
    InvalidEnum,
}

impl ValueError {
    fn into_param(self, key: String, value: i64) -> ParamError {
        match self {
            ValueError::OutOfRange => ParamError::OutOfRange { key, value },
            ValueError::InvalidBool => ParamError::InvalidBool { key, value },
            ValueError::InvalidEnum => ParamError::InvalidEnum { key, value },
        }
    }
}

const FNV64_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV64_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a64(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV64_PRIME);
    }
    hash
}
