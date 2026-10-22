class_name ActionButtonHost
extends Node
## 动作按钮宿主：把 BevyInputContext 的通道表映射成 ActionButton 并注册进 HudRegistry。
##
## 数据流（真实链路）：
##   ui_root.client -> client.input_context (BevyInputContext)
##   - channels() 拉通道表（channel 由 Rust 提供）；
##   - has_channel(key) / is_channel_available(key) 初始化 present / available；
##   - channel_changed(key, present, available) 增量刷新。
## 映射规则在 ButtonSpecs：tool 通道两个按钮，没有显式规格的通道走 fallback_spec。
##
## 不写坐标：按钮只声明 tray / align / priority / schemes，布局交给托盘 + SafeArea。
## 无 client（headless 冒烟 / 纯 UI 预览）时按钮数为 0，不报错。
##
## 测试注入缝：apply_channels / set_channel_state / buttons / channel_table /
## destroy / release_all（ui_smoke_test 用这些不依赖 BevyClient）。

var _ui_root = null
var _registry: HudRegistry = null
var _client = null
var _input_context = null
var _channels: Array = []
## key -> [present, available]；与 _channels 同步维护。
var _channel_states: Dictionary[String, Array] = {}
var _buttons: Array[ActionButton] = []


## 绑定 UiRoot 与注册表，并尝试接上真实 client。
func setup(ui_root: Node, registry: HudRegistry) -> void:
	_ui_root = ui_root
	_registry = registry
	_connect_ui_root()
	_bind_client()
	_reload_channels()


## 补偿刷新：UiRoot 在 client 的 input_capabilities_changed 时调用。
## 覆盖「client 晚于 UiRoot._ready 出现」「input_context 晚于 client 绑定」
## 「channels() 晚于 input_context 就绪」三种晚到情况。
func refresh() -> void:
	_bind_client()
	_reload_channels()


## 测试 / 外部注入：直接设通道表并重建按钮。
## 无 input_context 时按「present = available = true」建按钮；
## 行里显式给 present / available 时以行为准（方便构造异常态）。
func apply_channels(rows: Array) -> void:
	_channels = rows.duplicate()
	_channel_states.clear()
	for row in _channels:
		if not (row is Dictionary):
			continue
		var channel: Dictionary = row
		var key := String(channel.get("key", ""))
		if key.is_empty():
			continue
		var present := true
		var available := true
		if channel.has("present"):
			present = bool(channel["present"])
		if channel.has("available"):
			available = bool(channel["available"])
		if _input_context != null:
			if _input_context.has_method("has_channel"):
				present = bool(_input_context.has_channel(key))
			if _input_context.has_method("is_channel_available"):
				available = bool(_input_context.is_channel_available(key))
		_channel_states[key] = [present, available]
	_rebuild()


## 测试 / 外部注入：单通道状态变化（真实链路走 channel_changed 信号）。
func set_channel_state(key: String, present: bool, available: bool) -> void:
	_channel_states[key] = [present, available]
	for button in _buttons:
		if button != null and is_instance_valid(button) and button.channel_key() == key:
			button.set_channel_state(present, available)


func buttons() -> Array:
	return _buttons.duplicate()


func channel_table() -> Array:
	return _channels.duplicate()


## 释放所有按钮的按下状态（菜单 / 方案切走 / 失焦 / 退出时调用）。
func release_all() -> void:
	for button in _buttons:
		if button != null and is_instance_valid(button):
			button.release()


## 拆除：释放动作、断信号、从注册表移除按钮。
## 不释放自身节点（宿主挂在 UiRoot 下，随它一起释放；测试里 destroy 后还能读状态）。
func destroy() -> void:
	release_all()
	_disconnect_all()
	_free_buttons()
	_channels.clear()
	_channel_states.clear()


# ───────────────────────── 构建 ─────────────────────────

func _reload_channels() -> void:
	var rows: Array = []
	if _input_context != null and _input_context.has_method("channels"):
		rows = _input_context.channels()
	apply_channels(rows)


func _rebuild() -> void:
	_free_buttons()
	for row in _channels:
		if not (row is Dictionary):
			continue
		var channel: Dictionary = row
		var key := String(channel.get("key", ""))
		var mode := String(channel.get("mode", ""))
		var actions: Array = channel.get("actions", [])
		# axis2d 是摇杆负责的移动轴，不出按钮；空 key / 空 actions 也不出。
		if key.is_empty() or mode == "axis2d" or actions.is_empty():
			continue
		var raw_specs := ButtonSpecs.specs_for_channel(key)
		if raw_specs.is_empty():
			raw_specs = [ButtonSpecs.fallback_spec(channel)]
		for raw in raw_specs:
			if raw is Dictionary:
				_create_button(ButtonSpecs.normalize_spec(raw, channel))


