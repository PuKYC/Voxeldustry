extends Control
## 触屏控件层：左下虚拟摇杆 + 右侧视角区 + 右侧动作按钮。
##
## 仅在 TOUCH 方案显示（UiState.has_scheme(TOUCH)），菜单打开时隐藏并释放全部动作。
## 动作按钮不是写死的：通过 BevyClient.input_channels() 拉 game-core 的通道表，
## 跳过 mode=="axis2d"（移动由摇杆负责），对 button / hold / toggle 各生成一个按钮，
## 并用 BevyInputContext.channel_changed 订阅 present / available 做显隐与置灰。
##
## 由 UiRoot 实例化到 UiTree 下（res://ui/touch/touch_controls.tscn），
## 通过父链反查 UiRoot，不依赖固定节点路径；也支持外部 attach(ui_root)。

const TOUCH_JOYSTICK_SCRIPT := preload("res://ui/touch/touch_joystick.gd")

## 通道 key -> 英文标签（项目当前无 CJK 字体，UI 文案统一英文）。
const ACTION_LABELS := {
	"jump": "Jump",
	"glide": "Glide",
	"flight": "Fly",
	"interact": "Interact",
	"tool": "Tool",
	"sprint": "Sprint",
}

## toggle 模式点按后保持按下的时长（秒），之后 release 形成一次完整边沿。
const TOGGLE_RELEASE_DELAY := 0.1
## 右侧视角区占屏幕宽度的比例。
const LOOK_AREA_WIDTH_RATIO := 0.55
## 摇杆 / 按钮的最小设计尺寸。
const BUTTON_MIN_SIZE := 96.0
const EDGE_MARGIN := 32.0
const JOYSTICK_MARGIN := 40.0

## 视角灵敏度（乘在触摸位移上；曲线的其余部分在 game-core）。
@export var look_sensitivity: float = 1.0
## 摇杆底圈半径（设计单位）。
@export var joystick_radius: float = 140.0

var _ui_root = null
var _client = null
var _input_context = null
var _joystick = null
var _look_area: Control = null
var _buttons_grid: GridContainer = null
var _action_buttons: Dictionary[String, Button] = {}
var _button_actions: Dictionary[String, String] = {}
var _button_modes: Dictionary[String, String] = {}
var _pressed_actions: Dictionary[String, bool] = {}
var _look_touches: Dictionary[int, bool] = {}
var _buttons_signature: String = ""


func _ready() -> void:
	mouse_filter = Control.MOUSE_FILTER_IGNORE
	_build_joystick()
	_build_look_area()
	_build_buttons_container()
	# 极窄逻辑宽度下摇杆可能与右侧视角区相交：把摇杆提到最上层，左下角优先。
	move_child(_joystick, get_child_count() - 1)
	visibility_changed.connect(_on_visibility_changed)
	resized.connect(_layout_children)
	var root = _resolve_ui_root()
	if root != null:
		attach(root)
	else:
		_connect_client()
		_refresh_button_states()
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
	_connect_client()
	_ensure_action_buttons()
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


func _build_buttons_container() -> void:
	_buttons_grid = GridContainer.new()
	_buttons_grid.name = "ActionButtons"
	_buttons_grid.columns = 2
	_buttons_grid.mouse_filter = Control.MOUSE_FILTER_IGNORE
	_buttons_grid.add_theme_constant_override("h_separation", 12)
	_buttons_grid.add_theme_constant_override("v_separation", 12)
	add_child(_buttons_grid)


## 触屏手势区 / 按钮都不写坐标到场景文件：按当前逻辑尺寸排一次。
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
		var joystick_size := maxf(joystick_radius * 2.0 + 24.0, BUTTON_MIN_SIZE)
		_joystick.size = Vector2(joystick_size, joystick_size)
		_joystick.position = Vector2(
			JOYSTICK_MARGIN + inset_left,
			view.y - JOYSTICK_MARGIN - inset_bottom - joystick_size
		)
	if _look_area != null:
		var left := view.x * (1.0 - LOOK_AREA_WIDTH_RATIO)
		_look_area.position = Vector2(left, 0.0)
		_look_area.size = Vector2(maxf(0.0, view.x - left), view.y)
	if _buttons_grid != null:
		var grid_size := _buttons_grid.get_combined_minimum_size()
		_buttons_grid.size = grid_size
		_buttons_grid.position = Vector2(
			maxf(EDGE_MARGIN + inset_left, view.x - EDGE_MARGIN - inset_right - grid_size.x),
			maxf(EDGE_MARGIN + inset_top, view.y - EDGE_MARGIN - inset_bottom - grid_size.y)
		)


# ───────────────────────── UiRoot / 客户端 ─────────────────────────

func _resolve_ui_root():
	var node := get_parent()
	while node != null:
		if node.has_signal("scheme_changed") and node.has_signal("menu_changed"):
			return node
		node = node.get_parent()
	return null


