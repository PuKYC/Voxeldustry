//! 目录式 mod 包：从磁盘读到 `Mods` 集合的唯一入口。
//!
//! 包格式刻意保持「数据 + 声明」，不加载任何可执行代码：
//!
//! ```text
//! mods/
//!   fishing/
//!     mod.json                清单（严格 JSON，未知字段报错）
//!     data/voxel_objects.jsonc 静态数据补丁（JSONC）
//! ```
//!
//! 解析分两步：先把清单读成**拥有所有权**的 [`PackageManifest`]（可在 FFI 里
//! 持有、可测试），装配时再经 [`LoadedPackage::to_mod`] 转成 `'static` 的
//! [`ModManifest`] 交给 [`game_engine::modding::Mods`]。`Box::leak` 只发生在
//! 冷路径（启动一次），换取与编译期 native mod 完全相同的装配语义。
//!
//! 包不参与存档身份：存档冻结的是 mod 清单 `(id, version)` 与插件装配序列，
//! 包内容改了但 id/version 没变，读档照样放行——发布者必须升 version。

use std::collections::BTreeSet;
use std::fmt;
use std::marker::PhantomData;
use std::path::{Component, Path, PathBuf};

use bevy::prelude::*;
use game_engine::input::ACTION_NAME_MAX_BYTES;
use game_engine::modding::{
    Mod, ModContext, ModFieldSpec, ModFieldVis, ModManifest, ModPayloadSpec, ParamScope, ParamSpec,
    ParamType, VisibleIf,
};
use game_engine::spec::GameSpec;

use crate::static_data::{strip_jsonc, ObjectTableKind};

/// 清单文件名（相对包根目录）。
pub const PACKAGE_MANIFEST_FILE: &str = "mod.json";
/// 当前引擎 API 版本。
///
/// 包清单里的 `engine_api` 表示「按哪一版 API 写的」，落在
/// `[MIN_SUPPORTED_ENGINE_API, CURRENT_ENGINE_API]` 内即放行。
/// 只增不改的扩展（新字段、新载荷）只把它加一；
/// 破坏性变更才同时提高 [`MIN_SUPPORTED_ENGINE_API`]。
pub const CURRENT_ENGINE_API: u32 = 1;
/// 引擎仍然兼容的最低 `engine_api`。低于它的包按「需要更新」拒绝。
pub const MIN_SUPPORTED_ENGINE_API: u32 = 1;
/// 单次扫描的包数量上限。
pub const MAX_PACKAGES: usize = 64;
/// 每个包的数据补丁文件数量上限。
pub const MAX_DATA_FILES: usize = 64;
/// 每个包的 Lua 脚本数量上限。
pub const MAX_SCRIPT_FILES: usize = 64;
/// 单个 Lua 脚本大小上限。
pub const MAX_SCRIPT_FILE_BYTES: u64 = 256 * 1024;
/// 清单文件大小上限。
pub const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
/// 单个数据补丁文件大小上限。
pub const MAX_DATA_FILE_BYTES: u64 = 1024 * 1024;
/// 单个包的数据补丁总大小上限。
pub const MAX_PACKAGE_BYTES: u64 = 8 * 1024 * 1024;
/// mod id / 依赖名 / 动作名 / 表现字段名的通用长度上限（字节）。
pub const MAX_PACKAGE_NAME_BYTES: usize = 64;

/// 包解析失败的大类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageErrorKind {
    /// 文件系统错误（缺目录 / 读失败）。
    Io,
    /// 清单缺失、不是合法 JSON、或字段类型不对。
    Manifest,
    /// 清单语义不合法（重名、越界、非法名字）。
    Schema,
    /// 数据补丁缺失或格式不对。
    Data,
    /// `engine_api` 不在当前引擎支持的区间内（过旧或过新）。
    EngineApi,
}

impl PackageErrorKind {
    /// 跨 FFI / UI 的稳定错误码。
    pub const fn code(self) -> &'static str {
        match self {
            Self::Io => "mod.package.io",
            Self::Manifest => "mod.package.manifest",
            Self::Schema => "mod.package.schema",
            Self::Data => "mod.package.data",
            Self::EngineApi => "mod.package.engine_api",
        }
    }
}

