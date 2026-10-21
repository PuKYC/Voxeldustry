class_name CameraRig
extends Camera3D
## 后端相机的 Godot 端。
##
## 只做机械搬运：把 Bevy 同步下来的相机视角（CAMERA 载荷：yaw / pitch）和位置
## （同一实体的 TRANSFORM 载荷）写到本 Camera3D 上。朝向怎么累加、俯仰怎么夹、
## 相机跟谁，全部在 game-core（gameplay/camera.rs），这里不做任何视角决策。
##
## 用法：
##   var rig := CameraRig.new()
##   add_child(rig)
##   rig.bind(bevy_client)   # 要在后端发出首帧之前调用
##
## 相机模式见 CameraMode。鼠标捕获：FOLLOW_BACKEND + capture_mouse 时自动捕获，
## 点击窗口重新捕获。Esc / 暂停菜单 / 触屏方案由 UiRoot 统一协调：
## UiRoot 通过 `camera_rig` 组调用 set_ui_blocked()，本脚本不再处理 ui_cancel。

enum CameraMode {
	STATIC,         ## 不接管：保持场景里摆好的相机（旧行为）
	FOLLOW_BACKEND, ## 跟随后端相机实体（带 CameraFlag 的实体）
}

@export var camera_mode: CameraMode = CameraMode.FOLLOW_BACKEND
## FOLLOW_BACKEND 下是否捕获鼠标（FPS 视角必需）。
@export var capture_mouse: bool = true

# 不标注成 Node：BevyClient 的 codec / render_alpha 等成员在 Node 上不存在。
var _client = null
var _entity: EntityData = null
var _captured_by_rig := false
## UiRoot 置位：菜单打开 / 触屏方案时为 true，期间不捕获鼠标、不消费输入。
var _ui_blocked := false


func _init() -> void:
	# 晚于 BevyClient._process（默认 0）：同一帧里先收后端数据再摆相机。
	process_priority = 100


func _ready() -> void:
	add_to_group("camera_rig")


func _exit_tree() -> void:
	_release_mouse()


## 绑定 BevyClient。要在后端发出首帧之前调用，否则会错过已发出的 CAMERA 载荷
## （载荷只在首次出现与变更时下发）。
func bind(client) -> void:
	if _client != null:
		_unbind()
	_client = client
	_client.entity_state_changed.connect(_on_entity_state_changed)
	_client.entity_removed.connect(_on_entity_removed)
	_apply_mode()


func set_camera_mode(mode: CameraMode) -> void:
	camera_mode = mode
	_apply_mode()


## UiRoot 调用：菜单打开 / 触屏方案时屏蔽鼠标捕获。
##
## blocked 时释放鼠标并立即返回（Esc / 暂停菜单 / 触摸操作不应该被相机捕获）；
## 解除时按当前模式重新应用。
func set_ui_blocked(blocked: bool) -> void:
	_ui_blocked = blocked
	if blocked:
		if _client != null:
			_client.look_requires_mouse_capture = false
		_release_mouse()
		return
	_apply_mode()


func _process(_dt: float) -> void:
	if camera_mode != CameraMode.FOLLOW_BACKEND or _client == null or _entity == null:
		return

	var view = _entity.get_component(BevyEnums.Payload.CAMERA)
	if view is Dictionary:
		# Godot 的 rotation 是 YXZ 欧拉角，与 Rust 的 Quat::from_euler(YXZ, yaw, pitch, 0) 一致。
		rotation = Vector3(float(view.get("pitch", 0.0)), float(view.get("yaw", 0.0)), 0.0)

	if _entity.has_component(BevyEnums.Payload.TRANSFORM):
		position = _client.codec.sample_position(
			_entity, BevyEnums.Payload.TRANSFORM, _client.render_alpha
		)


func _unhandled_input(event: InputEvent) -> void:
	# Esc 由 UiRoot 统一处理（打开 / 关闭暂停菜单），这里不再消费 ui_cancel。
	if _ui_blocked:
		return
	# 触屏方案不捕获鼠标：视角走触摸拖拽，光标保持可见。
	if UiState.has_scheme(UiState.Scheme.TOUCH):
		return
	if camera_mode != CameraMode.FOLLOW_BACKEND or not capture_mouse:
		return
	if event is InputEventMouseButton and (event as InputEventMouseButton).pressed \
			and Input.mouse_mode != Input.MOUSE_MODE_CAPTURED:
		_capture_mouse()


func _unbind() -> void:
	if _client.entity_state_changed.is_connected(_on_entity_state_changed):
		_client.entity_state_changed.disconnect(_on_entity_state_changed)
	if _client.entity_removed.is_connected(_on_entity_removed):
		_client.entity_removed.disconnect(_on_entity_removed)
	_client = null
	_entity = null


func _apply_mode() -> void:
	var follow := camera_mode == CameraMode.FOLLOW_BACKEND
	var touch := UiState.has_scheme(UiState.Scheme.TOUCH)
	if _client != null:
		_client.look_requires_mouse_capture = follow and capture_mouse and not _ui_blocked and not touch
	if _ui_blocked or touch:
		_release_mouse()
		return
	if follow and capture_mouse:
		_capture_mouse()
	else:
		_release_mouse()


func _capture_mouse() -> void:
	Input.mouse_mode = Input.MOUSE_MODE_CAPTURED
	_captured_by_rig = true


func _release_mouse() -> void:
	# 只释放自己捕获的，不去动别处设置的鼠标模式。
	if _captured_by_rig:
		Input.mouse_mode = Input.MOUSE_MODE_VISIBLE
		_captured_by_rig = false


func _on_entity_state_changed(e: EntityData) -> void:
	if e.has_component(BevyEnums.Payload.CAMERA):
		_entity = e


func _on_entity_removed(e: EntityData, _reason: int) -> void:
	if e == _entity:
		_entity = null
