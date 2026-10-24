class_name ActionCluster
extends HudWidget
## 触屏动作簇：一个 HudWidget 落 bottom_end 槽位，内部弧形排布若干 ActionButton。
##
## 为什么是一个簇而不是每通道一个 HudWidget：
## - 动作按钮数量随能力动态变化（interact / glide / flight 时有时无），每个按钮单独
##   进托盘会让主按钮随邻居增减而位移；簇把按钮收进自己的局部坐标，主按钮钉在簇的
##   右下角（右手）/ 左下角（左手），簇右/下边又被 bottom_end 槽位钉住，所以主按钮
##   在屏幕上的位置不跳。
## - 簇只占一个 HUD 槽位 / 一个注册项，托盘与避让逻辑都简单。
##
## 布局：主按钮钉在簇的右下（左手：左下）角；其余按钮绕主按钮按极坐标角度
## [180,215,250,285,320] 度、半径≈(main+sub)/2*1.05 排布；簇最小尺寸由所有可见
## 按钮的包围盒 + PADDING 计算。
##
## UiRoot 有 update_touch_region() 时，簇在 resized / visibility_changed 时登记
## （隐藏时释放）自己的全局矩形，让 HUD 槽位避让触控区域。

const WIDGET_ID := &"action_cluster"
## 簇内边距（设计单位）。
const PADDING := 12.0
## 弧形角度的「规格值」（Godot 屏幕坐标：0 = 右、90 = 下、180 = 左、270 = 上）。
## 有效角度会把 [180,320] 映射到锚点远离屏幕角的那 90° 扇区（右手：左 -> 上），
## 保证按钮永远在锚点内侧，主按钮才能钉死在簇的角上而不被其它按钮挤动。
const ARC_ANGLES := [180.0, 215.0, 250.0, 285.0, 320.0]
## 规格角度的起止（用于线性映射到可用扇区）。
const ARC_SPEC_START := 180.0
const ARC_SPEC_END := 320.0
## 可用扇区宽度（度）：右手 180°(左) -> 270°(上)；左手镜像成 0°(右) -> 90°(上)。
const ARC_USABLE_SWEEP := 90.0
## 弧半径系数：radius = (anchor_size + size) / 2 * ARC_RADIUS_FACTOR。
const ARC_RADIUS_FACTOR := 1.05
## 相邻按钮之间的最小空隙（设计单位）；排布保证任意两个按钮边缘间距 >= 此值。
const BUTTON_GAP := 8.0
## 放不下时半径最多放大到的倍数。
const ARC_MAX_RADIUS_SCALE := 2.0
## 主按钮 role（CONTEXT 布局）；SPLIT 布局没有 primary，退化为第一个可见按钮。
const ANCHOR_ROLE := "primary"
const TOUCH_REGION_NAME := &"action_cluster"

## role -> 最新解析结果（resolve_touch 的一条）。
var _entries: Array = []
## 稳定 role 顺序（apply_slots 的输入顺序）。
var _order: Array = []
## role -> ActionButton（创建后复用，隐藏不销毁）。
var _buttons: Dictionary = {}
## role -> Rect2（簇本地坐标，测试 / 调试用，只含可见按钮）。
var _rects: Dictionary = {}
var _button_scale := 1.0
var _opacity := 0.75
var _left_handed := false
var _updating := false


func _init() -> void:
	widget_id = WIDGET_ID
	tray = HudWidget.Tray.BOTTOM
	align = HudWidget.Align.END
	priority = 10
	schemes = UiState.Scheme.TOUCH
	mouse_filter = Control.MOUSE_FILTER_IGNORE
	custom_minimum_size = Vector2.ZERO


func _ready() -> void:
	if not visibility_changed.is_connected(_on_visibility_changed):
		visibility_changed.connect(_on_visibility_changed)
	refresh_layout()


# ───────────────────────── 对外 API ─────────────────────────

