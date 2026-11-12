class_name ParamPanel
extends Control
## 通用参数面板：消费 BevyAppManager.get_param_schema() 的返回，按 schema 代码
## 构建控件，收集覆盖值并通过 manager.set_param_overrides() 提交。
##
## 不依赖任何 .tscn：_ready / 首次 load_schema 时自建 UI 树。样式尽量少，
## 只为「能读、能改、能看出世界参数」服务；没有新增字体资源。
##
## 对外契约：
## - load_schema(result: Dictionary) -> void：接受 get_param_schema() 的完整返回；
##   ok=false 时显示错误标签并禁用 Apply，schema 为空。
## - collect_values() -> Dictionary：全 key -> 落盘整数（Bool 0/1，Enum 下标，
##   Fixed 是 round(显示值 * scale)）。
## - reset_defaults() -> void：所有控件回到 schema 默认值。
## - apply_to(manager: Node) -> bool：调 manager.set_param_overrides(collect_values())；
##   manager 为空 / 没有该方法 / 后端返回 ok=false 时返回 false，不崩溃。
## - close_requested：用户按「关闭」时发出；面板本身不决定隐藏，由调用方收尾。
## - schema_entries() / rows() / set_value(key, value) / apply_button() / close_button()：
##   测试只读访问。
##
## visible_if 的 key 是同一 mod 内的 local key；这里用 mod_id 拼全 key 后再比较，
## 条件不满足时隐藏整行（不是禁用）。隐藏只影响显示，collect_values 仍包含该行，
## 避免切换父开关时丢掉用户输入。
##
## World 作用域参数在每行加「建档后不可改」标记与颜色；建档前仍可编辑，不禁用。

signal apply_requested
## 用户按「关闭」；面板不自行隐藏，调用方决定返回哪里。
signal close_requested

const WORLD_COLOR := Color(0.95, 0.78, 0.35)
const ERROR_COLOR := Color(0.95, 0.45, 0.4)
const OK_COLOR := Color(0.55, 0.85, 0.55)
const PANEL_MIN_SIZE := Vector2(520, 420)
const ROW_SPACING := 8

var _root: VBoxContainer = null
var _sections: VBoxContainer = null
var _error_label: Label = null
var _status_label: Label = null
var _apply_button: Button = null
var _close_button: Button = null

## schema 原始条目（load_schema 时复制一份）。
var _schema: Array = []
## 全 key -> 行记录 { key, entry, row, label, control, value_label, scale, decimals }。
var _rows: Dictionary = {}
## 全 key 的显示顺序（与 schema 升序一致）。
var _order: Array[String] = []


func _ready() -> void:
	_ensure_ui()


## 接受 get_param_schema() 的返回。失败时显示错误并禁用 Apply。
func load_schema(result: Dictionary) -> void:
	_ensure_ui()
	_clear_schema()
	_status_label.text = ""

	if not bool(result.get("ok", false)):
		var messages: Array[String] = []
		for raw_error in result.get("errors", []):
			if typeof(raw_error) != TYPE_DICTIONARY:
				continue
			var error: Dictionary = raw_error
			messages.append("%s: %s" % [
				String(error.get("code", "param.invalid_spec")),
				String(error.get("message", "")),
			])
		_set_error("参数 schema 不可用：" + "；".join(messages))
		_apply_button.disabled = true
		return

	var raw_schema: Variant = result.get("schema", [])
	if typeof(raw_schema) != TYPE_ARRAY:
		_set_error("参数 schema 字段不是 Array")
		_apply_button.disabled = true
		return

	_schema = (raw_schema as Array).duplicate(true)
	_build_rows(_schema)
	_set_error("")
	_apply_button.disabled = false
	_refresh_visibility()


## 全 key -> 落盘整数。隐藏行也包含在内（其控件值仍然合法）。
func collect_values() -> Dictionary:
	_ensure_ui()
	var values: Dictionary = {}
	for key in _order:
		values[key] = _read_value(_rows[key])
	return values


## 所有控件回到 schema 默认值。
func reset_defaults() -> void:
	_ensure_ui()
	for key in _order:
		var entry: Dictionary = _rows[key]["entry"]
		set_value(key, int(entry.get("default", 0)))


