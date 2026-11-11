extends Node3D
## 流式世界 / 体素流式检查场景（对应 `start_bevy_streaming`）。
##
## 覆盖六项：世界流式启动、体素流式加载、驻留升降档、内存软上限压力、
## 世界存档（停止后重进）、显示层与统计一致。
##
## 一个场景跑完整个流程后自己退出（headless 必须这样）：
##   ./build.sh --release
##   /tmp/Godot_v4.7.2-stable_linux.arm64 --headless --path godot-project test/streaming_check.tscn
## 必须用 release 库：debug（opt-level 0）下体素网格化每块要 1 s 量级，一批 33 块
## 会撑爆单帧（实测 debug 下 222 s 只推进 11 帧、约 2.3 s 仿真），阶段全部超时。
## release 下全程约 7 s。
## stdout 里每条断言是一行 `[streaming_check] CHECK PASS|FAIL`，末尾一行
## `RESULT PASS|FAIL`；退出码 0 = 全部通过，1 = 有失败。
##
## 断言口径与用户原始期望有两处偏差（引擎语义决定，不是阈值放宽）：
## 1. 「空存档启动初始 render_groups == 0」：首个快照到 Godot 时已经过至少
##    一个 tick（流式每 tick 结算 8 块），所以改断言「首个快照 <= 8 且随后
##    增长」；「秒开」用首快照延迟 < 3 s 表达。
## 2. 「退出重进后 loaded_from_store > 0 且 generated 近 0」：重进走的是
##    load_world，直接把存档块装进容器，loaded_from_store 恒为 0；且未编辑
##    的块根本不落内容。所以重进改断言 world.loaded == true 且
##    world.chunks >= 1；loaded_from_store 那条断言语义改由「同一会话内飞走
##    再飞回」覆盖（已编辑块卸载落盘 → 回程命中存档）。
##
## 观察者协议：扩展侧只在 start_bevy_streaming 下装配 ObserverDrivenStreamingPlayer，
## 它把 Godot 推进来的 voxel observer 位置写成玩家 Transform，于是
## set_voxel_observer 既驱动 LOD 也驱动世界流式的驻留锚点。

const VOXEL_VIEWER_SOURCE := 4
const BREAK_BLOCK_ACTION := "break_block"
## 出生点附近（世界原点那块），也是回程落点。
const SPAWN_POS := Vector3(7.2, 20.0, 7.2)
## 远点：离出生点 600 m，足以让出生点周围的块全部卸载落盘。
const FAR_POS := Vector3(600.0, 80.0, 0.0)
## 内存压力测试落点：既不是出生点也不是远点。
const PRESSURE_POS := Vector3(300.0, 40.0, 300.0)
const SAVE_DIR_NAME := "streaming_check_save"
## 单个阶段的墙钟超时。release 下全过程约 7 s；给到 120 s 只是留余量的保险丝。
const PHASE_TIMEOUT_MS := 120000
## 破坏方块重按间隔（tick 冷却 6 = 100 ms；给到 200 ms 更稳）。
const BREAK_INTERVAL_MS := 200
## 判断「显示一致」前先给表现层一帧同步时间。
const DISPLAY_SETTLE_MS := 400

enum Phase {
	BOOT,             ## 等首个流式快照
	LOAD,             ## generated 增长、queued / pending 回落
	DISPLAY,          ## VoxelGroupNode 数 == render_groups
	EDIT,             ## 破坏一个方块，让某块变成「已编辑」
	FLY_OUT,          ## 推远 → 卸载并落盘
	FLY_BACK,         ## 飞回 → 命中存档
	PRESSURE,         ## memory_soft_limit=0 → 升档被压住
	PRESSURE_RECOVER, ## 撤销上限 → 升档恢复
	RESTART_STOP,     ## request_stop，等世界落盘
	RESTART_START,    ## 重新 start_bevy_streaming
	RESTART_WAIT,     ## 等读档结果
	FINISHED,
}

var _client: Node
var _mgr: Node
var _save_dir: String = ""

var _phase: int = Phase.BOOT
var _phase_started_ms: int = 0
var _started_ms: int = 0
var _last_status_ms: int = 0

var _checks: Array[String] = []
var _failures: Array[String] = []

