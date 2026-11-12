//! 管理后台 Bevy App 的 Godot 节点。
//!
//! **边界铁律**：本文件只做两件事 ——
//!
//! 1. 把 Godot 的 `Variant` 搬成 `game_core` 的纯数据类型（转发给
//!    [`crate::presentation_bridge`]）；
//! 2. 把 `game_core` 的纯数据搬成 `Variant`。
//!
//! 这里**没有**任何业务判断：动作名查表、能力过滤、可见性、插值全部在 `game-core`。
//!
//! 线程约束（godot-rust 硬规则）：`Gd<T>` 是 `!Send + !Sync`，所有节点方法
//! 只能在 Godot 主线程调用；后台 Bevy 线程只碰 [`ClientBridge`] 里的纯数据通道。

use crossbeam_channel::{unbounded, Receiver, Sender};
use godot::builtin::{
    Array, PackedByteArray, PackedFloat32Array, PackedInt64Array, PackedStringArray, VarDictionary,
    VariantType,
};
#[cfg(debug_assertions)]
use godot::classes::FileAccess;
use godot::prelude::*;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use game_core::bevy_backend::*;
use game_core::mods::default_core_mods;
use game_core::package::{load_package, scan_package_root, LoadedPackage};
use game_core::param_schema::{
    collect_param_schema, param_error_code, validate_param_overrides, ParamKind,
};
use game_core::presentation::semantics::SemanticRegistry;
use game_core::spec::ClientBridge;
use game_core::static_data::ObjectTablesHandle;
#[cfg(debug_assertions)]
use game_core::static_data::ReloadObjectTables;
use game_core::verify_mod::{
    package_error_report, package_hash_hex, verify_cached, VerificationCache, VerificationReport,
    VerifyOptions,
};
use game_core::voxel::terrain::{
    nearest_block_distance_m, TerrainConfig, TerrainMode, VoxelLodRuntime,
};
use game_core::voxel::VoxelLodConfig;
use game_core::world::WorldLoadRejectionHandle;

use crate::presentation_bridge;

/// 管理后台 Bevy App 的 Godot 节点。
#[derive(GodotClass)]
#[class(base=Node)]
pub struct BevyAppManager {
    base: Base<Node>,

    /// 每次启动 Bevy 后台时递增。
    /// 用于过滤旧线程残留消息。
    session: u64,

    /// Godot -> Bevy 控制通道。
    to_bevy: Option<Sender<BevyControlMsg>>,

    /// Bevy -> Godot 生命周期通道。
    from_bevy: Option<Receiver<FromBevy>>,

    /// Bevy 后台线程。
    worker: Option<JoinHandle<()>>,

    /// 输入 / 表现通道（与后台线程共享同一组 `Arc` 句柄）。
    bridge: Option<ClientBridge>,

    /// 运行时语义注册表：`init` 就创建，跨会话保留，mod 可随时注册。
    semantics: SemanticRegistry,

    /// Godot 侧采集序号（诊断 + 丢帧统计）。
    input_seq: u64,

    /// 最近一帧的插值时刻 T_t / 步长（`render_alpha` 用）。
    last_tick_clock_ms: u64,
    last_timestep_ms: f32,

    /// 体素 LOD 运行期句柄：Godot 主线程写，后台 Bevy 线程经 VoxelLodHandle 读。
    voxel_lod: Arc<Mutex<VoxelLodRuntime>>,

    /// 内存软上限运行期句柄（百分比）：Godot 主线程写，驻留系统读。
    ///
    /// 外层 None = 不覆盖（沿用配置）；Some(None) = 关闭上限；
    /// Some(Some(p)) = 覆盖为 p%。测试用它制造 / 解除内存压力。
    memory_soft_limit: MemoryLimitHandle,

    /// 静态数据表句柄：Godot 写重载请求 / 读当前表；后台 Bevy 在 FixedUpdate 应用。
    object_tables: ObjectTablesHandle,

    /// 读档拒绝句柄：后台 Bevy 线程写，本节点（Godot 主线程）读。
    ///
    /// 与 voxel_lod / object_tables 同一模式；后台线程停掉之后值仍在，
    /// 因此 `bev_stopped` 之后仍能读出来展示差异。
    world_load_rejection: WorldLoadRejectionHandle,

    /// 已扫描通过的目录式 mod 包（见 [game_core::package]）。
    ///
    /// 扫描发生在 Godot 主线程，装配发生在每次 `start_backend`：包一旦扫描成功
    /// 就随之后每一次启动带进后台配置，直到下一次扫描覆盖。
    packages: Vec<LoadedPackage>,

    /// 导入验证缓存：按 (mod_id, version, package_hash, engine_api, engine_version) 命中。
    ///
    /// 只服务默认 VerifyOptions；UI 若允许改 ticks / seed，应另行清空缓存。
    verification_cache: VerificationCache,

    /// 待装配的参数覆盖值（全 key -> 落盘整数），由参数面板在启动前写入。
    ///
    /// 每次 start_backend 原样搬进 BevyBackendConfig 的 param_overrides；
    /// 真正的语义校验在 CoreGame::build_with_profile 里由 ParamTable 完成，
    /// 本节点只保证「面板提交时 schema 合法」。默认空表。
    param_overrides: BTreeMap<String, i64>,

    /// 后台线程回传的最后一次错误 (code, message)。
    ///
    /// 句柄里没有拒绝信息时，`world_load_rejection()` 用它兜底：`save.*` 的
    /// Error 一定先于 Stopped 到达（同一通道、同顺序），所以 stop 之后可读。
    last_error: Option<(String, String)>,
}

#[godot_api]
impl INode for BevyAppManager {
    fn init(base: Base<Node>) -> Self {
        Self {
            base,
            session: 0,
            to_bevy: None,
            from_bevy: None,
            worker: None,
            bridge: None,
            semantics: SemanticRegistry::new(),
            input_seq: 0,
            last_tick_clock_ms: 0,
            last_timestep_ms: 0.0,
            voxel_lod: Arc::new(Mutex::new(VoxelLodRuntime::default())),
            memory_soft_limit: Arc::new(Mutex::new(None)),
            object_tables: ObjectTablesHandle::default(),
            world_load_rejection: WorldLoadRejectionHandle::default(),
            packages: Vec::new(),
            verification_cache: VerificationCache::new(),
            param_overrides: BTreeMap::new(),
            last_error: None,
        }
    }

    fn ready(&mut self) {
        // 如果需要场景加载后自动启动，可在此处调用 self.start_bevy();
    }

    fn process(&mut self, _delta: f64) {
        let Some(rx) = self.from_bevy.clone() else {
            return;
        };
        while let Ok(msg) = rx.try_recv() {
            if msg.session != self.session {
                continue;
            }

            match msg.payload {
                BevyLifecycleMsg::Started => self.emit_started(),
                BevyLifecycleMsg::Paused => self.emit_paused(),
                BevyLifecycleMsg::Resumed => self.emit_resumed(),
                BevyLifecycleMsg::Error { code, message } => {
                    // 先留档再发信号：GDScript 可能在信号回调里立刻 stop_bevy()，
                    // 那时这条错误已经必须能被 world_load_rejection() 读到。
                    self.last_error = Some((code.clone(), message.clone()));
                    self.emit_error(code, message);
                }
                BevyLifecycleMsg::Custom(message) => self.emit_custom(message),
                BevyLifecycleMsg::Stopped(exit_code) => {
                    self.cleanup_after_stop();
                    self.emit_stopped(exit_code);
                }
            }
        }
    }

