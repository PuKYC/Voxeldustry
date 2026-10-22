extends SceneTree
## 体素材质默认资产 + 瓦片烘焙（可重复执行、幂等）。
##
## 做的事（docs/项目架构/06-体素世界与材质.md「烘焙管线」）：
## 1. 确保 res://bevy_client/voxel_material/defs/<key>.tres（9 个默认 VoxelMaterial）存在；
## 2. 对每个材质跑 VoxelTileBaker.build()，保存 res://bevy_client/voxel_material/tiles/<key>.res；
## 3. 把 tiles 赋回材质并保存 defs，最后保存库（materials 按 id 顺序 + blocks 内嵌）。
##
## 运行：
##   G=/tmp/Godot_v4.7.2-stable_linux.arm64
##   $G --headless --path godot-project --script res://tools/bake_voxel_materials.gd
##
## 幂等：作者参数 + seed 不变时，重复运行得到的 tiles .res 逐字节相同（脚本会打印 md5，
## 并在覆盖前对比旧 md5 给出 IDEMPOTENT / CHANGED）。

const LIB_PATH := "res://bevy_client/voxel_material/voxel_material_library.tres"
const DEF_DIR := "res://bevy_client/voxel_material/defs"
const TILE_DIR := "res://bevy_client/voxel_material/tiles"
const DEFAULT_TILE_SIZE := 64
## 方块名契约（game-core BLOCK_TABLE，V2 只导出名字）。
const BLOCK_NAMES := ["air", "stone", "dirt", "grass", "sand", "snow", "wood", "leaf"]

var _failures := 0


func _initialize() -> void:
	var code := _run()
	print("BAKE %s failures=%d" % ["OK" if code == 0 else "FAIL", _failures])
	quit(code)


func _run() -> int:
	_ensure_dir(DEF_DIR)
	_ensure_dir(TILE_DIR)

	var library := _load_or_create_library()
	var size := library.tile_size
	print("[bake] library=%s tile_size=%d mips=%s uv_scale=%s" % [
		LIB_PATH, size, library.generate_mipmaps, library.uv_scale,
	])
	if size < 8:
		_fail("library.tile_size 非法：%d" % size)
		return 1

	var specs := _default_material_specs()
	var materials: Array[VoxelMaterial] = []
	var keys: Array[String] = []
	for i in specs.size():
		var spec: Dictionary = specs[i]
		var key: String = spec["key"]
		keys.append(key)
		var def_path := DEF_DIR.path_join(key + ".tres")
		var material := _load_material(def_path)
		var created := material == null
		if created:
			material = _make_default_material(spec)
		# 作者参数以磁盘上的 defs 为准（幂等 / 可手工调参）；新建时用默认值。
		if material.material_key.is_empty():
			material.material_key = key
		if material.display_name.is_empty():
			material.display_name = String(spec["display_name"])

		var tiles := VoxelTileBaker.build(material, size)
		var tile_path := TILE_DIR.path_join(key + ".res")
		var old_md5 := FileAccess.get_md5(tile_path) if FileAccess.file_exists(tile_path) else ""
		if not _save(tiles, tile_path):
			return 1
		tiles.take_over_path(tile_path)
		var new_md5 := FileAccess.get_md5(tile_path)
		var state := "NEW" if old_md5.is_empty() else ("IDEMPOTENT" if old_md5 == new_md5 else "CHANGED")
		print("[bake] material %d %-10s %s tiles=%dx%d md5=%s %s" % [
			i, key, "created" if created else "loaded", size, size, new_md5, state,
		])
		material.tiles = tiles
		if not _save(material, def_path):
			return 1
		if material.resource_path != def_path:
			material.take_over_path(def_path)
		materials.append(material)

	# 保留已有库里的额外材质（未来“新增材质 = 加 .tres 并追加”不会被默认 bake 清掉）。
	if FileAccess.file_exists(LIB_PATH):
		var old_library := _load_material_library(LIB_PATH)
		if old_library != null:
			for material in old_library.materials:
				if material != null and not keys.has(material.material_key):
					materials.append(material)

	library.materials = materials
	library.blocks = _build_default_blocks(library)
	if library.tile_size != size:
		library.tile_size = size
	if not _save(library, LIB_PATH):
		return 1
	if library.resource_path != LIB_PATH:
		library.take_over_path(LIB_PATH)

	# 自检：默认库必须通过 validate / block 绑定对账，便于 CI 直接发现坏资产。
	var problems := library.validate()
	for problem in problems:
		_fail("library.validate: " + problem)
	var block_problems := library.validate_block_binding(PackedStringArray(BLOCK_NAMES))
	for problem in block_problems:
		_fail("library.validate_block_binding: " + problem)
	var face_map := library.build_face_material_map(PackedStringArray(BLOCK_NAMES))
	print("[bake] library: %s" % library.describe())
	print("[bake] face_map=%d bytes grass_block_faces=%s" % [
		face_map.size(), _grass_faces(face_map),
	])
	print("[bake] defs=%s tiles=%s" % [DEF_DIR, TILE_DIR])
	return 0 if _failures == 0 else 1