/// 一个包的错误（带包路径，便于 UI 定位）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageError {
    path: PathBuf,
    kind: PackageErrorKind,
    message: String,
}

impl PackageError {
    fn new(path: impl Into<PathBuf>, kind: PackageErrorKind, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind,
            message: message.into(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn kind(&self) -> PackageErrorKind {
        self.kind
    }

    pub fn code(&self) -> &'static str {
        self.kind.code()
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for PackageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mod 包 {}：{}", self.path.display(), self.message)
    }
}

impl std::error::Error for PackageError {}

/// 参数类型（拥有所有权版本，用于反序列化）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackageParamType {
    Bool,
    Int {
        min: i64,
        max: i64,
        step: i64,
    },
    Fixed {
        scale: u32,
        min: i64,
        max: i64,
        step: i64,
    },
    Enum {
        values: Vec<String>,
    },
}

/// 参数作用域。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageParamScope {
    World,
    Debug,
}

/// 一条包参数声明。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageParam {
    pub key: String,
    pub ty: PackageParamType,
    pub default: i64,
    pub scope: PackageParamScope,
    pub group: String,
    pub visible_if: Option<(String, i64)>,
}

/// 一个表现字段声明。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageField {
    pub key: u16,
    pub name: String,
    pub visibility: ModFieldVis,
    pub required_bits: u32,
}

/// 一个表现子种类声明。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackagePayload {
    pub name: String,
    pub fields: Vec<PackageField>,
}

/// 一个 Lua 脚本文件（已读取为文本，尚未编译）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageScript {
    /// 相对包根目录的路径（清单里写的原样）。
    pub path: String,
    /// UTF-8 源码。
    pub source: String,
}

/// 一个数据补丁文件（已校验存在与顶层 schema）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageData {
    /// 相对包根目录的路径（清单里写的原样）。
    pub path: String,
    /// 原始 JSONC 文本，装配时原样登记为 `ObjectLayer`。
    pub text: String,
}

/// 解析后、尚未 `'static` 化的清单。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageManifest {
    pub id: String,
    pub version: u32,
    pub engine_api: u32,
    pub load_after: Vec<String>,
    pub load_after_optional: Vec<String>,
    pub conflicts: Vec<String>,
    pub exclusive_groups: Vec<String>,
    pub actions: Vec<String>,
    pub params: Vec<PackageParam>,
    pub presentation: Vec<PackagePayload>,
    /// 数据补丁路径（相对包根目录，清单里写的原样）。
    pub data: Vec<String>,
    /// Lua 脚本路径（相对包根目录，清单里写的原样）。
    pub scripts: Vec<String>,
}

/// 一个已解析的包。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedPackage {
    /// 包根目录（扫描时得到；FFI 报错 / UI 展示用）。
    pub source: PathBuf,
    pub manifest: PackageManifest,
    pub data: Vec<PackageData>,
    /// 已读取的 Lua 脚本，按清单顺序；装配时交给 `ScriptSources`。
    pub scripts: Vec<PackageScript>,
}

impl LoadedPackage {
    /// 包 id。
    pub fn id(&self) -> &str {
        &self.manifest.id
    }

    /// 版本。
    pub fn version(&self) -> u32 {
        self.manifest.version
    }

    /// 改成 `'static` 的 mod，交给 `Mods::push`。
    ///
    /// **每次调用都会泄漏一份清单与补丁文本**：只应在启动/建档的冷路径调用一次。
    pub fn to_mod<S: GameSpec>(&self) -> PackageMod<S> {
        PackageMod {
            manifest: leak_manifest(&self.manifest),
            layers: self.data.iter().map(|data| leak_str(&data.text)).collect(),
            _spec: PhantomData,
        }
    }
}

/// 目录式包转成的 native mod。
pub struct PackageMod<S: GameSpec> {
    manifest: &'static ModManifest,
    layers: Vec<&'static str>,
    _spec: PhantomData<S>,
}

