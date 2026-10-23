extends SceneTree
## 期 3 设置层 headless 测试（无渲染依赖）。
##
## 覆盖：
## 1. UiState 新增设置默认值 / physical_units 上下限 / action_button_units 关系；
## 2. user://ui_settings.cfg 落盘：保存 -> 改字段 -> reset -> load 恢复；
## 3. 文件损坏 / 缺失 / 字段缺失 / 类型不符 / 越界时回默认并 clamp；
## 4. 暂停菜单设置区：按钮 pressed / TouchSlider gui_input -> 信号值与 Label 文案，
##    refresh_settings_controls() 不回环；
## 5. touch_controls 左手镜像：摇杆在右、视角区在左 55%、避让 reserve 落 bottom_end；
## 6. FLOATING 摇杆：按下处显示 / 位置跟随 / 拖动改方向 / 抬手隐藏复位并释放避让；
## 7. HealthBar 角色映射：键鼠 / 手柄 BOTTOM+BEGIN，TOUCH TOP+BEGIN；
## 8. 摇杆推到边缘自动 press sprint，回中 / reset 释放。
##
## 运行：
##   /tmp/Godot_v4.7.2-stable_linux.arm64 --headless --path godot-project \
##       --script res://test/ui_settings_test.gd
## 需要先生成 .godot/global_script_class_cache.cfg（res://test/gen_class_cache.gd）。

var _failures: PackedStringArray = PackedStringArray()
var _checks := 0

## 暂停菜单信号捕获。
var _got_action_layout: int = -1
var _got_button_scale: float = -1.0
var _got_button_opacity: float = -1.0
var _got_left_handed: bool = false
var _got_joystick_mode: int = -1
var _signal_counts := {"layout": 0, "scale": 0, "opacity": 0, "left": 0, "joystick": 0}


func _initialize() -> void:
	_run.call_deferred()


func _run() -> void:
	await process_frame
	print("[ui_settings] boot: dpi=%d screen=%s ui_scale=%.2f" % [
		DisplayServer.screen_get_dpi(), str(DisplayServer.screen_get_size()), UiState.ui_scale,
	])
	_remove_settings_file()
	_test_defaults_and_units()
	_test_persistence()
	_test_load_fallbacks()
	await _test_pause_menu_settings()
	await _test_touch_controls_mirror()
	await _test_floating_joystick()
	_test_health_bar_mapping()
	_test_sprint_edge()
	_remove_settings_file()
	UiState.reset()
	_finish()


# ───────────────────────── 1. 默认值 / 单位换算 ─────────────────────────

