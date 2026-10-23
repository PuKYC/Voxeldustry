class_name ActionButton
extends Control
## 动作按钮：自绘圆角矩形 + Label，不依赖原生 Button。
##
## 由 ActionCluster 统一创建、排布、显隐与回收（按钮不再继承 HudWidget、不再
## 注册进 HudRegistry —— 一个动作簇只占一个 HUD 槽位）。本类只负责：
## 1) 一根手指 = 一次按下：按 InputEventScreenTouch.index 匹配，多指各自独立；
## 2) 按下注入 Input.action_press(action)、抬起 Input.action_release(action)；
## 3) 释放时机可控：不可用 / 隐藏 / 失焦 / 退出树都会立即 release（防粘键）；
## 4) toggle 模式保留 0.1s 自动释放，形成一次完整边沿。
##
## 尺寸 / 透明度由簇通过 apply_size() / set_opacity() 下发；available=false 时
## 置灰并立即释放，但控件矩形不变（避免布局跳动）。

## toggle 模式点按后保持按下的时长（秒），之后 release 形成一次完整边沿。
const TOGGLE_RELEASE_DELAY := 0.1
const FONT_RATIO := 0.22
const FONT_MIN := 12
const FONT_MAX := 40
const MIN_SIZE := 44.0
const CORNER_RADIUS := 16
const BORDER_WIDTH := 2

const COLOR_BG := Color(0.08, 0.10, 0.14, 0.55)
const COLOR_BG_PRESSED := Color(0.24, 0.52, 0.86, 0.85)
const COLOR_BG_DISABLED := Color(0.10, 0.10, 0.10, 0.35)
const COLOR_BORDER := Color(1.0, 1.0, 1.0, 0.55)
const COLOR_BORDER_DISABLED := Color(1.0, 1.0, 1.0, 0.18)
const COLOR_TEXT_DISABLED := Color(1.0, 1.0, 1.0, 0.40)

## 稳定 id（"btn_" + role），测试 / 调试友好。
var widget_id: StringName = &""
## 要注入的 InputMap 动作名（例如 "attack"）。
var _action := ""
## 通道模式：hold / toggle / button ...
var _mode := ""
## 当前通道是否 present（显隐由簇决定，这里只用于隐藏时 release）。
var _present := true
## 通道状态：available 控制置灰 + 立即释放。
var _available := true
## 当前是否处于「按下」状态（toggle 自动释放后变回 false）。
var _pressed := false
## 当前跟踪的触摸手指 index；-1 = 没有。
var _touch_index := -1
## 每次 press 自增；toggle 定时器只释放自己那一次按下。
var _press_generation := 0
## 当前边长（像素）与不透明度。
var _size_px := 0.0
var _opacity := 0.75

var _label: Label = null
var _label_text := ""
var _box: StyleBoxFlat = null


func _init() -> void:
	focus_mode = Control.FOCUS_NONE
	mouse_filter = Control.MOUSE_FILTER_STOP
	custom_minimum_size = Vector2(MIN_SIZE, MIN_SIZE)


func _ready() -> void:
	_ensure_style()
	_ensure_label()
	_apply_label()
	_refresh_visual()


# ───────────────────────── 配置（簇 / 测试共用） ─────────────────────────

## 兼容旧测试入口：字典里取 id / label / action / mode，其余字段忽略。
func configure(spec: Dictionary) -> void:
	if spec.is_empty():
		return
	var id := String(spec.get("id", ""))
	if not id.is_empty():
		widget_id = StringName(id)
		name = id
	set_action(String(spec.get("action", "")))
	set_label(String(spec.get("label", "")))
	set_mode(String(spec.get("mode", "")))


func set_action(action: String) -> void:
	if _action == action:
		return
	if _pressed:
		release()
	_action = action


func action_name() -> String:
	return _action


func set_label(text: String) -> void:
	_label_text = text
	_apply_label()


func label_text() -> String:
	return _label_text


## 兼容别名：与 Label 语义一致。
func text() -> String:
	return _label_text


func set_mode(mode: String) -> void:
	_mode = mode


func mode() -> String:
	return _mode


## 通道状态变化：present=false 隐藏；available=false 置灰并立刻释放（不粘键）。
func set_channel_state(present: bool, available: bool) -> void:
	_present = present
	_available = available
	if not present or not available:
		release()
	visible = present
	_refresh_visual()


func set_available(value: bool) -> void:
	_available = value
	if not value:
		release()
	_refresh_visual()


func available() -> bool:
	return _available


func is_available() -> bool:
	return _available


func is_holding() -> bool:
	return _pressed


func size_px() -> float:
	return _size_px


## 应用按钮边长（像素）：同步控件最小尺寸 / 实际尺寸 / 字号。
func apply_size(px: float) -> void:
	var side := maxf(px, MIN_SIZE)
	_size_px = side
	custom_minimum_size = Vector2(side, side)
	size = custom_minimum_size
	if _label != null:
		var font_size := int(clampf(side * FONT_RATIO, float(FONT_MIN), float(FONT_MAX)))
		_label.add_theme_font_size_override("font_size", font_size)
	queue_redraw()


## 不透明度：空闲 alpha = opacity，按下时变亮（alpha 更高）。
func set_opacity(value: float) -> void:
	_opacity = clampf(value, 0.05, 1.0)
	_refresh_visual()


func opacity() -> float:
	return _opacity


## 立刻松手并释放动作（隐藏 / 菜单 / 失焦 / 退出 / 方案切走 / 回收时调用）。
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
	if _pressed or not _present or not _available:
		return
	_pressed = true
	_press_generation += 1
	if not _action.is_empty() and InputMap.has_action(_action):
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
	_label = Label.new()
	_label.name = "Label"
	_label.mouse_filter = Control.MOUSE_FILTER_IGNORE
	_label.set_anchors_and_offsets_preset(Control.PRESET_FULL_RECT)
	_label.horizontal_alignment = HORIZONTAL_ALIGNMENT_CENTER
	_label.vertical_alignment = VERTICAL_ALIGNMENT_CENTER
	_label.add_theme_font_size_override("font_size", int(FONT_MIN))
	_label.add_theme_color_override("font_outline_color", Color(0.0, 0.0, 0.0, 0.9))
	_label.add_theme_constant_override("outline_size", 4)
	add_child(_label)


func _apply_label() -> void:
	_ensure_label()
	if _label != null:
		_label.text = _label_text


func _refresh_visual() -> void:
	if _box != null:
		_box.bg_color = COLOR_BG_DISABLED if not _available else (COLOR_BG_PRESSED if _pressed else COLOR_BG)
		_box.border_color = COLOR_BORDER_DISABLED if not _available else COLOR_BORDER
	if _label != null:
		_label.modulate = COLOR_TEXT_DISABLED if not _available else Color.WHITE
	var alpha := _opacity
	if _pressed:
		alpha = minf(1.0, _opacity + 0.2)
	modulate = Color(1.0, 1.0, 1.0, alpha)
	queue_redraw()


func _draw() -> void:
	if _box == null:
		return
	draw_style_box(_box, Rect2(Vector2.ZERO, size))
