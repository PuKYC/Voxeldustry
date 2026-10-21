extends SceneTree
## UI headless 冒烟测试（无编辑器、无渲染依赖）。
##
## 覆盖：
## 1. UiRoot 场景可加载，托盘 9 个槽位全部创建；
## 2. HudWidget 注册 / 排序 / 方案显隐 / 紧凑显隐；
## 3. 菜单打开时 HUD 隐藏、关闭后恢复；
## 4. register_external（mod 路径）能通过 PackedScene 注册；
## 5. 逻辑尺寸 / 缩放 / 安全区状态字段有值。
##
## 运行：
##   /tmp/Godot_v4.7.2-stable_linux.arm64 --headless --path godot-project \
##       --script res://test/ui_smoke_test.gd
##
## 注意：需要 .godot/global_script_class_cache.cfg（无编辑器时由
## tools/ui_check.sh 先生成；该脚本会自动跑本测试）。

var _failures: PackedStringArray = PackedStringArray()
var _checks := 0


func _initialize() -> void:
	_run.call_deferred()


func _run() -> void:
	await process_frame
	print("[ui_smoke] boot: scale=%.2f logical=%.0fx%.0f scheme=%d" % [
		UiState.ui_scale, UiState.logical_width, UiState.logical_height, UiState.scheme,
	])

	var scene: PackedScene = load("res://ui/ui_root.tscn")
	_check(scene != null, "ui_root.tscn 可加载")
	if scene == null:
		_finish()
		return

	var ui: CanvasLayer = scene.instantiate()
	root.add_child(ui)
	await process_frame
	await process_frame

	_check(UiState.logical_width > 0.0 and UiState.logical_height > 0.0, "逻辑尺寸已计算")
	_check(UiState.ui_scale > 0.0, "content_scale_factor 已设置")

	# 移动端缩放公式（用 debug_force_mobile 在桌面 headless 下模拟）：
	# 目标逻辑宽 900（手机），逻辑单位≈dp；900 < 1000 触发紧凑等级 1。
	ui.set("debug_force_mobile", true)
	ui.call("_apply_scale")
	ui.call("_schedule_metrics")
	await process_frame
	await process_frame
	await process_frame
	_check(is_equal_approx(UiState.logical_width, 900.0), "移动端：逻辑宽度收敛到 900（实际 %.0f）" % UiState.logical_width)
	_check(UiState.compact_level == 1, "移动端：900 宽触发紧凑等级 1（实际 %d）" % UiState.compact_level)
	ui.set("debug_force_mobile", false)
	ui.call("_apply_scale")
	ui.call("_schedule_metrics")
	await process_frame
	await process_frame
	await process_frame
	_check(is_equal_approx(UiState.logical_width, 1920.0), "桌面：逻辑宽度回到 1920（实际 %.0f）" % UiState.logical_width)
	_check(UiState.compact_level == 0, "桌面：断点回到正常（实际 %d）" % UiState.compact_level)
	var slots: Dictionary = ui.get("_slots")
	_check(slots.size() == 9, "托盘槽位数量 = 9（实际 %d）" % slots.size())
	# 已落地的内置 HUD（widgets 子代理交付）应被 UiRoot 自动注册。
	var registered_names: Array[String] = []
	for widget: HudWidget in ui.registry.widgets():
		registered_names.append(String(widget.widget_id))
	print("[ui_smoke] builtin widgets: " + str(registered_names))
	_check(ui.registry.get_widget(&"crosshair") != null, "内置 HUD：crosshair 已注册")
	_check(ui.registry.get_widget(&"health_bar") != null, "内置 HUD：health_bar 已注册")
	_check(ui.registry.get_widget(&"interact_prompt") != null, "内置 HUD：interact_prompt 已注册")

	# 触屏控件 / 暂停菜单：存在即由 UiRoot 实例化。
	_check(ui.touch_controls != null, "触屏控件已实例化")
	_check(ui.pause_menu != null, "暂停菜单已实例化")
	if ui.touch_controls != null:
		# 冒烟没有 BevyClient，不会自动注册 InputMap；摇杆测试先手动注册移动动作。
		for action in ["move_forward", "move_back", "move_left", "move_right"]:
			if not InputMap.has_action(action):
				InputMap.add_action(action)
		ui.call("_set_scheme", UiState.Scheme.TOUCH)
		await process_frame
		_check(ui.touch_controls.visible, "触屏方案：触屏控件可见")
		var joystick: Control = ui.touch_controls.joystick()
		_check(joystick != null, "触屏控件：摇杆已创建")
		if joystick != null:
			joystick.call("_apply_direction", Vector2(0.0, -1.0))
			_check(Input.is_action_pressed("move_forward"), "摇杆：上推注入 move_forward")
			# BevyInput 的取法：pos_y = move_forward，所以前进时 y > 0。
			var move := Input.get_vector("move_left", "move_right", "move_back", "move_forward", 0.0)
			_check(move.y > 0.5, "摇杆：Input.get_vector 读到前进（y=%.2f）" % move.y)
			joystick.call("reset")
			_check(not Input.is_action_pressed("move_forward"), "摇杆：reset 释放动作")
			# 安全区：触屏控件不在 SafeArea 容器内，layout 必须自己避让 insets。
			UiState.safe_insets = Vector4(12.0, 0.0, 20.0, 48.0)
			ui.touch_controls.refresh_layout()
			_check(joystick.position.x >= ui.touch_controls.JOYSTICK_MARGIN + 12.0,
				"触屏布局：摇杆避开左安全区（x=%.0f）" % joystick.position.x)
			_check(joystick.position.y + joystick.size.y <= ui.touch_controls.size.y - 48.0,
				"触屏布局：摇杆避开底部安全区")
			UiState.safe_insets = Vector4.ZERO
			ui.touch_controls.refresh_layout()
		ui.call("_set_scheme", UiState.Scheme.KBM)
		await process_frame

	# 注册两个同槽元素，验证 priority 排序：TOP END 槽，priority 大的靠右（最后）。
	var first := HudWidget.new()
	first.name = "FirstWidget"
	first.widget_id = &"smoke_first"
	first.tray = HudWidget.Tray.TOP
	first.align = HudWidget.Align.END
	first.priority = 10
	first.custom_minimum_size = Vector2(40.0, 20.0)
	_check(ui.registry.register(first), "内置路径：注册元素成功")

	var second := HudWidget.new()
	second.name = "SecondWidget"
	second.widget_id = &"smoke_second"
	second.tray = HudWidget.Tray.TOP
	second.align = HudWidget.Align.END
	second.priority = 20
	second.custom_minimum_size = Vector2(40.0, 20.0)
	_check(ui.registry.register(second), "内置路径：第二个元素注册成功")
	await process_frame
	var top_end: Control = slots.get("top_end")
	var order_ok := top_end != null and top_end.get_child_count() >= 2 \
		and top_end.get_child(top_end.get_child_count() - 1) == second
	_check(order_ok, "priority 排序：大优先级靠屏幕边缘")

	# 方案显隐：仅键鼠的元素切到触屏后隐藏。
	first.schemes = UiState.Scheme.KBM
	ui.call("_set_scheme", UiState.Scheme.TOUCH)
	await process_frame
	_check(not first.visible, "输入方案切换：KBM 元素在触屏方案下隐藏")
	_check(second.visible, "输入方案切换：全方案元素保持可见")
	ui.call("_set_scheme", UiState.Scheme.KBM)

	# 紧凑显隐。
	first.compact_level = 1
	first.apply_compact(1)
	_check(not first.visible, "紧凑等级：compact_level=1 在 level=1 时隐藏")
	first.apply_compact(0)
	_check(first.visible, "紧凑等级：恢复后重新可见")

	# 菜单：HUD 隐藏 / 恢复，UiState 深度正确。
	ui.open_pause_menu()
	await process_frame
	_check(UiState.is_menu_open(), "菜单打开：UiState.menu_depth > 0")
	_check(not second.visible, "菜单打开：HUD 全部隐藏")
	ui.close_pause_menu()
	await process_frame
	_check(not UiState.is_menu_open(), "菜单关闭：menu_depth 归零")
	_check(second.visible, "菜单关闭：HUD 恢复")

	# 暂停菜单按钮 -> UiRoot 的接线：Resume 信号必须能关闭菜单。
	ui.open_pause_menu()
	await process_frame
	_check(ui.pause_menu.visible and ui.pause_menu.is_open(), "暂停菜单：open() 后可见")
	_check(not ui.touch_controls.visible, "暂停菜单：打开时隐藏触屏控件")
	ui.pause_menu.resume_requested.emit()
	await process_frame
	_check(not UiState.is_menu_open(), "暂停菜单：Resume 信号经 UiRoot 关闭菜单")

	# Settings 滑条 -> UiRoot.set_ui_scale()。
	ui.pause_menu.ui_scale_changed.emit(1.2)
	await process_frame
	await process_frame
	await process_frame
	_check(is_equal_approx(UiState.ui_scale, 1.2), "UI 缩放：滑条信号生效（实际 %.2f）" % UiState.ui_scale)
	ui.pause_menu.ui_scale_changed.emit(1.0)
	await process_frame
	await process_frame
	await process_frame
	_check(is_equal_approx(UiState.ui_scale, 1.0), "UI 缩放：恢复 1.0")

	# mod 路径：运行时 PackedScene + meta。
	var mod_widget := HudWidget.new()
	mod_widget.name = "ModWidget"
	mod_widget.custom_minimum_size = Vector2(32.0, 32.0)
	var packed := PackedScene.new()
	var pack_err := packed.pack(mod_widget)
	_check(pack_err == OK, "mod 场景打包成功")
	var registered: HudWidget = ui.register_external(packed, {
		"id": "smoke_mod",
		"tray": HudWidget.Tray.BOTTOM,
		"align": HudWidget.Align.CENTER,
		"priority": 5,
	})
	_check(registered != null, "mod 路径：register_external 成功")
	if registered != null:
		await process_frame
		_check(registered.get_parent() != null, "mod 路径：元素已进入槽位")
		_check(not ui.registry.register(registered), "mod 路径：重复 id 被拒绝")

	# 尺寸拒绝：超过上限的 mod UI。
	var huge := HudWidget.new()
	huge.name = "HugeWidget"
	huge.custom_minimum_size = Vector2(2000.0, 900.0)
	var huge_scene := PackedScene.new()
	huge_scene.pack(huge)
	_check(ui.register_external(huge_scene, {"id": "smoke_huge"}) == null, "mod 路径：超尺寸被拒绝")

	# 安全区字段：桌面为 0，移动端由 DisplayServer 提供（headless 为 0）。
	_check(UiState.safe_insets == Vector4.ZERO, "桌面安全区内缩为 0")
	_check(UiState.compact_level >= 0 and UiState.compact_level <= 2, "断点等级合法")

	_finish()


func _check(condition: bool, message: String) -> void:
	_checks += 1
	if condition:
		print("[ui_smoke] PASS  " + message)
	else:
		_failures.append(message)
		print("[ui_smoke] FAIL  " + message)


func _finish() -> void:
	print("[ui_smoke] checks=%d failures=%d" % [_checks, _failures.size()])
	if _failures.is_empty():
		print("UI_SMOKE PASS")
		quit(0)
	else:
		for failure in _failures:
			print("  - " + failure)
		print("UI_SMOKE FAIL")
		quit(1)