func _test_defaults_and_units() -> void:
	UiState.reset()
	_check(UiState.SETTINGS_VERSION == 1, "SETTINGS_VERSION = 1")
	_check(UiState.settings_path() == "user://ui_settings.cfg",
		"settings_path = user://ui_settings.cfg（实际 %s）" % UiState.settings_path())
	_check(UiState.action_layout == UiState.ActionLayout.CONTEXT, "默认操作布局 = CONTEXT")
	_check(is_equal_approx(UiState.button_scale, 1.0), "默认按钮大小 = 1.0")
	_check(is_equal_approx(UiState.button_opacity, 0.75), "默认按钮透明度 = 0.75")
	_check(UiState.left_handed == false, "默认右手模式")
	_check(UiState.joystick_mode == UiState.JoystickMode.FIXED, "默认摇杆 = FIXED")
	_check(UiState.ActionLayout.CONTEXT == 0 and UiState.ActionLayout.SPLIT == 1,
		"ActionLayout 枚举值 CONTEXT=0 / SPLIT=1")
	_check(UiState.JoystickMode.FIXED == 0 and UiState.JoystickMode.FLOATING == 1,
		"JoystickMode 枚举值 FIXED=0 / FLOATING=1")

	# physical_units：正常换算 + 上下限 clamp。
	UiState.ui_scale = 1.0
	var units_10 := UiState.physical_units(10.0)
	_check(units_10 >= UiState.PHYSICAL_UNITS_MIN - 0.001
			and units_10 <= UiState.PHYSICAL_UNITS_MAX + 0.001,
		"physical_units(10mm) 在 [48,220] 内（实际 %.1f）" % units_10)
	UiState.ui_scale = 4.0
	_check(is_equal_approx(UiState.physical_units(1.0), UiState.PHYSICAL_UNITS_MIN),
		"physical_units 下限 clamp 到 48（实际 %.1f）" % UiState.physical_units(1.0))
	UiState.ui_scale = 0.01
	_check(is_equal_approx(UiState.physical_units(1000.0), UiState.PHYSICAL_UNITS_MAX),
		"physical_units 上限 clamp 到 220（实际 %.1f）" % UiState.physical_units(1000.0))
	UiState.ui_scale = 0.0
	_check(UiState.physical_units(10.0) <= UiState.PHYSICAL_UNITS_MAX + 0.001,
		"ui_scale=0 不产生 inf（按 0.01 下限换算）")

	# action_button_units：main > sub，jump 在两者之间，平板放大 1.1。
	UiState.reset()
	UiState.ui_scale = _ui_scale_for_mid_range()
	var main := UiState.action_button_units("main")
	var sub := UiState.action_button_units("sub")
	var jump := UiState.action_button_units("jump")
	_check(main > sub, "action_button_units: main > sub（%.1f > %.1f）" % [main, sub])
	_check(jump > sub and jump < main,
		"action_button_units: sub < jump < main（%.1f < %.1f < %.1f）" % [sub, jump, main])
	for value: float in [main, sub, jump]:
		_check(value >= UiState.ACTION_BUTTON_UNITS_MIN - 0.001
				and value <= UiState.ACTION_BUTTON_UNITS_MAX + 0.001,
			"action_button_units 在 [56,180] 内（实际 %.1f）" % value)
	UiState.platform = UiState.Platform.TABLET
	var tablet_main := UiState.action_button_units("main")
	_check(tablet_main > main,
		"TABLET 按钮放大 1.1（桌面 %.1f / 平板 %.1f）" % [main, tablet_main])
	UiState.ui_scale = _ui_scale_for_mid_range()
	var scaled := UiState.action_button_units("main")
	UiState.button_scale = 1.3
	_check(UiState.action_button_units("main") > scaled, "button_scale 参与按钮尺寸计算")
	UiState.reset()
	UiState.platform = UiState.Platform.DESKTOP


## 选一个让 main 落在 ~100 设计单位的 ui_scale，避免 headless DPI 让 10mm/9mm 都撞 clamp。
func _ui_scale_for_mid_range() -> float:
	var dpi := float(DisplayServer.screen_get_dpi())
	var screen := DisplayServer.screen_get_size()
	if dpi <= 0.0 or screen.x <= 0 or screen.y <= 0:
		dpi = UiState.FALLBACK_DPI
	return clampf((10.0 / 25.4 * dpi) / 100.0, 0.05, 10.0)


# ───────────────────────── 2. 落盘 / 读回 ─────────────────────────

