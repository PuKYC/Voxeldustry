extends SceneTree
## 体素材质管线 headless 校验（无编辑器 / 无渲染依赖）。
##
## 覆盖（docs/项目架构/06-体素世界与材质.md「验证方式」）：
## 1. 默认库可加载、validate() / validate_block_binding() 全空；
## 2. 256×6 face 表：长度 1536、草方块六面 == [8,8,2,1,8,8]；
## 3. build_arrays()：层数 / has_emissive / 数组格式 / 层数 / mip / 尺寸**从库推导**
##    （允许“新增材质 = 追加 .tres”；不再写死 9 层 / 64 / 无自发光）；
## 4. 参数 LUT：width == 层数、height 1、RGBAH，
##    逐层 == (uv_scale*library.uv_scale, alpha_scissor, emission_energy)；
## 5. 像素断言：草侧草边在高行、normal 均值 ~128、ORM 通道 = 作者参数、空气 alpha=0、
##    石/木 normal 有起伏且雪基本平；
## 6. bake 确定性：同参数连续 build 两次四张 Image 逐字节相等；
## 7. 可扩展性：复制 stone 追加为第 N+1 层，build_arrays() 成功且 layer_count == N+1。
##
## 运行：
##   /tmp/Godot_v4.7.2-stable_linux.arm64 --headless --path godot-project
##       --script res://test/voxel_material_check.gd
##
## 失败非零退出；通过打印 VOXEL_MATERIAL_CHECK OK 并 quit(0)。

const LIB_PATH := "res://bevy_client/voxel_material/voxel_material_library.tres"
const BLOCK_NAMES := ["air", "stone", "dirt", "grass", "sand", "snow", "wood", "leaf"]
const EXPECTED_GRASS_FACES := [8, 8, 2, 1, 8, 8]
## 默认资产契约：**前 9 个** key 的顺序固定；第 10 层起允许追加（追加不破坏本契约）。
const DEFAULT_KEYS := [
	"stone", "dirt", "grass_top", "sand", "snow", "wood", "leaf", "air", "grass_side",
]

var _failures: PackedStringArray = PackedStringArray()
var _checks := 0


func _initialize() -> void:
	_run.call_deferred()