    fn exit_tree(&mut self) {
        self.shutdown_blocking();
    }
}

#[godot_api]
impl BevyAppManager {
    #[signal]
    fn bev_started();

    #[signal]
    fn bev_paused();

    #[signal]
    fn bev_resumed();

    #[signal]
    fn bev_stopped(exit_code: i64);

    /// 装配失败 / 后台 panic：`code` 是稳定错误码，`message` 是中文说明。
    #[signal]
    fn bev_error(code: GString, message: GString);

    #[signal]
    fn bev_custom(message: GString);

    // ───────────────────────── 生命周期 ─────────────────────────

    #[func]
    fn start_bevy(&mut self) {
        self.start_bevy_with_config(60.0, 60.0, true);
    }

    #[func]
    fn start_bevy_with_rate(&mut self, fixed_hz: f64, runner_hz: f64) {
        self.start_bevy_with_config(fixed_hz, runner_hz, true);
    }

    /// `enable_demo = false` 时不挂 `game-core::dev::demo`（生产环境应为 `false`）。
    #[func]
    fn start_bevy_with_config(&mut self, fixed_hz: f64, runner_hz: f64, enable_demo: bool) {
        self.start_backend(BevyBackendConfig {
            fixed_hz,
            runner_hz,
            enable_demo,
            ..BevyBackendConfig::default()
        });
    }

    /// 启动性能测试场景：生成 `entity_count` 个每 tick 移动的实体（取代 demo）。
    ///
    /// 用于量化「模拟 → 收集 → 编码 → 发布」各段耗时；Godot 侧可配合
    /// `parse_packed_frame_header()` 读 `command_count` 与 `render_clock_ms`。
    #[func]
    fn start_bevy_perf_test(&mut self, fixed_hz: f64, runner_hz: f64, entity_count: i64) {
        let count = usize::try_from(entity_count).unwrap_or(1).max(1);
        self.start_backend(BevyBackendConfig {
            fixed_hz,
            runner_hz,
            enable_demo: false,
            perf_entity_count: Some(count),
            ..BevyBackendConfig::default()
        });
    }

    /// 启动体素地形 demo：挂 `TerrainConfig::default()`（radius 2 / max_lod 1）。
    ///
    /// 地形在 Startup 生成一次，产出带 `VoxChunkRaw` 的 mesh 块实体；
    /// 贪婪 meshing 在 Godot 侧调 gdext `mesh_voxel_halo` 现算，再由 VoxelMeshNode 渲染。
    /// 同时挂体素观察者（`dev::voxel_viewer`）：一个落在地表上的玩家 + 相机，
    /// 让 break_block / place_block 有射线来源；输入源 ID = 4，Godot 侧
    /// `voxel_demo.tscn` 的 `BevyClient.source_id` 必须与它一致。
    ///
    /// 默认 radius 2 / max_lod 1（144 子块）：release ≈ 2–4 s，**debug ≈ 30 s**
    /// （生成是 debug 下的瓶颈）。只想快速看一眼可以用
    /// `start_bevy_voxel_perf(fixed_hz, runner_hz, 0, 0)`（36 子块，≈ 5 s）。
    #[func]
    fn start_bevy_voxel_demo(&mut self, fixed_hz: f64, runner_hz: f64) {
        self.start_backend(BevyBackendConfig {
            fixed_hz,
            runner_hz,
            enable_demo: false,
            terrain: Some(TerrainConfig::default()),
            voxel_viewer: true,
            ..BevyBackendConfig::default()
        });
    }

    /// 流式体素场景：地形只建空容器，玩家作为观察者按需加载区块，并启用世界存档。
    ///
    /// 与 start_bevy_voxel_demo 的区别：
    /// - 地形走 `TerrainMode::Streaming`，Startup 不生成任何块，块由驻留 / 流式管线
    ///   按玩家位置请求，所以进场景是秒开而不是等全量生成；
    /// - `save_dir` 非空时接上存档层（启动读档、SaveNow / 退场落盘）；
    /// - 仍然挂 voxel_viewer，给一个带相机 / 玩家的可玩场景 —— 流式管线要靠本地
    ///   玩家当观察者锚点，没有它永远不会有区块请求。
    ///
    /// 约定：`save_dir` 传空字符串表示「不落盘」。GDScript 侧没有 `Option`，
    /// 传 `""` 比让调用方再造一套重载简单。
    #[func]
    fn start_bevy_streaming(&mut self, fixed_hz: f64, runner_hz: f64, save_dir: GString) {
        // 空字符串 = 纯内存跑流式（不读档、不写盘）：空路径不是合法存档根，
        // 与其让 SavePaths 去拼相对当前工作目录的怪路径，不如明确当成 None。
        let save_dir = save_dir.to_string();
        self.start_backend(BevyBackendConfig {
            fixed_hz,
            runner_hz,
            enable_demo: false,
            terrain: Some(TerrainConfig::default()),
            voxel_viewer: true,
            terrain_mode: TerrainMode::Streaming,
            save_dir: godot_save_dir(&save_dir),
            ..BevyBackendConfig::default()
        });
    }

    /// 只读打开一个已有存档：读档、跑表现，但不推进玩法、不写盘。
    ///
    /// 与 start_bevy_streaming 的差别全在装配参数：
    /// - `read_only: true` → 后台插入 `ReadOnlySession`，输入链与 tick 冻结，
    ///   插件 profile 走 `readonly_plugin_selection()`（不装玩法 / 移动 / 相机），
    ///   存档路径用 `SavePaths::read_only`，任何写盘请求都直接拒绝；
    /// - `save_dir` 必须非空：只读打开没有存档就无从看起，空路径只警告不启动。
    ///
    /// 读档身份校验（mod / 动作表 / 插件序列）在只读下放宽，坏档仍然拒绝：
    /// 只读不等于可以打开一个结构损坏的世界。
    #[func]
    fn start_bevy_streaming_readonly(&mut self, fixed_hz: f64, runner_hz: f64, save_dir: GString) {
        let save_dir = save_dir.to_string();
        if save_dir.is_empty() {
            godot_warn!("[BevyAppManager] 只读打开需要 save_dir；忽略本次启动");
            return;
        }
        self.start_backend(BevyBackendConfig {
            fixed_hz,
            runner_hz,
            enable_demo: false,
            terrain: Some(TerrainConfig::default()),
            voxel_viewer: true,
            terrain_mode: TerrainMode::Streaming,
            save_dir: godot_save_dir(&save_dir),
            read_only: true,
            ..BevyBackendConfig::default()
        });
    }

    /// 流式体素场景 + 显式插件选择（其余参数与 start_bevy_streaming 完全一致）。
    ///
    /// `plugins` 原样交给后台装配：空数组表示一个可选插件都不装（不是默认
    /// profile）；未知 id 不会 panic，会以 bev_error(code="profile.unknown_plugin",
    /// message) 结构化回传并以失败退出。
    #[func]
    fn start_bevy_streaming_with_plugins(
        &mut self,
        fixed_hz: f64,
        runner_hz: f64,
        save_dir: GString,
        plugins: PackedStringArray,
    ) {
        let save_dir = save_dir.to_string();
        self.start_backend(BevyBackendConfig {
            fixed_hz,
            runner_hz,
            enable_demo: false,
            terrain: Some(TerrainConfig::default()),
            voxel_viewer: true,
            terrain_mode: TerrainMode::Streaming,
            save_dir: godot_save_dir(&save_dir),
            plugins: Some(presentation_bridge::packed_strings_to_vec(&plugins)),
            ..BevyBackendConfig::default()
        });
    }