# 各阶段的快照（用于「增量」断言，避免绝对值受世界内容影响）。
var _boot: Dictionary = {}
var _load: Dictionary = {}
var _edit: Dictionary = {}
var _out: Dictionary = {}
var _back: Dictionary = {}
var _pressure: Dictionary = {}
var _restart: Dictionary = {}

var _last_generated: int = -1
var _stop_requested: bool = false
var _stopped_code: int = -999
var _last_break_ms: int = 0
var _restart_requested_at_ms: int = 0


func _enter_tree() -> void:
	# 父节点的 _enter_tree 早于子节点的 _ready：BevyClient 的 autostart 在
	# _ready 里跑，所以这些导出必须在这里写（仿 voxel_perf.gd）。
	_save_dir = ProjectSettings.globalize_path("user://" + SAVE_DIR_NAME)
	_wipe_dir(_save_dir)
	DirAccess.make_dir_recursive_absolute(_save_dir)
	var client := get_node_or_null(^"BevyClient")
	if client != null:
		client.set("manager_path", ^"../BevyAppManager")
		client.set("entity_parent", ^"..")
		client.set("mode", 4) # Mode.STREAMING
		client.set("channel", 2) # PACKED_STREAMS
		client.set("autostart", true)
		client.set("enable_demo", false)
		client.set("save_dir", _save_dir)


func _ready() -> void:
	_started_ms = _now()
	_client = get_node_or_null(^"BevyClient")
	_mgr = get_node_or_null(^"BevyAppManager")
	if _client == null or _mgr == null:
		push_error("[streaming_check] 场景缺少 BevyClient / BevyAppManager")
		get_tree().quit(1)
		return
	_client.connect(&"bevy_error", _on_client_error)
	_client.connect(&"bevy_stopped", _on_client_stopped)
	print("[streaming_check] 存档目录：%s" % _save_dir)
	_enter(Phase.BOOT)


# ───────────────────────────── 主循环 / 阶段机 ─────────────────────────────

func _process(_delta: float) -> void:
	if _phase == Phase.FINISHED:
		return
	var age_ms := _now() - _phase_started_ms
	if age_ms > _phase_timeout_ms():
		_fail("阶段 %s 超时 %d ms（现状：%s）" % [_phase_name(), age_ms, _describe()])
		_finish()
		return
	if _now() - _last_status_ms >= 2000:
		_last_status_ms = _now()
		print("[streaming_check] t=%.1fs %s %s" % [(_now() - _started_ms) / 1000.0, _phase_name(), _describe()])
	match _phase:
		Phase.BOOT: _tick_boot()
		Phase.LOAD: _tick_load()
		Phase.DISPLAY: _tick_display()
		Phase.EDIT: _tick_edit()
		Phase.FLY_OUT: _tick_fly_out()
		Phase.FLY_BACK: _tick_fly_back()
		Phase.PRESSURE: _tick_pressure()
		Phase.PRESSURE_RECOVER: _tick_pressure_recover()
		Phase.RESTART_STOP: _tick_restart_stop()
		Phase.RESTART_START: _tick_restart_start()
		Phase.RESTART_WAIT: _tick_restart_wait()


## 世界流式（启动）：扩展已起来、流式层已装配，且不是从档恢复。
func _tick_boot() -> void:
	var diag := _diag()
	var stream := _dict(diag, "stream")
	if stream.is_empty():
		return
	_boot = diag
	var groups := _int(stream, "render_groups")
	var elapsed := _now() - _started_ms
	_check(elapsed < 3000, "世界启动：首个流式快照在 %d ms 内到达（秒开）" % elapsed)
	# 每 tick 结算 8 块，所以首个快照最多一批；严格 ==0 在 Godot 侧不可观测。
	_check(groups <= 8, "世界启动：首个快照 render_groups=%d <= 8（尚未铺满）" % groups)
	var world := _dict(diag, "world")
	_check(not bool(world.get("loaded", false)), "世界启动：空存档目录，world.loaded=false")
	_enter(Phase.LOAD)


