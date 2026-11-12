//! 目录式 mod 包的导入验证（P0-2）。
//!
//! 目标：给定一个 [LoadedPackage](crate::package::LoadedPackage)，在无头环境里
//! 用同一份插件 profile 装配两个互相隔离的 [App]，跑固定 tick 数并逐 tick 比较
//! 世界校验和；在第一个 App 的指定 tick 做一次快照 / 回滚 / 重跑，检查重跑结果
//! 与“未回滚的未来”是否一致。所有失败都写进 [VerificationReport]，不 panic。
//!
//! # 覆盖边界（刻意保守）
//!
//! - 只证明**本机、这组固定输入**下一致；不证明跨平台（浮点 / 哈希实现差异）一致。
//! - **不执行 Lua 脚本**：BMS 脚本层不参与本轮验证（scripting feature 默认关闭，
//!   即使开启本模块也不会触发脚本加载）。
//! - 回滚快照只覆盖带 [StableEntityId] 的实体的 [Transform] / [Velocity] 与
//!   [WorldRng] 资源。mod 自定义资源、命令队列、事件、调度器内部状态都不在快照
//!   里；因此“回滚窗内修改了自定义资源”的包会被判为不等价——这是刻意的保守策略，
//!   因为本模块无法知道第三方资源的语义。
//! - 回滚窗内**新生成**的带 StableEntityId 实体会被清理；窗内**消失**的实体无法
//!   恢复，会记 verify.rollback_entity_missing 并判不等价。
//! - 验证 App 是隔离的：不调用 publish_active_actions / publish_mod_payloads，
//!   不写存档目录、不碰 SaveRoot。
//!
//! # 缓存
//!
//! [VerificationCache] 的键是 (mod_id, version, package_hash, engine_api,
//! engine_version)；**不包含 [VerifyOptions]**。所以缓存只应服务于默认 / 固定
//! options 的重复验证；调用方若改变 ticks / seed，应使用独立缓存或绕过缓存。
//!
//! # first_divergence_tick
//!
//! 采用 **1-based** tick 编号（第 1 个记录 tick 为 1）；None 表示未分叉。

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use bevy::prelude::*;
use bevy::time::TimeUpdateStrategy;

use crate::app::{CoreGame, WorldBuildError};
use crate::gameplay::Velocity;
use crate::mods::CoreMods;
use crate::package::{
    LoadedPackage, PackageError, PackageParamScope, PackageParamType, CURRENT_ENGINE_API,
};
use crate::spec::CoreSpec;
use crate::verify::simulation_checksum;
use crate::world::{default_plugin_selection, WorldRng};
use game_engine::identity::StableEntityId;
use game_engine::modding::ModFieldVis;

/// 包哈希域前缀（改变哈希语义时递增）。
const HASH_DOMAIN: &str = "voxeldustry.package.v1";
/// FNV-1a 64 初始 offset basis。
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64 prime。
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
/// 探针实体数量。
const PROBE_COUNT: u64 = 8;
/// 探针 StableEntityId 基址（远离正常分配区间）。
const PROBE_ID_BASE: u64 = 0xFFFF_0000;

/// 验证选项。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerifyOptions {
    /// 记录并比较的 tick 数（默认 120）。
    pub ticks: u32,
    /// 在第几个记录 tick 结束后做回滚快照（默认 60，1-based）。
    pub rollback_at: u32,
    /// 回滚窗长度：快照后继续跑多少 tick 再恢复重跑（默认 30）。
    pub rollback_ticks: u32,
    /// WorldRng 固定种子（默认固定值）。
    pub seed: u64,
}

impl Default for VerifyOptions {
    fn default() -> Self {
        Self {
            ticks: 120,
            rollback_at: 60,
            rollback_ticks: 30,
            seed: 0x5EED_0BAD_C0DE_F00D,
        }
    }
}

/// 单个验证错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationError {
    /// 稳定错误码（verify.* 或 WorldBuildError::code() / PackageError::code()）。
    pub code: String,
    /// 中文可读消息。
    pub message: String,
    /// 相关 tick（1-based；与 tick 无关时为 None）。
    pub tick: Option<u32>,
}

