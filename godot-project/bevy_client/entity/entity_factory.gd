class_name EntityFactory
extends RefCounted
## prototype_id -> PackedScene，带对象池。
## register_prototype(..., poolable := true) 决定该原型是否回池；不可回池的
## release 直接 queue_free。未知原型 / 根节点不是 BevyEntityNode 时回退到占位体。

var _scenes: Dictionary[int, PackedScene] = {}
## prototype_id -> 是否可回池。
var _poolable: Dictionary[int, bool] = {}
var _pool: Dictionary[int, Array] = {}
## VoxelMeshNode 单独一个池（它们没有 prototype_id）。
var _voxel_pool: Array = []
var _root: Node

func _init(root: Node) -> void:
	_root = root

func register_prototype(id: int, scene: PackedScene, poolable: bool = true) -> void:
	_scenes[id] = scene
	_poolable[id] = poolable

func acquire(proto_id: int, build: int = ViewRules.Build.PROTOTYPE_SCENE) -> BevyEntityNode:
	if build == ViewRules.Build.VOXEL_RAW:
		return _acquire_voxel()
	var bucket: Array = _pool.get(proto_id, [])
	var e: BevyEntityNode
	if bucket.is_empty():
		e = _instantiate(proto_id)
		_root.add_child(e)
	else:
		e = bucket.pop_back()
	e.visible = true
	return e


func _acquire_voxel() -> BevyEntityNode:
	var e: VoxelMeshNode
	if _voxel_pool.is_empty():
		e = VoxelMeshNode.new()
		_root.add_child(e)
	else:
		e = _voxel_pool.pop_back()
	e.visible = true
	return e

func release(e: BevyEntityNode) -> void:
	if e == null:
		return
	if e is VoxelMeshNode:
		e.reset()
		_voxel_pool.append(e)
		return
	if not _poolable.get(e.prototype_id, true):
		e.reset()
		e.queue_free()
		return
	e.reset()
	if not _pool.has(e.prototype_id):
		_pool[e.prototype_id] = []
	_pool[e.prototype_id].append(e)

func _instantiate(proto_id: int) -> BevyEntityNode:
	var e: BevyEntityNode = null
	var scene: PackedScene = _scenes.get(proto_id)
	if scene == null:
		push_warning("[EntityFactory] 未知 prototype_id=%d，使用占位体" % proto_id)
	else:
		e = scene.instantiate() as BevyEntityNode
		if e == null:
			push_warning("[EntityFactory] prototype_id=%d 根节点不是 BevyEntityNode，使用占位体" % proto_id)
	if e == null:
		e = _make_placeholder()
	e.prototype_id = proto_id
	return e

func _make_placeholder() -> BevyEntityNode:
	var e := BevyEntityNode.new()
	var mesh := MeshInstance3D.new()
	mesh.mesh = BoxMesh.new()
	e.add_child(mesh)
	return e
