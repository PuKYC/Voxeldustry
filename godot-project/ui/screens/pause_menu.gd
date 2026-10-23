extends Control
## 暂停菜单：全屏半透明遮罩 + 中央面板。
##
## 对外契约（UiRoot 依赖）：
## - signal resume_requested：Resume 按钮 / 外部调用；
## - open() / close() / is_open()；
## - signal ui_scale_changed(value: float)：UI 缩放滑条（0.8 .. 1.3）；
## - signal debug_overlay_requested(enabled: bool)：调试层开关（仅 debug 构建显示）；
## - signal debug_scheme_requested(scheme: int)：强制输入方案（仅 debug 构建显示）；
## - 期 3 设置区信号（UiRoot 负责写 UiState + 落盘 + 立即应用）：
##   action_layout_changed(value: int) / button_scale_changed(value: float) /
##   button_opacity_changed(value: float) / left_handed_changed(value: bool) /
##   joystick_mode_changed(value: int)；
## - refresh_settings_controls()：从 UiState 同步设置控件初值（不触发信号回环）。
##
## 设置区控件在 _ready 里代码创建后插入 %SettingsPanel 的 Back 之前，
## 不写坐标、不判断设备；数值初值来自 UiState，交互只发信号。
##
## Esc 由 UiRoot 统一处理（打开 / 关闭暂停菜单），这里不重复处理。
## 键盘 / 手柄可聚焦：打开时焦点落在 Resume，Settings 展开后焦点落在滑条。
## 移动端没有 F3：调试入口只在 debug 构建的暂停面板里出现（release 包不显示）。

signal resume_requested
signal ui_scale_changed(value: float)
signal debug_overlay_requested(enabled: bool)
signal debug_scheme_requested(scheme: int)
## 期 3 设置区信号。
signal action_layout_changed(value: int)
signal button_scale_changed(value: float)
signal button_opacity_changed(value: float)
signal left_handed_changed(value: bool)
signal joystick_mode_changed(value: int)

const SCALE_MIN := 0.8
const SCALE_MAX := 1.3
const SCALE_STEP := 0.05
const TOUCH_SLIDER_SCRIPT := preload("res://ui/widgets/touch_slider.gd")
const SETTINGS_BUTTON_MIN_SIZE := Vector2(380, 50)
const SETTINGS_SLIDER_MIN_SIZE := Vector2(380, 64)

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

## 期 3 设置控件（代码创建；唯一名不在场景里，用访问器给测试）。
var _action_layout_button: Button = null
var _button_scale_slider: HSlider = null
var _button_scale_value: Label = null
var _button_opacity_slider: HSlider = null
var _button_opacity_value: Label = null
var _left_handed_button: Button = null
var _joystick_mode_button: Button = null
## 设置区本地镜像：refresh 时从 UiState 同步，交互时先改镜像再发信号。
var _settings_action_layout: int = UiState.ActionLayout.CONTEXT
var _settings_button_scale: float = UiState.DEFAULT_BUTTON_SCALE
var _settings_button_opacity: float = UiState.DEFAULT_BUTTON_OPACITY
var _settings_left_handed: bool = false
var _settings_joystick_mode: int = UiState.JoystickMode.FIXED


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
	# 期 3 设置区：先建控件，再同步初值（同步不触发信号）。
	_build_settings_controls()
	refresh_settings_controls()


func open() -> void:
	visible = true
	_settings_panel.visible = false
	_debug_panel.visible = false
	_debug_button.visible = is_debug_available()
	_refresh_debug_texts()
	refresh_settings_controls()
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


# ───────────────────────── 期 3 设置区 ─────────────────────────

