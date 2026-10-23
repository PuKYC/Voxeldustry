extends SceneTree
## 输入绑定层 / 动作簇 / 动作按钮 headless 测试（无渲染、无 BevyClient）。
##
## 覆盖：
## 1. InputBindings.resolve_touch：CONTEXT / SPLIT 槽位数量、role 顺序、文案、
##    主按钮 target=BLOCK 切到 break_block、stunned（present 但不可用）置灰不隐藏、
##    interact 随 interact_enabled 显隐、无通道表时的 action->channel 兜底；
## 2. InputBindings.hint_text：触屏为空、键鼠含真实键名（LMB / Space 等）；
## 3. InputBindings.remap_actions：CONTEXT + target=BLOCK 时 attack -> break_block，SPLIT 原样；
## 4. ActionCluster：主按钮钉在簇右下（左手左下）、interact 出现前后主按钮不位移、
##    按钮不出簇、无槽位时按钮数 0；
## 5. ActionButton：ScreenTouch 注入动作 / 释放 / 不可用立即释放且矩形不变 / toggle 自动释放；
## 6. BevyInputContext：INTERACTION 载荷 -> interaction_target_changed（去抖 / 实体切换复位）、
##    channel_states 快照。
##
## 运行：
##   /tmp/Godot_v4.7.2-stable_linux.arm64 --headless --path godot-project \
##       --script res://test/input_bindings_test.gd

const TOL := 0.6

## 测试用通道表（mask 位与 Rust 侧不必一致，只要互不重叠即可）。
const CHANNELS: Array = [
	{ "key": "move", "mask": 1, "mode": "axis2d", "actions": ["move_forward", "move_back", "move_left", "move_right"] },
	{ "key": "jump", "mask": 2, "mode": "button", "actions": ["jump"] },
	{ "key": "glide", "mask": 4, "mode": "hold", "actions": ["glide"] },
	{ "key": "flight", "mask": 8, "mode": "toggle", "actions": ["flight_toggle"] },
	{ "key": "interact", "mask": 16, "mode": "button", "actions": ["interact"] },
	{ "key": "attack", "mask": 32, "mode": "button", "actions": ["attack"] },
	{ "key": "break_block", "mask": 64, "mode": "button", "actions": ["break_block"] },
	{ "key": "place_block", "mask": 128, "mode": "button", "actions": ["place_block"] },
	{ "key": "sprint", "mask": 256, "mode": "button", "actions": ["sprint"] },
]

const ALL_ACTIONS: Array = [
	"attack", "break_block", "place_block", "jump", "interact", "glide", "flight_toggle", "sprint",
]

var _failures: PackedStringArray = PackedStringArray()
var _checks := 0


func _initialize() -> void:
	_run.call_deferred()


func _run() -> void:
	await process_frame
	print("[input_bindings] boot scheme=%d" % UiState.scheme)
	_ensure_actions()
	_test_resolve_context()
	_test_resolve_split()
	_test_states_and_fallback()
	_test_hints()
	_test_remap()
	await _test_cluster()
	await _test_button()
	await _test_input_context()
	await _test_bevy_input()
	_release_actions()
	_finish()


# ───────────────────────── 1. resolve_touch ─────────────────────────

