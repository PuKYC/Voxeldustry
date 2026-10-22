class_name VoxelMeshNode
extends BevyEntityNode
## 体素绘制节点：每块一个 MultiMeshInstance3D，逐顶点在 GPU 上把
## 39 bit 贪心矩形展开成四边形（见 shaders/voxel_rect.gdshader）。
##
## 8 B/矩形只有一份：R32G32_SFLOAT 纹理（float 位模式承载原始 u64），由
## RenderingDevice 创建 / 更新（Texture2DRD 绑定给 sampler2D）。之所以不用
## R32G32_UINT，是因为 Godot 4.x 的 Texture2DRD 无法把整数格式映射到
## Image::Format，会在 texture_rd_initialize 时报 "Unsupported image format"。
## 着色器端用 floatBitsToUint 还原 u64 位模式，无损。
##
## GDScript 每块只做一次 texture_update + instance_count 赋值，**绝不逐矩形循环**。
##
## 数据来源：同一实体上的 Transform 载荷给出块原点（米），RAWVOXELS 载荷给出
## lod + 34³ 原始体素；gdext mesher（mesh_fn）现算矩形流后上传。
## 几何数学的真相源是 game-core::presentation::voxel_mesh。

const SHADER_PATH := "res://bevy_client/shaders/voxel_rect.gdshader"
const WORD_BYTES := 8
## 每行最多多少个矩形（RD 纹理宽度上限远大于此）。
const TEX_WIDTH := 1024

## 全局共享的材质 GPU 数据（数组 + 参数 LUT + 层数）。
## 唯一来源是 Godot 资源库 VoxelMaterialLibrary（外观真相源在 Godot），
## BevyClient 启动时 load 一次，经 apply_material_library() 构建后写进所有节点。
static var material_arrays: VoxelMaterialArrays = null
## 每次 apply_material_library 成功后 +1；节点只在版本变化时写 uniform。
static var material_version: int = 0
## block→面材质表（256*6 字节，Godot 侧 VoxelMaterialLibrary 生成），
## 每次 mesh_fn 调用时传给 gdext mesher；空表 = Rust 侧 identity 降级。
static var face_material_map: PackedByteArray = PackedByteArray()
## 材质层数（= Texture2DArray depth），仅用于 shader 越界夹取。
static var layer_count: int = 0
## 是否存在自发光材质：false 时着色器跳过 emissive_array 采样（省片元带宽）。
static var has_emissive: bool = false
static var voxel_size: float = 0.45
## AO 总强度（静态全局）：1.0 = 当前观感（默认开启），0.0 = 完全关闭。
static var ao_strength: float = 1.0
## AO 曲线指数（静态全局）：1.0 = 当前观感（不做 pow）。
static var ao_power: float = 1.0
## LOD 切换交叉淡入时长（静态全局，秒）。0.0 = 关闭（默认，保持旧观感）：
## 关闭时不写 lod_fade，shader 侧的 vec2(0,0) 是严格 no-op。
static var lod_fade_duration: float = 0.0
## 跨 LOD 接缝裙边总开关（静态全局，默认开启）：false 时 set_raw_voxel_view
## 一律向 mesher 传 mask=0，行为退回普通 greedy meshing（镜像 godot_voxel 的
## per-model lod_skirts_enabled）。
static var lod_skirts_enabled: bool = true
## RAWVOXELS：由 BevyClient 装配的 gdext mesher 桥（manager.mesh_voxel_halo）。
## 无效时 set_raw_voxel_view 只告警，不做网格化。
static var mesh_fn: Callable = Callable()
## 扩展是否支持 mesh_voxel_halo 的第 4 参（face_material_map）。
## BevyClient 用 get_voxel_table() 是否含 block_names 判断新旧扩展；旧扩展按 3 参调用。
static var mesh_supports_face_map: bool = true
## 调试：为 true 时读回第一块纹理自检（会触发 GPU 同步，仅调试用）。
static var debug_self_check: bool = false

static var _shader: Shader = null
static var _quad: QuadMesh = null

var _mmi: MultiMeshInstance3D
var _mm: MultiMesh
var _material: ShaderMaterial
var _rd: RenderingDevice = null
var _texture: Texture2DRD = null
var _texture_rid := RID()
var _tex_capacity := 0
var _lod := 0
var _has_mesh := false
## 已写入材质的 AO 参数缓存；只在变化时写 shader，热路径不重复写。
var _ao_strength := -1.0
var _ao_power := -1.0
## 已写入材质的 GPU 数据版本（= VoxelMeshNode.material_version）。
## 只在版本变化时写数组 / LUT / layer_count 等 uniform。
var _applied_material_version: int = -1
## 材质未就绪的告警只打一次，避免每块每帧刷屏。
var _warned_missing_materials: bool = false
## LOD 淡入状态：切换到新 LOD 时从"全 discard"渐变到"全显示"。
var _fade_active := false
var _fade_elapsed := 0.0


