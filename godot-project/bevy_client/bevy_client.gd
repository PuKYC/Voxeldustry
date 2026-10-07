extends Node
## Bevy 表现层客户端：组合根 + 唯一 _process 驱动。
## 只走 PACKED_STREAMS；所有载荷经 schema 驱动的 PayloadCodec 解码，客户端不特判任何组件。
##
## 唯一认识 BevyAppManager 取帧 / 取事件 / 采样 alpha 的地方是 BevyBridge；
## 本脚本只做装配：输入采集、静态表、codec / view_layer 构造、信号转发、可选的事件自动表现。

## 启动模式（见下方 mode 导出）：决定 autostart 调哪个扩展入口。
enum Mode {
	DEMO,        ## 实体演示 / 生产：start_bevy_with_config
	VOXEL_DEMO,  ## 路径 B 体素演示：start_bevy_voxel_demo
	VOXEL_PERF,  ## 路径 B 体素性能场景：start_bevy_voxel_perf
}

@export var manager_path: NodePath = ^"/root/BevyAppManager"
## 实体 / 音效 / VFX 的挂载父节点；留空 = 本节点。
@export var entity_parent: NodePath = ^""
@export var scenes: Dictionary[int, PackedScene] = {}
## 表现消费路径。默认 PACKED_STREAMS（最快）；其余仅用于对照调试。
@export_enum("Dictionary", "Packed Dictionary", "Packed Streams") var channel: int = BevyEnums.Channel.PACKED_STREAMS

@export_group("Mode")
## 启动模式：demo=实体演示；voxel_demo=路径 B 体素演示；voxel_perf=体素性能场景。
## 新入口带 has_method 前向兼容：扩展里没有该方法时回退到实体演示。
@export_enum("demo", "voxel_demo", "voxel_perf") var mode: int = Mode.DEMO
## voxel_perf：水平方向 mesh 块半径（块），传给 start_bevy_voxel_perf。
@export_range(0, 16, 1) var voxel_radius_blocks: int = 2
## voxel_perf：最高 LOD（0..=3），传给 start_bevy_voxel_perf。
@export_range(0, 3, 1) var voxel_max_lod: int = 0

@export_group("Runtime")
@export var autostart: bool = true
@export var fixed_hz: float = 60.0
@export var runner_hz: float = 60.0
@export var enable_demo: bool = true
@export var source_id: int = 1

@export_group("Input")
## 自动注册 InputMap、采集 held/pressed/released 并每帧提交给 Bevy。
@export var enable_input: bool = true

@export_group("Presentation")
## id 或语义名 -> 资源；配了就自动播放 / 实例化（EventPresenter）。
@export var sound_library: Dictionary = {}
@export var anim_library: Dictionary = {}
@export var vfx_library: Dictionary = {}
## mod 语义覆盖：domain -> { id: name }。
@export var semantic_overrides: Dictionary = {}

signal entity_added(entity: EntityData)
signal entity_state_changed(entity: EntityData)
signal entity_removed(entity: EntityData, reason: int)
signal bevy_error(message: String)
signal bevy_stopped(exit_code: int)

var tables: StaticTables
var events := EventRouter.new()
var presenter := EventPresenter.new()
var input := BevyInput.new()
var dispatcher: PresentationDispatcher
## [data] wire -> 组件值。
var codec: PayloadCodec
## [view] id -> 节点 + 物化 / 呈现。
var view_layer: ViewLayer

var _bridge: BevyBridge
# _mgr 不能标注成 Node —— 它承载的是扩展方法（take_packed_streams / submit_input_frame 等），
# 标注成 Node 会被 GDScript 静态分析判定为「方法不存在」。
var _mgr = null
var _registry := EntityRegistry.new()
var _interp: EntityInterpolator
var _container: Node = null
var _tick: int = 0


func _ready() -> void:
	_mgr = get_node_or_null(manager_path)
	if _mgr == null:
		push_error("[BevyClient] 找不到 BevyAppManager：%s" % manager_path)
		set_process(false)
		set_process_input(false)
		return

	_container = get_node_or_null(entity_parent)
	if _container == null:
		_container = self

	_bridge = BevyBridge.new(_mgr)
	_bridge.channel = channel
	tables = StaticTables.new(_mgr)
	if not semantic_overrides.is_empty():
		tables.apply_overrides(semantic_overrides)
	_apply_voxel_table()

	var factory := EntityFactory.new(_container)
	for id in scenes:
		factory.register_prototype(id, scenes[id])

	codec = PayloadCodec.new()
	view_layer = ViewLayer.new(factory, codec)
	dispatcher = PresentationDispatcher.new(_registry, factory, codec, view_layer)
	dispatcher.entity_attached.connect(_on_entity_attached)
	dispatcher.entity_released.connect(_on_entity_released)
	dispatcher.entity_state_changed.connect(_on_entity_state_changed)
	_interp = EntityInterpolator.new(_registry, dispatcher)

	presenter.sound_library = sound_library
	presenter.anim_library = anim_library
	presenter.vfx_library = vfx_library
	presenter.bind(events, _registry, _container, tables, view_layer)

	if enable_input:
		input.setup(_mgr, source_id)

	if _mgr.has_signal("bev_error"):
		_mgr.bev_error.connect(_on_bevy_error)
	if _mgr.has_signal("bev_stopped"):
		_mgr.bev_stopped.connect(_on_bevy_stopped)
	if autostart:
		_start_backend()


