class_name ViewLayer
extends RefCounted
## [view] 层：唯一持有 id -> 节点 的地方，负责物化 / 刷新 / 回收 / 呈现。
##
## 数据在 EntityData（由 PayloadCodec 解码）；本层只做「怎么演」：
## - sync(data)：按 ViewRules 求值一次，惰性 acquire / present / unbind；
## - present 只走 schema 里 view != NONE 的载荷，按 Wire/View 枚举分派；
## - drive：插值载荷每渲染帧采样到节点。
##
## 不特判具体 Payload，也不用 node.set_meta 承载组件值（读 EntityData）。

var _factory: EntityFactory
var _codec: PayloadCodec
var _rules: ViewRules
## stable_id -> 显示节点（仅在规则满足时存在）。
var _nodes: Dictionary[int, BevyEntityNode] = {}


func _init(factory: EntityFactory, codec: PayloadCodec, rules: ViewRules = null) -> void:
	_factory = factory
	_codec = codec
	_rules = rules if rules != null else ViewRules.new()


## 帧末：对本次被命令触及的实体求值一次规则，同步节点。
func sync(data: EntityData) -> void:
	var changed := data.take_dirty_kinds()
	var rule := _rules.match_rule(data)
	var node: BevyEntityNode = _nodes.get(data.stable_id)
	if rule.is_empty():
		if node != null:
			unbind(data)
		return
	if node == null:
		_acquire(data, rule)
		return
	_present_all(data, node)
	# 只把本帧变化的 kind 通知给节点钩子。
	for kind in changed:
		if data.has_component(int(kind)):
			node.on_component_changed(int(kind))
		else:
			node.on_component_removed(int(kind))


## 规则不再满足：保留数据，只回收节点（on_unbound）。
func unbind(data: EntityData) -> void:
	var node: BevyEntityNode = _nodes.get(data.stable_id)
	if node == null:
		return
	_nodes.erase(data.stable_id)
	# 先跑钩子（此时 node.data 仍可读），再回池复位。
	node.on_unbound()
	_factory.release(node)


## DETACH / DESPAWN：节点钩子 + 回收（数据侧由 dispatcher 调 codec.clear）。
func release(data: EntityData, reason: int) -> void:
	var node: BevyEntityNode = _nodes.get(data.stable_id)
	if node == null:
		return
	node.on_removed(reason)
	for kind in data.component_kinds():
		node.on_component_removed(int(kind))
	node.data = null
	_nodes.erase(data.stable_id)
	_factory.release(node)


## 同帧重新 ATTACH：回收节点但**不触发任何钩子**（旧记录整体丢弃）。
func discard(data: EntityData) -> void:
	var node: BevyEntityNode = _nodes.get(data.stable_id)
	if node == null:
		return
	node.data = null
	_nodes.erase(data.stable_id)
	_factory.release(node)


## 按 alpha 驱动一批活动实体的插值呈现。
func drive(active: Array, alpha: float) -> void:
	for e in active:
		drive_entity(e, alpha)


## 驱动单个实体的插值呈现（interpolator 的 STALE_TICKS 落位也走这里）。
func drive_entity(data: EntityData, alpha: float) -> void:
	var node: BevyEntityNode = _nodes.get(data.stable_id)
	if node == null:
		return
	for kind in BevyEnums.PAYLOAD_SCHEMA:
		var schema: Dictionary = BevyEnums.PAYLOAD_SCHEMA[kind]
		if int(schema.get("view", BevyEnums.View.NONE)) != BevyEnums.View.TRANSFORM:
			continue
		if data.has_component(int(kind)):
			node.position = _codec.sample_position(data, int(kind), alpha)
			node.rotation.y = _codec.sample_yaw(data, int(kind), alpha)


## 供事件 / 业务层查当前显示节点（未物化返回 null）。
func node_of(data: EntityData) -> BevyEntityNode:
	return _nodes.get(data.stable_id)


func node_of_id(id: int) -> BevyEntityNode:
	return _nodes.get(id)


func node_count() -> int:
	return _nodes.size()


# ───────────────────────── 物化 / 呈现 ─────────────────────────

func _acquire(data: EntityData, rule: Dictionary) -> void:
	var build := int(rule.get("build", ViewRules.Build.PROTOTYPE_SCENE))
	var proto_id := _build_prototype_id(data, rule)
	var node := _factory.acquire(proto_id, build)
	_nodes[data.stable_id] = node
	node.data = data
	node.stable_id = data.stable_id
	node.prototype_id = proto_id
	_present_all(data, node)
	node.on_spawned()
	for kind in data.component_kinds():
		node.on_component_changed(int(kind))


func _build_prototype_id(data: EntityData, rule: Dictionary) -> int:
	match int(rule.get("build", ViewRules.Build.PROTOTYPE_SCENE)):
		ViewRules.Build.PROTOTYPE_SCENE:
			var proto = data.get_component(BevyEnums.Payload.PROTOTYPE)
			if proto is Dictionary:
				return int(proto.get(BevyEnums.PROTOTYPE_FIELD, 0))
	return 0


## 把 EntityData 里 schema 声明的 view 载荷刷进节点；缺失 view 载荷恢复默认。
func _present_all(data: EntityData, node: BevyEntityNode) -> void:
	for kind in BevyEnums.PAYLOAD_SCHEMA:
		var schema: Dictionary = BevyEnums.PAYLOAD_SCHEMA[kind]
		var view := int(schema.get("view", BevyEnums.View.NONE))
		if view == BevyEnums.View.NONE:
			continue
		if data.has_component(int(kind)):
			_present_view(data, node, int(kind), view)
		else:
			_reset_view(node, view)


func _present_view(data: EntityData, node: BevyEntityNode, kind: int, view: int) -> void:
	match view:
		BevyEnums.View.TRANSFORM:
			node.position = _codec.sample_position(data, kind, 1.0)
			node.rotation.y = _codec.sample_yaw(data, kind, 1.0)
		BevyEnums.View.VISIBLE:
			var value = data.get_component(kind)
			if value is Dictionary:
				node.visible = bool(value.get(_visible_field(kind), true))
		BevyEnums.View.RAWVOXEL:
			var raw_value = data.get_component(kind)
			if raw_value is Dictionary:
				node.set_raw_voxel_view(
					int(raw_value.get("lod", 0)),
					raw_value.get("blocks", PackedByteArray())
				)


## 可见字段名来自 schema，避免在视图层硬编码 "visible"。
func _visible_field(kind: int) -> String:
	var schema: Dictionary = BevyEnums.PAYLOAD_SCHEMA.get(kind, {})
	var fields: Array = schema.get("fields", [])
	if fields.is_empty():
		return "visible"
	return String(fields[0])


func _reset_view(node: BevyEntityNode, view: int) -> void:
	match view:
		BevyEnums.View.VISIBLE:
			node.visible = true
		BevyEnums.View.RAWVOXEL:
			node.reset_raw_voxel_view()
		_:
			pass