/// 验证报告（字段名固定，FFI 直接按名取值）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationReport {
    pub mod_id: String,
    pub version: u32,
    pub package_hash: u64,
    pub engine_api: u32,
    pub engine_version: String,
    pub ok: bool,
    pub deterministic: bool,
    pub rollback_equivalent: bool,
    pub ticks: u32,
    /// 被测包（第一个 App）的逐 tick 校验和；长度 = ticks。
    pub hashes: Vec<u64>,
    /// 第一处双跑分叉的 1-based tick；None 表示未分叉。
    pub first_divergence_tick: Option<u32>,
    pub errors: Vec<VerificationError>,
    /// 是否命中 [VerificationCache]。
    pub cached: bool,
}

/// 包哈希：FNV-1a 64，混入 manifest 语义字段 + 每个已声明 data/script 的
/// 相对路径与字节。未声明文件不参与。
pub fn package_hash(package: &LoadedPackage) -> u64 {
    let m = &package.manifest;
    let mut h = fnv_bytes(FNV_OFFSET, HASH_DOMAIN.as_bytes());
    h = hash_str(h, &m.id);
    h = hash_u32(h, m.version);
    h = hash_u32(h, m.engine_api);
    h = hash_str_vec(h, &m.load_after);
    h = hash_str_vec(h, &m.load_after_optional);
    h = hash_str_vec(h, &m.conflicts);
    h = hash_str_vec(h, &m.exclusive_groups);
    h = hash_str_vec(h, &m.actions);

    h = hash_u32(h, m.params.len() as u32);
    for param in &m.params {
        h = hash_str(h, &param.key);
        h = hash_param_type(h, &param.ty);
        h = hash_i64(h, param.default);
        h = hash_u32(
            h,
            match param.scope {
                PackageParamScope::World => 0,
                PackageParamScope::Debug => 1,
            },
        );
        h = hash_str(h, &param.group);
        match &param.visible_if {
            Some((key, value)) => {
                h = hash_u32(h, 1);
                h = hash_str(h, key);
                h = hash_i64(h, *value);
            }
            None => h = hash_u32(h, 0),
        }
    }

    h = hash_u32(h, m.presentation.len() as u32);
    for payload in &m.presentation {
        h = hash_str(h, &payload.name);
        h = hash_u32(h, payload.fields.len() as u32);
        for field in &payload.fields {
            h = hash_u32(h, field.key as u32);
            h = hash_str(h, &field.name);
            h = hash_u32(
                h,
                match field.visibility {
                    ModFieldVis::All => 0,
                    ModFieldVis::Required => 1,
                    ModFieldVis::ServerOnly => 2,
                },
            );
            h = hash_u32(h, field.required_bits);
        }
    }

    h = hash_str_vec(h, &m.data);
    h = hash_str_vec(h, &m.scripts);

    // 已声明文件的实际内容（按 LoadedPackage 的读取顺序，即 manifest 声明顺序）。
    h = hash_u32(h, package.data.len() as u32);
    for data in &package.data {
        h = hash_str(h, &data.path);
        h = hash_str(h, &data.text);
    }
    h = hash_u32(h, package.scripts.len() as u32);
    for script in &package.scripts {
        h = hash_str(h, &script.path);
        h = hash_str(h, &script.source);
    }
    h
}

/// 包哈希的 16 位小写十六进制表示。
pub fn package_hash_hex(hash: u64) -> String {
    format!("{hash:016x}")
}

/// 把 load_package 的错误包装成报告形状（FFI 坏包路径复用）。
pub fn package_error_report(path: &std::path::Path, error: &PackageError) -> VerificationReport {
    VerificationReport {
        mod_id: String::new(),
        version: 0,
        package_hash: 0,
        engine_api: CURRENT_ENGINE_API,
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        ok: false,
        deterministic: false,
        rollback_equivalent: false,
        ticks: 0,
        hashes: Vec::new(),
        first_divergence_tick: None,
        errors: vec![VerificationError {
            code: error.code().to_string(),
            message: format!("mod 包 {}：{}", path.display(), error.message()),
            tick: None,
        }],
        cached: false,
    }
}

/// 验证缓存键。
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct VerificationCacheKey {
    pub mod_id: String,
    pub version: u32,
    pub package_hash: u64,
    pub engine_api: u32,
    pub engine_version: String,
}