## 体素流式（加载）：generated 单调增长，队列排空，分组数比启动时多。
func _tick_load() -> void:
	var diag := _diag()
	var stream := _dict(diag, "stream")
	if stream.is_empty():
		return
	var generated := _int(stream, "generated")
	if _last_generated >= 0 and generated < _last_generated:
		_fail("体素流式：generated 回退 %d -> %d" % [_last_generated, generated])
	_last_generated = generated
	var groups := _int(stream, "render_groups")
	var queued := _int(stream, "queued")
	var pending := _int(stream, "pending")
	var staged := _int(stream, "staged")
	if groups < 8 or queued != 0 or pending != 0 or staged != 0:
		return
	_load = diag
	var boot_stream := _dict(_boot, "stream")
	var residency := _dict(diag, "residency")
	_check(generated > 0 and generated > _int(boot_stream, "generated"), "体素流式：generated %d -> %d（单调增长）" % [_int(boot_stream, "generated"), generated])
	_check(groups > _int(boot_stream, "render_groups"), "体素流式：render_groups %d -> %d（分组增长）" % [_int(boot_stream, "render_groups"), groups])
	_check(queued == 0 and pending == 0 and staged == 0, "体素流式：queued / pending / staged 全部回落到 0")
	_check(_int(residency, "render_chunks") > 0, "体素流式：驻留 render_chunks=%d > 0" % _int(residency, "render_chunks"))
	_enter(Phase.DISPLAY)


## 显示一致：一个 chunk 一个 VoxelGroupNode，节点数应与 render_groups 相等。
func _tick_display() -> void:
	if _now() - _phase_started_ms < DISPLAY_SETTLE_MS:
		return
	var stream := _dict(_diag(), "stream")
	if stream.is_empty():
		return
	var groups := _int(stream, "render_groups")
	var nodes := _count_voxel_groups(self)
	if nodes != groups:
		return
	_check(true, "显示一致：VoxelGroupNode %d == render_groups %d" % [nodes, groups])
	var entities := int(_client.call("entity_count"))
	_check(entities >= groups, "显示一致：entity_count=%d >= render_groups=%d（含玩家 / 相机 / 树）" % [entities, groups])
	_enter(Phase.EDIT)


## 制造一个「已编辑块」：站在地表、把视角越压越低，然后按 break_block。
func _tick_edit() -> void:
	if _now() - _last_break_ms >= BREAK_INTERVAL_MS:
		_last_break_ms = _now()
		_submit_break()
	var edited := _int(_diag(), "edited_chunks")
	if edited > 0:
		_edit = _diag()
		_check(true, "方块编辑：edited_chunks=%d > 0（该块卸载时会写内容）" % edited)
		_enter(Phase.FLY_OUT)


## 驻留（卸档）：推远 → 远端块卸载，已编辑块因此落盘。
func _tick_fly_out() -> void:
	_push_observer(FAR_POS)
	var diag := _diag()
	var stream := _dict(diag, "stream")
	if stream.is_empty():
		return
	var before := _int(_dict(_edit, "stream"), "unloaded")
	var now := _int(stream, "unloaded")
	if now - before >= 8 and _int(stream, "pending_unloads") == 0:
		_out = diag
		_check(true, "驻留卸档：unloaded %d -> %d，pending_unloads 归 0" % [before, now])
		_enter(Phase.FLY_BACK)


## 驻留（回程）：飞回出生点，已编辑块从存档恢复 → loaded_from_store 增长。
func _tick_fly_back() -> void:
	_push_observer(SPAWN_POS)
	var diag := _diag()
	var stream := _dict(diag, "stream")
	if stream.is_empty():
		return
	var before := _int(_dict(_edit, "stream"), "loaded_from_store")
	var now := _int(stream, "loaded_from_store")
	if now > before:
		_back = diag
		_check(true, "驻留回程：loaded_from_store %d -> %d（已编辑块命中存档）" % [before, now])
		_check(_int(_dict(diag, "residency"), "render_chunks") > 0, "驻留回程：render_chunks 重新 > 0")
		_enter(Phase.PRESSURE)


## 内存压力：把软上限压到 0，新区域的升档应被压住（deferred_upgrades 增长）。
func _tick_pressure() -> void:
	_push_observer(PRESSURE_POS)
	_client.call("set_memory_soft_limit", 0)
	var diag := _diag()
	var stream := _dict(diag, "stream")
	if stream.is_empty():
		return
	var before := _int(_dict(_back, "residency"), "deferred_upgrades")
	var now := _int(_dict(diag, "residency"), "deferred_upgrades")
	if now > before:
		_pressure = diag
		_check(true, "内存压力：memory_soft_limit=0 时 deferred_upgrades %d -> %d（升档被压住）" % [before, now])
		_enter(Phase.PRESSURE_RECOVER)


