class_name ActionButtonHost
extends Node
## 动作按钮宿主：把 BevyInputContext 的通道表 + 交互目标，映射成触屏动作簇
## （ActionCluster）与键鼠 / 手柄提示（ActionHints），各注册为一个 HudWidget。
##
## 数据流（真实链路）：
##   ui_root.client -> client.input_context (BevyInputContext)
##   - channels() 拉通道表（通道由 Rust 提供）；
##   - has_channel(key) / is_channel_available(key) 初始化 present / available；
##   - channel_changed(key, present, available) 增量刷新；
##   - interaction_target_changed(target, enabled) / interaction_target() 取交互目标。
## 槽位解析在 InputBindings：CONTEXT 布局主按钮在 attack / break_block 之间按目标切换。
##
## 无 client（headless 冒烟 / 纯 UI 预览）时不报错，按钮数为 0。
##
## 测试注入缝：apply_channels / set_channel_state / set_interaction_target / buttons /
## channel_table / refresh / refresh_settings / release_all / destroy
## （ui_smoke_test 用这些，不依赖 BevyClient）。

var _ui_root = null
var _registry: HudRegistry = null
var _client = null
var _input_context = null
var _channels: Array = []
## key -> [present, available]；与 _channels 同步维护。
var _channel_states: Dictionary = {}
## 交互上下文：{"target_kind": int, "interact_enabled": bool}。
var _context: Dictionary = {
	"target_kind": InputBindings.TARGET_NONE,
	"interact_enabled": false,
}
## 触屏动作簇 / 键鼠手柄提示。
var _cluster: ActionCluster = null
var _hints: ActionHints = null


## 绑定 UiRoot 与注册表，创建并注册动作簇 / 提示，然后尝试接上真实 client。
func setup(ui_root: Node, registry: HudRegistry) -> void:
	_ui_root = ui_root
	_registry = registry
	_connect_ui_root()
	_build_widgets()
	_bind_client()
	_reload_channels()


## 补偿刷新：UiRoot 在 client 的 input_capabilities_changed 时调用。
## 覆盖「client 晚于 UiRoot._ready 出现」「input_context 晚于 client 绑定」
## 「channels() 晚于 input_context 就绪」三种晚到情况。
func refresh() -> void:
	_bind_client()
	_reload_channels()
	refresh_settings()


## 测试 / 外部注入：直接设通道表并重建槽位。
## 行里显式给 present / available 时以行为准（方便构造异常态）；
## 有 input_context 时以真实能力位为准。
func apply_channels(rows: Array) -> void:
	_channels = rows.duplicate(true)
	_channel_states.clear()
	for raw in _channels:
		if not (raw is Dictionary):
			continue
		var channel: Dictionary = raw
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
	_refresh()


## 测试 / 外部注入：单通道状态变化（真实链路走 channel_changed 信号）。
func set_channel_state(key: String, present: bool, available: bool) -> void:
	_channel_states[key] = [present, available]
	_refresh()


## 测试 / 外部注入：交互目标类型与可用性（真实链路走 interaction_target_changed）。
func set_interaction_target(target_kind: int, enabled: bool) -> void:
	_context["target_kind"] = target_kind
	_context["interact_enabled"] = enabled
	_refresh()


func interaction_context() -> Dictionary:
	return _context.duplicate()


## 从 UiState 重新读取按钮缩放 / 透明度 / 左手模式并重排。
func refresh_settings() -> void:
	if _cluster != null:
		_cluster.refresh_settings()
	_refresh()


## 当前显示的按钮（触屏方案下可见的）。
func buttons() -> Array:
	if _cluster == null:
		return []
	return _cluster.buttons()


func channel_table() -> Array:
	return _channels.duplicate(true)


func cluster() -> ActionCluster:
	return _cluster


func hints() -> ActionHints:
	return _hints


## 释放所有按钮的按下状态（菜单 / 方案切走 / 失焦 / 退出时调用）。
func release_all() -> void:
	if _cluster != null:
		_cluster.release_all()


## 拆除：释放动作、断信号、从注册表移除动作簇与提示。
## 不释放自身节点（宿主挂在 UiRoot 下，随它一起释放）。
func destroy() -> void:
	release_all()
	_disconnect_all()
	_destroy_widget(_cluster)
	_destroy_widget(_hints)
	_cluster = null
	_hints = null
	_channels.clear()
	_channel_states.clear()
	_context = {
		"target_kind": InputBindings.TARGET_NONE,
		"interact_enabled": false,
	}


