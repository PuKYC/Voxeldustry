shader_type spatial;

// 路径 B 体素矩形展开着色器。
//
// 每块一个 MultiMeshInstance3D，quad 的 4 个顶点由 INSTANCE_ID 在 GPU 上
// 展开成一个贪心矩形：8 B/矩形只有一份（R32G32_UINT 纹理，1 texel = 1 个
// 39 bit 描述符）。GDScript 不逐矩形循环。
//
// 关键点：
// - world_vertex_coords：VERTEX 直接写世界坐标，绕过 MultiMesh 的实例变换
//   （Godot 的 MultiMesh 实例缓冲默认是零矩阵，不 identity），块原点由
//   block_origin uniform 携带。
// - INSTANCE_ID / texelFetch / usampler2D / textureSize 均在 vertex() 阶段可用。
// - 几何轴映射必须与 game-core::presentation::voxel_mesh::rect_corner 逐条一致
//   （由 Rust 单测锁死）。
render_mode unshaded, cull_disabled, world_vertex_coords, depth_draw_opaque;

// 1 texel = 1 个矩形 = R32G32_UINT（低 32 位 | 高 32 位，小端）。
uniform usampler2D rect_data : filter_nearest;
// 256 x 1 RGBA8：方块 id -> 线性 RGBA（game-core 的调色板原值，不换算）。
uniform sampler2D palette : filter_nearest;
// 块原点（米，世界坐标）；由节点在 Transform 变化时写入。
uniform vec3 block_origin = vec3(0.0);
uniform float voxel_size = 0.45;
uniform int block_lod = 0;
uniform vec3 sun_dir = vec3(0.35, 0.82, 0.45);
uniform float ambient = 0.45;
// AO：bit [39,47) 每角 2 bit（3 = 全亮）；ao_floor 是最暗角的环境光保底。
uniform float ao_floor = 0.4;

void vertex() {
	int inst = INSTANCE_ID;
	ivec2 tex_size = textureSize(rect_data, 0);
	int width = max(tex_size.x, 1);
	uvec2 packed = texelFetch(rect_data, ivec2(inst % width, inst / width), 0);

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
	float ao_f = mix(ao_floor, 1.0, ao);
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

	float ndl = max(dot(nrm, normalize(sun_dir)), 0.0);
	vec4 pal = texelFetch(palette, ivec2(int(material), 0), 0);
	COLOR = vec4(pal.rgb * (ambient + (1.0 - ambient) * ndl) * ao_f, 1.0);
}

void fragment() {
	ALBEDO = COLOR.rgb;
}