func _run() -> void:
	await process_frame
	var library := ResourceLoader.load(LIB_PATH) as VoxelMaterialLibrary
	_check(library != null, "库资源可加载：%s" % LIB_PATH)
	if library == null:
		_finish()
		return
	print("[voxel_material_check] library: %s" % library.describe())

	# ── 1. 资源自检 / block 绑定 ──────────────────────────────
	var layer_count := library.materials.size()
	_check(layer_count >= DEFAULT_KEYS.size(),
		"materials 数量 >= 默认 %d（实际 %d；第 10 层起允许追加）" % [DEFAULT_KEYS.size(), layer_count])
	var keys: Array[String] = []
	for material in library.materials:
		if material == null or material.material_key.is_empty():
			keys.append("")
		else:
			keys.append(material.material_key)
	_check(not keys.has(""), "所有 material 都有非空 material_key")
	_check(keys.slice(0, DEFAULT_KEYS.size()) == DEFAULT_KEYS,
		"前 %d 个 key 契约依次 == %s（实际 %s）" % [
			DEFAULT_KEYS.size(), str(DEFAULT_KEYS), str(keys.slice(0, DEFAULT_KEYS.size())),
		])

	var problems := library.validate()
	for problem in problems:
		print("  validate: " + problem)
	_check(problems.is_empty(), "library.validate() 为空")

	# bake 产物：格式 / 尺寸完整且**不带 mipmap**（mip 由 build_arrays 统一生成）。
	var tiles_ok := true
	for material in library.materials:
		if material == null or material.tiles == null or not material.tiles.is_ready():
			tiles_ok = false
			continue
		var images: Array[Image] = [
			material.tiles.albedo, material.tiles.normal,
			material.tiles.orm, material.tiles.emissive,
		]
		for image in images:
			if image.has_mipmaps():
				tiles_ok = false
	_check(tiles_ok, "%d 个材质的 tiles 完整（格式/尺寸）且不带 mipmap" % layer_count)
	var block_problems := library.validate_block_binding(PackedStringArray(BLOCK_NAMES))
	for problem in block_problems:
		print("  block_binding: " + problem)
	_check(block_problems.is_empty(), "validate_block_binding() 为空")

	# ── 2. face 表 ───────────────────────────────────────────
	var names := PackedStringArray(BLOCK_NAMES)
	var face_map := library.build_face_material_map(names)
	_check(face_map.size() == 256 * 6, "face 表长度 = 1536（实际 %d）" % face_map.size())
	var grass_id := names.find("grass")
	_check(grass_id == 3, "grass block id == 3（实际 %d）" % grass_id)
	var grass_faces: Array[int] = []
	for face in 6:
		grass_faces.append(face_map[grass_id * 6 + face])
	_check(grass_faces == EXPECTED_GRASS_FACES, "草方块六面 == [8,8,2,1,8,8]（实际 %s）" % str(grass_faces))

	# ── 3. build_arrays() ────────────────────────────────────
	var arrays := library.build_arrays()
	_check(arrays != null, "build_arrays() 成功（运行时默认路径，无 force）")
	if arrays == null:
		_finish()
		return
	print("[voxel_material_check] arrays: %s" % arrays.describe())
	var want_emissive := false
	for material in library.materials:
		if material != null and material.is_emissive():
			want_emissive = true
	_check(arrays.layer_count == layer_count,
		"layer_count == library.materials.size()（%d，实际 %d）" % [layer_count, arrays.layer_count])
	_check(arrays.has_emissive == want_emissive,
		"has_emissive == 任一 material.is_emissive()（期望 %s，实际 %s）" % [
			want_emissive, arrays.has_emissive,
		])
	if want_emissive:
		_check(arrays.emissive != null, "有自发光时 emissive 数组存在")
	else:
		_check(arrays.emissive == null, "无自发光时 emissive 数组为 null")
	_check(arrays.tile_size == library.tile_size,
		"arrays.tile_size == library.tile_size（%d，实际 %d）" % [library.tile_size, arrays.tile_size])
	_check(arrays.is_ready(), "VoxelMaterialArrays.is_ready()")
	_check(arrays.material_keys == PackedStringArray(keys),
		"arrays.material_keys 与 library.materials 顺序一致")

	_check_array(arrays.albedo, "albedo", Image.FORMAT_RGBA8, layer_count, library.tile_size)
	_check_array(arrays.normal, "normal", Image.FORMAT_RG8, layer_count, library.tile_size)
	_check_array(arrays.orm, "orm", Image.FORMAT_RGBA8, layer_count, library.tile_size)
	_check(arrays.albedo.has_mipmaps() == library.generate_mipmaps,
		"albedo mip 状态 == library.generate_mipmaps（%s）" % library.generate_mipmaps)
	_check(arrays.normal.has_mipmaps() == library.generate_mipmaps, "normal mip 状态正确")
	_check(arrays.orm.has_mipmaps() == library.generate_mipmaps, "orm mip 状态正确")

	# ── 4. 参数 LUT ─────────────────────────────────────────
	_check(arrays.params is ImageTexture, "params 是 ImageTexture")
	if arrays.params is ImageTexture:
		var lut := (arrays.params as ImageTexture).get_image()
		_check(lut != null, "LUT Image 可读")
		if lut != null:
			_check(lut.get_width() == layer_count and lut.get_height() == 1,
				"LUT 尺寸 %dx1（实际 %dx%d）" % [layer_count, lut.get_width(), lut.get_height()])
			_check(lut.get_format() == Image.FORMAT_RGBAH,
				"LUT 格式 RGBAH（实际 %d）" % lut.get_format())
			var lut_ok := true
			for layer in layer_count:
				var material := library.materials[layer]
				var want := Vector3(
					material.uv_scale * library.uv_scale,
					material.alpha_scissor,
					material.emission_energy
				)
				var got := lut.get_pixel(layer, 0)
				if not (is_equal_approx(got.r, want.x)
						and is_equal_approx(got.g, want.y)
						and is_equal_approx(got.b, want.z)):
					lut_ok = false
					print("  LUT[%d] want=%s got=%s" % [layer, str(want), str(got)])
			_check(lut_ok, "LUT 逐层 == (uv_scale, alpha_scissor, emission_energy)")

	# ── 5. 像素断言 ─────────────────────────────────────────
	var grass_side := _material(library, "grass_side")
	_check(grass_side != null, "找到 grass_side 材质")
	if grass_side != null:
		var size := grass_side.tiles.albedo.get_width()
		var low := _mean_channel(grass_side.tiles.albedo, 1, 0, size / 10)
		var high := _mean_channel(grass_side.tiles.albedo, 1, size - size / 10, size)
		print("[voxel_material_check] grass_side G mean low=%.4f high=%.4f" % [low, high])
		_check(high > low + 0.05, "草侧高 1/10 行 G 均值明显高于低 1/10 行")

	for key in ["stone", "dirt", "grass_top", "sand", "snow", "wood", "leaf"]:
		var material := _material(library, key)
		if material == null:
			_check(false, "找到材质 %s" % key)
			continue
		var normal := material.tiles.normal
		var mean_r := _mean_channel(normal, 0, 0, normal.get_height())
		var mean_g := _mean_channel(normal, 1, 0, normal.get_height())
		print("[voxel_material_check] %-10s normal mean r=%.3f g=%.3f" % [key, mean_r, mean_g])
		_check(absf(mean_r - 0.5) < 0.02 and absf(mean_g - 0.5) < 0.02,
			"%s normal 均值 ~128（r=%.3f g=%.3f）" % [key, mean_r, mean_g])
		var orm := material.tiles.orm
		var center := orm.get_pixel(orm.get_width() / 2, orm.get_height() / 2)
		_check(absf(center.r - material.occlusion) < 2.0 / 255.0,
			"%s ORM.R == occlusion" % key)
		_check(absf(center.g - material.roughness) < 2.0 / 255.0,
			"%s ORM.G == roughness（实际 %.3f / %.3f）" % [key, center.g, material.roughness])
		_check(absf(center.b - material.metallic) < 2.0 / 255.0,
			"%s ORM.B == metallic" % key)

	var air := _material(library, "air")
	_check(air != null, "找到 air 材质")
	if air != null:
		var data := air.tiles.albedo.get_data()
		var alpha_zero := true
		for i in data.size() / 4:
			if data[i * 4 + 3] != 0:
				alpha_zero = false
				break
		_check(alpha_zero, "空气 albedo alpha 全 0")

	var stone_std := _stddev_channel(_material(library, "stone").tiles.normal, 0)
	var wood_std := _stddev_channel(_material(library, "wood").tiles.normal, 0)
	var snow_std := _stddev_channel(_material(library, "snow").tiles.normal, 0)
	print("[voxel_material_check] normal R stddev stone=%.4f wood=%.4f snow=%.4f" % [
		stone_std, wood_std, snow_std,
	])
	_check(stone_std > 0.01, "石 normal 有起伏（stddev=%.4f）" % stone_std)
	_check(wood_std > 0.005, "木 normal 有起伏（stddev=%.4f）" % wood_std)
	_check(snow_std < 0.005, "雪 normal 基本平（stddev=%.4f）" % snow_std)
	_check(snow_std < stone_std * 0.5, "雪 normal 起伏远小于石")

	# ── 6. bake 确定性 ──────────────────────────────────────
	var source := library.materials[0]
	var a := VoxelTileBaker.build(source, library.tile_size)
	var b := VoxelTileBaker.build(source, library.tile_size)
	var deterministic := (
		a.albedo.get_data() == b.albedo.get_data()
		and a.normal.get_data() == b.normal.get_data()
		and a.orm.get_data() == b.orm.get_data()
		and a.emissive.get_data() == b.emissive.get_data()
	)
	_check(deterministic, "同参数连续 bake 两次，四张 Image 逐字节相等")
	_check(a.emissive_enabled == false, "默认材质 bake 后 emissive_enabled=false")

	# ── 7. 可扩展性：第 10 层 ───────────────────────────────
	var extended := VoxelMaterialLibrary.new()
	extended.tile_size = library.tile_size
	extended.generate_mipmaps = library.generate_mipmaps
	extended.uv_scale = library.uv_scale
	var materials: Array[VoxelMaterial] = []
	for material in library.materials:
		materials.append(material)
	var extra := library.materials[0].duplicate() as VoxelMaterial
	_check(extra != null, "可复制 stone 材质")
	if extra != null:
		# 新层的 key 必须唯一（库里可能已经有 “stone_copy” 之类的追加材质）。
		var extra_key := "stone_copy"
		while keys.has(extra_key):
			extra_key += "_x"
		extra.material_key = extra_key
		extra.tiles = VoxelTileBaker.build(extra, library.tile_size)
		materials.append(extra)
	extended.materials = materials
	extended.blocks = library.blocks.duplicate()
	var extended_count := layer_count + 1
	var extended_arrays := extended.build_arrays(true)
	_check(extended_arrays != null and extended_arrays.layer_count == extended_count,
		"追加第 %d 层后 build_arrays() 成功且 layer_count == %d（实际 %s）" % [
			extended_count, extended_count,
			"null" if extended_arrays == null else str(extended_arrays.layer_count),
		])

	_finish()