func _create_button(spec: Dictionary) -> void:
	var button := ActionButton.new()
	button.configure(spec)
	if _registry != null and is_instance_valid(_registry):
		if not _registry.register(button):
			button.free()
			return
	else:
		add_child(button)
	var key := String(spec.get("channel", ""))
	var state: Array = _channel_states.get(key, [true, true])
	button.set_channel_state(bool(state[0]), bool(state[1]))
	_buttons.append(button)


func _free_buttons() -> void:
	for button in _buttons:
		if button == null or not is_instance_valid(button):
			continue
		var removed := false
		if _registry != null and is_instance_valid(_registry):
			var id: StringName = button.widget_id
			if _registry.get_widget(id) == button:
				removed = _registry.unregister(id)
		if not removed:
			if button.get_parent() != null:
				button.get_parent().remove_child(button)
			button.queue_free()
	_buttons.clear()


# ───────────────────────── 真实链路 ─────────────────────────

func _resolve_client():
	if _ui_root != null and is_instance_valid(_ui_root):
		var client: Variant = null
		if _ui_root.has_method("get_client"):
			client = _ui_root.get_client()
		else:
			client = _ui_root.get("client")
		if client != null:
			return client
	if is_inside_tree():
		return get_tree().get_first_node_in_group("bevy_client")
	return null


func _bind_client() -> void:
	var client = _resolve_client()
	var context: Variant = null
	if client != null:
		context = client.get("input_context")
	if client == _client and context == _input_context:
		return
	_disconnect_input_context()
	_client = client
	if context == null or not context.has_signal("channel_changed"):
		return
	_input_context = context
	if not context.channel_changed.is_connected(_on_channel_changed):
		context.channel_changed.connect(_on_channel_changed)


func _disconnect_input_context() -> void:
	if _input_context != null and is_instance_valid(_input_context):
		if _input_context.has_signal("channel_changed"):
			if _input_context.channel_changed.is_connected(_on_channel_changed):
				_input_context.channel_changed.disconnect(_on_channel_changed)
	_input_context = null


func _disconnect_all() -> void:
	_disconnect_input_context()
	_client = null
	if _ui_root != null and is_instance_valid(_ui_root):
		if _ui_root.has_signal("menu_changed") and _ui_root.menu_changed.is_connected(_on_menu_changed):
			_ui_root.menu_changed.disconnect(_on_menu_changed)
		if _ui_root.has_signal("scheme_changed") and _ui_root.scheme_changed.is_connected(_on_scheme_changed):
			_ui_root.scheme_changed.disconnect(_on_scheme_changed)
	_ui_root = null


func _connect_ui_root() -> void:
	if _ui_root == null:
		return
	if _ui_root.has_signal("menu_changed"):
		if not _ui_root.menu_changed.is_connected(_on_menu_changed):
			_ui_root.menu_changed.connect(_on_menu_changed)
	if _ui_root.has_signal("scheme_changed"):
		if not _ui_root.scheme_changed.is_connected(_on_scheme_changed):
			_ui_root.scheme_changed.connect(_on_scheme_changed)


# ───────────────────────── 信号 / 生命周期 ─────────────────────────

func _on_channel_changed(key: String, present: bool, available: bool) -> void:
	# 未知 key 说明本宿主的通道表还是旧的 / 空的（client bind 晚于 UiRoot._ready），
	# 立刻重新拉表重建按钮；不能直接 return，否则按钮永远补不回来。
	if not _channel_states.has(key):
		_reload_channels()
		# 拉表后该 key 若已出现，补上这次边沿。
		if _channel_states.has(key):
			set_channel_state(key, present, available)
		return
	set_channel_state(key, present, available)


func _on_menu_changed(open: bool) -> void:
	if open:
		release_all()


func _on_scheme_changed(current: int) -> void:
	if (current & UiState.Scheme.TOUCH) == 0:
		release_all()


func _notification(what: int) -> void:
	if what == NOTIFICATION_WM_WINDOW_FOCUS_OUT:
		release_all()


func _exit_tree() -> void:
	release_all()
	_disconnect_all()