    /// 扫描 mod 包根目录：解析 <root>/<目录>/mod.json 与 data 补丁，结果留在节点上。
    ///
    /// 全有或全无：任一包失败则一个都不装（错误列在返回字典里），已扫描成功的结果
    /// 不会被半成品覆盖。返回 { ok, count, packages, errors }：
    /// - ok=true 时 packages 是 [{ id, version, source, actions, params, presentation, data_files }]；
    /// - ok=false 时 errors 是 [{ path, code, message }]，code 形如 mod.package.schema。
    ///
    /// 扫描只读文件，不装配；之后任一 start_bevy_* 都会把包带进后台配置。
    #[func]
    fn scan_mod_packages(&mut self, root: GString) -> VarDictionary {
        let root = root.to_string();
        let mut out = VarDictionary::new();
        let mut errors: Array<VarDictionary> = Array::new();
        if root.is_empty() {
            self.packages.clear();
            out.set("ok", false);
            out.set("count", 0);
            out.set("packages", &Array::<VarDictionary>::new());
            let mut error = VarDictionary::new();
            error.set("path", "");
            error.set("code", "mod.package.io");
            error.set("message", "mod 包根目录为空");
            errors.push(&error);
            out.set("errors", &errors);
            return out;
        }
        match scan_package_root(std::path::Path::new(&root)) {
            Ok(packages) => {
                let mut list: Array<VarDictionary> = Array::new();
                for package in &packages {
                    let mut item = VarDictionary::new();
                    item.set("id", package.id());
                    item.set("version", i64::from(package.version()));
                    item.set("source", package.source.display().to_string());
                    let actions: Vec<&str> = package
                        .manifest
                        .actions
                        .iter()
                        .map(|action| action.as_str())
                        .collect();
                    item.set("actions", &strings_to_packed(&actions));
                    let params: Vec<&str> = package
                        .manifest
                        .params
                        .iter()
                        .map(|param| param.key.as_str())
                        .collect();
                    item.set("params", &strings_to_packed(&params));
                    let presentation: Vec<&str> = package
                        .manifest
                        .presentation
                        .iter()
                        .map(|payload| payload.name.as_str())
                        .collect();
                    item.set("presentation", &strings_to_packed(&presentation));
                    let data: Vec<&str> = package
                        .manifest
                        .data
                        .iter()
                        .map(|path| path.as_str())
                        .collect();
                    item.set("data_files", &strings_to_packed(&data));
                    let scripts: Vec<&str> = package
                        .manifest
                        .scripts
                        .iter()
                        .map(|path| path.as_str())
                        .collect();
                    item.set("scripts", &strings_to_packed(&scripts));
                    list.push(&item);
                }
                out.set("ok", true);
                out.set("count", i64::try_from(packages.len()).unwrap_or(i64::MAX));
                out.set("packages", &list);
                out.set("errors", &errors);
                self.packages = packages;
            }
            Err(failures) => {
                for failure in failures {
                    let mut error = VarDictionary::new();
                    error.set("path", failure.path().display().to_string());
                    error.set("code", failure.code());
                    error.set("message", failure.message());
                    errors.push(&error);
                }
                out.set("ok", false);
                out.set("count", 0);
                out.set("packages", &Array::<VarDictionary>::new());
                out.set("errors", &errors);
            }
        }
        out
    }

    /// 验证单个目录式 mod 包（同步阻塞，见 [Self::verify_mods]）。
    ///
    /// **会阻塞调用线程约数秒**（120 tick 双跑 + 30 tick 回滚，默认 profile）。
    /// GDScript 应在后台线程 / 后续异步任务里调用，本轮不做异步。
    #[func]
    fn verify_mod(&mut self, path: GString) -> VarDictionary {
        verify_one_package(
            &path.to_string(),
            &self.packages,
            &mut self.verification_cache,
        )
    }

    /// 批量验证目录式 mod 包（同步阻塞）。
    ///
    /// 每个路径独立加载、独立验证；坏包也会返回报告形状的字典，不 panic。
    /// context 取本节点已扫描通过的包（排除与被测包同 id 的项），因此有依赖的包
    /// 在 scan_mod_packages 之后能解析。返回 Array[Dictionary]，字段见
    /// [verification_report_to_dict]。
    ///
    /// **线程边界**：内部会装配两个无头 Bevy App 并跑 tick（默认 120 + 30），
    /// 在本机大约需要数秒；这是在 Godot 主线程上执行的同步调用，会卡住 UI。
    /// 本轮不提供异步版本，调用方应放到后台线程 / 后续异步任务。
    #[func]
    fn verify_mods(&mut self, paths: PackedStringArray) -> Array<VarDictionary> {
        let mut out: Array<VarDictionary> = Array::new();
        for path in paths.to_vec() {
            let report = verify_one_package(
                &path.to_string(),
                &self.packages,
                &mut self.verification_cache,
            );
            out.push(&report);
        }
        out
    }

    /// 采集装配前参数 schema（编译期 mod + 已扫描的目录式包）。
    ///
    /// 返回 { ok, schema, errors }。
    ///
    /// 成功时 schema 是 Array[Dictionary]，每项字段：
    /// - key：全 key "<mod_id>.<local_key>"；
    /// - mod_id / local_key：拆开的 mod 与局部 key；
    /// - scope："world" / "debug"；
    /// - kind："bool" / "int" / "fixed" / "enum"；
    /// - default：落盘整数（Bool 0/1，Enum 是下标，Fixed 是乘过 scale 的整数）；
    /// - min / max / step / scale：不适用的字段给 -1；min/max/step 只对 int/fixed
    ///   有值，scale 只对 fixed 有值（且 > 0）；
    /// - values：PackedStringArray，enum 顺序即下标；非 enum 为空数组；
    /// - group：分组名，空字符串表示未分组；
    /// - visible_if：空字典，或 { key, equals }。这里的 key 是**同一 mod 内的
    ///   local key**，不是全 key；UI 先用 mod_id 拼全 key 再比较，避免跨 mod 误判。
    ///
    /// 失败时 ok=false、schema 为空数组，errors 是 [{ code, message }]，code 形如
    /// param.duplicate_key / param.invalid_spec / param.invalid_default。
    /// 本方法只读，不触碰节点上待用的覆盖表。
    #[func]
    fn get_param_schema(&self) -> VarDictionary {
        let mut out = VarDictionary::new();
        let mut schema: Array<VarDictionary> = Array::new();
        let mut errors: Array<VarDictionary> = Array::new();

        match collect_param_schema(&default_core_mods(), &self.packages) {
            Ok(entries) => {
                for entry in entries {
                    let mut item = VarDictionary::new();
                    item.set("key", entry.key.as_str());
                    item.set("mod_id", entry.mod_id.as_str());
                    item.set("local_key", entry.local_key.as_str());
                    item.set("scope", entry.scope_code());
                    item.set("kind", entry.kind.code());
                    item.set("default", entry.default);
                    item.set("min", entry.min.unwrap_or(-1));
                    item.set("max", entry.max.unwrap_or(-1));
                    item.set("step", entry.step.unwrap_or(-1));
                    item.set("scale", entry.scale.map_or(-1, i64::from));
                    let values: Vec<&str> = match &entry.kind {
                        ParamKind::Enum { values } => values.iter().map(String::as_str).collect(),
                        _ => Vec::new(),
                    };
                    item.set("values", &strings_to_packed(&values));
                    item.set("group", entry.group.as_str());
                    let mut visible_if = VarDictionary::new();
                    if let Some((key, equals)) = &entry.visible_if {
                        visible_if.set("key", key.as_str());
                        visible_if.set("equals", *equals);
                    }
                    item.set("visible_if", &visible_if);
                    schema.push(&item);
                }
                out.set("ok", true);
            }
            Err(error) => {
                push_param_error(&mut errors, param_error_code(&error), &error.to_string());
                out.set("ok", false);
            }
        }

        out.set("schema", &schema);
        out.set("errors", &errors);
        out
    }

