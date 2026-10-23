extends Control
## 触屏控件层：虚拟摇杆 + 视角区（+ 浮动摇杆的触摸区）。
##
## 仅在 TOUCH 方案显示（UiState.has_scheme(TOUCH)），菜单打开时隐藏并释放手势状态。
## 动作按钮不在这一层：通道表 -> 按钮簇的映射在 ui/input（InputBindings /
## ActionCluster / ActionButtonHost），落 HUD 托盘槽位，安全区由 SafeArea 负责。
## 本层只保留摇杆 + 视角手势 + 自身触摸区域布局，并把矩形登记给 UiRoot 的避让层。
##
## 期 3 设置：
## - 摇杆尺寸按物理毫米（UiState.physical_units(26)）与下限 96 决定，可 @export 覆盖；
## - UiState.left_handed 把摇杆 / 浮动区放右侧、视角区放左 55%，安全区 insets 镜像；
## - UiState.joystick_mode == FLOATING 时用 JoystickZone（覆盖摇杆一侧半屏）按下处浮起，
##   抬手隐藏并复位；FIXED 时行为与原来一致。
##
## 由 UiRoot 实例化到 UiTree 下（res://ui/touch/touch_controls.tscn），
## 通过父链反查 UiRoot，不依赖固定节点路径；也支持外部 attach(ui_root)。

const TOUCH_JOYSTICK_SCRIPT := preload("res://ui/touch/touch_joystick.gd")

## 视角区占屏幕宽度的比例（右手：右侧 55%；左手镜像到左侧 55%）。
const LOOK_AREA_WIDTH_RATIO := 0.55
## 摇杆半径下限（设计单位）。
const JOYSTICK_MIN_RADIUS := 96.0
## 摇杆控件最小边长（兜底；实际尺寸 = 半径 * 2 + 24）。
const JOYSTICK_MIN_SIZE := 96.0
const JOYSTICK_MARGIN := 40.0
## 浮动摇杆触摸区占屏幕宽度的比例（覆盖摇杆一侧半屏）。
const FLOAT_ZONE_WIDTH_RATIO := 0.5

## 视角灵敏度（乘在触摸位移上；曲线的其余部分在 game-core）。
@export var look_sensitivity: float = 1.0
## 摇杆底圈半径（设计单位）。0 = 按 UiState.physical_units(26) 自动计算；> 0 时覆盖（测试用）。
@export var joystick_radius: float = 0.0

var _ui_root = null
var _client = null
var _joystick = null
var _look_area: Control = null
var _joystick_zone: Control = null
var _look_touches: Dictionary[int, bool] = {}
## 当前是否左手模式 / 浮动摇杆（来自 UiState，refresh_settings 时重读）。
var _left_handed: bool = false
var _floating_mode: bool = false
## 浮动摇杆是否已经激活（手指按下并显示摇杆）。
var _floating_active: bool = false
## 浮动摇杆当前接管的手指 index；-1 = 没有。
var _floating_touch_index: int = -1


func _ready() -> void:
	mouse_filter = Control.MOUSE_FILTER_IGNORE
	_left_handed = UiState.left_handed
	_floating_mode = UiState.joystick_mode == UiState.JoystickMode.FLOATING
	# 层次（从下到上）：浮动触摸区 -> 视角区 -> 摇杆。
	_build_joystick_zone()
	_build_look_area()
	_build_joystick()
	if _joystick != null:
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


## 期 3 设置变更（UiRoot._apply_ui_settings 调用）：重读左手 / 摇杆模式并重排。
func refresh_settings() -> void:
	_left_handed = UiState.left_handed
	var floating := UiState.joystick_mode == UiState.JoystickMode.FLOATING
	if floating != _floating_mode:
		_floating_mode = floating
		_deactivate_floating(false)
	if _joystick != null and is_instance_valid(_joystick):
		_joystick.radius = _effective_joystick_radius()
	refresh_layout()


# ───────────────────────── 构建 ─────────────────────────

func _build_joystick() -> void:
	_joystick = TOUCH_JOYSTICK_SCRIPT.new()
	_joystick.name = "VirtualJoystick"
	_joystick.radius = _effective_joystick_radius()
	add_child(_joystick)


