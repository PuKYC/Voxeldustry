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
};
use godot::prelude::*;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use game_core::bevy_backend::*;
use game_core::presentation::semantics::SemanticRegistry;
use game_core::spec::ClientBridge;
use game_core::voxel::terrain::{nearest_block_distance_m, TerrainConfig, VoxelLodRuntime};
use game_core::voxel::VoxelLodConfig;

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
                BevyLifecycleMsg::Error(message) => self.emit_error(message),
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

    #[signal]
    fn bev_error(message: GString);

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
        self.start_backend(fixed_hz, runner_hz, enable_demo, None, None, false);
    }

    /// 启动性能测试场景：生成 `entity_count` 个每 tick 移动的实体（取代 demo）。
    ///
    /// 用于量化「模拟 → 收集 → 编码 → 发布」各段耗时；Godot 侧可配合
    /// `parse_packed_frame_header()` 读 `command_count` 与 `render_clock_ms`。
    #[func]
    fn start_bevy_perf_test(&mut self, fixed_hz: f64, runner_hz: f64, entity_count: i64) {
        let count = usize::try_from(entity_count).unwrap_or(1).max(1);
        self.start_backend(fixed_hz, runner_hz, false, Some(count), None, false);
    }

    /// 启动体素地形 demo：挂 `TerrainConfig::default()`（radius 2 / max_lod 1）。
    ///
    /// 地形在 Startup 生成一次，产出带 `VoxChunkRaw` 的 mesh 块实体；
    /// 贪婪 meshing 在 Godot 侧调 gdext `mesh_voxel_halo` 现算，再由 VoxelMeshNode 渲染。
    ///
    /// 默认 radius 2 / max_lod 1（196 子块）：release ≈ 2–4 s，**debug ≈ 30 s**
    /// （生成是 debug 下的瓶颈）。只想快速看一眼可以用
    /// `start_bevy_voxel_perf(fixed_hz, runner_hz, 0, 0)`（36 子块，≈ 5 s）。
    #[func]
    fn start_bevy_voxel_demo(&mut self, fixed_hz: f64, runner_hz: f64) {
        self.start_backend(
            fixed_hz,
            runner_hz,
            false,
            None,
            Some(TerrainConfig::default()),
            false,
        );
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
        self.start_backend(fixed_hz, runner_hz, false, None, Some(terrain), true);
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

    /// 启动时拉取动作表：`Array[{ name, bit }]`（按键由 Godot 侧配置）。
    ///
    /// 名字与通道位的唯一真相源是 `game_core::input::actions::ACTION_TABLE`。
    #[func]
    fn get_action_table(&self) -> Array<VarDictionary> {
        presentation_bridge::action_table_to_variant()
    }

    /// 每帧一次批量提交（避免 N 次跨 FFI）。
    ///
    /// - `held` / `pressed` / `released`：语义动作**名**列表（不是物理键）；
    /// - `move_axis` / `look_delta`：原始 `f32`，量化在 `game-core`；
    /// - `render_clock_ms`：Godot 渲染时钟，供 `game-core` 算插值 alpha。
    ///
    /// 注意：`u64` 不能跨 gdext FFI，所以这里一律用 `i64`。
    #[func]
    #[allow(clippy::too_many_arguments)]
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

    /// 体素调色板表（路径 B GPU 渲染启动时拉一次）：
    /// `{ voxel_size, palette(256×RGBA8 字节), material_count }`。
    #[func]
    fn get_voxel_table(&self) -> VarDictionary {
        presentation_bridge::voxel_table_to_variant()
    }

    /// 原始体素 halo → 39 bit 矩形流（调用 game-core mesher 现算）。
    ///
    /// lod 越界会被夹到 0..=3；blocks 是 RAWVOXELS 载荷解出的原始体素字节。
    /// mesher 输出 Vec<u64>，而 u64 没有 ToGodot，这里逐元素按位转 i64
    /// 交给 PackedInt64Array（每个 u64 一个矩形，保留完整 64 位，绝不截断成 32 位）。
    #[func]
    fn mesh_voxel_halo(&self, lod: i64, blocks: PackedByteArray) -> PackedInt64Array {
        let bytes = blocks.to_vec();
        let words = game_core::presentation::mesh_raw_halo(lod.clamp(0, 3) as u8, &bytes);
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
            runtime.observer = Some([position.x as f32, position.y as f32, position.z as f32]);
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
        presentation_bridge::prototype_table_to_variant()
    }

    /// 物品定义表：Array[{ item_id, name, category, category_name, sub_category, tags,
    /// max_stack, prototype_id, version }]。Godot 只拿 name 查图标 / 场景，不做业务判断。
    #[func]
    fn get_item_table(&self) -> Array<VarDictionary> {
        presentation_bridge::item_table_to_variant()
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
        out.set("items", &presentation_bridge::item_table_to_variant());
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
        out
    }
}

impl BevyAppManager {
    /// 启动后台 Bevy 后端的公共实现（被各 `#[func]` 启动入口复用）。
    fn start_backend(
        &mut self,
        fixed_hz: f64,
        runner_hz: f64,
        enable_demo: bool,
        perf_entity_count: Option<usize>,
        terrain: Option<TerrainConfig>,
        terrain_perf: bool,
    ) {
        if self.worker.is_some() {
            self.shutdown_blocking();
        }

        self.session = self.session.wrapping_add(1);
        let session = self.session;

        let (ctrl_tx, ctrl_rx) = unbounded();
        let (life_tx, life_rx) = unbounded();

        self.to_bevy = Some(ctrl_tx);
        self.from_bevy = Some(life_rx);

        let config = BevyBackendConfig {
            runner_hz,
            fixed_hz,
            enable_demo,
            perf_entity_count,
            terrain,
            terrain_perf,
            voxel_lod: Some(self.voxel_lod.clone()),
        };

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

    fn emit_error(&mut self, message: String) {
        let msg = GString::from(message.as_str());
        let _ = self
            .base_mut()
            .emit_signal("bev_error", &[Variant::from(msg)]);
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