func _test_persistence() -> void:
	_remove_settings_file()
	UiState.reset()
	UiState.action_layout = UiState.ActionLayout.SPLIT
	UiState.button_scale = 1.15
	UiState.button_opacity = 0.5
	UiState.left_handed = true
	UiState.joystick_mode = UiState.JoystickMode.FLOATING
	UiState.save_settings()
	_check(FileAccess.file_exists(UiState.settings_path()), "save_settings 写出了 user://ui_settings.cfg")

	var config := ConfigFile.new()
	_check(config.load(UiState.settings_path()) == OK, "落盘文件是合法 ConfigFile")
	_check(int(config.get_value("ui", "version", -1)) == UiState.SETTINGS_VERSION,
		"落盘文件 version = SETTINGS_VERSION")
	_check(int(config.get_value("ui", "action_layout", -1)) == UiState.ActionLayout.SPLIT,
		"落盘 action_layout = SPLIT")

	# 改字段 -> reset -> load 恢复。
	UiState.action_layout = UiState.ActionLayout.CONTEXT
	UiState.button_scale = 0.8
	UiState.button_opacity = 1.0
	UiState.left_handed = false
	UiState.joystick_mode = UiState.JoystickMode.FIXED
	UiState.reset()
	_check(UiState.action_layout == UiState.ActionLayout.CONTEXT and is_equal_approx(UiState.button_scale, 1.0),
		"reset() 恢复默认值")
	UiState.load_settings()
	_check(UiState.action_layout == UiState.ActionLayout.SPLIT, "load 恢复 action_layout = SPLIT")
	_check(is_equal_approx(UiState.button_scale, 1.15), "load 恢复 button_scale = 1.15")
	_check(is_equal_approx(UiState.button_opacity, 0.5), "load 恢复 button_opacity = 0.5")
	_check(UiState.left_handed, "load 恢复 left_handed = true")
	_check(UiState.joystick_mode == UiState.JoystickMode.FLOATING, "load 恢复 joystick_mode = FLOATING")
	_check(FileAccess.file_exists(UiState.settings_path()), "reset() 不删除设置文件")

	UiState.reset()
	_remove_settings_file()


# ───────────────────────── 3. 损坏 / 缺失 / 类型不符 ─────────────────────────

func _test_load_fallbacks() -> void:
	# 文件缺失：回默认（即使内存里是脏值）。
	_remove_settings_file()
	UiState.reset()
	UiState.button_scale = 1.2
	UiState.left_handed = true
	UiState.action_layout = UiState.ActionLayout.SPLIT
	UiState.load_settings()
	_check(is_equal_approx(UiState.button_scale, 1.0) and not UiState.left_handed
			and UiState.action_layout == UiState.ActionLayout.CONTEXT,
		"文件缺失：load 回默认值")

	# 文件损坏：回默认。
	var file := FileAccess.open(UiState.settings_path(), FileAccess.WRITE)
	_check(file != null, "可写临时坏文件")
	if file != null:
		file.store_string("这不是一个 ConfigFile\n[[[ = =\n")
		file.close()
	UiState.reset()
	UiState.button_opacity = 0.9
	UiState.load_settings()
	_check(is_equal_approx(UiState.button_opacity, 0.75), "文件损坏：load 回默认值")

	# 字段缺失 / 类型不符：缺的保持默认，数值越界 clamp。
	var config := ConfigFile.new()
	config.set_value("ui", "version", 1)
	config.set_value("ui", "button_scale", 5.0)
	config.set_value("ui", "button_opacity", 0.1)
	config.set_value("ui", "action_layout", "split")
	config.set_value("ui", "joystick_mode", 99)
	# left_handed 故意缺失。
	_check(config.save(UiState.settings_path()) == OK, "写临时越界配置")
	UiState.reset()
	UiState.load_settings()
	_check(is_equal_approx(UiState.button_scale, UiState.BUTTON_SCALE_MAX),
		"button_scale 越界 clamp 到 1.3（实际 %.2f）" % UiState.button_scale)
	_check(is_equal_approx(UiState.button_opacity, UiState.BUTTON_OPACITY_MIN),
		"button_opacity 越界 clamp 到 0.35（实际 %.2f）" % UiState.button_opacity)
	_check(UiState.action_layout == UiState.ActionLayout.CONTEXT,
		"action_layout 类型不符保持默认")
	_check(UiState.joystick_mode == UiState.JoystickMode.FLOATING,
		"joystick_mode 越界 clamp 到 1")
	_check(not UiState.left_handed, "left_handed 字段缺失保持默认")

	# 只写一个字段：其余缺省。
	var partial := ConfigFile.new()
	partial.set_value("ui", "button_scale", 1.15)
	_check(partial.save(UiState.settings_path()) == OK, "写临时部分字段配置")
	UiState.reset()
	UiState.button_scale = 0.8
	UiState.load_settings()
	_check(is_equal_approx(UiState.button_scale, 1.15), "部分字段：存在的字段被读取")
	_check(is_equal_approx(UiState.button_opacity, 0.75) and not UiState.left_handed,
		"部分字段：缺失字段回默认")
	_remove_settings_file()