    /// 提交参数覆盖值（Dictionary：String 全 key -> 整数），按当前 schema 校验。
    ///
    /// 接受 World 与 Debug 两种作用域（参数面板会同时显示）。返回
    /// { ok, count, errors }：成功时替换节点上待用的覆盖表，count 是条目数；
    /// 失败时 ok=false、count=0，errors 每项 { code, message }，且**旧覆盖表保持不变**。
    ///
    /// code 形如 param.unknown_key / param.out_of_range / param.invalid_bool /
    /// param.invalid_enum / param.invalid_override（key 不是 String，或 value 不是
    /// 整数）。schema 本身采集失败时回传对应的 param.invalid_spec /
    /// param.duplicate_key 等。
    ///
    /// 这里只做装配前校验；真正写进 ECS 是下一次 start_bevy_* 时
    /// CoreGame::build_with_profile 的事。
    #[func]
    fn set_param_overrides(&mut self, params: VarDictionary) -> VarDictionary {
        let mut errors: Array<VarDictionary> = Array::new();
        let mut parsed: BTreeMap<String, i64> = BTreeMap::new();

        for (key, value) in &params {
            if key.get_type() != VariantType::STRING {
                push_param_error(
                    &mut errors,
                    "param.invalid_override",
                    "参数覆盖的 key 必须是 String",
                );
                continue;
            }
            let key = match key.try_to::<GString>() {
                Ok(key) => key.to_string(),
                Err(_) => {
                    push_param_error(
                        &mut errors,
                        "param.invalid_override",
                        "参数覆盖的 key 无法转换为 String",
                    );
                    continue;
                }
            };
            if value.get_type() != VariantType::INT {
                push_param_error(
                    &mut errors,
                    "param.invalid_override",
                    &format!("参数覆盖 {key} 的值必须是整数"),
                );
                continue;
            }
            match value.try_to::<i64>() {
                Ok(value) => {
                    parsed.insert(key, value);
                }
                Err(_) => push_param_error(
                    &mut errors,
                    "param.invalid_override",
                    &format!("参数覆盖 {key} 的值超出 i64 范围"),
                ),
            }
        }

        if !errors.is_empty() {
            return param_override_result(false, 0, errors);
        }

        let entries = match collect_param_schema(&default_core_mods(), &self.packages) {
            Ok(entries) => entries,
            Err(error) => {
                push_param_error(&mut errors, param_error_code(&error), &error.to_string());
                return param_override_result(false, 0, errors);
            }
        };
        if let Err(error) = validate_param_overrides(&entries, &parsed) {
            push_param_error(&mut errors, param_error_code(&error), &error.to_string());
            return param_override_result(false, 0, errors);
        }

        let count = parsed.len();
        self.param_overrides = parsed;
        param_override_result(true, count, errors)
    }

    /// 启动体素性能场景：给定半径 / max_lod 的地形 + 每 tick 移动的观察者。
    ///
    /// `radius_blocks` 是 x/z 上围绕世界原点的基础子块半径（1 块 = 32 体素 ≈ 14.4 m），
    /// `max_lod` 取 0..=3（越界会被夹住）。debug 构建下大半径生成很慢，建议 release 跑；
    /// LOD 合并要半径 ≥ 5 才会越过第一档 32 m 阈值。
    #[func]
    fn start_bevy_voxel_perf(
        &mut self,
        fixed_hz: f64,
        runner_hz: f64,
        radius_blocks: i64,
        max_lod: i64,
    ) {
        let radius_blocks = i32::try_from(radius_blocks).unwrap_or(2).clamp(0, 64);
        let max_lod = u8::try_from(max_lod).unwrap_or(1).min(3);
        let terrain = TerrainConfig {
            radius_blocks,
            max_lod,
            ..TerrainConfig::default()
        };
        self.start_backend(BevyBackendConfig {
            fixed_hz,
            runner_hz,
            enable_demo: false,
            terrain: Some(terrain),
            terrain_perf: true,
            ..BevyBackendConfig::default()
        });
    }

    /// 启动游戏内容测试场景：静态体素平台（可见 + 碰撞）与带碰撞的玩家。
    ///
    /// Godot 侧对应 `test/gameplay_test.tscn`（`BevyClient.mode = gameplay_test`）。
    #[func]
    fn start_bevy_gameplay_test(&mut self, fixed_hz: f64, runner_hz: f64) {
        self.start_backend(BevyBackendConfig {
            fixed_hz,
            runner_hz,
            enable_demo: false,
            gameplay_test: true,
            ..BevyBackendConfig::default()
        });
    }

    #[func]
    fn request_stop(&mut self) {
        self.request_shutdown_internal();
    }

    #[func]
    fn stop_bevy(&mut self) {
        self.shutdown_blocking();
    }

    #[func]
    fn pause_bevy(&self) {
        self.send_control(BevyControlMsg::Pause);
    }

    #[func]
    fn resume_bevy(&self) {
        self.send_control(BevyControlMsg::Resume);
    }

    #[func]
    fn send_custom_command(&self, command: GString) {
        self.send_control(BevyControlMsg::Custom(command.to_string()));
    }

    #[func]
    fn is_bevy_running(&self) -> bool {
        self.worker.is_some()
    }

    // ───────────────────── 输入通道（Godot → game-core） ─────────────────────

    /// 拉取动作表：`Array[{ name, bit, channel? }]`（按键由 Godot 侧配置）。
    ///
    /// 名字与通道位的唯一真相源是本次装配解析出的全局动作表
    /// （`game_core::input::active_actions()`：核心动作 + mod 动作）；
    /// 世界加载完成后要重新拉一次，mod 动作才会进 Godot 的 `InputMap`。
    #[func]
    fn get_action_table(&self) -> Array<VarDictionary> {
        presentation_bridge::action_table_to_variant()
    }

    /// mod 表现子种类表：`Array[{ sub_kind, mod_id, name, fields: [{ key, name, vis, required_bits }] }]`。
    ///
    /// Godot 侧按 `sub_kind` 查表即可把 CUSTOM 载荷的字段 key 翻译成人可读的名字，
    /// 并按 `vis` 决定是否交给渲染器。表来自装配期发布的全局快照，世界加载完成后
    /// 要重新拉一次（与 `get_action_table` 同一生命周期）。
    #[func]
    fn get_mod_payload_table(&self) -> Array<VarDictionary> {
        presentation_bridge::mod_payload_table_to_variant()
    }

    /// 输入通道表：`Array[{ key, mask, mode, actions }]`。
    ///
    /// 供 Godot 的 BevyInputContext 把 INPUTCAPS 的位掩码翻译成通道级
    /// present / available；通道定义与掩码的唯一真相源在 game-core。
    #[func]
    fn get_input_channels(&self) -> Array<VarDictionary> {
        presentation_bridge::input_channels_to_variant()
    }

