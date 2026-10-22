class_name VoxelMaterialTiles
extends Resource
## 一个材质**烘焙后**的四张瓦片数据（bake 产物，运行时只读）。
##
## 为什么存 .res（Image 资源）而不是 PNG：
## - normal / orm 是**线性数据**，走 Godot 纹理导入会被默认 sRGB / VRAM 压缩二次污染，
##   关压缩又需要每个文件单独的 .import 配置；.res 保存的是逐字节精确的 Image，
##   编辑器 / CI / headless / 导出包行为完全一致；
## - Image 资源不经过 import 流程，`--headless --script` 与导出包都能直接 load；
## - 美术输入仍可用 PNG：bake 工具从 art/ 目录读 PNG 源图，PNG 只做输入/预览，
##   不进入运行时。
##
## 字节契约（与 shader / VoxelMaterialLibrary.build_arrays 一致）：
## - albedo   FORMAT_RGBA8，**线性**光值（sRGB 作者色在 bake 时转换），A 是裁剪用的 alpha；
## - normal   FORMAT_RG8，线性，OpenGL +Y（128,128 = 平），第 0 行 = v = 0 = 面底部；
## - orm      FORMAT_RGBA8，R = AO、G = roughness、B = metallic、A = 255（全线性）；
## - emissive FORMAT_RGBA8，线性 RGB（黑 = 不发光）、A = 255；
## - 四张图同边长、**不带 mipmap**（mip 由 Library 统一生成，保证格式一致）。

@export var albedo: Image = null
@export var normal: Image = null
@export var orm: Image = null
@export var emissive: Image = null
## bake 时判定：该材质的 emissive 是否有非黑内容（决定是否建第 4 张数组）。
@export var emissive_enabled: bool = false


## 是否具备可用数据（尺寸 / 格式基本自检；详细校验由 Library 做）。
func is_ready() -> bool:
	if albedo == null or normal == null or orm == null or emissive == null:
		return false
	var size := albedo.get_width()
	if size <= 0 or albedo.get_height() != size:
		return false
	if albedo.get_format() != Image.FORMAT_RGBA8:
		return false
	if normal.get_format() != Image.FORMAT_RG8:
		return false
	if orm.get_format() != Image.FORMAT_RGBA8 or emissive.get_format() != Image.FORMAT_RGBA8:
		return false
	return (
		normal.get_width() == size
		and orm.get_width() == size
		and emissive.get_width() == size
		and normal.get_height() == size
		and orm.get_height() == size
		and emissive.get_height() == size
	)


func describe() -> String:
	if albedo == null:
		return "VoxelMaterialTiles(empty)"
	return "VoxelMaterialTiles(%dx%d, emissive=%s)" % [albedo.get_width(), albedo.get_height(), emissive_enabled]