# ───────────────────────── 4. 暂停菜单设置区 ─────────────────────────

func _test_pause_menu_settings() -> void:
	UiState.reset()
	UiState.ui_scale = 1.0
	var scene: PackedScene = load("res://ui/screens/pause_menu.tscn")
	_check(scene != null, "pause_menu.tscn 可加载")
	if scene == null:
		return
	var menu: Control = scene.instantiate()
	root.add_child(menu)
	await process_frame
	await process_frame

	menu.action_layout_changed.connect(_on_action_layout_signal)
	menu.button_scale_changed.connect(_on_button_scale_signal)
	menu.button_opacity_changed.connect(_on_button_opacity_signal)
	menu.left_handed_changed.connect(_on_left_handed_signal)
	menu.joystick_mode_changed.connect(_on_joystick_mode_signal)

	# 控件存在且插在 Back 之前。
	var back: Button = menu.get_node("%BackButton")
	_check(menu.action_layout_button() != null, "设置区：操作布局按钮已创建")
	_check(menu.button_scale_slider() != null, "设置区：按钮大小滑条已创建")
	_check(menu.button_opacity_slider() != null, "设置区：按钮透明度滑条已创建")
	_check(menu.left_handed_button() != null, "设置区：左手模式按钮已创建")
	_check(menu.joystick_mode_button() != null, "设置区：摇杆模式按钮已创建")
	if menu.action_layout_button() != null and back != null:
		_check(menu.action_layout_button().get_index() < back.get_index(),
			"设置区控件插在 Back 之前（button=%d back=%d）" % [
				menu.action_layout_button().get_index(), back.get_index(),
			])

	# 初值来自 UiState，且 refresh 不回环。
	_check(menu.action_layout_button().text.contains("Context"),
		"初值：Layout 文案 Context（实际 %s）" % menu.action_layout_button().text)
	_check(menu.button_scale_value().text == "Button Size: 1.00",
		"初值：按钮大小 Label（实际 %s）" % menu.button_scale_value().text)
	_check(menu.left_handed_button().text.contains("OFF"), "初值：Left Handed OFF")
	_check(menu.joystick_mode_button().text.contains("FIXED"), "初值：Joystick FIXED")

	# 展开 Settings，等容器布局出真实尺寸。
	menu.visible = true
	menu.call("_on_settings_pressed")
	await process_frame
	await process_frame

	var scale_slider: HSlider = menu.button_scale_slider()
	var opacity_slider: HSlider = menu.button_opacity_slider()
	_check(scale_slider.size.x > 1.0, "按钮大小滑条已布局（宽 %.0f）" % scale_slider.size.x)

	# 操作布局按钮：Context -> Split -> Context。
	menu.action_layout_button().pressed.emit()
	_check(_got_action_layout == UiState.ActionLayout.SPLIT, "操作布局信号 = SPLIT")
	_check(menu.action_layout_button().text.contains("Split"), "操作布局文案变 Split")
	menu.action_layout_button().pressed.emit()
	_check(_got_action_layout == UiState.ActionLayout.CONTEXT, "再次点击回 Context")

	# 按钮大小 TouchSlider：触摸右端 -> 信号与 Label 同步。
	var scale_touch := _slider_touch(3, scale_slider, 0.9)
	scale_slider.emit_signal("gui_input", scale_touch)
	_check(_signal_counts["scale"] == 1, "按钮大小滑条发出 button_scale_changed")
	_check(_got_button_scale > 1.1, "按钮大小信号值变大（实际 %.2f）" % _got_button_scale)
	_check(is_equal_approx(_got_button_scale, scale_slider.value),
		"按钮大小信号值 = 滑条值（%.2f / %.2f）" % [_got_button_scale, scale_slider.value])
	_check(menu.button_scale_value().text == "Button Size: %.2f" % scale_slider.value,
		"按钮大小 Label 跟随（实际 %s）" % menu.button_scale_value().text)
	var scale_release := _slider_touch(3, scale_slider, 0.9, false)
	scale_slider.emit_signal("gui_input", scale_release)

	# 按钮透明度 TouchSlider：触摸左端 -> 变小。
	var opacity_touch := _slider_touch(4, opacity_slider, 0.05)
	opacity_slider.emit_signal("gui_input", opacity_touch)
	_check(_got_button_opacity < 0.6, "按钮透明度信号值变小（实际 %.2f）" % _got_button_opacity)
	_check(menu.button_opacity_value().text == "Button Opacity: %.2f" % opacity_slider.value,
		"按钮透明度 Label 跟随（实际 %s）" % menu.button_opacity_value().text)
	opacity_slider.emit_signal("gui_input", _slider_touch(4, opacity_slider, 0.05, false))
	_check(_signal_counts["opacity"] == 1, "按钮透明度只发一次 button_opacity_changed")

	# 左手 / 摇杆模式按钮切换。
	UiState.left_handed = false
	UiState.joystick_mode = UiState.JoystickMode.FIXED
	menu.refresh_settings_controls()
	menu.left_handed_button().pressed.emit()
	_check(_got_left_handed, "左手模式按钮信号 = true")
	_check(menu.left_handed_button().text.contains("ON"), "左手模式文案变 ON")
	menu.left_handed_button().pressed.emit()
	_check(not _got_left_handed, "再次点击关掉左手模式")
	menu.joystick_mode_button().pressed.emit()
	_check(_got_joystick_mode == UiState.JoystickMode.FLOATING, "摇杆模式信号 = FLOATING")
	_check(menu.joystick_mode_button().text.contains("FLOATING"), "摇杆模式文案变 FLOATING")

	# refresh_settings_controls：从 UiState 同步，不触发信号回环。
	var counts_before: Dictionary = _signal_counts.duplicate()
	UiState.action_layout = UiState.ActionLayout.SPLIT
	UiState.button_scale = 1.1
	UiState.button_opacity = 0.9
	UiState.left_handed = true
	UiState.joystick_mode = UiState.JoystickMode.FLOATING
	menu.refresh_settings_controls()
	_check(_signal_counts["layout"] == counts_before["layout"]
			and _signal_counts["scale"] == counts_before["scale"]
			and _signal_counts["left"] == counts_before["left"],
		"refresh_settings_controls 不触发信号回环")
	_check(menu.action_layout_button().text.contains("Split"), "refresh 同步 Layout 文案")
	_check(is_equal_approx(scale_slider.value, 1.1), "refresh 同步按钮大小滑条")
	_check(is_equal_approx(opacity_slider.value, 0.9), "refresh 同步按钮透明度滑条")
	_check(menu.left_handed_button().text.contains("ON"), "refresh 同步左手模式文案")

	menu.call("open")
	_check(menu.action_layout_button().text.contains("Split"), "open() 时同步设置初值")
	menu.call("close")
	menu.queue_free()
	await process_frame
	UiState.reset()


