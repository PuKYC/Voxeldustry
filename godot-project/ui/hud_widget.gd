class_name HudWidget
extends Control
## HUD 元素契约：只声明「放哪个托盘、靠哪边、多重要、哪些输入方案可见」，
## 不写坐标、不写分辨率判断。布局由 UiRoot 的托盘 + 容器完成。
##
## 未来 mod 提供的 UI 只要继承本类（或提供一个 root 为本类的 PackedScene），
## 交给 HudRegistry.register_external() 就能自动获得桌面 / 移动 / 安全区适配。

enum Tray {
	TOP,
	BOTTOM,
	LEFT,
	RIGHT,
	CENTER,
}

enum Align {
	BEGIN,
	CENTER,
	END,
}

## 槽内唯一 id（注册表去重用）。留空时用节点名。
@export var widget_id: StringName = &""
@export var tray: Tray = Tray.TOP
@export var align: Align = Align.END
## 槽内排序：越大越靠屏幕边缘（BEGIN 槽里越大越靠左/上）。
@export var priority: int = 0
## 哪些输入方案显示：1=触屏 2=键鼠 4=手柄。
@export_flags("Touch:1", "KBM:2", "Gamepad:4") var schemes: int = 7
## 紧凑等级达到该值时隐藏（0 = 永不因紧凑隐藏）。
@export var compact_level: int = 0

## UiRoot 引用；注册时由 HudRegistry 注入。
var ui_root: Node = null
var _attached := false
var _data_visible := true
var _scheme_visible := true
var _compact_visible := true


## 注册时调用；子类在 _on_attach() 里连接 client 信号。
func attach(root: Node) -> void:
	ui_root = root
	_attached = true
	_refresh_visibility()
	_on_attach()


## 注册表移除元素前调用：先断回调再 queue_free（幂等）。
## 子类在 _on_detach() 里断开自己在 _on_attach() 里连的 client 信号 / 载荷订阅。
func detach() -> void:
	if not _attached:
		return
	_attached = false
	_on_detach()
	ui_root = null


## 由注册表在输入方案变化时调用。
func refresh_scheme(current: int) -> void:
	_scheme_visible = (schemes & current) != 0
	_refresh_visibility()
	_on_scheme_changed(current)


## 由注册表在紧凑等级变化时调用。
func apply_compact(level: int) -> void:
	_compact_visible = compact_level <= 0 or level < compact_level
	_refresh_visibility()
	_on_compact_changed(level)


## 业务数据有无（例如没有 HEALTH 载荷时隐藏血条）。
func set_data_visible(value: bool) -> void:
	if _data_visible == value:
		return
	_data_visible = value
	_refresh_visibility()


## 菜单打开时统一隐藏 HUD；由注册表刷新。
func refresh_menu() -> void:
	_refresh_visibility()


func _refresh_visibility() -> void:
	visible = _data_visible and _scheme_visible and _compact_visible and not UiState.is_menu_open()


# ── 子类可覆盖 ──────────────────────────────────────────

func _on_attach() -> void:
	pass


## detach() 时调用：断开 _on_attach() 里连的回调（不负责隐藏 / 释放动作）。
func _on_detach() -> void:
	pass


func _on_scheme_changed(_current: int) -> void:
	pass


func _on_compact_changed(_level: int) -> void:
	pass
