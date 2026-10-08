class_name PayloadCodec
extends RefCounted
## [data] 层：schema 驱动的线格式解码器。**不认识节点**，只把 SoA 池解成
## EntityData 的通用组件值。
##
## - apply / clear：按 BevyEnums.PAYLOAD_SCHEMA 解码 / 清除；
## - is_active：该实体是否还有需要每帧插值的载荷；
## - sample_position / sample_yaw：给 ViewLayer 的无分配插值采样；
## - register_ext_handler：扩展袋字段的快捷订阅（原 ExtensionApplier.register）。
##
## 新增载荷只需要 BevyEnums.PAYLOAD_SCHEMA 一行（+ Rust 侧）。插值载荷的
## prev/curr 原始 f32 存在本类内部槽池，EntityData 只拿当前采样值。

var _ext_handlers: Dictionary[int, Callable] = {}
## kind -> InterpBuffer（仅 schema 标了 interp 的载荷）。
var _interp: Dictionary = {}


## 解析一条 ADD / UPDATE 的 payload。
func apply(e: EntityData, kind: int, c: CommandCursor) -> void:
	# 载荷 code 不是连续上界：code 7（已删除的 RectList）留空，RAWVOXELS = 8，
	# 而 PAYLOAD_COUNT 是「条目数」(8)。用 `kind >= PAYLOAD_COUNT` 判界会把
	# RAWVOXELS(8) 整条丢弃 -> 实体只剩 TRANSFORM，ViewRules 匹配不到 -> nodes=0。
	# 未知 code 交给下面的 schema 查询兜底。
	if kind < 0 or kind >= BevyEnums.PAYLOAD_NONE:
		return
	var schema: Dictionary = BevyEnums.PAYLOAD_SCHEMA.get(kind, {})
	if schema.is_empty():
		return
	if bool(schema.get("interp", false)):
		_apply_interp(e, kind, c, schema)
		return
	var decoded: Variant = _decode_wire(e, c, schema)
	if decoded == null:
		# 未知 tag 等硬错误：整条命令拒绝，保留旧值。
		return
	e.set_component(kind, decoded)


## 清除一个组件（REMOVE / 实体回收）；插值载荷额外回收槽位。
func clear(e: EntityData, kind: int) -> void:
	var schema: Dictionary = BevyEnums.PAYLOAD_SCHEMA.get(kind, {})
	if bool(schema.get("interp", false)):
		var buf: InterpBuffer = _interp.get(kind)
		if buf != null:
			buf.free_slot(e.stable_id)
	e.clear_component(kind)


## 该实体是否仍有需要每帧驱动的插值载荷（prev != curr）。
func is_active(e: EntityData) -> bool:
	for kind in _interp:
		if not e.has_component(int(kind)):
			continue
		var buf: InterpBuffer = _interp[kind]
		if buf != null and buf.is_active(e.stable_id):
			return true
	return false


## 无分配：按 alpha 在 prev/curr 之间采样位置。未知实体返回 Vector3.ZERO。
func sample_position(e: EntityData, kind: int, alpha: float) -> Vector3:
	var buf: InterpBuffer = _interp.get(kind)
	if buf == null:
		return Vector3.ZERO
	return buf.sample_position(e.stable_id, alpha)


## 无分配：按 alpha 在 prev/curr 之间采样 yaw。
func sample_yaw(e: EntityData, kind: int, alpha: float) -> float:
	var buf: InterpBuffer = _interp.get(kind)
	if buf == null:
		return 0.0
	return buf.sample_yaw(e.stable_id, alpha)


## 订阅扩展袋字段（回调：func(e: EntityData, tag: int, value: Variant)）。
func register_ext_handler(key: int, handler: Callable) -> void:
	_ext_handlers[key] = handler


## 便利：读取当前 EXT 字段值（组件值 shape：{"fields": {...}}）。
func ext_field(e: EntityData, key: int) -> Variant:
	var value = e.get_component(BevyEnums.Payload.EXT)
	if value is Dictionary:
		var fields: Dictionary = value.get(BevyEnums.EXT_FIELD, {})
		return fields.get(key)
	return null


# ───────────────────────── 通用 wire 解码 ─────────────────────────

