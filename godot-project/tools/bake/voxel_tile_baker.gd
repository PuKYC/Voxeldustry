class_name VoxelTileBaker
extends RefCounted
## 体素材质瓦片烘焙器：VoxelMaterial 作者参数 -> VoxelMaterialTiles（4 张线性 Image）。
##
## 只被 tools/bake_voxel_materials.gd 在离线 / CI 阶段调用；运行时不再生成任何像素。
##
## 契约（docs/项目架构/06-体素世界与材质.md「烘焙管线」）：
## - 确定性：仅整数 splitmix64 哈希（无 rand / 无系统时间），同参数 + 同 seed 逐字节相同；
## - 可平铺：所有噪声坐标按 size 环绕（cell / stripe 周期都整除 size）；
## - 方向：Image 第 0 行 = 纹理 v=0 = 面底部，草侧的草边画在**高行**（面顶部）；
## - 色彩：作者色是 sRGB，albedo / emissive 在 bake 时转线性字节（alpha 不转换）；
## - 格式：albedo RGBA8 / normal RG8 / orm RGBA8 / emissive RGBA8，均不带 mipmap；
## - 有源图时优先 res://art/voxel_materials/<key>_<suffix>.png，其次资源里的 Texture2D。
##
## 旧 Rust 实现（game-core::presentation::voxel_pbr，V1 已废弃并删除）的算法被
## 1:1 移植到这里：splitmix64 + 四个盐值 + 粗/细两组 signed_noise + 千分比 shade +
## 3x3 环绕平滑高度场 + 依据图案族取不同 divisor 的法线差分。

## 美术源图目录（PNG 只做输入 / 预览，不进运行时）。
const SRC_ROOT := "res://art/voxel_materials"
const RGBA8 := Image.FORMAT_RGBA8
const RG8 := Image.FORMAT_RG8

# splitmix64 常量。GDScript int 是 64 位有符号，> 2^63 的十六进制字面量不可解析，
# 因此写成等价的十进制负数；乘法溢出按 64 位补码回绕（Game 侧已验证）。
const MIX_GOLDEN := -7046029254386353131 # 0x9E3779B97F4A7C15
const MIX_MUL1 := -4658895280553007687 # 0xBF58476D1CE4E5B9
const MIX_MUL2 := -7723592293110705685 # 0x94D049BB133111EB

# 盐值（旧 Rust 常量；按 32 位量参与异或）。
const SALT_FINE := 608135816 # 0x243F6A88
const SALT_COARSE := 320440878 # 0x13198A2E
const SALT_GRAIN := 2752224802 # 0xA4093822
const SALT_EDGE := 137229464 # 0x082EFA98

## shade 的全局钳制范围（千分比，约 ±12% sRGB）。
const SHADE_LIMIT := 120


## 烘焙一个材质的四张瓦片（size x size，不带 mipmap）。
static func build(material: VoxelMaterial, size: int) -> VoxelMaterialTiles:
	assert(material != null, "VoxelTileBaker.build: material 不能为空")
	assert(size >= 8, "VoxelTileBaker.build: size 至少 8（当前 %d）" % size)
	var layer := _layer_salt(material)
	var albedo_result := _build_albedo(material, size, layer)
	var tiles := VoxelMaterialTiles.new()
	tiles.albedo = albedo_result["image"]
	tiles.normal = _build_normal(material, size, albedo_result["height"])
	tiles.orm = _build_orm(material, size)
	tiles.emissive = _build_emissive(material, size)
	tiles.emissive_enabled = material.emission_energy > 0.0 and _has_non_black(tiles.emissive)
	return tiles


# ───────────────────────── Albedo + 高度场 ─────────────────────────

## 返回 { image: Image(RGBA8 线性), height: PackedFloat32Array }。
static func _build_albedo(material: VoxelMaterial, size: int, layer: int) -> Dictionary:
	var source := _load_source_image(material.material_key, "albedo")
	if source == null and material.albedo_texture != null:
		source = material.albedo_texture.get_image()
	if source != null:
		return _albedo_from_texture(material, _fit(source, size, RGBA8))
	return _albedo_procedural(material, size, layer)


