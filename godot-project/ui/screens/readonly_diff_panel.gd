class_name ReadOnlyDiffPanel
extends Control
## 只读打开后的装配差异面板。
##
## 数据源是 BevyClient.world_load_diff_ready -> BevyAppManager.get_world_load_diff()；
## 面板只消费字典，不直接碰后端。所有差异为空时显示「装配完全一致」，而不是空白。
##
## 对外契约：
## - load_diff(diff: Dictionary) -> void：重填面板；未知字段忽略。
## - close_requested：用户按 Close / 面板请求关闭；面板不自行隐藏。
## - section_count() / row_count(section: String) / summary_text() / close_button()：
##   测试只读访问。
##
## 纯代码建树，不依赖 .tscn；层级：遮罩 -> 居中面板 -> 标题 / 摘要 / 滚动区 / 关闭。

## 用户选择「以只读方式继续」：关闭面板并恢复游戏（只读，不会写盘）。
signal continue_requested
## 用户选择返回：回到暂停面板，不恢复游戏。
signal close_requested

const MASK_COLOR := Color(0.0, 0.0, 0.0, 0.55)
const PANEL_COLOR := Color(0.10, 0.11, 0.13, 0.98)
const TITLE_COLOR := Color(0.95, 0.78, 0.35)
const MUTED_COLOR := Color(0.72, 0.74, 0.78)
const WARN_COLOR := Color(0.95, 0.55, 0.45)
const OK_COLOR := Color(0.55, 0.85, 0.55)
const PANEL_MIN_SIZE := Vector2(680, 480)

var _summary: Label = null
var _sections: VBoxContainer = null
var _continue_button: Button = null
var _close_button: Button = null
## section 名 -> 该区的行容器（测试按名字数行）。
var _section_rows: Dictionary = {}


func _ready() -> void:
	_ensure_ui()


func _ensure_ui() -> void:
	if _sections != null:
		return
	set_anchors_and_offsets_preset(Control.PRESET_FULL_RECT)
	mouse_filter = Control.MOUSE_FILTER_STOP

	var mask := ColorRect.new()
	mask.color = MASK_COLOR
	mask.set_anchors_and_offsets_preset(Control.PRESET_FULL_RECT)
	mask.mouse_filter = Control.MOUSE_FILTER_STOP
	add_child(mask)

	var center := CenterContainer.new()
	center.set_anchors_and_offsets_preset(Control.PRESET_FULL_RECT)
	add_child(center)

	var panel := PanelContainer.new()
	panel.custom_minimum_size = PANEL_MIN_SIZE
	center.add_child(panel)

	var style := StyleBoxFlat.new()
	style.bg_color = PANEL_COLOR
	style.set_corner_radius_all(8)
	style.set_content_margin_all(18)
	panel.add_theme_stylebox_override("panel", style)

	var outer := VBoxContainer.new()
	outer.add_theme_constant_override("separation", 10)
	panel.add_child(outer)

	var title := Label.new()
	title.text = "World differs from current build"
	title.add_theme_font_size_override("font_size", 22)
	title.add_theme_color_override("font_color", TITLE_COLOR)
	outer.add_child(title)

	_summary = Label.new()
	_summary.autowrap_mode = TextServer.AUTOWRAP_WORD_SMART
	_summary.add_theme_color_override("font_color", MUTED_COLOR)
	outer.add_child(_summary)

	var scroll := ScrollContainer.new()
	scroll.size_flags_vertical = Control.SIZE_EXPAND_FILL
	scroll.horizontal_scroll_mode = ScrollContainer.SCROLL_MODE_DISABLED
	outer.add_child(scroll)

	_sections = VBoxContainer.new()
	_sections.size_flags_horizontal = Control.SIZE_EXPAND_FILL
	_sections.add_theme_constant_override("separation", 12)
	scroll.add_child(_sections)

	var footer := HBoxContainer.new()
	footer.alignment = BoxContainer.ALIGNMENT_END
	outer.add_child(footer)
	_continue_button = Button.new()
	_continue_button.text = "Continue (read-only)"
	_continue_button.custom_minimum_size = Vector2(200, 40)
	_continue_button.pressed.connect(_on_continue_pressed)
	footer.add_child(_continue_button)

	_close_button = Button.new()
	_close_button.text = "Back"
	_close_button.custom_minimum_size = Vector2(120, 40)
	_close_button.pressed.connect(_on_close_pressed)
	footer.add_child(_close_button)


func _on_continue_pressed() -> void:
	continue_requested.emit()


func _on_close_pressed() -> void:
	close_requested.emit()