## 从 UiState 同步设置控件初值（不触发信号回环）。测试 / UiRoot 打开菜单时调用。
func refresh_settings_controls() -> void:
	_settings_action_layout = clampi(
		UiState.action_layout, UiState.ActionLayout.CONTEXT, UiState.ActionLayout.SPLIT
	)
	_settings_button_scale = clampf(
		UiState.button_scale, UiState.BUTTON_SCALE_MIN, UiState.BUTTON_SCALE_MAX
	)
	_settings_button_opacity = clampf(
		UiState.button_opacity, UiState.BUTTON_OPACITY_MIN, UiState.BUTTON_OPACITY_MAX
	)
	_settings_left_handed = UiState.left_handed
	_settings_joystick_mode = clampi(
		UiState.joystick_mode, UiState.JoystickMode.FIXED, UiState.JoystickMode.FLOATING
	)
	if _button_scale_slider != null:
		_button_scale_slider.set_value_no_signal(_settings_button_scale)
	if _button_opacity_slider != null:
		_button_opacity_slider.set_value_no_signal(_settings_button_opacity)
	_refresh_settings_texts()


func _build_settings_controls() -> void:
	if _settings_panel == null or _back_button == null:
		return
	_insert_settings_node(_make_settings_title("ActionLayoutTitle", "ACTION LAYOUT"))
	_action_layout_button = _make_settings_button("ActionLayoutButton", "Layout: Context")
	_action_layout_button.pressed.connect(_on_action_layout_pressed)
	_insert_settings_node(_action_layout_button)

	_insert_settings_node(_make_settings_title("ButtonScaleTitle", "BUTTON SIZE"))
	_button_scale_slider = _make_settings_slider(
		"ButtonScaleSlider",
		UiState.BUTTON_SCALE_MIN,
		UiState.BUTTON_SCALE_MAX,
		UiState.DEFAULT_BUTTON_SCALE
	)
	_insert_settings_node(_button_scale_slider)
	_button_scale_value = _make_settings_value("ButtonScaleValue", "Button Size: 1.00")
	_insert_settings_node(_button_scale_value)

	_insert_settings_node(_make_settings_title("ButtonOpacityTitle", "BUTTON OPACITY"))
	_button_opacity_slider = _make_settings_slider(
		"ButtonOpacitySlider",
		UiState.BUTTON_OPACITY_MIN,
		UiState.BUTTON_OPACITY_MAX,
		UiState.DEFAULT_BUTTON_OPACITY
	)
	_insert_settings_node(_button_opacity_slider)
	_button_opacity_value = _make_settings_value("ButtonOpacityValue", "Button Opacity: 0.75")
	_insert_settings_node(_button_opacity_value)

	_left_handed_button = _make_settings_button("LeftHandedButton", "Left Handed: OFF")
	_left_handed_button.pressed.connect(_on_left_handed_pressed)
	_insert_settings_node(_left_handed_button)

	_joystick_mode_button = _make_settings_button("JoystickModeButton", "Joystick: FIXED")
	_joystick_mode_button.pressed.connect(_on_joystick_mode_pressed)
	_insert_settings_node(_joystick_mode_button)

	# 交互 -> 信号（不在这里写 UiState：UiRoot 统一写 + 落盘）。
	_button_scale_slider.value_changed.connect(_on_button_scale_value_changed)
	_button_opacity_slider.value_changed.connect(_on_button_opacity_value_changed)


## 插到 %SettingsPanel 的 Back 之前（Back 永远在设置区最底部）。
func _insert_settings_node(node: Node) -> void:
	if node == null or _settings_panel == null or _back_button == null:
		return
	_settings_panel.add_child(node)
	var back_index := _back_button.get_index()
	if back_index >= 0:
		_settings_panel.move_child(node, back_index)


func _make_settings_title(node_name: String, text: String) -> Label:
	var label := Label.new()
	label.name = node_name
	label.text = text
	label.horizontal_alignment = HORIZONTAL_ALIGNMENT_CENTER
	label.add_theme_font_size_override("font_size", 20)
	return label


func _make_settings_button(node_name: String, text: String) -> Button:
	var button := Button.new()
	button.name = node_name
	button.text = text
	button.custom_minimum_size = SETTINGS_BUTTON_MIN_SIZE
	return button


