class_name ViewRules
extends RefCounted
## [view rules] 层：哪些组件组合意味着「该实体有一个节点」，以及怎么建。
##
## 纯规则，不碰节点：只回答 match_rule(data) -> { ... }。新增一种显示物 =
## 这里加一行规则（+ ViewLayer 对应的 build 分支）。
##
## 规则字段：
## - name    ：诊断名
## - require ：必须同时持有的 Payload kind
## - build   ：物化方式（ViewLayer 按它选工厂分支）

enum Build {
	PROTOTYPE_SCENE, ## 用 PROTOTYPE.prototype_id 从 EntityFactory 取 PackedScene
	VOXEL_RAW,       ## 原始体素 halo：gdext mesher 现算矩形流后交给 VoxelMeshNode
}

## 从上到下取第一条满足的规则；没有返回 {}。
var RULES: Array = [
	{
		"name": "voxel_raw",
		"require": [BevyEnums.Payload.TRANSFORM, BevyEnums.Payload.RAWVOXELS],
		"build": Build.VOXEL_RAW,
	},
	{
		"name": "spatial",
		"require": [BevyEnums.Payload.TRANSFORM, BevyEnums.Payload.PROTOTYPE],
		"build": Build.PROTOTYPE_SCENE,
	},
]


func match_rule(data: EntityData) -> Dictionary:
	for rule in RULES:
		var satisfied := true
		for kind in rule["require"]:
			if not data.has_component(int(kind)):
				satisfied = false
				break
		if satisfied:
			return rule
	return {}