## 重填面板。diff 为 get_world_load_diff() 的返回（空字典 = 装配完全一致）。
func load_diff(diff: Dictionary) -> void:
	_ensure_ui()
	_clear_sections()

	var mods: Array = _as_array(diff.get("mods", []))
	var plugins: Dictionary = _as_dictionary(diff.get("plugins", {}))
	var actions: Array = _as_array(diff.get("actions", []))
	var params: Array = _as_array(diff.get("params", []))

	var missing_plugins: Array = _as_array(plugins.get("missing", []))
	var extra_plugins: Array = _as_array(plugins.get("extra", []))
	var order_changed := bool(plugins.get("order_changed", false))

	if mods.is_empty() and missing_plugins.is_empty() and extra_plugins.is_empty() \
			and not order_changed and actions.is_empty() and params.is_empty():
		_summary.text = "装配完全一致：当前 mod、插件、动作表与参数都能对上存档。"
		_summary.add_theme_color_override("font_color", OK_COLOR)
		return

	_summary.text = "共 %d 项 mod、%d 项插件、%d 条动作、%d 个参数对不上；世界以只读方式打开，原档未被改写。" % [
		mods.size(),
		missing_plugins.size() + extra_plugins.size() + (1 if order_changed else 0),
		actions.size(),
		params.size(),
	]
	_summary.add_theme_color_override("font_color", WARN_COLOR)

	if not mods.is_empty():
		var rows := _add_section("mods", "Mods")
		for raw in mods:
			_add_row(rows, _format_mod(raw))

	if not missing_plugins.is_empty() or not extra_plugins.is_empty() or order_changed:
		var rows := _add_section("plugins", "Plugins")
		for raw in missing_plugins:
			_add_row(rows, "- missing: %s" % String(raw))
		for raw in extra_plugins:
			_add_row(rows, "+ extra: %s" % String(raw))
		if order_changed:
			var saved := PackedStringArray(_as_array(plugins.get("saved", [])))
			var current := PackedStringArray(_as_array(plugins.get("current", [])))
			_add_row(rows, "order changed")
			_add_row(rows, "saved:   %s" % ", ".join(saved))
			_add_row(rows, "current: %s" % ", ".join(current))

	if not actions.is_empty():
		var rows := _add_section("actions", "Actions")
		for raw in actions:
			_add_row(rows, _format_action(raw))

	if not params.is_empty():
		var rows := _add_section("params", "World params")
		for raw in params:
			_add_row(rows, _format_param(raw))


func section_count() -> int:
	return _section_rows.size()


func row_count(section: String) -> int:
	if not _section_rows.has(section):
		return 0
	return (_section_rows[section] as VBoxContainer).get_child_count()


func summary_text() -> String:
	return _summary.text if _summary != null else ""


func continue_button() -> Button:
	return _continue_button


func close_button() -> Button:
	return _close_button


func _add_section(key: String, title: String) -> VBoxContainer:
	var header := Label.new()
	header.text = title
	header.add_theme_font_size_override("font_size", 17)
	header.add_theme_color_override("font_color", TITLE_COLOR)
	_sections.add_child(header)

	var rows := VBoxContainer.new()
	rows.add_theme_constant_override("separation", 4)
	_sections.add_child(rows)
	_section_rows[key] = rows
	return rows


func _add_row(parent: VBoxContainer, text: String) -> void:
	var label := Label.new()
	label.text = text
	label.autowrap_mode = TextServer.AUTOWRAP_WORD_SMART
	label.add_theme_color_override("font_color", MUTED_COLOR)
	parent.add_child(label)


func _clear_sections() -> void:
	if _sections == null:
		return
	for child in _sections.get_children():
		_sections.remove_child(child)
		child.queue_free()
	_section_rows.clear()


func _format_mod(raw: Variant) -> String:
	if typeof(raw) != TYPE_DICTIONARY:
		return String(raw)
	var entry: Dictionary = raw
	var kind := String(entry.get("kind", "changed"))
	var id := String(entry.get("id", "?"))
	match kind:
		"missing":
			return "- missing: %s (saved v%d)" % [id, int(entry.get("saved_version", -1))]
		"extra":
			return "+ extra: %s (current v%d)" % [id, int(entry.get("current_version", -1))]
		_:
			return "~ version: %s (saved v%d, current v%d)" % [
				id,
				int(entry.get("saved_version", -1)),
				int(entry.get("current_version", -1)),
			]


func _format_action(raw: Variant) -> String:
	if typeof(raw) != TYPE_DICTIONARY:
		return String(raw)
	var entry: Dictionary = raw
	var kind := String(entry.get("kind", "changed"))
	var name := String(entry.get("name", "?"))
	match kind:
		"missing":
			return "- missing: %s (action %d, bit %d)" % [
				name,
				int(entry.get("saved_action", -1)),
				int(entry.get("saved_channel_bit", -1)),
			]
		"extra":
			return "+ extra: %s (action %d, bit %d)" % [
				name,
				int(entry.get("current_action", -1)),
				int(entry.get("current_channel_bit", -1)),
			]
		_:
			return "~ changed: %s (action %d -> %d, bit %d -> %d)" % [
				name,
				int(entry.get("saved_action", -1)),
				int(entry.get("current_action", -1)),
				int(entry.get("saved_channel_bit", -1)),
				int(entry.get("current_channel_bit", -1)),
			]


func _format_param(raw: Variant) -> String:
	if typeof(raw) != TYPE_DICTIONARY:
		return String(raw)
	var entry: Dictionary = raw
	var kind := String(entry.get("kind", "changed"))
	var key := String(entry.get("key", "?"))
	match kind:
		"missing":
			return "- missing: %s (saved %d)" % [key, int(entry.get("saved_value", 0))]
		"extra":
			return "+ extra: %s (current %d)" % [key, int(entry.get("current_value", 0))]
		_:
			return "~ changed: %s (%d -> %d)" % [
				key,
				int(entry.get("saved_value", 0)),
				int(entry.get("current_value", 0)),
			]


func _as_array(value: Variant) -> Array:
	if typeof(value) == TYPE_ARRAY:
		return value as Array
	if typeof(value) == TYPE_PACKED_STRING_ARRAY:
		var out: Array = []
		for item in value as PackedStringArray:
			out.append(item)
		return out
	return []


func _as_dictionary(value: Variant) -> Dictionary:
	if typeof(value) == TYPE_DICTIONARY:
		return value as Dictionary
	return {}