func _test_resolve_context() -> void:
	var context := { "target_kind": InputBindings.TARGET_NONE, "interact_enabled": false }
	var entries := InputBindings.resolve_touch(
		InputBindings.Layout.CONTEXT, CHANNELS, _states(), context)
	_check(entries.size() == 6, "CONTEXT：6 个配置槽（实际 %d）" % entries.size())
	var roles: Array = []
	for entry in entries:
		roles.append(String(entry["role"]))
	_check(roles == ["primary", "secondary", "jump", "interact", "glide", "flight"],
		"CONTEXT：role 顺序稳定（实际 %s）" % str(roles))

	var primary := InputBindings.entry_for_role(entries, "primary")
	_check(String(primary.get("action", "")) == "attack", "CONTEXT：primary 默认 attack")
	_check(String(primary.get("label", "")) == "Attack", "CONTEXT：primary 文案 Attack")
	_check(bool(primary.get("visible", false)) and bool(primary.get("available", false)),
		"CONTEXT：primary 可见且可用")
	_check(String(primary.get("size_role", "")) == "main", "CONTEXT：primary 尺寸角色 main")
	var secondary := InputBindings.entry_for_role(entries, "secondary")
	_check(String(secondary.get("action", "")) == "place_block", "CONTEXT：secondary = place_block")
	_check(String(secondary.get("label", "")) == "Place", "CONTEXT：secondary 文案 Place")
	var jump := InputBindings.entry_for_role(entries, "jump")
	_check(String(jump.get("size_role", "")) == "jump", "CONTEXT：jump 尺寸角色 jump")
	var interact := InputBindings.entry_for_role(entries, "interact")
	_check(not bool(interact.get("visible", true)), "CONTEXT：interact_enabled=false 时 interact 隐藏")
	_check(bool(InputBindings.entry_for_role(entries, "glide").get("visible", false)),
		"CONTEXT：glide 通道 present 时可见")
	_check(bool(InputBindings.entry_for_role(entries, "flight").get("visible", false)),
		"CONTEXT：flight 通道 present 时可见")

	# target = BLOCK：主按钮候选顺序反转。
	var block_entries := InputBindings.resolve_touch(
		InputBindings.Layout.CONTEXT, CHANNELS, _states(),
		{ "target_kind": InputBindings.TARGET_BLOCK, "interact_enabled": false })
	var block_primary := InputBindings.entry_for_role(block_entries, "primary")
	_check(String(block_primary.get("action", "")) == "break_block",
		"CONTEXT+BLOCK：主按钮变 break_block（实际 %s）" % str(block_primary.get("action", "")))
	_check(String(block_primary.get("label", "")) == "Break", "CONTEXT+BLOCK：文案 Break")

	# interact_enabled 控制 interact 显隐。
	var interact_entries := InputBindings.resolve_touch(
		InputBindings.Layout.CONTEXT, CHANNELS, _states(),
		{ "target_kind": InputBindings.TARGET_NONE, "interact_enabled": true })
	var interact_on := InputBindings.entry_for_role(interact_entries, "interact")
	_check(bool(interact_on.get("visible", false)) and bool(interact_on.get("available", false)),
		"CONTEXT：interact_enabled=true 时 interact 可见可用")
	_check(String(interact_on.get("label", "")) == "Interact", "CONTEXT：interact 文案 Interact")

	# 通道表里没有 interact 时，即使 interact_enabled 也不显示。
	var states_without_interact := _states()
	states_without_interact.erase("interact")
	var no_interact := InputBindings.resolve_touch(
		InputBindings.Layout.CONTEXT, CHANNELS, states_without_interact,
		{ "target_kind": InputBindings.TARGET_NONE, "interact_enabled": true })
	_check(not bool(InputBindings.entry_for_role(no_interact, "interact").get("visible", true)),
		"CONTEXT：通道缺失时 interact 仍隐藏")


