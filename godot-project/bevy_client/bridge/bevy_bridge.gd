class_name BevyBridge
extends RefCounted
## 唯一认识 BevyAppManager 的地方：取帧 / 取事件 / 采样 alpha。

var _mgr: Object
var channel: int = BevyEnums.Channel.PACKED_STREAMS

func _init(mgr: Object) -> void:
	_mgr = mgr

## 空字典 = 本 tick 无新帧（先 take 再判空，避免对空字节 parse 刷 warning）。
func take_frame() -> Dictionary:
	if channel == BevyEnums.Channel.DICTIONARY:
		return _mgr.take_presentation_frame()
	# 快通道：新扩展直接给 SoA + 帧头，省掉字节拷贝与解码。
	if channel == BevyEnums.Channel.PACKED_STREAMS and _mgr.has_method("take_packed_streams"):
		return _mgr.take_packed_streams()
	var b: PackedByteArray = _mgr.take_packed_presentation_frame()
	if b.is_empty():
		return {}
	if channel == BevyEnums.Channel.PACKED_DICTIONARY:
		return _mgr.parse_packed_presentation_frame(b)
	return _mgr.parse_packed_presentation_streams(b)

func drain_events() -> Array:
	return _mgr.drain_presentation_events()

func render_alpha() -> float:
	return _mgr.render_alpha()
