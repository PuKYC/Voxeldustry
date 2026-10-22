class_name UiRoot
extends CanvasLayer
## UI 根：托盘 + 槽位 + 安全区 + 缩放 + 输入方案 + 菜单/暂停 + F3 调试层。
##
## 布局树（全部代码构建，场景文件只挂脚本）：
##   UiTree(Control, IGNORE)
##   └ SafeArea(MarginContainer)     ← 安全区 / 边距只在这里加
##      └ VBox
##         ├ TopTray(HBox)    [begin | center | end]
##         ├ MidTray(HBox)    [左竖列 | 中央 CenterContainer | 右竖列]
##         └ BottomTray(HBox) [begin | center | end]
##
## 元素不写坐标：HudWidget 只声明 tray/align/priority/schemes；
## 容器负责重排，断点只控制显隐与边距。

signal scheme_changed(scheme: int)
signal compact_changed(level: int)
signal menu_changed(open: bool)

const BASE_WIDTH := 1920.0
const BASE_HEIGHT := 1080.0
## 逻辑宽度断点（设计单位）。
const COMPACT_WIDTH := 1000.0
const TIGHT_WIDTH := 760.0
const EDGE_PAD := 24.0
const TOUCH_PAD := 16.0
## 移动端目标逻辑宽度：手机 / 平板。
const PHONE_TARGET_WIDTH := 900.0
const TABLET_TARGET_WIDTH := 1280.0
const TABLET_MIN_INCHES := 7.0

const BUILTIN_WIDGET_PATHS: Array[String] = [
	"res://ui/widgets/crosshair.tscn",
	"res://ui/widgets/health_bar.tscn",
	"res://ui/widgets/interact_prompt.tscn",
]
const TOUCH_CONTROLS_SCENE := "res://ui/touch/touch_controls.tscn"
const PAUSE_MENU_SCENE := "res://ui/screens/pause_menu.tscn"
const DEBUG_OVERLAY_SCRIPT := preload("res://ui/debug/hud_debug_overlay.gd")

@export var manager_path: NodePath = ^"../BevyAppManager"
@export var auto_detect_scheme: bool = true
@export var enable_touch_controls: bool = true
@export var enable_pause_menu: bool = true
## 仅测试用：在桌面 headless 下强制走移动端缩放公式（验证 900/1280 逻辑宽）。
@export var debug_force_mobile: bool = false
## 混合设备防抖：输入事件驱动的方案切换最小间隔（秒）。0 = 关闭冷却。
## 手柄 + 键鼠同时接入时，避免两类事件互相把方案抢来抢去。
@export var scheme_switch_cooldown: float = 0.5
## 手柄摇杆漂移阈值：轴值绝对值低于它不算「用了手柄」。
@export var joypad_motion_threshold: float = 0.35
## 鼠标微动阈值（逻辑像素）：低于它的 MouseMotion 不算「用了键鼠」。
@export var mouse_motion_threshold: float = 2.0

## 当前 BevyClient（可能为 null：纯 UI 预览 / 后端未启动）。
var client: Node = null
var registry := HudRegistry.new()
var pause_menu: Control = null
var touch_controls: Control = null
var debug_overlay: Control = null

var _root: Control = null
var _safe: MarginContainer = null
var _vbox: VBoxContainer = null
var _slots: Dictionary[String, Control] = {}
var _manager: Node = null
var _user_scale := 1.0
var _base_factor := 1.0
var _applying_scale := false
var _metrics_pending := false
var _debug_visible := false
var _debug_t := 0.0
var _initialized := false
var _press_echo_guard := false
## 上一次真正发生方案切换的时刻（毫秒，Time.get_ticks_msec）。
var _last_scheme_switch_ms: int = 0


func _ready() -> void:
	layer = 10
	process_mode = Node.PROCESS_MODE_ALWAYS
	_initialized = false

	_resolve_client()
	_build_layout()
	_detect_initial_scheme()
	_apply_scale()
	_update_safe_area()
	_instantiate_optional()
	_register_builtin_widgets()
	_connect_viewport()
	_set_camera_blocked(UiState.is_menu_open() or UiState.has_scheme(UiState.Scheme.TOUCH))
	_initialized = true
	_schedule_metrics()


func _process(delta: float) -> void:
	# 这里只负责按 0.25s 节流 F3 调试层刷新；
	# 尺寸变化后「延迟一帧再读视口」的逻辑在 _schedule_metrics() / _refresh_metrics()。
	if not _initialized:
		return
	if debug_overlay == null or not _debug_visible:
		return
	_debug_t += delta
	if _debug_t < 0.25:
		return
	_debug_t = 0.0
	debug_overlay.refresh()