func _test_resolve_split() -> void:
	var entries := InputBindings.resolve_touch(
		InputBindings.Layout.SPLIT, CHANNELS, _states(),
		{ "target_kind": InputBindings.TARGET_NONE, "interact_enabled": false })
	_check(entries.size() == 7, "SPLIT：7 个配置槽（实际 %d）" % entries.size())
	var roles: Array = []
	for entry in entries:
		roles.append(String(entry["role"]))
	_check(roles == ["attack", "break_block", "place_block", "jump", "interact", "glide", "flight"],
		"SPLIT：role 顺序稳定（实际 %s）" % str(roles))
	_check(String(InputBindings.entry_for_role(entries, "attack").get("action", "")) == "attack",
		"SPLIT：attack 独立按钮")
	_check(String(InputBindings.entry_for_role(entries, "break_block").get("action", "")) == "break_block",
		"SPLIT：break_block 独立按钮")
	_check(String(InputBindings.entry_for_role(entries, "place_block").get("action", "")) == "place_block",
		"SPLIT：place_block 独立按钮")
	_check(String(InputBindings.entry_for_role(entries, "attack").get("size_role", "")) == "sub",
		"SPLIT：无 main，攻击按钮为 sub")
	_check(String(InputBindings.entry_for_role(entries, "jump").get("size_role", "")) == "jump",
		"SPLIT：jump 仍是 jump 尺寸角色")
	_check(not bool(InputBindings.entry_for_role(entries, "interact").get("visible", true)),
		"SPLIT：interact_enabled=false 时隐藏")

	# SPLIT 下 target=BLOCK 不改变动作（按钮已拆分，不做代打）。
	var block_entries := InputBindings.resolve_touch(
		InputBindings.Layout.SPLIT, CHANNELS, _states(),
		{ "target_kind": InputBindings.TARGET_BLOCK, "interact_enabled": false })
	_check(String(InputBindings.entry_for_role(block_entries, "attack").get("action", "")) == "attack",
		"SPLIT+BLOCK：attack 保持 attack")


# ───────────────────────── 2. 状态 / 兜底 ─────────────────────────

func _test_states_and_fallback() -> void:
	# stunned：present 仍为 true，但 available 全 false -> 可见 + 置灰，不隐藏。
	var stunned := {}
	for key: String in _states().keys():
		stunned[key] = [true, false]
	var stunned_entries := InputBindings.resolve_touch(
		InputBindings.Layout.CONTEXT, CHANNELS, stunned,
		{ "target_kind": InputBindings.TARGET_NONE, "interact_enabled": false })
	var stunned_primary := InputBindings.entry_for_role(stunned_entries, "primary")
	_check(bool(stunned_primary.get("visible", false)), "stunned：primary 仍 visible")
	_check(not bool(stunned_primary.get("available", true)), "stunned：primary available=false")
	_check(String(stunned_primary.get("action", "")) == "attack",
		"stunned：全不可用时取第一个 present 候选 attack")
	# 第一个候选不可用、第二个可用时取第二个。
	var mixed := _states()
	mixed["attack"] = [true, false]
	mixed["break_block"] = [true, true]
	var mixed_primary := InputBindings.entry_for_role(InputBindings.resolve_touch(
		InputBindings.Layout.CONTEXT, CHANNELS, mixed,
		{ "target_kind": InputBindings.TARGET_NONE, "interact_enabled": false }), "primary")
	_check(String(mixed_primary.get("action", "")) == "break_block",
		"候选回退：attack 不可用 -> 取可用的 break_block")
	_check(bool(mixed_primary.get("available", false)), "候选回退：break_block available=true")

	# 全无 present：全部隐藏，action 保持第一个候选。
	var absent := {}
	var absent_entries := InputBindings.resolve_touch(
		InputBindings.Layout.CONTEXT, CHANNELS, absent,
		{ "target_kind": InputBindings.TARGET_NONE, "interact_enabled": false })
	for entry in absent_entries:
		_check(not bool(entry.get("visible", true)), "无通道：%s 不可见" % str(entry["role"]))
		_check(not bool(entry.get("available", true)), "无通道：%s 不可用" % str(entry["role"]))
	_check(String(InputBindings.entry_for_role(absent_entries, "primary").get("action", "")) == "attack",
		"无通道：primary 保持第一个候选 attack")

	# channels 为空时按 action 名当 channel key 兜底（测试注入方便）。
	var fallback_states := { "attack": [true, true] }
	var fallback_entries := InputBindings.resolve_touch(
		InputBindings.Layout.CONTEXT, [], fallback_states,
		{ "target_kind": InputBindings.TARGET_NONE, "interact_enabled": false })
	var fallback_primary := InputBindings.entry_for_role(fallback_entries, "primary")
	_check(bool(fallback_primary.get("visible", false)) and bool(fallback_primary.get("available", false)),
		"兜底：无通道表时按动作名匹配 states")