impl VerificationCacheKey {
    /// 从包与已算好的哈希构造。
    pub fn new(package: &LoadedPackage, package_hash: u64) -> Self {
        Self {
            mod_id: package.id().to_string(),
            version: package.version(),
            package_hash,
            engine_api: CURRENT_ENGINE_API,
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// 进程内验证缓存（按包 + 引擎版本失效）。
#[derive(Clone, Debug, Default)]
pub struct VerificationCache {
    entries: BTreeMap<VerificationCacheKey, VerificationReport>,
}

impl VerificationCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &VerificationCacheKey) -> Option<&VerificationReport> {
        self.entries.get(key)
    }

    pub fn insert(&mut self, key: VerificationCacheKey, report: VerificationReport) {
        self.entries.insert(key, report);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// 无缓存验证入口。
pub fn verify_package(
    package: &LoadedPackage,
    context: &[LoadedPackage],
    options: VerifyOptions,
) -> VerificationReport {
    verify_package_with_mods(package, options, package_mods(package, context))
}

/// 带缓存的验证入口。命中时返回 clone 且 cached = true，不重建 App。
pub fn verify_cached(
    package: &LoadedPackage,
    context: &[LoadedPackage],
    options: VerifyOptions,
    cache: &mut VerificationCache,
) -> VerificationReport {
    verify_cached_impl(package, options, cache, || package_mods(package, context))
}

fn verify_cached_impl<F: FnOnce() -> CoreMods>(
    package: &LoadedPackage,
    options: VerifyOptions,
    cache: &mut VerificationCache,
    make_mods: F,
) -> VerificationReport {
    let hash = package_hash(package);
    let key = VerificationCacheKey::new(package, hash);
    if let Some(hit) = cache.get(&key) {
        let mut report = hit.clone();
        report.cached = true;
        return report;
    }
    let mut report = verify_package_with_mods(package, options, make_mods());
    report.cached = false;
    cache.insert(key, report.clone());
    report
}

/// 构造 mod 集合：被测包 + context（按 id 去重，避免重复装配）。
fn package_mods(package: &LoadedPackage, context: &[LoadedPackage]) -> CoreMods {
    let mut mods = CoreMods::new();
    let mut seen = BTreeSet::new();
    seen.insert(package.id().to_string());
    mods = mods.push(package.to_mod::<CoreSpec>());
    for candidate in context {
        if seen.insert(candidate.id().to_string()) {
            mods = mods.push(candidate.to_mod::<CoreSpec>());
        }
    }
    mods
}

fn base_report(package: &LoadedPackage, hash: u64) -> VerificationReport {
    VerificationReport {
        mod_id: package.id().to_string(),
        version: package.version(),
        package_hash: hash,
        engine_api: CURRENT_ENGINE_API,
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        ok: false,
        deterministic: true,
        rollback_equivalent: true,
        ticks: 0,
        hashes: Vec::new(),
        first_divergence_tick: None,
        errors: Vec::new(),
        cached: false,
    }
}

fn verify_package_with_mods(
    package: &LoadedPackage,
    options: VerifyOptions,
    mods: CoreMods,
) -> VerificationReport {
    let hash = package_hash(package);
    let mut report = base_report(package, hash);

    let mut app_a = match build_verification_app(mods.clone(), options.seed) {
        Ok(app) => app,
        Err(error) => {
            push_build_error(&mut report, &error);
            return report;
        }
    };
    let mut app_b = match build_verification_app(mods, options.seed) {
        Ok(app) => app,
        Err(error) => {
            push_build_error(&mut report, &error);
            return report;
        }
    };

    // 第一个 App 可能比 ticks 跑得更久（回滚窗），保证回滚点一定被执行。
    let run_ticks = options
        .ticks
        .max(options.rollback_at.saturating_add(options.rollback_ticks));
    let mut hashes_a = Vec::with_capacity(run_ticks as usize);
    let mut hashes_b = Vec::with_capacity(options.ticks as usize);
    let mut snapshot = if options.rollback_at == 0 {
        Some(capture_rollback_snapshot(&mut app_a))
    } else {
        None
    };

    // 先各 update 一次，让 Startup / PostStartup 跑完；这次不计入 tick。
    app_a.update();
    app_b.update();

    for tick in 0..run_ticks {
        app_a.update();
        hashes_a.push(simulation_checksum(app_a.world_mut()));
        if tick.saturating_add(1) == options.rollback_at {
            snapshot = Some(capture_rollback_snapshot(&mut app_a));
        }
        if tick < options.ticks {
            app_b.update();
            hashes_b.push(simulation_checksum(app_b.world_mut()));
        }
    }

    report.ticks = options.ticks;
    report.hashes = hashes_a
        .iter()
        .copied()
        .take(options.ticks as usize)
        .collect();

    for (index, (a, b)) in report.hashes.iter().zip(hashes_b.iter()).enumerate() {
        if a != b {
            let tick = index as u32 + 1;
            report.deterministic = false;
            report.first_divergence_tick = Some(tick);
            report.errors.push(VerificationError {
                code: "verify.nondeterministic".to_string(),
                message: format!(
                    "双跑在第 {tick} tick 分叉：{a:016x} != {b:016x}（同一包在同一引擎版本上不应分叉）"
                ),
                tick: Some(tick),
            });
            break;
        }
    }

    if let Some(snapshot) = snapshot {
        // future checksum：第一个 App 在未回滚情况下跑到回滚窗末尾的校验和。
        let future_checksum = if options.rollback_ticks == 0 {
            Some(snapshot.checksum)
        } else {
            let index = options.rollback_at as usize + options.rollback_ticks as usize - 1;
            hashes_a.get(index).copied()
        };
        let future_checksum = match future_checksum {
            Some(checksum) => checksum,
            None => {
                report.rollback_equivalent = false;
                report.errors.push(VerificationError {
                    code: "verify.rollback_unavailable".to_string(),
                    message: "回滚窗未能跑完，无法取得未来校验和".to_string(),
                    tick: Some(options.rollback_at),
                });
                finalize_report(&mut report);
                return report;
            }
        };

        let outcome = restore_rollback_snapshot(&mut app_a, &snapshot);
        if !outcome.missing.is_empty() {
            report.rollback_equivalent = false;
            let preview: Vec<u64> = outcome.missing.iter().copied().take(8).collect();
            report.errors.push(VerificationError {
                code: "verify.rollback_entity_missing".to_string(),
                message: format!(
                    "回滚快照中的 {} 个实体在恢复时已不存在（示例 id：{preview:?}）",
                    outcome.missing.len()
                ),
                tick: Some(options.rollback_at),
            });
        }
        if !outcome.rng_restored {
            report.rollback_equivalent = false;
            report.errors.push(VerificationError {
                code: "verify.rollback_rng_missing".to_string(),
                message: "回滚时 WorldRng 资源缺失，无法恢复随机流".to_string(),
                tick: Some(options.rollback_at),
            });
        }

        for _ in 0..options.rollback_ticks {
            app_a.update();
        }
        let replay_checksum = simulation_checksum(app_a.world_mut());
        let end_tick = options.rollback_at.saturating_add(options.rollback_ticks);
        if replay_checksum != future_checksum {
            report.rollback_equivalent = false;
            report.errors.push(VerificationError {
                code: "verify.rollback_mismatch".to_string(),
                message: format!(
                    "回滚重跑不一致：未来 {future_checksum:016x}，恢复后 {replay_checksum:016x}（快照只含 StableEntityId 实体的 Transform/Velocity 与 WorldRng；mod 自定义资源不在快照内）"
                ),
                tick: Some(end_tick),
            });
        }
    } else {
        report.rollback_equivalent = false;
        report.errors.push(VerificationError {
            code: "verify.rollback_unavailable".to_string(),
            message: format!(
                "回滚点 {} 未落在运行窗口内（ticks={}）",
                options.rollback_at, run_ticks
            ),
            tick: Some(options.rollback_at),
        });
    }

    finalize_report(&mut report);
    report
}

fn push_build_error(report: &mut VerificationReport, error: &WorldBuildError) {
    report.deterministic = false;
    report.rollback_equivalent = false;
    report.errors.push(VerificationError {
        code: error.code().to_string(),
        message: format!("无头验证装配失败：{error}"),
        tick: None,
    });
}

fn finalize_report(report: &mut VerificationReport) {
    report.ok = report.deterministic && report.rollback_equivalent && report.errors.is_empty();
}

fn build_verification_app(mods: CoreMods, seed: u64) -> Result<App, WorldBuildError> {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins);
    app.insert_resource(Time::<Fixed>::from_hz(60.0));
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    CoreGame.build_with_profile(
        &mut app,
        mods,
        &BTreeMap::new(),
        &default_plugin_selection(),
    )?;
    // WorldPlugin 可能已 init 默认 WorldRng；探针 App 必须覆盖成固定种子。
    app.insert_resource(WorldRng::from_seed(seed));
    for i in 0..PROBE_COUNT {
        app.world_mut().spawn((
            StableEntityId(PROBE_ID_BASE + i),
            Transform::from_xyz(i as f32, 0.0, 0.0),
            Velocity {
                linear: Vec3::new(1.0, 0.0, 0.5),
            },
        ));
    }
    app.finish();
    app.cleanup();
    Ok(app)
}

#[derive(Clone, Debug)]
struct EntitySnapshot {
    entity: Entity,
    id: StableEntityId,
    transform: Transform,
    velocity: Option<Velocity>,
}

#[derive(Clone, Debug)]
struct RollbackSnapshot {
    entities: Vec<EntitySnapshot>,
    rng: Option<WorldRng>,
    checksum: u64,
}

#[derive(Debug, Default)]
struct RestoreOutcome {
    missing: Vec<u64>,
    rng_restored: bool,
}

fn capture_rollback_snapshot(app: &mut App) -> RollbackSnapshot {
    let world = app.world_mut();
    let mut query = world.query::<(Entity, &StableEntityId, &Transform, Option<&Velocity>)>();
    let mut entities: Vec<EntitySnapshot> = query
        .iter(world)
        .map(|(entity, id, transform, velocity)| EntitySnapshot {
            entity,
            id: *id,
            transform: *transform,
            velocity: velocity.cloned(),
        })
        .collect();
    entities.sort_by_key(|snapshot| snapshot.id.0);
    let rng = world.get_resource::<WorldRng>().cloned();
    let checksum = simulation_checksum(world);
    RollbackSnapshot {
        entities,
        rng,
        checksum,
    }
}

fn restore_rollback_snapshot(app: &mut App, snapshot: &RollbackSnapshot) -> RestoreOutcome {
    let world = app.world_mut();
    let mut outcome = RestoreOutcome::default();
    let snapshot_entities: BTreeSet<Entity> =
        snapshot.entities.iter().map(|entry| entry.entity).collect();

    for entry in &snapshot.entities {
        let Ok(mut entity) = world.get_entity_mut(entry.entity) else {
            outcome.missing.push(entry.id.0);
            continue;
        };
        if let Some(mut transform) = entity.get_mut::<Transform>() {
            *transform = entry.transform;
        } else {
            outcome.missing.push(entry.id.0);
            continue;
        }
        match &entry.velocity {
            Some(velocity) => {
                if let Some(mut current) = entity.get_mut::<Velocity>() {
                    *current = velocity.clone();
                } else {
                    entity.insert(velocity.clone());
                }
            }
            None => {
                entity.remove::<Velocity>();
            }
        }
    }

    // 清理回滚窗内新生成的、带 StableEntityId 的实体。
    let mut query = world.query_filtered::<Entity, With<StableEntityId>>();
    let spawned_in_window: Vec<Entity> = query
        .iter(world)
        .filter(|entity| !snapshot_entities.contains(entity))
        .collect();
    for entity in spawned_in_window {
        world.despawn(entity);
    }

    if let Some(rng) = &snapshot.rng {
        if let Some(mut current) = world.get_resource_mut::<WorldRng>() {
            *current = rng.clone();
        } else {
            world.insert_resource(rng.clone());
        }
        outcome.rng_restored = true;
    }

    outcome
}

fn fnv_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn hash_u32(hash: u64, value: u32) -> u64 {
    fnv_bytes(hash, &value.to_le_bytes())
}

fn hash_i64(hash: u64, value: i64) -> u64 {
    fnv_bytes(hash, &value.to_le_bytes())
}

fn hash_str(hash: u64, value: &str) -> u64 {
    let hash = hash_u32(hash, value.len() as u32);
    fnv_bytes(hash, value.as_bytes())
}

fn hash_str_vec(mut hash: u64, values: &[String]) -> u64 {
    hash = hash_u32(hash, values.len() as u32);
    for value in values {
        hash = hash_str(hash, value);
    }
    hash
}

fn hash_param_type(mut hash: u64, ty: &PackageParamType) -> u64 {
    match ty {
        PackageParamType::Bool => hash_u32(hash, 0),
        PackageParamType::Int { min, max, step } => {
            hash = hash_u32(hash, 1);
            hash = hash_i64(hash, *min);
            hash = hash_i64(hash, *max);
            hash_i64(hash, *step)
        }
        PackageParamType::Fixed {
            scale,
            min,
            max,
            step,
        } => {
            hash = hash_u32(hash, 2);
            hash = hash_u32(hash, *scale);
            hash = hash_i64(hash, *min);
            hash = hash_i64(hash, *max);
            hash_i64(hash, *step)
        }
        PackageParamType::Enum { values } => {
            hash = hash_u32(hash, 3);
            hash = hash_u32(hash, values.len() as u32);
            for value in values {
                hash = hash_str(hash, value);
            }
            hash
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use game_engine::modding::{Mod, ModContext, ModManifest};
    use game_engine::sim::EngineSet;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    const PATCH_EMPTY: &str = r#"{"version":1,"table":"voxel_objects","patch":[]}"#;
    const PATCH_STONE: &str = r#"{"version":1,"table":"voxel_objects","patch":[{"name":"stone","set":{"replaceable":true}}]}"#;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "ecs_game_verify_{tag}_{}_{nanos}_{seq}",
                std::process::id()
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
        std::fs::write(path, text).expect("写文件");
    }

    fn manifest_json(id: &str, version: u32, data: &[&str]) -> String {
        let data = data
            .iter()
            .map(|path| format!("\"{path}\""))
            .collect::<Vec<_>>()
            .join(",");
        format!(r#"{{"id":"{id}","version":{version},"engine_api":1,"data":[{data}]}}"#)
    }

    fn write_package(
        dir: &TempDir,
        id: &str,
        version: u32,
        data: &[(&str, &str)],
    ) -> LoadedPackage {
        let paths: Vec<&str> = data.iter().map(|(path, _)| *path).collect();
        write_file(
            &dir.path().join("mod.json"),
            &manifest_json(id, version, &paths),
        );
        for (path, text) in data {
            write_file(&dir.path().join(path), text);
        }
        crate::package::load_package(dir.path()).expect("加载测试包")
    }

    fn simple_package(tag: &str, id: &str) -> (TempDir, LoadedPackage) {
        let dir = TempDir::new(tag);
        let package = write_package(&dir, id, 3, &[("data/objects.json", PATCH_EMPTY)]);
        (dir, package)
    }

    // ── 测试专用 mod：随机源失败 ──────────────────────────────────────────

    static RANDOM_MANIFEST: ModManifest = ModManifest {
        id: "test:verify_random",
        version: 1,
        engine_api: CURRENT_ENGINE_API,
        load_after: &[],
        load_after_optional: &[],
        conflicts: &[],
        exclusive_groups: &[],
        params: &[],
        actions: &[],
        presentation: &[],
    };

    struct RandomProbeMod;

    fn random_probe_system(mut query: Query<&mut Transform, With<StableEntityId>>) {
        static TICKS: AtomicU64 = AtomicU64::new(0);
        let tick = TICKS.fetch_add(1, Ordering::Relaxed);
        let offset = tick as f32 * 0.01;
        for mut transform in query.iter_mut() {
            transform.translation.y = offset;
        }
    }

    impl Mod<CoreSpec> for RandomProbeMod {
        fn manifest(&self) -> &'static ModManifest {
            &RANDOM_MANIFEST
        }

        fn install(&self, ctx: &mut ModContext<'_, CoreSpec>) {
            ctx.add_system(EngineSet::Reaction, random_probe_system);
        }
    }

    // ── 测试专用 mod：回滚窗内的非快照资源 ────────────────────────────────

    #[derive(Resource, Default)]
    struct RollbackCounter(u64);

    static ROLLBACK_MANIFEST: ModManifest = ModManifest {
        id: "test:verify_rollback",
        version: 1,
        engine_api: CURRENT_ENGINE_API,
        load_after: &[],
        load_after_optional: &[],
        conflicts: &[],
        exclusive_groups: &[],
        params: &[],
        actions: &[],
        presentation: &[],
    };

    struct RollbackCounterMod;

    fn rollback_counter_system(
        mut counter: ResMut<RollbackCounter>,
        mut query: Query<&mut Transform, With<StableEntityId>>,
    ) {
        counter.0 += 1;
        let bump = counter.0 as f32 * 0.01;
        for mut transform in query.iter_mut() {
            transform.translation.x += bump;
        }
    }

    impl Mod<CoreSpec> for RollbackCounterMod {
        fn manifest(&self) -> &'static ModManifest {
            &ROLLBACK_MANIFEST
        }

        fn install(&self, ctx: &mut ModContext<'_, CoreSpec>) {
            ctx.app().init_resource::<RollbackCounter>();
            ctx.add_system(EngineSet::Reaction, rollback_counter_system);
        }
    }

    // ── 测试专用 mod：缓存重跑计数 ───────────────────────────────────────

    static CACHE_TICKS: AtomicU64 = AtomicU64::new(0);

    static CACHE_MANIFEST: ModManifest = ModManifest {
        id: "test:verify_cache",
        version: 1,
        engine_api: CURRENT_ENGINE_API,
        load_after: &[],
        load_after_optional: &[],
        conflicts: &[],
        exclusive_groups: &[],
        params: &[],
        actions: &[],
        presentation: &[],
    };

    struct CacheCountingMod;

    fn cache_counting_system() {
        CACHE_TICKS.fetch_add(1, Ordering::Relaxed);
    }

    impl Mod<CoreSpec> for CacheCountingMod {
        fn manifest(&self) -> &'static ModManifest {
            &CACHE_MANIFEST
        }

        fn install(&self, ctx: &mut ModContext<'_, CoreSpec>) {
            ctx.add_system(EngineSet::Reaction, cache_counting_system);
        }
    }

    #[test]
    fn good_package_passes_dual_run_and_rollback() {
        let (_dir, package) = simple_package("good", "verify_good");
        let report = verify_package(&package, &[], VerifyOptions::default());
        assert!(report.ok, "报告应通过：{report:#?}");
        assert!(report.deterministic);
        assert!(report.rollback_equivalent);
        assert_eq!(report.ticks, 120);
        assert_eq!(report.hashes.len(), 120);
        assert!(report.first_divergence_tick.is_none());
        assert!(!report.cached);
        assert_eq!(report.engine_api, CURRENT_ENGINE_API);
        assert_eq!(report.engine_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(report.package_hash, package_hash(&package));
    }

    #[test]
    fn random_source_diverges_and_reports_tick() {
        let (_dir, package) = simple_package("random", "verify_random_pkg");
        let mods = package_mods(&package, &[]).push(RandomProbeMod);
        let report = verify_package_with_mods(&package, VerifyOptions::default(), mods);
        assert!(!report.ok);
        assert!(!report.deterministic);
        assert!(
            report.first_divergence_tick.is_some(),
            "随机源必须造成双跑分叉：{report:#?}"
        );
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.code == "verify.nondeterministic"),
            "缺少 verify.nondeterministic：{report:#?}"
        );
    }