## 内存压力解除：撤销上限后新区域应升到 Render。
func _tick_pressure_recover() -> void:
	_client.call("set_memory_soft_limit", -1)
	var diag := _diag()
	var stream := _dict(diag, "stream")
	if stream.is_empty():
		return
	var groups := _int(stream, "render_groups")
	var pending := _int(stream, "pending")
	var deferred := _int(_dict(diag, "residency"), "deferred_upgrades")
	if deferred != _int(_dict(_pressure, "residency"), "deferred_upgrades"):
		return
	if groups >= 8 and pending == 0:
		_check(true, "内存压力解除：撤销上限后 render_groups=%d 恢复，deferred_upgrades 停在 %d" % [groups, deferred])
		_enter(Phase.RESTART_STOP)


## 世界存档（一）：把观察者放回出生点，request_stop，等后端退出（退出前落盘）。
func _tick_restart_stop() -> void:
	_push_observer(SPAWN_POS)
	if not _stop_requested:
		_stop_requested = true
		_stopped_code = -999
		print("[streaming_check] request_stop（等世界落盘）")
		_mgr.call("request_stop")
		return
	if _stopped_code == -999:
		return
	_check(true, "世界存档：后端已停止（exit_code=%d），世界应已落盘" % _stopped_code)
	_enter(Phase.RESTART_START)


## 世界存档（二）：同一 save_dir 重新启动流式后端。
func _tick_restart_start() -> void:
	print("[streaming_check] 重启后端：start_bevy_streaming(save_dir=%s)" % _save_dir)
	_restart_requested_at_ms = _now()
	_mgr.call("start_bevy_streaming", 60.0, 60.0, _save_dir)
	_enter(Phase.RESTART_WAIT)


## 世界存档（三）：重进后应当是从档恢复（load_world），而不是重新生成。
func _tick_restart_wait() -> void:
	var diag := _diag()
	var stream := _dict(diag, "stream")
	if stream.is_empty():
		return
	var world := _dict(diag, "world")
	if not bool(world.get("loaded", false)):
		return
	var groups := _int(stream, "render_groups")
	if groups <= 0:
		return
	_restart = diag
	var restart_ms := _now() - _restart_requested_at_ms
	_check(true, "世界存档：重进后 world.loaded=true（%d ms 内读档完成）" % restart_ms)
	_check(_int(world, "chunks") >= 1, "世界存档：重进恢复 %d 块 / %d 体 / %d 实体（stale=%d）" % [_int(world, "chunks"), _int(world, "bodies"), _int(world, "entities"), _int(world, "stale_chunks")])
	_check(groups > 0, "世界存档：重进后重新开始流式（render_groups=%d）" % groups)
	_enter(Phase.FINISHED)
	_finish()


# ───────────────────────────── 工具 ─────────────────────────────

func _enter(phase: int) -> void:
	_phase = phase
	_phase_started_ms = _now()
	print("[streaming_check] ===> %s" % _phase_name())


func _phase_name() -> String:
	return str(Phase.keys()[_phase]).to_lower()


func _phase_timeout_ms() -> int:
	if _phase == Phase.BOOT:
		return 30000
	if _phase == Phase.RESTART_STOP or _phase == Phase.RESTART_START or _phase == Phase.RESTART_WAIT:
		return 60000
	return PHASE_TIMEOUT_MS


func _now() -> int:
	return Time.get_ticks_msec()


func _diag() -> Dictionary:
	if _client == null:
		return {}
	var snap: Variant = _client.call("streaming_diagnostics")
	return snap if snap is Dictionary else {}


func _dict(source: Dictionary, key: String) -> Dictionary:
	var value: Variant = source.get(key)
	return value if value is Dictionary else {}


func _int(source: Dictionary, key: String) -> int:
	var value: Variant = source.get(key)
	if value is int or value is float:
		return int(value)
	return 0