# ───────────────────────── 布局 ─────────────────────────

func _build_layout() -> void:
	_root = Control.new()
	_root.name = "UiTree"
	_root.set_anchors_and_offsets_preset(Control.PRESET_FULL_RECT)
	_root.mouse_filter = Control.MOUSE_FILTER_IGNORE
	_root.theme = _build_theme()
	add_child(_root)

	_safe = MarginContainer.new()
	_safe.name = "SafeArea"
	_safe.set_anchors_and_offsets_preset(Control.PRESET_FULL_RECT)
	_safe.mouse_filter = Control.MOUSE_FILTER_IGNORE
	_root.add_child(_safe)

	_vbox = VBoxContainer.new()
	_vbox.name = "Trays"
	_vbox.mouse_filter = Control.MOUSE_FILTER_IGNORE
	_vbox.add_theme_constant_override("separation", 12)
	_safe.add_child(_vbox)

	# 上：三个等宽槽，分别左对齐 / 居中 / 右对齐。
	var top := HBoxContainer.new()
	top.name = "TopTray"
	top.mouse_filter = Control.MOUSE_FILTER_IGNORE
	top.add_theme_constant_override("separation", 12)
	_vbox.add_child(top)
	_add_top_bottom_slots(top, "top")

	# 中：左竖列 + 中央 + 右竖列；中央吃掉剩余空间。
	var mid := HBoxContainer.new()
	mid.name = "MidTray"
	mid.size_flags_vertical = Control.SIZE_EXPAND_FILL
	mid.mouse_filter = Control.MOUSE_FILTER_IGNORE
	mid.add_theme_constant_override("separation", 12)
	_vbox.add_child(mid)

	var left := _make_box_slot("MidBegin", true, BoxContainer.ALIGNMENT_CENTER)
	left.size_flags_vertical = Control.SIZE_EXPAND_FILL
	mid.add_child(left)
	_slots["mid_begin"] = left

	var center := CenterContainer.new()
	center.name = "MidCenter"
	center.size_flags_horizontal = Control.SIZE_EXPAND_FILL
	center.size_flags_vertical = Control.SIZE_EXPAND_FILL
	center.mouse_filter = Control.MOUSE_FILTER_IGNORE
	mid.add_child(center)
	_slots["mid_center"] = center

	var right := _make_box_slot("MidEnd", true, BoxContainer.ALIGNMENT_CENTER)
	right.size_flags_vertical = Control.SIZE_EXPAND_FILL
	mid.add_child(right)
	_slots["mid_end"] = right

	# 下：与上对称。
	var bottom := HBoxContainer.new()
	bottom.name = "BottomTray"
	bottom.mouse_filter = Control.MOUSE_FILTER_IGNORE
	bottom.add_theme_constant_override("separation", 12)
	_vbox.add_child(bottom)
	_add_top_bottom_slots(bottom, "bottom")

	registry.setup(self, _slots)


func _add_top_bottom_slots(row: HBoxContainer, prefix: String) -> void:
	var begin := _make_box_slot(prefix + "_begin", false, BoxContainer.ALIGNMENT_BEGIN)
	var center := _make_box_slot(prefix + "_center", false, BoxContainer.ALIGNMENT_CENTER)
	var end := _make_box_slot(prefix + "_end", false, BoxContainer.ALIGNMENT_END)
	for box in [begin, center, end]:
		box.size_flags_horizontal = Control.SIZE_EXPAND_FILL
		row.add_child(box)
	_slots[prefix + "_begin"] = begin
	_slots[prefix + "_center"] = center
	_slots[prefix + "_end"] = end


func _make_box_slot(slot_name: String, vertical: bool, alignment: int) -> BoxContainer:
	var box: BoxContainer
	if vertical:
		box = VBoxContainer.new()
	else:
		box = HBoxContainer.new()
	box.name = slot_name
	box.alignment = alignment
	box.mouse_filter = Control.MOUSE_FILTER_IGNORE
	box.add_theme_constant_override("separation", 8)
	return box


func _build_theme() -> Theme:
	var theme := Theme.new()
	theme.default_font_size = 22
	theme.set_color("font_color", "Label", Color(1.0, 1.0, 1.0))
	theme.set_color("font_outline_color", "Label", Color(0.0, 0.0, 0.0, 0.9))
	theme.set_constant("outline_size", "Label", 4)
	return theme


# ───────────────────────── 可选子场景 ─────────────────────────