    #[test]
    fn rollback_with_unsnapshotted_resource_is_not_equivalent() {
        let (_dir, package) = simple_package("rollback", "verify_rollback_pkg");
        let mods = package_mods(&package, &[]).push(RollbackCounterMod);
        let report = verify_package_with_mods(&package, VerifyOptions::default(), mods);
        assert!(report.deterministic, "双跑应一致：{report:#?}");
        assert!(
            !report.rollback_equivalent,
            "自定义资源不在快照里，回滚必须判不等价：{report:#?}"
        );
        assert!(!report.ok);
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.code == "verify.rollback_mismatch"),
            "缺少 verify.rollback_mismatch：{report:#?}"
        );
        assert!(
            !report
                .errors
                .iter()
                .any(|error| error.code == "verify.rollback_entity_missing"),
            "不应误报实体缺失：{report:#?}"
        );
    }

    #[test]
    fn cache_hit_does_not_rerun() {
        let (_dir, package) = simple_package("cache", "verify_cache_pkg");
        let options = VerifyOptions::default();
        let mut cache = VerificationCache::new();

        let before = CACHE_TICKS.load(Ordering::Relaxed);
        let first = verify_cached_impl(&package, options, &mut cache, || {
            package_mods(&package, &[]).push(CacheCountingMod)
        });
        let after_first = CACHE_TICKS.load(Ordering::Relaxed);
        assert!(!first.cached);
        assert!(first.ok, "{first:#?}");
        assert!(after_first > before, "首次验证应实际运行");

        // 命中时闭包不应被调用：这里故意传空 mods。
        let second = verify_cached_impl(&package, options, &mut cache, CoreMods::new);
        assert!(second.cached);
        assert_eq!(CACHE_TICKS.load(Ordering::Relaxed), after_first);
        assert_eq!(second.package_hash, first.package_hash);
    }

    #[test]
    fn bad_package_becomes_report_error() {
        let dir = TempDir::new("bad");
        let error = crate::package::load_package(dir.path()).expect_err("缺 mod.json 应失败");
        let report = package_error_report(dir.path(), &error);
        assert!(!report.ok);
        assert!(!report.deterministic);
        assert!(!report.rollback_equivalent);
        assert_eq!(report.errors.len(), 1);
        assert!(
            report.errors[0].code.starts_with("mod.package."),
            "{report:#?}"
        );
        assert!(
            report.errors[0]
                .message
                .contains(&dir.path().display().to_string()),
            "消息应带路径：{report:#?}"
        );
        assert!(report.mod_id.is_empty());
        assert!(report.hashes.is_empty());
    }

    #[test]
    fn package_hash_changes_with_data_bytes() {
        let dir = TempDir::new("hash_data");
        let package = write_package(&dir, "verify_hash_data", 3, &[("data/a.json", PATCH_EMPTY)]);
        let before = package_hash(&package);
        write_file(&dir.path().join("data/a.json"), PATCH_STONE);
        let after = package_hash(&crate::package::load_package(dir.path()).expect("重新加载"));
        assert_ne!(before, after);
    }

    #[test]
    fn package_hash_changes_with_version() {
        let dir_a = TempDir::new("hash_version_a");
        let dir_b = TempDir::new("hash_version_b");
        let a = write_package(
            &dir_a,
            "verify_hash_version",
            3,
            &[("data/a.json", PATCH_EMPTY)],
        );
        let b = write_package(
            &dir_b,
            "verify_hash_version",
            4,
            &[("data/a.json", PATCH_EMPTY)],
        );
        assert_ne!(package_hash(&a), package_hash(&b));
    }

    #[test]
    fn package_hash_ignores_undeclared_files() {
        let dir = TempDir::new("hash_extra");
        let package = write_package(
            &dir,
            "verify_hash_extra",
            3,
            &[("data/a.json", PATCH_EMPTY)],
        );
        let before = package_hash(&package);
        write_file(&dir.path().join("unused.txt"), "not declared");
        let after = package_hash(&crate::package::load_package(dir.path()).expect("重新加载"));
        assert_eq!(before, after);
    }

    #[test]
    fn package_hash_hex_is_stable() {
        let hash = 0x0123_4567_89ab_cdef;
        assert_eq!(package_hash_hex(hash), "0123456789abcdef");
        assert_eq!(package_hash_hex(hash).len(), 16);
    }

    #[test]
    fn context_resolves_dependency() {
        let dir_dep = TempDir::new("context_dep");
        let dep = write_package(&dir_dep, "verify_dep", 1, &[]);

        let dir_user = TempDir::new("context_user");
        write_file(
            &dir_user.path().join("mod.json"),
            r#"{"id":"verify_user","version":1,"engine_api":1,"load_after":["verify_dep"]}"#,
        );
        let user = crate::package::load_package(dir_user.path()).expect("加载依赖包");

        let options = VerifyOptions {
            ticks: 12,
            rollback_at: 6,
            rollback_ticks: 4,
            ..VerifyOptions::default()
        };
        let alone = verify_package(&user, &[], options);
        assert!(!alone.ok, "缺失依赖应失败：{alone:#?}");
        assert!(
            alone
                .errors
                .iter()
                .any(|error| error.code == "mod.missing_dependency"),
            "{alone:#?}"
        );

        let with_context = verify_package(&user, &[dep], options);
        assert!(with_context.ok, "带 context 应通过：{with_context:#?}");
    }

    #[test]
    fn cache_key_changes_with_version_and_hash() {
        let dir_a = TempDir::new("cache_key_a");
        let a = write_package(&dir_a, "verify_key", 3, &[("data/a.json", PATCH_EMPTY)]);
        let key_a = VerificationCacheKey::new(&a, package_hash(&a));
        let dir_b = TempDir::new("cache_key_b");
        let b = write_package(&dir_b, "verify_key", 4, &[("data/a.json", PATCH_EMPTY)]);
        let key_b = VerificationCacheKey::new(&b, package_hash(&b));
        assert_ne!(key_a, key_b);
    }
}