func _init() -> void:
	_mm = MultiMesh.new()
	_mm.transform_format = MultiMesh.TRANSFORM_3D
	_mm.mesh = _shared_quad()

	_mmi = MultiMeshInstance3D.new()
	_mmi.multimesh = _mm
	_mmi.cast_shadow = GeometryInstance3D.SHADOW_CASTING_SETTING_OFF
	# 着色器用 INSTANCE_ID + block_origin 自算世界坐标，实例 AABB 不可信，
	# 用一个大 AABB 阻止整体剔除。
	var huge := 1000000.0
	_mmi.custom_aabb = AABB(
		Vector3(-huge, -huge, -huge),
		Vector3(huge * 2.0, huge * 2.0, huge * 2.0)
	)
	add_child(_mmi)

	_material = ShaderMaterial.new()
	_material.shader = _shared_shader()
	_mmi.material_override = _material


static func _shared_shader() -> Shader:
	if _shader == null:
		_shader = load(SHADER_PATH) as Shader
	return _shader


static func _shared_quad() -> QuadMesh:
	if _quad == null:
		_quad = QuadMesh.new()
		_quad.size = Vector2.ONE
		# 4 个顶点、2 个三角形；UV 0..1 用来区分角点。
		_quad.subdivide_width = 0
		_quad.subdivide_depth = 0
	return _quad


## 配置 AO（静态全局，影响之后物化 / 重新上传的体素节点）。
## strength：1.0 = 当前观感；0.0 = 完全关闭。power：1.0 = 原曲线，>1 加深。
static func set_ao(strength: float = 1.0, power: float = 1.0) -> void:
	ao_strength = clampf(strength, 0.0, 1.0)
	ao_power = clampf(power, 0.0, 8.0)


## AO 开关（静态全局）。
static func set_ao_enabled(enabled: bool) -> void:
	ao_strength = 1.0 if enabled else 0.0


## 配置 LOD 交叉淡入时长（秒，静态全局）。0.0 = 关闭（默认）。
static func set_lod_fade_duration(seconds: float) -> void:
	lod_fade_duration = clampf(seconds, 0.0, 1.0)


## 跨 LOD 接缝裙边开关（静态全局）。false 时所有块按 mask=0 网格化。
static func set_lod_skirts_enabled(enabled: bool) -> void:
	lod_skirts_enabled = enabled


## 从 Godot 资源库构建共享材质 GPU 数据（Texture2DArray ×3~4 + 参数 LUT）。
##
## 外观真相源是 VoxelMaterialLibrary（VoxelMaterial + bake 产物 tiles），
## 这里只做「构建 + 发布静态量」：成功 material_version += 1，失败 push_error
## 并保留上一版数组（不破坏正在渲染的节点）。
static func apply_material_library(library: VoxelMaterialLibrary) -> bool:
	if library == null:
		push_error("[VoxelMeshNode] apply_material_library: library 为 null")
		return false
	var arrays := library.build_arrays()
	if arrays == null or not arrays.is_ready():
		push_error("[VoxelMeshNode] 材质库构建失败：%s" % library.describe())
		return false
	material_arrays = arrays
	layer_count = arrays.layer_count
	has_emissive = arrays.has_emissive
	material_version += 1
	return true


## 设置 block→面材质表（256*6 字节，索引 block*6+face），传给 gdext mesher。
## 空表 = Rust 侧 identity 降级（block id 当材质 id），只用于诊断。
static func set_face_material_map(map: PackedByteArray) -> void:
	if map.size() != 256 * 6:
		push_warning("[VoxelMeshNode] face_material_map 长度 %d，期望 %d；mesher 将 identity 降级" % [
			map.size(), 256 * 6,
		])
		face_material_map = PackedByteArray()
		return
	face_material_map = map


# ───────────────────────── ViewLayer 钩子 ─────────────────────────

## 内部矩形上传 helper：lod + 39 bit 矩形流（u64 原样，绝不截断成 32 位）。
## 唯一调用方是同节点的 set_raw_voxel_view；ViewLayer 不直接调用。
func set_mesh_view(lod: int, words: PackedInt64Array) -> void:
	var count := words.size()
	if count <= 0:
		reset_mesh_view()
		return

	# 同一节点被复用且 LOD 变了：启动一次淡入（首次上传不算切换）。
	if _has_mesh and lod != _lod and lod_fade_duration > 0.0:
		_fade_active = true
		_fade_elapsed = 0.0
	_lod = lod
	# 顺序很重要：instance_count 必须先扩容，再设置 visible_instance_count，
	# 否则 Godot 会抛 "p_count > instance_count"。
	if _mm.instance_count != count:
		_mm.instance_count = count
	if _mm.visible_instance_count != count:
		_mm.visible_instance_count = count

	_material.set_shader_parameter("voxel_size", voxel_size)
	_material.set_shader_parameter("block_lod", lod)
	_apply_ao()
	_apply_materials()
	_material.set_shader_parameter("block_origin", global_position)
	_apply_lod_fade(0.0)

	_upload(words, count)
	_has_mesh = true