func _slider_touch(index: int, slider: HSlider, ratio: float, pressed: bool = true) -> InputEventScreenTouch:
	var event := InputEventScreenTouch.new()
	event.index = index
	event.pressed = pressed
	event.position = Vector2(slider.size.x * ratio, slider.size.y * 0.5)
	return event


func _on_action_layout_signal(value: int) -> void:
	_got_action_layout = value
	_signal_counts["layout"] += 1


func _on_button_scale_signal(value: float) -> void:
	_got_button_scale = value
	_signal_counts["scale"] += 1


func _on_button_opacity_signal(value: float) -> void:
	_got_button_opacity = value
	_signal_counts["opacity"] += 1


func _on_left_handed_signal(value: bool) -> void:
	_got_left_handed = value
	_signal_counts["left"] += 1


func _on_joystick_mode_signal(value: int) -> void:
	_got_joystick_mode = value
	_signal_counts["joystick"] += 1


# ───────────────────────── 5. 触屏左手镜像 ─────────────────────────

func _test_touch_controls_mirror() -> void:
	UiState.reset()
	UiState.ui_scale = 1.0
	UiState.set_scheme(UiState.Scheme.TOUCH)
	UiState.safe_insets = Vector4.ZERO
	UiState.left_handed = false
	UiState.joystick_mode = UiState.JoystickMode.FIXED

	var stub := _StubUiRoot.new()
	stub.name = "StubUiRoot"
	root.add_child(stub)
	var scene: PackedScene = load("res://ui/touch/touch_controls.tscn")
	_check(scene != null, "touch_controls.tscn 可加载")
	if scene == null:
		stub.queue_free()
		return
	var touch: Control = scene.instantiate()
	stub.add_child(touch)
	await process_frame
	await process_frame

	var view: Vector2 = touch.size
	_check(view.x > 1.0 and view.y > 1.0, "触屏层有布局尺寸（%.0fx%.0f）" % [view.x, view.y])
	var joystick: Control = touch.joystick()
	var look: Control = touch.look_area()
	_check(joystick != null and look != null, "摇杆 / 视角区已创建")
	if joystick == null or look == null:
		touch.queue_free()
		stub.queue_free()
		return
	_check(joystick.radius >= touch.JOYSTICK_MIN_RADIUS,
		"摇杆半径不小于 96（实际 %.1f）" % joystick.radius)

	# 右手：摇杆在左、视角区在右 55%。
	_check(joystick.position.x < view.x * 0.5,
		"右手：摇杆在左半屏（x=%.0f）" % joystick.position.x)
	_check(is_equal_approx(look.position.x, view.x * (1.0 - touch.LOOK_AREA_WIDTH_RATIO)),
		"右手：视角区起点 = 45%% 宽（x=%.0f）" % look.position.x)
	_check(is_equal_approx(look.size.x, view.x * touch.LOOK_AREA_WIDTH_RATIO),
		"右手：视角区宽 = 55%%（%.0f）" % look.size.x)
	_check(joystick.visible, "FIXED：摇杆常驻可见")

	var joystick_region: Dictionary = stub.regions.get(&"joystick", {})
	_check(not joystick_region.is_empty(), "右手：向 UiRoot 登记 joystick 区域")
	var reserves: Dictionary = joystick_region.get("reserves", {})
	_check(reserves.has("bottom_begin") and not reserves.has("bottom_end"),
		"右手：避让 reserve 落在 bottom_begin")
	if reserves.has("bottom_begin"):
		var left_reserve: Vector4 = reserves["bottom_begin"]
		_check(is_equal_approx(left_reserve.x, joystick.size.x + touch.JOYSTICK_MARGIN),
			"右手：reserve 左 inset = 摇杆宽 + margin（%.1f）" % left_reserve.x)
	var look_region: Dictionary = stub.regions.get(&"look_area", {})
	_check(not look_region.is_empty(), "视角区也登记（只登记不避让）")
	_check((look_region.get("reserves", {}) as Dictionary).is_empty(), "视角区 reserves 为空")

	# 左手：摇杆镜像到右、视角区到左 55%、避让落 bottom_end。
	UiState.left_handed = true
	touch.refresh_settings()
	await process_frame
	_check(joystick.position.x > view.x * 0.5,
		"左手：摇杆镜像到右半屏（x=%.0f）" % joystick.position.x)
	_check(is_equal_approx(joystick.position.x + joystick.size.x, view.x - touch.JOYSTICK_MARGIN),
		"左手：摇杆贴右边缘（右 %.0f）" % (joystick.position.x + joystick.size.x))
	_check(is_equal_approx(look.position.x, 0.0), "左手：视角区移到左侧（x=%.0f）" % look.position.x)
	_check(is_equal_approx(look.size.x, view.x * touch.LOOK_AREA_WIDTH_RATIO),
		"左手：视角区仍占 55%% 宽（%.0f）" % look.size.x)
	joystick_region = stub.regions.get(&"joystick", {})
	reserves = joystick_region.get("reserves", {})
	_check(reserves.has("bottom_end") and not reserves.has("bottom_begin"),
		"左手：避让 reserve 落在 bottom_end")
	if reserves.has("bottom_end"):
		var right_reserve: Vector4 = reserves["bottom_end"]
		_check(is_equal_approx(right_reserve.z, joystick.size.x + touch.JOYSTICK_MARGIN),
			"左手：reserve 右 inset = 摇杆宽 + margin（%.1f）" % right_reserve.z)

	touch.queue_free()
	stub.queue_free()
	await process_frame
	UiState.reset()