func _decode_wire(e: EntityData, c: CommandCursor, schema: Dictionary) -> Variant:
	var wire: Array = schema["wire"]
	var fields: Array = schema["fields"]
	var value: Dictionary = {}
	var f32i := 0
	var i32i := 0
	for token_index in wire.size():
		var token: int = wire[token_index]
		var name: String = String(fields[token_index])
		match token:
			BevyEnums.Wire.F32, BevyEnums.Wire.ANGLE:
				value[name] = float(c.f32_pool[c.f32_off + f32i])
				f32i += 1
			BevyEnums.Wire.VEC3:
				value[name] = Vector3(
					c.f32_pool[c.f32_off + f32i],
					c.f32_pool[c.f32_off + f32i + 1],
					c.f32_pool[c.f32_off + f32i + 2]
				)
				f32i += 3
			BevyEnums.Wire.I32:
				value[name] = int(c.i32_pool[c.i32_off + i32i])
				i32i += 1
			BevyEnums.Wire.BOOL:
				value[name] = c.i32_pool[c.i32_off + i32i] != 0
				i32i += 1
			BevyEnums.Wire.TAGS:
				var n := int(c.i32_pool[c.i32_off + i32i])
				i32i += 1
				value[name] = _read_tags(c.i32_pool, c.i32_off + i32i, n)
				i32i += n
			BevyEnums.Wire.BYTES:
				# 原始体素 halo：i32 池 = [byte_len, chunk_count, chunk...]，每个 chunk
				# 是 8 字节小端打包的 u64（Rust 侧 as i64 推入，最高位可能为负）。
				# 逐字节取 chunk 的第 (j & 7) 个小端字节；chunk 下标 = j >> 3。
				var byte_len := int(c.i32_pool[c.i32_off + i32i])
				var chunk_count := int(c.i32_pool[c.i32_off + i32i + 1])
				if byte_len < 0 or chunk_count < 0:
					return null
				i32i += 2
				var blocks := PackedByteArray()
				blocks.resize(byte_len)
				for j in byte_len:
					var chunk := int(c.i32_pool[c.i32_off + i32i + (j >> 3)])
					blocks[j] = (chunk >> ((j & 7) * 8)) & 0xFF
				i32i += chunk_count
				value[name] = blocks
			BevyEnums.Wire.EXT_BAG:
				var bag: Variant = _decode_ext(e, c, c.i32_off + i32i)
				if bag == null:
					return null
				value[name] = bag
				i32i = c.i32_cnt
			_:
				return null
	return value


# ───────────────────────── EXT 袋 ─────────────────────────

func _decode_ext(e: EntityData, c: CommandCursor, start: int) -> Variant:
	var p := c.i32_pool
	var cur := start
	var count := int(p[cur])
	cur += 1
	var fields: Dictionary = {}
	for _field in count:
		var key := int(p[cur])
		var tag := int(p[cur + 1])
		cur += 2
		var value: Variant
		match tag:
			BevyEnums.EXT_TAG_I32:
				value = int(p[cur])
				cur += 1
			BevyEnums.EXT_TAG_BOOL:
				value = p[cur] != 0
				cur += 1
			BevyEnums.EXT_TAG_TAGS:
				var n := int(p[cur])
				cur += 1
				value = _read_tags(p, cur, n)
				cur += n
			_:
				# 未知 tag：Rust 解码会整帧拒绝，这里也放弃本组件。
				push_warning("[PayloadCodec] 未知 EXT tag=%d，丢弃实体 %d 的 EXT 组件" % [tag, e.stable_id])
				return null
		fields[key] = value
		var h: Callable = _ext_handlers.get(key, Callable())
		if h.is_valid():
			h.call(e, tag, value)
	return fields


static func _read_tags(src: PackedInt64Array, start: int, n: int) -> PackedInt32Array:
	var tags := PackedInt32Array()
	if n <= 0:
		return tags
	tags.resize(n)
	for j in n:
		tags[j] = int(src[start + j])
	return tags


# ───────────────────────── 插值载荷 ─────────────────────────