func _push_observer(position: Vector3) -> void:
	_client.call("set_voxel_observer", position)


## 按一次 break_block，同时把视角往下压（look_pitch > 0 = 向下看，3.0 rad / 1000 px）。
## 多按几次后 pitch 会夹到接近 -90°，保证射线打在脚下 6 m 内。
func _submit_break() -> void:
	_mgr.call(
		"submit_input_frame",
		PackedStringArray(),
		PackedStringArray([BREAK_BLOCK_ACTION]),
		PackedStringArray(),
		Vector2.ZERO,
		Vector2(0.0, 40.0),
		_now(),
		VOXEL_VIEWER_SOURCE
	)


## 显示层实体数（脚本免崩：client 还没挂上时给 0）。
func _entity_count() -> int:
	if _client == null:
		return 0
	return int(_client.call("entity_count"))


## 统计**可见**的 VoxelGroupNode：池化回收只调 reset()（visible=false）并留在
## entity_parent 下，所以不能把不可见的旧节点算进来。
func _count_voxel_groups(root: Node) -> int:
	var total := 0
	for child in root.get_children():
		if child is VoxelGroupNode:
			if (child as Node3D).visible:
				total += 1
		total += _count_voxel_groups(child)
	return total


func _describe() -> String:
	var diag := _diag()
	var stream := _dict(diag, "stream")
	if stream.is_empty():
		return "<无流式快照：扩展未启动或未装配流式层>"
	var residency := _dict(diag, "residency")
	var memory := _dict(diag, "memory")
	var world := _dict(diag, "world")
	return "q=%d p=%d st=%d gen=%d store=%d unload=%d groups=%d | chunks n/d/r=%d/%d/%d defer=%d | mem=%d%%(%d/%d) | nodes=%d edited=%d world{loaded=%s chunks=%d} ent=%d" % [
		_int(stream, "queued"),
		_int(stream, "pending"),
		_int(stream, "staged"),
		_int(stream, "generated"),
		_int(stream, "loaded_from_store"),
		_int(stream, "unloaded"),
		_int(stream, "render_groups"),
		_int(residency, "none_chunks"),
		_int(residency, "data_chunks"),
		_int(residency, "render_chunks"),
		_int(residency, "deferred_upgrades"),
		_int(memory, "fill_percent"),
		_int(memory, "used_nodes"),
		_int(memory, "capacity_nodes"),
		_count_voxel_groups(self),
		_int(diag, "edited_chunks"),
		str(bool(world.get("loaded", false))),
		_int(world, "chunks"),
		_entity_count(),
	]


func _check(ok: bool, message: String) -> void:
	if ok:
		_checks.append(message)
		print("[streaming_check] CHECK PASS  %s" % message)
	else:
		_fail(message)


func _fail(message: String) -> void:
	_failures.append(message)
	print("[streaming_check] CHECK FAIL  %s" % message)


func _on_client_error(code: String, message: String) -> void:
	_fail("Bevy 错误 [%s]：%s" % [code, message])


func _on_client_stopped(exit_code: int) -> void:
	_stopped_code = exit_code


func _finish() -> void:
	var failed := _failures.size()
	var total_ms := _now() - _started_ms
	print("")
	print("================ streaming_check 汇总 ================")
	for line in _checks:
		print("  PASS  %s" % line)
	for line in _failures:
		print("  FAIL  %s" % line)
	print("  通过 %d，失败 %d，耗时 %.1f s" % [_checks.size(), failed, total_ms / 1000.0])
	print("  最终状态：%s" % _describe())
	print("[streaming_check] RESULT %s" % ("PASS" if failed == 0 else "FAIL"))
	print("=====================================================")
	_phase = Phase.FINISHED
	get_tree().quit(0 if failed == 0 else 1)


func _wipe_dir(path: String) -> void:
	if not DirAccess.dir_exists_absolute(path):
		return
	var dir := DirAccess.open(path)
	if dir == null:
		return
	dir.list_dir_begin()
	var name := dir.get_next()
	while name != "":
		if name != "." and name != "..":
			var child := path.path_join(name)
			if DirAccess.dir_exists_absolute(child):
				_wipe_dir(child)
			DirAccess.remove_absolute(child)
		name = dir.get_next()
	dir.list_dir_end()
