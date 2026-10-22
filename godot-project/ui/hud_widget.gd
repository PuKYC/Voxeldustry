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

## 按方案覆盖位置与尺寸。key 为 UiState.Scheme 的值（1 / 2 / 4），value 可含
## tray / align / priority / min_size（Vector2）；缺省字段沿用上面的默认声明。
## 例：{1: {"tray": Tray.BOTTOM, "align": Align.END, "min_size": Vector2(120, 120)}}
@export var scheme_overrides: Dictionary = {}
## 同组互斥 id（空 = 不参与）。同组里只有命中当前方案且 priority 最高的元素会激活，
## 用于「桌面是面板、手机是底部抽屉」这类结构不同、共用一个数据控制器的元素。
@export var group_id: StringName = &""

## UiRoot 引用；注册时由 HudRegistry 注入。
var ui_root: Node = null
var _attached := false
## 默认最小尺寸（首次注册 / 首次生效时记下），方案覆盖切回时用它恢复。
var _base_min_size: Vector2 = Vector2.ZERO
var _base_min_size_captured := false
var _data_visible := true
var _scheme_visible := true
var _compact_visible := true
var _group_visible := true


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


## 按当前方案解析后的落点 / 次序 / 尺寸（注册表用它们排布与排序）。
func effective_tray(current: int) -> int:
	return int(_override_for(current).get("tray", tray))


func effective_align(current: int) -> int:
	return int(_override_for(current).get("align", align))


func effective_priority(current: int) -> int:
	return int(_override_for(current).get("priority", priority))


## 当前方案下的最小尺寸：有覆盖用覆盖值，否则用默认值。
func effective_min_size(current: int) -> Vector2:
	_capture_base_min_size()
	var override := _override_for(current)
	if not override.has("min_size"):
		return _base_min_size
	return _to_size(override["min_size"])


## 所有方案会用到的最小尺寸（mod 尺寸上限检查要对每个方案都生效）。
func all_min_sizes() -> Array[Vector2]:
	_capture_base_min_size()
	var result: Array[Vector2] = [_base_min_size]
	for key: Variant in scheme_overrides.keys():
		var override: Variant = scheme_overrides[key]
		if override is Dictionary and (override as Dictionary).has("min_size"):
			result.append(_to_size((override as Dictionary)["min_size"]))
	return result


## 由注册表在输入方案变化时调用。
func refresh_scheme(current: int) -> void:
	_capture_base_min_size()
	_scheme_visible = (schemes & current) != 0
	custom_minimum_size = effective_min_size(current)
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


## 同组互斥的激活位；由注册表按「命中当前方案 + priority 最高」算出。
func set_group_active(value: bool) -> void:
	if _group_visible == value:
		return
	_group_visible = value
	_refresh_visibility()


func _refresh_visibility() -> void:
	visible = _data_visible and _scheme_visible and _compact_visible and _group_visible and not UiState.is_menu_open()


## 当前方案命中的覆盖：先找精确 key，再找位掩码相交的 key。
func _override_for(current: int) -> Dictionary:
	if scheme_overrides.is_empty():
		return {}
	var exact: Variant = scheme_overrides.get(current, null)
	if exact is Dictionary:
		return exact
	for key: Variant in scheme_overrides.keys():
		var override: Variant = scheme_overrides[key]
		if override is Dictionary and (int(key) & current) != 0:
			return override
	return {}


## 首次生效前记下默认最小尺寸：不能放 _ready()，子类覆写 _ready() 时会被跳过。
func _capture_base_min_size() -> void:
	if _base_min_size_captured:
		return
	_base_min_size = custom_minimum_size
	_base_min_size_captured = true


func _to_size(raw: Variant) -> Vector2:
	if raw is Vector2:
		return raw
	if raw is Vector2i:
		return Vector2(raw)
	if raw is Array and (raw as Array).size() == 2:
		return Vector2(float(raw[0]), float(raw[1]))
	return _base_min_size


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
