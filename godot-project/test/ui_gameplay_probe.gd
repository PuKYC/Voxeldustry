extends SceneTree
## 临时集成探针：加载 gameplay_test.tscn，检查 UiRoot 与真实 BevyClient / CameraRig / 暂停接线。

func _initialize() -> void:
	_run.call_deferred()


func _run() -> void:
	var packed: PackedScene = load("res://test/gameplay_test.tscn")
	if packed == null:
		print("PROBE FAIL load scene")
		quit(1)
		return
	var scene := packed.instantiate()
	root.add_child(scene)
	for i in range(90):
		await process_frame

	var ui = scene.get_node_or_null("UiRoot")
	print("PROBE ui=", ui != null)
	if ui == null:
		quit(1)
		return
	var client = ui.client
	print("PROBE client=", client != null, " widgets=", ui.registry.widgets().size())

	# P3：真实通道表 -> ButtonSpecs -> ActionButtonHost 的端到端检查。
	var host = ui.action_host
	var button_count: int = 0 if host == null else host.buttons().size()
	var tool_buttons := 0
	if host != null:
		for button in host.buttons():
			if button.channel_key() == "tool":
				tool_buttons += 1
	print("PROBE action_host=", host != null, " buttons=", button_count,
		" tool_buttons=", tool_buttons, " channels=", 0 if host == null else host.channel_table().size())
	if host == null or button_count <= 0 or tool_buttons != 2:
		print("PROBE FAIL action buttons")
		quit(1)
		return
	print("PROBE touch=", ui.touch_controls != null, " pause=", ui.pause_menu != null)
	var camera = get_first_node_in_group("camera_rig")
	print("PROBE camera=", camera != null)
	if camera == null:
		print("PROBE FAIL no camera")
		quit(1)
		return

	ui.open_pause_menu()
	await process_frame
	print("PROBE menu_open=", UiState.is_menu_open(), " backend_paused=", UiState.backend_paused, " camera_blocked=", camera._ui_blocked)
	var paused_ok: bool = UiState.is_menu_open() and UiState.backend_paused and bool(camera._ui_blocked)
	ui.close_pause_menu()
	await process_frame
	print("PROBE menu_closed=", not UiState.is_menu_open(), " backend_paused=", UiState.backend_paused, " camera_blocked=", camera._ui_blocked)
	var resumed_ok: bool = not UiState.is_menu_open() and not UiState.backend_paused and not bool(camera._ui_blocked)

	if not paused_ok or not resumed_ok:
		print("PROBE FAIL pause")
		quit(1)
		return

	print("PROBE PASS")
	quit(0)
