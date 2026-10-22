extends SceneTree
## 体素着色器静态校验（headless 也能跑 ShaderLanguage 解析，不需要真实 GPU）。
##
## 背景：headless 的 dummy 渲染器仍会执行着色器语言前端（语法/类型/hint/内建），
## 因此 load() 一个 .gdshader 就能抓出 Parse Error / Unknown identifier 等；
## 坏 shader 会让 get_shader_uniform_list() 为空。SPIR-V 后端编译仍要真机/桌面。
##
## 断言：shader 能加载、uniform 集合与 V2 契约一致、render_mode 未被改掉。
## 失败非零退出；成功打印 VOXEL_SHADER_CHECK OK。

const SHADER_PATH := "res://bevy_client/shaders/voxel_rect.gdshader"
## V2 着色器契约（shader / GDScript / docs/项目架构/06-体素世界与材质.md「着色器与参数 LUT」必须同时一致）。
const EXPECTED_UNIFORMS := [
	"rect_data", "albedo_array", "normal_array", "orm_array", "emissive_array",
	"pbr_params", "use_material_layers", "block_origin", "voxel_size", "block_lod",
	"ao_floor", "ao_strength", "ao_power", "lod_fade", "layer_count", "has_emissive",
]


func _initialize() -> void:
	var failures := 0
	var shader: Shader = load(SHADER_PATH)
	if shader == null:
		print("FAIL  shader 加载失败：", SHADER_PATH)
		quit(1)
		return
	var code := shader.code
	failures += _expect("render_mode cull_disabled", code.contains("cull_disabled"))
	failures += _expect("render_mode world_vertex_coords", code.contains("world_vertex_coords"))
	failures += _expect("FRONT_FACING 翻回背面法线", code.contains("FRONT_FACING"))
	failures += _expect("alpha 裁剪读 albedo.a", code.contains("alpha") and code.contains("discard"))

	var actual := PackedStringArray()
	for uniform in shader.get_shader_uniform_list():
		actual.append(String(uniform.get("name")))
	actual.sort()
	var expected := PackedStringArray(EXPECTED_UNIFORMS)
	expected.sort()
	failures += _expect("uniform 集合与契约一致（%d 个）" % expected.size(), actual == expected, "实际 %s" % [actual])
	# 坏 shader 的 uniform 列表会是空，上面那条已能兜住；这里再给一条直白提示。
	failures += _expect("uniform 数量 > 0（shader 编译未失败）", actual.size() > 0, "0 个 uniform")

	if failures == 0:
		print("VOXEL_SHADER_CHECK OK uniforms=%d" % actual.size())
		quit(0)
	else:
		print("VOXEL_SHADER_CHECK FAIL failures=%d" % failures)
		quit(1)


func _expect(label: String, ok: bool, detail: String = "") -> int:
	if ok:
		print("PASS  ", label)
		return 0
	print("FAIL  ", label, " — ", detail)
	return 1