## 清除矩形（内部 helper；组件被移除时恢复默认呈现）。
func reset_mesh_view() -> void:
	if _mm != null:
		_mm.visible_instance_count = 0
	_has_mesh = false
	_fade_active = false
	if _material != null:
		_material.set_shader_parameter("lod_fade", Vector2(0.0, 0.0))


## 呈现原始体素 halo：调用 gdext 暴露的 Rust mesher 现算 39 bit 矩形流，
## 再复用 set_mesh_view 上传。lod/blocks/skirt_mask 由 ViewLayer 从 RAWVOXELS
## 组件解出；lod_skirts_enabled=false 时把 mask 归零（行为退回无裙边）。
func set_raw_voxel_view(lod: int, blocks: PackedByteArray, skirt_mask: int) -> void:
	if not mesh_fn.is_valid():
		push_warning("[VoxelMeshNode] mesh_fn 未设置，无法网格化 RAWVOXELS halo")
		return
	var effective_mask := skirt_mask if lod_skirts_enabled else 0
	# 第 4 个参数是 Godot 侧维护的 block→面材质表（256*6）；空表时 Rust 侧 identity 降级。
	# 旧扩展（mesh_voxel_halo 只有 3 参）按 has_method 无法区分，用 BevyClient 探测的
	# mesh_supports_face_map 决定，避免逐块报 “Invalid call”。
	var words: PackedInt64Array
	if mesh_supports_face_map:
		words = mesh_fn.call(lod, blocks, effective_mask, face_material_map)
	else:
		words = mesh_fn.call(lod, blocks, effective_mask)
	set_mesh_view(lod, words)


## 清除原始体素呈现（组件被移除时恢复默认）。
func reset_raw_voxel_view() -> void:
	reset_mesh_view()


## 把静态 AO 参数同步到本节点材质（仅在与上次不同时写，避免每帧无谓更新）。
func _apply_ao() -> void:
	if _material == null:
		return
	if _ao_strength != ao_strength:
		_material.set_shader_parameter("ao_strength", ao_strength)
		_ao_strength = ao_strength
	if _ao_power != ao_power:
		_material.set_shader_parameter("ao_power", ao_power)
		_ao_power = ao_power


## 把共享材质 GPU 数据（3~4 张数组 + 参数 LUT）同步到本节点材质：
## 只在 material_version 变化时写 uniform，其余帧只做一次整数比较（热路径不写）。
func _apply_materials() -> void:
	if _material == null or _applied_material_version == material_version:
		return

	var ready := material_arrays != null and material_arrays.is_ready()
	_material.set_shader_parameter("use_material_layers", ready)

	if ready:
		_material.set_shader_parameter("albedo_array", material_arrays.albedo)
		_material.set_shader_parameter("normal_array", material_arrays.normal)
		_material.set_shader_parameter("orm_array", material_arrays.orm)
		_material.set_shader_parameter("pbr_params", material_arrays.params)
		_material.set_shader_parameter("layer_count", material_arrays.layer_count)
		_material.set_shader_parameter("has_emissive", material_arrays.has_emissive)
		if material_arrays.has_emissive and material_arrays.emissive != null:
			_material.set_shader_parameter("emissive_array", material_arrays.emissive)
	elif not _warned_missing_materials:
		_warned_missing_materials = true
		push_warning("[VoxelMeshNode] 材质库未就绪，体素用中性灰兜底（检查 VoxelMaterialLibrary 加载 / bake）")

	_applied_material_version = material_version


func _process(delta: float) -> void:
	# 静态块的 Transform 不常变，但 ViewLayer 可能在本帧任何时刻改写位置。
	if _has_mesh and _material != null:
		_material.set_shader_parameter("block_origin", global_position)
		_apply_ao()
		# 材质数据可能在节点物化之后才装配完：这里只做整数版本比较，未变化时零写入。
		_apply_materials()
		_apply_lod_fade(delta)


## 推进 LOD 交叉淡入。只在 _fade_active 时写 shader，结束后复位成 no-op。
func _apply_lod_fade(delta: float) -> void:
	if _material == null or not _fade_active:
		return
	if lod_fade_duration <= 0.0:
		_fade_active = false
		_material.set_shader_parameter("lod_fade", Vector2(0.0, 0.0))
		return
	_fade_elapsed += delta
	var progress := clampf(_fade_elapsed / lod_fade_duration, 0.0, 1.0)
	# y = 1.0（淡入）：x 从 1.0 降到 0.0，被 discard 的像素越来越少。
	_material.set_shader_parameter("lod_fade", Vector2(1.0 - progress, 1.0))
	if progress >= 1.0:
		_fade_active = false
		_material.set_shader_parameter("lod_fade", Vector2(0.0, 0.0))


