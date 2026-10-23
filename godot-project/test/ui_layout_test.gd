extends SceneTree
## HUD 布局测试（headless，无编辑器 / 无真实窗口）。
##
## 用 SubViewport 精确控制视口尺寸（844x390 手机横屏 / 1280x800 平板 / 1920x1080 桌面），
## 验证 HudSlot 的「分配 footprint」、对齐、换行、预留带、inset 与 TouchRegionRegistry。
##
## 注意：Godot 的 Control.size 会被最小尺寸钳住，所以断言一律用
## HudSlot.layout_rects() 返回的**分配 footprint**，不读 child.size。
##
## 运行：
##   /tmp/Godot_v4.7.2-stable_linux.arm64 --headless --path godot-project \
##       --script res://test/ui_layout_test.gd

const EXPECTED_KEYS: Array[String] = [
	"top_begin", "top_center", "top_end",
	"mid_begin", "mid_center", "mid_end",
	"bottom_begin", "bottom_center", "bottom_end",
]
const PHONE := Vector2(844.0, 390.0)
const TABLET := Vector2(1280.0, 800.0)
const DESKTOP := Vector2(1920.0, 1080.0)
const EPS := 0.01

var _checks := 0
var _failures: PackedStringArray = PackedStringArray()
var _seq := 0


func _initialize() -> void:
	_run.call_deferred()


func _run() -> void:
	await process_frame
	_check(HudSlot.QUICKBAR_RESERVE == 72.0, "HudSlot.QUICKBAR_RESERVE = 72")
	await _test_case(PHONE, true)
	await _test_case(TABLET, false)
	await _test_case(DESKTOP, false)
	_test_touch_registry()
	await _test_hud_registry()
	_finish()


# ───────────────────────── 用例 ─────────────────────────

func _test_case(size: Vector2, detailed: bool) -> void:
	var tag := "%dx%d" % [int(size.x), int(size.y)]
	var case_data := await _make_case(size)
	var viewport: SubViewport = case_data["viewport"]
	var layer: Control = case_data["layer"]
	var slots: Dictionary = case_data["slots"]

	_check(layer.size.distance_to(size) < EPS,
		"%s：SubViewport 内层 Control 铺满视口（实际 %s）" % [tag, str(layer.size)])
	_check_standard_slots(tag, slots, size)

	var made := _populate(slots)
	await process_frame
	await process_frame

	_check_footprints(tag, slots, size)
	_check_center(tag, slots, size)
	_check_alignment(tag, slots)
	_check_reserve(tag, slots, size)

	if detailed:
		await _check_wrap(tag, slots)
		await _check_hidden(tag, slots, made)
		await _check_inset(tag, slots, size)
		await _check_oversized(tag, slots)

	viewport.queue_free()
	await process_frame


func _make_case(size: Vector2) -> Dictionary:
	var viewport := SubViewport.new()
	viewport.name = "LayoutViewport_%dx%d" % [int(size.x), int(size.y)]
	viewport.size = Vector2i(int(size.x), int(size.y))
	viewport.disable_3d = true
	root.add_child(viewport)

	var layer := Control.new()
	layer.name = "Layer"
	layer.mouse_filter = Control.MOUSE_FILTER_IGNORE
	viewport.add_child(layer)
	layer.set_anchors_and_offsets_preset(Control.PRESET_FULL_RECT)

	await process_frame
	await process_frame

	var slots := HudSlot.build_standard(layer)
	await process_frame
	await process_frame
	return {"viewport": viewport, "layer": layer, "slots": slots}


func _expected_specs() -> Dictionary:
	return {
		"top_begin": {"edge": HudSlot.Edge.TOP, "align": HudSlot.Align.BEGIN, "vertical": false, "reserve": Vector2.ZERO},
		"top_center": {"edge": HudSlot.Edge.TOP, "align": HudSlot.Align.CENTER, "vertical": false, "reserve": Vector2.ZERO},
		"top_end": {"edge": HudSlot.Edge.TOP, "align": HudSlot.Align.END, "vertical": false, "reserve": Vector2.ZERO},
		"mid_begin": {"edge": HudSlot.Edge.LEFT, "align": HudSlot.Align.CENTER, "vertical": true, "reserve": Vector2.ZERO},
		"mid_center": {"edge": HudSlot.Edge.CENTER, "align": HudSlot.Align.CENTER, "vertical": false, "reserve": Vector2.ZERO},
		"mid_end": {"edge": HudSlot.Edge.RIGHT, "align": HudSlot.Align.CENTER, "vertical": true, "reserve": Vector2.ZERO},
		"bottom_begin": {"edge": HudSlot.Edge.BOTTOM, "align": HudSlot.Align.BEGIN, "vertical": false, "reserve": Vector2.ZERO},
		"bottom_center": {"edge": HudSlot.Edge.BOTTOM, "align": HudSlot.Align.CENTER, "vertical": false, "reserve": Vector2(0.0, HudSlot.QUICKBAR_RESERVE)},
		"bottom_end": {"edge": HudSlot.Edge.BOTTOM, "align": HudSlot.Align.END, "vertical": false, "reserve": Vector2.ZERO},
	}


