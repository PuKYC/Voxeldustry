class_name EntityData
extends RefCounted
## 纯数据对象：stable_id -> 通用组件槽。**不碰节点**，也不知道任何具体载荷。
##
## 数据 / 视图彻底分离：
## - PayloadCodec 的 apply / clear 只写这里（以及注册表索引）；
## - 显示节点由 ViewLayer 依据 ViewRules 在帧末惰性 acquire / present / release；
## - set_component / clear_component 只存值并发信号，不再调用任何节点钩子。

signal component_changed(kind: int)
signal component_removed(kind: int)

var stable_id: int = 0
var list_index: int = -1
var active_index: int = -1
var last_tick: int = 0

var _components: Dictionary = {}
var _kinds: Array[int] = []
## 自上次 take_dirty_kinds() 起被写过 / 清除过的 kind（保序去重）。
var _dirty: Array[int] = []
var _dirty_set: Dictionary[int, bool] = {}


## 通用组件槽：PayloadCodec 唯一的写入口。
func set_component(kind: int, value: Variant) -> void:
	if not _components.has(kind):
		_kinds.append(kind)
	_components[kind] = value
	_mark_dirty(kind)
	component_changed.emit(kind)


func get_component(kind: int) -> Variant:
	return _components.get(kind)


func has_component(kind: int) -> bool:
	return _components.has(kind)


## 清除组件并发出 component_removed；视图侧下一帧同步时恢复默认呈现。
func clear_component(kind: int) -> void:
	if _components.erase(kind):
		_kinds.erase(kind)
		_mark_dirty(kind)
		component_removed.emit(kind)


## 兼容旧调用名（等价 clear_component）。
func remove_component(kind: int) -> void:
	clear_component(kind)


## 内部握手：取走自上次调用起变化的 kind 并清空。**只允许 ViewLayer 帧末调用一次**；
## 业务侧请用 component_changed 信号，别调这里，否则会截走 ViewLayer 的 kind 通知。
func take_dirty_kinds() -> Array[int]:
	var out: Array[int] = _dirty
	_dirty.clear()
	_dirty_set.clear()
	return out


## 只读查看（不清空）。
func dirty_kinds() -> Array[int]:
	return _dirty


## 已写入的组件 kind 列表（内部引用，只读遍历，避免每帧分配）。
func component_kinds() -> Array[int]:
	return _kinds


func _mark_dirty(kind: int) -> void:
	if not _dirty_set.has(kind):
		_dirty_set[kind] = true
		_dirty.append(kind)