func _exit_tree() -> void:
	_free_texture()


func reset() -> void:
	super.reset()
	reset_mesh_view()
	_has_mesh = false


# ───────────────────────── RD 纹理 ─────────────────────────

func _upload(words: PackedInt64Array, count: int) -> void:
	if _rd == null:
		_rd = RenderingServer.get_rendering_device()
	if _rd == null:
		# Compatibility 渲染器没有 RD；GPU 矩形展开需要 Forward+ / Mobile。
		push_warning("[VoxelMeshNode] RenderingDevice 不可用，需要 Forward+ / Mobile 渲染器")
		return

	var width := mini(count, TEX_WIDTH)
	var height := (count + width - 1) / width
	var needed := width * height

	# PackedInt64Array 已经是小端序的 8B/word；直接把位模式原样送 GPU，
	# 着色器端 floatBitsToUint 还原。
	var bytes := words.to_byte_array()
	if bytes.size() != needed * WORD_BYTES:
		# 最后一行可能不满：补零到纹理大小（GPU 只读前 count 个 texel）。
		bytes.resize(needed * WORD_BYTES)

	if _texture_rid.is_valid() and _tex_capacity == needed:
		_rd.texture_update(_texture_rid, 0, bytes)
		return

	var fmt := RDTextureFormat.new()
	fmt.texture_type = RenderingDevice.TEXTURE_TYPE_2D
	# 关键：用 SFLOAT 承载 u64 位模式，Texture2DRD 才认得 (Image::FORMAT_RGF)。
	fmt.format = RenderingDevice.DATA_FORMAT_R32G32_SFLOAT
	fmt.width = width
	fmt.height = height
	fmt.depth = 1
	fmt.array_layers = 1
	fmt.mipmaps = 1
	fmt.usage_bits = (
		RenderingDevice.TEXTURE_USAGE_SAMPLING_BIT
		| RenderingDevice.TEXTURE_USAGE_CAN_UPDATE_BIT
	)

	# 先建新纹理，再把 Texture2DRD 切到新 RID，最后才释放旧 RID。
	# 旧写法先 free_rid 再建新的：材质的 uniform set 还指着已释放的 RID，
	# 渲染线程在这段空隙里绘制就会刷 "Texture (binding: 1, index 0) is not a
	# valid texture" / "Parameter 'us' is null"（binding 1 = rect_data）。
	var old_rid := _texture_rid
	var new_rid := _rd.texture_create(fmt, RDTextureView.new(), [bytes])
	if not new_rid.is_valid():
		push_error("[VoxelMeshNode] RD 纹理创建失败（%d x %d，%d 个矩形）" % [width, height, count])
		return
	_texture_rid = new_rid
	_tex_capacity = needed

	if _texture == null:
		_texture = Texture2DRD.new()
		_texture.texture_rd_rid = new_rid
		_material.set_shader_parameter("rect_data", _texture)
	else:
		# 同一个 Texture2DRD 换底层 RID：引擎会让依赖它的 uniform set 失效重建。
		_texture.texture_rd_rid = new_rid

	if old_rid.is_valid():
		_rd.free_rid(old_rid)

	if debug_self_check and OS.is_debug_build():
		_debug_verify(words)


func _free_texture() -> void:
	# 先让 Texture2DRD 放开底层 RID，再释放 RID；材质可能还引用着这个
	# Texture2DRD 对象，直接 free_rid 会让它指向一个已释放的纹理。
	if _texture != null:
		_texture.texture_rd_rid = RID()
	if _rd != null and _texture_rid.is_valid():
		_rd.free_rid(_texture_rid)
	_texture_rid = RID()
	_texture = null
	_tex_capacity = 0
	_has_mesh = false


## 读回纹理第一个 texel，验证 u64 位模式无损（仅调试，触发 GPU 同步）。
## 注意：R32G32_SFLOAT 下读回的字节是原始位模式，直接当 u64 解码即可。
func _debug_verify(words: PackedInt64Array) -> void:
	if words.is_empty():
		return
	var got: PackedByteArray = _rd.texture_get_data(_texture_rid, 0)
	if got.size() < WORD_BYTES:
		push_error("[VoxelMeshNode] 纹理读回失败")
		return
	var want := words[0] as int
	var got_word := got.decode_s64(0)
	if got_word != want:
		push_error("[VoxelMeshNode] 第一个矩形位模式不一致: want=%d got=%d" % [want, got_word])