    /// 每帧一次批量提交（避免 N 次跨 FFI）。
    ///
    /// - `held` / `pressed` / `released`：语义动作**名**列表（不是物理键）；
    /// - `move_axis` / `look_delta`：原始 `f32`，量化在 `game-core`；
    /// - `render_clock_ms`：Godot 渲染时钟，供 `game-core` 算插值 alpha。
    ///
    /// 注意：`u64` 不能跨 gdext FFI，所以这里一律用 `i64`。
    #[func]
    // 参数个数是 GDScript 侧契约（`BevyClient.submit_input_frame` 一次批量提交
    // 全部输入）；收成结构体会改变 Godot 绑定签名，故保留为单函数入口。
    #[allow(
        clippy::too_many_arguments,
        reason = "Godot #[func] 绑定签名，参数个数是对 GDScript 的公开契约"
    )]
    fn submit_input_frame(
        &mut self,
        held: PackedStringArray,
        pressed: PackedStringArray,
        released: PackedStringArray,
        move_axis: Vector2,
        look_delta: Vector2,
        render_clock_ms: i64,
        source_id: i64,
    ) {
        let Some(bridge) = self.bridge.clone() else {
            return;
        };

        self.input_seq = self.input_seq.wrapping_add(1);

        let (frame, unknown) = presentation_bridge::build_raw_input_frame(
            &held,
            &pressed,
            &released,
            move_axis,
            look_delta,
            u64::try_from(render_clock_ms).unwrap_or(0),
            self.input_seq,
        );

        if !unknown.is_empty() {
            // 前向兼容：老客户端遇到新动作名只警告，不崩。
            godot_warn!("[BevyAppManager] 未知动作名（已忽略）：{unknown:?}");
        }

        bridge.submit_input_frame(
            presentation_bridge::source_id_from_i64(source_id),
            frame,
            unknown.len(),
        );
    }

    // ───────────────────── 表现通道（game-core → Godot） ─────────────────────

    /// 一次取走最新表现帧。
    ///
    /// 返回空 `Dictionary` 表示**没有新帧**（正常情况，不是错误）。
    #[func]
    fn take_presentation_frame(&mut self) -> VarDictionary {
        let Some(bridge) = self.bridge.clone() else {
            return VarDictionary::new();
        };

        match bridge.take_presentation_frame() {
            Some(frame) => {
                self.last_tick_clock_ms = frame.render_clock_ms;
                self.last_timestep_ms = frame.timestep_ms;
                presentation_bridge::frame_to_dictionary(&frame)
            }
            None => VarDictionary::new(),
        }
    }

    // ─────────────── GPF1 二进制打包（SoA 快通道） ───────────────

    /// 一次取走最新表现帧的 GPF1 打包字节。
    ///
    /// 与 `take_presentation_frame()` **互斥**：一帧只能走一条通道。
    /// 返回空 `PackedByteArray` 表示没有新帧（正常情况）。
    #[func]
    fn take_packed_presentation_frame(&mut self) -> PackedByteArray {
        let Some(bridge) = self.bridge.clone() else {
            return PackedByteArray::new();
        };
        let Some(bytes) = bridge.take_packed_presentation_frame() else {
            return PackedByteArray::new();
        };

        // 顺带更新插值时钟：只读 44 字节定长头，O(1) 无分配。
        if let Ok(header) = game_core::presentation::packed::decode_header(&bytes) {
            self.last_tick_clock_ms = header.render_clock_ms;
            self.last_timestep_ms = header.timestep_ms;
        }

        PackedByteArray::from(&bytes[..])
    }

    /// 解析 GPF1 打包帧为 SoA 平行数组（快通道，不逐实体构造 Dictionary）。
    #[func]
    fn parse_packed_presentation_streams(&self, data: PackedByteArray) -> VarDictionary {
        presentation_bridge::packed_frame_to_streams(&data)
    }

    /// 解析 GPF1 打包帧为完整 Dictionary（兼容旧通道）。
    #[func]
    fn parse_packed_presentation_frame(&self, data: PackedByteArray) -> VarDictionary {
        presentation_bridge::packed_frame_to_dictionary(&data)
    }

    /// 只解析 GPF1 打包帧头部（O(1)）。
    #[func]
    fn parse_packed_frame_header(&self, data: PackedByteArray) -> VarDictionary {
        presentation_bridge::packed_header_to_dictionary(&data)
    }

    /// 当前渲染时刻的插值 alpha。
    ///
    /// Godot 每帧调用一次，配合自己缓存的 (prev, curr) 采样 —— 这样即使这一帧
    /// 没有新表现帧（例如逻辑没变），插值仍然平滑。采样用与 T_t 同源的进程级
    /// 单调时钟，纯函数在 game-core。
    #[func]
    fn render_alpha(&self) -> f64 {
        game_core::presentation::sample_alpha(
            game_core::presentation::monotonic_now_ms(),
            self.last_tick_clock_ms,
            self.last_timestep_ms,
        ) as f64
    }

    /// 取走全部待处理表现事件（保序，不可丢）。
    #[func]
    fn drain_presentation_events(&mut self) -> Array<VarDictionary> {
        let Some(bridge) = self.bridge.clone() else {
            return Array::new();
        };

        let events = bridge.drain_presentation_events();
        presentation_bridge::events_to_array(&events)
    }

    /// 体素渲染数据表（路径 B GPU 渲染启动时拉一次）：
    /// `{ voxel_size, block_names(256 项，下标 == block id，未知 id 为空串) }`。
    /// 外观（颜色 / PBR / 纹理）与 block→面材质绑定都由 Godot 侧资源接管，
    /// Rust 只给 block id→名字；具体布局见
    /// `presentation_bridge::voxel_table_to_variant`。
    #[func]
    fn get_voxel_table(&self) -> VarDictionary {
        let tables = self.object_tables.current();
        presentation_bridge::voxel_table_to_variant(tables.as_ref())
    }

    // ───────────────────── 静态表热重载（dev） ─────────────────────

    /// 从 `user://tables/` 读取四张覆盖表并排队重载。
    ///
    /// 文件不存在时该表沿用嵌入版本；读取与解析分离：**文件读取在 Godot 侧，
    /// Rust 只解析字符串**。请求在下一个 FixedUpdate（tick 边界）生效，
    /// 返回 `{ generation, last_error, reload_enabled }`（generation 是**应用前**的值，
    /// tick 之后才会变）。
    #[cfg(debug_assertions)]
    #[func]
    fn reload_object_tables(&self) -> VarDictionary {
        self.object_tables.queue_reload(ReloadObjectTables {
            voxel_src: read_user_table("voxel_objects.jsonc"),
            biome_src: read_user_table("biomes.jsonc"),
            entity_src: read_user_table("entity_objects.jsonc"),
            item_src: read_user_table("item_placement.jsonc"),
        });
        self.object_tables_status()
    }

    /// 直接以字符串提交四张覆盖表（空串 = 沿用嵌入版本）。调试 / 单测入口，
    /// 与 [reload_object_tables] 走同一条 tick 边界重载路径。
    #[cfg(debug_assertions)]
    #[func]
    fn reload_object_tables_from_strings(
        &self,
        voxel_src: GString,
        biome_src: GString,
        entity_src: GString,
        item_src: GString,
    ) -> VarDictionary {
        let source = |text: GString| {
            let text = text.to_string();
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        };
        self.object_tables.queue_reload(ReloadObjectTables {
            voxel_src: source(voxel_src),
            biome_src: source(biome_src),
            entity_src: source(entity_src),
            item_src: source(item_src),
        });
        self.object_tables_status()
    }

    /// 当前静态表状态：`{ generation, last_error, reload_enabled }`。
    /// last_error 为空串表示正常。
    #[func]
    fn object_tables_status(&self) -> VarDictionary {
        let mut out = VarDictionary::new();
        out.set("generation", i64::from(self.object_tables.generation()));
        let last_error = self.object_tables.last_error().unwrap_or_default();
        out.set("last_error", last_error.as_str());
        out.set("reload_enabled", self.object_tables.reload_enabled());
        out
    }

    /// 运行期开关：联网会话等场景可临时禁用重载。默认 dev 构建开启、release 关闭；
    /// release 构建中此方法不编译，开关保持关闭。
    #[cfg(debug_assertions)]
    #[func]
    fn set_object_tables_reload_enabled(&self, enabled: bool) {
        self.object_tables.set_reload_enabled(enabled);
    }

    /// 原始体素 halo → 39 bit 矩形流（调用 game-core mesher 现算）。
    ///
    /// lod 越界会被夹到 0..=3；blocks 是 RAWVOXELS 载荷解出的原始体素字节。
    /// skirt_mask 是 6 bit 跨 LOD 接缝裙边掩码（ExternalPlane 顺序；0 = 无裙边）；
    /// Godot 侧 VoxelMeshNode.lod_skirts_enabled=false 时固定传 0，行为退回
    /// 普通 mesh_raw_halo。
    ///
    /// face_material_map 由 Godot 的 VoxelMaterialLibrary.build_face_material_map()
    /// 生成：索引 = block*6 + face（face = plane*2+dir，顺序 [+x, -x, +y, -y, +z, -z]），
    /// 值 = 该面要写的材质 id；Rust 不持有任何材质表，只机械重映射。
    /// **空 map（或长度 != 256*6）= identity 降级**：矩形 material 保持 block id。
    ///
    /// mesher 输出 Vec<u64>，而 u64 没有 ToGodot，这里逐元素按位转 i64
    /// 交给 PackedInt64Array（每个 u64 一个矩形，保留完整 64 位，绝不截断成 32 位）。
    #[func]
    fn mesh_voxel_halo(
        &self,
        lod: i64,
        blocks: PackedByteArray,
        skirt_mask: i64,
        face_material_map: PackedByteArray,
    ) -> PackedInt64Array {
        let bytes = blocks.to_vec();
        let mask = (skirt_mask & 0x3F) as u8;
        let material_map = face_material_map.to_vec();
        let words = game_core::presentation::voxel_mesh::mesh_raw_halo_for_render(
            lod.clamp(0, 3) as u8,
            &bytes,
            mask,
            &material_map,
        );
        let signed: Vec<i64> = words.iter().map(|w| *w as i64).collect();
        PackedInt64Array::from(&signed[..])
    }

    /// 距离（米）-> LOD，用运行期配置（未设置则默认 32/64/128 m）。
    #[func]
    fn voxel_lod_for_distance(&self, distance_m: f64) -> i64 {
        i64::from(self.voxel_lod_config().lod_for_distance_m(distance_m))
    }

    /// 块（米原点 + 边长）-> LOD。距离公式的唯一真相源在 game-core。
    #[func]
    fn voxel_lod_for_block(&self, observer: Vector3, block_origin: Vector3, span_m: f64) -> i64 {
        let distance_m = nearest_block_distance_m(
            [observer.x as f64, observer.y as f64, observer.z as f64],
            [
                block_origin.x as f64,
                block_origin.y as f64,
                block_origin.z as f64,
            ],
            span_m,
        );
        i64::from(self.voxel_lod_config().lod_for_distance_m(distance_m))
    }

    /// 运行时覆写观察者位置（米）；运行期流式系统优先用它。
    #[func]
    fn set_voxel_observer(&mut self, position: Vector3) {
        if let Ok(mut runtime) = self.voxel_lod.lock() {
            // godot 的 `real` 在本工程按单精度（f32）编译，无需再转一次。
            runtime.observer = Some([position.x, position.y, position.z]);
        }
    }

    /// 运行时覆写 LOD 配置：max_lod（0..=3）+ 三档阈值（米）。
    #[func]
    fn set_voxel_lod_config(&mut self, max_lod: i64, t0: f64, t1: f64, t2: f64) {
        if let Ok(mut runtime) = self.voxel_lod.lock() {
            runtime.config = Some(VoxelLodConfig {
                max_lod: u8::try_from(max_lod).unwrap_or(3).min(3),
                thresholds_m: [t0 as f32, t1 as f32, t2 as f32],
            });
        }
    }

    /// 当前生效的 LOD 配置（已 sanitized）：max_lod + thresholds(PackedFloat32Array)。
    #[func]
    fn get_voxel_lod_config(&self) -> VarDictionary {
        let config = self.voxel_lod_config();
        let mut out = VarDictionary::new();
        out.set("max_lod", i64::from(config.max_lod));
        out.set(
            "thresholds",
            &PackedFloat32Array::from(&config.thresholds_m[..]),
        );
        out
    }

    /// 原型表：`Array[{ prototype_id, name, version }]`。
    ///
    /// Godot 用它把 `prototype_id` 映射到自己的 `PackedScene`。
    #[func]
    fn get_prototype_table(&self) -> Array<VarDictionary> {
        let tables = self.object_tables.current();
        presentation_bridge::prototype_table_to_variant(tables.as_ref())
    }

    /// 物品定义表：Array[{ item_id, name, category, category_name, sub_category, tags,
    /// max_stack, mass_grams, stack_capacity_grams, prototype_id, version }]。
    /// Godot 只拿 name 查图标 / 场景，不做业务判断。
    #[func]
    fn get_item_table(&self) -> Array<VarDictionary> {
        let tables = self.object_tables.current();
        presentation_bridge::item_table_to_variant(tables.as_ref())
    }

    /// 品类表：Array[{ id, name }]。封闭核心枚举，mod 不得新增。
    #[func]
    fn get_item_category_table(&self) -> Array<VarDictionary> {
        presentation_bridge::item_category_table_to_variant()
    }

    /// 标签表：Array[{ id, name, partition, source }]。
    #[func]
    fn get_item_tag_table(&self) -> Array<VarDictionary> {
        presentation_bridge::item_tag_table_to_variant()
    }

    /// 子品类表：Array[{ id, name, parent, parent_name, partition, source }]。
    #[func]
    fn get_item_subcategory_table(&self) -> Array<VarDictionary> {
        presentation_bridge::item_subcategory_table_to_variant()
    }

    /// 合并入口：{ categories, subcategories, tags, items }，减少跨 FFI 调用次数。
    #[func]
    fn get_item_tables(&self) -> VarDictionary {
        let tables = self.object_tables.current();
        let mut out = VarDictionary::new();
        out.set(
            "categories",
            &presentation_bridge::item_category_table_to_variant(),
        );
        out.set(
            "subcategories",
            &presentation_bridge::item_subcategory_table_to_variant(),
        );
        out.set("tags", &presentation_bridge::item_tag_table_to_variant());
        out.set(
            "items",
            &presentation_bridge::item_table_to_variant(tables.as_ref()),
        );
        out
    }

    /// 语义表：`{ domain: Array[{ id, name, partition, source }] }`（mod 基石）。
    ///
    /// 合并核心表与运行时注册；`source` 为 `core` / `mod`。
    /// 名字只用于表现层；逻辑 / 协议永远只看 ID。
    #[func]
    fn get_semantic_tables(&self) -> VarDictionary {
        presentation_bridge::semantic_tables_to_variant(&self.semantics)
    }

    /// 运行时注册 / 覆盖一条语义名（任意域名都接受）。
    ///
    /// 返回 `true` 表示新增，`false` 表示覆盖（或 id 非法）。
    /// GDScript mod 用它把自定义状态 / 动画 / 音效名注册进 `game-core`。
    #[func]
    fn register_semantic(&self, domain: GString, id: i64, name: GString) -> bool {
        let Ok(id) = u32::try_from(id) else {
            return false;
        };
        self.semantics
            .register(domain.to_string().as_str(), id, name.to_string().as_str())
    }

    /// 用名字哈希注册（推荐入口）：返回生成的 ID（>= 10000）。
    ///
    /// 例如 `register_semantic_named("overlay_tag", "mymod:wet")`。
    #[func]
    fn register_semantic_named(&self, domain: GString, namespaced_name: GString) -> i64 {
        i64::from(self.semantics.register_named(
            domain.to_string().as_str(),
            namespaced_name.to_string().as_str(),
        ))
    }

    /// mod 名字 -> 确定性语义 ID（>= 10000，不与核心分区冲突）。
    ///
    /// 只计算不注册；例如 `mod_semantic_id("mymod:burning")`。
    #[func]
    fn mod_semantic_id(&self, namespaced_name: GString) -> i64 {
        i64::from(game_core::presentation::semantics::mod_hash_id(
            namespaced_name.to_string().as_str(),
        ))
    }

    /// 诊断信息（可选调用）。
    #[func]
    fn presentation_diagnostics(&self) -> VarDictionary {
        let mut out = VarDictionary::new();
        let Some(bridge) = self.bridge.clone() else {
            return out;
        };

        out.set("session", self.session as i64);
        out.set("input_seq", self.input_seq as i64);
        out.set("submitted_frames", bridge.submitted_frames() as i64);
        out.set("unknown_actions", bridge.unknown_actions() as i64);
        out.set("dropped_events", bridge.dropped_events() as i64);
        out.set("input_suspended", bridge.input.is_suspended());
        out.set(
            "streaming",
            &presentation_bridge::streaming_diagnostics_to_dictionary(&bridge.diagnostics()),
        );
        out
    }

    /// 流式 / 驻留诊断快照（Bevy 侧每帧在 Last 集合抄一份，这里只读共享槽）。
    ///
    /// 返回 { stream: {...}, residency: {...}, memory: {...} }；
    /// **未装配那一层时对应子字典为空字典**（0 是合法值，空字典才表示「没跑」）。
    /// 后台未启动 / 已停止时返回空字典。
    #[func]
    fn streaming_diagnostics(&self) -> VarDictionary {
        let Some(bridge) = self.bridge.clone() else {
            return VarDictionary::new();
        };
        presentation_bridge::streaming_diagnostics_to_dictionary(&bridge.diagnostics())
    }

    /// 只读打开时的装配差异快照。
    ///
    /// 返回 { mods: [...], plugins: {...}, actions: [...], params: [...] }；
    /// 不是只读会话、或装配完全一致时返回空字典。
    /// 只读读档成功后才会有值，轮询方应在 streaming_diagnostics().world 出现后取一次。
    #[func]
    fn get_world_load_diff(&self) -> VarDictionary {
        let Some(bridge) = self.bridge.clone() else {
            return VarDictionary::new();
        };
        let diagnostics = bridge.diagnostics();
        presentation_bridge::world_load_diff_to_dictionary(diagnostics.diff.as_ref())
    }

    /// 运行期覆盖内存软上限（百分比，0..=100）；传负数撤销覆盖。
    ///
    /// 撤销后立刻回到 ResidencyConfig::memory_soft_limit 的配置值
    /// （覆盖是「读时生效」，不改写配置）。
    #[func]
    fn set_memory_soft_limit(&self, percent: i64) {
        let handle = MemorySoftLimitOverride::from_handle(self.memory_soft_limit.clone());
        if percent < 0 {
            handle.clear();
        } else {
            handle.set_percent(Some(u32::try_from(percent).unwrap_or(100).min(100)));
        }
    }

    /// 当前内存软上限覆盖：{ override: bool, percent: int }（percent=-1 表示关闭上限）。
    #[func]
    fn get_memory_soft_limit(&self) -> VarDictionary {
        let handle = MemorySoftLimitOverride::from_handle(self.memory_soft_limit.clone());
        let mut out = VarDictionary::new();
        match handle.get() {
            None => {
                out.set("override", false);
                out.set("percent", -1i64);
            }
            Some(None) => {
                out.set("override", true);
                out.set("percent", -1i64);
            }
            Some(Some(percent)) => {
                out.set("override", true);
                out.set("percent", i64::from(percent));
            }
        }
        out
    }

    /// 最近一次读档拒绝：`{ code, path, message }`；**空字典 = 没有拒绝过**。
    ///
    /// 值来自后台 Bevy 线程写入的共享句柄（`WorldLoadRejectionHandle`），
    /// 所以 `bev_stopped` / stop 之后仍能读：GDScript 可以「先收 bev_error
    /// 弹窗，玩家点返回 → stop，再按需读详情」。句柄为空但最后一次错误码以
    /// `save.` 开头时退化成 `{ code, path: "", message }`（headless / 旧版
    /// game-core 没有句柄时的兜底），保证原因不丢。
    #[func]
    fn world_load_rejection(&self) -> VarDictionary {
        let mut out = VarDictionary::new();
        if let Some(rejection) = self.world_load_rejection.get() {
            let code = Variant::from(GString::from(rejection.code.as_str()));
            let path = Variant::from(GString::from(rejection.path.as_str()));
            let message = Variant::from(GString::from(rejection.message.as_str()));
            out.set("code", &code);
            out.set("path", &path);
            out.set("message", &message);
            return out;
        }
        let Some((code, message)) = self.last_error.clone() else {
            return out;
        };
        if !code.starts_with("save.") {
            return out;
        }
        let code = Variant::from(GString::from(code.as_str()));
        let path = Variant::from(GString::from(""));
        let message = Variant::from(GString::from(message.as_str()));
        out.set("code", &code);
        out.set("path", &path);
        out.set("message", &message);
        out
    }
}

