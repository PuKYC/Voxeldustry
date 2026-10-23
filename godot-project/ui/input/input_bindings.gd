class_name InputBindings
extends RefCounted
## 输入绑定层：触屏动作簇槽位解析 / 键鼠+手柄提示文案 / 键鼠提交前上下文重映射。
##
## 纯 static、无节点、无状态：这里是「槽位配置 + 文案」的唯一真相源。
## Rust 只描述「有哪些通道、每个通道有哪些动作」（BevyInputContext.channels()），
## 一个通道显示几个按钮、按钮叫什么、怎么排布，全部由 Godot 侧决定。
##
## 布局决策（用户已确认）：键鼠 / 触屏默认使用精简的 CONTEXT 布局：
##   primary = attack（target=BLOCK 时与 break_block 互换，主按钮仍是攻/破）
##   secondary = place_block，jump / interact / glide / flight 各一个。
## SPLIT 布局把三个工具动作拆成独立按钮，便于未来做自定义 HUD。
##
## UiState 新字段由 touch-settings 模块提供；这里用脚本反射读取（has_method /
## 属性兜底），在字段尚未落地时退回到 CONTEXT / 1.0 / 0.75 / 右手，保证本模块
## 可以独立跑测试，最终与 UiState 契约一致。

const UI_STATE_SCRIPT: GDScript = preload("res://ui/ui_state.gd")

## 动作簇布局。与 UiState.ActionLayout 数值一致。
enum Layout {
	CONTEXT = 0,
	SPLIT = 1,
}

## Rust INTERACTION 载荷的 target 取值（game-core payload::INTERACTION_TARGET_*）。
const TARGET_NONE := 0
const TARGET_ENTITY := 1
const TARGET_BLOCK := 2

## 语义动作 -> 按钮文案（触屏按钮 / 键鼠提示共用）。
const LABELS: Dictionary = {
	"attack": "Attack",
	"break_block": "Break",
	"place_block": "Place",
	"jump": "Jump",
	"glide": "Glide",
	"flight_toggle": "Fly",
	"interact": "Interact",
	"sprint": "Sprint",
}

## CONTEXT 布局槽位（稳定 role 顺序）。actions 是候选动作，按优先级排列；
## target=BLOCK 时 primary 的候选顺序反转（破坏优先）。
const SLOTS_CONTEXT: Array = [
	{ "role": "primary", "actions": ["attack", "break_block"], "size_role": "main" },
	{ "role": "secondary", "actions": ["place_block"], "size_role": "sub" },
	{ "role": "jump", "actions": ["jump"], "size_role": "jump" },
	{ "role": "interact", "actions": ["interact"], "size_role": "sub" },
	{ "role": "glide", "actions": ["glide"], "size_role": "sub" },
	{ "role": "flight", "actions": ["flight_toggle"], "size_role": "sub" },
]

## SPLIT 布局槽位：三个工具动作各自独立（没有 main，除 jump 外都是 sub）。
const SLOTS_SPLIT: Array = [
	{ "role": "attack", "actions": ["attack"], "size_role": "sub" },
	{ "role": "break_block", "actions": ["break_block"], "size_role": "sub" },
	{ "role": "place_block", "actions": ["place_block"], "size_role": "sub" },
	{ "role": "jump", "actions": ["jump"], "size_role": "jump" },
	{ "role": "interact", "actions": ["interact"], "size_role": "sub" },
	{ "role": "glide", "actions": ["glide"], "size_role": "sub" },
	{ "role": "flight", "actions": ["flight_toggle"], "size_role": "sub" },
]

## 键鼠 / 手柄提示里要展示的 role（主 / 副 / 跳跃；SPLIT 下展示三个工具动作）。
const HINT_ROLES_CONTEXT: Array = ["primary", "secondary", "jump"]
const HINT_ROLES_SPLIT: Array = ["attack", "break_block", "place_block", "jump"]


# ───────────────────────── UiState 兼容读取 ─────────────────────────

## 当前动作簇布局（UiState 字段缺失时回退 CONTEXT）。
static func ui_layout() -> int:
	if "action_layout" in UI_STATE_SCRIPT:
		var value: Variant = UI_STATE_SCRIPT.get("action_layout")
		if value is int or value is float:
			return int(value)
	return Layout.CONTEXT


