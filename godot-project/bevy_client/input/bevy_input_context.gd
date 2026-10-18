class_name BevyInputContext
extends RefCounted
## 本地玩家 / 当前被操纵对象的输入能力上下文（表现侧只读）。
##
## 数据来源：INPUTCAPS 载荷。载荷只在字段真正变化时下发，所以这里只订阅、不轮询。
## - present   = 能力组件并集（「有没有这个能力」）；
## - available = 经 InputDisabled / Stunned / Rooted 削减后的可用位；
## - source    = ControlledBy.source（0 = 无控制源哨兵）；
## - local     = 实体是否挂 LocalPlayer；
## - look      = 是否挂 LookChannel（视角是独立轴，不占动作位）。
##
## 单本地玩家假设：`local` 直接来自 `LocalPlayer` 标记，一个 context 只跟踪一个
## 实体。分屏 / 多玩家时应按 `source` 扩展（每个 source 一份 context，或把单例
## 改成字典）；本轮不做移动端 UI / 输入 scheme。

signal controlled_changed(entity: EntityData)
signal capabilities_changed(present: int, available: int)
## 视角轴（LookChannel）开关；视角不占动作位，所以单独一个信号。
signal look_changed(available: bool)
signal channel_changed(key: String, present: bool, available: bool)

## 发起订阅的 BevyClient（不标注 Node：它承载的是脚本方法，避免静态分析误判）。
var _client = null
## 当前受控的本地实体；null 表示没有本地控制对象。
var _entity: EntityData = null
## 当前能力位 / 可用位（来自 INPUTCAPS）。
var _present: int = 0
var _available: int = 0
## 视角轴是否开放（LookChannel）。
var _look: bool = false
## 通道表：Array[{ key, mask, mode, actions }]，来自扩展 get_input_channels()。
var _channels: Array = []
## key -> mask（通道级位判断用，避免每次线性查表）。
var _mask_by_key: Dictionary[String, int] = {}
## key -> [present, available] 上一次状态（channel_changed 的边沿去抖）。
var _channel_state: Dictionary[String, Array] = {}


## 绑定客户端：拉一次通道表，订阅 INPUTCAPS 与实体移除。
func bind(client) -> void:
	_client = client
	if _client == null:
		return

	_channels = []
	_mask_by_key.clear()
	_channel_state.clear()
	if _client.has_method("input_channels"):
		_channels = _client.input_channels()
	for row: Dictionary in _channels:
		var key := String(row.get("key", ""))
		if key.is_empty():
			continue
		_mask_by_key[key] = int(row.get("mask", 0))
		_channel_state[key] = [false, false]

	if _client.has_method("listen_component"):
		_client.listen_component(BevyEnums.Payload.INPUTCAPS, _on_caps_changed)
	if _client.has_signal("entity_removed"):
		_client.entity_removed.connect(_on_entity_removed)

	_clear()


## 后端重启 / 停止时复位（保留通道表，重启后可复用）。
func reset() -> void:
	_clear()


func controlled_entity() -> EntityData:
	return _entity


func present_mask() -> int:
	return _present


func available_mask() -> int:
	return _available


func channels() -> Array:
	return _channels


## 该通道的能力是否存在（present 覆盖通道全部动作位）。
func has_channel(key: String) -> bool:
	var mask: int = _mask_by_key.get(key, 0)
	return mask != 0 and (_present & mask) == mask


## 该通道当前是否可用（available 覆盖通道全部动作位）。
func is_channel_available(key: String) -> bool:
	var mask: int = _mask_by_key.get(key, 0)
	return mask != 0 and (_available & mask) == mask


## 视角轴是否开放。视角不在 channels() 里（它不占动作位），UI 单独判断。
func has_look() -> bool:
	return _look


## 视角轴当前是否可用（与 has_look 同义，保留 UI 侧统一命名）。
func is_look_available() -> bool:
	return _look


# ───────────────────────── 载荷订阅 ─────────────────────────

func _on_caps_changed(e: EntityData) -> void:
	var caps: Variant = e.get_component(BevyEnums.Payload.INPUTCAPS)
	if not (caps is Dictionary):
		return
	var data: Dictionary = caps as Dictionary

	# local = 是否挂 LocalPlayer；非本地玩家不进入本上下文
	#（它恰好是当前实体 = 刚失去 LocalPlayer 标记，清零）。
	if not bool(data.get("local", false)):
		if e == _entity:
			_clear()
		return

	var present := int(data.get("present", 0))
	var available := int(data.get("available", 0))
	var look := bool(data.get("look", false))

	if e != _entity:
		_entity = e
		_present = present
		_available = available
		_look = look
		# 新实体从「无」开始算通道边沿，避免沿用旧实体的状态。
		_reset_channel_state()
		controlled_changed.emit(e)
		capabilities_changed.emit(present, available)
		look_changed.emit(look)
		_emit_channel_changes(present, available)
		return

	if present == _present and available == _available and look == _look:
		return

	var masks_changed := present != _present or available != _available
	var look_flipped := look != _look
	_present = present
	_available = available
	_look = look
	if masks_changed:
		capabilities_changed.emit(present, available)
	if look_flipped:
		look_changed.emit(look)
	_emit_channel_changes(present, available)


## 把通道状态全部置为「无」；只在切换实体时用（不在这里发信号）。
func _reset_channel_state() -> void:
	for row: Dictionary in _channels:
		var key := String(row.get("key", ""))
		if not key.is_empty():
			_channel_state[key] = [false, false]


## 通道级边沿：按通道表（固定顺序）计算，只有翻转时才发信号。
func _emit_channel_changes(present: int, available: int) -> void:
	for row: Dictionary in _channels:
		var key := String(row.get("key", ""))
		var mask := int(row.get("mask", 0))
		if key.is_empty() or mask == 0:
			continue
		var channel_present := (present & mask) == mask
		var channel_available := (available & mask) == mask
		var prev: Array = _channel_state.get(key, [false, false])
		if prev[0] == channel_present and prev[1] == channel_available:
			continue
		_channel_state[key] = [channel_present, channel_available]
		channel_changed.emit(key, channel_present, channel_available)


func _on_entity_removed(e: EntityData, _reason: int) -> void:
	if e == _entity:
		_clear()


func _clear() -> void:
	var had_entity := _entity != null
	_entity = null
	var had_caps := _present != 0 or _available != 0
	var had_look := _look
	_present = 0
	_available = 0
	_look = false
	if had_entity:
		controlled_changed.emit(null)
	if had_caps:
		capabilities_changed.emit(0, 0)
	if had_look:
		look_changed.emit(false)
	# 通道全部回落到「无」；即使位图本来就是 0，也要复位通道状态
	#（后端重启后复用同一 context 时不残留陈旧边沿）。
	for row: Dictionary in _channels:
		var key := String(row.get("key", ""))
		if key.is_empty():
			continue
		var prev: Array = _channel_state.get(key, [false, false])
		if prev[0] == false and prev[1] == false:
			continue
		_channel_state[key] = [false, false]
		channel_changed.emit(key, false, false)