## 浮动摇杆触摸区：覆盖摇杆一侧半屏，在最底层（视角区 / 摇杆之上不抢）。
func _build_joystick_zone() -> void:
	_joystick_zone = Control.new()
	_joystick_zone.name = "JoystickZone"
	_joystick_zone.mouse_filter = Control.MOUSE_FILTER_IGNORE
	_joystick_zone.gui_input.connect(_on_joystick_zone_gui_input)
	add_child(_joystick_zone)


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
	var inset_right := maxf(0.0, safe.z)
	var inset_bottom := maxf(0.0, safe.w)
	if _joystick != null:
		var radius := _effective_joystick_radius()
		_joystick.radius = radius
		var joystick_size := maxf(radius * 2.0 + 24.0, JOYSTICK_MIN_SIZE)
		_joystick.size = Vector2(joystick_size, joystick_size)
		if not _floating_active:
			var joystick_x := JOYSTICK_MARGIN + inset_left
			if _left_handed:
				joystick_x = view.x - JOYSTICK_MARGIN - inset_right - joystick_size
			_joystick.position = Vector2(
				joystick_x,
				view.y - JOYSTICK_MARGIN - inset_bottom - joystick_size
			)
		# FIXED：摇杆常驻；FLOATING：未激活时隐藏，按下处才出现。
		_joystick.visible = not _uses_floating_zone() or _floating_active
	if _joystick_zone != null:
		var zone_width := maxf(0.0, view.x * FLOAT_ZONE_WIDTH_RATIO)
		var zone_x := 0.0 if not _left_handed else view.x - zone_width
		_joystick_zone.position = Vector2(zone_x, 0.0)
		_joystick_zone.size = Vector2(zone_width, view.y)
		_joystick_zone.visible = _uses_floating_zone()
		_joystick_zone.mouse_filter = (
			Control.MOUSE_FILTER_STOP if _uses_floating_zone() else Control.MOUSE_FILTER_IGNORE
		)
	if _look_area != null:
		if _left_handed:
			_look_area.position = Vector2.ZERO
		else:
			_look_area.position = Vector2(view.x * (1.0 - LOOK_AREA_WIDTH_RATIO), 0.0)
		_look_area.size = Vector2(view.x * LOOK_AREA_WIDTH_RATIO, view.y)
	_update_touch_regions()


## 摇杆半径：@export 覆盖优先，否则按物理毫米（26mm）与下限 96 计算。
func _effective_joystick_radius() -> float:
	if joystick_radius > 0.0:
		return joystick_radius
	return maxf(UiState.physical_units(26.0), JOYSTICK_MIN_RADIUS)


func _uses_floating_zone() -> bool:
	return _floating_mode


# ───────────────────────── 避让登记 ─────────────────────────

## 把摇杆 / 视角区矩形登记给 UiRoot 的 TouchRegionRegistry（若存在）。
## 摇杆占用摇杆一侧的底部槽位（右手 bottom_begin / 左手 bottom_end）；视角区只登记不避让。
func _update_touch_regions() -> void:
	if _ui_root == null or not is_instance_valid(_ui_root):
		return
	if not _ui_root.has_method("update_touch_region"):
		return
	var can_release: bool = _ui_root.has_method("release_touch_region")
	if not is_visible_in_tree():
		if can_release:
			_ui_root.release_touch_region(&"joystick")
			_ui_root.release_touch_region(&"look_area")
		return
	if _joystick != null:
		if _uses_floating_zone() and not _floating_active:
			if can_release:
				_ui_root.release_touch_region(&"joystick")
		else:
			var joystick_rect: Rect2 = _joystick.get_global_rect()
			var margin := JOYSTICK_MARGIN
			var reserves := {}
			if _left_handed:
				reserves = {"bottom_end": Vector4(0.0, 0.0, joystick_rect.size.x + margin, 0.0)}
			else:
				reserves = {"bottom_begin": Vector4(joystick_rect.size.x + margin, 0.0, 0.0, 0.0)}
			_ui_root.update_touch_region(&"joystick", joystick_rect, reserves)
	if _look_area != null:
		_ui_root.update_touch_region(&"look_area", _look_area.get_global_rect(), {})


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


# ───────────────────────── 浮动摇杆 ─────────────────────────

