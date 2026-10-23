class_name HudSlot
extends Control
## 自绘 HUD 槽位：一个铺满父级的透明 Control，按「边缘 + 对齐」摆放子元素。
##
## 设计要点：
## - 不用 Container：HudWidget 的 priority 排序由 HudRegistry 通过插入顺序表达，
##   本槽只认 child index；Container 还会把子元素最小尺寸求和进自身最小尺寸，
##   把整条托盘撑大。这里最小尺寸只取 reserve（边缘预留带）。
## - 分配 footprint 与 Control.size 可能不同：Godot 的 Control.size 会被最小尺寸钳住
##   （实测 set_size(50, 50) 但 min 200 仍是 200）。layout_rects() 返回的是
##   **分配 footprint**，也是测试与文档的依据；position / size 仍然都设置到子元素上。
## - debug_warnings 记录「引擎拒绝缩小」「wrap=false 截断」「交叉轴溢出」等诊断。
##
## 坐标系：子元素 position / footprint 都是本槽内坐标（槽铺满父级，通常是视口或 SafeArea）。

enum Edge { TOP, BOTTOM, LEFT, RIGHT, CENTER }
enum Align { BEGIN, CENTER, END }

## 快捷栏预留带高度（设计单位）：bottom_center 靠它给快捷栏让位。
const QUICKBAR_RESERVE := 72.0
## 浮点比较容差（设计单位）。
const LAYOUT_EPSILON := 0.001

## 槽位贴的边：决定内容区锚点、交叉轴对齐与换行/换列方向。
@export var edge: Edge = Edge.TOP:
	set(value):
		edge = value
		_property_changed(true)

## 主轴对齐：BEGIN 从头排、END 贴端、CENTER 整体居中。
@export var align: Align = Align.BEGIN:
	set(value):
		align = value
		_property_changed(false)

## 主轴是否竖直（true = 竖排 / 换列，false = 横排 / 换行）。
@export var vertical: bool = false:
	set(value):
		vertical = value
		_property_changed(false)

## 边缘预留带：TOP/BOTTOM 用 y，LEFT/RIGHT 用 x；CENTER 仅作最小尺寸。
@export var reserve: Vector2 = Vector2.ZERO:
	set(value):
		reserve = value
		_property_changed(true)

## 内容避让：(left, top, right, bottom)，设计单位。
@export var inset: Vector4 = Vector4.ZERO:
	set(value):
		inset = value
		_property_changed(false)

## 子元素间距（主轴与交叉轴都使用）。
@export var spacing: float = 8.0:
	set(value):
		spacing = maxf(0.0, value)
		_property_changed(false)

## 主轴放不下时是否换行/换列；false 时截断到可用空间。
@export var wrap: bool = true:
	set(value):
		wrap = value
		_property_changed(false)

## 最近一次布局的告警（只读，测试用）。
var debug_warnings: PackedStringArray = PackedStringArray()

var _layout_rects: Dictionary = {}
var _dirty := false
var _relayout_queued := false


func _ready() -> void:
	mouse_filter = Control.MOUSE_FILTER_IGNORE
	if not resized.is_connected(mark_dirty):
		resized.connect(mark_dirty)
	if not child_entered_tree.is_connected(_on_child_entered_tree):
		child_entered_tree.connect(_on_child_entered_tree)
	if not child_exiting_tree.is_connected(_on_child_exiting_tree):
		child_exiting_tree.connect(_on_child_exiting_tree)
	for child in get_children():
		_connect_child_visibility(child)
	mark_dirty()


# ───────────────────────── 最小尺寸 ─────────────────────────

## 最小尺寸只取 reserve，绝不取子元素之和（否则托盘会被 HUD 元素撑大）。
func _get_minimum_size() -> Vector2:
	match edge:
		Edge.TOP, Edge.BOTTOM:
			return Vector2(0.0, maxf(0.0, reserve.y))
		Edge.LEFT, Edge.RIGHT:
			return Vector2(maxf(0.0, reserve.x), 0.0)
		_:
			return Vector2(maxf(0.0, reserve.x), maxf(0.0, reserve.y))


# ───────────────────────── 对外接口 ─────────────────────────

func set_inset(value: Vector4) -> void:
	inset = value


func get_inset() -> Vector4:
	return inset


## 调度一次延迟重排（只在 idle 排一次；测试可随后直接调 layout_rects()）。
func mark_dirty() -> void:
	_dirty = true
	if _relayout_queued:
		return
	_relayout_queued = true
	_relayout_deferred.call_deferred()


