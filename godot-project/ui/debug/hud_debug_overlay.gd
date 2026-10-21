class_name HudDebugOverlay
extends Control
## F3 调试层：显示逻辑尺寸 / 缩放 / 安全区 / 断点 / 槽位与元素矩形。
## 只读，不参与布局（PRESET_FULL_RECT + IGNORE）。

var _ui_root: Node = null
var _label: Label = null


func _ready() -> void:
	set_anchors_and_offsets_preset(Control.PRESET_FULL_RECT)
	mouse_filter = Control.MOUSE_FILTER_IGNORE

	var panel := PanelContainer.new()
	panel.name = "Panel"
	panel.position = Vector2(12.0, 12.0)
	panel.mouse_filter = Control.MOUSE_FILTER_IGNORE
	var style := StyleBoxFlat.new()
	style.bg_color = Color(0.0, 0.0, 0.0, 0.65)
	style.content_margin_left = 10.0
	style.content_margin_right = 10.0
	style.content_margin_top = 8.0
	style.content_margin_bottom = 8.0
	panel.add_theme_stylebox_override("panel", style)
	add_child(panel)

	_label = Label.new()
	_label.name = "Text"
	_label.mouse_filter = Control.MOUSE_FILTER_IGNORE
	_label.add_theme_font_size_override("font_size", 14)
	_label.add_theme_color_override("font_color", Color(0.8, 1.0, 0.8))
	panel.add_child(_label)


func setup(ui_root: Node) -> void:
	_ui_root = ui_root


func refresh() -> void:
	if _label == null or _ui_root == null:
		return
	var lines := PackedStringArray()
	lines.append("UI DEBUG  (F3)")
	lines.append("scheme=%s  compact=%d  scale=%.2f" % [
		_scheme_name(UiState.scheme), UiState.compact_level, UiState.ui_scale,
	])
	lines.append("logical=%.0fx%.0f  window=%s" % [
		UiState.logical_width, UiState.logical_height, str(get_window().size),
	])
	lines.append("safe l=%.0f t=%.0f r=%.0f b=%.0f" % [
		UiState.safe_insets.x, UiState.safe_insets.y, UiState.safe_insets.z, UiState.safe_insets.w,
	])
	lines.append("menu=%s  backend_paused=%s" % [
		str(UiState.is_menu_open()), str(UiState.backend_paused),
	])

	var client: Node = _ui_root.get("client")
	if client != null:
		var ctx: Variant = client.get("input_context")
		if ctx != null:
			lines.append("caps present=0x%x available=0x%x look=%s" % [
				ctx.present_mask(), ctx.available_mask(), str(ctx.has_look()),
			])

	var registry: Variant = _ui_root.get("registry")
	if registry != null:
		var widgets: Array = registry.widgets()
		lines.append("widgets=%d" % widgets.size())
		var rects: Array[Rect2] = []
		for widget: HudWidget in widgets:
			if not widget.visible:
				continue
			var rect := widget.get_global_rect()
			rects.append(rect)
			lines.append("  %s %s" % [widget.widget_id, str(rect)])
		for i in range(rects.size()):
			for j in range(i + 1, rects.size()):
				if rects[i].intersects(rects[j]):
					lines.append("  WARN overlap #%d <-> #%d" % [i, j])

	_label.text = "\n".join(lines)


func _scheme_name(scheme: int) -> String:
	var names := PackedStringArray()
	if scheme & UiState.Scheme.TOUCH:
		names.append("touch")
	if scheme & UiState.Scheme.KBM:
		names.append("kbm")
	if scheme & UiState.Scheme.GAMEPAD:
		names.append("gamepad")
	return "+".join(names)
