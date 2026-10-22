extends Control
## 暂停菜单：全屏半透明遮罩 + 中央面板。
##
## 对外契约（UiRoot 依赖）：
## - signal resume_requested：Resume 按钮 / 外部调用；
## - open() / close() / is_open()；
## - signal ui_scale_changed(value: float)：UI 缩放滑条（0.8 .. 1.3）；
## - signal debug_overlay_requested(enabled: bool)：调试层开关（仅 debug 构建显示）；
## - signal debug_scheme_requested(scheme: int)：强制输入方案（仅 debug 构建显示）。
##
## Esc 由 UiRoot 统一处理（打开 / 关闭暂停菜单），这里不重复处理。
## 键盘 / 手柄可聚焦：打开时焦点落在 Resume，Settings 展开后焦点落在滑条。
## 移动端没有 F3：调试入口只在 debug 构建的暂停面板里出现（release 包不显示）。

signal resume_requested
signal ui_scale_changed(value: float)
signal debug_overlay_requested(enabled: bool)
signal debug_scheme_requested(scheme: int)

const SCALE_MIN := 0.8
const SCALE_MAX := 1.3
const SCALE_STEP := 0.05

## 仅测试 / 手工预览用：强制显示调试入口（默认跟随 OS.is_debug_build()）。
@export var force_show_debug: bool = false

@onready var _resume_button: Button = %ResumeButton
@onready var _settings_button: Button = %SettingsButton
@onready var _quit_button: Button = %QuitButton
@onready var _settings_panel: Control = %SettingsPanel
@onready var _back_button: Button = %BackButton
@onready var _scale_slider: HSlider = %ScaleSlider
@onready var _scale_value: Label = %ScaleValue
@onready var _hint: Label = %Hint
@onready var _debug_button: Button = %DebugButton
@onready var _debug_panel: Control = %DebugPanel
@onready var _debug_overlay_button: Button = %DebugOverlayButton
@onready var _force_touch_button: Button = %ForceTouchButton
@onready var _debug_back_button: Button = %DebugBackButton

## 调试层当前状态（与 UiRoot 同步，只影响按钮文案）。
var _overlay_enabled := false


func _ready() -> void:
	visible = false
	process_mode = Node.PROCESS_MODE_ALWAYS
	_settings_panel.visible = false
	_debug_panel.visible = false
	_debug_button.visible = is_debug_available()

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
	_debug_button.pressed.connect(_on_debug_pressed)
	_debug_back_button.pressed.connect(_on_debug_back_pressed)
	_debug_overlay_button.pressed.connect(_on_debug_overlay_pressed)
	_force_touch_button.pressed.connect(_on_force_touch_pressed)
	_refresh_debug_texts()


func open() -> void:
	visible = true
	_settings_panel.visible = false
	_debug_panel.visible = false
	_debug_button.visible = is_debug_available()
	_refresh_debug_texts()
	_hint.text = "Tap Resume" if UiState.has_scheme(UiState.Scheme.TOUCH) else "Esc to resume"
	_resume_button.call_deferred("grab_focus")


func close() -> void:
	visible = false
	_settings_panel.visible = false
	_debug_panel.visible = false
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


# ───────────────────────── 调试入口（仅 debug 构建） ─────────────────────────

## 调试入口是否可用：默认跟随构建类型，测试 / 手工预览可用 force_show_debug 强制打开。
func is_debug_available() -> bool:
	return OS.is_debug_build() or force_show_debug


## 外部（UiRoot）在每次打开菜单时同步调试层状态，避免按钮文案与实际不符。
func set_debug_overlay_state(enabled: bool) -> void:
	_overlay_enabled = enabled
	_refresh_debug_texts()


## 测试 / 运行期改 force_show_debug 后重算入口显隐。
func apply_debug_visibility() -> void:
	_debug_button.visible = is_debug_available()
	if not is_debug_available():
		_debug_panel.visible = false


func _on_debug_pressed() -> void:
	_settings_panel.visible = false
	_debug_panel.visible = true
	_debug_overlay_button.grab_focus()


func _on_debug_back_pressed() -> void:
	_debug_panel.visible = false
	if _debug_button.visible:
		_debug_button.grab_focus()
	else:
		_settings_button.grab_focus()


func _on_debug_overlay_pressed() -> void:
	_overlay_enabled = not _overlay_enabled
	_refresh_debug_texts()
	debug_overlay_requested.emit(_overlay_enabled)


## 强制方案：在 TOUCH / KBM 之间切，方便桌面预览触屏 UI 或手机接键盘时回到键鼠。
func _on_force_touch_pressed() -> void:
	var next := UiState.Scheme.KBM if UiState.has_scheme(UiState.Scheme.TOUCH) else UiState.Scheme.TOUCH
	debug_scheme_requested.emit(next)
	_refresh_debug_texts()


func _refresh_debug_texts() -> void:
	if _debug_overlay_button == null:
		return
	_debug_overlay_button.text = "Debug Overlay: %s" % ("ON" if _overlay_enabled else "OFF")
	_force_touch_button.text = "Force TOUCH: %s" % ("ON" if UiState.has_scheme(UiState.Scheme.TOUCH) else "OFF")
	_hint.text = "Tap Resume" if UiState.has_scheme(UiState.Scheme.TOUCH) else "Esc to resume"


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
