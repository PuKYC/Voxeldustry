class_name CommandCursor
extends RefCounted
## 复用同一个对象遍历整帧命令，避免每条命令分配。

var f32_pool: PackedFloat32Array
var i32_pool: PackedInt64Array
var f32_offsets: PackedInt64Array
var f32_counts: PackedInt64Array
var i32_offsets: PackedInt64Array
var i32_counts: PackedInt64Array
var f32_off: int
var f32_cnt: int
var i32_off: int
var i32_cnt: int

func bind(f: Dictionary) -> void:
	f32_pool = f["f32_pool"]
	i32_pool = f["i32_pool"]
	f32_offsets = f["f32_offsets"]
	f32_counts = f["f32_counts"]
	i32_offsets = f["i32_offsets"]
	i32_counts = f["i32_counts"]

func at(i: int) -> void:
	f32_off = f32_offsets[i]
	f32_cnt = f32_counts[i]
	i32_off = i32_offsets[i]
	i32_cnt = i32_counts[i]
