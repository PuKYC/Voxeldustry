class_name VoxelMaterialLibrary
extends Resource
## 体素材质库：有序材质表 + block→面材质绑定 + 构建参数；把资源编译成 GPU 数据。
##
## 这是**外观的唯一真相源**（docs/项目架构/06-体素世界与材质.md「Godot 资源模型」）：
## - materials 数组下标 == material id == Texture2DArray layer；没有 id 字段；
## - blocks 定义 block id → 六个面的材质 id（Rust 只提供方块名字用于对账）；
## - 材质像素来自 bake 产物 VoxelMaterialTiles，运行时不生成任何像素；
## - 构建产物 VoxelMaterialArrays 在所有块之间共享（一个 ShaderMaterial）。

const MAX_LAYERS := 64
const DEFAULT_TILE_SIZE := 64
const MIN_TILE_SIZE := 8
const FACE_COUNT := 6
const BLOCK_COUNT := 256

## 有序材质表：下标 = layer = material id。
@export var materials: Array[VoxelMaterial] = []
## block → 面材质绑定。block_key 必须等于 game-core BLOCK_TABLE 的方块名。
@export var blocks: Array[VoxelBlockMaterial] = []
## 每层瓦片边长（像素，2 的幂）。必须与 bake 产物一致，否则重建数组会报错。
@export_range(8, 512) var tile_size: int = DEFAULT_TILE_SIZE
## 是否为每层生成 mipmap（关掉可省显存，但远处会闪烁）。
@export var generate_mipmaps: bool = true
## 全局 UV 缩放乘数（乘以每个材质的 uv_scale）。
@export var uv_scale: float = 1.0

## 构建缓存：材质是只读资源，运行时只构建一次；编辑器里改完参数要 force。
var _cache: VoxelMaterialArrays = null
var _cache_signature: String = ""


## 层数 = 材质数 = Texture2DArray depth。
func material_count() -> int:
	return materials.size()


## 按 layer 取材质；越界返回 null。
func material_at(layer: int) -> VoxelMaterial:
	if layer < 0 or layer >= materials.size():
		return null
	return materials[layer]


## 构建前自检：返回错误列表（空 = 通过）。
func validate() -> PackedStringArray:
	var errors := PackedStringArray()
	if materials.is_empty():
		errors.append("materials 为空：至少需要一个材质层（id 0）")
		return errors
	if materials.size() > MAX_LAYERS:
		errors.append("材质层数 %d 超过上限 %d" % [materials.size(), MAX_LAYERS])
	if tile_size < MIN_TILE_SIZE or tile_size > 512 or (tile_size & (tile_size - 1)) != 0:
		errors.append("tile_size 必须是 8..512 的 2 的幂（当前 %d）" % tile_size)
	if not (is_finite(uv_scale) and uv_scale > 0.0):
		errors.append("uv_scale 必须是有限正数（当前 %s）" % uv_scale)

	var seen := {}
	for i in materials.size():
		var mat := materials[i]
		if mat == null:
			errors.append("第 %d 层材质为 null" % i)
			continue
		var key := mat.material_key
		if key.is_empty():
			errors.append("第 %d 层 material_key 为空" % i)
		elif seen.has(key):
			errors.append("material_key 重复：%s（第 %d 层与第 %d 层）" % [key, seen[key], i])
		else:
			seen[key] = i
		for err in mat.validate():
			errors.append("第 %d 层（%s）：%s" % [i, key, err])
		if not mat.has_baked_tiles():
			errors.append("第 %d 层（%s）没有 bake 产物：先跑 tools/bake_voxel_materials.gd" % [i, key])
			continue
		var tiles := mat.tiles
		if tiles.albedo.get_width() != tile_size:
			errors.append("第 %d 层（%s）瓦片边长 %d != library.tile_size %d：重新 bake" % [
				i, key, tiles.albedo.get_width(), tile_size
			])
		# UV 必须是 1/256 的整数倍：块局部 UV = 32 * 2^lod * uv_scale，非整数会在
		# 每个块边界产生 1/32 级别的不连续（接缝）。
		var effective_uv := mat.uv_scale * uv_scale
		if absf(effective_uv * 256.0 - roundf(effective_uv * 256.0)) > 1e-3:
			errors.append("第 %d 层（%s）有效 uv_scale=%s 不是 1/256 的整数倍，块边界会出接缝" % [
				i, key, effective_uv
			])

	var seen_blocks := {}
	for block in blocks:
		if block == null:
			errors.append("blocks 里有 null 条目")
			continue
		if block.block_key.is_empty():
			errors.append("blocks 里有空的 block_key")
		elif seen_blocks.has(block.block_key):
			errors.append("block_key 重复：%s" % block.block_key)
		else:
			seen_blocks[block.block_key] = true
		if not block.is_complete():
			errors.append("方块 %s 的 faces 必须是 6 项（当前 %d）" % [block.block_key, block.faces.size()])
			continue
		for face in block.faces.size():
			var id := block.faces[face]
			if id < 0 or id >= materials.size():
				errors.append("方块 %s 的第 %d 面材质 id %d 越界（层数 %d）" % [
					block.block_key, face, id, materials.size()
				])
	return errors


