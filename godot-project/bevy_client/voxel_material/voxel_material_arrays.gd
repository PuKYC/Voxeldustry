class_name VoxelMaterialArrays
extends RefCounted
## 一次构建好的体素材质 GPU 数据：3~4 张 Texture2DArray + 1 张参数 LUT。
##
## 由 VoxelMaterialLibrary.build_arrays() 产出，VoxelMeshNode.apply_material_library()
## 持有并写进共享 ShaderMaterial。构建完成后**不再增长**，可以安全地在所有块之间共享。
##
## 纹理约定（详见 docs/项目架构/06-体素世界与材质.md「数组与格式」）：
## - 四张数组同尺寸（tile_size）、同层数（layer_count）、同 mip 状态，layer = 材质 id；
## - albedo / emissive 是**线性** RGBA8（构建时已从 sRGB 作者色转换，shader 不加
##   source_color）；normal 是线性 RG8（OpenGL +Y，z 由引擎重建）；orm 是线性 RGBA8
##   （R = AO、G = roughness、B = metallic）；
## - has_emissive = false 时 emissive 为 null（第 4 张数组不建不绑，省带宽/显存）；
## - params.params 是 width = layer_count、height = 1 的 RGBAH LUT，
##   texel(layer) = (uv_scale, alpha_scissor, emission_energy, 0)，
##   shader 用 texelFetch 读——所有逐材质标量都从这里走，不占逐帧 uniform。

## 3 张常用数组（必选）。
var albedo: Texture2DArray = null
var normal: Texture2DArray = null
var orm: Texture2DArray = null
## 可选第 4 张（仅 has_emissive 时存在）。
var emissive: Texture2DArray = null
## RGBAH 参数 LUT（见类注释）。
var params: Texture2D = null
## 层数（== 材质数 == Texture2DArray depth）。
var layer_count: int = 0
## 任一材质自发光。
var has_emissive: bool = false
## 单层边长（像素）。
var tile_size: int = 0
## 每层 binding_key（构建时快照，供校验 / 调试）。
var material_keys: PackedStringArray = PackedStringArray()
## 每层的参数（与 params LUT 同源；测试 / 调试读取，GPU 走 LUT）。
var layer_params: Array[Vector4] = []


## GPU 数据是否齐全（着色器可以进入 PBR 路径，不需要回退）。
func is_ready() -> bool:
	if layer_count <= 0 or tile_size <= 0:
		return false
	if albedo == null or normal == null or orm == null or params == null:
		return false
	if albedo.get_layers() != layer_count:
		return false
	if normal.get_layers() != layer_count or orm.get_layers() != layer_count:
		return false
	if has_emissive and emissive == null:
		return false
	return true


## 第 layer 层的参数（uv_scale, alpha_scissor, emission_energy, 0）。
func params_of(layer: int) -> Vector4:
	if layer < 0 or layer >= layer_params.size():
		return Vector4.ONE
	return layer_params[layer]


## LUT 的单像素编码：写入 GPU 前唯一的一处编码定义。
static func encode_params(uv_scale: float, alpha_scissor: float, emission_energy: float) -> Color:
	return Color(uv_scale, alpha_scissor, emission_energy, 0.0)


## 构建参数 LUT 的 Image（RGBAH，width = layers，height = 1，no mip / NEAREST）。
static func build_param_image(values: Array[Vector4]) -> Image:
	var count := maxi(values.size(), 1)
	var image := Image.create(count, 1, false, Image.FORMAT_RGBAH)
	for i in values.size():
		var v := values[i]
		image.set_pixel(i, 0, encode_params(v.x, v.y, v.z))
	return image


## 供日志 / 测试的一行摘要。
func describe() -> String:
	return "VoxelMaterialArrays(layers=%d tile=%d has_emissive=%s keys=%s)" % [
		layer_count, tile_size, has_emissive, ",".join(material_keys),
	]
