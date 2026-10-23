class_name TouchRegionRegistry
extends RefCounted
## 触屏区域登记表：把摇杆 / 视角区 / 动作按钮等触摸响应矩形登记下来，
## 汇总成「HUD 槽位需要避让的 inset」，供 HudSlot.set_inset() 使用。
##
## 单位是设计单位（逻辑像素）；本类不做坐标变换、不持有节点、不监听输入，
## 只做「命名矩形的增删改查 + 各槽位每边取 max」。
##
## reserves 形如 {"bottom_begin": Vector4(left, top, right, bottom)}，
## key 与 HudSlot 的 key（top_begin / mid_end / bottom_center ...）一致。

var _regions: Dictionary = {}


## 登记一个触摸区域（同名覆盖）。reserves 为该区域需要 HUD 槽位避让的 inset。
func register(name: StringName, rect: Rect2, reserves: Dictionary = {}) -> void:
	_regions[name] = {
		"rect": rect,
		"reserves": reserves.duplicate(true),
	}


## 更新一个已登记区域；未登记时等价于 register。
func update(name: StringName, rect: Rect2, reserves: Dictionary = {}) -> void:
	register(name, rect, reserves)


func release(name: StringName) -> void:
	_regions.erase(name)


func clear() -> void:
	_regions.clear()


## name -> {"rect": Rect2, "reserves": Dictionary} 的副本。
func regions() -> Dictionary:
	return _regions.duplicate(true)


## slot_key -> Vector4：同一槽位下所有区域给定 inset 每边取最大值。
func slot_insets() -> Dictionary:
	var result: Dictionary = {}
	for key in _regions:
		var entry: Dictionary = _regions[key]
		var reserves: Dictionary = entry.get("reserves", {})
		for slot_key in reserves:
			var value := _to_vector4(reserves[slot_key])
			var current: Vector4 = result.get(slot_key, Vector4.ZERO)
			result[slot_key] = Vector4(
				maxf(current.x, value.x),
				maxf(current.y, value.y),
				maxf(current.z, value.z),
				maxf(current.w, value.w)
			)
	return result


## 矩形相交诊断（可选）。
## pairs 可以是 [[a, b], ...] 显式配对，也可以是一组名字（自动两两配对）；
## 返回实际相交的配对数组 [[a, b], ...]。
func overlapping(pairs: Array) -> Array:
	var result: Array = []
	var explicit: Array = []
	var names: Array[StringName] = []
	for entry in pairs:
		if entry is Array:
			var pair: Array = entry
			if pair.size() >= 2:
				explicit.append([StringName(pair[0]), StringName(pair[1])])
		else:
			names.append(StringName(entry))
	for pair in explicit:
		var a: StringName = pair[0]
		var b: StringName = pair[1]
		if _intersects(a, b):
			result.append([a, b])
	for i in names.size():
		for j in range(i + 1, names.size()):
			if _intersects(names[i], names[j]):
				result.append([names[i], names[j]])
	return result


func _intersects(a: StringName, b: StringName) -> bool:
	if not _regions.has(a) or not _regions.has(b):
		return false
	var rect_a: Rect2 = _regions[a]["rect"]
	var rect_b: Rect2 = _regions[b]["rect"]
	return rect_a.intersects(rect_b)


## 把 Vector4 / Array[4] / Vector2 规范成 Vector4，每边负数夹到 0。
func _to_vector4(raw: Variant) -> Vector4:
	var value := Vector4.ZERO
	if raw is Vector4:
		value = raw
	elif raw is Array and (raw as Array).size() == 4:
		var array: Array = raw
		value = Vector4(float(array[0]), float(array[1]), float(array[2]), float(array[3]))
	elif raw is Vector2:
		var vector: Vector2 = raw
		value = Vector4(vector.x, vector.y, 0.0, 0.0)
	return Vector4(
		maxf(0.0, value.x),
		maxf(0.0, value.y),
		maxf(0.0, value.z),
		maxf(0.0, value.w)
	)
