extends Node3D
## 路径 B 体素性能场景（test/voxel_perf.tscn）。
##
## 与 voxel_demo 同一套装配，只是 BevyClient 走 mode=voxel_perf：本脚本把自己的
## 导出（radius_blocks / max_lod / fixed_hz / runner_hz）在 _enter_tree 推进
## BevyClient —— 父节点 _enter_tree 早于子节点 _ready，所以这些值一定在它的
## autostart 之前生效。
##
## 统计口径：
##   - frame_ms  = _process 的 delta（Godot 实测帧时长；开垂直同步时会被钳到刷新率）
##   - fps       = Engine.get_frames_per_second()
##   - script_ms = 本脚本 _process 自身耗时（不含 BevyClient / VoxelMeshNode 的工作）
##   - entities  = BevyClient.entity_count()（稳定 id 记录数，含体素 mesh 块）
##   - nodes     = ViewLayer.node_count()（已物化显示节点，含 VoxelMeshNode）
##
## 启动：编辑器打开 test/voxel_perf.tscn 运行（F6）。Rust 侧对照数字来自
## cargo bench -p game-core --bench voxel_terrain（release）。

## 传给 start_bevy_voxel_perf 的 mesh 块半径（块）。
@export_range(0, 16, 1) var radius_blocks: int = 2
## 传给 start_bevy_voxel_perf 的最高 LOD（0..=3）。
@export_range(0, 3, 1) var max_lod: int = 0
@export var fixed_hz: float = 60.0
@export var runner_hz: float = 60.0

## 相机取景目标（米），同 voxel_demo。
@export var focus_point: Vector3 = Vector3(0.0, 28.8, 0.0)
@export var aim_camera_on_ready: bool = true

## 预热帧数（不计入统计）。
@export var warmup_frames: int = 60
## 统计帧数；到量后打印汇总并停止统计（<= 0 表示一直统计）。
@export var measure_frames: int = 600
## 每多少帧打印一行即时统计（<= 0 关闭即时行）。
@export var report_every_frames: int = 60
## 每帧都打印（调试用，输出量大）。
@export var print_every_frame: bool = false

var _client: Node = null
var _frames: int = 0
var _measured: int = 0
var _total_frame_usec: int = 0
var _max_frame_usec: int = 0
var _total_script_usec: int = 0
var _entities_max: int = 0
var _finished: bool = false


func _enter_tree() -> void:
	_client = get_node_or_null(^"BevyClient")
	if _client == null:
		return
	# 必须在 BevyClient autostart 之前写好（父 _enter_tree 先于子 _ready）。
	_client.set("manager_path", ^"../BevyAppManager")
	_client.set("entity_parent", ^"..")
	_client.set("mode", 2)  # BevyClient.Mode.VOXEL_PERF
	_client.set("channel", 2)  # PACKED_STREAMS（RECTLIST 只走快通道）
	_client.set("autostart", true)
	_client.set("fixed_hz", fixed_hz)
	_client.set("runner_hz", runner_hz)
	_client.set("enable_demo", false)
	_client.set("voxel_radius_blocks", radius_blocks)
	_client.set("voxel_max_lod", max_lod)


func _ready() -> void:
	if aim_camera_on_ready:
		var camera := get_node_or_null(^"Camera3D") as Camera3D
		if camera != null:
			camera.look_at(focus_point, Vector3.UP)

	if _client == null:
		_client = get_node_or_null(^"BevyClient")
	if _client == null:
		push_error("[voxel_perf] 场景里找不到 BevyClient 节点")
		set_process(false)
		return

	_client.connect(&"bevy_error", _on_bevy_error)
	_client.connect(&"bevy_stopped", _on_bevy_stopped)

	# 只读回检：确认 _enter_tree 的配置没被后续覆盖。
	var actual_radius := int(_client.get("voxel_radius_blocks"))
	var actual_lod := int(_client.get("voxel_max_lod"))
	if actual_radius != radius_blocks or actual_lod != max_lod:
		push_warning("[voxel_perf] 配置未生效：client radius=%d max_lod=%d，期望 %d/%d" % [actual_radius, actual_lod, radius_blocks, max_lod])
	print("[voxel_perf] 启动：radius_blocks=%d max_lod=%d fixed_hz=%.1f runner_hz=%.1f warmup=%d measure=%d" % [radius_blocks, max_lod, fixed_hz, runner_hz, warmup_frames, measure_frames])


func _process(delta: float) -> void:
	if _client == null or _finished:
		return

	var begin := Time.get_ticks_usec()
	_frames += 1
	var entities := _entity_count()
	var nodes := _node_count()
	var script_usec := Time.get_ticks_usec() - begin

	if _frames <= warmup_frames:
		if print_every_frame:
			print("[voxel_perf] warmup frame=%d frame_ms=%.2f entities=%d nodes=%d" % [_frames, delta * 1000.0, entities, nodes])
		return

	var frame_usec := int(delta * 1000000.0)
	_measured += 1
	_total_frame_usec += frame_usec
	_total_script_usec += script_usec
	_max_frame_usec = maxi(_max_frame_usec, frame_usec)
	_entities_max = maxi(_entities_max, entities)

	if print_every_frame or (report_every_frames > 0 and _measured % report_every_frames == 0):
		print("[voxel_perf] frame=%d frame_ms=%.2f fps=%.1f script_ms=%.3f entities=%d nodes=%d" % [_measured, frame_usec / 1000.0, Engine.get_frames_per_second(), script_usec / 1000.0, entities, nodes])

	if measure_frames > 0 and _measured >= measure_frames:
		_report(nodes)


func _report(nodes: int) -> void:
	_finished = true
	var measured := float(maxi(_measured, 1))
	var avg_frame_ms := (_total_frame_usec / measured) / 1000.0
	var avg_script_ms := (_total_script_usec / measured) / 1000.0
	print("[voxel_perf] 汇总 | radius_blocks=%d max_lod=%d | 帧 %d | 帧时长 avg %.2f ms / max %.2f ms | 脚本 avg %.3f ms | entities max %d | nodes %d | fps_now %.1f" % [radius_blocks, max_lod, _measured, avg_frame_ms, _max_frame_usec / 1000.0, avg_script_ms, _entities_max, nodes, Engine.get_frames_per_second()])
	print("[voxel_perf] 提示：Rust 侧对照用 cargo bench -p game-core --bench voxel_terrain。")


func _entity_count() -> int:
	if _client == null:
		return 0
	return int(_client.call("entity_count"))


func _node_count() -> int:
	if _client == null:
		return 0
	var view_layer: Object = _client.get("view_layer")
	if view_layer != null and view_layer.has_method("node_count"):
		return int(view_layer.call("node_count"))
	return 0


func _on_bevy_error(message: String) -> void:
	push_error("[voxel_perf] Bevy 后端错误：%s" % message)


func _on_bevy_stopped(exit_code: int) -> void:
	if exit_code != 0:
		push_warning("[voxel_perf] Bevy 后端已停止，exit_code=%d" % exit_code)
