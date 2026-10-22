extends SceneTree
## UI headless 冒烟测试（无编辑器、无渲染依赖）。
##
## 覆盖：
## 1. UiRoot 场景可加载，托盘 9 个槽位全部创建；
## 2. HudWidget 注册 / 排序 / 方案显隐 / 紧凑显隐；
## 3. 菜单打开时 HUD 隐藏、关闭后恢复；
## 4. register_external（mod 路径）能通过 PackedScene 注册；
## 5. 逻辑尺寸 / 缩放 / 安全区状态字段有值；
## 6. P0：unregister 先 detach（不残留载荷回调 / 信号）、无 manager 不写 backend_paused；
## 7. P1：platform 与 scheme 分离（边距 / 缩放看 platform，不看输入方案）；
## 8. P2：scheme_overrides 覆盖 tray / align / priority / min_size，切回后恢复；
## 9. P4：同组互斥（同组只激活一个 priority 最高者）；
## 10. P5：混合设备防抖（切换冷却 / 摇杆漂移阈值 / 鼠标微动阈值）；
## 11. P3：ButtonSpecs 规格生成（tool 双按钮 / 缺规格兜底）、ActionButton 多点触摸
##     按下释放、通道 unavailable 立即释放、菜单 / 切方案隐藏；
## 12. P3b：交互提示用真实绑定（触屏显示 Tap）；
## 13. P6：移动端入口 —— 触屏暂停按钮、触摸可拖动滑条、debug 构建的暂停菜单 Debug 区。
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

	# P1：platform 与 scheme 分离 —— 边距 / 缩放看 platform，不看 scheme。
	_check(UiState.platform == UiState.Platform.DESKTOP, "P1：桌面 platform = DESKTOP（实际 %d）" % UiState.platform)
	ui.call("_set_scheme", UiState.Scheme.TOUCH)
	await process_frame
	_check(UiState.platform == UiState.Platform.DESKTOP, "P1：切触屏方案不改变 platform")
	_check(_safe_margin(ui) == int(UiRoot.EDGE_PAD), "P1：桌面切触屏方案后边距仍为 EDGE_PAD（实际 %d）" % _safe_margin(ui))
	ui.set("debug_force_mobile", true)
	ui.call("_apply_scale")
	ui.call("_schedule_metrics")
	await process_frame
	await process_frame
	await process_frame
	_check(UiState.platform == UiState.Platform.PHONE, "P1：debug_force_mobile 视为 PHONE（实际 %d）" % UiState.platform)
	_check(_safe_margin(ui) == int(UiRoot.TOUCH_PAD), "P1：PHONE 边距为 TOUCH_PAD（实际 %d）" % _safe_margin(ui))
	ui.set("debug_force_mobile", false)
	ui.call("_apply_scale")
	ui.call("_schedule_metrics")
	await process_frame
	await process_frame
	await process_frame
	_check(UiState.platform == UiState.Platform.DESKTOP, "P1：关闭 debug_force_mobile 回到 DESKTOP（实际 %d）" % UiState.platform)
	ui.call("_set_scheme", UiState.Scheme.KBM)
	await process_frame

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
	var order_ok := false
	if top_end != null:
		var first_index := -1
		var second_index := -1
		for i in range(top_end.get_child_count()):
			var child := top_end.get_child(i)
			if child == first:
				first_index = i
			elif child == second:
				second_index = i
		# 槽里还有内置元素（例如触屏暂停按钮），这里只断言两者的相对顺序。
		order_ok = first_index >= 0 and second_index > first_index
	_check(order_ok, "priority 排序：大优先级靠屏幕边缘")

	# 方案显隐：仅键鼠的元素切到触屏后隐藏。
	first.schemes = UiState.Scheme.KBM
	ui.call("_set_scheme", UiState.Scheme.TOUCH)
	await process_frame
	_check(not first.visible, "输入方案切换：KBM 元素在触屏方案下隐藏")
	_check(second.visible, "输入方案切换：全方案元素保持可见")
	ui.call("_set_scheme", UiState.Scheme.KBM)

	# P2：按方案覆盖 tray / align / priority / min_size。
	# 默认落 TOP+BEGIN，TOUCH 下覆盖成 BOTTOM+END 且最小尺寸变大。
	var mover := HudWidget.new()
	mover.name = "MoverWidget"
	mover.widget_id = &"smoke_mover"
	mover.tray = HudWidget.Tray.TOP
	mover.align = HudWidget.Align.BEGIN
	mover.custom_minimum_size = Vector2(40.0, 20.0)
	mover.scheme_overrides = {
		UiState.Scheme.TOUCH: {
			"tray": HudWidget.Tray.BOTTOM,
			"align": HudWidget.Align.END,
			"min_size": Vector2(120.0, 48.0),
		},
	}
	_check(ui.registry.register(mover), "P2：带 scheme_overrides 的元素注册成功")
	await process_frame
	_check(mover.get_parent() == slots.get("top_begin"), "P2：默认方案落在 top_begin")
	_check(mover.custom_minimum_size == Vector2(40.0, 20.0), "P2：默认方案最小尺寸为默认值")

	var mover2 := HudWidget.new()
	mover2.name = "MoverWidget2"
	mover2.widget_id = &"smoke_mover2"
	mover2.tray = HudWidget.Tray.TOP
	mover2.align = HudWidget.Align.BEGIN
	mover2.priority = 1
	mover2.custom_minimum_size = Vector2(40.0, 20.0)
	mover2.scheme_overrides = {
		UiState.Scheme.TOUCH: {
			"tray": HudWidget.Tray.BOTTOM,
			"align": HudWidget.Align.END,
			"priority": 30,
		},
	}
	_check(ui.registry.register(mover2), "P2：第二个带覆盖的元素注册成功")
	ui.call("_set_scheme", UiState.Scheme.TOUCH)
	await process_frame
	_check(mover.get_parent() == slots.get("bottom_end"), "P2：TOUCH 方案覆盖后搬到 bottom_end")
	_check(mover.custom_minimum_size == Vector2(120.0, 48.0),
		"P2：TOUCH 方案最小尺寸被覆盖（实际 %s）" % str(mover.custom_minimum_size))
	var bottom_end: Control = slots.get("bottom_end")
	_check(bottom_end != null and bottom_end.get_child_count() >= 2
			and bottom_end.get_child(bottom_end.get_child_count() - 1) == mover2,
		"P2：覆盖后同槽仍按 priority 排序（大者贴 END 边）")

	# 覆盖后的元素仍然参与紧凑 / 菜单的显隐。
	mover.compact_level = 1
	mover.apply_compact(1)
	_check(not mover.visible, "P2：覆盖后的元素仍响应紧凑等级")
	mover.apply_compact(0)
	mover.compact_level = 0
	ui.open_pause_menu()
	await process_frame
	_check(not mover.visible, "P2：覆盖后的元素仍响应菜单隐藏")
	ui.close_pause_menu()
	await process_frame

	ui.call("_set_scheme", UiState.Scheme.KBM)
	await process_frame
	_check(mover.get_parent() == slots.get("top_begin"), "P2：切回键鼠方案后落点恢复")
	_check(mover.custom_minimum_size == Vector2(40.0, 20.0),
		"P2：切回后最小尺寸恢复默认（实际 %s）" % str(mover.custom_minimum_size))

	# P4：同组互斥 —— 同组多个元素同时命中当前方案时只激活 priority 最高的一个。
	var panel := HudWidget.new()
	panel.name = "GroupPanel"
	panel.widget_id = &"smoke_group_panel"
	panel.group_id = &"smoke_inventory"
	panel.schemes = UiState.Scheme.KBM
	panel.priority = 10
	panel.custom_minimum_size = Vector2(40.0, 20.0)
	var drawer := HudWidget.new()
	drawer.name = "GroupDrawer"
	drawer.widget_id = &"smoke_group_drawer"
	drawer.group_id = &"smoke_inventory"
	drawer.schemes = UiState.Scheme.TOUCH
	drawer.priority = 20
	drawer.custom_minimum_size = Vector2(40.0, 20.0)
	_check(ui.registry.register(panel), "P4：同组面板元素注册成功")
	_check(ui.registry.register(drawer), "P4：同组抽屉元素注册成功")
	ui.call("_set_scheme", UiState.Scheme.KBM)
	await process_frame
	_check(panel.visible and not drawer.visible, "P4：键鼠方案只激活同组里命中的那个")
	drawer.schemes = UiState.Scheme.KBM | UiState.Scheme.TOUCH
	ui.registry.refresh_scheme(UiState.Scheme.KBM)
	await process_frame
	_check(drawer.visible and not panel.visible, "P4：同组同时命中时只激活 priority 最高者")
	drawer.schemes = UiState.Scheme.TOUCH
	ui.registry.refresh_scheme(UiState.Scheme.KBM)
	await process_frame
	_check(panel.visible and not drawer.visible, "P4：同组恢复互补方案后回到各自激活")

	# P5：混合设备防抖 —— 冷却期内输入事件不夺回方案，摇杆漂移 / 鼠标微动不算换设备。
	ui.set("scheme_switch_cooldown", 0.5)
	ui.set("_last_scheme_switch_ms", 0)
	ui.call("_set_scheme", UiState.Scheme.KBM)
	ui.set("_last_scheme_switch_ms", 0)
	var joy_event := InputEventJoypadButton.new()
	joy_event.button_index = JOY_BUTTON_A
	joy_event.pressed = true
	ui.call("_input", joy_event)
	await process_frame
	_check(UiState.scheme == UiState.Scheme.GAMEPAD, "P5：手柄按键切到 GAMEPAD")
	var key_event := InputEventKey.new()
	key_event.physical_keycode = KEY_E
	key_event.pressed = true
	ui.call("_input", key_event)
	await process_frame
	_check(UiState.scheme == UiState.Scheme.GAMEPAD, "P5：冷却期内键鼠事件不夺回方案")
	ui.set("scheme_switch_cooldown", 0.0)
	ui.call("_input", key_event)
	await process_frame
	_check(UiState.scheme == UiState.Scheme.KBM, "P5：冷却结束后键鼠事件接管")
	var drift := InputEventJoypadMotion.new()
	drift.axis = JOY_AXIS_LEFT_X
	drift.axis_value = 0.05
	ui.set("_last_scheme_switch_ms", 0)
	ui.call("_input", drift)
	await process_frame
	_check(UiState.scheme == UiState.Scheme.KBM, "P5：摇杆漂移（低于阈值）不切方案")
	var push := InputEventJoypadMotion.new()
	push.axis = JOY_AXIS_LEFT_X
	push.axis_value = 0.9
	ui.set("_last_scheme_switch_ms", 0)
	ui.call("_input", push)
	await process_frame
	_check(UiState.scheme == UiState.Scheme.GAMEPAD, "P5：超过阈值的手柄摇杆切到 GAMEPAD")
	var micro := InputEventMouseMotion.new()
	micro.relative = Vector2(0.5, 0.0)
	ui.call("_input", micro)
	await process_frame
	_check(UiState.scheme == UiState.Scheme.GAMEPAD, "P5：鼠标微动（低于阈值）不切方案")
	var sweep := InputEventMouseMotion.new()
	sweep.relative = Vector2(24.0, 0.0)
	ui.call("_input", sweep)
	await process_frame
	_check(UiState.scheme == UiState.Scheme.KBM, "P5：正常鼠标位移切回 KBM")
	ui.set("scheme_switch_cooldown", 0.5)
	ui.set("_last_scheme_switch_ms", 0)

	# P3：动作按钮映射移到 Godot —— ButtonSpecs 生成规格，ActionButtonHost 建按钮，
	# ActionButton 自己按下 / 松开（多点触控按 index 各自独立）。
	_check(ui.action_host != null, "P3：UiRoot 已创建 ActionButtonHost")
	if ui.action_host != null:
		var host: ActionButtonHost = ui.action_host
		_check(host.buttons().is_empty(), "P3：无 BevyClient 时按钮数为 0（实际 %d）" % host.buttons().size())
		# 规格表与 Rust 动作名保持一致（防 ACTION_TABLE 漂移）。
		_check(ButtonSpecs.specs_for_channel("tool").size() == 2, "P3：tool 通道有两条规格")
		var flight_specs := ButtonSpecs.specs_for_channel("flight")
		_check(not flight_specs.is_empty()
				and String((flight_specs[0] as Dictionary).get("action", "")) == "flight_toggle",
			"P3：flight 动作名与 Rust 表一致（flight_toggle）")
		for action in ["jump", "primary_tool", "secondary_tool", "turbo_action"]:
			if not InputMap.has_action(action):
				InputMap.add_action(action)
		host.apply_channels([
			{"key": "move", "mode": "axis2d", "actions": ["move_forward"]},
			{"key": "jump", "mode": "button", "actions": ["jump"]},
			{"key": "tool", "mode": "hold", "actions": ["primary_tool", "secondary_tool"]},
			{"key": "turbo", "mode": "toggle", "actions": ["turbo_action"]},
		])
		await process_frame
		# 期望 4 个：jump 1 + tool 2 + turbo 兜底 1；move 是 axis2d 不出按钮。
		_check(host.buttons().size() == 4,
			"P3：tool 生成两个、无规格通道走兜底、axis2d 不出按钮（实际 %d）" % host.buttons().size())
		var by_id := {}
		for button: ActionButton in host.buttons():
			by_id[button.widget_id] = button
		_check(by_id.has(&"btn_primary") and by_id.has(&"btn_secondary"), "P3：tool 通道两个按钮 id 正确")
		_check(by_id.has(&"btn_turbo"), "P3：缺规格通道用兜底规格（btn_turbo）")
		ui.call("_set_scheme", UiState.Scheme.TOUCH)
		await process_frame
		var jump_button: ActionButton = by_id.get(&"btn_jump", null)
		var turbo_button: ActionButton = by_id.get(&"btn_turbo", null)
		_check(jump_button != null and jump_button.visible, "P3：触屏方案下动作按钮可见")

		var touch_a := InputEventScreenTouch.new()
		touch_a.index = 0
		touch_a.pressed = true
		touch_a.position = jump_button.size * 0.5
		jump_button._gui_input(touch_a)
		_check(Input.is_action_pressed("jump"), "P3：按钮按下注入动作")
		var touch_b := InputEventScreenTouch.new()
		touch_b.index = 1
		touch_b.pressed = true
		touch_b.position = turbo_button.size * 0.5
		turbo_button._gui_input(touch_b)
		_check(Input.is_action_pressed("jump") and Input.is_action_pressed("turbo_action"),
			"P3：两根手指同时按住两个按钮都生效")
		var touch_up := InputEventScreenTouch.new()
		touch_up.index = 0
		touch_up.pressed = false
		jump_button._gui_input(touch_up)
		_check(not Input.is_action_pressed("jump"), "P3：松开按钮释放动作")
		_check(Input.is_action_pressed("turbo_action"), "P3：松开一根手指不影响另一个按钮")
		# toggle：按住 turbo 后 0.1s 自动释放，形成一次完整边沿。
		await create_timer(0.25).timeout
		_check(not Input.is_action_pressed("turbo_action"), "P3：toggle 模式自动释放动作")

		# available=false：立即释放该动作并隐藏？（present 管显隐，available 管置灰）
		host.set_channel_state("jump", true, true)
		var touch_c := InputEventScreenTouch.new()
		touch_c.index = 2
		touch_c.pressed = true
		touch_c.position = jump_button.size * 0.5
		jump_button._gui_input(touch_c)
		_check(Input.is_action_pressed("jump"), "P3：重新按下按钮")
		host.set_channel_state("jump", true, false)
		_check(not Input.is_action_pressed("jump"), "P3：通道不可用时立即释放动作")
		host.set_channel_state("jump", false, true)
		_check(not jump_button.visible, "P3：通道 present=false 时按钮隐藏")
		host.set_channel_state("jump", true, true)
		await process_frame
		_check(jump_button.visible, "P3：通道恢复后按钮重新可见")

		# 菜单打开 / 方案切走时释放全部动作并隐藏按钮。
		host.release_all()
		ui.open_pause_menu()
		await process_frame
		_check(not jump_button.visible, "P3：菜单打开时动作按钮隐藏")
		ui.close_pause_menu()
		await process_frame
		ui.call("_set_scheme", UiState.Scheme.KBM)
		await process_frame
		_check(not jump_button.visible, "P3：切回键鼠方案后动作按钮隐藏")
		host.release_all()
		for action in ["jump", "primary_tool", "secondary_tool", "turbo_action"]:
			if InputMap.has_action(action):
				Input.action_release(action)

		# 规格字段原样落到 HudWidget（P2 集成：ActionButton 不自带覆盖逻辑）。
		var spec_button := ActionButton.new()
		spec_button.configure(ButtonSpecs.normalize_spec({
			"id": "btn_probe",
			"label": "Probe",
			"action": "jump",
			"tray": HudWidget.Tray.BOTTOM,
			"align": HudWidget.Align.BEGIN,
			"priority": 7,
			"schemes": UiState.Scheme.TOUCH | UiState.Scheme.KBM,
			"scheme_overrides": {UiState.Scheme.TOUCH: {"tray": HudWidget.Tray.TOP}},
		}, {"key": "jump", "mode": "button", "actions": ["jump"]}))
		_check(spec_button.tray == HudWidget.Tray.BOTTOM and spec_button.priority == 7
				and spec_button.schemes == (UiState.Scheme.TOUCH | UiState.Scheme.KBM)
				and spec_button.scheme_overrides.has(UiState.Scheme.TOUCH),
			"P3：规格字段原样落到 HudWidget")
		spec_button.free()

		# 无 client 时 refresh() 要安全：清空按钮而不是报错。
		host.refresh()
		await process_frame
		_check(host.buttons().is_empty(), "P3：无 client 时 refresh() 安全清空按钮")
		host.destroy()
		await process_frame
		_check(ui.registry.get_widget(&"btn_jump") == null, "P3：destroy 后按钮已从注册表移除")

	# P3b：交互提示用真实绑定（触屏固定 Tap）。
	var prompt = ui.registry.get_widget(&"interact_prompt")
	_check(prompt != null, "P3b：交互提示元素已注册")
	if prompt != null:
		var prompt_label: Label = prompt.get_node_or_null("Prompt")
		_check(prompt_label != null, "P3b：交互提示有 Prompt Label")
		if prompt_label != null:
			ui.call("_set_scheme", UiState.Scheme.KBM)
			await process_frame
			_check(prompt_label.text.contains("Interact"), "P3b：键鼠方案提示含 Interact（实际 \"%s\"）" % prompt_label.text)
			# 真实绑定：注册 E 后提示变 "[E] Interact"，改键成 F 后跟着变。
			if not InputMap.has_action("interact"):
				InputMap.add_action("interact")
			var key_e := InputEventKey.new()
			key_e.physical_keycode = KEY_E
			InputMap.action_add_event("interact", key_e)
			prompt.call("_refresh_prompt")
			_check(prompt_label.text == "[E] Interact", "P3b：提示跟随 InputMap 绑定（实际 \"%s\"）" % prompt_label.text)
			InputMap.action_erase_event("interact", key_e)
			var key_f := InputEventKey.new()
			key_f.physical_keycode = KEY_F
			InputMap.action_add_event("interact", key_f)
			prompt.call("_refresh_prompt")
			_check(prompt_label.text == "[F] Interact", "P3b：改键后提示跟着变（实际 \"%s\"）" % prompt_label.text)
			InputMap.action_erase_event("interact", key_f)
			ui.call("_set_scheme", UiState.Scheme.TOUCH)
			await process_frame
			_check(prompt_label.text == "Tap", "P3b：触屏方案提示为 Tap（实际 \"%s\"）" % prompt_label.text)
			ui.call("_set_scheme", UiState.Scheme.KBM)
			await process_frame

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

	# P0：没有 manager（headless 无后端）时不能把 backend_paused 置 true。
	# 冒烟场景 root 下没有 BevyAppManager，open_pause_menu 会走 manager == null 分支。
	UiState.backend_paused = false
	ui.open_pause_menu()
	await process_frame
	_check(not UiState.backend_paused, "P0：无 manager 时不写 backend_paused")
	ui.close_pause_menu()
	await process_frame

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
	_check(ui.registry.get_widget(&"smoke_huge") == null, "mod 路径：超尺寸元素未留在注册表")

	# P2：mod meta 支持 scheme_overrides，且 640x480 上限对所有方案的 min_size 都生效。
	var meta_scene := HudWidget.new()
	meta_scene.name = "ModMetaWidget"
	meta_scene.custom_minimum_size = Vector2(32.0, 32.0)
	var packed_meta := PackedScene.new()
	packed_meta.pack(meta_scene)
	var meta_widget: HudWidget = ui.register_external(packed_meta, {
		"id": "smoke_mod_meta",
		"scheme_overrides": {
			UiState.Scheme.TOUCH: {
				"tray": HudWidget.Tray.BOTTOM,
				"align": HudWidget.Align.BEGIN,
				"min_size": Vector2(48.0, 48.0),
			},
		},
	})
	_check(meta_widget != null, "P2：mod meta 可传 scheme_overrides")
	if meta_widget != null:
		_check(meta_widget.scheme_overrides.has(UiState.Scheme.TOUCH), "P2：mod meta 的覆盖已写入元素")
		ui.call("_set_scheme", UiState.Scheme.TOUCH)
		await process_frame
		_check(meta_widget.get_parent() == slots.get("bottom_begin"), "P2：mod 元素按覆盖落到 bottom_begin")
		ui.call("_set_scheme", UiState.Scheme.KBM)
		await process_frame

	var wide_scene := HudWidget.new()
	wide_scene.name = "ModWideWidget"
	wide_scene.custom_minimum_size = Vector2(32.0, 32.0)
	var packed_wide := PackedScene.new()
	packed_wide.pack(wide_scene)
	_check(ui.register_external(packed_wide, {
		"id": "smoke_mod_wide",
		"scheme_overrides": {UiState.Scheme.TOUCH: {"min_size": Vector2(2000.0, 40.0)}},
	}) == null, "P2：mod 只在 TOUCH 方案超尺寸也被拒绝")

	# P0：unregister 必须先 detach —— 否则被拒 / 被卸载的 mod 元素会在 client 上留下回调。
	var stub := _StubClient.new()
	stub.name = "StubClient"
	root.add_child(stub)
	ui.client = stub
	var probe := _ProbeWidget.new()
	probe.name = "ProbeWidget"
	probe.widget_id = &"smoke_probe"
	probe.custom_minimum_size = Vector2(32.0, 32.0)
	_check(ui.registry.register(probe), "P0：可断回调的探针元素注册成功")
	_check(stub.listen_count == 1, "P0：attach 时订阅了载荷回调（实际 %d）" % stub.listen_count)
	_check(stub.controlled_entity_changed.get_connections().size() == 1, "P0：attach 时连了受控实体信号")
	_check(ui.registry.unregister(&"smoke_probe"), "P0：unregister 成功")
	_check(stub.listen_count == 0, "P0：unregister 后载荷回调已取消（实际 %d）" % stub.listen_count)
	_check(stub.controlled_entity_changed.get_connections().size() == 0, "P0：unregister 后信号已断开")
	ui.client = null

	# 安全区字段：桌面为 0，移动端由 DisplayServer 提供（headless 为 0）。
	_check(UiState.safe_insets == Vector4.ZERO, "桌面安全区内缩为 0")
	_check(UiState.compact_level >= 0 and UiState.compact_level <= 2, "断点等级合法")

	# P6-A：触屏暂停按钮 —— 手机没有 Esc / F3，靠这个按钮进暂停菜单。
	var pause_button = ui.registry.get_widget(&"pause_button")
	_check(pause_button != null, "P6：触屏暂停按钮已注册")
	if pause_button != null:
		ui.call("_set_scheme", UiState.Scheme.KBM)
		await process_frame
		_check(not pause_button.visible, "P6：键鼠方案下暂停按钮隐藏")
		ui.call("_set_scheme", UiState.Scheme.TOUCH)
		await process_frame
		_check(pause_button.visible, "P6：触屏方案下暂停按钮可见")
		var tap := InputEventScreenTouch.new()
		tap.index = 3
		tap.pressed = true
		pause_button._gui_input(tap)
		await process_frame
		_check(UiState.is_menu_open(), "P6：点暂停按钮打开暂停菜单")
		ui.close_pause_menu()
		await process_frame
		ui.call("_set_scheme", UiState.Scheme.KBM)
		await process_frame

	# P6-B：原生 HSlider 只认鼠标事件，触屏滑条必须自己处理 ScreenTouch / ScreenDrag。
	ui.open_pause_menu()
	await process_frame
	ui.pause_menu.call("_on_settings_pressed")
	await process_frame
	await process_frame
	var scale_slider: HSlider = ui.pause_menu.get_node("%ScaleSlider")
	var slider_touch := InputEventScreenTouch.new()
	slider_touch.index = 4
	slider_touch.pressed = true
	slider_touch.position = Vector2(scale_slider.size.x * 0.9, scale_slider.size.y * 0.5)
	scale_slider.emit_signal("gui_input", slider_touch)
	await process_frame
	_check(scale_slider.value > 1.15, "P6：触摸按下滑条右端后数值变大（实际 %.2f）" % scale_slider.value)
	var slider_drag := InputEventScreenDrag.new()
	slider_drag.index = 4
	slider_drag.position = Vector2(scale_slider.size.x * 0.05, scale_slider.size.y * 0.5)
	slider_drag.relative = slider_drag.position - slider_touch.position
	scale_slider.emit_signal("gui_input", slider_drag)
	await process_frame
	_check(scale_slider.value < 0.9, "P6：触摸拖动后数值变小（实际 %.2f）" % scale_slider.value)
	var slider_release := InputEventScreenTouch.new()
	slider_release.index = 4
	slider_release.pressed = false
	slider_release.position = slider_drag.position
	scale_slider.emit_signal("gui_input", slider_release)
	await process_frame
	_check(is_equal_approx(UiState.ui_scale, scale_slider.value),
		"P6：滑条触摸经 ui_scale_changed 写回 UiState（ui_scale=%.2f）" % UiState.ui_scale)

	# P6-C：debug 构建的暂停菜单 Debug 区 —— F3 的移动端 / 手柄替代入口。
	var pause_menu = ui.pause_menu
	pause_menu.set("force_show_debug", true)
	pause_menu.call("apply_debug_visibility")
	await process_frame
	var debug_button: Button = pause_menu.get_node("%DebugButton")
	_check(debug_button.visible, "P6：debug 构建显示调试入口")
	debug_button.pressed.emit()
	await process_frame
	_check(pause_menu.get_node("%DebugPanel").visible, "P6：打开 Debug 面板")
	var overlay_button: Button = pause_menu.get_node("%DebugOverlayButton")
	var force_touch_button: Button = pause_menu.get_node("%ForceTouchButton")
	overlay_button.pressed.emit()
	await process_frame
	_check(ui.debug_overlay.visible, "P6：Debug 区可打开调试层")
	_check(overlay_button.text.contains("ON"), "P6：调试层按钮文案同步（实际 %s）" % overlay_button.text)
	overlay_button.pressed.emit()
	await process_frame
	_check(not ui.debug_overlay.visible, "P6：再次点击关闭调试层")
	force_touch_button.pressed.emit()
	await process_frame
	_check(UiState.has_scheme(UiState.Scheme.TOUCH), "P6：Debug 区可强制触屏方案")
	var hint: Label = pause_menu.get_node("%Hint")
	_check(hint.text == "Tap Resume", "P6：触屏方案下暂停提示改 Tap（实际 %s）" % hint.text)
	force_touch_button.pressed.emit()
	await process_frame
	_check(UiState.has_scheme(UiState.Scheme.KBM), "P6：再次点击回到键鼠方案")
	pause_menu.set("force_show_debug", false)
	pause_menu.call("apply_debug_visibility")
	await process_frame
	_check(debug_button.visible == OS.is_debug_build(),
		"P6：release 构建不显示调试入口（当前 debug=%s）" % str(OS.is_debug_build()))
	ui.close_pause_menu()
	await process_frame
	ui.set_debug_overlay(false)
	ui.set_ui_scale(1.0)
	await process_frame

	_finish()