## 提交到后端。成功返回 true；manager 不可用 / 没有方法 / 校验失败返回 false。
func apply_to(manager: Node) -> bool:
	_ensure_ui()
	if manager == null or not manager.has_method("set_param_overrides"):
		push_error("[ParamPanel] manager 为空或缺少 set_param_overrides()")
		_set_status("无法应用：后端管理器不可用", true)
		return false

	var result: Variant = manager.call("set_param_overrides", collect_values())
	if typeof(result) != TYPE_DICTIONARY:
		push_error("[ParamPanel] set_param_overrides() 没有返回 Dictionary")
		_set_status("无法应用：后端返回格式错误", true)
		return false

	var answer: Dictionary = result
	if not bool(answer.get("ok", false)):
		_set_status(_format_errors(answer.get("errors", [])), true)
		return false

	_set_status("已记录 %d 项参数，下次启动生效" % int(answer.get("count", 0)), false)
	return true


## 测试用：schema 条目的副本。
func schema_entries() -> Array:
	return _schema.duplicate(true)


## 测试用：行记录的副本（含 control / row 节点引用）。
func rows() -> Dictionary:
	return _rows.duplicate(true)


## 测试用：经控件写值（不绕过控件）。
func set_value(key: String, value: int) -> void:
	_ensure_ui()
	if not _rows.has(key):
		push_warning("[ParamPanel] set_value 未知 key：%s" % key)
		return
	var row: Dictionary = _rows[key]
	var kind := String(row["entry"].get("kind", ""))
	match kind:
		"bool":
			row["control"].button_pressed = value != 0
		"int":
			row["control"].value = float(value)
		"fixed":
			var scale := int(row["scale"])
			row["control"].value = float(value) / float(scale)
		"enum":
			var option: OptionButton = row["control"]
			option.select(clampi(value, 0, maxi(0, option.item_count - 1)))
	_update_fixed_label(row)
	_refresh_visibility()


## 测试用：Apply 按钮（失败 schema 时应 disabled）。
func apply_button() -> Button:
	_ensure_ui()
	return _apply_button


## 测试用：Close 按钮（面板不自行隐藏，只发 close_requested）。
func close_button() -> Button:
	_ensure_ui()
	return _close_button


func _on_apply_pressed() -> void:
	apply_requested.emit()


func _on_close_pressed() -> void:
	close_requested.emit()


# ───────────────────────── UI 构建 ─────────────────────────

func _ensure_ui() -> void:
	if _root != null:
		return

	custom_minimum_size = PANEL_MIN_SIZE
	_root = VBoxContainer.new()
	_root.name = "ParamPanelRoot"
	add_child(_root)
	_root.set_anchors_and_offsets_preset(Control.PRESET_FULL_RECT)
	_root.add_theme_constant_override("separation", ROW_SPACING)

	var title := Label.new()
	title.name = "TitleLabel"
	title.text = "参数设置"
	title.add_theme_font_size_override("font_size", 20)
	_root.add_child(title)

	_error_label = Label.new()
	_error_label.name = "ErrorLabel"
	_error_label.autowrap_mode = TextServer.AUTOWRAP_WORD_SMART
	_error_label.add_theme_color_override("font_color", ERROR_COLOR)
	_error_label.visible = false
	_root.add_child(_error_label)

	var scroll := ScrollContainer.new()
	scroll.name = "Scroll"
	scroll.size_flags_horizontal = Control.SIZE_EXPAND_FILL
	scroll.size_flags_vertical = Control.SIZE_EXPAND_FILL
	scroll.custom_minimum_size = Vector2(0, 320)
	_root.add_child(scroll)

	_sections = VBoxContainer.new()
	_sections.name = "Sections"
	_sections.size_flags_horizontal = Control.SIZE_EXPAND_FILL
	_sections.add_theme_constant_override("separation", 12)
	scroll.add_child(_sections)

	_status_label = Label.new()
	_status_label.name = "StatusLabel"
	_status_label.autowrap_mode = TextServer.AUTOWRAP_WORD_SMART
	_status_label.visible = false
	_root.add_child(_status_label)

	var buttons := HBoxContainer.new()
	buttons.name = "Buttons"
	buttons.add_theme_constant_override("separation", ROW_SPACING)
	_root.add_child(buttons)

	var reset_button := Button.new()
	reset_button.name = "ResetButton"
	reset_button.text = "恢复默认"
	reset_button.pressed.connect(reset_defaults)
	buttons.add_child(reset_button)

	var spacer := Control.new()
	spacer.size_flags_horizontal = Control.SIZE_EXPAND_FILL
	buttons.add_child(spacer)

	_close_button = Button.new()
	_close_button.name = "CloseButton"
	_close_button.text = "关闭"
	_close_button.pressed.connect(_on_close_pressed)
	buttons.add_child(_close_button)

	_apply_button = Button.new()
	_apply_button.name = "ApplyButton"
	_apply_button.text = "应用"
	_apply_button.disabled = true
	_apply_button.pressed.connect(_on_apply_pressed)
	buttons.add_child(_apply_button)


