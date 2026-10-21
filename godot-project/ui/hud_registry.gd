class_name HudRegistry
extends RefCounted
## HUD 注册表：托盘槽位 + 排序 + 外部（mod）注册校验。
##
## 只做三件事：把 HudWidget 放进对应槽、按 priority 排序、把方案 / 紧凑 / 菜单
## 状态广播给所有元素。不做布局求解，也不认识具体元素类型。

const MAX_WIDGET_WIDTH := 640.0
const MAX_WIDGET_HEIGHT := 480.0

var _ui_root: Node = null
var _slots: Dictionary[String, Control] = {}
var _widgets: Array[HudWidget] = []
var _by_id: Dictionary[StringName, HudWidget] = {}


func setup(ui_root: Node, slots: Dictionary) -> void:
	_ui_root = ui_root
	_slots.clear()
	for key: String in slots:
		var slot: Variant = slots[key]
		if slot is Control:
			_slots[key] = slot as Control


## 注册一个内置元素；重复 id / 未知槽位 / 超大元素会被拒绝。
func register(widget: HudWidget) -> bool:
	if widget == null:
		return false
	var id := _id_of(widget)
	if id == &"":
		push_warning("[HudRegistry] HUD 元素缺少 widget_id")
		return false
	if _by_id.has(id):
		push_warning("[HudRegistry] HUD 元素 id 重复：%s" % id)
		return false
	var slot := _slot_for(widget.tray, widget.align)
	if slot == null:
		push_warning("[HudRegistry] 未知托盘：%s" % widget.tray)
		return false
	widget.widget_id = id
	_by_id[id] = widget
	_widgets.append(widget)
	_insert(slot, widget)
	widget.attach(_ui_root)
	widget.refresh_scheme(UiState.scheme)
	widget.apply_compact(UiState.compact_level)
	widget.refresh_menu()
	return true


## mod 注册入口：PackedScene 的根节点必须继承 HudWidget。
## meta 支持 id / tray / align / priority / schemes / compact_level / mouse_filter。
func register_external(scene: PackedScene, meta: Dictionary) -> HudWidget:
	if scene == null:
		push_warning("[HudRegistry] mod UI scene 为空")
		return null
	var node := scene.instantiate()
	if not node is HudWidget:
		push_warning("[HudRegistry] mod UI 根节点必须继承 HudWidget")
		node.queue_free()
		return null
	var widget := node as HudWidget
	if meta.has("id"):
		widget.widget_id = StringName(String(meta["id"]))
	if meta.has("tray"):
		widget.tray = int(meta["tray"]) as HudWidget.Tray
	if meta.has("align"):
		widget.align = int(meta["align"]) as HudWidget.Align
	if meta.has("priority"):
		widget.priority = int(meta["priority"])
	if meta.has("schemes"):
		widget.schemes = int(meta["schemes"])
	if meta.has("compact_level"):
		widget.compact_level = int(meta["compact_level"])
	if meta.has("mouse_filter"):
		widget.mouse_filter = int(meta["mouse_filter"])
	if not register(widget):
		widget.queue_free()
		return null
	# 进树之后才拿得到组合最小尺寸；超限直接拒绝，避免 mod 挤爆 HUD。
	var min_size := widget.get_combined_minimum_size()
	if min_size.x > MAX_WIDGET_WIDTH or min_size.y > MAX_WIDGET_HEIGHT:
		push_warning("[HudRegistry] mod UI 尺寸超限：%s (%.0f x %.0f)" % [widget.widget_id, min_size.x, min_size.y])
		unregister(widget.widget_id)
		return null
	return widget


func unregister(id: StringName) -> bool:
	var widget: HudWidget = _by_id.get(id, null)
	if widget == null:
		return false
	_by_id.erase(id)
	_widgets.erase(widget)
	if is_instance_valid(widget):
		widget.queue_free()
	return true


func refresh_scheme(current: int) -> void:
	for widget in _widgets:
		widget.refresh_scheme(current)


func refresh_compact(level: int) -> void:
	for widget in _widgets:
		widget.apply_compact(level)


func refresh_menu() -> void:
	for widget in _widgets:
		widget.refresh_menu()


func widgets() -> Array[HudWidget]:
	return _widgets.duplicate()


func get_widget(id: StringName) -> HudWidget:
	return _by_id.get(id, null)


# ── 内部 ────────────────────────────────────────────────

func _id_of(widget: HudWidget) -> StringName:
	if widget.widget_id != &"":
		return widget.widget_id
	return StringName(widget.name)


func _slot_for(tray: int, align: int) -> Control:
	var suffix := "begin"
	if align == HudWidget.Align.CENTER:
		suffix = "center"
	elif align == HudWidget.Align.END:
		suffix = "end"
	match tray:
		HudWidget.Tray.TOP:
			return _slots.get("top_" + suffix, null)
		HudWidget.Tray.BOTTOM:
			return _slots.get("bottom_" + suffix, null)
		HudWidget.Tray.LEFT:
			return _slots.get("mid_begin", null)
		HudWidget.Tray.RIGHT:
			return _slots.get("mid_end", null)
		_:
			return _slots.get("mid_center", null)


func _insert(slot: Control, widget: HudWidget) -> void:
	var box := slot as BoxContainer
	if box == null:
		slot.add_child(widget)
		return
	var index := box.get_child_count()
	for i in range(box.get_child_count()):
		var other := box.get_child(i) as HudWidget
		if other == null:
			continue
		var before := false
		if widget.align == HudWidget.Align.BEGIN:
			before = widget.priority > other.priority
		else:
			before = widget.priority < other.priority
		if before:
			index = i
			break
	box.add_child(widget)
	if index < box.get_child_count() - 1:
		box.move_child(widget, index)
