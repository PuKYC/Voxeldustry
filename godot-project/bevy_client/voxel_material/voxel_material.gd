class_name VoxelMaterial
extends Resource
## 体素材质定义：**外观数据的唯一真相源（Godot 侧）**。
##
## 两级结构（见 docs/项目架构/06-体素世界与材质.md「材质管线」）：
## 1. **作者参数**（本文件上半部分）：颜色 / 图案 / 粗糙度 / 金属度 / 自发光 / UV / Alpha，
##    全部在检查器里可编辑，是 bake 工具的输入；
## 2. **烘焙产物** `tiles: VoxelMaterialTiles`：bake 出来的四张线性 Image，
##    运行时只读，不再有任何逐像素 GDScript 生成。
##
## 契约：
## - **数组下标 == material id == layer 索引**；本资源没有 id 字段，避免顺序/编号两套真相；
## - block → 面材质的绑定在 VoxelBlockMaterial（同样是 Godot 资源），Rust 不再知道材质；
## - material_key 是稳定键（mod 覆盖 / 语义 / 调试用），启动期只校验唯一性；
## - 颜色一律是 **sRGB 作者色**；bake 时转线性字节，shader 不加 source_color；
## - uv_scale / alpha_scissor / emission_energy 不进逐像素常量，而是烘焙进一张 1 行 LUT
##   （VoxelMaterialArrays.params）；normal_strength / occlusion / 图案强度在 bake 时
##   烘进纹理，运行时不产生额外分支。

## 程序化图案族（bake 工具的输入；运行时不再生成）。
enum Pattern {
	## 纯色 + 极轻颗粒（雪、空气）。
	NONE,
	## 颗粒 / 斑点（石、土、沙）。
	SPECKLE,
	## 方向性条纹（木纹）。
	GRAIN,
	## 草方块顶面：草色 + 斑点。
	GRASS_TOP,
	## 草方块侧面：土底 + 顶部一条草边（高行 = 面顶部）。
	GRASS_SIDE,
	## 树叶：深浅斑块 + 少量暗块。
	LEAFY,
}

@export_group("Identity")
## 稳定键（mod 覆盖 / 调试 / 日志用），在库内唯一。
@export var material_key: String = ""
## 人类可读标签（UI / 调试 / 检查器显示），不参与渲染契约。
@export var display_name: String = ""

@export_group("Baked")
## bake 产物（tools/bake_voxel_materials.gd 生成）。为空时运行时会报错并跳过该库。
@export var tiles: VoxelMaterialTiles = null

@export_group("Albedo")
## 可选 albedo 源纹理（仅 bake 读取）。为空时按 albedo_color + albedo_pattern 生成。
@export var albedo_texture: Texture2D = null
## albedo 源纹理的字节空间：true = sRGB 编码（美术 PNG 默认），bake 时转线性；
## false = 已是线性值（HDR / 数据图）。
@export var albedo_texture_srgb: bool = true
## sRGB 作者色。无纹理 = 纯色；有纹理 = 线性域乘色（tint）。
@export var albedo_color: Color = Color.WHITE
## 无 albedo_texture 时使用的程序化图案。
@export var albedo_pattern: Pattern = Pattern.NONE
## 图案的次要 sRGB 作者色（草侧的草色；LEAFY 的亮斑色等）。
@export var pattern_secondary_color: Color = Color.WHITE
## 图案频率倍数：1.0 = 每贴图约 8 个粗颗粒单元；>1 更细，<1 更粗。
@export var pattern_scale: float = 1.0
## 图案明度扰动幅度：0 = 死平，1 = 默认（约 ±12% sRGB）。
@export var pattern_strength: float = 1.0
## GRASS_SIDE 专用：顶部被草覆盖的高度比例（0..1）。
@export var pattern_band: float = 0.2
## 图案噪声种子（同参数 + 同种子 = 逐字节确定）。
@export var pattern_seed: int = 0

