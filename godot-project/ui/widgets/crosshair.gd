class_name Crosshair
extends HudWidget
## 屏幕中心准星：只在视角轴（LookChannel）开放时显示。
##
## 不写坐标：tray=CENTER 由 UiRoot 的 CenterContainer 居中；
## 尺寸取 custom_minimum_size 24x24，绘制始终以控件中心为原点。
## 无 client（纯 UI 预览 / 无编辑器 headless 冒烟）时保持隐藏，不做任何轮询。

## 十字四臂长度、线宽、中心间隙（设计单位 / 像素）。
## 线宽取 4px；臂长 6px + 中心间隙 4px，整体 20x20 落在 24x24 控件内。
const ARM_LENGTH := 6.0
const ARM_THICKNESS := 4.0
const CENTER_GAP := 4.0
const COLOR := Color(1.0, 1.0, 1.0, 0.85)

## BevyClient；未 attach 或场景中没有 client 时为 null。
var _client: Object = null


func _on_attach() -> void:
	_client = _resolve_client()
	# 默认隐藏：等 input_look_changed 明确告知「视角可用」再显示。
	set_data_visible(false)
	if _client == null:
		return
	if _client.has_signal("input_look_changed"):
		_client.connect(&"input_look_changed", _on_input_look_changed)
	_refresh_from_context()


func _on_detach() -> void:
	# 先断 client 信号，再让注册表 queue_free，避免残留回调打到已释放实例。
	if _client != null and _client.has_signal("input_look_changed"):
		if _client.is_connected(&"input_look_changed", _on_input_look_changed):
			_client.disconnect(&"input_look_changed", _on_input_look_changed)
	_client = null


func _notification(what: int) -> void:
	if what == NOTIFICATION_RESIZED:
		queue_redraw()


func _draw() -> void:
	# 以控件中心为原点画四条短臂，中间留 CENTER_GAP。
	var c := size * 0.5
	var half := ARM_THICKNESS * 0.5
	# 上
	draw_rect(Rect2(c.x - half, c.y - CENTER_GAP - ARM_LENGTH, ARM_THICKNESS, ARM_LENGTH), COLOR)
	# 下
	draw_rect(Rect2(c.x - half, c.y + CENTER_GAP, ARM_THICKNESS, ARM_LENGTH), COLOR)
	# 左
	draw_rect(Rect2(c.x - CENTER_GAP - ARM_LENGTH, c.y - half, ARM_LENGTH, ARM_THICKNESS), COLOR)
	# 右
	draw_rect(Rect2(c.x + CENTER_GAP, c.y - half, ARM_LENGTH, ARM_THICKNESS), COLOR)


func _on_input_look_changed(_available: bool) -> void:
	_refresh_from_context()


## 从 input_context 读一次视角状态（信号驱动，不轮询）。
func _refresh_from_context() -> void:
	if _client == null:
		set_data_visible(false)
		return
	var ctx: Variant = _client.get("input_context")
	if ctx != null and ctx.has_method("has_look"):
		set_data_visible(bool(ctx.has_look()))
		return
	set_data_visible(false)


## UiRoot 暴露 client 属性 / get_client()；都不存在时返回 null（headless 安全）。
func _resolve_client() -> Object:
	if ui_root == null:
		return null
	if ui_root.has_method("get_client"):
		return ui_root.get_client()
	var c: Variant = ui_root.get("client")
	return c if c != null else null
