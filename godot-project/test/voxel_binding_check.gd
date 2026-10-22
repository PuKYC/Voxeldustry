extends SceneTree
## 体素材质绑定端到端校验（需要已构建的 GDExtension）：
##   Godot 资源库 -> block_names 对账 -> face_material_map(256*6) -> gdext mesher
##   -> 39 bit 矩形流 -> 逐面材质 id。
##
## 覆盖的是 test/voxel_material_check.gd 测不到的一段：Rust 导出契约与 mesher 的
## 第 4 个参数（face_material_map）真的被应用，且 identity 降级路径不 panic。
## 失败非零退出；成功打印 MATERIAL_BINDING OK。

const LIBRARY_PATH := "res://bevy_client/voxel_material/voxel_material_library.tres"
const RAW_HALO := 34


func _initialize() -> void:
	var failures := 0
	var mgr = ClassDB.instantiate("BevyAppManager")
	if mgr == null:
		print("FAIL: 无法实例化 BevyAppManager（GDExtension 未构建或未启用）")
		quit(1)
		return
	get_root().add_child(mgr)

	var table: Dictionary = mgr.get_voxel_table()
	if table.keys() != ["voxel_size", "block_names"]:
		failures += _expect("get_voxel_table 恰好两个键", false, "实际 %s" % [table.keys()])
	var block_names: PackedStringArray = table.get("block_names", PackedStringArray())
	failures += _expect("block_names 长度 256", block_names.size() == 256, "实际 %d" % block_names.size())
	failures += _expect("block 0..7 名字", block_names.slice(0, 8) == PackedStringArray([
		"air", "stone", "dirt", "grass", "sand", "snow", "wood", "leaf",
	]), "实际 %s" % [block_names.slice(0, 8)])

	var library: VoxelMaterialLibrary = load(LIBRARY_PATH)
	if library == null:
		print("FAIL: 材质库加载失败 %s" % LIBRARY_PATH)
		quit(1)
		return
	var problems := library.validate_block_binding(block_names)
	failures += _expect("材质库与 block_names 对账", problems.is_empty(), str(problems))

	var face_map := library.build_face_material_map(block_names)
	failures += _expect("face_material_map 长度 1536", face_map.size() == 256 * 6, "实际 %d" % face_map.size())
	var arrays := library.build_arrays()
	failures += _expect("材质数组构建", arrays != null and arrays.is_ready(), "arrays=%s" % arrays)

	var grass_id := block_names.find("grass")
	var blocks := PackedByteArray()
	blocks.resize(RAW_HALO * RAW_HALO * RAW_HALO)
	blocks[1 + RAW_HALO * (1 + RAW_HALO * 1)] = grass_id

	# 草方块六面材质：+x/-x/+z/-z = grass_side(8)，+y = grass_top(2)，-y = dirt(1)。
	var want := {0: 8, 1: 8, 2: 2, 3: 1, 4: 8, 5: 8}
	var words: PackedInt64Array = mgr.mesh_voxel_halo(0, blocks, 0, face_map)
	failures += _expect("单个体素 6 个矩形", words.size() == 6, "实际 %d" % words.size())
	var seen := {}
	for w in words:
		var word := int(w)
		var orientation := word & 7
		var material := (word >> 31) & 0xFF
		seen[orientation] = material
		failures += _expect("面 %d 材质 %d" % [orientation, want.get(orientation, -1)],
			material == want.get(orientation, -1), "实际 %d" % material)

	# 空 map = identity 降级：material 保持 block id，几何不变。
	var identity: PackedInt64Array = mgr.mesh_voxel_halo(0, blocks, 0, PackedByteArray())
	failures += _expect("identity 降级仍是 6 个矩形", identity.size() == 6, "实际 %d" % identity.size())
	for w in identity:
		var material := (int(w) >> 31) & 0xFF
		failures += _expect("identity 保持 block id", material == grass_id, "实际 %d" % material)

	if failures == 0:
		print("MATERIAL_BINDING OK rects=%d grass_faces=%s" % [words.size(), seen])
		quit(0)
	else:
		print("MATERIAL_BINDING FAIL failures=%d" % failures)
		quit(1)


func _expect(label: String, ok: bool, detail: String) -> int:
	if ok:
		print("PASS  ", label)
		return 0
	print("FAIL  ", label, " — ", detail)
	return 1