## 按钮缩放（UiState 字段缺失时回退 1.0）。
static func ui_button_scale() -> float:
	if "button_scale" in UI_STATE_SCRIPT:
		var value: Variant = UI_STATE_SCRIPT.get("button_scale")
		if value is int or value is float:
			return maxf(0.05, float(value))
	return 1.0


## 按钮不透明度（UiState 字段缺失时回退 0.75）。
static func ui_button_opacity() -> float:
	if "button_opacity" in UI_STATE_SCRIPT:
		var value: Variant = UI_STATE_SCRIPT.get("button_opacity")
		if value is int or value is float:
			return clampf(float(value), 0.05, 1.0)
	return 0.75


## 左手模式（UiState 字段缺失时回退右手）。
static func ui_left_handed() -> bool:
	if "left_handed" in UI_STATE_SCRIPT:
		return bool(UI_STATE_SCRIPT.get("left_handed"))
	return false


## 单个动作按钮边长（像素）。优先用 UiState.action_button_units（已含 button_scale /
## 平板放大 / 物理单位换算）；UiState 尚未落地时按 10mm 基准的近似像素兜底。
static func ui_button_units(size_role: String) -> float:
	if UI_STATE_SCRIPT.has_method("action_button_units"):
		var value: Variant = UI_STATE_SCRIPT.call("action_button_units", size_role)
		if value is int or value is float:
			return maxf(1.0, float(value))
	var base := 86.0
	match size_role:
		"main":
			base = 96.0
		"jump":
			base = 91.0
		_:
			base = 86.0
	return base * ui_button_scale()


# ───────────────────────── 槽位解析 ─────────────────────────

## 动作 -> 通道 key（channels 反查）；查不到时按动作名当通道 key 兜底，
## 方便测试直接注入 {action: [present, available]}，也兼容未来无通道表的情况。
static func channel_key_for_action(action: String, channels: Array) -> String:
	for raw in channels:
		if not (raw is Dictionary):
			continue
		var row: Dictionary = raw
		var actions: Array = row.get("actions", [])
		for candidate in actions:
			if String(candidate) == action:
				return String(row.get("key", ""))
	return action


## 取某个动作对应通道的 [present, available]；没有记录时视为 [false, false]。
static func state_for_action(action: String, channels: Array, states: Dictionary) -> Array:
	var key := channel_key_for_action(action, channels)
	var raw: Variant = states.get(key, null)
	if raw is Array and (raw as Array).size() >= 2:
		return [bool((raw as Array)[0]), bool((raw as Array)[1])]
	return [false, false]


## 当前布局的槽位配置（返回新数组；target=BLOCK 时反转 primary 候选顺序）。
static func slots_for_layout(layout: int, target_kind: int = TARGET_NONE) -> Array:
	if layout == Layout.SPLIT:
		return SLOTS_SPLIT.duplicate(true)
	var out: Array = []
	for raw in SLOTS_CONTEXT:
		var slot: Dictionary = (raw as Dictionary).duplicate(true)
		if String(slot.get("role", "")) == "primary" and target_kind == TARGET_BLOCK:
			var actions: Array = slot.get("actions", [])
			actions.reverse()
			slot["actions"] = actions
		out.append(slot)
	return out


## 按布局解析触屏按钮槽位。返回每个配置槽一条（含 visible=false 的）：
##   { role, action, label, visible, available, size_role }
## visible 只由「候选动作的通道 present」决定（interact 还要 context.interact_enabled）；
## available=false 时按钮置灰但位置保留（不位移，避免布局跳动）。
static func resolve_touch(layout: int, channels: Array, states: Dictionary, context: Dictionary) -> Array:
	var target_kind := int(context.get("target_kind", TARGET_NONE))
	var interact_enabled := bool(context.get("interact_enabled", false))
	var out: Array = []
	for raw in slots_for_layout(layout, target_kind):
		var slot: Dictionary = raw
		var role := String(slot.get("role", ""))
		var actions: Array = slot.get("actions", [])
		var size_role := String(slot.get("size_role", "sub"))
		var first_present := ""
		var resolved := ""
		var available := false
		for candidate in actions:
			var action := String(candidate)
			var state := state_for_action(action, channels, states)
			var present := bool(state[0])
			if not present:
				continue
			if first_present.is_empty():
				first_present = action
			if bool(state[1]):
				resolved = action
				available = true
				break
		if resolved.is_empty():
			if not first_present.is_empty():
				resolved = first_present
			elif not actions.is_empty():
				resolved = String(actions[0])
			available = false
		var visible := not first_present.is_empty()
		if role == "interact":
			visible = visible and interact_enabled
			if not visible:
				available = false
		out.append({
			"role": role,
			"action": resolved,
			"label": label_for_action(resolved),
			"visible": visible,
			"available": available,
			"size_role": size_role,
		})
	return out