# ───────────────────────── 3. hint_text ─────────────────────────

func _test_hints() -> void:
	_ensure_hint_events()
	var context := { "target_kind": InputBindings.TARGET_NONE, "interact_enabled": false }
	_check(InputBindings.hint_text(UiState.Scheme.TOUCH, CHANNELS, _states(), context) == "",
		"hint：触屏方案返回空串")
	var text := InputBindings.hint_text(UiState.Scheme.KBM, CHANNELS, _states(), context)
	_check(text.contains("Attack"), "hint：含 Attack（实际 \"%s\"）" % text)
	_check(text.contains("LMB"), "hint：鼠标左键显示 LMB（实际 \"%s\"）" % text)
	_check(text.contains("Place"), "hint：含 Place（实际 \"%s\"）" % text)
	_check(text.contains("Jump"), "hint：含 Jump（实际 \"%s\"）" % text)
	_check(text.contains("Space"), "hint：跳跃显示 Space（实际 \"%s\"）" % text)
	_check(text.contains("[") and text.contains("]"), "hint：按键名带方括号")
	var block_text := InputBindings.hint_text(UiState.Scheme.KBM, CHANNELS, _states(),
		{ "target_kind": InputBindings.TARGET_BLOCK, "interact_enabled": false })
	_check(block_text.contains("Break"), "hint+BLOCK：主提示变 Break（实际 \"%s\"）" % block_text)


# ───────────────────────── 4. remap_actions ─────────────────────────

func _test_remap() -> void:
	var context := InputBindings.TARGET_NONE
	var remapped := InputBindings.remap_actions(
		["attack", "jump"], InputBindings.Layout.CONTEXT, CHANNELS, _states(), context)
	_check(remapped == ["attack", "jump"], "remap：target=NONE 保持 attack")
	var block := InputBindings.remap_actions(
		["attack", "jump"], InputBindings.Layout.CONTEXT, CHANNELS, _states(),
		InputBindings.TARGET_BLOCK)
	_check(block == ["break_block", "jump"], "remap：target=BLOCK 时 attack -> break_block（实际 %s）" % str(block))
	var entity := InputBindings.remap_actions(
		["attack"], InputBindings.Layout.CONTEXT, CHANNELS, _states(),
		InputBindings.TARGET_ENTITY)
	_check(entity == ["attack"], "remap：target=ENTITY 保持 attack")
	var split := InputBindings.remap_actions(
		["attack"], InputBindings.Layout.SPLIT, CHANNELS, _states(),
		InputBindings.TARGET_BLOCK)
	_check(split == ["attack"], "remap：SPLIT 原样返回")
	# 去重：attack 与 break_block 同时按下时只提交一个 break_block。
	var dedupe := InputBindings.remap_actions(
		["attack", "break_block"], InputBindings.Layout.CONTEXT, CHANNELS, _states(),
		InputBindings.TARGET_BLOCK)
	_check(dedupe == ["break_block"], "remap：映射后去重（实际 %s）" % str(dedupe))
	# break_block 不可用时不代打。
	var stunned := _states()
	stunned["break_block"] = [true, false]
	var not_available := InputBindings.remap_actions(
		["attack"], InputBindings.Layout.CONTEXT, CHANNELS, stunned,
		InputBindings.TARGET_BLOCK)
	_check(not_available == ["attack"], "remap：break_block 不可用时保持 attack")


# ───────────────────────── 5. ActionCluster ─────────────────────────

