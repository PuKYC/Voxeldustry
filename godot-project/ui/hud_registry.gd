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
	var slot := _slot_for(widget.effective_tray(UiState.scheme), widget.effective_align(UiState.scheme))
	if slot == null:
		push_warning("[HudRegistry] 未知托盘：%s" % widget.effective_tray(UiState.scheme))
		return false
	widget.widget_id = id
	_by_id[id] = widget
	_widgets.append(widget)
	_insert(slot, widget, UiState.scheme)
	widget.attach(_ui_root)
	widget.refresh_scheme(UiState.scheme)
	widget.apply_compact(UiState.compact_level)
	widget.refresh_menu()
	_apply_groups(UiState.scheme)
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
	if meta.has("scheme_overrides"):
		var overrides: Variant = meta["scheme_overrides"]
		if overrides is Dictionary:
			widget.scheme_overrides = overrides
		else:
			push_warning("[HudRegistry] scheme_overrides 必须是 Dictionary：%s" % widget.widget_id)
	# 入树前先量一次：custom_minimum_size / 子节点尺寸大多能直接得出，
	# 尽早拒绝，省掉「先 attach 再 detach」的回滚成本。
	var pre_size := _oversized(widget)
	if pre_size != Vector2.ZERO:
		push_warning("[HudRegistry] mod UI 尺寸超限：%s (%.0f x %.0f)" % [widget.widget_id, pre_size.x, pre_size.y])
		widget.queue_free()
		return null
	if not register(widget):
		widget.queue_free()
		return null
	# 进树后再量一次：容器与子节点的最小尺寸此时才最终确定。
	# 超限走 unregister（它会先 detach 再 queue_free），不留 client 回调。
	var min_size := _oversized(widget)
	if min_size != Vector2.ZERO:
		push_warning("[HudRegistry] mod UI 尺寸超限：%s (%.0f x %.0f)" % [widget.widget_id, min_size.x, min_size.y])
		unregister(widget.widget_id)
		return null
	return widget


## 超过尺寸上限时返回实际尺寸，否则返回 Vector2.ZERO。
## 对每个方案声明的 min_size 都检查：只在 TOUCH 下超限的 mod 元素也要被拒。
func _oversized(widget: HudWidget) -> Vector2:
	var worst := widget.get_combined_minimum_size()
	for size: Vector2 in widget.all_min_sizes():
		worst.x = maxf(worst.x, size.x)
		worst.y = maxf(worst.y, size.y)
	if worst.x > MAX_WIDGET_WIDTH or worst.y > MAX_WIDGET_HEIGHT:
		return worst
	return Vector2.ZERO


func unregister(id: StringName) -> bool:
	var widget: HudWidget = _by_id.get(id, null)
	if widget == null:
		return false
	_by_id.erase(id)
	_widgets.erase(widget)
	_detach_widget(widget)
	_apply_groups(UiState.scheme)
	return true


## 移除元素：detach 先断回调，再离开容器并 queue_free。
## 只 queue_free 不会断开 listen_component / 信号连接，被拒的 mod 元素会留下回调。
func _detach_widget(widget: HudWidget) -> void:
	if not is_instance_valid(widget):
		return
	widget.detach()
	var parent := widget.get_parent()
	if parent != null:
		parent.remove_child(widget)
	widget.queue_free()


func refresh_scheme(current: int) -> void:
	# 先按新方案重排（tray / align / priority），再广播给元素自己（尺寸 / 文案 / 显隐）。
	for widget in _widgets.duplicate():
		relocate(widget, current)
	for widget in _widgets:
		widget.refresh_scheme(current)
	_apply_groups(current)


## 按当前方案重排一个元素：落点槽位变了就搬槽位，没变则按新 priority 重新插入。
## 返回 true 表示父容器确实变了。
func relocate(widget: HudWidget, current: int) -> bool:
	if not is_instance_valid(widget) or not _widgets.has(widget):
		return false
	var tray := widget.effective_tray(current)
	var slot := _slot_for(tray, widget.effective_align(current))
	if slot == null:
		push_warning("[HudRegistry] 未知托盘：%s" % tray)
		return false
	var parent := widget.get_parent()
	var changed := parent != slot
	if parent != null:
		parent.remove_child(widget)
	_insert(slot, widget, current)
	return changed


func refresh_compact(level: int) -> void:
	for widget in _widgets:
		widget.apply_compact(level)


func refresh_menu() -> void:
	for widget in _widgets:
		widget.refresh_menu()


## 同组互斥：每个 group_id 只让「命中当前方案 + priority 最高」的那个元素激活。
## 不命中方案的元素本来就被 schemes 隐藏，这里不参与竞争。
func _apply_groups(current: int) -> void:
	if _widgets.is_empty():
		return
	var best: Dictionary[StringName, HudWidget] = {}
	for widget in _widgets:
		if widget.group_id == &"" or (widget.schemes & current) == 0:
			continue
		var chosen: HudWidget = best.get(widget.group_id, null)
		if chosen == null or widget.effective_priority(current) > chosen.effective_priority(current):
			best[widget.group_id] = widget
	for widget in _widgets:
		if widget.group_id == &"":
			widget.set_group_active(true)
		else:
			widget.set_group_active(best.get(widget.group_id, null) == widget)


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


func _insert(slot: Control, widget: HudWidget, current: int) -> void:
	var box := slot as BoxContainer
	if box == null:
		slot.add_child(widget)
		return
	var priority := widget.effective_priority(current)
	var align := widget.effective_align(current)
	var index := box.get_child_count()
	for i in range(box.get_child_count()):
		var other := box.get_child(i) as HudWidget
		if other == null:
			continue
		var before := false
		if align == HudWidget.Align.BEGIN:
			before = priority > other.effective_priority(current)
		else:
			before = priority < other.effective_priority(current)
		if before:
			index = i
			break
	box.add_child(widget)
	if index < box.get_child_count() - 1:
		box.move_child(widget, index)