impl<S: GameSpec> Mod<S> for PackageMod<S> {
    fn manifest(&self) -> &'static ModManifest {
        self.manifest
    }

    fn install(&self, ctx: &mut ModContext<'_, S>) {
        for layer in &self.layers {
            ctx.add_object_layer(layer);
        }
    }
}

/// 读一个包目录。目录里必须正好有 [`PACKAGE_MANIFEST_FILE`]。
pub fn load_package(dir: &Path) -> Result<LoadedPackage, PackageError> {
    let manifest_path = dir.join(PACKAGE_MANIFEST_FILE);
    let metadata = std::fs::metadata(&manifest_path).map_err(|error| {
        PackageError::new(
            &manifest_path,
            PackageErrorKind::Io,
            format!("读不到 {PACKAGE_MANIFEST_FILE}：{error}"),
        )
    })?;
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(PackageError::new(
            &manifest_path,
            PackageErrorKind::Manifest,
            format!("清单超过 {MAX_MANIFEST_BYTES} 字节"),
        ));
    }
    let text = std::fs::read_to_string(&manifest_path).map_err(|error| {
        PackageError::new(
            &manifest_path,
            PackageErrorKind::Io,
            format!("读取清单失败：{error}"),
        )
    })?;
    let raw: RawManifest = serde_json::from_str(&text).map_err(|error| {
        PackageError::new(
            &manifest_path,
            PackageErrorKind::Manifest,
            format!("清单不是合法 JSON：{error}"),
        )
    })?;
    let manifest = validate_manifest(&manifest_path, raw)?;

    let mut data = Vec::with_capacity(manifest.data.len());
    let mut total = 0u64;
    for relative in &manifest.data {
        let relative = Path::new(relative);
        let path = resolve_data_path(dir, relative)?;
        let metadata = std::fs::metadata(&path).map_err(|error| {
            PackageError::new(
                &path,
                PackageErrorKind::Data,
                format!("数据补丁 {} 不存在：{error}", relative.display()),
            )
        })?;
        if metadata.len() > MAX_DATA_FILE_BYTES {
            return Err(PackageError::new(
                &path,
                PackageErrorKind::Data,
                format!("数据补丁超过 {MAX_DATA_FILE_BYTES} 字节"),
            ));
        }
        total += metadata.len();
        if total > MAX_PACKAGE_BYTES {
            return Err(PackageError::new(
                &path,
                PackageErrorKind::Data,
                format!("包内数据补丁总大小超过 {MAX_PACKAGE_BYTES} 字节"),
            ));
        }
        let text = std::fs::read_to_string(&path).map_err(|error| {
            PackageError::new(&path, PackageErrorKind::Data, format!("读取失败：{error}"))
        })?;
        validate_data_patch(&path, &text)?;
        data.push(PackageData {
            path: relative.display().to_string(),
            text,
        });
    }

    let mut scripts = Vec::with_capacity(manifest.scripts.len());
    for relative in &manifest.scripts {
        let relative_path = Path::new(relative);
        let path = resolve_data_path(dir, relative_path)?;
        let metadata = std::fs::metadata(&path).map_err(|error| {
            PackageError::new(
                &path,
                PackageErrorKind::Data,
                format!("脚本 {} 不存在：{error}", relative_path.display()),
            )
        })?;
        if metadata.len() > MAX_SCRIPT_FILE_BYTES {
            return Err(PackageError::new(
                &path,
                PackageErrorKind::Data,
                format!("脚本超过 {MAX_SCRIPT_FILE_BYTES} 字节"),
            ));
        }
        let source = std::fs::read_to_string(&path).map_err(|error| {
            PackageError::new(
                &path,
                PackageErrorKind::Data,
                format!("读取脚本失败：{error}"),
            )
        })?;
        scripts.push(PackageScript {
            path: relative.clone(),
            source,
        });
    }

    Ok(LoadedPackage {
        source: dir.to_path_buf(),
        manifest,
        data,
        scripts,
    })
}