func _check_standard_slots(tag: String, slots: Dictionary, size: Vector2) -> void:
	var problems: PackedStringArray = PackedStringArray()
	if slots.size() != EXPECTED_KEYS.size():
		problems.append("槽位数量=%d" % slots.size())
	var specs := _expected_specs()
	for key in EXPECTED_KEYS:
		if not slots.has(key):
			problems.append("缺 %s" % key)
			continue
		var value: Variant = slots[key]
		if not (value is HudSlot):
			problems.append("%s 不是 HudSlot" % key)
			continue
		var slot: HudSlot = value
		var want: Dictionary = specs[key]
		if slot.name != StringName(key):
			problems.append("%s 名字=%s" % [key, String(slot.name)])
		if slot.edge != int(want["edge"]):
			problems.append("%s edge=%d" % [key, slot.edge])
		if slot.align != int(want["align"]):
			problems.append("%s align=%d" % [key, slot.align])
		if slot.vertical != bool(want["vertical"]):
			problems.append("%s vertical=%s" % [key, str(slot.vertical)])
		if not slot.reserve.is_equal_approx(want["reserve"] as Vector2):
			problems.append("%s reserve=%s" % [key, str(slot.reserve)])
		if slot.mouse_filter != Control.MOUSE_FILTER_IGNORE:
			problems.append("%s mouse_filter=%d" % [key, slot.mouse_filter])
		if slot.size.distance_to(size) > 1.0:
			problems.append("%s size=%s" % [key, str(slot.size)])
	_check(problems.is_empty(),
		"%s：9 槽齐全、铺满视口、属性符合契约（问题：%s）" % [tag, str(problems)])


func _populate(slots: Dictionary) -> Dictionary:
	var made: Dictionary = {}
	made["top_begin"] = [_add(slots, "top_begin", 96.0, 32.0), _add(slots, "top_begin", 64.0, 24.0, true)]
	made["top_center"] = [_add(slots, "top_center", 120.0, 28.0)]
	made["top_end"] = [_add(slots, "top_end", 64.0, 24.0), _add(slots, "top_end", 48.0, 24.0)]
	made["mid_begin"] = [_add(slots, "mid_begin", 80.0, 48.0), _add(slots, "mid_begin", 80.0, 48.0)]
	made["mid_center"] = [_add(slots, "mid_center", 24.0, 24.0)]
	made["mid_end"] = [
		_add(slots, "mid_end", 128.0, 96.0),
		_add(slots, "mid_end", 128.0, 96.0),
		_add(slots, "mid_end", 128.0, 96.0),
		_add(slots, "mid_end", 128.0, 96.0),
	]
	made["bottom_begin"] = [_add(slots, "bottom_begin", 72.0, 40.0), _add(slots, "bottom_begin", 72.0, 40.0)]
	made["bottom_center"] = [_add(slots, "bottom_center", 64.0, 64.0)]
	made["bottom_end"] = [_add(slots, "bottom_end", 56.0, 32.0)]
	return made


func _add(slots: Dictionary, key: String, width: float, height: float, as_widget: bool = false) -> Control:
	var slot: HudSlot = slots[key]
	_seq += 1
	var control: Control = HudWidget.new() if as_widget else Control.new()
	control.name = "%s_item_%d" % [key, _seq]
	control.custom_minimum_size = Vector2(width, height)
	control.mouse_filter = Control.MOUSE_FILTER_IGNORE
	slot.add_child(control)
	return control


# ───────────────────────── 断言 ─────────────────────────