## 内容区：去掉 inset 与边缘预留带后的可用矩形（本槽内坐标）。
func content_rect() -> Rect2:
	var left := maxf(0.0, inset.x)
	var top := maxf(0.0, inset.y)
	var right := maxf(0.0, inset.z)
	var bottom := maxf(0.0, inset.w)
	var position := Vector2(left, top)
	var extent := Vector2(
		maxf(0.0, size.x - left - right),
		maxf(0.0, size.y - top - bottom)
	)
	match edge:
		Edge.TOP:
			var band_top := maxf(0.0, reserve.y)
			position.y += band_top
			extent.y = maxf(0.0, extent.y - band_top)
		Edge.BOTTOM:
			extent.y = maxf(0.0, extent.y - maxf(0.0, reserve.y))
		Edge.LEFT:
			var band_left := maxf(0.0, reserve.x)
			position.x += band_left
			extent.x = maxf(0.0, extent.x - band_left)
		Edge.RIGHT:
			extent.x = maxf(0.0, extent.x - maxf(0.0, reserve.x))
		_:
			pass
	return Rect2(position, extent)


## Node -> Rect2（本槽内坐标，最近一次布局分配的 footprint）。
## 只包含**可见**子节点：隐藏元素不占位（visibility_changed 会触发重排）。
func layout_rects() -> Dictionary:
	if _dirty and is_inside_tree():
		_relayout()
	return _layout_rects.duplicate()


# ───────────────────────── 构建标准槽位 ─────────────────────────

## 生成 9 个铺满 parent 的槽位，返回 key(String) -> HudSlot。
## parent 通常是 UiRoot 的 UiTree 或测试里的 SubViewport 内层 Control。
static func build_standard(parent: Control) -> Dictionary:
	var result: Dictionary = {}
	if parent == null:
		return result
	result["top_begin"] = _make_slot(parent, "top_begin", Edge.TOP, Align.BEGIN, false, Vector2.ZERO)
	result["top_center"] = _make_slot(parent, "top_center", Edge.TOP, Align.CENTER, false, Vector2.ZERO)
	result["top_end"] = _make_slot(parent, "top_end", Edge.TOP, Align.END, false, Vector2.ZERO)
	result["mid_begin"] = _make_slot(parent, "mid_begin", Edge.LEFT, Align.CENTER, true, Vector2.ZERO)
	result["mid_center"] = _make_slot(parent, "mid_center", Edge.CENTER, Align.CENTER, false, Vector2.ZERO)
	result["mid_end"] = _make_slot(parent, "mid_end", Edge.RIGHT, Align.CENTER, true, Vector2.ZERO)
	result["bottom_begin"] = _make_slot(parent, "bottom_begin", Edge.BOTTOM, Align.BEGIN, false, Vector2.ZERO)
	result["bottom_center"] = _make_slot(parent, "bottom_center", Edge.BOTTOM, Align.CENTER, false, Vector2(0.0, QUICKBAR_RESERVE))
	result["bottom_end"] = _make_slot(parent, "bottom_end", Edge.BOTTOM, Align.END, false, Vector2.ZERO)
	return result


static func _make_slot(parent: Control, key: String, edge_value: Edge, align_value: Align, vertical_value: bool, reserve_value: Vector2) -> HudSlot:
	var slot := HudSlot.new()
	slot.name = key
	slot.edge = edge_value
	slot.align = align_value
	slot.vertical = vertical_value
	slot.reserve = reserve_value
	slot.mouse_filter = Control.MOUSE_FILTER_IGNORE
	parent.add_child(slot)
	slot.set_anchors_and_offsets_preset(Control.PRESET_FULL_RECT)
	return slot


# ───────────────────────── 信号 ─────────────────────────

func _on_child_entered_tree(node: Node) -> void:
	_connect_child_visibility(node)
	mark_dirty()


func _on_child_exiting_tree(_node: Node) -> void:
	mark_dirty()


func _connect_child_visibility(node: Node) -> void:
	var control := node as Control
	if control == null:
		return
	if not control.visibility_changed.is_connected(mark_dirty):
		control.visibility_changed.connect(mark_dirty)


func _property_changed(min_size_changed: bool) -> void:
	if min_size_changed:
		update_minimum_size()
	mark_dirty()


# ───────────────────────── 布局 ─────────────────────────

func _relayout_deferred() -> void:
	_relayout_queued = false
	if not is_inside_tree():
		return
	if not _dirty:
		return
	_relayout()


