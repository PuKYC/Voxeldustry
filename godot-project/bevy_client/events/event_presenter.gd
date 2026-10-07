class_name EventPresenter
extends RefCounted
## 一次性事件 -> 实际播放（可选层，从旧门面的 _auto_present 收编）。
##
## 只做「id / 语义名 -> 资源 -> 播放 / 实例化」的搬运，不含业务判断。
## 资源库为空时不命中任何分支；事件本身仍由 EventRouter 的信号暴露给业务。
##
## 库的键可以是 int(id) 或 String(语义名)，与旧门面一致。

var sound_library: Dictionary = {}
var anim_library: Dictionary = {}
var vfx_library: Dictionary = {}

var _registry: EntityRegistry
var _container: Node
var _tables: StaticTables
## 查显示节点用（EntityData 不再持有节点）。
var _view_layer: ViewLayer
var _bound := false


func bind(
	router: EventRouter,
	registry: EntityRegistry,
	container: Node,
	tables: StaticTables,
	view_layer: ViewLayer = null
) -> void:
	if _bound:
		return
	_registry = registry
	_container = container
	_tables = tables
	_view_layer = view_layer
	router.play_sound.connect(_on_play_sound)
	router.trigger_anim.connect(_on_trigger_anim)
	router.spawn_vfx.connect(_on_spawn_vfx)
	_bound = true


func _on_play_sound(sound_id: int, position: Vector3) -> void:
	var stream = _resolve("sound", sound_library, sound_id)
	if stream is AudioStream:
		_spawn_audio(stream, position)


func _on_trigger_anim(entity_id: int, anim_id: int, speed: float) -> void:
	var e := _registry.find(entity_id)
	if e == null:
		return
	var node: BevyEntityNode = _view_layer.node_of(e) if _view_layer != null else null
	if node == null:
		return
	var player := node.find_child("AnimationPlayer", true, false) as AnimationPlayer
	if player == null:
		return
	var resolved = _resolve("anim", anim_library, anim_id)
	if resolved is StringName:
		player.play(resolved, -1.0, speed)
	elif resolved is String and resolved != "":
		player.play(StringName(resolved), -1.0, speed)


func _on_spawn_vfx(vfx_id: int, position: Vector3) -> void:
	var scene = _resolve("vfx", vfx_library, vfx_id)
	if scene is PackedScene:
		var instance: Node = (scene as PackedScene).instantiate()
		_container.add_child(instance)
		if instance is Node3D:
			(instance as Node3D).global_position = position


## 先按 id，再按语义名；都找不到返回 null。
func _resolve(domain: String, library: Dictionary, id: int) -> Variant:
	if library.is_empty():
		return null
	if library.has(id):
		return library[id]
	var sem_name := ""
	if _tables != null:
		sem_name = _tables.name_of(domain, id)
	if sem_name != "" and not sem_name.begins_with("#") and library.has(sem_name):
		return library[sem_name]
	return null


func _spawn_audio(stream: AudioStream, position: Vector3) -> void:
	var player := AudioStreamPlayer3D.new()
	player.stream = stream
	_container.add_child(player)
	player.global_position = position
	player.finished.connect(player.queue_free)
	player.play()