func _instantiate_optional() -> void:
	if enable_touch_controls:
		touch_controls = _load_scene_instance(TOUCH_CONTROLS_SCENE) as Control
		if touch_controls != null:
			touch_controls.name = "TouchControls"
			_root.add_child(touch_controls)
			# 触屏控件位于 HUD 之下，避免盖住血条 / 提示。
			_root.move_child(touch_controls, 0)
			touch_controls.visible = UiState.has_scheme(UiState.Scheme.TOUCH)

	if enable_pause_menu:
		pause_menu = _load_scene_instance(PAUSE_MENU_SCENE) as Control
		if pause_menu != null:
			pause_menu.name = "PauseMenu"
			_root.add_child(pause_menu)
			pause_menu.visible = false
			if pause_menu.has_signal("resume_requested"):
				pause_menu.resume_requested.connect(close_pause_menu)
			if pause_menu.has_signal("ui_scale_changed"):
				pause_menu.ui_scale_changed.connect(_on_ui_scale_changed)

	debug_overlay = DEBUG_OVERLAY_SCRIPT.new()
	debug_overlay.name = "HudDebugOverlay"
	_root.add_child(debug_overlay)
	debug_overlay.setup(self)
	debug_overlay.visible = false


func _load_scene_instance(path: String) -> Node:
	if not ResourceLoader.exists(path):
		return null
	var resource := load(path)
	if not (resource is PackedScene):
		push_warning("[UiRoot] 无法加载场景：%s" % path)
		return null
	return (resource as PackedScene).instantiate()


func _register_builtin_widgets() -> void:
	for path in BUILTIN_WIDGET_PATHS:
		var instance := _load_scene_instance(path)
		if instance == null:
			continue
		if not (instance is HudWidget):
			push_warning("[UiRoot] HUD 场景根节点不是 HudWidget：%s" % path)
			instance.queue_free()
			continue
		if not registry.register(instance as HudWidget):
			instance.queue_free()


# ───────────────────────── 缩放 / 安全区 / 断点 ─────────────────────────

func _resolve_client() -> void:
	client = get_tree().get_first_node_in_group("bevy_client")
	if client != null:
		return
	var node := get_parent()
	while node != null:
		if node.has_signal("input_capabilities_changed") and node.has_method("listen_component"):
			client = node
			return
		node = node.get_parent()


func _detect_initial_scheme() -> void:
	if OS.has_feature("mobile") or DisplayServer.is_touchscreen_available():
		UiState.set_scheme(UiState.Scheme.TOUCH)
	else:
		UiState.set_scheme(UiState.Scheme.KBM)


## 设备类别只看静态特征（是否移动端 / 屏幕对角线），不看输入事件。
## debug_force_mobile 视为 PHONE：它模拟的是手机缩放公式。
func _update_platform() -> void:
	if debug_force_mobile:
		UiState.platform = UiState.Platform.PHONE
	elif not _is_mobile():
		UiState.platform = UiState.Platform.DESKTOP
	elif is_equal_approx(_mobile_target_width(), TABLET_TARGET_WIDTH):
		UiState.platform = UiState.Platform.TABLET
	else:
		UiState.platform = UiState.Platform.PHONE


func _apply_scale() -> void:
	_update_platform()
	_base_factor = _compute_base_scale()
	var factor := _base_factor * _user_scale
	factor = clampf(factor, 0.5, 8.0)
	var win := get_window()
	if win == null:
		return
	if is_equal_approx(win.content_scale_factor, factor):
		UiState.ui_scale = factor
		return
	_applying_scale = true
	win.content_scale_factor = factor
	UiState.ui_scale = factor
	_applying_scale = false


func _is_mobile() -> bool:
	return debug_force_mobile or OS.has_feature("mobile")


func _compute_base_scale() -> float:
	if not _is_mobile():
		return 1.0
	var win := Vector2(get_window().size)
	if win.x <= 0.0 or win.y <= 0.0:
		win = Vector2(BASE_WIDTH, BASE_HEIGHT)
	var stretch := minf(win.x / BASE_WIDTH, win.y / BASE_HEIGHT)
	if stretch <= 0.0:
		stretch = 1.0
	var target := _mobile_target_width()
	return clampf(win.x / (target * stretch), 1.0, 8.0)


func _mobile_target_width() -> float:
	var screen := Vector2(DisplayServer.screen_get_size())
	var dpi := float(DisplayServer.screen_get_dpi())
	if screen.x > 0.0 and screen.y > 0.0 and dpi > 0.0:
		var inches := screen.length() / dpi
		if inches >= TABLET_MIN_INCHES:
			return TABLET_TARGET_WIDTH
	return PHONE_TARGET_WIDTH


