class_name HealthBar
extends HudWidget
## 受控实体血条：ProgressBar + 数值 Label。
##
## 数据源：BevyEnums.Payload.HEALTH（fields: current, max）。
## 只在「当前受控实体有 HEALTH 载荷」时显示；无 client / 无实体时隐藏。
## 全部信号驱动：client.listen_component + controlled_entity_changed，不轮询、不写坐标。

## 数值文案：英文，避免本机缺 CJK 字体时显示方块。
const VALUE_FORMAT := "%d / %d"

## BevyClient；未 attach 时为 null。
var _client: Object = null
## 当前受控实体（controlled_entity_changed 的最近值）。
var _entity: EntityData = null
var _bar: ProgressBar = null
var _value_label: Label = null


func _on_attach() -> void:
	_resolve_nodes()
	_client = _resolve_client()
	set_data_visible(false)
	if _client == null:
		return
	if _client.has_method("listen_component"):
		_client.listen_component(BevyEnums.Payload.HEALTH, _on_health_changed)
	if _client.has_signal("controlled_entity_changed"):
		_client.connect(&"controlled_entity_changed", _on_controlled_entity_changed)
	# 首帧同步一次：注册时若已有受控实体，不必等下一次载荷变化才显示。
	_sync_from_context()


## HEALTH 载荷 ADD / UPDATE：任何实体都会到达，只认当前受控实体。
## 受控实体未知时先接受（注册早于 controlled_entity_changed 的窗口期）。
func _on_health_changed(e: EntityData) -> void:
	if e == null:
		return
	if _entity != null and e != _entity:
		return
	_apply(e)


## 受控实体切换：整体重置，避免沿用上一个实体的血量。
func _on_controlled_entity_changed(e: EntityData) -> void:
	_entity = e
	if e == null:
		set_data_visible(false)
		return
	_apply(e)


func _apply(e: EntityData) -> void:
	var health: Variant = e.get_component(BevyEnums.Payload.HEALTH)
	if not (health is Dictionary):
		# 「当前实体没有 HEALTH 载荷」= 不显示血条。
		set_data_visible(false)
		return
	var data: Dictionary = health as Dictionary
	var current := float(data.get("current", 0.0))
	var maximum := float(data.get("max", 0.0))
	if _bar != null:
		# ProgressBar 的 max 必须 > 0；display 仍用真实 max 显示。
		_bar.max_value = maxf(maximum, 1.0)
		_bar.value = clampf(current, 0.0, maxf(maximum, 1.0))
	if _value_label != null:
		_value_label.text = VALUE_FORMAT % [int(round(current)), int(round(maximum))]
	set_data_visible(true)


func _sync_from_context() -> void:
	var ctx: Variant = _client.get("input_context")
	if ctx != null and ctx.has_method("controlled_entity"):
		_on_controlled_entity_changed(ctx.controlled_entity())


func _resolve_nodes() -> void:
	_bar = get_node_or_null("Bar") as ProgressBar
	_value_label = get_node_or_null("Value") as Label


## UiRoot 暴露 client 属性 / get_client()；都不存在时返回 null（headless 安全）。
func _resolve_client() -> Object:
	if ui_root == null:
		return null
	if ui_root.has_method("get_client"):
		return ui_root.get_client()
	var c: Variant = ui_root.get("client")
	return c if c != null else null
