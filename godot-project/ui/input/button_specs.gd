class_name ButtonSpecs
extends RefCounted
## 动作通道 -> 按钮规格表（Godot 侧）。
##
## 数据源：BevyInputContext.channels() 返回的 Array[{ key, mask, mode, actions }]
## （通道表由 Rust 提供）。Rust 只描述「有哪些动作通道」，一个通道显示几个按钮、
## 按钮文案、放在哪个托盘，全部由这里决定，改 UI 不需要改 Rust。
##
## 规格只声明语义（tray / align / priority / schemes），不写坐标：
## 布局交给托盘容器 + SafeArea。
##
## 本次约定（不要改）：动作按钮默认放右下角（tray = RIGHT, align = END），
## priority = 0，并且只在触屏方案显示（schemes = UiState.Scheme.TOUCH）。

## 有显式规格的通道。tool 通道有两个动作（主工具 / 副工具），所以对应两个按钮。
const BY_CHANNEL: Dictionary = {
	"jump":     [{ "id": "btn_jump", "label": "Jump", "action": "jump" }],
	"tool":     [
		{ "id": "btn_primary", "label": "Use", "action": "primary_tool" },
		{ "id": "btn_secondary", "label": "Alt", "action": "secondary_tool" },
	],
	"glide":    [{ "id": "btn_glide", "label": "Glide", "action": "glide" }],
	"flight":   [{ "id": "btn_flight", "label": "Fly", "action": "flight_toggle" }],
	"interact": [{ "id": "btn_interact", "label": "Interact", "action": "interact" }],
	"sprint":   [{ "id": "btn_sprint", "label": "Sprint", "action": "sprint" }],
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
## scheme_overrides 都可以逐条覆盖），缺省托盘约定见文件头。
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
		"tray": int(spec.get("tray", HudWidget.Tray.RIGHT)),
		"align": int(spec.get("align", HudWidget.Align.END)),
		"priority": int(spec.get("priority", 0)),
		"schemes": int(spec.get("schemes", UiState.Scheme.TOUCH)),
		"scheme_overrides": override_dict,
	}