func _update_safe_area() -> void:
	var insets := Vector4.ZERO
	if _is_mobile() and _safe != null:
		var safe := DisplayServer.get_display_safe_area()
		var win_size := Vector2(DisplayServer.window_get_size())
		if win_size.x <= 0.0 or win_size.y <= 0.0:
			win_size = Vector2(get_window().size)
		var logical_width := maxf(UiState.logical_width, 1.0)
		var px_per_unit := win_size.x / logical_width
		if win_size.x > 0.0 and win_size.y > 0.0 and px_per_unit > 0.0:
			insets = Vector4(
				maxf(0.0, float(safe.position.x)),
				maxf(0.0, float(safe.position.y)),
				maxf(0.0, win_size.x - float(safe.end.x)),
				maxf(0.0, win_size.y - float(safe.end.y))
			) / px_per_unit
	UiState.safe_insets = insets
	if _safe == null:
		return
	# 边距看平台不看方案：平板接键盘时界面不该因为 scheme 抖动而跳变。
	var pad := TOUCH_PAD if UiState.platform != UiState.Platform.DESKTOP else EDGE_PAD
	_safe.add_theme_constant_override("margin_left", int(round(pad + insets.x)))
	_safe.add_theme_constant_override("margin_top", int(round(pad + insets.y)))
	_safe.add_theme_constant_override("margin_right", int(round(pad + insets.z)))
	_safe.add_theme_constant_override("margin_bottom", int(round(pad + insets.w)))
	# 触屏控件在 SafeArea 之外，安全区更新后要重新排一次（移动端 insets 延迟一帧才准）。
	if touch_controls != null and touch_controls.has_method("refresh_layout"):
		touch_controls.refresh_layout()


func _connect_viewport() -> void:
	var viewport := get_viewport()
	if viewport != null and not viewport.size_changed.is_connected(_on_viewport_size_changed):
		viewport.size_changed.connect(_on_viewport_size_changed)


func _on_viewport_size_changed() -> void:
	if _applying_scale:
		return
	_apply_scale()
	_schedule_metrics()


func _schedule_metrics() -> void:
	if _metrics_pending or not is_inside_tree():
		return
	_metrics_pending = true
	await get_tree().process_frame
	_metrics_pending = false
	_refresh_metrics()


func _refresh_metrics() -> void:
	var viewport := get_viewport()
	if viewport == null:
		return
	var size := viewport.get_visible_rect().size
	if size.x < 1.0 or size.y < 1.0:
		size = Vector2(BASE_WIDTH, BASE_HEIGHT)
	var width_changed := not is_equal_approx(size.x, UiState.logical_width)
	UiState.logical_width = size.x
	UiState.logical_height = size.y

	var level := 0
	if size.x < TIGHT_WIDTH:
		level = 2
	elif size.x < COMPACT_WIDTH:
		level = 1
	if level != UiState.compact_level:
		UiState.compact_level = level
		registry.refresh_compact(level)
		compact_changed.emit(level)
	if width_changed:
		_update_safe_area()


# ───────────────────────── 输入方案 ─────────────────────────

func _input(event: InputEvent) -> void:
	if not auto_detect_scheme:
		return
	var next := 0
	if event is InputEventScreenTouch or event is InputEventScreenDrag:
		next = UiState.Scheme.TOUCH
	elif event is InputEventJoypadButton:
		next = UiState.Scheme.GAMEPAD
	elif event is InputEventJoypadMotion:
		# 摇杆漂移不该把方案抢成 GAMEPAD：超过阈值才算「真的在用手柄」。
		if absf((event as InputEventJoypadMotion).axis_value) < joypad_motion_threshold:
			return
		next = UiState.Scheme.GAMEPAD
	elif event is InputEventKey or event is InputEventMouseButton:
		next = UiState.Scheme.KBM
	elif event is InputEventMouseMotion:
		var motion := event as InputEventMouseMotion
		if motion.device == InputEvent.DEVICE_ID_EMULATION:
			return
		# 手在触控板上微动一下不该把触屏 / 手柄方案抢走。
		if motion.relative.length() < mouse_motion_threshold:
			return
		next = UiState.Scheme.KBM
	if next != 0:
		_set_scheme(next, true)