func _check_footprints(tag: String, slots: Dictionary, size: Vector2) -> void:
	var total := 0
	var problems: PackedStringArray = PackedStringArray()
	var viewport_rect := Rect2(Vector2.ZERO, size)
	for key in EXPECTED_KEYS:
		var slot: HudSlot = slots[key]
		var content := slot.content_rect()
		var rects := slot.layout_rects()
		total += rects.size()
		for node in rects:
			var rect: Rect2 = rects[node]
			if not _inside(rect, content, 1.0):
				problems.append("%s/%s footprint=%s 超出内容区 %s" % [key, String(node.name), str(rect), str(content)])
			if not _inside(rect, viewport_rect, 1.0):
				problems.append("%s/%s footprint=%s 超出视口" % [key, String(node.name), str(rect)])
	_check(problems.is_empty() and total >= 16,
		"%s：%d 个 footprint 全部在内容区 / 视口内（问题：%s）" % [tag, total, str(problems)])


func _check_center(tag: String, slots: Dictionary, size: Vector2) -> void:
	var slot: HudSlot = slots["mid_center"]
	var rects := slot.layout_rects()
	var problems: PackedStringArray = PackedStringArray()
	if rects.size() != 1:
		problems.append("footprint 数=%d" % rects.size())
	else:
		var rect: Rect2 = rects.values()[0]
		if not rect.size.is_equal_approx(Vector2(24.0, 24.0)):
			problems.append("footprint=%s" % str(rect.size))
		if rect.get_center().distance_to(size * 0.5) >= EPS:
			problems.append("中心=%s，视口中心=%s" % [str(rect.get_center()), str(size * 0.5)])
	_check(problems.is_empty(),
		"%s：mid_center 的 24x24 元素中心 == 视口中心（问题：%s）" % [tag, str(problems)])


func _check_alignment(tag: String, slots: Dictionary) -> void:
	var problems: PackedStringArray = PackedStringArray()
	# 水平主轴 + END：一行里最后一个元素贴内容区右端。
	for key in ["top_end", "bottom_end"]:
		var slot: HudSlot = slots[key]
		var content := slot.content_rect()
		var values: Array = slot.layout_rects().values()
		if values.is_empty():
			problems.append("%s 没有 footprint" % key)
			continue
		var last: Rect2 = values[values.size() - 1]
		if absf(last.end.x - content.end.x) > EPS:
			problems.append("%s 末元素 end.x=%.2f，内容区右端 %.2f" % [key, last.end.x, content.end.x])
	# 水平主轴 + BEGIN：一行里第一个元素贴内容区左端。
	for key in ["top_begin", "bottom_begin"]:
		var slot: HudSlot = slots[key]
		var content := slot.content_rect()
		var values: Array = slot.layout_rects().values()
		if values.is_empty():
			problems.append("%s 没有 footprint" % key)
			continue
		var first: Rect2 = values[0]
		if absf(first.position.x - content.position.x) > EPS:
			problems.append("%s 首元素 x=%.2f，内容区左端 %.2f" % [key, first.position.x, content.position.x])
	# 竖直主轴 + RIGHT 边缘：最右一列贴内容区右端（交叉轴对齐）。
	var right_slot: HudSlot = slots["mid_end"]
	var right_content := right_slot.content_rect()
	var rightmost := -INF
	for node in right_slot.layout_rects():
		rightmost = maxf(rightmost, (right_slot.layout_rects()[node] as Rect2).end.x)
	if absf(rightmost - right_content.end.x) > EPS:
		problems.append("mid_end 最右列 end.x=%.2f，期望 %.2f" % [rightmost, right_content.end.x])
	# 竖直主轴 + LEFT 边缘：最左一列贴内容区左端。
	var left_slot: HudSlot = slots["mid_begin"]
	var left_content := left_slot.content_rect()
	var leftmost := INF
	for node in left_slot.layout_rects():
		leftmost = minf(leftmost, (left_slot.layout_rects()[node] as Rect2).position.x)
	if absf(leftmost - left_content.position.x) > EPS:
		problems.append("mid_begin 最左列 x=%.2f，期望 %.2f" % [leftmost, left_content.position.x])
	# 水平主轴 + CENTER：首个元素水平居中。
	var center_slot: HudSlot = slots["top_center"]
	var center_content := center_slot.content_rect()
	var center_values: Array = center_slot.layout_rects().values()
	if center_values.is_empty():
		problems.append("top_center 没有 footprint")
	elif absf((center_values[0] as Rect2).get_center().x - center_content.get_center().x) > EPS:
		problems.append("top_center 未水平居中")
	_check(problems.is_empty(),
		"%s：END 贴右 / BEGIN 贴左 / CENTER 居中（问题：%s）" % [tag, str(problems)])