# ───────────────────────── 6. 浮动摇杆 ─────────────────────────

func _test_floating_joystick() -> void:
	UiState.reset()
	UiState.ui_scale = 1.0
	UiState.set_scheme(UiState.Scheme.TOUCH)
	UiState.safe_insets = Vector4.ZERO
	UiState.left_handed = false
	UiState.joystick_mode = UiState.JoystickMode.FLOATING

	var stub := _StubUiRoot.new()
	stub.name = "StubUiRootFloat"
	root.add_child(stub)
	var scene: PackedScene = load("res://ui/touch/touch_controls.tscn")
	var touch: Control = scene.instantiate()
	stub.add_child(touch)
	await process_frame
	await process_frame

	var view: Vector2 = touch.size
	var joystick: Control = touch.joystick()
	var zone: Control = touch.joystick_zone()
	_check(zone != null, "FLOATING：JoystickZone 已创建")
	if joystick == null or zone == null:
		touch.queue_free()
		stub.queue_free()
		return
	_check(not joystick.visible, "FLOATING 未激活：摇杆隐藏")
	_check(zone.visible and zone.mouse_filter == Control.MOUSE_FILTER_STOP,
		"FLOATING：触摸区可见且拦截触摸")
	_check(not stub.regions.has(&"joystick"), "FLOATING 未激活：joystick 避让已释放")

	# 模拟按下左半屏：摇杆显示在按下处并接管该手指。
	var press_position := Vector2(view.x * 0.4, view.y * 0.5)
	var press := InputEventScreenTouch.new()
	press.index = 7
	press.pressed = true
	press.position = press_position
	zone.emit_signal("gui_input", press)
	_check(joystick.visible, "FLOATING 按下：摇杆可见")
	_check(touch.is_floating_active(), "FLOATING 按下：记录激活状态")
	var expected_center: Vector2 = touch.get_global_rect().position + press_position
	var actual_center: Vector2 = joystick.get_global_rect().get_center()
	_check(actual_center.distance_to(expected_center) < 1.5,
		"FLOATING 按下：摇杆中心跟随手指（实际 %s / 期望 %s）" % [str(actual_center), str(expected_center)])
	_check(stub.regions.has(&"joystick"), "FLOATING 激活：重新登记 joystick 避让")

	# 拖动：方向跟随。
	var drag := InputEventScreenDrag.new()
	drag.index = 7
	drag.position = press_position + Vector2(48.0, -40.0)
	drag.relative = Vector2(48.0, -40.0)
	zone.emit_signal("gui_input", drag)
	_check(joystick.direction().length() > 0.1,
		"FLOATING 拖动：方向非零（%s）" % str(joystick.direction()))
	_check(joystick.direction().y < 0.0, "FLOATING 拖动：向上推 direction.y < 0")

	# 抬手：隐藏、复位、释放避让。
	var release := InputEventScreenTouch.new()
	release.index = 7
	release.pressed = false
	release.position = drag.position
	zone.emit_signal("gui_input", release)
	_check(not joystick.visible, "FLOATING 抬手：摇杆隐藏")
	_check(not touch.is_floating_active(), "FLOATING 抬手：激活状态复位")
	_check(joystick.direction() == Vector2.ZERO, "FLOATING 抬手：方向归零")
	_check(not stub.regions.has(&"joystick"), "FLOATING 抬手：joystick 避让释放")
	_check(stub.releases.has(&"joystick"), "FLOATING 抬手：调用 release_touch_region(joystick)")

	touch.queue_free()
	stub.queue_free()
	await process_frame
	UiState.reset()


