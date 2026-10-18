extends Node3D
## 游戏内容测试场景：静态体素平台（带碰撞） + 玩家（带碰撞）。
##
## 场景内容全部由 game-core 的 `dev::gameplay_test` 生成：
## - 平台：VoxVolume + VoxelCollider + RigidBody::Static，通过 RawVoxelGroups 渲染；
## - 玩家：LocalPlayer + 动态刚体 + 胶囊子碰撞体，WASD 移动、Space 跳跃。
##
## 本脚本只做表现侧装配：实例化 BevyClient、注册玩家原型场景、绑定相机 / 灯光。
## 启动入口：BevyClient.mode = GAMEPLAY_TEST -> start_bevy_gameplay_test。
##
## 用法：编辑器打开本场景按 F6，或把 project.godot 的 run/main_scene 指向本场景。

const BEVY_CLIENT_SCENE := preload("res://bevy_client/bevy_client.tscn")
const BEVY_CLIENT_SCRIPT := preload("res://bevy_client/bevy_client.gd")
const PLAYER_SCENE := preload("res://test/player_test.tscn")

## PrototypeId(1) = player（见 game-core static_data/prototype.rs）。
const PLAYER_PROTOTYPE := 1

@export var camera_fov: float = 70.0
@export var capture_mouse: bool = true

var _client: Node = null
var _camera: CameraRig = null


func _ready() -> void:
	_setup_light()
	_setup_client()
	_setup_camera()


func _setup_light() -> void:
	var light := DirectionalLight3D.new()
	light.name = "DirectionalLight3D"
	light.rotation_degrees = Vector3(-55.0, -35.0, 0.0)
	light.shadow_enabled = true
	add_child(light)


func _setup_client() -> void:
	_client = BEVY_CLIENT_SCENE.instantiate()
	_client.name = "BevyClient"
	# 与 bevy_client.gd 的 @export 对齐：管理器是兄弟节点，实体挂在场景根。
	_client.set("manager_path", ^"../BevyAppManager")
	_client.set("entity_parent", ^"..")
	_client.set("mode", BEVY_CLIENT_SCRIPT.Mode.GAMEPLAY_TEST)
	_client.set("enable_demo", false)
	# 玩家原型 -> 胶囊 Mesh 场景；平台走 RawVoxelGroups，不需要原型场景。
	_client.set("scenes", { PLAYER_PROTOTYPE: PLAYER_SCENE })
	add_child(_client)

	_client.connect(&"bevy_error", _on_bevy_error)
	_client.connect(&"bevy_stopped", _on_bevy_stopped)


func _setup_camera() -> void:
	_camera = CameraRig.new()
	_camera.name = "Camera3D"
	_camera.fov = camera_fov
	_camera.camera_mode = CameraRig.CameraMode.FOLLOW_BACKEND
	_camera.capture_mouse = capture_mouse
	add_child(_camera)

	# 后端首帧到达前先给一个能看见平台 / 玩家的取景；随后由 CameraRig 接管。
	_camera.position = Vector3(7.2, 5.0, 16.0)
	_camera.look_at(Vector3(7.2, 2.0, 7.2), Vector3.UP)
	# 必须在首帧之前绑定，否则会错过只下发一次的 CAMERA 载荷。
	_camera.bind(_client)


func _on_bevy_error(message: String) -> void:
	push_error("[gameplay_test] Bevy 后端错误：%s" % message)


func _on_bevy_stopped(exit_code: int) -> void:
	if exit_code != 0:
		push_warning("[gameplay_test] Bevy 后端已停止，exit_code=%d" % exit_code)