## autostart 分派：按 mode 选择扩展入口。
##
## 路径 B 的两个入口用 `has_method` 保护：扩展还没编译进这些方法时回退到实体
## 演示，而不是让场景直接起不来（前向兼容）。
func _start_backend() -> void:
	match mode:
		Mode.VOXEL_DEMO:
			if _mgr.has_method("start_bevy_voxel_demo"):
				_mgr.start_bevy_voxel_demo(fixed_hz, runner_hz)
			else:
				push_warning("[BevyClient] 扩展缺少 start_bevy_voxel_demo()，回退到实体演示")
				_start_entity_backend()
		Mode.VOXEL_PERF:
			if _mgr.has_method("start_bevy_voxel_perf"):
				_mgr.start_bevy_voxel_perf(fixed_hz, runner_hz, voxel_radius_blocks, voxel_max_lod)
			else:
				push_warning("[BevyClient] 扩展缺少 start_bevy_voxel_perf()，回退到实体演示")
				_start_entity_backend()
		_:
			_start_entity_backend()


## 既有入口：实体演示 / 生产装配（start_bevy_with_config）。
func _start_entity_backend() -> void:
	if _mgr.has_method("start_bevy_with_config"):
		_mgr.start_bevy_with_config(fixed_hz, runner_hz, enable_demo)


## 路径 B：拉一次体素调色板 / 体素边长，写入 VoxelMeshNode 的共享静态量。
## 查表在 game-core（get_voxel_table），这里只做字节 -> ImageTexture 的搬运。
func _apply_voxel_table() -> void:
	if not _mgr.has_method("get_voxel_table"):
		return
	var table: Variant = _mgr.get_voxel_table()
	if not (table is Dictionary) or (table as Dictionary).is_empty():
		return
	VoxelMeshNode.voxel_size = float(table.get("voxel_size", VoxelMeshNode.voxel_size))
	var bytes: PackedByteArray = table.get("palette", PackedByteArray())
	if bytes.size() != 256 * 4:
		push_warning("[BevyClient] 体素调色板字节数异常：%d" % bytes.size())
		return
	var image := Image.create_from_data(256, 1, false, Image.FORMAT_RGBA8, bytes)
	VoxelMeshNode.palette_texture = ImageTexture.create_from_image(image)


## 业务扩展：按 key 订阅 EXT 字段（转发给 PayloadCodec）。
func register_extension(key: int, handler: Callable) -> void:
	codec.register_ext_handler(key, handler)


func _input(event: InputEvent) -> void:
	if enable_input and event is InputEventMouseMotion:
		input.on_mouse_motion((event as InputEventMouseMotion).relative)


func _process(_dt: float) -> void:
	if enable_input:
		input.collect_and_submit()

	var frame := _bridge.take_frame()
	if not frame.is_empty() and int(frame["command_count"]) > 0:
		_tick = int(frame["tick"])
		dispatcher.apply(frame)
	
	events.route(_bridge.drain_events())
	# 空闲/全静止时不进 interp，也不做 render_alpha 的 FFI。
	if _registry.active().size() > 0:
		_interp.tick(_bridge.render_alpha(), _tick)


func get_entity(id: int) -> EntityData:
	return _registry.find(id)


func entity_count() -> int:
	return _registry.all().size()


func set_semantic_override(domain: String, id: int, name: String) -> bool:
	return tables.register_semantic(domain, id, name)


# ───────────────────────── 生命周期转发 ─────────────────────────

func _on_entity_attached(e: EntityData) -> void:
	entity_added.emit(e)


func _on_entity_state_changed(e: EntityData) -> void:
	entity_state_changed.emit(e)


func _on_entity_released(e: EntityData, reason: int) -> void:
	entity_removed.emit(e, reason)


func _on_bevy_error(message: String) -> void:
	bevy_error.emit(message)


func _on_bevy_stopped(exit_code: int) -> void:
	bevy_stopped.emit(exit_code)
