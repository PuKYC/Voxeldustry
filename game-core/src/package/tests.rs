use super::*;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use game_engine::input::ResolvedActions;
use game_engine::modding::{DataRegistry, Mods, ObjectLayers, ResolvedModPayloads};
use game_engine::spec::DefaultSpec;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("系统时钟早于 UNIX_EPOCH")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ecs_game_package_{tag}_{}_{nanos}_{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&path).expect("创建临时目录");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn write_file(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("创建父目录");
    }
    std::fs::write(path, text).expect("写测试文件");
}

fn manifest(extra: &str) -> String {
    format!(
        r#"{{"id":"fishing","version":3,"engine_api":1,"actions":["cast_line"],"presentation":[{{"name":"bobber","fields":[{{"key":1,"name":"state"}},{{"key":2,"name":"secret","visibility":"server_only"}}]}}]{extra}}}"#
    )
}

fn package_with(dir: &Path, manifest_text: &str, data: &[(&str, &str)]) {
    write_file(&dir.join(PACKAGE_MANIFEST_FILE), manifest_text);
    for (relative, text) in data {
        write_file(&dir.join(relative), text);
    }
}

fn valid_patch() -> &'static str {
    r#"{
        // JSONC 注释也允许
        "version": 1,
        "table": "voxel_objects",
        "patch": [{ "name": "stone", "set": { "replaceable": true } }],
    }"#
}

#[test]
fn parses_package_and_joins_mod_pipeline() {
    let dir = TempDir::new("ok");
    let text = manifest(
        r#","data":["data/voxel_objects.jsonc"],"params":[{"key":"bite_time","type":{"kind":"int","min":1,"max":600,"step":1},"default":30,"scope":"world","group":"fishing"},{"key":"debug_aim","type":{"kind":"bool"},"default":0,"scope":"debug"}]"#,
    );
    package_with(
        dir.path(),
        &text,
        &[("data/voxel_objects.jsonc", valid_patch())],
    );

    let package = load_package(dir.path()).expect("包应解析成功");
    assert_eq!(package.id(), "fishing");
    assert_eq!(package.version(), 3);
    assert_eq!(package.manifest.actions, vec!["cast_line"]);
    assert_eq!(package.manifest.params.len(), 2);
    assert_eq!(package.manifest.presentation.len(), 1);
    assert_eq!(package.data.len(), 1);

    let module = package.to_mod::<DefaultSpec>();
    assert_eq!(module.manifest().id, "fishing");
    assert_eq!(module.manifest().params.len(), 2);
    assert_eq!(module.manifest().actions, &["cast_line"]);

    let mut app = App::new();
    let mut data = DataRegistry::new();
    Mods::<DefaultSpec>::new()
        .push(module)
        .apply(&mut app, &mut data)
        .expect("mod 集合应装配成功");

    assert_eq!(app.world().resource::<ObjectLayers>().len(), 1);
    let actions = app.world().resource::<ResolvedActions>();
    assert!(actions.lookup("cast_line").is_some());
    let payloads = app.world().resource::<ResolvedModPayloads>();
    assert_eq!(payloads.kinds().len(), 1);
    assert_eq!(payloads.kinds()[0].name, "bobber");
    assert_eq!(payloads.kinds()[0].fields.len(), 2);
    assert_eq!(payloads.kinds()[0].sub_kind, 1);
}