## 应用解析后的槽位（InputBindings.resolve_touch 的产物）。按 role 创建 / 复用按钮，
## 更新 label / action / available / visible；只给可见按钮排位置，不可见的 hide + release。
func apply_slots(entries: Array) -> void:
	_entries = entries.duplicate(true)
	_order.clear()
	for raw in _entries:
		if not (raw is Dictionary):
			continue
		var role := String((raw as Dictionary).get("role", ""))
		if role.is_empty():
			continue
		_order.append(role)
		var entry: Dictionary = raw
		var visible := bool(entry.get("visible", false))
		var available := bool(entry.get("available", false))
		var button := _ensure_button(role)
		if button == null:
			continue
		button.set_action(String(entry.get("action", "")))
		button.set_label(String(entry.get("label", "")))
		button.set_opacity(_opacity)
		button.set_available(available)
		button.set_channel_state(visible, available)
		button.visible = visible
		if not visible:
			button.release()
	refresh_layout()


## 当前可见按钮（稳定 role 顺序）。
func buttons() -> Array:
	var out: Array = []
	for role in _order:
		var button: ActionButton = _buttons.get(role)
		if button != null and is_instance_valid(button) and button.visible:
			out.append(button)
	return out


## 按 role 取按钮（含隐藏的，测试 / 调试用）；没有返回 null。
func button_for(role: String) -> ActionButton:
	var button: Variant = _buttons.get(role)
	if button is ActionButton and is_instance_valid(button):
		return button
	return null


## role -> Rect2（簇本地坐标，只含可见按钮）。
func slot_rects() -> Dictionary:
	return _rects.duplicate()


## 按钮缩放（像素尺寸由 InputBindings.ui_button_units 统一换算，这里只触发重排）。
func set_button_scale(value: float) -> void:
	_button_scale = maxf(0.05, value)
	refresh_layout()


func button_scale() -> float:
	return _button_scale


func set_opacity(value: float) -> void:
	_opacity = clampf(value, 0.05, 1.0)
	for role in _buttons:
		var button: ActionButton = _buttons.get(role)
		if button != null and is_instance_valid(button):
			button.set_opacity(_opacity)


func opacity() -> float:
	return _opacity


## 左手模式：簇改落 bottom_begin 槽位并水平镜像（主按钮到左下角）。
func set_left_handed(value: bool) -> void:
	var changed := _left_handed != value
	_left_handed = value
	align = HudWidget.Align.BEGIN if value else HudWidget.Align.END
	if changed:
		_relocate_in_root()
	refresh_layout()


func left_handed() -> bool:
	return _left_handed


## 从 UiState 重新读取按钮缩放 / 透明度 / 左手模式（设置变化时由宿主调用）。
func refresh_settings() -> void:
	_button_scale = InputBindings.ui_button_scale()
	_opacity = InputBindings.ui_button_opacity()
	var handed := InputBindings.ui_left_handed()
	var changed := _left_handed != handed
	_left_handed = handed
	align = HudWidget.Align.BEGIN if handed else HudWidget.Align.END
	if changed:
		_relocate_in_root()
	refresh_layout()


## 释放所有按钮（菜单 / 方案切走 / 失焦 / 退出时调用）。
func release_all() -> void:
	for role in _buttons:
		var button: ActionButton = _buttons.get(role)
		if button != null and is_instance_valid(button):
			button.release()


# ───────────────────────── 布局 ─────────────────────────

