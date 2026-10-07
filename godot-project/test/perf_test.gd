extends Node
## 性能测试：启动 N 实体场景，统计「每帧命令数」与「GDScript 侧取帧 + 解析」耗时。
##
## 用法：
## 1. 场景里放一个 BevyAppManager（默认路径 ^"../BevyAppManager"）；
## 2. 把本脚本挂到另一个 Node 上；
## 3. 运行后看控制台平均值，并与 game-core 的
##    dev::perf::tests::bench_presentation_pipeline_1000_entities 对照。
##
## use_packed_streams = true 走 GPF1 SoA 快通道；false 走旧 Dictionary 通道。

@export var manager_path: NodePath = ^"../BevyAppManager"
@export var entity_count: int = 1000
@export var fixed_hz: float = 60.0
@export var runner_hz: float = 60.0
@export var warmup_frames: int = 60
@export var measure_frames: int = 600
@export var use_packed_streams: bool = true

var _manager: Node
var _frame: int = 0
var _taking: bool = false
var _start_usec: int = 0
var _taken: int = 0
var _commands: int = 0
var _total_apply_usec: int = 0


func _ready() -> void:
	_manager = get_node_or_null(manager_path)
	if _manager == null:
		push_error("[perf_test] 找不到 BevyAppManager：%s" % manager_path)
		set_process(false)
		return

	_manager.bev_started.connect(_on_bev_started)
	_manager.bev_stopped.connect(_on_bev_stopped)
	print("[perf_test] 启动中：entity_count=%d, packed_streams=%s" % [entity_count, use_packed_streams])
	_manager.start_bevy_perf_test(fixed_hz, runner_hz, entity_count)


func _on_bev_started() -> void:
	print("[perf_test] Bevy 已启动")


func _on_bev_stopped(exit_code: int) -> void:
	print("[perf_test] Bevy 已停止：exit_code=%d" % exit_code)


func _process(_delta: float) -> void:
	if _manager == null:
		return

	if not _taking:
		# 预热：仍然消费通道，避免单槽合并影响测量。
		_consume()
		_frame += 1
		if _frame >= warmup_frames:
			_frame = 0
			_taking = true
			# 清掉预热期的累计，测量只统计正式阶段。
			_taken = 0
			_commands = 0
			_total_apply_usec = 0
			_start_usec = Time.get_ticks_usec()
		return

	var begin := Time.get_ticks_usec()
	var produced := _consume()
	_total_apply_usec += Time.get_ticks_usec() - begin
	if produced:
		_taken += 1

	_frame += 1
	if _frame >= measure_frames:
		_report()


func _consume() -> bool:
	if use_packed_streams:
		var packed: PackedByteArray = _manager.take_packed_presentation_frame()
		if packed.is_empty():
			return false
		var streams: Dictionary = _manager.parse_packed_presentation_streams(packed)
		_commands += int(streams.get("command_count", 0))
		return true

	var frame: Dictionary = _manager.take_presentation_frame()
	if frame.is_empty():
		return false
	_commands += (frame["commands"] as Array).size()
	return true


func _report() -> void:
	set_process(false)
	var measured_frames := float(measure_frames)
	var elapsed_ms := (Time.get_ticks_usec() - _start_usec) / 1000.0
	var apply_ms := float(_total_apply_usec) / 1000.0
	print(
		"[perf_test] 通道=%s | 总 %.1f ms / %d 帧 = %.3f ms/帧 | 取帧+解析 %.3f ms/帧 | 取到帧 %d/%d | 命令 %d (%.1f/帧) | diag=%s"
		% [
			"packed_streams" if use_packed_streams else "dictionary",
			elapsed_ms,
			measure_frames,
			elapsed_ms / measured_frames,
			apply_ms / measured_frames,
			_taken,
			measure_frames,
			_commands,
			_commands / measured_frames,
			str(_manager.presentation_diagnostics()),
		]
	)