func _check_reserve(tag: String, slots: Dictionary, size: Vector2) -> void:
	var slot: HudSlot = slots["bottom_center"]
	var content := slot.content_rect()
	var problems: PackedStringArray = PackedStringArray()
	if absf(content.end.y - (size.y - HudSlot.QUICKBAR_RESERVE)) > EPS:
		problems.append("content.end.y=%.2f，期望 %.2f" % [content.end.y, size.y - HudSlot.QUICKBAR_RESERVE])
	var rects := slot.layout_rects()
	if rects.is_empty():
		problems.append("没有 footprint")
	for node in rects:
		var rect: Rect2 = rects[node]
		if rect.end.y > size.y - HudSlot.QUICKBAR_RESERVE + 1.0:
			problems.append("%s 底边 %.2f 侵入快捷栏预留带" % [String(node.name), rect.end.y])
	_check(problems.is_empty(),
		"%s：bottom_center reserve=72 生效，子元素底边 <= 视口底-72（问题：%s）" % [tag, str(problems)])


func _check_wrap(tag: String, slots: Dictionary) -> void:
	var slot: HudSlot = slots["mid_end"]
	var rects := slot.layout_rects()
	var content := slot.content_rect()
	var columns: Array[float] = []
	var problems: PackedStringArray = PackedStringArray()
	for node in rects:
		var rect: Rect2 = rects[node]
		var seen := false
		for value in columns:
			if absf(value - rect.position.x) < EPS:
				seen = true
		if not seen:
			columns.append(rect.position.x)
		if not _inside(rect, content, 1.0):
			problems.append("%s 越界" % String(node.name))
	if rects.size() != 4:
		problems.append("footprint 数=%d" % rects.size())
	if columns.size() < 2:
		problems.append("没有换列（列数=%d）" % columns.size())
	_check(problems.is_empty(),
		"%s：mid_end 4 个 128x96 在 390 高时换列且不越界（列数=%d）" % [tag, columns.size()])


func _check_hidden(tag: String, slots: Dictionary, made: Dictionary) -> void:
	var before := _snapshot(slots)
	var children: Array = made["top_begin"]
	var victim: Control = children[children.size() - 1]
	victim.visible = false
	await process_frame
	var after := _snapshot(slots)
	var problems: PackedStringArray = PackedStringArray()
	for node in after:
		if not before.has(node):
			problems.append("%s 新出现" % String(node.name))
		elif not (after[node] as Rect2).is_equal_approx(before[node] as Rect2):
			problems.append("%s 位置变了 %s → %s" % [String(node.name), str(before[node]), str(after[node])])
	if after.has(victim):
		problems.append("隐藏元素仍在 layout_rects")
	_check(problems.is_empty(),
		"%s：隐藏 top_begin 末尾元素后其余 footprint 不变（问题：%s）" % [tag, str(problems)])
	victim.visible = true
	await process_frame


func _check_inset(tag: String, slots: Dictionary, size: Vector2) -> void:
	var slot: HudSlot = slots["mid_begin"]
	slot.set_inset(Vector4(40.0, 0.0, 20.0, 60.0))
	await process_frame
	var content := slot.content_rect()
	var want := Rect2(Vector2(40.0, 0.0), Vector2(size.x - 60.0, size.y - 60.0))
	var problems: PackedStringArray = PackedStringArray()
	if not content.is_equal_approx(want):
		problems.append("content_rect=%s，期望 %s" % [str(content), str(want)])
	if not slot.get_inset().is_equal_approx(Vector4(40.0, 0.0, 20.0, 60.0)):
		problems.append("get_inset()=%s" % str(slot.get_inset()))
	var rects := slot.layout_rects()
	if rects.is_empty():
		problems.append("没有 footprint")
	for node in rects:
		if not _inside(rects[node], content, EPS):
			problems.append("%s 侵入避让区" % String(node.name))
	_check(problems.is_empty(),
		"%s：inset=(40,0,20,60) 内容区 %s，子元素都在避让区外（问题：%s）" % [tag, str(content), str(problems)])
	slot.set_inset(Vector4.ZERO)
	await process_frame