## 按当前可见按钮重算尺寸 / 位置。主按钮钉在簇右下（左手左下）角。
func refresh_layout() -> void:
	if _updating:
		return
	_updating = true
	# 1) 每个可见按钮的目标边长。
	var sizes: Dictionary = {}
	for role in _order:
		var entry := _entry_for(role)
		if entry.is_empty() or not bool(entry.get("visible", false)):
			continue
		var button := _ensure_button(role)
		if button == null:
			continue
		var px := InputBindings.ui_button_units(String(entry.get("size_role", "sub")))
		button.apply_size(px)
		button.set_opacity(_opacity)
		sizes[role] = px
	# 2) 可见 role 列表（保持稳定顺序）。
	var visible_roles: Array = []
	for role in _order:
		if sizes.has(role):
			visible_roles.append(role)
	_rects.clear()
	if visible_roles.is_empty():
		custom_minimum_size = Vector2.ZERO
		size = Vector2.ZERO
		_updating = false
		_sync_touch_region()
		_notify_parent_layout()
		return
	# 3) 锚点（主按钮）钉在局部原点居中；其余按钮按极坐标展开。
	var anchor_role: String = _anchor_role(visible_roles)
	var anchor_size: float = sizes[anchor_role]
	var anchor_half := anchor_size * 0.5
	var rects: Dictionary = {}
	rects[anchor_role] = Rect2(-anchor_half, -anchor_half, anchor_size, anchor_size)
	var others: Array = []
	for role in visible_roles:
		if role != anchor_role:
			others.append({"role": role, "size": float(sizes[role])})
	var centers := _arc_centers(anchor_size, others)
	for role in centers:
		var btn_size: float = sizes[role]
		var center: Vector2 = centers[role]
		if _left_handed:
			center.x = -center.x
		rects[role] = Rect2(center - Vector2(btn_size, btn_size) * 0.5, Vector2(btn_size, btn_size))
	# 4) 包围盒 + 内边距 = 簇最小尺寸。
	var bbox: Rect2 = rects[anchor_role]
	for role in rects:
		bbox = bbox.merge(rects[role])
	var cluster_size := Vector2(ceilf(bbox.size.x + PADDING * 2.0), ceilf(bbox.size.y + PADDING * 2.0))
	# 5) 平移：右手把锚点右下角钉在簇右下角（左手：左下角钉在簇左下角）。
	var anchor_rect: Rect2 = rects[anchor_role]
	var shift := Vector2.ZERO
	if _left_handed:
		shift.x = PADDING - anchor_rect.position.x
	else:
		shift.x = cluster_size.x - PADDING - anchor_rect.end.x
	shift.y = cluster_size.y - PADDING - anchor_rect.end.y
	for role in rects:
		var rect: Rect2 = rects[role]
		rect.position += shift
		_rects[role] = rect
		var button: ActionButton = _buttons.get(role)
		if button != null and is_instance_valid(button):
			button.position = rect.position
			button.size = rect.size
	custom_minimum_size = cluster_size
	size = cluster_size
	_updating = false
	_sync_touch_region()
	_notify_parent_layout()


## 弧形排布（防重叠）：按顺序沿弧放置，每个按钮取「与上一个（或主按钮）中心距 >=
## 半径之和 + BUTTON_GAP」的最小角度；扇区（180°~270°）放不下就整体放大半径重试。
## 返回 role -> 中心点（以主按钮中心为原点，右手坐标；左手由调用方镜像 x）。
func _arc_centers(anchor_size: float, others: Array) -> Dictionary:
	var result: Dictionary = {}
	if others.is_empty():
		return result
	var scale := 1.0
	while scale <= ARC_MAX_RADIUS_SCALE + 0.001:
		result = {}
		var ok := true
		var prev_center := Vector2.ZERO
		var prev_size := anchor_size
		var angle := ARC_SPEC_START
		for i in others.size():
			var item: Dictionary = others[i]
			var btn_size: float = item["size"]
			var radius := ((anchor_size + btn_size) * 0.5 + BUTTON_GAP) * scale
			var need := (btn_size + prev_size) * 0.5 + BUTTON_GAP
			var center := Vector2.ZERO
			var found := false
			var a := angle
			while a <= ARC_SPEC_START + ARC_USABLE_SWEEP + 0.001:
				var r := deg_to_rad(a)
				center = Vector2(cos(r), sin(r)) * radius
				if i == 0 or center.distance_to(prev_center) >= need:
					found = true
					angle = a
					break
				a += 0.5
			if not found:
				ok = false
				break
			result[String(item["role"])] = center
			prev_center = center
			prev_size = btn_size
		if ok:
			return result
		scale += 0.1
	# 兜底：放不下时按最大半径均分扇区（仍尽量分开）。
	result = {}
	var count := others.size()
	for i in count:
		var item: Dictionary = others[i]
		var btn_size: float = item["size"]
		var radius := ((anchor_size + btn_size) * 0.5 + BUTTON_GAP) * ARC_MAX_RADIUS_SCALE
		var t := 0.0 if count == 1 else float(i) / float(count - 1)
		var r := deg_to_rad(ARC_SPEC_START + t * ARC_USABLE_SWEEP)
		result[String(item["role"])] = Vector2(cos(r), sin(r)) * radius
	return result


