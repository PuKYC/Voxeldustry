extends Control
## 虚拟摇杆（单指）。
##
## 只负责「触摸位置 -> 归一化方向 -> 四个移动动作的强度」：死区与死区后的线性
## 重映射在这里做，真正的曲线 / 量化 / 能力过滤在 game-core。
##
## 多指：只跟踪落在本控件上的第一根手指（按 event.index 匹配），后来的手指忽略；
## 手指抬起时按 index 匹配，避免多指时误判。
## 退出 / 隐藏 / 菜单打开 / 窗口失焦时由父级调用 reset() 归零并释放全部动作。

## 归一化方向（x 向右为正，y 向下为正；已应用死区与重映射，长度 0..1）。
signal vector_changed(direction: Vector2)

const MOVE_ACTIONS := ["move_forward", "move_back", "move_left", "move_right"]

## 底圈有效半径（设计单位）。
@export var radius: float = 140.0
## 死区（0..1）。
@export var dead_zone: float = 0.15
## 摇杆帽半径占底圈半径的比例。
@export var knob_ratio: float = 0.42

var _touch_index: int = -1
var _knob_offset: Vector2 = Vector2.ZERO
var _active: bool = false


func _ready() -> void:
	mouse_filter = Control.MOUSE_FILTER_STOP
	queue_redraw()


func _gui_input(event: InputEvent) -> void:
	if event is InputEventScreenTouch:
		var touch := event as InputEventScreenTouch
		if touch.pressed:
			# 已有手指按在摇杆上：忽略其它手指（不接受焦点）。
			if _touch_index != -1:
				return
			_touch_index = touch.index
			_active = true
			_update_from_position(touch.position)
			accept_event()
		elif touch.index == _touch_index:
			reset()
			accept_event()
	elif event is InputEventScreenDrag:
		var drag := event as InputEventScreenDrag
		if drag.index != _touch_index:
			return
		_update_from_position(drag.position)
		accept_event()


## 归零并释放全部移动动作（隐藏 / 菜单 / 失焦 / 退出时调用）。
func reset() -> void:
	_touch_index = -1
	_active = false
	_knob_offset = Vector2.ZERO
	_release_actions()
	vector_changed.emit(Vector2.ZERO)
	queue_redraw()


## 当前归一化方向；未触摸时为零。
func direction() -> Vector2:
	var travel := _travel_radius()
	if travel <= 0.0:
		return Vector2.ZERO
	return _shape_direction(_knob_offset / travel)


func _notification(what: int) -> void:
	if what == NOTIFICATION_VISIBILITY_CHANGED and not is_visible_in_tree():
		reset()
	elif what == NOTIFICATION_WM_WINDOW_FOCUS_OUT:
		reset()


func _draw() -> void:
	var center := size * 0.5
	var base := _base_radius()
	if base <= 1.0:
		return
	var knob_radius := base * clampf(knob_ratio, 0.2, 0.6)
	draw_circle(center, base, Color(1.0, 1.0, 1.0, 0.08))
	draw_arc(center, base, 0.0, TAU, 64, Color(1.0, 1.0, 1.0, 0.38), 3.0, true)
	var knob := center + _knob_offset
	draw_circle(knob, knob_radius, Color(1.0, 1.0, 1.0, 0.55 if _active else 0.30))
	draw_arc(knob, knob_radius, 0.0, TAU, 48, Color(1.0, 1.0, 1.0, 0.65), 2.5, true)


# ───────────────────────── 内部 ─────────────────────────

func _base_radius() -> float:
	return maxf(1.0, minf(radius, minf(size.x, size.y) * 0.5 - 8.0))


func _travel_radius() -> float:
	var base := _base_radius()
	var knob_radius := base * clampf(knob_ratio, 0.2, 0.6)
	return maxf(1.0, base - knob_radius)


func _update_from_position(local_position: Vector2) -> void:
	var center := size * 0.5
	var travel := _travel_radius()
	var offset := local_position - center
	if offset.length() > travel:
		offset = offset.normalized() * travel
	_knob_offset = offset
	_apply_direction(_shape_direction(offset / travel))
	queue_redraw()


## 死区 + 线性重映射：死区内归零，死区外从 0 平滑升到 1。
func _shape_direction(raw: Vector2) -> Vector2:
	var magnitude := clampf(raw.length(), 0.0, 1.0)
	if magnitude <= dead_zone:
		return Vector2.ZERO
	var scaled := (magnitude - dead_zone) / maxf(0.001, 1.0 - dead_zone)
	return raw.normalized() * clampf(scaled, 0.0, 1.0)


func _apply_direction(output: Vector2) -> void:
	_press_axis("move_forward", maxf(0.0, -output.y))
	_press_axis("move_back", maxf(0.0, output.y))
	_press_axis("move_left", maxf(0.0, -output.x))
	_press_axis("move_right", maxf(0.0, output.x))
	vector_changed.emit(output)


func _press_axis(action: String, strength: float) -> void:
	if not InputMap.has_action(action):
		return
	if strength > 0.001:
		Input.action_press(action, strength)
	else:
		Input.action_release(action)


func _release_actions() -> void:
	for action: String in MOVE_ACTIONS:
		if InputMap.has_action(action):
			Input.action_release(action)