## 动作 -> 文案。
static func label_for_action(action: String) -> String:
	if action.is_empty():
		return ""
	if LABELS.has(action):
		return String(LABELS[action])
	return action.replace("_", " ").capitalize()


# ───────────────────────── 键鼠 / 手柄提示 ─────────────────────────

## 键鼠 / 手柄的按键提示：触屏返回空串（触屏没有物理按键可提示）。
## 例：CONTEXT -> "[LMB] Attack  [RMB] Place  [Space] Jump"
static func hint_text(scheme: int, channels: Array, states: Dictionary, context: Dictionary) -> String:
	if (scheme & UiState.Scheme.TOUCH) != 0:
		return ""
	var layout := ui_layout()
	var entries := resolve_touch(layout, channels, states, context)
	var roles: Array = HINT_ROLES_SPLIT if layout == Layout.SPLIT else HINT_ROLES_CONTEXT
	var parts: PackedStringArray = []
	for role in roles:
		var entry := entry_for_role(entries, String(role))
		if entry.is_empty():
			continue
		var action := String(entry.get("action", ""))
		var key_text := action_key_text(action)
		if key_text.is_empty():
			continue
		parts.append("[%s] %s" % [key_text, label_for_action(action)])
	return "  ".join(parts)


## 从 resolve_touch 结果里按 role 取一条；没有返回空字典。
static func entry_for_role(entries: Array, role: String) -> Dictionary:
	for raw in entries:
		if raw is Dictionary and String((raw as Dictionary).get("role", "")) == role:
			return raw
	return {}


## 某个动作在 InputMap 里的第一个事件的文本（键盘取物理键名，鼠标取短名，手柄 as_text）。
static func action_key_text(action: String) -> String:
	if action.is_empty() or not InputMap.has_action(action):
		return ""
	for raw in InputMap.action_get_events(action):
		var event := raw as InputEvent
		if event == null:
			continue
		var text := ""
		if event is InputEventKey:
			var key := event as InputEventKey
			var code: Key = key.physical_keycode if key.physical_keycode != 0 else key.keycode
			text = OS.get_keycode_string(code)
			if text.is_empty():
				text = key.as_text()
		elif event is InputEventMouseButton:
			text = mouse_button_text((event as InputEventMouseButton).button_index)
		else:
			text = event.as_text()
		if not text.is_empty():
			return text
	return ""


## 鼠标键短名（与提示示例 "[LMB] Attack" 对齐）；其它键退回 as_text()。
static func mouse_button_text(button: int) -> String:
	match button:
		MOUSE_BUTTON_LEFT:
			return "LMB"
		MOUSE_BUTTON_MIDDLE:
			return "MMB"
		MOUSE_BUTTON_RIGHT:
			return "RMB"
		MOUSE_BUTTON_WHEEL_UP:
			return "WheelUp"
		MOUSE_BUTTON_WHEEL_DOWN:
			return "WheelDown"
		_:
			return "Mouse%d" % button


# ───────────────────────── 键鼠 / 手柄提交前重映射 ─────────────────────────

## 键鼠 / 手柄提交前按目标类型重映射动作名：
## CONTEXT 布局下 attack 在 target=BLOCK 且 break_block 通道可用时改报 break_block，
## 否则保持原样；SPLIT 布局原样返回（按钮已拆分，不需要代打）。
## 返回值已去重且保持原顺序。
static func remap_actions(actions: Array, layout: int, channels: Array, states: Dictionary, target_kind: int) -> Array:
	var out: Array = []
	if layout == Layout.SPLIT:
		for raw in actions:
			var action := String(raw)
			if not action.is_empty() and not out.has(action):
				out.append(action)
		return out
	var break_available := false
	var break_state := state_for_action("break_block", channels, states)
	if bool(break_state[0]) and bool(break_state[1]):
		break_available = true
	for raw in actions:
		var action := String(raw)
		if action.is_empty():
			continue
		if action == "attack" and target_kind == TARGET_BLOCK and break_available:
			action = "break_block"
		if not out.has(action):
			out.append(action)
	return out
