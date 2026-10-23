class_name ActionHints
extends HudWidget
## 键鼠 / 手柄动作提示：一行 Label，显示当前布局的按键提示
## （例："[LMB] Attack  [RMB] Place  [Space] Jump"，由 InputBindings.hint_text 生成）。
##
## 只在键鼠 / 手柄方案显示（schemes = KBM|GAMEPAD），触屏方案没有物理按键可提示，
## hint_text 返回空串时整行隐藏。不拦输入（mouse_filter = IGNORE）。

const WIDGET_ID := &"action_hints"
const FONT_SIZE := 20

var _label: Label = null
var _text := ""


func _init() -> void:
	widget_id = WIDGET_ID
	tray = HudWidget.Tray.BOTTOM
	align = HudWidget.Align.BEGIN
	priority = 10
	schemes = UiState.Scheme.KBM | UiState.Scheme.GAMEPAD
	mouse_filter = Control.MOUSE_FILTER_IGNORE
	_ensure_label()


func _ready() -> void:
	_ensure_label()
	_apply()


## 设置提示文案；空串整行隐藏。
func set_hint(text: String) -> void:
	_text = text
	_apply()


func hint_text() -> String:
	return _text


func label() -> Label:
	return _label


func _apply() -> void:
	_ensure_label()
	if _label != null:
		_label.text = _text
	set_data_visible(not _text.is_empty())


func _ensure_label() -> void:
	if _label != null and is_instance_valid(_label):
		return
	_label = Label.new()
	_label.name = "Hints"
	_label.mouse_filter = Control.MOUSE_FILTER_IGNORE
	_label.horizontal_alignment = HORIZONTAL_ALIGNMENT_LEFT
	_label.vertical_alignment = VERTICAL_ALIGNMENT_CENTER
	_label.add_theme_font_size_override("font_size", FONT_SIZE)
	_label.add_theme_color_override("font_outline_color", Color(0.0, 0.0, 0.0, 0.9))
	_label.add_theme_constant_override("outline_size", 4)
	add_child(_label)
	_label.set_anchors_and_offsets_preset(Control.PRESET_FULL_RECT)