/// 单个路径的验证：load_package -> verify_cached -> 字典。
///
/// FFI 与单测共用；坏包走报告形状，不 panic。context 由调用方给已扫描包，
/// 本函数只排除同 id 的项，避免同一包装两次。
fn verify_one_package(
    path: &str,
    packages: &[LoadedPackage],
    cache: &mut VerificationCache,
) -> VarDictionary {
    let source = PathBuf::from(path);
    let package = match load_package(&source) {
        Ok(package) => package,
        Err(error) => return verification_report_to_dict(&package_error_report(&source, &error)),
    };
    let context: Vec<LoadedPackage> = packages
        .iter()
        .filter(|candidate| candidate.id() != package.id())
        .cloned()
        .collect();
    let report = verify_cached(&package, &context, VerifyOptions::default(), cache);
    verification_report_to_dict(&report)
}

/// VerificationReport -> Godot 字典（字段名固定）。
///
/// u64 哈希在 GDScript 侧没有无符号 64 位整数：
/// - package_hash / final_hash 给 16 位十六进制字符串（推荐用于比较 / 显示；
///   final_hash 是最后一个 tick 的世界校验和，不是包哈希）；
/// - package_hash_u64 与 hashes 按 i64 位模式给（保留全部 64 位，可能为负，仅用于透传）；
/// - first_divergence_tick 用 -1 表示 None。
fn verification_report_to_dict(report: &VerificationReport) -> VarDictionary {
    let mut out = VarDictionary::new();
    out.set("mod_id", report.mod_id.as_str());
    out.set("version", i64::from(report.version));
    out.set(
        "package_hash",
        package_hash_hex(report.package_hash).as_str(),
    );
    out.set("package_hash_u64", report.package_hash as i64);
    out.set("engine_api", i64::from(report.engine_api));
    out.set("engine_version", report.engine_version.as_str());
    out.set("ok", report.ok);
    out.set("deterministic", report.deterministic);
    out.set("rollback_equivalent", report.rollback_equivalent);
    out.set("ticks", i64::from(report.ticks));
    let hashes: Vec<i64> = report.hashes.iter().map(|hash| *hash as i64).collect();
    out.set("hashes", &PackedInt64Array::from(&hashes[..]));
    let final_hash = report.hashes.last().copied().unwrap_or(0);
    out.set("final_hash", package_hash_hex(final_hash).as_str());
    out.set(
        "first_divergence_tick",
        report.first_divergence_tick.map(i64::from).unwrap_or(-1),
    );
    let mut errors: Array<VarDictionary> = Array::new();
    for error in &report.errors {
        let mut item = VarDictionary::new();
        item.set("code", error.code.as_str());
        item.set("message", error.message.as_str());
        item.set("tick", error.tick.map(i64::from).unwrap_or(-1));
        errors.push(&item);
    }
    out.set("errors", &errors);
    out.set("cached", report.cached);
    out
}