static func _albedo_from_texture(material: VoxelMaterial, source: Image) -> Dictionary:
	var size := source.get_width()
	var data := PackedByteArray()
	data.resize(size * size * 4)
	var height := PackedFloat32Array()
	height.resize(size * size)
	# 作者色是 sRGB：先转线性，再与已线性化的源图相乘（alpha 不转换）。
	var tint := material.albedo_color.srgb_to_linear()
	for row in size:
		for col in size:
			var c := source.get_pixel(col, row)
			var lin := c.srgb_to_linear() if material.albedo_texture_srgb else c
			var r := clampf(lin.r * tint.r, 0.0, 1.0)
			var g := clampf(lin.g * tint.g, 0.0, 1.0)
			var b := clampf(lin.b * tint.b, 0.0, 1.0)
			var a := clampf(c.a * material.albedo_color.a, 0.0, 1.0)
			var offset := (row * size + col) * 4
			data[offset] = _to_byte(r)
			data[offset + 1] = _to_byte(g)
			data[offset + 2] = _to_byte(b)
			data[offset + 3] = _to_byte(a)
			# 高度场：线性亮度（供 normal_from_albedo 的差分使用）。
			height[row * size + col] = (0.2126 * r + 0.7152 * g + 0.0722 * b) * 1000.0
	var image := Image.create_from_data(size, size, false, RGBA8, data)
	return {"image": image, "height": height}


static func _albedo_procedural(material: VoxelMaterial, size: int, layer: int) -> Dictionary:
	var data := PackedByteArray()
	data.resize(size * size * 4)
	var height := PackedFloat32Array()
	height.resize(size * size)

	var strength := maxf(material.pattern_strength, 0.0)
	var scale := maxf(material.pattern_scale, 0.01)
	var band := clampf(material.pattern_band, 0.0, 1.0)
	var cell := maxi(int(size / (8.0 * scale)), 2)
	var grass_band := maxi(int(round(float(size) * band)), 1)
	var base_bytes := _color_bytes(material.albedo_color)
	var secondary_bytes := _color_bytes(material.pattern_secondary_color)

	for row in size:
		for col in size:
			var fine := _signed_noise(col, row, layer, SALT_FINE)
			var coarse := _signed_noise(int(col / float(cell)), int(row / float(cell)), layer, SALT_COARSE)
			var shade := 0
			var color_bytes := base_bytes
			match material.albedo_pattern:
				VoxelMaterial.Pattern.NONE:
					shade = fine * 30 / 1000
				VoxelMaterial.Pattern.SPECKLE:
					shade = (coarse + fine * 2) / 3 * 120 / 1000
				VoxelMaterial.Pattern.GRAIN:
					var sw := _divisor_near(size, float(size) / (8.0 * scale))
					var stripe := ((col % sw) * 2 - (sw - 1)) * 1000 / sw
					var streak := _signed_noise(int(col / 2.0), int(row / float(cell)), layer, SALT_GRAIN)
					shade = (stripe + streak * 2 + fine * 2) / 5 * 110 / 1000
				VoxelMaterial.Pattern.GRASS_TOP:
					var noise := (coarse + fine * 2) / 3
					if fine > 850:
						noise = mini(noise + 350, 1000)
					shade = noise * 100 / 1000
				VoxelMaterial.Pattern.GRASS_SIDE:
					var jitter := posmod(_hash_at(col, 0, layer, SALT_EDGE), 3) - 1
					var boundary := size - grass_band + jitter
					if row >= boundary:
						color_bytes = secondary_bytes
						shade = (coarse + fine * 2) / 3 * 90 / 1000
					else:
						shade = (coarse + fine * 2) / 3 * 120 / 1000
				VoxelMaterial.Pattern.LEAFY:
					var noise := coarse
					if absi(coarse) <= 300:
						noise = (coarse + fine * 2) / 3
					shade = noise * 120 / 1000
					if fine < -820:
						shade = -120
			# pattern_strength：0 = 死平，1 = 旧实现的默认幅度。
			shade = clampi(int(round(float(shade) * strength)), -SHADE_LIMIT, SHADE_LIMIT)
			height[row * size + col] = float(shade)
			var srgb := _shaded_srgb(color_bytes, shade)
			var lin := srgb.srgb_to_linear()
			var offset := (row * size + col) * 4
			data[offset] = _to_byte(lin.r)
			data[offset + 1] = _to_byte(lin.g)
			data[offset + 2] = _to_byte(lin.b)
			data[offset + 3] = _to_byte(srgb.a)
	var image := Image.create_from_data(size, size, false, RGBA8, data)
	return {"image": image, "height": height}


