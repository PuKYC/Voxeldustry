class_name PauseButton
extends HudWidget
## 触屏方案的暂停入口。
##
## 手机 / 平板没有 Esc，也没有物理键盘：只靠 Android 返回键（WM_GO_BACK_REQUEST）
## 进菜单既不可发现、又不通用。这个按钮只在 TOUCH 方案显示（schemes = TOUCH），
## 键鼠仍走 Esc、手柄走 ui_cancel。
##
## 不写坐标：落 TOP / END 托盘，尺寸取 custom_minimum_size；自绘暂停图标（两条竖杠），
## 与 ActionButton 一样自己处理 ScreenTouch（一个按钮只跟一根手指）。

const BUTTON_SIZE := 72.0
const COLOR_BG := Color(0.06, 0.08, 0.11, 0.55)
const COLOR_ICON := Color(1.0, 1.0, 1.0, 0.88)


func _init() -> void:
	custom_minimum_size = Vector2(BUTTON_SIZE, BUTTON_SIZE)
	focus_mode = Control.FOCUS_NONE
	mouse_filter = Control.MOUSE_FILTER_STOP
	widget_id = &"pause_button"
	tray = Tray.TOP
	align = Align.END
	priority = 50
	schemes = UiState.Scheme.TOUCH


func _gui_input(event: InputEvent) -> void:
	if event is InputEventScreenTouch:
		var touch := event as InputEventScreenTouch
		if touch.pressed:
			accept_event()
			_trigger()
	elif event is InputEventMouseButton:
		var mouse := event as InputEventMouseButton
		if mouse.button_index == MOUSE_BUTTON_LEFT and mouse.pressed:
			accept_event()
			_trigger()


func _notification(what: int) -> void:
	if what == NOTIFICATION_RESIZED:
		queue_redraw()


## 只负责发起：菜单开关 / 暂停语义仍在 UiRoot。
func _trigger() -> void:
	if ui_root == null or not ui_root.has_method("open_pause_menu"):
		return
	ui_root.open_pause_menu()


func _draw() -> void:
	draw_rect(Rect2(Vector2.ZERO, size), COLOR_BG, true)
	var center := size * 0.5
	var half_height := minf(size.y * 0.26, 22.0)
	var bar_width := maxf(4.0, size.x * 0.09)
	var gap := bar_width * 1.4
	draw_rect(Rect2(center.x - bar_width - gap * 0.5, center.y - half_height, bar_width, half_height * 2.0), COLOR_ICON)
	draw_rect(Rect2(center.x + gap * 0.5, center.y - half_height, bar_width, half_height * 2.0), COLOR_ICON)