# ───────────────────────── 7. 血条角色映射 ─────────────────────────

func _test_health_bar_mapping() -> void:
	var scene: PackedScene = load("res://ui/widgets/health_bar.tscn")
	_check(scene != null, "health_bar.tscn 可加载")
	if scene == null:
		return
	var bar = scene.instantiate()
	_check(bar is HudWidget, "health_bar 根节点是 HudWidget")
	if not (bar is HudWidget):
		bar.free()
		return
	_check(bar.effective_tray(UiState.Scheme.KBM) == HudWidget.Tray.BOTTOM,
		"血条键鼠落 BOTTOM（左下）")
	_check(bar.effective_tray(UiState.Scheme.GAMEPAD) == HudWidget.Tray.BOTTOM,
		"血条手柄落 BOTTOM（左下）")
	_check(bar.effective_tray(UiState.Scheme.TOUCH) == HudWidget.Tray.TOP,
		"血条触屏落 TOP（左上）")
	_check(bar.effective_align(UiState.Scheme.TOUCH) == HudWidget.Align.BEGIN,
		"血条触屏 align = BEGIN")
	_check(bar.effective_priority(UiState.Scheme.TOUCH) == 20, "血条触屏 priority = 20")
	_check(bar.scheme_overrides.has(UiState.Scheme.TOUCH), "血条声明了 TOUCH scheme_overrides")
	bar.free()