func _test_cluster() -> void:
	var viewport := SubViewport.new()
	viewport.size = Vector2i(900, 620)
	root.add_child(viewport)
	await process_frame

	var cluster := ActionCluster.new()
	viewport.add_child(cluster)
	await process_frame
	cluster.set_left_handed(false)
	var context := { "target_kind": InputBindings.TARGET_NONE, "interact_enabled": false }
	cluster.apply_slots(InputBindings.resolve_touch(
		InputBindings.Layout.CONTEXT, CHANNELS, _states(), context))
	await process_frame
	await process_frame

	var rects := cluster.slot_rects()
	_check(rects.has("primary"), "cluster：primary 已排布")
	_check(rects.has("secondary") and rects.has("jump"), "cluster：secondary / jump 已排布")
	var main_button := cluster.button_for("primary")
	_check(main_button != null and main_button.visible, "cluster：主按钮已创建且可见")
	_check(cluster.buttons().size() == 5,
		"cluster：可见按钮 5 个（interact 隐藏，实际 %d）" % cluster.buttons().size())
	var cluster_size := cluster.size
	_check(cluster_size.x > 0.0 and cluster_size.y > 0.0, "cluster：尺寸非零")
	var main_rect: Rect2 = rects["primary"]
	_check(absf(main_rect.end.x - (cluster_size.x - ActionCluster.PADDING)) <= TOL,
		"cluster：主按钮贴簇右边（right=%.1f size=%.1f）" % [main_rect.end.x, cluster_size.x])
	_check(absf(main_rect.end.y - (cluster_size.y - ActionCluster.PADDING)) <= TOL,
		"cluster：主按钮贴簇下边")
	_check(main_button.position.is_equal_approx(main_rect.position),
		"cluster：主按钮实际位置与 slot_rects 一致")
	for role in rects:
		var rect: Rect2 = rects[role]
		_check(rect.position.x >= -TOL and rect.position.y >= -TOL
				and rect.end.x <= cluster_size.x + TOL and rect.end.y <= cluster_size.y + TOL,
			"cluster：%s 不越出簇" % str(role))

	# interact 出现：簇变大，但主按钮距簇右 / 下边距离不变（槽位 bottom_end 固定右/下边）。
	var right_margin := cluster_size.x - main_rect.end.x
	var bottom_margin := cluster_size.y - main_rect.end.y
	cluster.apply_slots(InputBindings.resolve_touch(
		InputBindings.Layout.CONTEXT, CHANNELS, _states(),
		{ "target_kind": InputBindings.TARGET_NONE, "interact_enabled": true }))
	await process_frame
	var grown := cluster.size
	var rects2 := cluster.slot_rects()
	_check(rects2.has("interact"), "cluster：interact_enabled 后 interact 按钮出现")
	var main_rect2: Rect2 = rects2["primary"]
	_check(grown.x > cluster_size.x + TOL or grown.y > cluster_size.y + TOL,
		"cluster：interact 出现后簇变大（%.1fx%.1f -> %.1fx%.1f）" % [cluster_size.x, cluster_size.y, grown.x, grown.y])
	_check(absf((grown.x - main_rect2.end.x) - right_margin) <= TOL,
		"cluster：interact 出现前后主按钮距右边不变")
	_check(absf((grown.y - main_rect2.end.y) - bottom_margin) <= TOL,
		"cluster：interact 出现前后主按钮距下边不变")

	# 左手：落 bottom_begin，主按钮钉左下角。
	cluster.set_left_handed(true)
	await process_frame
	var rects3 := cluster.slot_rects()
	var main_rect3: Rect2 = rects3["primary"]
	_check(cluster.align == HudWidget.Align.BEGIN, "cluster：左手模式改 align=BEGIN")
	_check(absf(main_rect3.position.x - ActionCluster.PADDING) <= TOL,
		"cluster：左手主按钮贴簇左边（left=%.1f）" % main_rect3.position.x)
	_check(absf(main_rect3.end.y - (cluster.size.y - ActionCluster.PADDING)) <= TOL,
		"cluster：左手主按钮仍贴簇下边")
	for role in rects3:
		var rect: Rect2 = rects3[role]
		_check(rect.position.x >= -TOL and rect.position.y >= -TOL
				and rect.end.x <= cluster.size.x + TOL and rect.end.y <= cluster.size.y + TOL,
			"cluster：左手模式 %s 不越出簇" % str(role))
	cluster.set_left_handed(false)

	# 无槽位（无 client / 无通道表）：按钮数 0，簇尺寸归零。
	cluster.apply_slots([])
	await process_frame
	_check(cluster.buttons().is_empty(), "cluster：无槽位时按钮数 0")
	_check(cluster.size.is_equal_approx(Vector2.ZERO), "cluster：无槽位时尺寸归零")
	cluster.queue_free()
	viewport.queue_free()
	await process_frame


