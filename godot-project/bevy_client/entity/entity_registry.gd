class_name EntityRegistry
extends RefCounted
## stable_id -> EntityData；全量列表 + 活动列表，均用 swap-remove 保持 O(1)。
## 注册表索引的是**纯数据对象**（EntityData）；显示节点由 ViewLayer 单独持有。

var _data: Dictionary[int, EntityData] = {}
var _list: Array[EntityData] = []
var _active: Array[EntityData] = []

func add(id: int, e: EntityData) -> void:
	e.stable_id = id
	e.list_index = _list.size()
	_data[id] = e
	_list.append(e)

func find(id: int) -> EntityData:
	return _data.get(id)

func remove(id: int) -> EntityData:
	var e: EntityData = _data.get(id)
	if e == null:
		return null
	_data.erase(id)
	set_active(e, false)
	var last: EntityData = _list[-1]
	_list[e.list_index] = last
	last.list_index = e.list_index
	_list.pop_back()
	e.list_index = -1
	return e

func set_active(e: EntityData, on: bool) -> void:
	if on and e.active_index < 0:
		e.active_index = _active.size()
		_active.append(e)
	elif not on and e.active_index >= 0:
		var last: EntityData = _active[-1]
		_active[e.active_index] = last
		last.active_index = e.active_index
		_active.pop_back()
		e.active_index = -1

func all() -> Array[EntityData]:
	return _list

func active() -> Array[EntityData]:
	return _active