func _check_oversized(tag: String, slots: Dictionary) -> void:
	var slot: HudSlot = slots["top_begin"]
	var giant := _add(slots, "top_begin", 900.0, 200.0)
	var tail := _add(slots, "top_begin", 40.0, 32.0)
	await process_frame
	var content := slot.content_rect()
	var rects := slot.layout_rects()
	var problems: PackedStringArray = PackedStringArray()
	if not rects.has(giant):
		problems.append("超尺寸元素没有 footprint")
	else:
		var giant_rect: Rect2 = rects[giant]
		if giant_rect.size.x > content.size.x + 1.0:
			problems.append("footprint 宽 %.1f 超出可用 %.1f" % [giant_rect.size.x, content.size.x])
		if not _inside(giant_rect, content, 1.0):
			problems.append("footprint 越界 %s" % str(giant_rect))
	if slot.debug_warnings.is_empty():
		problems.append("debug_warnings 为空")
	if not rects.has(tail) or not _inside(rects[tail], content, 1.0):
		problems.append("后续元素被推出内容区")
	_check(problems.is_empty(),
		"%s：超尺寸元素 footprint 夹到内容区、后续元素不被推出（问题：%s；warnings=%s）" % [tag, str(problems), str(slot.debug_warnings)])
	giant.queue_free()
	tail.queue_free()
	await process_frame


# ───────────────────────── TouchRegionRegistry ─────────────────────────

func _test_touch_registry() -> void:
	var registry := TouchRegionRegistry.new()
	registry.register(&"joystick", Rect2(24.0, 270.0, 140.0, 140.0), {"bottom_begin": Vector4(180.0, 0.0, 0.0, 0.0)})
	registry.register(&"look_area", Rect2(380.0, 0.0, 464.0, 390.0), {"mid_end": Vector4(0.0, 0.0, 0.0, 40.0)})

	var insets := registry.slot_insets()
	_check(insets.size() == 2 and insets.has("bottom_begin") and insets.has("mid_end"),
		"TouchRegion：slot_insets 只含给定槽位（实际 %s）" % str(insets))
	_check((insets.get("bottom_begin", Vector4.ZERO) as Vector4).is_equal_approx(Vector4(180.0, 0.0, 0.0, 0.0)),
		"TouchRegion：摇杆 reserve 的 bottom_begin left=180")
	_check((insets.get("mid_end", Vector4.ZERO) as Vector4).is_equal_approx(Vector4(0.0, 0.0, 0.0, 40.0)),
		"TouchRegion：视角区 reserve 的 mid_end bottom=40")

	registry.register(&"pause", Rect2(760.0, 12.0, 56.0, 56.0), {"bottom_begin": Vector4(40.0, 0.0, 90.0, 20.0)})
	var merged: Vector4 = registry.slot_insets()["bottom_begin"]
	_check(merged.is_equal_approx(Vector4(180.0, 0.0, 90.0, 20.0)),
		"TouchRegion：同槽位多区域每边取 max（实际 %s）" % str(merged))

	registry.update(&"joystick", Rect2(24.0, 270.0, 140.0, 140.0), {"bottom_begin": Vector4(120.0, 0.0, 0.0, 0.0)})
	merged = registry.slot_insets()["bottom_begin"]
	_check(merged.is_equal_approx(Vector4(120.0, 0.0, 90.0, 20.0)),
		"TouchRegion：update 覆盖旧 reserve（实际 %s）" % str(merged))

	registry.release(&"pause")
	merged = registry.slot_insets()["bottom_begin"]
	_check(merged.is_equal_approx(Vector4(120.0, 0.0, 0.0, 0.0)),
		"TouchRegion：release 后不再参与 max（实际 %s）" % str(merged))

	var snapshot := registry.regions()
	_check(snapshot.size() == 2, "TouchRegion：regions() 数量 = 2（实际 %d）" % snapshot.size())
	snapshot.erase(&"joystick")
	_check(registry.regions().size() == 2, "TouchRegion：regions() 返回副本，外部修改不影响内部")

	var none := registry.overlapping([[&"joystick", &"look_area"]])
	_check(none.is_empty(), "TouchRegion：不相交区域诊断为空（实际 %s）" % str(none))
	registry.register(&"wide", Rect2(0.0, 0.0, 500.0, 390.0))
	var pairs := registry.overlapping([&"joystick", &"look_area", &"wide"])
	_check(pairs.size() == 2, "TouchRegion：相交诊断报告 2 对重叠（实际 %s）" % str(pairs))

	registry.clear()
	_check(registry.regions().is_empty() and registry.slot_insets().is_empty(),
		"TouchRegion：clear 清空区域与 inset")


# ───────────────────────── HudRegistry 接入 ─────────────────────────