# ───────────────────────── 6. ActionButton ─────────────────────────

func _test_button() -> void:
	var button := ActionButton.new()
	root.add_child(button)
	await process_frame
	button.set_action("jump")
	button.set_mode("button")
	button.apply_size(72.0)
	button.set_opacity(0.5)
	_check(absf(button.size.x - 72.0) <= TOL, "button：apply_size 生效（%.1f）" % button.size.x)
	_check(absf(button.modulate.a - 0.5) <= 0.01, "button：空闲 alpha = opacity（%.2f）" % button.modulate.a)

	var press := InputEventScreenTouch.new()
	press.index = 0
	press.pressed = true
	button._gui_input(press)
	_check(Input.is_action_pressed("jump"), "button：按下注入动作")
	_check(button.is_holding(), "button：按下时 is_holding=true")
	_check(button.modulate.a > 0.5, "button：按下时变亮")

	var lift := InputEventScreenTouch.new()
	lift.index = 0
	lift.pressed = false
	button._gui_input(lift)
	_check(not Input.is_action_pressed("jump"), "button：抬起释放动作")
	_check(not button.is_holding(), "button：抬起后 is_holding=false")

	# 不可用：立即释放，但控件矩形不变。
	var press2 := InputEventScreenTouch.new()
	press2.index = 1
	press2.pressed = true
	button._gui_input(press2)
	_check(Input.is_action_pressed("jump"), "button：再次按下")
	button.set_available(false)
	_check(not Input.is_action_pressed("jump"), "button：不可用时立即释放动作")
	_check(not button.is_holding(), "button：不可用时 is_holding=false")
	_check(absf(button.size.x - 72.0) <= TOL, "button：不可用时矩形不变")
	button.set_available(true)

	# toggle 模式：按下后 0.1s 自动释放，形成一次完整边沿。
	button.set_mode("toggle")
	var press3 := InputEventScreenTouch.new()
	press3.index = 2
	press3.pressed = true
	button._gui_input(press3)
	_check(button.is_holding(), "button：toggle 按下后保持")
	await create_timer(0.25).timeout
	_check(not button.is_holding(), "button：toggle 模式自动释放")
	_check(not Input.is_action_pressed("jump"), "button：toggle 自动释放动作")

	# present=false 隐藏。
	button.set_channel_state(false, true)
	_check(not button.visible, "button：present=false 时隐藏")
	button.set_channel_state(true, true)
	_check(button.visible, "button：present=true 时恢复可见")
	button.release()
	button.queue_free()
	await process_frame


# ───────────────────────── 7. BevyInputContext ─────────────────────────

