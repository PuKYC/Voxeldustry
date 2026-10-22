class_name ActionButton
extends HudWidget
## 动作按钮：自绘圆角矩形 + Label，不依赖原生 Button。
##
## 为什么不用 Button：project.godot 里 pointing/emulate_mouse_from_touch=false，
## 触屏时原生 Button 收不到鼠标事件；这里直接处理 InputEventScreenTouch，
## 同时处理 InputEventMouseButton 以兼容桌面调试 / headless 测试。
##
## 语义：按下 = Input.action_press(action)，松开 = Input.action_release(action)。
## toggle 模式沿用现状：按下后 TOGGLE_RELEASE_DELAY 秒自动 release，形成一次完整边沿。
##
## 防粘键：隐藏 / 方案切走（由父级 release_all 兜底）/ 窗口失焦 / 退出树 /
## 注册表 detach 都会 release()。一个按钮只跟踪一根手指（按 event.index 匹配）。

## 按钮边长（设计单位）。触屏目标尺寸，不随分辨率写死坐标。
const BUTTON_SIZE := 96.0
## toggle 模式点按后保持按下的时长（秒），之后 release 形成一次完整边沿。
const TOGGLE_RELEASE_DELAY := 0.1
const FONT_SIZE := 22
const CORNER_RADIUS := 16
const BORDER_WIDTH := 2

const COLOR_BG := Color(0.08, 0.10, 0.14, 0.55)
const COLOR_BG_PRESSED := Color(0.24, 0.52, 0.86, 0.85)
const COLOR_BG_DISABLED := Color(0.10, 0.10, 0.10, 0.35)
const COLOR_BORDER := Color(1.0, 1.0, 1.0, 0.55)
const COLOR_BORDER_DISABLED := Color(1.0, 1.0, 1.0, 0.18)
const COLOR_TEXT_DISABLED := Color(1.0, 1.0, 1.0, 0.40)

## 通道 key（BevyInputContext 的通道名，例如 "tool"）。
var _channel_key := ""
## 要注入的 InputMap 动作名（例如 "primary_tool"）。
var _action := ""
## 通道模式：hold / toggle / button ...
var _mode := ""
## 通道状态：present 控制显隐，available 控制置灰 + 立即释放。
var _present := true
var _available := true
## 当前是否处于「按下」状态（toggle 自动释放后变回 false）。
var _pressed := false
## 当前跟踪的触摸手指 index；-1 = 没有。
var _touch_index := -1
## 每次 press 自增；toggle 定时器只释放自己那一次按下。
var _press_generation := 0
var _label: Label = null
var _label_text := ""
var _box: StyleBoxFlat = null


func _init() -> void:
	custom_minimum_size = Vector2(BUTTON_SIZE, BUTTON_SIZE)
	focus_mode = Control.FOCUS_NONE
	mouse_filter = Control.MOUSE_FILTER_STOP


func _ready() -> void:
	_ensure_style()
	_ensure_label()
	_apply_label()
	_refresh_visual()


## 应用一条归一化规格（ButtonSpecs.normalize_spec 的产物）。
## 注册表 / UiRoot 不实现方案覆盖逻辑，这里只把字段原样交给 HudWidget。
func configure(spec: Dictionary) -> void:
	if spec.is_empty():
		return
	var id := String(spec.get("id", ""))
	if not id.is_empty():
		widget_id = StringName(id)
		name = id
	_channel_key = String(spec.get("channel", ""))
	_action = String(spec.get("action", ""))
	_mode = String(spec.get("mode", ""))
	_label_text = String(spec.get("label", ""))
	tray = int(spec.get("tray", HudWidget.Tray.RIGHT)) as HudWidget.Tray
	align = int(spec.get("align", HudWidget.Align.END)) as HudWidget.Align
	priority = int(spec.get("priority", 0))
	schemes = int(spec.get("schemes", UiState.Scheme.TOUCH))
	var overrides: Variant = spec.get("scheme_overrides", {})
	if overrides is Dictionary:
		scheme_overrides = (overrides as Dictionary).duplicate()
	_apply_label()
	_refresh_visual()


func channel_key() -> String:
	return _channel_key


func action_name() -> String:
	return _action


func mode() -> String:
	return _mode


func is_holding() -> bool:
	return _pressed


## 通道状态变化：present -> 显隐；not available -> 置灰并立刻释放（不粘键）。
func set_channel_state(present: bool, available: bool) -> void:
	_present = present
	_available = available
	if not available:
		release()
	set_data_visible(present)
	_refresh_visual()


