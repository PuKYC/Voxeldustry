shader_type spatial;

// 路径 B 体素矩形展开着色器（引擎 lit 光照 + 4 张并行 sampler2DArray）。
//
// 每块一个 MultiMeshInstance3D，quad 的 4 个顶点由 INSTANCE_ID 在 GPU 上
// 展开成一个贪心矩形：8 B/矩形只有一份（R32G32_SFLOAT 承载原始 u64 位模式，
// 1 texel = 1 个 39 bit 描述符）。GDScript 不逐矩形循环。
//
// 关键点：
// - world_vertex_coords：VERTEX 直接写世界坐标，绕过 MultiMesh 的实例变换
//   （Godot 的 MultiMesh 实例缓冲默认是零矩阵，不 identity），块原点由
//   block_origin uniform 携带。NORMAL / TANGENT / BINORMAL 也按世界空间写，
//   引擎之后再统一转到 view space（官方 spatial shader 文档：world_vertex_coords
//   下 VERTEX / NORMAL / TANGENT / BINORMAL 都按世界空间处理）。
// - INSTANCE_ID / texelFetch / usampler2D / textureSize 均在 vertex() 阶段可用。
// - 几何轴映射必须与 game-core::presentation::voxel_mesh::rect_corner 逐条一致
//   （由 Rust 单测锁死）。
// - layer 索引 = 材质 id：Godot 的 VoxelMaterialLibrary 把 block→面材质绑定表传给
//   mesher，mesher 把 RectInstance.material 换成材质 id；四张数组共享同一 layer。
render_mode cull_disabled, world_vertex_coords, depth_draw_opaque;

// 1 texel = 1 个矩形 = R32G32_SFLOAT（原始 u64 位模式，按 32 位两段读回）。
uniform usampler2D rect_data : filter_nearest;
// 3~4 张同尺寸 / 同层数 / 同 mip 的并行数组，layer = 材质 id。
// 全部是**线性数据**：bake 时已把 sRGB 作者色转成线性字节（VoxelMaterialTiles），
// 所以这些数组**不加 source_color**——sampler2DArray 的 sRGB 视图在 4.7 没有
// 文档保证，加错 hint 会让整个着色器编译失败；线性存储零 API 风险，
// Forward+ / Mobile 行为一致。
// 不写 filter/repeat hint：用项目默认的 linear mipmap + repeat。
uniform sampler2DArray albedo_array;
uniform sampler2DArray normal_array;
uniform sampler2DArray orm_array;
uniform sampler2DArray emissive_array;
// 逐材质标量参数 LUT（bake 工具写入）：width = layer_count、height = 1、RGBAH，
// texel(layer) = (uv_scale, alpha_scissor, emission_energy, 0)。
// 用 texelFetch 读、不参与过滤。以后加逐材质标量只需加一列，不动分配结构。
uniform sampler2D pbr_params : filter_nearest, repeat_disable;
// true = 材质数组已就绪（正常路径）；false = 库未加载/构建失败时的中性灰兜底。
// 由 VoxelMeshNode.apply_material_library() 写入；默认 false，避免 1x1 默认
// 纹理的 alpha_scissor=1 把整屏 discard 掉。
uniform bool use_material_layers = false;
// 块原点（米，世界坐标）；由节点在 Transform 变化时写入。
uniform vec3 block_origin = vec3(0.0);
uniform float voxel_size = 0.45;
uniform int block_lod = 0;
// AO：bit [39,47) 每角 2 bit（3 = 全亮）；ao_floor 是最暗角的环境光保底。
uniform float ao_floor = 0.4;
// AO 总强度：1.0 = 保持原观感（默认开启），0.0 = 完全关闭（ao_f == 1.0）。
uniform float ao_strength : hint_range(0.0, 1.0) = 1.0;
// AO 曲线指数：1.0 = 保持原观感（不做 pow）；>1 加深，<1 减淡。
uniform float ao_power : hint_range(0.0, 8.0) = 1.0;
// LOD 交叉淡入（机制同 godot_voxel 的 u_lod_fade）：
//   x = 过渡进度；y = 1 淡入 / 0 淡出。默认 vec2(0,0) 是严格 no-op（不 discard）。
uniform vec2 lod_fade = vec2(0.0);
// 数组层数（启动时写入；fragment 不调 textureSize，官方文档建议改用 uniform）。
uniform int layer_count = 9;
// 无自发光材质时跳过第 4 次采样（默认 false，省片元带宽）。
uniform bool has_emissive = false;

