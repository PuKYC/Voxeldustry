class_name VoxelMeshNode
extends BevyEntityNode
## 体素绘制节点：每块一个 MultiMeshInstance3D，逐顶点在 GPU 上把
## 39 bit 贪心矩形展开成四边形（见 shaders/voxel_rect.gdshader）。
##
## 8 B/矩形只有一份：R32G32_UINT 纹理，由 RenderingDevice 创建 / 更新
## （Texture2DRD 绑定给 usampler2D）。GDScript 每块只做一次
## texture_update + instance_count 赋值，**绝不逐矩形循环**。
##
## 数据来源：同一实体上的 Transform 载荷给出块原点（米），RAWVOXELS 载荷给出
## lod + 34³ 原始体素；gdext mesher（mesh_fn）现算矩形流后上传。
## 几何数学的真相源是 game-core::presentation::voxel_mesh。

const SHADER_PATH := "res://bevy_client/shaders/voxel_rect.gdshader"
const WORD_BYTES := 8
## 每行最多多少个矩形（RD 纹理宽度上限远大于此）。
const TEX_WIDTH := 1024

## 全局共享：调色板纹理（256 x 1 RGBA8，方块 id -> 线性 RGBA）与体素边长。
## BevyClient 启动时从 get_voxel_table() 拉一次并写入。
static var palette_texture: Texture2D = null
static var voxel_size: float = 0.45
## RAWVOXELS：由 BevyClient 装配的 gdext mesher 桥（manager.mesh_voxel_halo）。
## 无效时 set_raw_voxel_view 只告警，不做网格化。
static var mesh_fn: Callable = Callable()
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


# ───────────────────────── ViewLayer 钩子 ─────────────────────────

## 内部矩形上传 helper：lod + 39 bit 矩形流（u64 原样，绝不截断成 32 位）。
## 唯一调用方是同节点的 set_raw_voxel_view；ViewLayer 不直接调用。
func set_mesh_view(lod: int, words: PackedInt64Array) -> void:
	var count := words.size()
	if count <= 0:
		reset_mesh_view()
		return

	_lod = lod
	if _mm.visible_instance_count != count:
		_mm.visible_instance_count = count
	if _mm.instance_count != count:
		_mm.instance_count = count

	_material.set_shader_parameter("voxel_size", voxel_size)
	_material.set_shader_parameter("block_lod", lod)
	if palette_texture != null:
		_material.set_shader_parameter("palette", palette_texture)
	_material.set_shader_parameter("block_origin", global_position)

	_upload(words, count)
	_has_mesh = true


## 清除矩形（内部 helper；组件被移除时恢复默认呈现）。
func reset_mesh_view() -> void:
	if _mm != null:
		_mm.visible_instance_count = 0
	_has_mesh = false


## 呈现原始体素 halo：调用 gdext 暴露的 Rust mesher 现算 39 bit 矩形流，
## 再复用 set_mesh_view 上传。lod/blocks 由 ViewLayer 从 RAWVOXELS 组件解出。
func set_raw_voxel_view(lod: int, blocks: PackedByteArray) -> void:
	if not mesh_fn.is_valid():
		push_warning("[VoxelMeshNode] mesh_fn 未设置，无法网格化 RAWVOXELS halo")
		return
	var words: PackedInt64Array = mesh_fn.call(lod, blocks)
	set_mesh_view(lod, words)


## 清除原始体素呈现（组件被移除时恢复默认）。
func reset_raw_voxel_view() -> void:
	reset_mesh_view()


func _process(_delta: float) -> void:
	# 静态块的 Transform 不常变，但 ViewLayer 可能在本帧任何时刻改写位置。
	if _has_mesh and _material != null:
		_material.set_shader_parameter("block_origin", global_position)


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

	var bytes := words.to_byte_array()
	if bytes.size() != needed * WORD_BYTES:
		# 最后一行可能不满：补零到纹理大小（GPU 只读前 count 个 texel）。
		bytes.resize(needed * WORD_BYTES)

	if _texture_rid.is_valid() and _tex_capacity == needed:
		_rd.texture_update(_texture_rid, 0, bytes)
		return

	_free_texture()

	var fmt := RDTextureFormat.new()
	fmt.texture_type = RenderingDevice.TEXTURE_TYPE_2D
	fmt.format = RenderingDevice.DATA_FORMAT_R32G32_UINT
	fmt.width = width
	fmt.height = height
	fmt.depth = 1
	fmt.array_layers = 1
	fmt.mipmaps = 1
	fmt.usage_bits = (
		RenderingDevice.TEXTURE_USAGE_SAMPLING_BIT
		| RenderingDevice.TEXTURE_USAGE_CAN_UPDATE_BIT
	)

	_texture_rid = _rd.texture_create(fmt, RDTextureView.new(), [bytes])
	if not _texture_rid.is_valid():
		push_error("[VoxelMeshNode] RD 纹理创建失败（%d x %d，%d 个矩形）" % [width, height, count])
		return
	_tex_capacity = needed

	_texture = Texture2DRD.new()
	_texture.texture_rd_rid = _texture_rid
	_material.set_shader_parameter("rect_data", _texture)

	if debug_self_check and OS.is_debug_build():
		_debug_verify(words)


func _free_texture() -> void:
	if _rd != null and _texture_rid.is_valid():
		_rd.free_rid(_texture_rid)
	_texture_rid = RID()
	_texture = null
	_tex_capacity = 0
	_has_mesh = false


## 读回纹理第一个 texel，验证 u64 位模式无损（仅调试，触发 GPU 同步）。
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
