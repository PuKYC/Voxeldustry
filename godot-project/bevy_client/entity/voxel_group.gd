class_name VoxelGroupNode
extends BevyEntityNode
## 体素分组绘制节点：一个实体承载同一 LOD 的多块（RAWVOXELGROUPS），
## 组内**每个 chunk 一个子 VoxelMeshNode**，合起来仍然只是「一个 LOD 分组
## 一个 Godot 节点，而不是一块一个实体节点」。
##
## 为什么不是把所有 chunk 的矩形并进同一个 MultiMesh：
## voxel_rect.gdshader 是 world_vertex_coords，VERTEX = block_origin + local*scale，
## 故意忽略 MultiMesh 实例变换；一个 MultiMesh 只有一个 block_origin，多块会
## 全部叠在组原点。把每块交给一个独立 VoxelMeshNode（它的 block_origin = 自己的
## global_position，由 VoxelMeshNode 内部维护），既复用现成的逐块上传 / AO /
## LOD 淡入逻辑，又保证每块落在正确的世界位置。
##
## 数据来源：RAWVOXELGROUPS 载荷给出 lod + 一组 (x, y, z, skirt_mask, 34³ halo)。
## 与 VoxelMeshNode 相同：GDScript 绝不逐矩形循环，也不设逐实例 custom data。

## 一个基础 mesh 块的边长（体素），与 game-core 的 CHUNK_SIZE 一致。
const CHUNK_SIZE := 32

## chunk key（Vector3i）-> 子 VoxelMeshNode。按 chunk key 复用，避免每次重建。
var _children: Dictionary = {}


## 呈现一组原始体素 halo：逐个 chunk 复用 / 新建子 VoxelMeshNode，放在该块的
## 绝对世界原点，再调用 VoxelMeshNode 现成的 set_raw_voxel_view —— mesher 调用、
## 纹理上传、block_origin / palette / AO / lod_fade 全部由它负责，这里不重复。
## chunks 元素 shape：{ x, y, z, skirt_mask: int, blocks: PackedByteArray }（BTreeMap 升序）。
func set_raw_voxel_group_view(lod: int, chunks: Array) -> void:
	var seen: Dictionary = {}
	var voxel_size: float = VoxelMeshNode.voxel_size
	for chunk in chunks:
		if not (chunk is Dictionary):
			continue
		var key := Vector3i(
			int(chunk.get("x", 0)),
			int(chunk.get("y", 0)),
			int(chunk.get("z", 0))
		)
		seen[key] = true
		var child: VoxelMeshNode = _children.get(key)
		if child == null:
			child = VoxelMeshNode.new()
			add_child(child)
			_children[key] = child
		child.visible = true
		# 块的绝对世界原点（米）= key * CHUNK_SIZE * voxel_size；
		# 与 game-core::terrain::block_origin_meters 同式，不乘 2^lod。
		child.global_position = Vector3(
			float(key.x * CHUNK_SIZE) * voxel_size,
			float(key.y * CHUNK_SIZE) * voxel_size,
			float(key.z * CHUNK_SIZE) * voxel_size
		)
		var blocks: PackedByteArray = chunk.get("blocks", PackedByteArray())
		# 逐块跨 LOD 裙边掩码必须透传给子节点：F2 起分组通道是 live 路径，
		# 这里硬编码 0 会静默关闭裙边。
		var skirt_mask: int = int(chunk.get("skirt_mask", 0))
		child.set_raw_voxel_view(lod, blocks, skirt_mask)
	_hide_unseen(seen)


## 本次未出现的 chunk：清空矩形并 hide，留在池里等 key 再次出现时复用；
## 真正的释放只在 reset_raw_voxel_group_view()（节点回收 / 组件移除）时发生。
func _hide_unseen(seen: Dictionary) -> void:
	for key in _children:
		if seen.has(key):
			continue
		var child: VoxelMeshNode = _children[key]
		if is_instance_valid(child):
			child.reset_raw_voxel_view()
			child.visible = false


## 清除整组呈现：释放全部子节点并清空池（组件移除 / 节点回收时调用）。
func reset_raw_voxel_group_view() -> void:
	for key in _children:
		var child: VoxelMeshNode = _children[key]
		if is_instance_valid(child):
			child.reset_raw_voxel_view()
			child.queue_free()
	_children.clear()


func _exit_tree() -> void:
	reset_raw_voxel_group_view()


func reset() -> void:
	super.reset()
	reset_raw_voxel_group_view()
