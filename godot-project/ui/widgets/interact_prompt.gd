class_name InteractPrompt
extends HudWidget
## 交互提示：当前有可用交互时显示英文文案（Label + 描边）。
##
## 数据源：BevyEnums.Payload.INTERACTION（fields: action, enabled）。
## enabled 且 action != 0 才显示。
## 限制：载荷目前只给 ActionId，没有动作显示名，所以不反查动作名
## （等 INTERACTION 契约扩展出显示名后再本地化；不为此改 Rust）。
## 无 client 时隐藏；全部信号驱动，不轮询、不写坐标。

## 没有可用键位 / 触屏时的兜底文案。
const FALLBACK_TEXT := "Interact"
## 键鼠方案：按键名 + 动作名，例如 "[E] Interact"。
const PROMPT_FORMAT := "[%s] Interact"
## 触屏方案没有物理按键提示，固定显示 Tap。
const TOUCH_TEXT := "Tap"

## BevyClient；未 attach 时为 null。
var _client: Object = null
## 当前受控实体（用于切换时立即刷新 / 隐藏）。
var _entity: EntityData = null
var _label: Label = null


func _on_attach() -> void:
	_label = get_node_or_null("Prompt") as Label
	_refresh_prompt()
	_client = _resolve_client()
	set_data_visible(false)
	if _client == null:
		return
	if _client.has_method("listen_component"):
		_client.listen_component(BevyEnums.Payload.INTERACTION, _on_interaction_changed)
	if _client.has_signal("controlled_entity_changed"):
		_client.connect(&"controlled_entity_changed", _on_controlled_entity_changed)


func _on_detach() -> void:
	if _client == null:
		return
	if _client.has_method("unlisten_component"):
		_client.unlisten_component(BevyEnums.Payload.INTERACTION, _on_interaction_changed)
	if _client.has_signal("controlled_entity_changed"):
		if _client.is_connected(&"controlled_entity_changed", _on_controlled_entity_changed):
			_client.disconnect(&"controlled_entity_changed", _on_controlled_entity_changed)
	_client = null
	_entity = null


## P3b：提示文案用真实输入绑定刷新（方案切换时由注册表调用本钩子）。
func _on_scheme_changed(_current: int) -> void:
	_refresh_prompt()


## 触屏固定 Tap；键鼠取 interact 的第一个键鼠事件 as_text()，拼 "[%s] Interact"；
## 没有 interact 动作 / 没有键鼠事件时回退 "Interact"。
func _refresh_prompt() -> void:
	if _label == null:
		return
	_label.text = _prompt_text()


func _prompt_text() -> String:
	if UiState.has_scheme(UiState.Scheme.TOUCH):
		return TOUCH_TEXT
	var key_text := _first_key_text()
	if key_text.is_empty():
		return FALLBACK_TEXT
	return PROMPT_FORMAT % key_text


func _first_key_text() -> String:
	if not InputMap.has_action("interact"):
		return ""
	for event in InputMap.action_get_events("interact"):
		if event is InputEventKey:
			var key := event as InputEventKey
			# BevyInput 用 physical_keycode 注册动作，InputEventKey.as_text() 会带上
			# " - Physical" 后缀；这里按物理键位取干净的键名（"E" 而不是 "E - Physical"）。
			var code: Key = key.physical_keycode if key.physical_keycode != 0 else key.keycode
			var text := OS.get_keycode_string(code)
			if not text.is_empty():
				return text
			return key.as_text()
		if event is InputEventMouseButton:
			return (event as InputEvent).as_text()
	return ""


## INTERACTION 载荷 ADD / UPDATE：enabled 且 action != 0 才显示。
func _on_interaction_changed(e: EntityData) -> void:
	if e == null:
		return
	_apply(e)


## 受控实体切换：立即按新实体的当前载荷刷新（没有就隐藏），不等下一次变化。
func _on_controlled_entity_changed(e: EntityData) -> void:
	_entity = e
	if e == null:
		set_data_visible(false)
		return
	_apply(e)


func _apply(e: EntityData) -> void:
	var hint: Variant = e.get_component(BevyEnums.Payload.INTERACTION)
	if not (hint is Dictionary):
		set_data_visible(false)
		return
	var data: Dictionary = hint as Dictionary
	var action := int(data.get("action", 0))
	var enabled := bool(data.get("enabled", false))
	set_data_visible(enabled and action != 0)


## UiRoot 暴露 client 属性 / get_client()；都不存在时返回 null（headless 安全）。
func _resolve_client() -> Object:
	if ui_root == null:
		return null
	if ui_root.has_method("get_client"):
		return ui_root.get_client()
	var c: Variant = ui_root.get("client")
	return c if c != null else null