# ───────────────────────── Normal ─────────────────────────

static func _build_normal(
	material: VoxelMaterial, size: int, height: PackedFloat32Array
) -> Image:
	var source := _load_source_image(material.material_key, "normal")
	if source == null and material.normal_texture != null:
		source = material.normal_texture.get_image()
	if source != null:
		return _normal_from_texture(material, _fit(source, size, RGBA8))
	var strength := maxf(material.normal_strength, 0.0)
	if not material.normal_from_albedo or strength <= 0.0:
		return _flat_normal(size)
	return _normal_from_height(height, size, strength, _normal_divisor(material.albedo_pattern))


static func _normal_from_texture(material: VoxelMaterial, source: Image) -> Image:
	var size := source.get_width()
	var data := PackedByteArray()
	data.resize(size * size * 2)
	var strength := maxf(material.normal_strength, 0.0)
	for row in size:
		for col in size:
			var c := source.get_pixel(col, row)
			var r := c.r
			var g := c.g
			if material.normal_flip_green:
				g = 1.0 - g
			# normal_strength 乘在 (rg*2-1) 上，再以 128 为基线编码。
			var nx := clampf((r * 2.0 - 1.0) * strength, -1.0, 1.0)
			var ny := clampf((g * 2.0 - 1.0) * strength, -1.0, 1.0)
			var offset := (row * size + col) * 2
			data[offset] = _to_byte(nx * 0.5 + 0.5)
			data[offset + 1] = _to_byte(ny * 0.5 + 0.5)
	return Image.create_from_data(size, size, false, RG8, data)


## 旧 Rust 的 normal 生成：3x3 环绕盒式平滑 / 9，中心差分后按图案族 divisor 压成
## 0..255（128 = 平），normal_strength 乘在 (rg*2-1) 上再编码。
static func _normal_from_height(
	height: PackedFloat32Array, size: int, strength: float, divisor: int
) -> Image:
	var smooth := PackedFloat32Array()
	smooth.resize(size * size)
	for row in size:
		for col in size:
			var sum := 0.0
			for dy in range(-1, 2):
				var yy := posmod(row + dy, size) * size
				for dx in range(-1, 2):
					sum += height[yy + posmod(col + dx, size)]
			smooth[row * size + col] = sum / 9.0

	var data := PackedByteArray()
	data.resize(size * size * 2)
	var div := float(maxi(divisor, 1))
	for row in size:
		for col in size:
			# dhdu / dhdv：右-左、上-下（环绕取样）。
			var right := smooth[row * size + posmod(col + 1, size)]
			var left := smooth[row * size + posmod(col + size - 1, size)]
			var up := smooth[posmod(row + 1, size) * size + col]
			var down := smooth[posmod(row + size - 1, size) * size + col]
			var dhdu := right - left
			var dhdv := up - down
			# Rust 整数除法向零截断；这里同样先转 int 再减，保证 0 附近稳定。
			var nx := clampi(128 - int(dhdu / div), 0, 255)
			var ny := clampi(128 - int(dhdv / div), 0, 255)
			var sx := clampf((float(nx) / 255.0 * 2.0 - 1.0) * strength, -1.0, 1.0)
			var sy := clampf((float(ny) / 255.0 * 2.0 - 1.0) * strength, -1.0, 1.0)
			var offset := (row * size + col) * 2
			data[offset] = _to_byte(sx * 0.5 + 0.5)
			data[offset + 1] = _to_byte(sy * 0.5 + 0.5)
	return Image.create_from_data(size, size, false, RG8, data)