// 插值量：v_material 用 flat（同一矩形内材质相同，不做插值）。
// 注意 Godot 的插值限定符写在 varying 之后（官方文档示例：varying flat vec3）。
varying vec2 v_uv;
varying float v_ao;
varying flat float v_material;

// 屏幕空间哈希：给每个像素一个稳定随机阈值，配合 lod_fade 做互补抖动淡入淡出。
float lod_fade_hash(vec2 c) {
	return fract(sin(dot(c, vec2(12.9898, 78.233))) * 43758.5453);
}

void vertex() {
	int inst = INSTANCE_ID;
	ivec2 tex_size = textureSize(rect_data, 0);
	int width = max(tex_size.x, 1);
	uvec4 packed = texelFetch(rect_data, ivec2(inst % width, inst / width), 0);

	uint lo = packed.x;
	uint hi = packed.y;

	// 39 bit 布局（game-engine/src/voxel/pack.rs）：
	// orientation(3) slice(6) row(5) col(5) w(6) h(6) material(8)
	// bit [39,47) = 4 角 AO（每角 2 bit，3=全亮），角序 i = cx | (cy << 1)
	uint orientation = lo & 7u;
	uint slice = (lo >> 3u) & 63u;
	uint row = (lo >> 9u) & 31u;
	uint col = (lo >> 14u) & 31u;
	uint rect_w = (lo >> 19u) & 63u;
	uint rect_h = (lo >> 25u) & 63u;
	uint material = ((lo >> 31u) & 1u) | ((hi & 127u) << 1u);
	// AO 在 hi 的 bit 7..15（全局 bit [39,47)），material 只占 hi 的 bit 0..6。
	uint ao_bits = (hi >> 7u) & 255u;

	uint plane = orientation >> 1u;
	uint dir = orientation & 1u;

	float cx = UV.x;
	float cy = UV.y;
	// 角序 i = cx | (cy << 1)，与 Rust 的 rect_corner / compute_face_ao 同序。
	int iao = int(cx) | (int(cy) << 1);
	float ao = float((ao_bits >> uint(iao * 2)) & 3u) / 3.0;
	// ao_power == 1.0（默认）时跳过 pow，保证与旧实现逐位一致。
	float ao_curve = ao;
	if (ao_power != 1.0) {
		ao_curve = pow(ao, ao_power);
	}
	float ao_base = mix(ao_floor, 1.0, ao_curve);
	// ao_strength == 1.0（默认）直通 ao_base，逐位保持旧观感；
	// ao_strength == 0.0 时 mix 结果严格为 1.0（完全关闭 AO）。
	float ao_f = ao_base;
	if (ao_strength < 1.0) {
		ao_f = mix(1.0, ao_base, ao_strength);
	}
	float cols = float(col) + cx * float(rect_w);
	float rows = float(row) + cy * float(rect_h);
	float s = float(slice);

	vec3 local;
	vec3 nrm;
	if (plane == 0u) {
		local = vec3(s, rows, cols); // YZ: rows=y, cols=z
		nrm = vec3(1.0, 0.0, 0.0);
	} else if (plane == 1u) {
		local = vec3(cols, s, rows); // XZ: rows=z, cols=x
		nrm = vec3(0.0, 1.0, 0.0);
	} else {
		local = vec3(cols, rows, s); // XY: rows=y, cols=x
		nrm = vec3(0.0, 0.0, 1.0);
	}
	if (dir == 1u) {
		nrm = -nrm;
	}

	float scale = voxel_size * exp2(float(block_lod));
	VERTEX = block_origin + local * scale;

	// 世界法线（dir==1 取反）。TANGENT 恒等于纹理 u 轴（plane 0 -> +Z，其余 -> +X），
	// BINORMAL 恒等于纹理 v 轴（plane 0/2 -> +Y，plane 1 -> +Z）：两轴与面朝向无关，
	// 法线贴图的 R/G 在所有六面上都与采样 UV 的 u/v 对齐（不能写成 cross(N,T)：
	// 那样有一半朝向的 v 轴会被镜像）。world_vertex_coords 下引擎自动转 view space。
	vec3 world_normal = nrm;
	vec3 tangent = (plane == 0u) ? vec3(0.0, 0.0, 1.0) : vec3(1.0, 0.0, 0.0);
	vec3 binormal = (plane == 1u) ? vec3(0.0, 0.0, 1.0) : vec3(0.0, 1.0, 0.0);
	NORMAL = world_normal;
	TANGENT = tangent;
	BINORMAL = binormal;

	// 材质 id 夹到 layer_count-1，避免越界层；layer_count <= 0 时取 0。
	uint layer_max = 0u;
	if (layer_count > 0) {
		layer_max = uint(layer_count - 1);
	}
	int layer = int(min(material, layer_max));
	v_material = float(layer);

	// 块局部 UV：cols/rows 是展开后的「该 LOD 体素」单位坐标。LOD L 的一个格子
	// 等于 2^L 个基础体素，所以乘 2^L 才能让贴图的**世界尺寸**跨 LOD 不变
	// （否则每级 LOD 纹理放大一倍，切换时图案会明显跳）。32*2^L 仍是整数，
	// 块边相位连续；纹理默认 repeat，跨矩形不接缝（不要 fract）。
	// 逐材质 UV 缩放来自参数 LUT（顶点阶段 texelFetch；数组未就绪时取 1.0）。
	float layer_uv_scale = 1.0;
	if (use_material_layers) {
		layer_uv_scale = texelFetch(pbr_params, ivec2(layer, 0), 0).r;
	}
	v_uv = vec2(cols, rows) * exp2(float(block_lod)) * layer_uv_scale;
	v_ao = ao_f;
}