func _make_settings_slider(node_name: String, min_value: float, max_value: float, value: float) -> HSlider:
	var slider: HSlider = TOUCH_SLIDER_SCRIPT.new()
	slider.name = node_name
	slider.custom_minimum_size = SETTINGS_SLIDER_MIN_SIZE
	slider.min_value = min_value
	slider.max_value = max_value
	slider.step = 0.05
	slider.set_value_no_signal(value)
	return slider


func _make_settings_value(node_name: String, text: String) -> Label:
	var label := Label.new()
	label.name = node_name
	label.text = text
	label.horizontal_alignment = HORIZONTAL_ALIGNMENT_CENTER
	label.add_theme_font_size_override("font_size", 18)
	return label


func _refresh_settings_texts() -> void:
	if _action_layout_button != null:
		var layout_text := "Split" if _settings_action_layout == UiState.ActionLayout.SPLIT else "Context"
		_action_layout_button.text = "Layout: %s" % layout_text
	if _button_scale_value != null:
		_button_scale_value.text = "Button Size: %.2f" % _settings_button_scale
	if _button_opacity_value != null:
		_button_opacity_value.text = "Button Opacity: %.2f" % _settings_button_opacity
	if _left_handed_button != null:
		_left_handed_button.text = "Left Handed: %s" % ("ON" if _settings_left_handed else "OFF")
	if _joystick_mode_button != null:
		var mode_text := (
			"FLOATING" if _settings_joystick_mode == UiState.JoystickMode.FLOATING else "FIXED"
		)
		_joystick_mode_button.text = "Joystick: %s" % mode_text


func _on_action_layout_pressed() -> void:
	var next := UiState.ActionLayout.CONTEXT
	if _settings_action_layout == UiState.ActionLayout.CONTEXT:
		next = UiState.ActionLayout.SPLIT
	_settings_action_layout = next
	_refresh_settings_texts()
	action_layout_changed.emit(next)


func _on_button_scale_value_changed(value: float) -> void:
	_settings_button_scale = clampf(value, UiState.BUTTON_SCALE_MIN, UiState.BUTTON_SCALE_MAX)
	_refresh_settings_texts()
	button_scale_changed.emit(_settings_button_scale)


func _on_button_opacity_value_changed(value: float) -> void:
	_settings_button_opacity = clampf(value, UiState.BUTTON_OPACITY_MIN, UiState.BUTTON_OPACITY_MAX)
	_refresh_settings_texts()
	button_opacity_changed.emit(_settings_button_opacity)


func _on_left_handed_pressed() -> void:
	_settings_left_handed = not _settings_left_handed
	_refresh_settings_texts()
	left_handed_changed.emit(_settings_left_handed)


func _on_joystick_mode_pressed() -> void:
	var next := UiState.JoystickMode.FIXED
	if _settings_joystick_mode == UiState.JoystickMode.FIXED:
		next = UiState.JoystickMode.FLOATING
	_settings_joystick_mode = next
	_refresh_settings_texts()
	joystick_mode_changed.emit(next)


# ── 设置控件访问器（测试 / UiRoot 用，避免依赖运行期唯一名） ──

func action_layout_button() -> Button:
	return _action_layout_button


func button_scale_slider() -> HSlider:
	return _button_scale_slider


func button_scale_value() -> Label:
	return _button_scale_value


func button_opacity_slider() -> HSlider:
	return _button_opacity_slider


func button_opacity_value() -> Label:
	return _button_opacity_value


func left_handed_button() -> Button:
	return _left_handed_button


func joystick_mode_button() -> Button:
	return _joystick_mode_button


## name -> 控件，供测试按名字取（也方便文档 / 冒烟断言）。
func settings_controls() -> Dictionary:
	return {
		"ActionLayoutButton": _action_layout_button,
		"ButtonScaleSlider": _button_scale_slider,
		"ButtonScaleValue": _button_scale_value,
		"ButtonOpacitySlider": _button_opacity_slider,
		"ButtonOpacityValue": _button_opacity_value,
		"LeftHandedButton": _left_handed_button,
		"JoystickModeButton": _joystick_mode_button,
	}


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
	refresh_settings_controls()
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