/// 扫描一个 mods 根目录。
///
/// - 根目录本身有 `mod.json` → 当成单个包；
/// - 否则把每个**子目录**当成包，没有 `mod.json` 的子目录跳过；
/// - 任一包失败 → 整体失败（半套 mod 比缺 mod 更危险）。
pub fn scan_package_root(root: &Path) -> Result<Vec<LoadedPackage>, Vec<PackageError>> {
    if root.join(PACKAGE_MANIFEST_FILE).is_file() {
        return load_package(root)
            .map(|package| vec![package])
            .map_err(|error| vec![error]);
    }
    let entries = std::fs::read_dir(root).map_err(|error| {
        vec![PackageError::new(
            root,
            PackageErrorKind::Io,
            format!("读不到 mods 目录：{error}"),
        )]
    })?;
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            vec![PackageError::new(
                root,
                PackageErrorKind::Io,
                format!("遍历 mods 目录失败：{error}"),
            )]
        })?;
        let path = entry.path();
        if path.is_dir() && path.join(PACKAGE_MANIFEST_FILE).is_file() {
            dirs.push(path);
        }
    }
    dirs.sort();
    if dirs.len() > MAX_PACKAGES {
        return Err(vec![PackageError::new(
            root,
            PackageErrorKind::Schema,
            format!("mod 包数量 {} 超过上限 {MAX_PACKAGES}", dirs.len()),
        )]);
    }

    let mut packages = Vec::with_capacity(dirs.len());
    let mut errors = Vec::new();
    for dir in dirs {
        match load_package(&dir) {
            Ok(package) => packages.push(package),
            Err(error) => errors.push(error),
        }
    }
    if !errors.is_empty() {
        errors.sort_by(|a, b| a.path.cmp(&b.path));
        return Err(errors);
    }

    packages.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
    let mut seen = BTreeSet::new();
    for package in &packages {
        if !seen.insert(package.manifest.id.clone()) {
            return Err(vec![PackageError::new(
                &package.source,
                PackageErrorKind::Schema,
                format!("mod id \"{}\" 重复", package.manifest.id),
            )]);
        }
    }
    Ok(packages)
}