func _connect_client() -> void:
	var client = _client_node()
	if client == _client and _input_context != null:
		return
	if _input_context != null and _input_context.has_signal("channel_changed"):
		if _input_context.channel_changed.is_connected(_on_channel_changed):
			_input_context.channel_changed.disconnect(_on_channel_changed)
	_client = client
	_input_context = null
	if client != null:
		var context = client.get("input_context")
		if context != null:
			_input_context = context
	if _input_context != null and _input_context.has_signal("channel_changed"):
		if not _input_context.channel_changed.is_connected(_on_channel_changed):
			_input_context.channel_changed.connect(_on_channel_changed)


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


func _channels() -> Array:
	if _input_context == null:
		return []
	if _input_context.has_method("channels"):
		return _input_context.channels()
	return []


# ───────────────────────── 动作按钮 ─────────────────────────

func _channels_signature() -> String:
	var parts := PackedStringArray()
	for row in _channels():
		if row is Dictionary:
			var channel: Dictionary = row
			parts.append(String(channel.get("key", "")) + ":" + String(channel.get("mode", "")))
	return "|".join(parts)


func _ensure_action_buttons() -> void:
	var signature := _channels_signature()
	if signature == _buttons_signature and not _action_buttons.is_empty():
		_refresh_button_states()
		return
	if signature.is_empty() and _buttons_signature.is_empty() and _action_buttons.is_empty():
		return
	_buttons_signature = signature
	_rebuild_action_buttons()


func _rebuild_action_buttons() -> void:
	for child in _buttons_grid.get_children():
		_buttons_grid.remove_child(child)
		child.free()
	_action_buttons.clear()
	_button_actions.clear()
	_button_modes.clear()
	for row in _channels():
		if not (row is Dictionary):
			continue
		var channel: Dictionary = row
		var mode := String(channel.get("mode", ""))
		if mode == "axis2d":
			continue
		var key := String(channel.get("key", ""))
		var actions: Array = channel.get("actions", [])
		if key.is_empty() or actions.is_empty():
			continue
		var action := String(actions[0])
		var button := _make_action_button(key, mode, action)
		_buttons_grid.add_child(button)
		_action_buttons[key] = button
		_button_actions[key] = action
		_button_modes[key] = mode
	_refresh_button_states()
	_layout_children.call_deferred()


func _make_action_button(key: String, mode: String, action: String) -> Button:
	var button := Button.new()
	button.name = "Action_" + key
	button.text = _label_for(key)
	button.custom_minimum_size = Vector2(BUTTON_MIN_SIZE, BUTTON_MIN_SIZE)
	button.focus_mode = Control.FOCUS_NONE
	button.mouse_filter = Control.MOUSE_FILTER_STOP
	button.add_theme_font_size_override("font_size", 22)
	button.button_down.connect(_on_action_button_down.bind(key, mode, action))
	button.button_up.connect(_on_action_button_up.bind(key, mode, action))
	return button


func _label_for(key: String) -> String:
	var label := String(ACTION_LABELS.get(key, ""))
	if not label.is_empty():
		return label
	return key.replace("_", " ").capitalize()


func _refresh_button_states() -> void:
	for key: String in _action_buttons.keys():
		var button: Button = _action_buttons[key]
		if button == null or not is_instance_valid(button):
			continue
		var present := true
		var available := true
		if _input_context != null:
			if _input_context.has_method("has_channel"):
				present = _input_context.has_channel(key)
			if _input_context.has_method("is_channel_available"):
				available = _input_context.is_channel_available(key)
		button.visible = present
		button.disabled = not available


func _on_channel_changed(key: String, present: bool, available: bool) -> void:
	_ensure_action_buttons()
	var button: Button = _action_buttons.get(key, null)
	if button == null or not is_instance_valid(button):
		return
	button.visible = present
	button.disabled = not available
	if not available:
		_release_action(String(_button_actions.get(key, "")))


func _on_action_button_down(key: String, mode: String, action: String) -> void:
	_press_action(action)
	# toggle：点按产生一次「按下 -> 约 0.1s 后松开」的完整边沿。
	if mode == "toggle":
		_schedule_action_release(action)


func _on_action_button_up(_key: String, mode: String, action: String) -> void:
	if mode == "toggle":
		return
	_release_action(action)


func _press_action(action: String) -> void:
	if action.is_empty() or not InputMap.has_action(action):
		return
	Input.action_press(action)
	_pressed_actions[action] = true


func _release_action(action: String) -> void:
	if action.is_empty():
		return
	if InputMap.has_action(action):
		Input.action_release(action)
	_pressed_actions.erase(action)


func _schedule_action_release(action: String) -> void:
	if not is_inside_tree():
		_release_action(action)
		return
	var timer := get_tree().create_timer(TOGGLE_RELEASE_DELAY)
	timer.timeout.connect(_release_action.bind(action), CONNECT_ONE_SHOT)


func _release_all() -> void:
	if _joystick != null and is_instance_valid(_joystick) and _joystick.has_method("reset"):
		_joystick.reset()
	_look_touches.clear()
	for action: String in _pressed_actions.keys():
		if InputMap.has_action(action):
			Input.action_release(action)
	_pressed_actions.clear()


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


func action_buttons() -> Dictionary:
	return _action_buttons.duplicate()