# ───────────────────────── 默认材质 / 方块 ─────────────────────────

## 默认材质表：参数取自 HEAD 版 game-core static_data（颜色 = sRGB 作者色）。
func _default_material_specs() -> Array[Dictionary]:
	var specs: Array[Dictionary] = []
	specs.append(_spec("stone", "Stone", Color8(122, 122, 126), 220, VoxelMaterial.Pattern.SPECKLE))
	specs.append(_spec("dirt", "Dirt", Color8(110, 78, 50), 255, VoxelMaterial.Pattern.SPECKLE))
	specs.append(_spec(
		"grass_top", "Grass Top", Color8(86, 148, 62), 255, VoxelMaterial.Pattern.GRASS_TOP
	))
	specs.append(_spec("sand", "Sand", Color8(214, 198, 140), 255, VoxelMaterial.Pattern.SPECKLE))
	specs.append(_spec("snow", "Snow", Color8(236, 240, 246), 200, VoxelMaterial.Pattern.NONE))
	specs.append(_spec("wood", "Wood", Color8(122, 88, 52), 230, VoxelMaterial.Pattern.GRAIN))
	specs.append(_spec("leaf", "Leaf", Color8(64, 122, 54), 255, VoxelMaterial.Pattern.LEAFY))
	var air := _spec("air", "Air", Color(0.0, 0.0, 0.0, 0.0), 255, VoxelMaterial.Pattern.NONE)
	air["normal_from_albedo"] = false
	specs.append(air)
	var grass_side := _spec(
		"grass_side", "Grass Side", Color8(110, 78, 50), 255, VoxelMaterial.Pattern.GRASS_SIDE
	)
	grass_side["pattern_secondary_color"] = Color8(86, 148, 62)
	grass_side["pattern_band"] = 0.2
	specs.append(grass_side)
	return specs


func _spec(
	key: String, display_name: String, color: Color,
	roughness_byte: int, pattern: VoxelMaterial.Pattern
) -> Dictionary:
	return {
		"key": key,
		"display_name": display_name,
		"albedo_color": color,
		"roughness": float(roughness_byte) / 255.0,
		"pattern": pattern,
		"pattern_secondary_color": Color8(255, 255, 255),
		"pattern_band": 0.2,
		"normal_from_albedo": true,
	}


## 只在新建设 defs 时调用：默认参数（颜色 / 粗糙度 / 图案 / 无自发光）。
func _make_default_material(spec: Dictionary) -> VoxelMaterial:
	var material := VoxelMaterial.new()
	material.material_key = String(spec["key"])
	material.display_name = String(spec["display_name"])
	material.albedo_color = spec["albedo_color"]
	material.albedo_pattern = spec["pattern"]
	material.pattern_secondary_color = spec["pattern_secondary_color"]
	material.pattern_scale = 1.0
	material.pattern_strength = 1.0
	material.pattern_band = float(spec["pattern_band"])
	material.pattern_seed = 0
	material.roughness = float(spec["roughness"])
	material.metallic = 0.0
	material.occlusion = 1.0
	material.normal_strength = 1.0
	material.normal_flip_green = false
	material.normal_from_albedo = bool(spec["normal_from_albedo"])
	material.emission_color = Color.BLACK
	# 显式 0.0：默认材质没有自发光（LUT 里 emission_energy 也是 0）。
	material.emission_energy = 0.0
	material.uv_scale = 1.0
	material.alpha_scissor = 0.0
	return material