// ─────────────────────────── 反序列化模型 ───────────────────────────

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    id: String,
    version: u32,
    engine_api: u32,
    #[serde(default)]
    load_after: Vec<String>,
    #[serde(default)]
    load_after_optional: Vec<String>,
    #[serde(default)]
    conflicts: Vec<String>,
    #[serde(default)]
    exclusive_groups: Vec<String>,
    #[serde(default)]
    actions: Vec<String>,
    #[serde(default)]
    params: Vec<RawParam>,
    #[serde(default)]
    presentation: Vec<RawPayload>,
    #[serde(default)]
    data: Vec<PathBuf>,
    #[serde(default)]
    scripts: Vec<PathBuf>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawParam {
    key: String,
    #[serde(rename = "type")]
    ty: RawParamType,
    default: i64,
    scope: RawScope,
    #[serde(default)]
    group: String,
    #[serde(default)]
    visible_if: Option<RawVisibleIf>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RawParamType {
    Bool,
    Int {
        min: i64,
        max: i64,
        #[serde(default = "default_step")]
        step: i64,
    },
    Fixed {
        scale: u32,
        min: i64,
        max: i64,
        #[serde(default = "default_step")]
        step: i64,
    },
    Enum {
        values: Vec<String>,
    },
}

fn default_step() -> i64 {
    1
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawScope {
    World,
    Debug,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVisibleIf {
    key: String,
    equals: i64,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPayload {
    name: String,
    #[serde(default)]
    fields: Vec<RawField>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawField {
    key: u16,
    name: String,
    #[serde(default)]
    visibility: RawVisibility,
    #[serde(default)]
    required_bits: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawVisibility {
    #[default]
    All,
    Required,
    ServerOnly,
}

// ─────────────────────────── 校验 ───────────────────────────

/// `engine_api` 区间判定。拆成纯函数，便于用任意区间做边界测试。
///
/// 过旧与过新给出不同文案：前者提示作者更新 mod，后者提示玩家更新游戏。
fn check_engine_api(declared: u32, min: u32, current: u32) -> Result<(), String> {
    if declared < min {
        return Err(format!(
            "清单 engine_api {declared} 低于当前引擎支持的最低版本 {min}，需要更新此 mod"
        ));
    }
    if declared > current {
        return Err(format!(
            "清单 engine_api {declared} 高于当前引擎版本 {current}，需要更新游戏"
        ));
    }
    Ok(())
}

fn validate_manifest(path: &Path, raw: RawManifest) -> Result<PackageManifest, PackageError> {
    let schema = |message: String| PackageError::new(path, PackageErrorKind::Schema, message);

    validate_name(&raw.id).map_err(|what| schema(format!("mod id {}", what)))?;
    if raw.version == 0 {
        return Err(schema("version 必须大于 0".to_string()));
    }
    check_engine_api(raw.engine_api, MIN_SUPPORTED_ENGINE_API, CURRENT_ENGINE_API)
        .map_err(|message| PackageError::new(path, PackageErrorKind::EngineApi, message))?;

    validate_name_list(&raw.load_after, "load_after", &schema)?;
    validate_name_list(&raw.load_after_optional, "load_after_optional", &schema)?;
    validate_name_list(&raw.conflicts, "conflicts", &schema)?;
    validate_name_list(&raw.exclusive_groups, "exclusive_groups", &schema)?;
    if raw.load_after.iter().any(|name| name == &raw.id)
        || raw.load_after_optional.iter().any(|name| name == &raw.id)
    {
        return Err(schema("load_after 不能引用自己".to_string()));
    }
    if raw.conflicts.iter().any(|name| name == &raw.id) {
        return Err(schema("conflicts 不能引用自己".to_string()));
    }

    let mut actions = Vec::with_capacity(raw.actions.len());
    let mut action_names = BTreeSet::new();
    for action in raw.actions {
        if action.is_empty()
            || action.len() > ACTION_NAME_MAX_BYTES
            || action.chars().any(char::is_control)
        {
            return Err(schema(format!(
                "动作名 \"{action}\" 非法（必须非空、不超过 {ACTION_NAME_MAX_BYTES} 字节、且不含控制字符）"
            )));
        }
        if !action_names.insert(action.clone()) {
            return Err(schema(format!("动作名 \"{action}\" 在包内重复")));
        }
        actions.push(action);
    }

    let mut params: Vec<PackageParam> = Vec::with_capacity(raw.params.len());
    let mut param_keys = BTreeSet::new();
    for param in raw.params {
        validate_package_param(&param).map_err(&schema)?;
        if !param_keys.insert(param.key.clone()) {
            return Err(schema(format!("参数 key \"{}\" 在包内重复", param.key)));
        }
        let RawParam {
            key,
            ty,
            default,
            scope,
            group,
            visible_if,
        } = param;
        let ty = match ty {
            RawParamType::Bool => PackageParamType::Bool,
            RawParamType::Int { min, max, step } => PackageParamType::Int { min, max, step },
            RawParamType::Fixed {
                scale,
                min,
                max,
                step,
            } => PackageParamType::Fixed {
                scale,
                min,
                max,
                step,
            },
            RawParamType::Enum { values } => PackageParamType::Enum { values },
        };
        let scope = match scope {
            RawScope::World => PackageParamScope::World,
            RawScope::Debug => PackageParamScope::Debug,
        };
        params.push(PackageParam {
            key,
            ty,
            default,
            scope,
            group,
            visible_if: visible_if.map(|visible| (visible.key, visible.equals)),
        });
    }
    for param in &params {
        if let Some((key, _)) = &param.visible_if {
            if !param_keys.contains(key) {
                return Err(schema(format!(
                    "参数 \"{}\" 的 visible_if 引用了未声明的 key \"{key}\"",
                    param.key
                )));
            }
        }
    }

    let mut presentation = Vec::with_capacity(raw.presentation.len());
    let mut payload_names = BTreeSet::new();
    for payload in raw.presentation {
        if payload.name.is_empty() || payload.name.len() > MAX_PACKAGE_NAME_BYTES {
            return Err(schema(format!(
                "表现子种类名 \"{}\" 必须非空且不超过 {MAX_PACKAGE_NAME_BYTES} 字节",
                payload.name
            )));
        }
        if !payload_names.insert(payload.name.clone()) {
            return Err(schema(format!(
                "表现子种类 \"{}\" 在包内重复",
                payload.name
            )));
        }
        let mut field_keys = BTreeSet::new();
        let mut field_names = BTreeSet::new();
        let mut fields = Vec::with_capacity(payload.fields.len());
        for field in payload.fields {
            if field.name.is_empty() || field.name.len() > MAX_PACKAGE_NAME_BYTES {
                return Err(schema(format!(
                    "表现字段名 \"{}\" 必须非空且不超过 {MAX_PACKAGE_NAME_BYTES} 字节",
                    field.name
                )));
            }
            if !field_keys.insert(field.key) {
                return Err(schema(format!(
                    "表现子种类 \"{}\" 的字段 key {} 重复",
                    payload.name, field.key
                )));
            }
            if !field_names.insert(field.name.clone()) {
                return Err(schema(format!(
                    "表现子种类 \"{}\" 的字段名 \"{}\" 重复",
                    payload.name, field.name
                )));
            }
            let (visibility, required_bits) = match field.visibility {
                RawVisibility::All => (ModFieldVis::All, 0),
                RawVisibility::ServerOnly => (ModFieldVis::ServerOnly, 0),
                RawVisibility::Required => {
                    if field.required_bits == 0 {
                        return Err(schema(format!(
                            "表现字段 \"{}\" 是 required，但 required_bits 为 0",
                            field.name
                        )));
                    }
                    (ModFieldVis::Required, field.required_bits)
                }
            };
            if field.visibility != RawVisibility::Required && field.required_bits != 0 {
                return Err(schema(format!(
                    "表现字段 \"{}\" 不是 required，但 required_bits 非 0",
                    field.name
                )));
            }
            fields.push(PackageField {
                key: field.key,
                name: field.name,
                visibility,
                required_bits,
            });
        }
        presentation.push(PackagePayload {
            name: payload.name,
            fields,
        });
    }

    if raw.data.len() > MAX_DATA_FILES {
        return Err(schema(format!(
            "数据补丁数量 {} 超过上限 {MAX_DATA_FILES}",
            raw.data.len()
        )));
    }
    let mut data = Vec::with_capacity(raw.data.len());
    let mut seen_data = BTreeSet::new();
    for path in raw.data {
        if path.as_os_str().is_empty()
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(schema(format!(
                "数据补丁路径 \"{}\" 必须是包内相对路径",
                path.display()
            )));
        }
        let text = path.to_string_lossy().to_string();
        if !seen_data.insert(text.clone()) {
            return Err(schema(format!("数据补丁路径 \"{text}\" 重复")));
        }
        data.push(text);
    }

    if raw.scripts.len() > MAX_SCRIPT_FILES {
        return Err(schema(format!(
            "脚本数量 {} 超过上限 {MAX_SCRIPT_FILES}",
            raw.scripts.len()
        )));
    }
    let mut scripts = Vec::with_capacity(raw.scripts.len());
    let mut seen_scripts = BTreeSet::new();
    for path in raw.scripts {
        if path.as_os_str().is_empty()
            || path
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(schema(format!(
                "脚本路径 \"{}\" 必须是包内相对路径",
                path.display()
            )));
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("lua") {
            return Err(schema(format!(
                "脚本路径 \"{}\" 必须以 .lua 结尾",
                path.display()
            )));
        }
        let text = path.to_string_lossy().to_string();
        if !seen_scripts.insert(text.clone()) {
            return Err(schema(format!("脚本路径 \"{text}\" 重复")));
        }
        scripts.push(text);
    }

    Ok(PackageManifest {
        id: raw.id,
        version: raw.version,
        engine_api: raw.engine_api,
        load_after: raw.load_after,
        load_after_optional: raw.load_after_optional,
        conflicts: raw.conflicts,
        exclusive_groups: raw.exclusive_groups,
        actions,
        params,
        presentation,
        data,
        scripts,
    })
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_PACKAGE_NAME_BYTES {
        return Err(format!("必须非空且不超过 {MAX_PACKAGE_NAME_BYTES} 字节"));
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        return Err("只能包含 ASCII 字母、数字、下划线、短横线和点".to_string());
    }
    Ok(())
}

fn validate_name_list(
    values: &[String],
    field: &str,
    schema: &impl Fn(String) -> PackageError,
) -> Result<(), PackageError> {
    let mut seen = BTreeSet::new();
    for value in values {
        validate_name(value).map_err(|what| schema(format!("{field} 里的 \"{value}\" {what}")))?;
        if !seen.insert(value.clone()) {
            return Err(schema(format!("{field} 里的 \"{value}\" 重复")));
        }
    }
    Ok(())
}

fn validate_package_param(param: &RawParam) -> Result<(), String> {
    validate_name(&param.key).map_err(|what| format!("参数 key \"{}\" {what}", param.key))?;
    if param.group.len() > MAX_PACKAGE_NAME_BYTES {
        return Err(format!(
            "参数 \"{}\" 的 group 超过 {MAX_PACKAGE_NAME_BYTES} 字节",
            param.key
        ));
    }
    let value_in_range = |value: i64, min: i64, max: i64| value >= min && value <= max;
    match &param.ty {
        RawParamType::Bool => {
            if param.default != 0 && param.default != 1 {
                return Err(format!("布尔参数 \"{}\" 的默认值必须是 0 或 1", param.key));
            }
        }
        RawParamType::Int { min, max, step } => {
            if min > max {
                return Err(format!("参数 \"{}\" 的 min 大于 max", param.key));
            }
            if *step <= 0 {
                return Err(format!("参数 \"{}\" 的 step 必须大于 0", param.key));
            }
            if !value_in_range(param.default, *min, *max) {
                return Err(format!("参数 \"{}\" 的默认值不在范围内", param.key));
            }
        }
        RawParamType::Fixed {
            scale,
            min,
            max,
            step,
        } => {
            if *scale == 0 {
                return Err(format!("参数 \"{}\" 的 scale 必须大于 0", param.key));
            }
            if min > max {
                return Err(format!("参数 \"{}\" 的 min 大于 max", param.key));
            }
            if *step <= 0 {
                return Err(format!("参数 \"{}\" 的 step 必须大于 0", param.key));
            }
            if !value_in_range(param.default, *min, *max) {
                return Err(format!("参数 \"{}\" 的默认值不在范围内", param.key));
            }
        }
        RawParamType::Enum { values } => {
            if values.is_empty() {
                return Err(format!("枚举参数 \"{}\" 至少需要一个值", param.key));
            }
            if values
                .iter()
                .any(|value| value.is_empty() || value.len() > MAX_PACKAGE_NAME_BYTES)
            {
                return Err(format!(
                    "枚举参数 \"{}\" 的值必须非空且不超过 {MAX_PACKAGE_NAME_BYTES} 字节",
                    param.key
                ));
            }
            if param.default < 0 || param.default as usize >= values.len() {
                return Err(format!("枚举参数 \"{}\" 的默认值下标越界", param.key));
            }
        }
    }
    if let Some(visible) = &param.visible_if {
        validate_name(&visible.key)
            .map_err(|what| format!("参数 \"{}\" 的 visible_if.key {what}", param.key))?;
    }
    Ok(())
}

fn resolve_data_path(dir: &Path, relative: &Path) -> Result<PathBuf, PackageError> {
    let path = dir.join(relative);
    let canonical_dir = std::fs::canonicalize(dir).map_err(|error| {
        PackageError::new(
            dir,
            PackageErrorKind::Io,
            format!("无法解析包目录：{error}"),
        )
    })?;
    let canonical = std::fs::canonicalize(&path).map_err(|error| {
        PackageError::new(
            &path,
            PackageErrorKind::Data,
            format!("数据补丁 {} 不存在：{error}", relative.display()),
        )
    })?;
    if !canonical.starts_with(&canonical_dir) {
        return Err(PackageError::new(
            &path,
            PackageErrorKind::Data,
            format!("数据补丁 {} 指向包目录之外", relative.display()),
        ));
    }
    Ok(path)
}

fn validate_data_patch(path: &Path, text: &str) -> Result<(), PackageError> {
    let value: serde_json::Value = serde_json::from_str(&strip_jsonc(text)).map_err(|error| {
        PackageError::new(
            path,
            PackageErrorKind::Data,
            format!("补丁不是合法 JSONC：{error}"),
        )
    })?;
    let object = value
        .as_object()
        .ok_or_else(|| PackageError::new(path, PackageErrorKind::Data, "补丁顶层必须是对象"))?;
    let table = object
        .get("table")
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            PackageError::new(path, PackageErrorKind::Data, "补丁缺少字符串字段 table")
        })?;
    if ObjectTableKind::parse(table).is_none() {
        return Err(PackageError::new(
            path,
            PackageErrorKind::Data,
            format!("未知 table \"{table}\""),
        ));
    }
    if !object.get("patch").is_some_and(|value| value.is_array()) {
        return Err(PackageError::new(
            path,
            PackageErrorKind::Data,
            "补丁缺少数组字段 patch",
        ));
    }
    Ok(())
}

// ─────────────────────────── 泄漏成 'static ───────────────────────────

fn leak_str(text: &str) -> &'static str {
    Box::leak(text.to_string().into_boxed_str())
}

fn leak_slice<T>(values: Vec<T>) -> &'static [T] {
    Box::leak(values.into_boxed_slice())
}

fn leak_str_slice(values: &[String]) -> &'static [&'static str] {
    leak_slice(values.iter().map(|value| leak_str(value)).collect())
}