func _test_hud_registry() -> void:
	var case_data := await _make_case(PHONE)
	var viewport: SubViewport = case_data["viewport"]
	var slots: Dictionary = case_data["slots"]

	var registry := HudRegistry.new()
	registry.setup(null, slots)
	var low := _registry_widget(&"reg_low", HudWidget.Tray.TOP, HudWidget.Align.BEGIN, 0, Vector2(64.0, 32.0))
	var high := _registry_widget(&"reg_high", HudWidget.Tray.TOP, HudWidget.Align.BEGIN, 10, Vector2(64.0, 32.0))
	var middle := _registry_widget(&"reg_mid", HudWidget.Tray.TOP, HudWidget.Align.BEGIN, 5, Vector2(64.0, 32.0))
	_check(registry.register(low) and registry.register(high) and registry.register(middle),
		"HudRegistry：HudSlot 槽位注册 3 个元素成功")

	var top_begin: HudSlot = slots["top_begin"]
	await process_frame
	var ids: Array = []
	for i in top_begin.get_child_count():
		var child := top_begin.get_child(i)
		if child is HudWidget:
			ids.append((child as HudWidget).widget_id)
	_check(ids == [&"reg_high", &"reg_mid", &"reg_low"],
		"HudRegistry：BEGIN 槽按 priority 降序插入（实际 %s）" % str(ids))
	_check(top_begin.layout_rects().size() == 3,
		"HudRegistry：3 个元素都拿到 footprint（实际 %d）" % top_begin.layout_rects().size())

	# refresh_menu 的 mark_dirty：直接改最小尺寸后槽位必须重排（子元素不会通知父槽位）。
	high.custom_minimum_size = Vector2(120.0, 32.0)
	registry.refresh_menu()
	var after := top_begin.layout_rects()
	var width := -1.0
	if after.has(high):
		width = (after[high] as Rect2).size.x
	_check(absf(width - 120.0) < EPS,
		"HudRegistry：refresh_menu 后槽位 mark_dirty 重排（footprint 宽 %.1f）" % width)

	_check(registry.unregister(&"reg_low"), "HudRegistry：unregister 成功")
	await process_frame
	_check(top_begin.get_child_count() == 2 and top_begin.layout_rects().size() == 2,
		"HudRegistry：unregister 后槽位只剩 2 个元素")

	# 向后兼容：旧 BoxContainer 槽位仍可注册（_insert 不再要求槽位是 BoxContainer）。
	var compat_registry := HudRegistry.new()
	var box := HBoxContainer.new()
	box.name = "CompatBox"
	(case_data["layer"] as Control).add_child(box)
	compat_registry.setup(null, {"top_begin": box})
	_check(compat_registry.register(_registry_widget(&"compat", HudWidget.Tray.TOP, HudWidget.Align.BEGIN, 0, Vector2(32.0, 32.0))),
		"HudRegistry：旧 BoxContainer 槽位仍可注册（向后兼容）")

	viewport.queue_free()
	await process_frame


func _registry_widget(id: StringName, tray: int, align: int, priority: int, min_size: Vector2) -> HudWidget:
	var widget := HudWidget.new()
	widget.name = String(id)
	widget.widget_id = id
	widget.tray = tray
	widget.align = align
	widget.priority = priority
	widget.custom_minimum_size = min_size
	return widget


# ───────────────────────── 工具 ─────────────────────────

func _snapshot(slots: Dictionary) -> Dictionary:
	var out: Dictionary = {}
	for key in EXPECTED_KEYS:
		var slot: HudSlot = slots[key]
		var rects := slot.layout_rects()
		for node in rects:
			out[node] = rects[node]
	return out


func _inside(inner: Rect2, outer: Rect2, tolerance: float) -> bool:
	return inner.position.x >= outer.position.x - tolerance \
		and inner.position.y >= outer.position.y - tolerance \
		and inner.end.x <= outer.end.x + tolerance \
		and inner.end.y <= outer.end.y + tolerance


func _check(condition: bool, message: String) -> void:
	_checks += 1
	if condition:
		print("[ui_layout] PASS  " + message)
	else:
		_failures.append(message)
		print("[ui_layout] FAIL  " + message)


func _finish() -> void:
	print("[ui_layout] checks=%d failures=%d" % [_checks, _failures.size()])
	if _failures.is_empty():
		print("UI_LAYOUT PASS")
		quit(0)
	else:
		for failure in _failures:
			print("  - " + failure)
		print("UI_LAYOUT FAIL")
		quit(1)