/// 往 errors 数组追加一条 { code, message }（参数 FFI 共用）。
fn push_param_error(errors: &mut Array<VarDictionary>, code: &str, message: &str) {
    let mut error = VarDictionary::new();
    error.set("code", code);
    error.set("message", message);
    errors.push(&error);
}

/// set_param_overrides 的统一返回形状。
fn param_override_result(ok: bool, count: usize, errors: Array<VarDictionary>) -> VarDictionary {
    let mut out = VarDictionary::new();
    out.set("ok", ok);
    out.set("count", i64::try_from(count).unwrap_or(i64::MAX));
    out.set("errors", &errors);
    out
}

/// 借用字符串切片 -> Godot 字符串数组（FFI 返回值的说明性字段用）。
fn strings_to_packed(items: &[&str]) -> PackedStringArray {
    let owned: Vec<GString> = items.iter().map(|item| GString::from(*item)).collect();
    PackedStringArray::from(owned)
}

/// Godot 传来的存档目录 -> `Option<PathBuf>`：空字符串 = 不落盘。
///
/// 抽成自由函数是为了能被单测直接调用（`start_backend` 一调就起后台线程，
/// 单测里不能碰）。
fn godot_save_dir(dir: &str) -> Option<PathBuf> {
    if dir.is_empty() {
        None
    } else {
        Some(PathBuf::from(dir))
    }
}