## 立刻松手并释放动作（隐藏 / 菜单 / 失焦 / 退出 / 方案切走 / detach 时调用）。
func release() -> void:
	_touch_index = -1
	if not _pressed:
		return
	_pressed = false
	_press_generation += 1
	_release_action()
	_refresh_visual()


# ───────────────────────── 触摸 / 鼠标 ─────────────────────────

func _gui_input(event: InputEvent) -> void:
	if event is InputEventScreenTouch:
		var touch := event as InputEventScreenTouch
		if touch.pressed:
			# 一个按钮只跟一根手指；已有手指按着时忽略后来的手指。
			if _touch_index != -1:
				return
			if not _available or not is_visible_in_tree():
				return
			_touch_index = touch.index
			_press()
			accept_event()
		elif touch.index == _touch_index:
			_touch_index = -1
			_lift()
			accept_event()
	elif event is InputEventMouseButton:
		var mouse := event as InputEventMouseButton
		if mouse.button_index != MOUSE_BUTTON_LEFT:
			return
		if mouse.pressed:
			if not _available or not is_visible_in_tree():
				return
			_press()
		else:
			_lift()
		accept_event()


func _press() -> void:
	if _pressed:
		return
	_pressed = true
	_press_generation += 1
	if InputMap.has_action(_action):
		Input.action_press(_action)
	if _mode == "toggle":
		_schedule_toggle_release(_press_generation)
	_refresh_visual()


## 手指 / 鼠标抬起：hold 模式立刻释放；toggle 模式等 0.1s 的自动边沿。
func _lift() -> void:
	if _mode == "toggle":
		return
	release()


func _schedule_toggle_release(generation: int) -> void:
	if not is_inside_tree():
		release()
		return
	var timer := get_tree().create_timer(TOGGLE_RELEASE_DELAY)
	timer.timeout.connect(_finish_toggle.bind(generation), CONNECT_ONE_SHOT)


## 只释放自己那一次按下，避免把之后的新按下误释放。
func _finish_toggle(generation: int) -> void:
	if generation != _press_generation:
		return
	release()


func _release_action() -> void:
	if _action.is_empty():
		return
	if InputMap.has_action(_action):
		Input.action_release(_action)


# ───────────────────────── 生命周期 / 防粘键 ─────────────────────────

func _on_scheme_changed(current: int) -> void:
	# 方案切走（不含本元素的 schemes）时立刻释放；隐藏也会触发 visibility 通知。
	if (schemes & current) == 0:
		release()


func _on_detach() -> void:
	release()


func _notification(what: int) -> void:
	if what == NOTIFICATION_VISIBILITY_CHANGED and not is_visible_in_tree():
		release()
	elif what == NOTIFICATION_WM_WINDOW_FOCUS_OUT:
		release()


func _exit_tree() -> void:
	release()


# ───────────────────────── 自绘 ─────────────────────────

func _ensure_style() -> void:
	if _box != null:
		return
	_box = StyleBoxFlat.new()
	_box.set_corner_radius_all(CORNER_RADIUS)
	_box.set_border_width_all(BORDER_WIDTH)


func _ensure_label() -> void:
	if _label != null and is_instance_valid(_label):
		return
	_label = get_node_or_null("Label") as Label
	if _label != null:
		return
	_label = Label.new()
	_label.name = "Label"
	_label.mouse_filter = Control.MOUSE_FILTER_IGNORE
	_label.set_anchors_and_offsets_preset(Control.PRESET_FULL_RECT)
	_label.horizontal_alignment = HORIZONTAL_ALIGNMENT_CENTER
	_label.vertical_alignment = VERTICAL_ALIGNMENT_CENTER
	_label.add_theme_font_size_override("font_size", FONT_SIZE)
	_label.add_theme_color_override("font_outline_color", Color(0.0, 0.0, 0.0, 0.9))
	_label.add_theme_constant_override("outline_size", 4)
	add_child(_label)


func _apply_label() -> void:
	if _label == null:
		return
	_label.text = _label_text


func _refresh_visual() -> void:
	if _box != null:
		_box.bg_color = COLOR_BG_DISABLED if not _available else (COLOR_BG_PRESSED if _pressed else COLOR_BG)
		_box.border_color = COLOR_BORDER_DISABLED if not _available else COLOR_BORDER
	if _label != null:
		_label.modulate = COLOR_TEXT_DISABLED if not _available else Color.WHITE
	queue_redraw()


func _draw() -> void:
	if _box == null:
		return
	draw_style_box(_box, Rect2(Vector2.ZERO, size))