func _relayout() -> void:
	_dirty = false
	debug_warnings.clear()
	_layout_rects.clear()

	var content := content_rect()
	var main_vertical := vertical
	var main_size := maxf(content.size.y if main_vertical else content.size.x, 0.0)
	var cross_size := maxf(content.size.x if main_vertical else content.size.y, 0.0)

	var children: Array[Control] = []
	for child in get_children():
		var control := child as Control
		if control == null or not control.visible:
			continue
		children.append(control)
	if children.is_empty():
		return

	# ── 第一遍：按主轴把子元素切成行 / 列（顺序 = child index）──
	var lines: Array = []
	var line: Array = []
	var used := 0.0
	for control in children:
		var request := control.get_combined_minimum_size()
		var req_main := request.y if main_vertical else request.x
		var req_cross := request.x if main_vertical else request.y
		if req_main > main_size + LAYOUT_EPSILON:
			debug_warnings.append("%s：主轴最小尺寸 %.1f 超出可用 %.1f，footprint 夹到可用空间" % [control.name, req_main, main_size])
			req_main = main_size
		if req_cross > cross_size + LAYOUT_EPSILON:
			debug_warnings.append("%s：交叉轴最小尺寸 %.1f 超出可用 %.1f，footprint 夹到可用空间" % [control.name, req_cross, cross_size])
			req_cross = cross_size
		if not line.is_empty() and used + spacing + req_main > main_size + LAYOUT_EPSILON:
			if wrap:
				lines.append(line)
				line = []
				used = 0.0
			else:
				var truncated := maxf(0.0, main_size - used - spacing)
				if truncated < req_main - LAYOUT_EPSILON:
					debug_warnings.append("%s：主轴放不下且 wrap=false，footprint 截断到 %.1f" % [control.name, truncated])
				req_main = truncated
		line.append({"node": control, "main": req_main, "cross": req_cross})
		used += req_main + (spacing if line.size() > 1 else 0.0)
	if not line.is_empty():
		lines.append(line)

	# ── 第二遍：算每条线的范围，再按对齐放 ──
	var line_extents: Array[float] = []
	var line_cross: Array[float] = []
	var cross_total := 0.0
	for i in lines.size():
		var group: Array = lines[i]
		var extent := 0.0
		var group_cross := 0.0
		for item in group:
			var dict: Dictionary = item
			extent += float(dict["main"])
			group_cross = maxf(group_cross, float(dict["cross"]))
		extent += spacing * maxf(0.0, group.size() - 1)
		line_extents.append(extent)
		line_cross.append(group_cross)
		cross_total += group_cross
	cross_total += spacing * maxf(0.0, lines.size() - 1)

	# 交叉轴：线从边缘往「远离该边缘」的方向堆（TOP 向下 / BOTTOM 向上 / LEFT 向右 / RIGHT 向左）。
	var cross_positions: Array[float] = []
	var cross_cursor := 0.0
	if edge == Edge.BOTTOM or edge == Edge.RIGHT:
		for i in lines.size():
			cross_cursor += line_cross[i]
			cross_positions.append(maxf(0.0, cross_size - cross_cursor))
			cross_cursor += spacing
	elif edge == Edge.CENTER:
		cross_cursor = maxf(0.0, (cross_size - cross_total) * 0.5)
		for i in lines.size():
			cross_positions.append(cross_cursor)
			cross_cursor += line_cross[i] + spacing
	else:
		for i in lines.size():
			cross_positions.append(cross_cursor)
			cross_cursor += line_cross[i] + spacing

	for i in lines.size():
		var group: Array = lines[i]
		var extent: float = line_extents[i]
		var main_cursor := 0.0
		match align:
			Align.END:
				main_cursor = main_size - extent
			Align.CENTER:
				main_cursor = (main_size - extent) * 0.5
			_:
				main_cursor = 0.0
		main_cursor = maxf(main_cursor, 0.0)
		var line_cross_start: float = cross_positions[i]
		for item in group:
			var dict: Dictionary = item
			var control: Control = dict["node"]
			var footprint_main := float(dict["main"])
			var footprint_cross := float(dict["cross"])
			var room_cross := maxf(0.0, cross_size - line_cross_start)
			if footprint_cross > room_cross + LAYOUT_EPSILON:
				footprint_cross = room_cross
			var room_main := maxf(0.0, main_size - main_cursor)
			if footprint_main > room_main + LAYOUT_EPSILON:
				if footprint_main > float(dict["main"]) + LAYOUT_EPSILON:
					debug_warnings.append("%s：主轴需要 %.1f，只剩 %.1f，footprint 截断" % [control.name, footprint_main, room_main])
				footprint_main = room_main
			var position := Vector2.ZERO
			var footprint := Vector2.ZERO
			if main_vertical:
				position = Vector2(content.position.x + line_cross_start, content.position.y + main_cursor)
				footprint = Vector2(footprint_cross, footprint_main)
			else:
				position = Vector2(content.position.x + main_cursor, content.position.y + line_cross_start)
				footprint = Vector2(footprint_main, footprint_cross)
			_apply_child(control, position, footprint)
			main_cursor += footprint_main + spacing


func _apply_child(control: Control, position: Vector2, footprint: Vector2) -> void:
	control.position = position
	control.size = footprint
	_layout_rects[control] = Rect2(position, footprint)
	var actual := control.size
	if actual.x > footprint.x + LAYOUT_EPSILON or actual.y > footprint.y + LAYOUT_EPSILON:
		debug_warnings.append("%s：引擎按最小尺寸钳住 size（footprint %s → 实际 %s）" % [control.name, str(footprint), str(actual)])