func _test_input_context() -> void:
	var stub := _StubClient.new()
	stub.channels_override = CHANNELS
	root.add_child(stub)
	var context := BevyInputContext.new()
	var observed: Array = []
	context.interaction_target_changed.connect(
		func(target: int, enabled: bool) -> void: observed.append([target, enabled]))
	context.bind(stub)
	await process_frame
	_check(context.channels().size() == CHANNELS.size(), "context：通道表来自 client")

	var e1 := EntityData.new()
	e1.set_component(BevyEnums.Payload.INPUTCAPS, {
		"present": 0xFFFF, "available": 0xFFFF, "source": 1, "local": true, "look": true,
	})
	stub.emit_payload(BevyEnums.Payload.INPUTCAPS, e1)
	_check(context.controlled_entity() == e1, "context：INPUTCAPS 绑定受控实体")
	_check(context.interaction_target() == 0 and not context.interaction_enabled(),
		"context：初始交互目标为 NONE")

	var states := context.channel_states()
	_check(states.has("attack") and states["attack"] == [true, true],
		"context：channel_states 快照（attack 可用）")

	e1.set_component(BevyEnums.Payload.INTERACTION, { "action": 1, "enabled": true, "target": 2 })
	stub.emit_payload(BevyEnums.Payload.INTERACTION, e1)
	_check(context.interaction_target() == 2, "context：INTERACTION target=BLOCK 生效")
	_check(context.interaction_enabled(), "context：INTERACTION enabled 生效")
	var emitted := observed.size()
	stub.emit_payload(BevyEnums.Payload.INTERACTION, e1)
	_check(observed.size() == emitted, "context：同值 INTERACTION 去抖不重复发信号")

	# 受控实体切换且新实体没有交互组件：复位并发一次信号。
	var e2 := EntityData.new()
	e2.set_component(BevyEnums.Payload.INPUTCAPS, {
		"present": 0xFFFF, "available": 0xFFFF, "source": 2, "local": true, "look": true,
	})
	stub.emit_payload(BevyEnums.Payload.INPUTCAPS, e2)
	_check(context.controlled_entity() == e2, "context：切换受控实体")
	_check(context.interaction_target() == 0 and not context.interaction_enabled(),
		"context：实体切换后交互目标复位")
	_check(observed.size() > emitted, "context：复位时发一次信号")

	context.reset()
	_check(not context.interaction_enabled(), "context：reset 后交互不可用")
	root.remove_child(stub)
	stub.queue_free()
	await process_frame


# ───────────────────────── 8. BevyInput 提交前重映射 ─────────────────────────

func _test_bevy_input() -> void:
	# 动作表 / 提交替身：只验证「提交前按交互目标重映射」这一层。
	var manager := _StubInputManager.new()
	var bevy_input := BevyInput.new()
	bevy_input.setup(manager, 1)
	var stub := _StubClient.new()
	stub.channels_override = CHANNELS
	root.add_child(stub)
	var context := BevyInputContext.new()
	context.bind(stub)
	var entity := EntityData.new()
	entity.set_component(BevyEnums.Payload.INPUTCAPS, {
		"present": 0xFFFF, "available": 0xFFFF, "source": 1, "local": true, "look": true,
	})
	stub.emit_payload(BevyEnums.Payload.INPUTCAPS, entity)
	bevy_input.set_input_context(context)

	# target=NONE：attack 原样提交（按下与采集在同一帧，just_pressed 才有效）。
	Input.action_press("attack")
	bevy_input.collect_and_submit()
	var frame: Dictionary = manager.frames.back() if not manager.frames.is_empty() else {}
	_check(_array_has(frame.get("pressed", []), "attack"), "bevy_input：target=NONE 时提交 attack")

	# target=BLOCK：提交前把 attack 改报 break_block（held / pressed / released 一致）。
	entity.set_component(BevyEnums.Payload.INTERACTION, { "action": 1, "enabled": true, "target": 2 })
	stub.emit_payload(BevyEnums.Payload.INTERACTION, entity)
	bevy_input.collect_and_submit()
	frame = manager.frames.back() if not manager.frames.is_empty() else {}
	_check(_array_has(frame.get("held", []), "break_block"),
		"bevy_input：target=BLOCK 时 held 提交 break_block")
	_check(not _array_has(frame.get("held", []), "attack"),
		"bevy_input：target=BLOCK 时不再提交 attack")
	Input.action_release("attack")
	bevy_input.collect_and_submit()
	frame = manager.frames.back() if not manager.frames.is_empty() else {}
	_check(_array_has(frame.get("released", []), "break_block"),
		"bevy_input：target=BLOCK 时 released 提交 break_block")

	# 无 context：原样提交，不做重映射。
	bevy_input.set_input_context(null)
	Input.action_press("attack")
	bevy_input.collect_and_submit()
	frame = manager.frames.back() if not manager.frames.is_empty() else {}
	_check(_array_has(frame.get("pressed", []), "attack"), "bevy_input：无 context 时原样提交")
	Input.action_release("attack")
	root.remove_child(stub)
	stub.queue_free()
	await process_frame