## 规格角度 -> 可用扇区内的有效角度（右手：左 -> 上；左手再做水平镜像）。
func _arc_angle(index: int) -> float:
	var spec: float = ARC_ANGLES[index % ARC_ANGLES.size()]
	var t := clampf((spec - ARC_SPEC_START) / (ARC_SPEC_END - ARC_SPEC_START), 0.0, 1.0)
	return ARC_SPEC_START + t * ARC_USABLE_SWEEP


func _anchor_role(visible_roles: Array) -> String:
	for role in visible_roles:
		if String(role) == ANCHOR_ROLE:
			return ANCHOR_ROLE
	return String(visible_roles[0])


func _entry_for(role: String) -> Dictionary:
	for raw in _entries:
		if raw is Dictionary and String((raw as Dictionary).get("role", "")) == role:
			return raw
	return {}


func _ensure_button(role: String) -> ActionButton:
	var existing: Variant = _buttons.get(role)
	if existing is ActionButton and is_instance_valid(existing):
		return existing
	var button := ActionButton.new()
	button.name = "btn_" + role
	button.widget_id = StringName("btn_" + role)
	add_child(button)
	_buttons[role] = button
	return button


# ───────────────────────── UiRoot 接线 ─────────────────────────

func _on_attach() -> void:
	if not visibility_changed.is_connected(_on_visibility_changed):
		visibility_changed.connect(_on_visibility_changed)
	refresh_settings()


func _on_detach() -> void:
	release_all()
	_release_touch_region()
	ui_root = null


func _on_scheme_changed(_current: int) -> void:
	if not UiState.has_scheme(UiState.Scheme.TOUCH):
		release_all()
	refresh_settings()


func _on_visibility_changed() -> void:
	if not is_visible_in_tree():
		release_all()
	_sync_touch_region()


func _notification(what: int) -> void:
	if what == NOTIFICATION_RESIZED:
		if not _updating:
			_sync_touch_region()
			_notify_parent_layout()
	elif what == NOTIFICATION_WM_WINDOW_FOCUS_OUT:
		release_all()


func _exit_tree() -> void:
	release_all()
	_release_touch_region()


func _relocate_in_root() -> void:
	if ui_root == null or not is_instance_valid(ui_root):
		return
	var registry: Variant = ui_root.get("registry")
	if registry != null and registry.has_method("relocate"):
		registry.relocate(self, UiState.scheme)


func _notify_parent_layout() -> void:
	update_minimum_size()
	var parent := get_parent()
	if parent != null and parent.has_method("mark_dirty"):
		parent.mark_dirty()


func _sync_touch_region() -> void:
	if ui_root == null or not is_instance_valid(ui_root):
		return
	if not ui_root.has_method("update_touch_region"):
		return
	var visible_now := is_visible_in_tree() and size.x > 0.5 and size.y > 0.5
	if not visible_now:
		_release_touch_region()
		return
	var rect := get_global_rect()
	var view_w := get_viewport_rect().size.x
	# 簇占住屏幕一侧：让中间槽位（快捷栏等）从簇旁边让开，不再压在按钮下面。
	var reserves := {}
	if _left_handed:
		reserves["bottom_center"] = Vector4(rect.end.x + PADDING, 0.0, 0.0, 0.0)
	else:
		reserves["bottom_center"] = Vector4(0.0, 0.0, maxf(0.0, view_w - rect.position.x) + PADDING, 0.0)
	ui_root.update_touch_region(TOUCH_REGION_NAME, rect, reserves)


func _release_touch_region() -> void:
	if ui_root == null or not is_instance_valid(ui_root):
		return
	if ui_root.has_method("release_touch_region"):
		ui_root.release_touch_region(TOUCH_REGION_NAME)
	elif ui_root.has_method("update_touch_region"):
		ui_root.update_touch_region(TOUCH_REGION_NAME, Rect2(), {})