## P0 测试替身：只记录 listen / unlisten，不真正派发载荷。
class _StubClient:
	extends Node

	signal controlled_entity_changed(entity)

	var listen_count := 0


	func listen_component(_kind: int, _cb: Callable) -> void:
		listen_count += 1


	func unlisten_component(_kind: int, _cb: Callable) -> void:
		listen_count = maxi(0, listen_count - 1)


## P0 测试替身：attach 时挂回调、detach 时摘掉（模拟 HealthBar 的订阅习惯）。
class _ProbeWidget:
	extends HudWidget

	var _client = null
	var _cb: Callable


	func _on_attach() -> void:
		_client = ui_root.get_client()
		if _client == null:
			return
		_cb = _on_entity_changed
		_client.listen_component(BevyEnums.Payload.HEALTH, _cb)
		_client.connect(&"controlled_entity_changed", _on_entity_changed)


	func _on_detach() -> void:
		if _client == null:
			return
		if _client.has_method("unlisten_component"):
			_client.unlisten_component(BevyEnums.Payload.HEALTH, _cb)
		if _client.is_connected(&"controlled_entity_changed", _on_entity_changed):
			_client.disconnect(&"controlled_entity_changed", _on_entity_changed)
		_client = null


	func _on_entity_changed(_e) -> void:
		pass


## SafeArea 的基础边距（不含安全区 inset；headless 下 inset 恒 0）。
func _safe_margin(ui: CanvasLayer) -> int:
	var safe: MarginContainer = ui.get("_safe")
	if safe == null:
		return -1
	return safe.get_theme_constant("margin_left")


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
