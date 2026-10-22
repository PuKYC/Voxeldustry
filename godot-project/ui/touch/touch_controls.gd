extends Control
## 触屏控件层：左下虚拟摇杆 + 右侧视角区。
##
## 仅在 TOUCH 方案显示（UiState.has_scheme(TOUCH)），菜单打开时隐藏并释放手势状态。
## 动作按钮不在这一层：通道表 -> 按钮的映射已移到 ui/input（ButtonSpecs /
## ActionButton / ActionButtonHost），按钮作为 HudWidget 放进托盘槽位，
## 安全区由 SafeArea 负责。本层只保留摇杆 + 视角手势 + 自身的触摸区域布局。
##
## 由 UiRoot 实例化到 UiTree 下（res://ui/touch/touch_controls.tscn），
## 通过父链反查 UiRoot，不依赖固定节点路径；也支持外部 attach(ui_root)。

const TOUCH_JOYSTICK_SCRIPT := preload("res://ui/touch/touch_joystick.gd")

## 右侧视角区占屏幕宽度的比例。
const LOOK_AREA_WIDTH_RATIO := 0.55
## 摇杆最小设计尺寸（半径很小时也不至于小到点不中）。
const JOYSTICK_MIN_SIZE := 96.0
const JOYSTICK_MARGIN := 40.0

## 视角灵敏度（乘在触摸位移上；曲线的其余部分在 game-core）。
@export var look_sensitivity: float = 1.0
## 摇杆底圈半径（设计单位）。
@export var joystick_radius: float = 140.0

var _ui_root = null
var _client = null
var _joystick = null
var _look_area: Control = null
var _look_touches: Dictionary[int, bool] = {}


func _ready() -> void:
	mouse_filter = Control.MOUSE_FILTER_IGNORE
	_build_joystick()
	_build_look_area()
	# 极窄逻辑宽度下摇杆可能与右侧视角区相交：把摇杆提到最上层，左下角优先。
	move_child(_joystick, get_child_count() - 1)
	visibility_changed.connect(_on_visibility_changed)
	resized.connect(_layout_children)
	var root = _resolve_ui_root()
	if root != null:
		attach(root)
	else:
		_refresh_visibility()
	_layout_children.call_deferred()


## 绑定 UiRoot（UiRoot 走父链自动解析；外部代码也可以显式调用）。
func attach(root: Node) -> void:
	if root == null:
		return
	_ui_root = root
	if _ui_root.has_signal("scheme_changed"):
		if not _ui_root.scheme_changed.is_connected(_on_scheme_changed):
			_ui_root.scheme_changed.connect(_on_scheme_changed)
	if _ui_root.has_signal("menu_changed"):
		if not _ui_root.menu_changed.is_connected(_on_menu_changed):
			_ui_root.menu_changed.connect(_on_menu_changed)
	_refresh_visibility()
	_layout_children.call_deferred()


# ───────────────────────── 构建 ─────────────────────────

func _build_joystick() -> void:
	_joystick = TOUCH_JOYSTICK_SCRIPT.new()
	_joystick.name = "VirtualJoystick"
	_joystick.radius = joystick_radius
	add_child(_joystick)


func _build_look_area() -> void:
	_look_area = Control.new()
	_look_area.name = "LookArea"
	_look_area.mouse_filter = Control.MOUSE_FILTER_STOP
	_look_area.gui_input.connect(_on_look_area_gui_input)
	add_child(_look_area)


