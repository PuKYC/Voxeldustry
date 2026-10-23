class_name UiState
extends RefCounted
## UI 全局状态（静态，无 Autoload）。
##
## 读：任何 UI、CameraRig、BevyClient；写：只有 UiRoot（方案 / 缩放 / 菜单 / 设置）。
## 用静态状态而不是 Autoload，是为了让独立场景（gameplay_test / 未来主场景）
## 挂上 UiRoot 就能用，不依赖 project.godot 的 autoload 顺序。

## 输入方案位掩码。一套 UI 树，按位显隐，不做两套界面。
enum Scheme {
	TOUCH = 1,
	KBM = 2,
	GAMEPAD = 4,
}

const ALL_SCHEMES := Scheme.TOUCH | Scheme.KBM | Scheme.GAMEPAD

## 静态设备类别：决定缩放目标、安全区与基础边距。
## 与动态的 scheme（输入方案）分开：平板接键盘时 scheme 变 KBM，platform 仍是 TABLET，
## 边距与缩放不该跟着输入事件抖动。
enum Platform {
	DESKTOP,
	PHONE,
	TABLET,
}

## 操作布局预设（期 3）：CONTEXT = 精简上下文（主按钮随目标在 attack / break_block 间切换），
## SPLIT = 拆成独立按钮。只影响输入绑定层生成哪些按钮，不改变 HUD 位置。
enum ActionLayout {
	CONTEXT = 0,
	SPLIT = 1,
}

## 虚拟摇杆模式（期 3）：FIXED = 固定在屏幕角落；FLOATING = 按下处浮起、抬手隐藏。
enum JoystickMode {
	FIXED = 0,
	FLOATING = 1,
}

## 设置文件：user://ui_settings.cfg，ConfigFile 的 [ui] 段。
const SETTINGS_VERSION := 1
const SETTINGS_PATH := "user://ui_settings.cfg"
const SETTINGS_SECTION := "ui"

## 设置默认值与范围（reset() / load_settings() 的校验边界）。
const DEFAULT_BUTTON_SCALE := 1.0
const DEFAULT_BUTTON_OPACITY := 0.75
const BUTTON_SCALE_MIN := 0.8
const BUTTON_SCALE_MAX := 1.3
const BUTTON_OPACITY_MIN := 0.35
const BUTTON_OPACITY_MAX := 1.0

## 物理尺寸换算（毫米 -> 设计单位）。屏幕 DPI 不可靠时回退 160。
const FALLBACK_DPI := 160.0
const PHYSICAL_UNITS_MIN := 48.0
const PHYSICAL_UNITS_MAX := 220.0
## 动作按钮尺寸：先按物理毫米算出设计单位，再乘 button_scale；平板再放大 1.1。
const ACTION_BUTTON_UNITS_MIN := 56.0
const ACTION_BUTTON_UNITS_MAX := 180.0
const TABLET_BUTTON_FACTOR := 1.1
const ACTION_BUTTON_MM: Dictionary = {
	"main": 10.0,
	"sub": 9.0,
	"jump": 9.5,
}

## 当前设备类别（静态，由 UiRoot 在启动 / 缩放重算时写入）。
static var platform: int = Platform.DESKTOP
## 当前输入方案（位掩码）。
static var scheme: int = Scheme.KBM
## 菜单栈深度：> 0 表示有菜单打开（游戏暂停、不捕获鼠标、HUD 隐藏）。
static var menu_depth: int = 0
## 是否已经请求后端暂停。
static var backend_paused: bool = false
## 当前 Window.content_scale_factor。
static var ui_scale: float = 1.0
## 安全区内缩，设计单位：(left, top, right, bottom)。
static var safe_insets: Vector4 = Vector4.ZERO
## 当前逻辑可用尺寸（设计单位，不含物理像素）。
static var logical_width: float = 1920.0
static var logical_height: float = 1080.0
## 0 = 正常，1 = 紧凑，2 = 极窄。
static var compact_level: int = 0

## 期 3 设置（落盘 user://ui_settings.cfg，由暂停菜单的设置区修改）。
static var action_layout: int = ActionLayout.CONTEXT
## 动作按钮整体缩放（0.8 .. 1.3）。
static var button_scale: float = DEFAULT_BUTTON_SCALE
## 动作按钮空闲透明度（0.35 .. 1.0，按下时更亮）。
static var button_opacity: float = DEFAULT_BUTTON_OPACITY
## 左手模式：摇杆 / 动作簇放右侧，视角区放左侧。
static var left_handed: bool = false
## 摇杆模式：FIXED / FLOATING。
static var joystick_mode: int = JoystickMode.FIXED


static func has_scheme(value: int) -> bool:
	return (scheme & value) != 0


## 返回 true 表示方案真的变了（调用方据此刷新 UI）。
static func set_scheme(value: int) -> bool:
	if value == scheme:
		return false
	scheme = value
	return true


static func is_menu_open() -> bool:
	return menu_depth > 0


## 只有没有菜单时才允许玩法输入（BevyClient 每帧判断）。
static func accepts_gameplay_input() -> bool:
	return menu_depth == 0


## 键鼠视角是否需要鼠标捕获；触屏方案永不捕获。
static func mouse_look_allowed() -> bool:
	return menu_depth == 0 and not has_scheme(Scheme.TOUCH)


static func set_menu_open(open: bool) -> void:
	menu_depth = 1 if open else 0


