class_name VoxelBlockMaterial
extends Resource
## 一个方块的面材质绑定（Godot 侧真相源）：block id → 六个面的材质 id。
##
## 面序固定为 [+x, -x, +y, -y, +z, -z]（= plane*2+dir，和 mesher 的 RectInstance
## orientation 完全一致），与 game-engine 的 ExternalPlane 顺序相同。
##
## block_key 必须等于 game-core BLOCK_TABLE 里同 id 方块的名字（启动期对账），
## 防止两端 block id 漂移导致「草方块刷成石头」这种静默错误。

@export var block_key: String = ""
## 六面材质 id，顺序 [+x, -x, +y, -y, +z, -z]；越界 id 由 Library 报错。
@export var faces: PackedInt32Array = PackedInt32Array()


func is_complete() -> bool:
	return faces.size() == 6