## 触屏手势区不写坐标到场景文件：按当前逻辑尺寸 + 安全区排一次。
func _layout_children() -> void:
	if not is_inside_tree():
		return
	var view := size
	if view.x < 1.0 or view.y < 1.0:
		return
	# 安全区：touch 层在 SafeArea 容器之外，必须自己避开刘海 / 挖孔 / 手势条。
	var safe := UiState.safe_insets
	var inset_left := maxf(0.0, safe.x)
	var inset_top := maxf(0.0, safe.y)
	var inset_right := maxf(0.0, safe.z)
	var inset_bottom := maxf(0.0, safe.w)
	if _joystick != null:
		var joystick_size := maxf(joystick_radius * 2.0 + 24.0, JOYSTICK_MIN_SIZE)
		_joystick.size = Vector2(joystick_size, joystick_size)
		_joystick.position = Vector2(
			JOYSTICK_MARGIN + inset_left,
			view.y - JOYSTICK_MARGIN - inset_bottom - joystick_size
		)
	if _look_area != null:
		var left := view.x * (1.0 - LOOK_AREA_WIDTH_RATIO)
		_look_area.position = Vector2(left, 0.0)
		_look_area.size = Vector2(maxf(0.0, view.x - left), view.y)


# ───────────────────────── UiRoot / 客户端 ─────────────────────────

func _resolve_ui_root():
	var node := get_parent()
	while node != null:
		if node.has_signal("scheme_changed") and node.has_signal("menu_changed"):
			return node
		node = node.get_parent()
	return null


func _client_node():
	if _client != null and is_instance_valid(_client):
		return _client
	if _ui_root != null:
		var root_client = _ui_root.get("client")
		if root_client != null:
			return root_client
	if is_inside_tree():
		return get_tree().get_first_node_in_group("bevy_client")
	return null


# ───────────────────────── 视角区 ─────────────────────────

func _on_look_area_gui_input(event: InputEvent) -> void:
	if event is InputEventScreenTouch:
		var touch := event as InputEventScreenTouch
		if touch.pressed:
			_look_touches[touch.index] = true
		else:
			_look_touches.erase(touch.index)
		_look_area.accept_event()
	elif event is InputEventScreenDrag:
		var drag := event as InputEventScreenDrag
		# 只处理从视角区按下的那根手指（多指各自独立）。
		if _look_touches.has(drag.index):
			apply_look_delta(drag.relative)
			_look_area.accept_event()


## 把触摸位移交给 BevyInput 的 look 通道；client / input 可能为 null。
func apply_look_delta(relative: Vector2) -> void:
	if relative == Vector2.ZERO:
		return
	if not UiState.accepts_gameplay_input():
		return
	var client = _client_node()
	if client == null:
		return
	var input = client.get("input")
	if input == null:
		return
	var delta := relative * look_sensitivity
	if input.has_method("on_look_delta"):
		input.on_look_delta(delta)
	elif input.has_method("on_mouse_motion"):
		input.on_mouse_motion(delta)


# ───────────────────────── 方案 / 菜单 / 可见性 ─────────────────────────

func _on_scheme_changed(_current: int) -> void:
	_refresh_visibility()
	if not UiState.has_scheme(UiState.Scheme.TOUCH):
		_release_all()


func _on_menu_changed(open: bool) -> void:
	if open:
		_release_all()
	_refresh_visibility()


func _on_visibility_changed() -> void:
	if not is_visible_in_tree():
		_release_all()
		return
	_layout_children.call_deferred()


func _refresh_visibility() -> void:
	visible = UiState.has_scheme(UiState.Scheme.TOUCH) and not UiState.is_menu_open()


## 松开摇杆并清掉视角手势（动作按钮由 ActionButtonHost 自己释放）。
func _release_all() -> void:
	if _joystick != null and is_instance_valid(_joystick) and _joystick.has_method("reset"):
		_joystick.reset()
	_look_touches.clear()


func _notification(what: int) -> void:
	if what == NOTIFICATION_WM_WINDOW_FOCUS_OUT:
		_release_all()


func _exit_tree() -> void:
	_release_all()


# ───────────────────────── 测试 / 调试访问 ─────────────────────────

## UiRoot 在安全区 / 缩放 / 断点更新后调用：重新按新尺寸排一次触屏控件。
func refresh_layout() -> void:
	_layout_children()


func joystick():
	return _joystick


func look_area() -> Control:
	return _look_area