static func _flat_normal(size: int) -> Image:
	var data := PackedByteArray()
	data.resize(size * size * 2)
	data.fill(128)
	return Image.create_from_data(size, size, false, RG8, data)


static func _normal_divisor(pattern: VoxelMaterial.Pattern) -> int:
	match pattern:
		VoxelMaterial.Pattern.NONE:
			return 40
		VoxelMaterial.Pattern.SPECKLE:
			return 6
		VoxelMaterial.Pattern.GRAIN:
			return 6
		VoxelMaterial.Pattern.GRASS_TOP:
			return 10
		VoxelMaterial.Pattern.GRASS_SIDE:
			return 12
		VoxelMaterial.Pattern.LEAFY:
			return 6
		_:
			return 6


# ───────────────────────── ORM / Emissive ─────────────────────────

static func _build_orm(material: VoxelMaterial, size: int) -> Image:
	var occlusion := clampf(material.occlusion, 0.0, 1.0)
	var roughness := clampf(material.roughness, 0.0, 1.0)
	var metallic := clampf(material.metallic, 0.0, 1.0)
	var source := _load_source_image(material.material_key, "orm")
	if source == null and material.orm_texture != null:
		source = material.orm_texture.get_image()
	var data := PackedByteArray()
	data.resize(size * size * 4)
	if source != null:
		var img := _fit(source, size, RGBA8)
		for row in size:
			for col in size:
				var c := img.get_pixel(col, row)
				var offset := (row * size + col) * 4
				data[offset] = _to_byte(clampf(c.r * occlusion, 0.0, 1.0))
				data[offset + 1] = _to_byte(clampf(c.g * roughness, 0.0, 1.0))
				data[offset + 2] = _to_byte(clampf(c.b * metallic, 0.0, 1.0))
				data[offset + 3] = 255
	else:
		var r := _to_byte(occlusion)
		var g := _to_byte(roughness)
		var b := _to_byte(metallic)
		for i in size * size:
			var offset := i * 4
			data[offset] = r
			data[offset + 1] = g
			data[offset + 2] = b
			data[offset + 3] = 255
	return Image.create_from_data(size, size, false, RGBA8, data)


static func _build_emissive(material: VoxelMaterial, size: int) -> Image:
	var source := _load_source_image(material.material_key, "emissive")
	if source == null and material.emission_texture != null:
		source = material.emission_texture.get_image()
	var data := PackedByteArray()
	data.resize(size * size * 4)
	if source != null:
		var img := _fit(source, size, RGBA8)
		for row in size:
			for col in size:
				var c := img.get_pixel(col, row)
				var lin := c.srgb_to_linear() if material.emission_texture_srgb else c
				var offset := (row * size + col) * 4
				data[offset] = _to_byte(clampf(lin.r, 0.0, 1.0))
				data[offset + 1] = _to_byte(clampf(lin.g, 0.0, 1.0))
				data[offset + 2] = _to_byte(clampf(lin.b, 0.0, 1.0))
				data[offset + 3] = 255
	elif material.emission_energy > 0.0:
		var lin := material.emission_color.srgb_to_linear()
		var r := _to_byte(clampf(lin.r, 0.0, 1.0))
		var g := _to_byte(clampf(lin.g, 0.0, 1.0))
		var b := _to_byte(clampf(lin.b, 0.0, 1.0))
		for i in size * size:
			var offset := i * 4
			data[offset] = r
			data[offset + 1] = g
			data[offset + 2] = b
			data[offset + 3] = 255
	else:
		data.fill(0)
		for i in size * size:
			data[i * 4 + 3] = 255
	return Image.create_from_data(size, size, false, RGBA8, data)


# ───────────────────────── 通用 helper ─────────────────────────

## 有源图时统一：去 mip、转目标格式、resize 到 size（双线性）。
static func _fit(source: Image, size: int, format: Image.Format) -> Image:
	var img := source.duplicate() as Image
	if img.has_mipmaps():
		img.clear_mipmaps()
	if img.get_format() != format:
		img.convert(format)
	if img.get_width() != size or img.get_height() != size:
		img.resize(size, size, Image.INTERPOLATE_BILINEAR)
	return img