func _clear_schema() -> void:
	_order.clear()
	_rows.clear()
	_schema.clear()
	if _sections == null:
		return
	for child in _sections.get_children():
		_sections.remove_child(child)
		child.free()


func _build_rows(entries: Array) -> void:
	var group_sections: Dictionary = {}
	for raw_entry in entries:
		if typeof(raw_entry) != TYPE_DICTIONARY:
			continue
		var entry: Dictionary = raw_entry
		var key := String(entry.get("key", ""))
		if key.is_empty():
			continue

		var group := String(entry.get("group", ""))
		if group.is_empty():
			group = "常规"

		var section: VBoxContainer
		if group_sections.has(group):
			section = group_sections[group]
		else:
			section = _make_section(group)
			group_sections[group] = section
			_sections.add_child(section)

		var row := _make_row(entry)
		section.add_child(row["row"])
		_rows[key] = row
		_order.append(key)


func _make_section(group: String) -> VBoxContainer:
	var section := VBoxContainer.new()
	section.name = "Section_" + _node_safe(group)
	section.size_flags_horizontal = Control.SIZE_EXPAND_FILL
	section.add_theme_constant_override("separation", 6)

	var heading := Label.new()
	heading.text = group
	heading.add_theme_font_size_override("font_size", 16)
	section.add_child(heading)
	section.add_child(HSeparator.new())
	return section


func _make_row(entry: Dictionary) -> Dictionary:
	var key := String(entry.get("key", ""))
	var kind := String(entry.get("kind", "int"))
	var mod_id := String(entry.get("mod_id", ""))

	var row := HBoxContainer.new()
	row.name = "Row_" + _node_safe(key)
	row.add_theme_constant_override("separation", ROW_SPACING)

	var label := Label.new()
	label.text = String(entry.get("local_key", key))
	label.tooltip_text = key
	label.size_flags_horizontal = Control.SIZE_EXPAND_FILL
	row.add_child(label)

	if String(entry.get("scope", "world")) == "world":
		label.add_theme_color_override("font_color", WORLD_COLOR)
		var badge := Label.new()
		badge.text = "建档后不可改"
		badge.tooltip_text = "世界参数在建立存档后不可修改；建档前仍可编辑"
		badge.add_theme_color_override("font_color", WORLD_COLOR)
		badge.add_theme_font_size_override("font_size", 12)
		row.add_child(badge)

	var control: Control = null
	var value_label: Label = null
	var scale := 1
	var decimals := 0
	var default := int(entry.get("default", 0))

	match kind:
		"bool":
			var check := CheckButton.new()
			check.button_pressed = default != 0
			check.toggled.connect(_on_row_value_changed)
			control = check
			row.add_child(check)
		"int":
			var spin := SpinBox.new()
			var int_min := int(entry.get("min", 0))
			var int_max := int(entry.get("max", 0))
			if int_max < int_min:
				int_max = int_min
			spin.min_value = float(int_min)
			spin.max_value = float(int_max)
			spin.step = float(maxi(1, int(entry.get("step", 1))))
			spin.value = float(default)
			spin.allow_greater = false
			spin.allow_lesser = false
			spin.custom_minimum_size = Vector2(120, 0)
			spin.value_changed.connect(_on_row_value_changed)
			control = spin
			row.add_child(spin)
		"fixed":
			scale = maxi(1, int(entry.get("scale", 1)))
			decimals = _decimals_of(scale)
			var fixed_min := int(entry.get("min", 0))
			var fixed_max := int(entry.get("max", 0))
			if fixed_max < fixed_min:
				fixed_max = fixed_min
			var step := maxi(1, int(entry.get("step", 1)))
			var slider := HSlider.new()
			slider.min_value = float(fixed_min) / float(scale)
			slider.max_value = float(fixed_max) / float(scale)
			slider.step = float(step) / float(scale)
			slider.value = float(default) / float(scale)
			slider.size_flags_horizontal = Control.SIZE_EXPAND_FILL
			slider.custom_minimum_size = Vector2(160, 0)
			slider.value_changed.connect(_on_row_value_changed)
			control = slider
			row.add_child(slider)
			value_label = Label.new()
			value_label.custom_minimum_size = Vector2(64, 0)
			value_label.horizontal_alignment = HORIZONTAL_ALIGNMENT_RIGHT
			row.add_child(value_label)
		"enum":
			var option := OptionButton.new()
			for value in entry.get("values", []):
				option.add_item(String(value))
			option.select(clampi(default, 0, maxi(0, option.item_count - 1)))
			option.item_selected.connect(_on_row_value_changed)
			option.custom_minimum_size = Vector2(140, 0)
			control = option
			row.add_child(option)
		_:
			var fallback := Label.new()
			fallback.text = "（未知类型）"
			control = fallback
			row.add_child(fallback)

	var result := {
		"key": key,
		"entry": entry,
		"mod_id": mod_id,
		"row": row,
		"label": label,
		"control": control,
		"value_label": value_label,
		"scale": scale,
		"decimals": decimals,
	}
	_update_fixed_label(result)
	return result


