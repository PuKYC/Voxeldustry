extends Node3D
## 路径 B 体素演示场景（test/voxel_demo.tscn）。
##
## 场景固定提供：BevyAppManager（Rust GDExtension）、Camera3D、DirectionalLight3D，
## 以及一个 `mode = voxel_demo` 的 BevyClient 实例（res://bevy_client/bevy_client.tscn）。
## 体素网格由 BevyClient -> ViewLayer -> VoxelMeshNode（RenderingDevice 路径 B）渲染：
## 本脚本不碰几何，只做取景、错误转发与低频状态打印。
##
## 启动：编辑器里打开 test/voxel_demo.tscn 运行（F6）；或把 project.godot 的
## run/main_scene 临时指向该场景。参数（radius / LOD）在 Rust 侧
## start_bevy_voxel_demo 的默认值里，改参数请用 test/voxel_perf.tscn。

## 相机取景目标（米）。默认对准地表基线：生物群系 base_height = 64 体素 × 0.45 m。
@export var focus_point: Vector3 = Vector3(0.0, 28.8, 0.0)
## true：_ready() 里对 focus_point 做一次 look_at，保证俯视地形（不依赖手摆相机）。
@export var aim_camera_on_ready: bool = true
## 状态打印间隔（帧）；<= 0 关闭。
@export var status_every_frames: int = 120

var _client: Node = null
var _frames: int = 0


func _ready() -> void:
	if aim_camera_on_ready:
		_aim_camera()

	_client = get_node_or_null(^"BevyClient")
	if _client == null:
		push_error("[voxel_demo] 场景里找不到 BevyClient 节点")
		return
	if _client.has_signal("bevy_error"):
		_client.connect(&"bevy_error", _on_bevy_error)
	if _client.has_signal("bevy_stopped"):
		_client.connect(&"bevy_stopped", _on_bevy_stopped)
	print("[voxel_demo] mode=VOXEL_DEMO focus=%s（体素网格由 VoxelMeshNode 路径 B 渲染）" % focus_point)


func _aim_camera() -> void:
	var camera := get_node_or_null(^"Camera3D") as Camera3D
	if camera == null:
		push_warning("[voxel_demo] 场景里找不到 Camera3D，跳过取景")
		return
	camera.look_at(focus_point, Vector3.UP)


func _process(_delta: float) -> void:
	if _client == null or status_every_frames <= 0:
		return
	_frames += 1
	if _frames % status_every_frames != 0:
		return
	print("[voxel_demo] frame=%d entities=%d nodes=%d" % [_frames, _entity_count(), _node_count()])


func _entity_count() -> int:
	if _client == null:
		return 0
	return int(_client.call("entity_count"))


## 已物化显示节点数（VoxelMeshNode + 原型节点；未就绪返回 0）。
func _node_count() -> int:
	if _client == null:
		return 0
	var view_layer: Object = _client.get("view_layer")
	if view_layer != null and view_layer.has_method("node_count"):
		return int(view_layer.call("node_count"))
	return 0


func _on_bevy_error(message: String) -> void:
	push_error("[voxel_demo] Bevy 后端错误：%s" % message)


func _on_bevy_stopped(exit_code: int) -> void:
	if exit_code != 0:
		push_warning("[voxel_demo] Bevy 后端已停止，exit_code=%d" % exit_code)