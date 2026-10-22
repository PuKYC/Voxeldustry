class_name VoxelEnv
extends RefCounted
## 体素场景统一环境光 helper。
##
## 引擎 lit 光照必须有环境光，否则背光面 / AO 暗部会接近全黑。体素场景只保留一盏
## DirectionalLight3D（各场景已有，本 helper **不加灯**），环境光统一走 WorldEnvironment：
## - 背景：BG_COLOR + 天空蓝灰（不依赖天空资源，也不生成 radiance map）；
## - 环境光：AMBIENT_SOURCE_COLOR + 偏冷天光，energy 约 0.5；
## - tonemap 保持 LINEAR（默认值），不开 SSAO / glow / SSR（移动端成本）。
##
## 幂等：场景里已有 WorldEnvironment 时直接复用，保证「只加一次」。
## 使用：VoxelEnv.ensure(self)（node_3d / voxel_demo / gameplay_test）。
## voxel_perf.tscn 是纯场景装配（脚本不动），环境节点直接写在 tscn 里，参数同下。

## 天空蓝灰（BG_COLOR）。
const SKY_COLOR := Color(0.45, 0.55, 0.68, 1.0)
## 偏冷天光（AMBIENT_SOURCE_COLOR）。
const AMBIENT_COLOR := Color(0.55, 0.68, 0.88, 1.0)
## 环境光强度（引擎 lit 保底照明；方向光才是主光）。
const AMBIENT_ENERGY := 0.5


## 确保 parent 下存在 WorldEnvironment（不存在时创建），返回该节点。
## parent 为空时返回 null（不崩溃）。
static func ensure(parent: Node) -> WorldEnvironment:
	if parent == null:
		return null
	var existing := parent.get_node_or_null(^"WorldEnvironment") as WorldEnvironment
	if existing != null:
		return existing

	var environment := Environment.new()
	environment.background_mode = Environment.BG_COLOR
	environment.background_color = SKY_COLOR
	# 纯色环境光：不采样天空、不生成 radiance map，移动端最便宜。
	environment.ambient_light_source = Environment.AMBIENT_SOURCE_COLOR
	environment.ambient_light_color = AMBIENT_COLOR
	# 官方文档：ambient_light_color / ambient_light_energy 仅在
	# ambient_light_sky_contribution < 1.0（exclusive）时生效。
	environment.ambient_light_sky_contribution = 0.0
	environment.ambient_light_energy = AMBIENT_ENERGY
	# tonemap 保持默认 LINEAR；不写 glow / ssao 属性（默认关闭，省移动端成本）。

	var node := WorldEnvironment.new()
	node.name = "WorldEnvironment"
	node.environment = environment
	parent.add_child(node)
	return node
