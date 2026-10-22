class_name TouchSlider
extends HSlider
## 触屏可拖动的滑条。
##
## 背景（实测）：Godot 4.7 的 HSlider 只处理鼠标事件；project.godot 里
## pointing/emulate_mouse_from_touch=false 时触摸不会合成鼠标事件，所以手机上
## 原生 HSlider 按不动、拖不动——设置面板会变成只读。
##
## 做法：不覆写 _gui_input（内置控件的 C++ 输入处理不受 GDScript 覆写影响），
## 而是监听 gui_input 信号，自己处理 InputEventScreenTouch / ScreenDrag，
## 按触摸 x 位置换算成 value。鼠标 / 键盘 / 手柄行为完全不变。

## 当前跟踪的触摸 index；-1 = 没有。
var _touch_index: int = -1
## 两端死区（设计单位）：手指落在滑条最边缘也能推到极值。
const EDGE_INSET := 8.0


func _ready() -> void:
	gui_input.connect(_on_gui_input)


func _on_gui_input(event: InputEvent) -> void:
	if event is InputEventScreenTouch:
		var touch := event as InputEventScreenTouch
		if touch.pressed:
			_touch_index = touch.index
			_apply_touch_position(touch.position.x)
			accept_event()
		elif touch.index == _touch_index:
			_touch_index = -1
			accept_event()
	elif event is InputEventScreenDrag:
		var drag := event as InputEventScreenDrag
		if drag.index != _touch_index:
			return
		_apply_touch_position(drag.position.x)
		accept_event()


func _notification(what: int) -> void:
	if what == NOTIFICATION_WM_WINDOW_FOCUS_OUT:
		_touch_index = -1
	elif what == NOTIFICATION_VISIBILITY_CHANGED and not is_visible_in_tree():
		_touch_index = -1


## 触摸 x -> value：写 value 会按 step 取整并发 value_changed，与鼠标拖动一致。
func _apply_touch_position(x: float) -> void:
	var span := maxf(1.0, size.x - EDGE_INSET * 2.0)
	var ratio := clampf((x - EDGE_INSET) / span, 0.0, 1.0)
	value = min_value + ratio * (max_value - min_value)