# ───────────────────────── 构建 ─────────────────────────

func _build_widgets() -> void:
	if _cluster != null:
		return
	_cluster = ActionCluster.new()
	_cluster.name = "ActionCluster"
	_mount_widget(_cluster)
	_hints = ActionHints.new()
	_hints.name = "ActionHints"
	_mount_widget(_hints)


## 优先走 HudRegistry；没有注册表 / 注册失败时直接挂到 UiRoot 下，按钮仍可用。
func _mount_widget(widget: HudWidget) -> void:
	if widget == null:
		return
	if _registry != null and is_instance_valid(_registry):
		if _registry.register(widget):
			return
	if _ui_root != null and is_instance_valid(_ui_root):
		_ui_root.add_child(widget)
		widget.attach(_ui_root)
	elif not widget.is_inside_tree():
		add_child(widget)
	if _ui_root != null and is_instance_valid(_ui_root):
		widget.attach(_ui_root)
	widget.refresh_scheme(UiState.scheme)
	widget.apply_compact(UiState.compact_level)
	widget.refresh_menu()


func _destroy_widget(widget: HudWidget) -> void:
	if widget == null or not is_instance_valid(widget):
		return
	var removed := false
	if _registry != null and is_instance_valid(_registry):
		if _registry.get_widget(widget.widget_id) == widget:
			removed = _registry.unregister(widget.widget_id)
	if not removed:
		if widget.get_parent() != null:
			widget.get_parent().remove_child(widget)
		widget.queue_free()


func _reload_channels() -> void:
	var rows: Array = []
	if _input_context != null and _input_context.has_method("channels"):
		rows = _input_context.channels()
	apply_channels(rows)


# ───────────────────────── 刷新 ─────────────────────────

func _refresh() -> void:
	if _cluster == null:
		return
	var layout := InputBindings.ui_layout()
	var entries := InputBindings.resolve_touch(layout, _channels, _channel_states, _context)
	_cluster.apply_slots(entries)
	if _hints != null:
		_hints.set_hint(InputBindings.hint_text(UiState.scheme, _channels, _channel_states, _context))


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
	context.channel_changed.connect(_on_channel_changed)
	if context.has_signal("interaction_target_changed"):
		context.interaction_target_changed.connect(_on_interaction_target_changed)
	_sync_context_from_input()


## 把 context 当前的交互目标同步进 _context（去抖：只在变化时刷新）。
func _sync_context_from_input() -> void:
	if _input_context == null:
		return
	var target := InputBindings.TARGET_NONE
	var enabled := false
	if _input_context.has_method("interaction_target"):
		target = int(_input_context.interaction_target())
	if _input_context.has_method("interaction_enabled"):
		enabled = bool(_input_context.interaction_enabled())
	if int(_context.get("target_kind", 0)) == target and bool(_context.get("interact_enabled", false)) == enabled:
		return
	_context["target_kind"] = target
	_context["interact_enabled"] = enabled
	_refresh()


func _disconnect_input_context() -> void:
	if _input_context != null and is_instance_valid(_input_context):
		if _input_context.has_signal("channel_changed"):
			if _input_context.channel_changed.is_connected(_on_channel_changed):
				_input_context.channel_changed.disconnect(_on_channel_changed)
		if _input_context.has_signal("interaction_target_changed"):
			if _input_context.interaction_target_changed.is_connected(_on_interaction_target_changed):
				_input_context.interaction_target_changed.disconnect(_on_interaction_target_changed)
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
	# 立刻重新拉表重建槽位；不能直接 return，否则按钮永远补不回来。
	if not _channel_states.has(key):
		_reload_channels()
		if _channel_states.has(key):
			set_channel_state(key, present, available)
		return
	set_channel_state(key, present, available)


func _on_interaction_target_changed(target: int, enabled: bool) -> void:
	set_interaction_target(target, enabled)


func _on_menu_changed(open: bool) -> void:
	if open:
		release_all()


func _on_scheme_changed(current: int) -> void:
	if (current & UiState.Scheme.TOUCH) == 0:
		release_all()
	refresh_settings()


func _notification(what: int) -> void:
	if what == NOTIFICATION_WM_WINDOW_FOCUS_OUT:
		release_all()


func _exit_tree() -> void:
	release_all()
	_disconnect_all()
