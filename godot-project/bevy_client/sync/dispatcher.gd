class_name PresentationDispatcher
extends RefCounted
## 命令流 -> EntityData 组件槽 + 帧末视图同步。只有这里认识 command kind /
## payload_kind 的数值；解释工作全部托管给 PayloadCodec（数据）与 ViewLayer（视图）。
##
## 数据 / 视图分离：命令只改 EntityData；帧末对本帧 touched 的 id 调
## view_layer.sync 求值一次物化规则（ViewRules），惰性 acquire / refresh / release。

signal entity_attached(entity: EntityData)
signal entity_released(entity: EntityData, reason: int)
signal entity_despawned(id: int)
signal entity_state_changed(entity: EntityData)

var _registry: EntityRegistry
var _codec: PayloadCodec
var _view_layer: ViewLayer
var _listeners: Dictionary[int, Array] = {}
var _cursor := CommandCursor.new()
## 本帧被命令触及的 id（保序去重），帧末据此求值物化规则。
var _touched: Array[int] = []


func _init(
	reg: EntityRegistry,
	fac: EntityFactory,
	codec: PayloadCodec = null,
	view_layer: ViewLayer = null
) -> void:
	_registry = reg
	_codec = codec if codec != null else PayloadCodec.new()
	_view_layer = view_layer if view_layer != null else ViewLayer.new(fac, _codec)


func codec() -> PayloadCodec:
	return _codec


func view_layer() -> ViewLayer:
	return _view_layer


## 按 kind 订阅变更：callable 收到 (entity: EntityData)。
func listen(kind: int, callable: Callable) -> void:
	if not _listeners.has(kind):
		_listeners[kind] = []
	_listeners[kind].append(callable)


func apply(f: Dictionary) -> void:
	var n: int = f["command_count"]
	var tick: int = f["tick"]
	var kinds: PackedInt32Array = f["kinds"]
	var ids: PackedInt64Array = f["entity_ids"]
	var pkinds: PackedInt32Array = f["payload_kinds"]
	_cursor.bind(f)

	for i in n:
		var id := int(ids[i])
		match kinds[i]:
			BevyEnums.Cmd.ATTACH:
				# 重新 Attach = 干净记录：单槽合并可能吃掉中间的 Detach
				# （coalesce_commands 用「最新 Attach 清掉更早终结」表达这一点），
				# 客户端若还留着旧记录，必须整体丢弃，否则未被重发的旧组件会残留。
				var prior := _registry.find(id)
				if prior != null:
					_discard(prior)
					_registry.remove(id)
				var data := EntityData.new()
				_registry.add(id, data)
				entity_attached.emit(data)
				_mark(id)
			BevyEnums.Cmd.ADD, BevyEnums.Cmd.UPDATE:
				var e := _registry.find(id)
				if e == null:
					continue
				var pk := int(pkinds[i])
				_cursor.at(i)
				_codec.apply(e, pk, _cursor)
				e.last_tick = tick
				_registry.set_active(e, _codec.is_active(e))
				_notify_listeners(pk, e)
				entity_state_changed.emit(e)
				_mark(id)
			BevyEnums.Cmd.REMOVE:
				var rem := _registry.find(id)
				if rem == null:
					continue
				var rk := int(pkinds[i])
				_codec.clear(rem, rk)
				_registry.set_active(rem, _codec.is_active(rem))
				_mark(id)
			BevyEnums.Cmd.DETACH:
				_release(id, BevyEnums.RemovalReason.LEFT_VIEW, false)
			BevyEnums.Cmd.DESPAWN:
				_release(id, BevyEnums.RemovalReason.DESPAWNED, true)

	# 帧末：对本帧 touched 的 id 让 ViewLayer 求值一次物化规则。
	for id in _touched:
		var e := _registry.find(id)
		if e == null:
			continue
		_view_layer.sync(e)
		# 活动 = 有插值数据 **且** 真有节点可驱动；解绑后立即停掉每帧驱动。
		_registry.set_active(e, _codec.is_active(e) and _view_layer.node_of(e) != null)
	_touched.clear()


## 每渲染帧驱动：ViewLayer 按 alpha 采样到节点。
func drive(active: Array, alpha: float) -> void:
	_view_layer.drive(active, alpha)


## 单个实体驱动（interpolator 的 STALE_TICKS 落位用）。
func drive_entity(e: EntityData, alpha: float) -> void:
	_view_layer.drive_entity(e, alpha)


func _mark(id: int) -> void:
	if not _touched.has(id):
		_touched.append(id)


func _notify_listeners(kind: int, e: EntityData) -> void:
	if _listeners.is_empty():
		return
	var cbs: Array = _listeners.get(kind, [])
	for cb: Callable in cbs:
		cb.call(e)


## 丢弃一个已有数据记录但**不发 entity_released**：用于同帧「重新 Attach」前把旧
## 记录清干净（节点回收 + 逐 kind 清 codec 状态），随后由调用方从注册表移除。
func _discard(e: EntityData) -> void:
	_view_layer.discard(e)
	# 遍历副本，避免 clear 内部边删边迭代。
	var kinds := e.component_kinds().duplicate()
	for kind in kinds:
		_codec.clear(e, int(kind))
	_registry.set_active(e, false)


## DETACH / DESPAWN 共用：先跑实体钩子，再逐 kind 清数据并回收显示节点。
func _release(id: int, reason: int, despawn: bool) -> void:
	var e := _registry.remove(id)
	if e == null:
		return
	entity_released.emit(e, reason)
	_view_layer.release(e, reason)
	# 释放 codec 侧按实体保存的状态；遍历副本，避免 clear 内部边删边迭代。
	var kinds := e.component_kinds().duplicate()
	for kind in kinds:
		_codec.clear(e, int(kind))
	if despawn:
		entity_despawned.emit(id)