@export_group("Normal")
## 可选法线源纹理（切线空间，OpenGL +Y；仅 bake 读取）。
@export var normal_texture: Texture2D = null
## 法线强度：0 = 平，1 = 原始纹理强度；bake 时缩放 RG。
@export var normal_strength: float = 1.0
## 源资源若是 DirectX 风格（绿通道朝下）则勾选，bake 时翻转 G。
@export var normal_flip_green: bool = false
## 无法线源纹理时：true = 从 albedo 明度高度场差分生成；false = 全平（128,128）。
@export var normal_from_albedo: bool = true

@export_group("Roughness / Metallic / AO")
## 可选 ORM 源纹理：R = AO、G = roughness、B = metallic（线性数据，仅 bake 读取）。
@export var orm_texture: Texture2D = null
## 粗糙度（0 = 镜面，1 = 完全粗糙）。有 orm_texture 时作为 G 通道乘数。
@export var roughness: float = 1.0
## 金属度（0 = 非金属，1 = 纯金属）。有 orm_texture 时作为 B 通道乘数。
@export var metallic: float = 0.0
## AO 乘数（0..1）：烘进 ORM.R，和顶点 AO 相乘。1 = 不额外压暗。
@export var occlusion: float = 1.0

@export_group("Emission")
## 可选自发光源纹理（仅 bake 读取）。为空时用 emission_color 纯色。
@export var emission_texture: Texture2D = null
## 自发光源纹理的字节空间：true = sRGB 编码；false = 已是线性。
@export var emission_texture_srgb: bool = true
## sRGB 自发光作者色；黑色 = 不发光。
@export var emission_color: Color = Color.BLACK
## 自发光能量（线性域乘数，可 >1）。0 = 关闭该材质自发光（LUT 里写 0）。
@export var emission_energy: float = 1.0

@export_group("UV / Alpha")
## 每材质 UV 缩放（乘以 library.uv_scale）。1 = 每个体素一个 tile。
@export var uv_scale: float = 1.0
## Alpha 裁剪阈值（0 = 关闭）：仅做 cutout（discard），不走透明混合。
@export var alpha_scissor: float = 0.0


## 是否已 bake（运行时必须有 tiles 才能建数组）。
func has_baked_tiles() -> bool:
	return tiles != null and tiles.is_ready()


## 该材质是否会写 EMISSION（决定 library 是否需要第 4 张数组）。
func is_emissive() -> bool:
	if emission_energy <= 0.0:
		return false
	if tiles != null:
		return tiles.emissive_enabled
	return emission_color.r > 0.0 or emission_color.g > 0.0 or emission_color.b > 0.0


## 自检：作者参数层面的问题（空 = 通过）。bake 产物完整性另外检查。
func validate() -> PackedStringArray:
	var errors := PackedStringArray()
	if material_key.is_empty():
		errors.append("material_key 不能为空")
	if not (is_finite(uv_scale) and uv_scale > 0.0):
		errors.append("uv_scale 必须是有限正数（当前 %s）" % uv_scale)
	if alpha_scissor < 0.0 or alpha_scissor > 1.0:
		errors.append("alpha_scissor 必须在 [0,1]（当前 %s）" % alpha_scissor)
	if emission_energy < 0.0:
		errors.append("emission_energy 不能为负（当前 %s）" % emission_energy)
	if normal_strength < 0.0:
		errors.append("normal_strength 不能为负（当前 %s）" % normal_strength)
	if occlusion < 0.0 or occlusion > 1.0:
		errors.append("occlusion 必须在 [0,1]（当前 %s）" % occlusion)
	if roughness < 0.0 or roughness > 1.0:
		errors.append("roughness 必须在 [0,1]（当前 %s）" % roughness)
	if metallic < 0.0 or metallic > 1.0:
		errors.append("metallic 必须在 [0,1]（当前 %s）" % metallic)
	return errors