static func _load_source_image(material_key: String, suffix: String) -> Image:
	if material_key.is_empty():
		return null
	var path := SRC_ROOT.path_join(material_key + "_" + suffix + ".png")
	if not FileAccess.file_exists(path):
		return null
	return Image.load_from_file(path)


static func _layer_salt(material: VoxelMaterial) -> int:
	# build() 的固定签名没有 layer 参数：用 material_key 的稳定哈希 + pattern_seed
	# 派生一个 0..255 的层盐（同一材质跨运行稳定；不同材质噪声不同）。
	var key := 0
	if not material.material_key.is_empty():
		key = material.material_key.hash()
	return posmod(key + material.pattern_seed, 256)


static func _color_bytes(c: Color) -> PackedInt32Array:
	return PackedInt32Array([
		_to_int_byte(c.r), _to_int_byte(c.g), _to_int_byte(c.b), _to_int_byte(c.a),
	])


## 千分比明度扰动：channel * (1000 + shade) / 1000，整数截断（同旧 Rust）。
static func _shaded_srgb(bytes: PackedInt32Array, shade: int) -> Color:
	return Color(
		_shade_channel(bytes[0], shade) / 255.0,
		_shade_channel(bytes[1], shade) / 255.0,
		_shade_channel(bytes[2], shade) / 255.0,
		bytes[3] / 255.0
	)


static func _shade_channel(channel: int, shade: int) -> int:
	return clampi((channel * (1000 + shade)) / 1000, 0, 255)


static func _to_int_byte(value: float) -> int:
	return clampi(roundi(value * 255.0), 0, 255)


static func _to_byte(value: float) -> int:
	return clampi(roundi(clampf(value, 0.0, 1.0) * 255.0), 0, 255)


static func _has_non_black(image: Image) -> bool:
	var data := image.get_data()
	var pixel_count := image.get_width() * image.get_height()
	for i in pixel_count:
		if data[i * 4] != 0 or data[i * 4 + 1] != 0 or data[i * 4 + 2] != 0:
			return true
	return false


# ───────────────────────── splitmix64 / signed_noise ─────────────────────────

## 逻辑右移：GDScript 的 >> 只接受非负操作数，这里用“清符号位 + 补最高位”模拟 u64 右移。
static func _ushr(value: int, shift: int) -> int:
	var result := (value & 0x7FFFFFFFFFFFFFFF) >> shift
	if value < 0:
		result |= 1 << (63 - shift)
	return result


## splitmix64：加法 / 乘法按 64 位补码回绕，右移用 _ushr。
static func _mix64(value: int) -> int:
	var v := value + MIX_GOLDEN
	v = (v ^ _ushr(v, 30)) * MIX_MUL1
	v = (v ^ _ushr(v, 27)) * MIX_MUL2
	return v ^ _ushr(v, 31)


## hash_at(col,row,layer,salt) = mix64((col << 32) ^ row ^ (layer << 56) ^ salt)。
## 左移用乘 2^32 / 2^56 实现（GDScript 左移同样只接受非负操作数，且乘法溢出行为一致）。
static func _hash_at(col: int, row: int, layer: int, salt: int) -> int:
	var key := (
		((col & 0xFFFF) * 4294967296)
		^ (row & 0xFFFF)
		^ ((layer & 0xFF) * 72057594037927936)
		^ (salt & 0xFFFFFFFF)
	)
	return _mix64(key)


## signed_noise = (hash % 2001) - 1000，范围 -1000..1000。
static func _signed_noise(col: int, row: int, layer: int, salt: int) -> int:
	return posmod(_hash_at(col, row, layer, salt), 2001) - 1000


## size 的 2 的幂因子中离 target 最近的一个（GRAIN 的条纹周期必须整除 size 才能平铺）。
static func _divisor_near(size: int, target: float) -> int:
	var best := 1
	var best_error := 1.0e20
	var d := 1
	while d <= size:
		if size % d == 0:
			var error := absf(float(d) - target)
			if error < best_error:
				best_error = error
				best = d
		d *= 2
	return best
