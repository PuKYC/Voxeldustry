class_name BevyEntityNode
extends Node3D
## 表现节点（**纯显示**）：不持有任何组件数据，只把状态反映到自身。
##
## 数据在 EntityData（RefCounted）里，由 PayloadCodec 写；节点由 ViewLayer 按
## ViewRules 物化 / 呈现 / 回收。节点不认识任何具体载荷：值由 ViewLayer 刷进来。

## 所属纯数据对象；解绑后为 null。
var data: EntityData = null
var stable_id: int = 0
var prototype_id: int = 0


# ───────────────────────── 生命周期虚函数 ─────────────────────────
# 原型场景（根节点为 BevyEntityNode）可重写这些钩子。

## 首次物化：数据齐备后进入表现层。
func on_spawned() -> void:
	pass


## 某个组件被写入。**权威值在 `data`（EntityData）**：钩子触发时
## `data.get_component(kind)` 已是新值；节点自身的显示（meta / visible 等）
## 由 ViewLayer 在本帧末按 schema 的 view 统一刷新。
func on_component_changed(_kind: int) -> void:
	pass


## 某个组件被移除。
func on_component_removed(_kind: int) -> void:
	pass


## 解绑：离开表现层但数据仍在（例如原型组件被移除）。
func on_unbound() -> void:
	pass


## DETACH / DESPAWN：reason 见 BevyEnums.RemovalReason。
func on_removed(_reason: int) -> void:
	pass


## 内部矩形上传 helper（VoxelMeshNode 覆写）；ViewLayer 不直接调用。
func set_mesh_view(_lod: int, _words: PackedInt64Array) -> void:
	pass


func reset_mesh_view() -> void:
	pass


## 原始体素 halo 呈现（RAWVOXELS）：默认无操作，VoxelMeshNode 覆写。
func set_raw_voxel_view(_lod: int, _blocks: PackedByteArray) -> void:
	pass


func reset_raw_voxel_view() -> void:
	pass


## 对象池复位：隐藏 + 归零位姿 + 清 meta + 断开数据，防止复用残影。
func reset() -> void:
	data = null
	visible = false
	position = Vector3.ZERO
	rotation = Vector3.ZERO
	for key in get_meta_list():
		remove_meta(key)