## 构建（或复用缓存的）GPU 数据；失败返回 null 并 push_error。
##
## 每层取 bake 产物 Image → duplicate → generate_mipmaps → Texture2DArray.create_from_images；
## 参数 LUT 每层一个 RGBAH texel（uv_scale, alpha_scissor, emission_energy, 0）。
## 成功后才原子替换缓存，正在渲染的节点不会看到半成品。
func build_arrays(force: bool = false) -> VoxelMaterialArrays:
	var signature := _signature()
	if not force and _cache != null and _cache_signature == signature:
		return _cache

	var errors := validate()
	if not errors.is_empty():
		for error in errors:
			push_error("[VoxelMaterialLibrary] " + error)
		return null

	var count := materials.size()
	var albedo_images: Array[Image] = []
	var normal_images: Array[Image] = []
	var orm_images: Array[Image] = []
	var emissive_images: Array[Image] = []
	var params: Array[Vector4] = []
	var has_emissive := false
	var keys := PackedStringArray()

	for i in count:
		var mat := materials[i]
		var tiles := mat.tiles
		var albedo_image := _prepare(tiles.albedo)
		var normal_image := _prepare(tiles.normal)
		var orm_image := _prepare(tiles.orm)
		var emissive_image := _prepare(tiles.emissive)
		if albedo_image == null or normal_image == null or orm_image == null or emissive_image == null:
			push_error("[VoxelMaterialLibrary] 第 %d 层（%s）瓦片 Image 不可用" % [i, mat.material_key])
			return null
		if generate_mipmaps:
			for pair in [["albedo", albedo_image], ["normal", normal_image], ["orm", orm_image], ["emissive", emissive_image]]:
				if pair[1].generate_mipmaps() != OK:
					push_error("[VoxelMaterialLibrary] 第 %d 层 %s mipmap 生成失败" % [i, pair[0]])
					return null
		albedo_images.append(albedo_image)
		normal_images.append(normal_image)
		orm_images.append(orm_image)
		emissive_images.append(emissive_image)
		has_emissive = has_emissive or mat.is_emissive()
		params.append(Vector4(mat.uv_scale * uv_scale, mat.alpha_scissor, mat.emission_energy, 0.0))
		keys.append(mat.material_key)

	var albedo := _create_array(albedo_images, "albedo")
	var normal := _create_array(normal_images, "normal")
	var orm := _create_array(orm_images, "orm")
	if albedo == null or normal == null or orm == null:
		return null
	var emissive: Texture2DArray = null
	if has_emissive:
		emissive = _create_array(emissive_images, "emissive")
		if emissive == null:
			return null

	var arrays := VoxelMaterialArrays.new()
	arrays.albedo = albedo
	arrays.normal = normal
	arrays.orm = orm
	arrays.emissive = emissive
	arrays.params = ImageTexture.create_from_image(VoxelMaterialArrays.build_param_image(params))
	arrays.layer_count = count
	arrays.has_emissive = has_emissive
	arrays.tile_size = tile_size
	arrays.material_keys = keys
	arrays.layer_params = params
	if not arrays.is_ready():
		push_error("[VoxelMaterialLibrary] 构建结果自检失败：%s" % arrays.describe())
		return null

	_cache = arrays
	_cache_signature = signature
	return arrays