func _apply_interp(e: EntityData, kind: int, c: CommandCursor, schema: Dictionary) -> void:
	var buf: InterpBuffer = _interp.get(kind)
	if buf == null:
		buf = InterpBuffer.new(schema)
		_interp[kind] = buf
	buf.write(c.f32_pool, c.f32_off, e.stable_id)
	# EntityData 只暴露当前采样值；prev/curr 原始 f32 留在 buf 池里。
	# 复用同一个 Dictionary 原地更新：首次分配后热路径不再逐实体分配。
	var value = e.get_component(kind)
	if value is Dictionary:
		value["position"] = buf.current_position(e.stable_id)
		value["yaw"] = buf.current_yaw(e.stable_id)
		e.set_component(kind, value)
	else:
		e.set_component(kind, {
			"position": buf.current_position(e.stable_id),
			"yaw": buf.current_yaw(e.stable_id),
		})


## 每个插值载荷一个独立槽池：stride * samples 个 f32 / slot。
## 约定：schema["fields"][i] 命名 wire[i]；fields 之外的尾部 token 视为纠正偏移。
class InterpBuffer:
	var stride: int = 1
	var samples: int = 1
	var position_off: int = 0
	var yaw_off: int = 0
	var correction_off: int = -1
	var slot_by_id: Dictionary[int, int] = {}
	var pool := PackedFloat32Array()
	var slot_count: int = 0
	var free: Array[int] = []

	func _init(schema: Dictionary) -> void:
		stride = int(schema.get("stride", 1))
		samples = int(schema.get("samples", 1))
		var wire: Array = schema.get("wire", [])
		var fields: Array = schema.get("fields", [])
		var off := 0
		for i in wire.size():
			if i < fields.size():
				match String(fields[i]):
					"position":
						position_off = off
					"yaw":
						yaw_off = off
			off += wire_size(int(wire[i]))
		if fields.size() < wire.size():
			var extra_off := 0
			for i in fields.size():
				extra_off += wire_size(int(wire[i]))
			correction_off = extra_off

	static func wire_size(token: int) -> int:
		match token:
			BevyEnums.Wire.VEC3:
				return 3
			BevyEnums.Wire.F32, BevyEnums.Wire.ANGLE, BevyEnums.Wire.I32, BevyEnums.Wire.BOOL:
				return 1
			_:
				return 0

	func ensure_slot(id: int) -> int:
		var existing: int = slot_by_id.get(id, -1)
		if existing >= 0:
			return existing
		var slot: int
		if free.is_empty():
			slot = slot_count
			slot_count += 1
			pool.resize((slot + 1) * stride * samples)
		else:
			slot = free.pop_back()
		slot_by_id[id] = slot
		return slot

	func free_slot(id: int) -> void:
		if slot_by_id.has(id):
			free.append(slot_by_id[id])
			slot_by_id.erase(id)

	func base(slot: int) -> int:
		return slot * stride * samples

	func write(src: PackedFloat32Array, off: int, id: int) -> void:
		var slot := ensure_slot(id)
		var b := base(slot)
		var total := stride * samples
		for k in total:
			pool[b + k] = src[off + k]

	func is_active(id: int) -> bool:
		var slot: int = slot_by_id.get(id, -1)
		if slot < 0:
			return false
		var b := base(slot)
		for k in stride:
			if pool[b + k] != pool[b + stride + k]:
				return true
		return false

	func current_position(id: int) -> Vector3:
		var slot: int = slot_by_id.get(id, -1)
		if slot < 0:
			return Vector3.ZERO
		return vec3_at(base(slot) + (samples - 1) * stride, position_off)

	func current_yaw(id: int) -> float:
		var slot: int = slot_by_id.get(id, -1)
		if slot < 0:
			return 0.0
		return pool[base(slot) + (samples - 1) * stride + yaw_off]

	func sample_position(id: int, alpha: float) -> Vector3:
		var slot: int = slot_by_id.get(id, -1)
		if slot < 0:
			return Vector3.ZERO
		var b := base(slot)
		var curr := b + (samples - 1) * stride
		var sampled := vec3_at(b, position_off).lerp(vec3_at(curr, position_off), alpha)
		if correction_off >= 0:
			sampled += vec3_at(b, correction_off).lerp(vec3_at(curr, correction_off), alpha)
		return sampled

	func sample_yaw(id: int, alpha: float) -> float:
		var slot: int = slot_by_id.get(id, -1)
		if slot < 0:
			return 0.0
		var b := base(slot)
		return lerp_angle(
			pool[b + yaw_off],
			pool[b + (samples - 1) * stride + yaw_off],
			alpha
		)

	func vec3_at(b: int, off: int) -> Vector3:
		return Vector3(pool[b + off], pool[b + off + 1], pool[b + off + 2])