## 物理毫米 -> 设计单位。
## mm/25.4 * dpi 得到物理像素，再除以 content_scale_factor 得到设计单位；
## dpi <= 0 或屏幕尺寸无效时回退 160，最后 clamp 到 [48, 220]。
static func physical_units(mm: float) -> float:
	var dpi := float(DisplayServer.screen_get_dpi())
	var screen := DisplayServer.screen_get_size()
	if dpi <= 0.0 or screen.x <= 0 or screen.y <= 0:
		dpi = FALLBACK_DPI
	var units := mm / 25.4 * dpi / maxf(ui_scale, 0.01)
	return clampf(units, PHYSICAL_UNITS_MIN, PHYSICAL_UNITS_MAX)


## 动作按钮尺寸（设计单位）：按 role 取物理毫米，乘 button_scale，平板再放大 1.1。
static func action_button_units(size_role: String) -> float:
	var mm := float(ACTION_BUTTON_MM.get(size_role, ACTION_BUTTON_MM["sub"]))
	var units := physical_units(mm) * button_scale
	if platform == Platform.TABLET:
		units *= TABLET_BUTTON_FACTOR
	return clampf(units, ACTION_BUTTON_UNITS_MIN, ACTION_BUTTON_UNITS_MAX)


static func settings_path() -> String:
	return SETTINGS_PATH


## 从 user://ui_settings.cfg 读取设置。文件缺失 / 解析失败 / 字段缺失或类型不符时
## 保持默认值，数值越界则 clamp（方便以后加字段时向前兼容）。
static func load_settings() -> void:
	_apply_settings_defaults()
	var config := ConfigFile.new()
	if config.load(SETTINGS_PATH) != OK:
		return
	# 版本仅记录，当前只有 v1；未知版本仍按已知键名读取，缺的字段用默认值。
	var version := SETTINGS_VERSION
	if config.has_section_key(SETTINGS_SECTION, "version"):
		var raw_version: Variant = config.get_value(SETTINGS_SECTION, "version")
		if raw_version is int:
			version = int(raw_version)
	if config.has_section_key(SETTINGS_SECTION, "action_layout"):
		var action_layout_value: Variant = config.get_value(SETTINGS_SECTION, "action_layout")
		if action_layout_value is int:
			action_layout = clampi(int(action_layout_value), ActionLayout.CONTEXT, ActionLayout.SPLIT)
	if config.has_section_key(SETTINGS_SECTION, "button_scale"):
		var scale_value: Variant = config.get_value(SETTINGS_SECTION, "button_scale")
		if _is_number(scale_value):
			button_scale = clampf(float(scale_value), BUTTON_SCALE_MIN, BUTTON_SCALE_MAX)
	if config.has_section_key(SETTINGS_SECTION, "button_opacity"):
		var opacity_value: Variant = config.get_value(SETTINGS_SECTION, "button_opacity")
		if _is_number(opacity_value):
			button_opacity = clampf(float(opacity_value), BUTTON_OPACITY_MIN, BUTTON_OPACITY_MAX)
	if config.has_section_key(SETTINGS_SECTION, "left_handed"):
		var left_handed_value: Variant = config.get_value(SETTINGS_SECTION, "left_handed")
		if left_handed_value is bool:
			left_handed = bool(left_handed_value)
	if config.has_section_key(SETTINGS_SECTION, "joystick_mode"):
		var joystick_mode_value: Variant = config.get_value(SETTINGS_SECTION, "joystick_mode")
		if joystick_mode_value is int:
			joystick_mode = clampi(int(joystick_mode_value), JoystickMode.FIXED, JoystickMode.FLOATING)
	if version is int and int(version) > SETTINGS_VERSION:
		push_warning("[UiState] ui_settings.cfg 版本 %d 高于当前 %d，按已知字段读取" % [int(version), SETTINGS_VERSION])


## 立即写盘到 user://ui_settings.cfg。写失败只告警，不影响当前会话。
static func save_settings() -> void:
	var config := ConfigFile.new()
	config.set_value(SETTINGS_SECTION, "version", SETTINGS_VERSION)
	config.set_value(SETTINGS_SECTION, "action_layout", action_layout)
	config.set_value(SETTINGS_SECTION, "button_scale", button_scale)
	config.set_value(SETTINGS_SECTION, "button_opacity", button_opacity)
	config.set_value(SETTINGS_SECTION, "left_handed", left_handed)
	config.set_value(SETTINGS_SECTION, "joystick_mode", joystick_mode)
	var err := config.save(SETTINGS_PATH)
	if err != OK:
		push_warning("[UiState] 保存设置失败 (%d)：%s" % [err, SETTINGS_PATH])


## 全部归默认（测试 / 退出场景用）。不删除已经落盘的设置文件。
static func reset() -> void:
	platform = Platform.DESKTOP
	scheme = Scheme.KBM
	menu_depth = 0
	backend_paused = false
	ui_scale = 1.0
	safe_insets = Vector4.ZERO
	logical_width = 1920.0
	logical_height = 1080.0
	compact_level = 0
	_apply_settings_defaults()


static func _apply_settings_defaults() -> void:
	action_layout = ActionLayout.CONTEXT
	button_scale = DEFAULT_BUTTON_SCALE
	button_opacity = DEFAULT_BUTTON_OPACITY
	left_handed = false
	joystick_mode = JoystickMode.FIXED


static func _is_number(value: Variant) -> bool:
	return value is int or value is float