# ───────────────────────── 断言 / 统计 helper ─────────────────────────

func _check(condition: bool, message: String) -> void:
	_checks += 1
	if condition:
		print("[voxel_material_check] PASS  " + message)
	else:
		_failures.append(message)
		print("[voxel_material_check] FAIL  " + message)


func _finish() -> void:
	print("[voxel_material_check] checks=%d failures=%d" % [_checks, _failures.size()])
	if _failures.is_empty():
		print("VOXEL_MATERIAL_CHECK OK")
		quit(0)
		return
	for failure in _failures:
		print("  - " + failure)
	print("VOXEL_MATERIAL_CHECK FAIL")
	quit(1)


func _check_array(
	array: Texture2DArray, label: String, format: Image.Format,
	expected_layers: int, expected_size: int
) -> void:
	_check(array != null, "%s 数组存在" % label)
	if array == null:
		return
	_check(array.get_layers() == expected_layers,
		"%s layers == %d（实际 %d）" % [label, expected_layers, array.get_layers()])
	_check(array.get_width() == expected_size and array.get_height() == expected_size,
		"%s 尺寸 %dx%d（实际 %dx%d）" % [
			label, expected_size, expected_size, array.get_width(), array.get_height(),
		])
	_check(array.get_format() == format, "%s 格式正确（实际 %d）" % [label, array.get_format()])