## 写方案。from_input = true 时受切换冷却约束（混合设备防抖）；
## 程序化设置（测试 / 外部 API）不受冷却限制。
func _set_scheme(next: int, from_input: bool = false) -> void:
	if from_input and scheme_switch_cooldown > 0.0:
		var now := Time.get_ticks_msec()
		if now - _last_scheme_switch_ms < int(scheme_switch_cooldown * 1000.0):
			return
	if not UiState.set_scheme(next):
		return
	_last_scheme_switch_ms = Time.get_ticks_msec()
	_apply_scale()
	_update_safe_area()
	if touch_controls != null:
		touch_controls.visible = UiState.has_scheme(UiState.Scheme.TOUCH)
	_set_camera_blocked(UiState.is_menu_open() or UiState.has_scheme(UiState.Scheme.TOUCH))
	registry.refresh_scheme(next)
	scheme_changed.emit(next)
	_schedule_metrics()


# ───────────────────────── 菜单 / 暂停 ─────────────────────────

func _unhandled_input(event: InputEvent) -> void:
	if event.is_action_pressed("ui_cancel"):
		get_viewport().set_input_as_handled()
		if UiState.is_menu_open():
			close_pause_menu()
		else:
			open_pause_menu()
		return
	if event is InputEventKey:
		var key_event := event as InputEventKey
		if key_event.pressed and not key_event.echo and key_event.physical_keycode == KEY_F3:
			_toggle_debug_overlay()
			get_viewport().set_input_as_handled()


func _notification(what: int) -> void:
	if what == NOTIFICATION_WM_GO_BACK_REQUEST:
		if UiState.is_menu_open():
			close_pause_menu()
		else:
			open_pause_menu()


func open_pause_menu() -> void:
	if UiState.is_menu_open():
		return
	UiState.set_menu_open(true)
	if not UiState.has_scheme(UiState.Scheme.TOUCH):
		Input.mouse_mode = Input.MOUSE_MODE_VISIBLE
	if touch_controls != null:
		touch_controls.visible = false
	_set_backend_paused(true)
	_set_camera_blocked(true)
	registry.refresh_menu()
	if pause_menu != null:
		pause_menu.visible = true
		if pause_menu.has_method("open"):
			pause_menu.open()
	menu_changed.emit(true)


func close_pause_menu() -> void:
	if not UiState.is_menu_open():
		return
	UiState.set_menu_open(false)
	if pause_menu != null:
		if pause_menu.has_method("close"):
			pause_menu.close()
		pause_menu.visible = false
	_set_backend_paused(false)
	_set_camera_blocked(UiState.has_scheme(UiState.Scheme.TOUCH))
	registry.refresh_menu()
	if touch_controls != null:
		touch_controls.visible = UiState.has_scheme(UiState.Scheme.TOUCH)
	menu_changed.emit(false)


func toggle_pause_menu() -> void:
	if UiState.is_menu_open():
		close_pause_menu()
	else:
		open_pause_menu()


func _set_backend_paused(paused: bool) -> void:
	# 后端没起来（纯 UI 预览 / headless 冒烟）时不要写 backend_paused：
	# 否则调试层会显示一个根本没发出去的暂停。
	var manager := _get_manager()
	if manager == null:
		return
	UiState.backend_paused = paused
	if paused:
		if manager.has_method("pause_bevy"):
			manager.pause_bevy()
	elif manager.has_method("resume_bevy"):
		manager.resume_bevy()


func _get_manager() -> Node:
	if _manager != null and is_instance_valid(_manager):
		return _manager
	_manager = get_node_or_null(manager_path)
	if _manager == null and is_inside_tree():
		_manager = get_tree().root.find_child("BevyAppManager", true, false)
	return _manager


func _set_camera_blocked(blocked: bool) -> void:
	if not is_inside_tree():
		return
	var camera := get_tree().get_first_node_in_group("camera_rig")
	if camera != null and camera.has_method("set_ui_blocked"):
		camera.set_ui_blocked(blocked)


func set_ui_scale(value: float) -> void:
	_user_scale = clampf(value, 0.8, 1.3)
	_apply_scale()
	_schedule_metrics()


func _on_ui_scale_changed(value: float) -> void:
	set_ui_scale(value)


# ───────────────────────── 调试层 ─────────────────────────

func _toggle_debug_overlay() -> void:
	_debug_visible = not _debug_visible
	if debug_overlay != null:
		debug_overlay.visible = _debug_visible
		debug_overlay.refresh()


## 供 mod / 外部代码注册 UI（见 HudRegistry.register_external）。
func register_external(scene: PackedScene, meta: Dictionary) -> HudWidget:
	return registry.register_external(scene, meta)


func get_client() -> Node:
	return client