#[test]
fn loads_lua_scripts_from_package() {
    let dir = TempDir::new("scripts");
    package_with(
        dir.path(),
        &manifest(r#","scripts":["scripts/bobber.lua"]"#),
        &[("scripts/bobber.lua", "function on_update() end")],
    );
    let package = load_package(dir.path()).expect("包应解析成功");
    assert_eq!(package.manifest.scripts, vec!["scripts/bobber.lua"]);
    assert_eq!(package.scripts.len(), 1);
    assert_eq!(package.scripts[0].path, "scripts/bobber.lua");
    assert!(package.scripts[0].source.contains("on_update"));
}

#[test]
fn rejects_bad_script_paths() {
    let dir = TempDir::new("bad_scripts");
    package_with(
        dir.path(),
        &manifest(r#","scripts":["../escape.lua"]"#),
        &[],
    );
    let error = load_package(dir.path()).expect_err("包外路径应拒绝");
    assert_eq!(error.kind(), PackageErrorKind::Schema);

    let dir = TempDir::new("bad_script_ext");
    package_with(
        dir.path(),
        &manifest(r#","scripts":["scripts/a.txt"]"#),
        &[],
    );
    let error = load_package(dir.path()).expect_err("非 .lua 应拒绝");
    assert_eq!(error.kind(), PackageErrorKind::Schema);
    assert!(error.message().contains(".lua"));

    let dir = TempDir::new("missing_script");
    package_with(
        dir.path(),
        &manifest(r#","scripts":["scripts/none.lua"]"#),
        &[],
    );
    let error = load_package(dir.path()).expect_err("缺失脚本应拒绝");
    assert_eq!(error.kind(), PackageErrorKind::Data);
}

#[test]
fn rejects_unknown_manifest_field() {
    let dir = TempDir::new("unknown");
    package_with(dir.path(), &manifest(r#","extra":1"#), &[]);
    let error = load_package(dir.path()).expect_err("未知字段应拒绝");
    assert_eq!(error.kind(), PackageErrorKind::Manifest);
    assert!(error.message().contains("unknown field"), "{}", error);
}

#[test]
fn rejects_engine_api_mismatch() {
    let dir = TempDir::new("api");
    package_with(
        dir.path(),
        r#"{"id":"fishing","version":1,"engine_api":99}"#,
        &[],
    );
    let error = load_package(dir.path()).expect_err("engine_api 不符应拒绝");
    assert_eq!(error.kind(), PackageErrorKind::EngineApi);
    assert_eq!(error.code(), "mod.package.engine_api");
}

#[test]
fn engine_api_range_boundaries() {
    // 区间 [2, 4]：两端含，区间外分别报过旧 / 过新。
    assert!(check_engine_api(2, 2, 4).is_ok());
    assert!(check_engine_api(3, 2, 4).is_ok());
    assert!(check_engine_api(4, 2, 4).is_ok());

    let old = check_engine_api(1, 2, 4).expect_err("低于最低版本应拒绝");
    assert!(old.contains("更新此 mod"), "{old}");
    let new = check_engine_api(5, 2, 4).expect_err("高于当前版本应拒绝");
    assert!(new.contains("更新游戏"), "{new}");

    // min == current 时退化为相等判断（旧行为）。
    assert!(check_engine_api(1, 1, 1).is_ok());
    assert!(check_engine_api(0, 1, 1).is_err());
    assert!(check_engine_api(2, 1, 1).is_err());
}

#[test]
fn accepts_engine_api_inside_supported_range() {
    const { assert!(MIN_SUPPORTED_ENGINE_API <= CURRENT_ENGINE_API) };
    for api in MIN_SUPPORTED_ENGINE_API..=CURRENT_ENGINE_API {
        let dir = TempDir::new("api_ok");
        package_with(
            dir.path(),
            &format!(r#"{{"id":"fishing","version":1,"engine_api":{api}}}"#),
            &[],
        );
        load_package(dir.path()).expect("区间内的 engine_api 应放行");
    }
}

#[test]
fn rejects_engine_api_below_minimum() {
    let dir = TempDir::new("api_zero");
    package_with(
        dir.path(),
        r#"{"id":"fishing","version":1,"engine_api":0}"#,
        &[],
    );
    let error = load_package(dir.path()).expect_err("engine_api 0 低于最低版本");
    assert_eq!(error.kind(), PackageErrorKind::EngineApi);
}

#[test]
fn rejects_missing_and_escaping_data_files() {
    let dir = TempDir::new("data");
    package_with(
        dir.path(),
        &manifest(r#","data":["data/missing.jsonc"]"#),
        &[],
    );
    let error = load_package(dir.path()).expect_err("缺失数据文件应拒绝");
    assert_eq!(error.kind(), PackageErrorKind::Data);
    assert!(error.message().contains("不存在"), "{}", error);

    let dir = TempDir::new("escape");
    package_with(
        dir.path(),
        &manifest(r#","data":["../outside.jsonc"]"#),
        &[],
    );
    let error = load_package(dir.path()).expect_err("越界路径应拒绝");
    assert_eq!(error.kind(), PackageErrorKind::Schema);
    assert!(error.message().contains("相对路径"), "{}", error);
}

#[test]
fn rejects_bad_data_patch() {
    let dir = TempDir::new("badpatch");
    package_with(
        dir.path(),
        &manifest(r#","data":["data/bad.jsonc"]"#),
        &[("data/bad.jsonc", r#"{"table":"no_such_table","patch":[]}"#)],
    );
    let error = load_package(dir.path()).expect_err("未知 table 应拒绝");
    assert_eq!(error.kind(), PackageErrorKind::Data);
    assert!(error.message().contains("未知 table"), "{}", error);
}

#[test]
fn rejects_schema_conflicts() {
    let dir = TempDir::new("dupparam");
    package_with(
        dir.path(),
        r#"{"id":"fishing","version":1,"engine_api":1,"params":[
            {"key":"a","type":{"kind":"bool"},"default":0,"scope":"world"},
            {"key":"a","type":{"kind":"bool"},"default":0,"scope":"world"}
        ]}"#,
        &[],
    );
    let error = load_package(dir.path()).expect_err("重复参数应拒绝");
    assert_eq!(error.kind(), PackageErrorKind::Schema);
    assert!(error.message().contains("重复"), "{}", error);

    let dir = TempDir::new("dupaction");
    package_with(
        dir.path(),
        r#"{"id":"fishing","version":1,"engine_api":1,"actions":["cast","cast"]}"#,
        &[],
    );
    let error = load_package(dir.path()).expect_err("重复动作应拒绝");
    assert!(error.message().contains("动作名"), "{}", error);

    let dir = TempDir::new("visible");
    package_with(
        dir.path(),
        r#"{"id":"fishing","version":1,"engine_api":1,"params":[
            {"key":"a","type":{"kind":"bool"},"default":0,"scope":"world",
             "visible_if":{"key":"missing","equals":1}}
        ]}"#,
        &[],
    );
    let error = load_package(dir.path()).expect_err("visible_if 悬空应拒绝");
    assert!(error.message().contains("visible_if"), "{}", error);
}

#[test]
fn rejects_invalid_presentation_fields() {
    let dir = TempDir::new("presentation");
    package_with(
        dir.path(),
        r#"{"id":"fishing","version":1,"engine_api":1,"presentation":[
            {"name":"bobber","fields":[{"key":1,"name":"secret","visibility":"required"}]}
        ]}"#,
        &[],
    );
    let error = load_package(dir.path()).expect_err("required 缺 bits 应拒绝");
    assert!(error.message().contains("required_bits"), "{}", error);
}

#[test]
fn scans_root_and_rejects_duplicate_ids() {
    let dir = TempDir::new("scan");
    let one = dir.path().join("fishing_a");
    let two = dir.path().join("fishing_b");
    package_with(&one, &manifest(""), &[]);
    package_with(&two, &manifest(""), &[]);
    std::fs::create_dir_all(dir.path().join("assets_only")).expect("创建无关目录");

    let errors = scan_package_root(dir.path()).expect_err("重复 id 应拒绝");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].kind(), PackageErrorKind::Schema);
    assert!(errors[0].message().contains("重复"), "{}", errors[0]);
}

#[test]
fn scans_root_and_sorts_by_id() {
    let dir = TempDir::new("sort");
    package_with(
        &dir.path().join("b"),
        r#"{"id":"zebra","version":1,"engine_api":1}"#,
        &[],
    );
    package_with(
        &dir.path().join("a"),
        r#"{"id":"alpha","version":1,"engine_api":1}"#,
        &[],
    );
    std::fs::create_dir_all(dir.path().join("empty")).expect("创建无关目录");

    let packages = scan_package_root(dir.path()).expect("扫描应成功");
    assert_eq!(
        packages.iter().map(|p| p.id()).collect::<Vec<_>>(),
        vec!["alpha", "zebra"]
    );
}

#[test]
fn scans_single_package_root() {
    let dir = TempDir::new("single");
    package_with(dir.path(), &manifest(""), &[]);
    let packages = scan_package_root(dir.path()).expect("根目录单包应成功");
    assert_eq!(packages.len(), 1);
    assert_eq!(packages[0].id(), "fishing");
}