void fragment() {
	// cull_disabled 下引擎在片元前置段会对背面片元做 NORMAL = -NORMAL
	// （scene_shader_forward_clustered.cpp 的 DO_SIDE_CHECK）。我们的矩形
	// 展开与 dir 无关（dir 只翻法线，不换绕序），所以每个平面族必然有一半朝向
	// 从外侧看是背面；不覆盖的话那一半法线朝内、方向光失效只剩环境光。
	// FRONT_FACING 是引擎内建；这里把背面翻回来，TANGENT/BINORMAL 未被翻转，
	// 因此翻回后 TBN 仍与几何一致。
	if (!FRONT_FACING) {
		NORMAL = -NORMAL;
	}

	float layer = v_material;

	if (!use_material_layers) {
		// 兜底：材质库未加载或构建失败时给中性灰，保证画面可读而不是全透明。
		ALBEDO = vec3(0.65);
		ROUGHNESS = 1.0;
		METALLIC = 0.0;
		AO = v_ao;
	} else {
		// 逐材质标量：一次 texelFetch（uv_scale 已在 vertex 用过，这里取 alpha/energy）。
		// 只在数组就绪的分支里读，避免 fallback 时 sampler 未绑定还做 fetch。
		vec4 params = texelFetch(pbr_params, ivec2(int(layer), 0), 0);

		// albedo 数组存的是线性 RGBA8，A 是 alpha 裁剪通道。
		vec4 albedo = texture(albedo_array, vec3(v_uv, layer));
		ALBEDO = albedo.rgb;
		// alpha 裁剪（cutout，不做透明混合）：阈值逐材质来自 LUT，0 = 关闭。
		if (params.g > 0.0 && albedo.a < params.g) {
			discard;
		}

		// 法线数组是 RG8 线性数据（OpenGL +Y）；normal_strength 在 bake 时已烘进 RG。
		// NORMAL_MAP 是 0..1 编码的切线空间法线：引擎只取 xy（*2-1）、忽略蓝通道
		// 并自己按单位向量重建 z，所以不需要 sqrt / normalize。
		vec2 nxy = texture(normal_array, vec3(v_uv, layer)).rg * 2.0 - 1.0;
		NORMAL_MAP = vec3(nxy * 0.5 + 0.5, 0.5);

		// ORM：R = 材质 AO，G = roughness，B = metallic（线性，不能加 source_color）。
		vec4 orm = texture(orm_array, vec3(v_uv, layer));
		ROUGHNESS = orm.g;
		METALLIC = orm.b;
		AO = clamp(v_ao * orm.r, 0.0, 1.0);

		// 无自发光材质时跳过第 4 次采样（uniform 分支，不是逐像素纹理采样）。
		if (has_emissive) {
			EMISSION = texture(emissive_array, vec3(v_uv, layer)).rgb * params.b;
		}
	}

	// 旧观感是顶点 AO 乘总光照；AO 默认只乘环境光（AO_LIGHT_AFFECT=0.0），
	// 会让直射光下的凹陷几乎看不出 AO。这里恢复「AO 乘总光照」。
	AO_LIGHT_AFFECT = 1.0;

	// 渐进 discard 实现 LOD 交叉淡入。必须放在 fragment 末尾。
	// lod_fade == vec2(0,0) 时两个分支都不 discard（严格保持旧行为）。
	float h = lod_fade_hash(SCREEN_UV);
	if (lod_fade.y > 0.5) {
		if (lod_fade.x < h) {
			discard;
		}
	} else {
		if (lod_fade.x > h) {
			discard;
		}
	}
}
