extends Control
## 暂停菜单：全屏半透明遮罩 + 中央面板。
##
## 对外契约（UiRoot 依赖）：
## - signal resume_requested：Resume 按钮 / 外部调用；
## - open() / close() / is_open()；
## - signal ui_scale_changed(value: float)：UI 缩放滑条（0.8 .. 1.3）。
##
## Esc 由 UiRoot 统一处理（打开 / 关闭暂停菜单），这里不重复处理。
## 键盘 / 手柄可聚焦：打开时焦点落在 Resume，Settings 展开后焦点落在滑条。

signal resume_requested
signal ui_scale_changed(value: float)

const SCALE_MIN := 0.8
const SCALE_MAX := 1.3
const SCALE_STEP := 0.05

@onready var _resume_button: Button = %ResumeButton
@onready var _settings_button: Button = %SettingsButton
@onready var _quit_button: Button = %QuitButton
@onready var _settings_panel: Control = %SettingsPanel
@onready var _back_button: Button = %BackButton
@onready var _scale_slider: HSlider = %ScaleSlider
@onready var _scale_value: Label = %ScaleValue


func _ready() -> void:
	visible = false
	process_mode = Node.PROCESS_MODE_ALWAYS
	_settings_panel.visible = false

	# 先写初值再连信号：初始化过程不应对外发 ui_scale_changed。
	_scale_slider.min_value = SCALE_MIN
	_scale_slider.max_value = SCALE_MAX
	_scale_slider.step = SCALE_STEP
	_scale_slider.value = 1.0
	_update_scale_label(_scale_slider.value)

	_resume_button.pressed.connect(_on_resume_pressed)
	_settings_button.pressed.connect(_on_settings_pressed)
	_quit_button.pressed.connect(_on_quit_pressed)
	_back_button.pressed.connect(_on_back_pressed)
	_scale_slider.value_changed.connect(_on_scale_value_changed)


func open() -> void:
	visible = true
	_settings_panel.visible = false
	_resume_button.call_deferred("grab_focus")


func close() -> void:
	visible = false
	_settings_panel.visible = false
	if _resume_button.has_focus():
		_resume_button.release_focus()


func is_open() -> bool:
	return visible


## 外部同步滑条数值（不触发 ui_scale_changed 回环）。
func set_scale_value(value: float) -> void:
	var clamped := clampf(value, SCALE_MIN, SCALE_MAX)
	if is_equal_approx(_scale_slider.value, clamped):
		return
	_scale_slider.set_value_no_signal(clamped)
	_update_scale_label(clamped)


# ───────────────────────── 按钮回调 ─────────────────────────

func _on_resume_pressed() -> void:
	resume_requested.emit()


func _on_settings_pressed() -> void:
	_settings_panel.visible = true
	_scale_slider.grab_focus()


func _on_back_pressed() -> void:
	_settings_panel.visible = false
	_settings_button.grab_focus()


func _on_quit_pressed() -> void:
	get_tree().quit()


func _on_scale_value_changed(value: float) -> void:
	_update_scale_label(value)
	ui_scale_changed.emit(value)


func _update_scale_label(value: float) -> void:
	_scale_value.text = "UI Scale: %.2f" % value
