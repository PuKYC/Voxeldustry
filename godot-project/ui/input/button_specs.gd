class_name ButtonSpecs
extends RefCounted
## 动作通道 -> 按钮规格表（Godot 侧）的兼容数据层。
##
## 注意：动作簇（ActionCluster / InputBindings）已改为「一个簇一个 HudWidget」的
## 语义槽位模型，按下述 ButtonSpecs 不再参与触屏布局。本文件保留给：
## 1) 旧测试 / 外部工具读取「通道 -> 按钮语义」；
## 2) mod 或临时 UI 仍想按通道拉一条规格时的轻量兜底。
## 新代码请优先用 InputBindings（槽位 / 文案 / 提示的唯一真相源）。
##
## 数据源：BevyInputContext.channels() 返回的 Array[{ key, mask, mode, actions }]。
## Rust 侧语义动作：attack / break_block / place_block（tool 通道已整体移除）。

## 有显式规格的通道。一个通道可以映射多个按钮（例如未来自定义 HUD 拆键）。
const BY_CHANNEL: Dictionary = {
	"jump":        [{ "id": "btn_jump", "label": "Jump", "action": "jump" }],
	"glide":       [{ "id": "btn_glide", "label": "Glide", "action": "glide" }],
	"flight":      [{ "id": "btn_flight", "label": "Fly", "action": "flight_toggle" }],
	"interact":    [{ "id": "btn_interact", "label": "Interact", "action": "interact" }],
	"sprint":      [{ "id": "btn_sprint", "label": "Sprint", "action": "sprint" }],
	"attack":      [{ "id": "btn_attack", "label": "Attack", "action": "attack" }],
	"break_block": [{ "id": "btn_break_block", "label": "Break", "action": "break_block" }],
	"place_block": [{ "id": "btn_place_block", "label": "Place", "action": "place_block" }],
}

## action -> 规格 的反查表（预留：供后续「按动作名找按钮」用，本轮不填）。
const BY_ACTION: Dictionary = {}


## 某通道的显式规格数组；没有显式规格时返回 []（调用方应走 fallback_spec）。
static func specs_for_channel(key: String) -> Array:
	var specs: Array = BY_CHANNEL.get(key, [])
	if specs.is_empty():
		return []
	return specs


## 兜底规格：mod 新增的通道没有显式规格时，用 key + actions[0] 生成一个按钮，
## 保证 Rust 加了通道、Godot 没改代码时也不会丢按钮。
static func fallback_spec(channel: Dictionary) -> Dictionary:
	var key := String(channel.get("key", ""))
	var actions: Array = channel.get("actions", [])
	var action := ""
	if not actions.is_empty():
		action = String(actions[0])
	return {
		"id": "btn_" + key,
		"label": key.replace("_", " ").capitalize(),
		"action": action,
	}


## 通道表 -> 扁平归一化规格列表。
## 跳过 axis2d（移动由摇杆负责）、空 key、空 actions 的通道；
## 每个通道先查显式规格，没有就 fallback_spec 兜底。
static func specs_for_channels(channels: Array) -> Array:
	var out: Array = []
	for row in channels:
		if not (row is Dictionary):
			continue
		var channel: Dictionary = row
		var key := String(channel.get("key", ""))
		var mode := String(channel.get("mode", ""))
		var actions: Array = channel.get("actions", [])
		if key.is_empty() or mode == "axis2d" or actions.is_empty():
			continue
		var specs := specs_for_channel(key)
		if specs.is_empty():
			out.append(normalize_spec(fallback_spec(channel), channel))
			continue
		for raw in specs:
			if raw is Dictionary:
				out.append(normalize_spec(raw, channel))
	return out


## 通道的默认文案（显式规格取第一条的 label，否则 key -> Title Case）。
static func label_for(key: String) -> String:
	var specs := specs_for_channel(key)
	if not specs.is_empty():
		var first: Dictionary = specs[0]
		var label := String(first.get("label", ""))
		if not label.is_empty():
			return label
	return key.replace("_", " ").capitalize()


## 补齐归一化字段：id / label / action / mode / channel / tray / align /
## priority / schemes / scheme_overrides 缺一不可。
## 规格里的可选字段优先（mode / tray / align / priority / schemes /
## scheme_overrides 都可以逐条覆盖），缺省托盘约定见 InputBindings。
static func normalize_spec(spec: Dictionary, channel: Dictionary) -> Dictionary:
	var key := String(channel.get("key", ""))
	var actions: Array = channel.get("actions", [])
	var action := String(spec.get("action", ""))
	if action.is_empty() and not actions.is_empty():
		action = String(actions[0])
	var id := String(spec.get("id", ""))
	if id.is_empty():
		id = "btn_" + (action if not action.is_empty() else key)
	var label := String(spec.get("label", ""))
	if label.is_empty():
		label = key.replace("_", " ").capitalize()
	var overrides: Variant = spec.get("scheme_overrides", {})
	var override_dict: Dictionary = {}
	if overrides is Dictionary:
		override_dict = (overrides as Dictionary).duplicate()
	return {
		"id": id,
		"label": label,
		"action": action,
		"mode": String(spec.get("mode", channel.get("mode", ""))),
		"channel": key,
		"tray": int(spec.get("tray", HudWidget.Tray.BOTTOM)),
		"align": int(spec.get("align", HudWidget.Align.END)),
		"priority": int(spec.get("priority", 10)),
		"schemes": int(spec.get("schemes", UiState.Scheme.TOUCH)),
		"scheme_overrides": override_dict,
	}