func _material(library: VoxelMaterialLibrary, key: String) -> VoxelMaterial:
	for material in library.materials:
		if material != null and material.material_key == key:
			return material
	return null


## 指定通道在 [row_begin, row_end) 行内的均值（0..1）。
func _mean_channel(image: Image, channel: int, row_begin: int, row_end: int) -> float:
	var total := 0.0
	var count := 0
	for row in range(row_begin, row_end):
		for col in image.get_width():
			var c := image.get_pixel(col, row)
			match channel:
				0:
					total += c.r
				1:
					total += c.g
				2:
					total += c.b
				3:
					total += c.a
			count += 1
	return total / float(maxi(count, 1))


func _stddev_channel(image: Image, channel: int) -> float:
	var total := 0.0
	var count := 0
	for row in image.get_height():
		for col in image.get_width():
			var c := image.get_pixel(col, row)
			total += c.r if channel == 0 else (c.g if channel == 1 else c.b)
			count += 1
	var mean := total / float(maxi(count, 1))
	var variance := 0.0
	for row in image.get_height():
		for col in image.get_width():
			var c := image.get_pixel(col, row)
			var value := c.r if channel == 0 else (c.g if channel == 1 else c.b)
			variance += (value - mean) * (value - mean)
	return sqrt(variance / float(maxi(count, 1)))
