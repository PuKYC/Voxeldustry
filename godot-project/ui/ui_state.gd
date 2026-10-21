class_name UiState
extends RefCounted
## UI 全局状态（静态，无 Autoload）。
##
## 读：任何 UI、CameraRig、BevyClient；写：只有 UiRoot（方案 / 缩放 / 菜单）。
## 用静态状态而不是 Autoload，是为了让独立场景（gameplay_test / 未来主场景）
## 挂上 UiRoot 就能用，不依赖 project.godot 的 autoload 顺序。

## 输入方案位掩码。一套 UI 树，按位显隐，不做两套界面。
enum Scheme {
	TOUCH = 1,
	KBM = 2,
	GAMEPAD = 4,
}

const ALL_SCHEMES := Scheme.TOUCH | Scheme.KBM | Scheme.GAMEPAD

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


static func reset() -> void:
	scheme = Scheme.KBM
	menu_depth = 0
	backend_paused = false
	ui_scale = 1.0
	safe_insets = Vector4.ZERO
	logical_width = 1920.0
	logical_height = 1080.0
	compact_level = 0