func _array_has(values, action: String) -> bool:
	for value in values:
		if String(value) == action:
			return true
	return false


# ───────────────────────── 工具 ─────────────────────────

func _states() -> Dictionary:
	var out := {}
	for channel in CHANNELS:
		out[String(channel["key"])] = [true, true]
	return out


func _ensure_actions() -> void:
	for action in ALL_ACTIONS:
		if not InputMap.has_action(action):
			InputMap.add_action(action)


func _ensure_hint_events() -> void:
	_add_key_event("jump", KEY_SPACE)
	_add_mouse_event("attack", MOUSE_BUTTON_LEFT)
	_add_mouse_event("place_block", MOUSE_BUTTON_RIGHT)
	_add_mouse_event("break_block", MOUSE_BUTTON_MIDDLE)


func _add_key_event(action: String, code: Key) -> void:
	if not InputMap.has_action(action):
		InputMap.add_action(action)
	if not InputMap.action_get_events(action).is_empty():
		return
	var event := InputEventKey.new()
	event.physical_keycode = code
	InputMap.action_add_event(action, event)


func _add_mouse_event(action: String, index: int) -> void:
	if not InputMap.has_action(action):
		InputMap.add_action(action)
	if not InputMap.action_get_events(action).is_empty():
		return
	var event := InputEventMouseButton.new()
	event.button_index = index
	InputMap.action_add_event(action, event)


func _release_actions() -> void:
	for action in ALL_ACTIONS:
		if InputMap.has_action(action):
			Input.action_release(action)


func _check(condition: bool, message: String) -> void:
	_checks += 1
	if condition:
		print("[input_bindings] PASS  " + message)
	else:
		_failures.append(message)
		print("[input_bindings] FAIL  " + message)


func _finish() -> void:
	print("[input_bindings] checks=%d failures=%d" % [_checks, _failures.size()])
	if _failures.is_empty():
		print("INPUT_BINDINGS PASS")
		quit(0)
	else:
		for failure in _failures:
			print("  - " + failure)
		print("INPUT_BINDINGS FAIL")
		quit(1)


## 只记录 listen_component 回调的最小 client 替身。
class _StubClient:
	extends Node

	signal entity_removed(entity, reason)

	var channels_override: Array = []
	var _callbacks: Dictionary = {}

	func input_channels() -> Array:
		return channels_override

	func listen_component(kind: int, callback: Callable) -> void:
		_callbacks[kind] = callback

	func unlisten_component(kind: int, _callback: Callable) -> void:
		_callbacks.erase(kind)

	func emit_payload(kind: int, entity) -> void:
		if _callbacks.has(kind):
			var callback: Callable = _callbacks[kind]
			callback.call(entity)


## 输入提交替身：记录 submit_input_frame 的参数。
class _StubInputManager:
	extends RefCounted

	var frames: Array = []

	func get_action_table() -> Array:
		return [
			{ "name": "move_forward", "bit": 0 },
			{ "name": "move_back", "bit": 1 },
			{ "name": "move_left", "bit": 2 },
			{ "name": "move_right", "bit": 3 },
			{ "name": "jump", "bit": 4 },
			{ "name": "attack", "bit": 8 },
			{ "name": "break_block", "bit": 9 },
			{ "name": "place_block", "bit": 14 },
		]

	func submit_input_frame(held, pressed, released, move, look_delta, timestamp, source_id) -> void:
		frames.append({
			"held": held,
			"pressed": pressed,
			"released": released,
			"move": move,
			"look_delta": look_delta,
			"timestamp": timestamp,
			"source_id": source_id,
		})
