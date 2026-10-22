class_name InteractPrompt
extends HudWidget
## 交互提示：当前有可用交互时显示英文文案（Label + 描边）。
##
## 数据源：BevyEnums.Payload.INTERACTION（fields: action, enabled）。
## enabled 且 action != 0 才显示。
## 限制：载荷目前只给 ActionId，没有动作显示名，所以不反查动作名
## （等 INTERACTION 契约扩展出显示名后再本地化；不为此改 Rust）。
## 无 client 时隐藏；全部信号驱动，不轮询、不写坐标。

const PROMPT_TEXT := "Interact"

## BevyClient；未 attach 时为 null。
var _client: Object = null
## 当前受控实体（用于切换时立即刷新 / 隐藏）。
var _entity: EntityData = null
var _label: Label = null


func _on_attach() -> void:
	_label = get_node_or_null("Prompt") as Label
	if _label != null:
		_label.text = PROMPT_TEXT
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