# ───────────────────────── 值 / 可见性 ─────────────────────────

func _read_value(row: Dictionary) -> int:
	var kind := String(row["entry"].get("kind", ""))
	match kind:
		"bool":
			return 1 if row["control"].button_pressed else 0
		"int":
			return int(row["control"].value)
		"fixed":
			return int(round(float(row["control"].value) * float(row["scale"])))
		"enum":
			return int(row["control"].selected)
	return int(row["entry"].get("default", 0))


func _on_row_value_changed(_value: Variant = null) -> void:
	for key in _order:
		_update_fixed_label(_rows[key])
	_refresh_visibility()


func _update_fixed_label(row: Dictionary) -> void:
	if row["value_label"] == null:
		return
	var decimals := int(row["decimals"])
	var display := float(row["control"].value)
	row["value_label"].text = String.num(display, decimals)


func _refresh_visibility() -> void:
	for key in _order:
		var row: Dictionary = _rows[key]
		row["row"].visible = _is_row_visible(row["entry"])


## visible_if.key 是同一 mod 的 local key；拼全 key 后再查表，避免跨 mod 误判。
func _is_row_visible(entry: Dictionary) -> bool:
	var visible_if: Dictionary = entry.get("visible_if", {})
	if visible_if.is_empty():
		return true
	var local_key := String(visible_if.get("key", ""))
	if local_key.is_empty():
		return true
	var full_key := "%s.%s" % [String(entry.get("mod_id", "")), local_key]
	if not _rows.has(full_key):
		# 引用不在本表：交给 Rust schema 校验报错，UI 不擅自隐藏。
		return true
	return _read_value(_rows[full_key]) == int(visible_if.get("equals", 0))


func _decimals_of(scale: int) -> int:
	var decimals := 0
	var value := scale
	while value >= 10:
		value /= 10
		decimals += 1
	return decimals


func _node_safe(text: String) -> String:
	return text.replace(".", "_").replace(":", "_").replace("/", "_").replace("@", "_")


func _set_error(message: String) -> void:
	_error_label.text = message
	_error_label.visible = not message.is_empty()


func _set_status(message: String, is_error: bool) -> void:
	_status_label.text = message
	_status_label.visible = not message.is_empty()
	_status_label.add_theme_color_override("font_color", ERROR_COLOR if is_error else OK_COLOR)


func _format_errors(errors: Variant) -> String:
	var parts: Array[String] = []
	for raw_error in errors:
		if typeof(raw_error) != TYPE_DICTIONARY:
			continue
		var error: Dictionary = raw_error
		parts.append("%s: %s" % [
			String(error.get("code", "param.invalid_spec")),
			String(error.get("message", "")),
		])
	if parts.is_empty():
		return "应用失败"
	return "应用失败：" + "；".join(parts)
