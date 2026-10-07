class_name EventRouter
extends RefCounted
## 一次性表现事件 -> 信号；position 由 Rust 的 PackedFloat32Array 转成 Vector3。

signal play_sound(sound_id: int, position: Vector3)
signal trigger_anim(entity_id: int, anim_id: int, speed: float)
signal spawn_vfx(vfx_id: int, position: Vector3)
signal damage_popup(entity_id: int, amount: int)

func route(events: Array) -> void:
	for ev: Dictionary in events:
		match String(ev["type"]):
			"play_sound":
				play_sound.emit(int(ev["sound"]), _vec3(ev["position"]))
			"trigger_anim":
				trigger_anim.emit(int(ev["id"]), int(ev["anim"]), float(ev["speed"]))
			"spawn_vfx":
				spawn_vfx.emit(int(ev["vfx"]), _vec3(ev["position"]))
			"damage_popup":
				damage_popup.emit(int(ev["id"]), int(ev["amount"]))

static func _vec3(v: Variant) -> Vector3:
	if v is Vector3:
		return v
	if (v is PackedFloat32Array or v is Array) and v.size() >= 3:
		return Vector3(v[0], v[1], v[2])
	return Vector3.ZERO