/// 读 `user://tables/<name>`；文件不存在返回 None（该表沿用嵌入版本）。
/// 仅 dev 构建：release 不编译文件覆盖路径。
///
/// 空文件返回 Some("")：交给 Rust 解析层报错，而不是静默回退到嵌入版本。
#[cfg(debug_assertions)]
fn read_user_table(name: &str) -> Option<String> {
    let path = format!("user://tables/{name}");
    if !FileAccess::file_exists(path.as_str()) {
        return None;
    }
    Some(FileAccess::get_file_as_string(path.as_str()).to_string())
}

impl BevyAppManager {
    /// 启动后台 Bevy 后端的公共实现（被各 `#[func]` 启动入口复用）。
    ///
    /// 参数收进 [`BevyBackendConfig`]：字段名自解释，调用点不必按位置对齐 10 个
    /// 参数（bool 换 bool 位置挪错也能编译通过）。`voxel_lod` 由本节点统一注入 ——
    /// 各启动入口都不持有 LOD 运行时句柄。
    fn start_backend(&mut self, mut config: BevyBackendConfig) {
        if self.worker.is_some() {
            self.shutdown_blocking();
        }

        self.session = self.session.wrapping_add(1);
        let session = self.session;

        let (ctrl_tx, ctrl_rx) = unbounded();
        let (life_tx, life_rx) = unbounded();

        self.to_bevy = Some(ctrl_tx);
        self.from_bevy = Some(life_rx);

        config.voxel_lod = Some(self.voxel_lod.clone());
        config.memory_soft_limit = Some(self.memory_soft_limit.clone());
        config.object_tables = Some(self.object_tables.clone());
        config.packages = self.packages.clone();
        config.param_overrides = self.param_overrides.clone();
        // 新会话从零开始：上一次的拒绝不能冒充这一次的。
        self.world_load_rejection.clear();
        self.last_error = None;
        config.world_load_rejection = Some(self.world_load_rejection.clone());

        // 跨边界句柄：Godot 主线程留一份，后台线程拿一份克隆（内部是 Arc 共享）。
        let bridge = ClientBridge::with_semantics(session, self.semantics.clone());
        let thread_bridge = bridge.clone();
        self.bridge = Some(bridge);
        self.input_seq = 0;

        self.worker = Some(thread::spawn(move || {
            run_bevy_backend(ctrl_rx, life_tx, session, config, thread_bridge);
        }));
    }

    /// 读取当前生效的 LOD 配置（未设置则默认 32/64/128），已 sanitized。
    fn voxel_lod_config(&self) -> VoxelLodConfig {
        self.voxel_lod
            .lock()
            .ok()
            .and_then(|runtime| runtime.config)
            .unwrap_or_default()
            .sanitized()
    }

    fn send_control(&self, msg: BevyControlMsg) {
        if let Some(tx) = &self.to_bevy {
            let _ = tx.send(msg);
        }
    }

    fn request_shutdown_internal(&mut self) {
        if let Some(tx) = self.to_bevy.take() {
            let _ = tx.send(BevyControlMsg::Shutdown);
        }
    }

    fn join_worker_internal(&mut self) {
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }

    fn shutdown_blocking(&mut self) {
        self.request_shutdown_internal();
        self.join_worker_internal();
        self.to_bevy = None;
        self.worker = None;
        self.bridge = None;
    }

    fn cleanup_after_stop(&mut self) {
        self.join_worker_internal();
        self.to_bevy = None;
        self.worker = None;
    }

    fn emit_started(&mut self) {
        let _ = self.base_mut().emit_signal("bev_started", &[]);
    }

    fn emit_paused(&mut self) {
        let _ = self.base_mut().emit_signal("bev_paused", &[]);
    }

    fn emit_resumed(&mut self) {
        let _ = self.base_mut().emit_signal("bev_resumed", &[]);
    }

    fn emit_stopped(&mut self, exit_code: i64) {
        let _ = self
            .base_mut()
            .emit_signal("bev_stopped", &[Variant::from(exit_code)]);
    }

    fn emit_error(&mut self, code: String, message: String) {
        let code = GString::from(code.as_str());
        let msg = GString::from(message.as_str());
        let _ = self
            .base_mut()
            .emit_signal("bev_error", &[Variant::from(code), Variant::from(msg)]);
    }

    fn emit_custom(&mut self, message: String) {
        let msg = GString::from(message.as_str());
        let _ = self
            .base_mut()
            .emit_signal("bev_custom", &[Variant::from(msg)]);
    }
}

impl Drop for BevyAppManager {
    fn drop(&mut self) {
        self.shutdown_blocking();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GDScript 没有 Option：空字符串是「不要存档」的唯一表示，
    /// 非空则原样当路径（包括相对路径，交给引擎侧解析）。
    #[test]
    fn empty_save_dir_means_no_persistence() {
        assert_eq!(godot_save_dir(""), None);
        assert_eq!(
            godot_save_dir("/tmp/ecs_game_save"),
            Some(PathBuf::from("/tmp/ecs_game_save"))
        );
    }
}