fn leak_manifest(manifest: &PackageManifest) -> &'static ModManifest {
    let params = manifest
        .params
        .iter()
        .map(|param| ParamSpec {
            key: leak_str(&param.key),
            ty: match &param.ty {
                PackageParamType::Bool => ParamType::Bool,
                PackageParamType::Int { min, max, step } => ParamType::Int {
                    min: *min,
                    max: *max,
                    step: *step,
                },
                PackageParamType::Fixed {
                    scale,
                    min,
                    max,
                    step,
                } => ParamType::Fixed {
                    scale: *scale,
                    min: *min,
                    max: *max,
                    step: *step,
                },
                PackageParamType::Enum { values } => ParamType::Enum {
                    values: leak_str_slice(values),
                },
            },
            default: param.default,
            scope: match param.scope {
                PackageParamScope::World => ParamScope::World,
                PackageParamScope::Debug => ParamScope::Debug,
            },
            group: leak_str(&param.group),
            visible_if: param.visible_if.as_ref().map(|(key, equals)| VisibleIf {
                key: leak_str(key),
                equals: *equals,
            }),
        })
        .collect();
    let presentation = manifest
        .presentation
        .iter()
        .map(|payload| ModPayloadSpec {
            name: leak_str(&payload.name),
            fields: leak_slice(
                payload
                    .fields
                    .iter()
                    .map(|field| ModFieldSpec {
                        key: field.key,
                        name: leak_str(&field.name),
                        visibility: field.visibility,
                        required_bits: field.required_bits,
                    })
                    .collect(),
            ),
        })
        .collect();
    Box::leak(Box::new(ModManifest {
        id: leak_str(&manifest.id),
        version: manifest.version,
        engine_api: manifest.engine_api,
        load_after: leak_str_slice(&manifest.load_after),
        load_after_optional: leak_str_slice(&manifest.load_after_optional),
        conflicts: leak_str_slice(&manifest.conflicts),
        exclusive_groups: leak_str_slice(&manifest.exclusive_groups),
        params: leak_slice(params),
        actions: leak_str_slice(&manifest.actions),
        presentation: leak_slice(presentation),
    }))
}

#[cfg(test)]
mod tests;