## 默认 block -> 六面材质 id 绑定（面序 [+x,-x,+y,-y,+z,-z] == plane*2+dir）。
func _build_default_blocks(library: VoxelMaterialLibrary) -> Array[VoxelBlockMaterial]:
	var existing_by_key := {}
	for block in library.blocks:
		if block != null and not block.block_key.is_empty():
			existing_by_key[block.block_key] = block
	var defaults := [
		["air", [7, 7, 7, 7, 7, 7]],
		["stone", [0, 0, 0, 0, 0, 0]],
		["dirt", [1, 1, 1, 1, 1, 1]],
		["grass", [8, 8, 2, 1, 8, 8]],
		["sand", [3, 3, 3, 3, 3, 3]],
		["snow", [4, 4, 4, 4, 4, 4]],
		["wood", [5, 5, 5, 5, 5, 5]],
		["leaf", [6, 6, 6, 6, 6, 6]],
	]
	var blocks: Array[VoxelBlockMaterial] = []
	var default_keys: Array[String] = []
	for entry in defaults:
		var key: String = entry[0]
		default_keys.append(key)
		var block: VoxelBlockMaterial = existing_by_key.get(key)
		if block == null:
			block = VoxelBlockMaterial.new()
		# 只补空字段：作者在 .tres 里改过的 block_key / 六面绑定绝不被 bake 覆盖。
		if block.block_key.is_empty():
			block.block_key = key
		if block.faces.size() != 6:
			var faces := PackedInt32Array()
			for id in entry[1]:
				faces.append(int(id))
			print("[bake] block %-8s faces 缺失（%d 项），写入默认六面" % [key, block.faces.size()])
			block.faces = faces
		blocks.append(block)
	# 保留额外绑定，避免默认 bake 清掉未来的 mod / 自定义方块。
	for block in library.blocks:
		if block != null and not block.block_key.is_empty() and not default_keys.has(block.block_key):
			blocks.append(block)
	return blocks


# ───────────────────────── 资源 IO ─────────────────────────

func _load_or_create_library() -> VoxelMaterialLibrary:
	if FileAccess.file_exists(LIB_PATH):
		var loaded := _load_material_library(LIB_PATH)
		if loaded != null:
			return loaded
		_fail("库资源存在但加载失败：%s" % LIB_PATH)
	var library := VoxelMaterialLibrary.new()
	library.tile_size = DEFAULT_TILE_SIZE
	library.generate_mipmaps = true
	library.uv_scale = 1.0
	return library


func _load_material_library(path: String) -> VoxelMaterialLibrary:
	var resource := ResourceLoader.load(path)
	if resource is VoxelMaterialLibrary:
		return resource
	_fail("不是 VoxelMaterialLibrary：%s" % path)
	return null


func _load_material(path: String) -> VoxelMaterial:
	if not FileAccess.file_exists(path):
		return null
	var resource := ResourceLoader.load(path)
	if resource is VoxelMaterial:
		return resource
	_fail("不是 VoxelMaterial：%s" % path)
	return null


func _save(resource: Resource, path: String) -> bool:
	var err := ResourceSaver.save(resource, path)
	if err != OK:
		_fail("ResourceSaver.save 失败 err=%d path=%s" % [err, path])
		return false
	return true


func _ensure_dir(path: String) -> void:
	if not DirAccess.dir_exists_absolute(ProjectSettings.globalize_path(path)):
		var err := DirAccess.make_dir_recursive_absolute(ProjectSettings.globalize_path(path))
		if err != OK:
			_fail("无法创建目录 %s err=%d" % [path, err])


func _grass_faces(face_map: PackedByteArray) -> String:
	var grass_id := BLOCK_NAMES.find("grass")
	if grass_id < 0 or face_map.size() < (grass_id + 1) * 6:
		return "?"
	var faces: Array[String] = []
	for face in 6:
		faces.append(str(face_map[grass_id * 6 + face]))
	return "[" + ",".join(faces) + "]"


func _fail(message: String) -> void:
	_failures += 1
	push_error("[bake] " + message)