# ───────────────────────── 8. 摇杆边缘 sprint ─────────────────────────

func _test_sprint_edge() -> void:
	for action: String in ["move_forward", "move_back", "move_left", "move_right", "sprint"]:
		if not InputMap.has_action(action):
			InputMap.add_action(action)
	var joystick = load("res://ui/touch/touch_joystick.gd").new()
	joystick.size = Vector2(300.0, 300.0)
	joystick._apply_direction(Vector2(1.0, 0.0))
	_check(Input.is_action_pressed("sprint"), "摇杆推到边缘自动 press sprint")
	joystick._apply_direction(Vector2(0.3, 0.0))
	_check(not Input.is_action_pressed("sprint"), "摇杆回中自动 release sprint")
	joystick._apply_direction(Vector2(0.0, -1.0))
	_check(Input.is_action_pressed("sprint"), "反向推到边缘也 press sprint")
	joystick.reset()
	_check(not Input.is_action_pressed("sprint"), "reset 释放 sprint")
	joystick.free()


# ───────────────────────── 测试替身 / 基础设施 ─────────────────────────

## 最小 UiRoot 替身：只提供 touch_controls 依赖的信号与避让登记方法。
class _StubUiRoot:
	extends Node

	signal scheme_changed(scheme: int)
	signal menu_changed(open: bool)

	var regions: Dictionary = {}
	var releases: Array[StringName] = []


	func update_touch_region(name: StringName, rect: Rect2, reserves: Dictionary = {}) -> void:
		regions[name] = {"rect": rect, "reserves": reserves.duplicate()}


	func release_touch_region(name: StringName) -> void:
		regions.erase(name)
		releases.append(name)


func _remove_settings_file() -> void:
	if FileAccess.file_exists(UiState.settings_path()):
		DirAccess.remove_absolute(ProjectSettings.globalize_path(UiState.settings_path()))


func _check(condition: bool, message: String) -> void:
	_checks += 1
	if condition:
		print("[ui_settings] PASS  " + message)
	else:
		_failures.append(message)
		print("[ui_settings] FAIL  " + message)


func _finish() -> void:
	print("[ui_settings] checks=%d failures=%d" % [_checks, _failures.size()])
	if _failures.is_empty():
		print("UI_SETTINGS PASS")
		quit(0)
	else:
		for failure: String in _failures:
			print("  - " + failure)
		print("UI_SETTINGS FAIL")
		quit(1)