## 与 gdext 导出的方块名对账：block_names[id] 必须等于 blocks 里的 block_key。
## 返回 "ERROR: ..." / "WARN: ..." 列表（空 = 一致）。
func validate_block_binding(block_names: PackedStringArray) -> PackedStringArray:
	var problems := PackedStringArray()
	var seen := {}
	for block in blocks:
		if block == null or block.block_key.is_empty():
			continue
		var id := block_names.find(block.block_key)
		if id < 0:
			problems.append("WARN: 库里的方块 %s 不存在于 Rust BLOCK_TABLE（不会参与渲染）" % block.block_key)
			continue
		if seen.has(id):
			problems.append("ERROR: 方块 id %d 被 %s 与 %s 同时绑定" % [id, seen[id], block.block_key])
		else:
			seen[id] = block.block_key

	# 反向检查：Rust 导出的每个非空方块都必须有绑定。否则 map 里该 block 保持 0，
	# 会被**静默**渲染成 material 0（stone）——这比开不了机更难查。
	for id in mini(block_names.size(), BLOCK_COUNT):
		var name := block_names[id]
		if name.is_empty() or seen.has(id):
			continue
		problems.append("ERROR: Rust 方块 %s（id %d）没有面材质绑定，会被渲染成 material 0" % [name, id])
	return problems


## 生成 256*6 的 face_materials 字节表（index = block*6 + face；未绑定的方块保持 0）。
func build_face_material_map(block_names: PackedStringArray) -> PackedByteArray:
	# 没有对账数据时返回空表：Rust 侧会走 identity 降级，而不是把全图刷成 material 0。
	if block_names.is_empty():
		return PackedByteArray()
	var map := PackedByteArray()
	map.resize(BLOCK_COUNT * FACE_COUNT)
	var resolved := 0
	for block in blocks:
		if block == null or block.block_key.is_empty() or not block.is_complete():
			continue
		var id := block_names.find(block.block_key)
		if id < 0 or id >= BLOCK_COUNT:
			continue
		resolved += 1
		for face in FACE_COUNT:
			map[id * FACE_COUNT + face] = clampi(block.faces[face], 0, 255)
	# 一个都没对上（例如库里的名字与 Rust 全不一致）同样交给 identity 降级。
	if resolved == 0:
		return PackedByteArray()
	return map


## 供日志 / 测试的一行摘要。
func describe() -> String:
	var keys := PackedStringArray()
	for mat in materials:
		keys.append("<null>" if mat == null else mat.material_key)
	return "VoxelMaterialLibrary(layers=%d blocks=%d tile=%d mips=%s uv_scale=%s keys=%s)" % [
		materials.size(), blocks.size(), tile_size, generate_mipmaps, uv_scale, ",".join(keys),
	]


## 构建缓存签名：资源实例 + 关键构建参数。运行时材质只读，所以实例不变即产物不变；
## 编辑器内改参数后需要 build_arrays(true)（bake 工具会重写 tiles，实例 id 也会变）。
func _signature() -> String:
	var values: Array = [tile_size, generate_mipmaps, uv_scale, materials.size(), blocks.size()]
	for mat in materials:
		if mat == null:
			values.append("null")
			continue
		values.append_array([
			mat.get_instance_id(), mat.material_key,
			0 if mat.tiles == null else mat.tiles.get_instance_id(),
			mat.emission_energy, mat.uv_scale, mat.alpha_scissor,
		])
	for block in blocks:
		if block == null:
			values.append("null")
			continue
		values.append_array([block.get_instance_id(), block.block_key, block.faces])
	return str(values)


## 复制一份 Image（不修改资源里的烘焙数据），并清掉可能已有的 mip。
static func _prepare(source: Image) -> Image:
	if source == null:
		return null
	var image := source.duplicate() as Image
	if image == null:
		return null
	if image.has_mipmaps():
		image.clear_mipmaps()
	return image


## 同尺寸 / 同层 / 同 mip 的 Texture2DArray；失败返回 null（首图决定格式与 mip）。
func _create_array(images: Array[Image], label: String) -> Texture2DArray:
	var array := Texture2DArray.new()
	if array.create_from_images(images) != OK:
		push_error("[VoxelMaterialLibrary] %s Texture2DArray 创建失败（%d 层）" % [label, images.size()])
		return null
	return array
