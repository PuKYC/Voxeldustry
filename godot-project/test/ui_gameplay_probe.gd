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
	ui.close_pause_menu()
	await process_frame
	print("PROBE menu_closed=", not UiState.is_menu_open(), " backend_paused=", UiState.backend_paused, " camera_blocked=", camera._ui_blocked)

	print("PROBE PASS")
	quit(0)