func _on_joystick_zone_gui_input(event: InputEvent) -> void:
	if not _uses_floating_zone() or _joystick == null:
		return
	if event is InputEventScreenTouch:
		var touch := event as InputEventScreenTouch
		if touch.pressed:
			if _floating_touch_index != -1:
				return
			_floating_touch_index = touch.index
			_activate_floating(touch.position)
			accept_event()
		elif touch.index == _floating_touch_index:
			_deactivate_floating()
			accept_event()
	elif event is InputEventScreenDrag:
		var drag := event as InputEventScreenDrag
		if drag.index != _floating_touch_index:
			return
		_joystick.move_touch(drag.index, _zone_to_joystick(drag.position))
		accept_event()


## 触摸区坐标 -> 摇杆本地坐标（两者是同一个父级的兄弟节点）。
func _zone_to_joystick(zone_position: Vector2) -> Vector2:
	return _joystick_zone.position + zone_position - _joystick.position


## 手指按下：把摇杆放到按下处（夹在屏幕内）、显示并接管这根手指。
func _activate_floating(zone_position: Vector2) -> void:
	var center: Vector2 = _joystick_zone.position + zone_position
	var half: Vector2 = _joystick.size * 0.5
	center.x = clampf(center.x, half.x, maxf(half.x, size.x - half.x))
	center.y = clampf(center.y, half.y, maxf(half.y, size.y - half.y))
	_floating_active = true
	_joystick.position = center - half
	_joystick.visible = true
	_joystick.begin_touch(_floating_touch_index, half)
	_update_touch_regions()


## 抬手：复位并隐藏摇杆（FLOATING 未激活时不占位）。
func _deactivate_floating(restore_layout: bool = true) -> void:
	_floating_touch_index = -1
	if not _floating_active:
		# 仍然保证隐藏 / 复位（例如切换模式时）。
		if _joystick != null and is_instance_valid(_joystick):
			if _uses_floating_zone():
				_joystick.visible = false
		return
	_floating_active = false
	if _joystick != null and is_instance_valid(_joystick):
		if _joystick.has_method("reset"):
			_joystick.reset()
		_joystick.visible = false
	if restore_layout:
		_layout_children()
	else:
		_update_touch_regions()


# ───────────────────────── 方案 / 菜单 / 可见性 ─────────────────────────

func _on_scheme_changed(_current: int) -> void:
	_refresh_visibility()
	if not UiState.has_scheme(UiState.Scheme.TOUCH):
		_release_all()
	_update_touch_regions()


func _on_menu_changed(open: bool) -> void:
	if open:
		_release_all()
	_refresh_visibility()


func _on_visibility_changed() -> void:
	if not is_visible_in_tree():
		_release_all()
		_update_touch_regions()
		return
	_layout_children.call_deferred()


func _refresh_visibility() -> void:
	visible = UiState.has_scheme(UiState.Scheme.TOUCH) and not UiState.is_menu_open()


## 松开摇杆并清掉视角手势（动作按钮由 ActionButtonHost / ActionCluster 自己释放）。
func _release_all() -> void:
	if _joystick != null and is_instance_valid(_joystick) and _joystick.has_method("reset"):
		_joystick.reset()
	_look_touches.clear()
	if _uses_floating_zone():
		_deactivate_floating(false)
		if _joystick != null and is_instance_valid(_joystick):
			_joystick.visible = false


func _notification(what: int) -> void:
	if what == NOTIFICATION_WM_WINDOW_FOCUS_OUT:
		_release_all()
		_update_touch_regions()


func _exit_tree() -> void:
	_release_all()
	if _ui_root != null and is_instance_valid(_ui_root) and _ui_root.has_method("release_touch_region"):
		_ui_root.release_touch_region(&"joystick")
		_ui_root.release_touch_region(&"look_area")


# ───────────────────────── 测试 / 调试访问 ─────────────────────────

## UiRoot 在安全区 / 缩放 / 断点更新后调用：重新按新尺寸排一次触屏控件。
func refresh_layout() -> void:
	_layout_children()


func joystick():
	return _joystick


func look_area() -> Control:
	return _look_area


func joystick_zone() -> Control:
	return _joystick_zone


## 当前是否处于浮动摇杆激活（测试用）。
func is_floating_active() -> bool:
	return _floating_active
