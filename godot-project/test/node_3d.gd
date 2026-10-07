extends Node3D
## 演示场景的装配点。
##
## 这里只负责相机 / 灯光与实例化 BevyClient：
## - `BevyAppManager`（Rust GDExtension）由场景里的兄弟节点提供；
## - `res://bevy_client/bevy_client.tscn` 负责取帧、输入采集、事件路由与视图物化。

const BEVY_CLIENT_SCENE := preload("res://bevy_client/bevy_client.tscn")

## 相机距离（纯表现，和逻辑无关）。
@export var camera_height: float = 90.0
@export var camera_distance: float = 110.0

var _client: Node = null


func _ready() -> void:
	_setup_camera_and_light()

	_client = BEVY_CLIENT_SCENE.instantiate()
	_client.name = "BevyClient"
	# 与 bevy_client.gd 的 @export 对齐：管理器是兄弟节点，实体挂在场景根。
	_client.set("manager_path", ^"../BevyAppManager")
	_client.set("entity_parent", ^"..")
	add_child(_client)

	_client.connect(&"bevy_error", _on_bevy_error)
	_client.connect(&"bevy_stopped", _on_bevy_stopped)


func _setup_camera_and_light() -> void:
	var camera := Camera3D.new()
	camera.name = "Camera3D"
	camera.fov = 60.0
	add_child(camera)
	camera.look_at_from_position(
		Vector3(0.0, camera_height, camera_distance),
		Vector3.ZERO,
		Vector3.UP
	)

	var light := DirectionalLight3D.new()
	light.name = "DirectionalLight3D"
	light.rotation_degrees = Vector3(-60.0, -30.0, 0.0)
	light.shadow_enabled = true
	add_child(light)


var _dbg_t := 0.0

func _debug_print(delta: float) -> void:
	_dbg_t += delta
	if _dbg_t < 0.5 or _client == null:
		return
	_dbg_t = 0.0
	var all: Array = _client._registry.all()
	var pos = "none"
	if all.size() > 0:
		var n = _client.view_layer.node_of(all[all.size() - 1])
		if n != null:
			pos = n.position
	print("ms=%d tick=%d active=%d total=%d pos=%s" % [
		Time.get_ticks_msec(), _client._tick,
		_client._registry.active().size(), all.size(), pos])


func _process(_delta: float) -> void:
	# _debug_print(delta)
	# 诊断：节点数应约等于「本地玩家视野内的实体数」，而不是全量实体数。
	if _client != null and Engine.get_process_frames() % 120 == 0:
		print_verbose("[node_3d] 已同步实体数 = %d" % int(_client.call("entity_count")))


func _on_bevy_error(message: String) -> void:
	push_error("[node_3d] Bevy 后端错误：%s" % message)


func _on_bevy_stopped(exit_code: int) -> void:
	if exit_code != 0:
		push_warning("[node_3d] Bevy 后端已停止，exit_code=%d" % exit_code)
