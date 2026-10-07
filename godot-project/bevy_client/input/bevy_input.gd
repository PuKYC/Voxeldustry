class_name BevyInput
extends RefCounted
## 输入采集门面（Godot -> game-core）。
##
## 自动从 game-core 拉取动作表并注册 / 校验 Godot 的 InputMap，
## 业务侧只面对「语义动作名」，不再手拼 held/pressed/released。
##
## 铁律：这里只采集物理输入，不做能力过滤、不做玩法语义判断。

signal action_pressed(action: String)
signal action_released(action: String)

## 物理绑定令牌 -> Godot 物理键。
##
## 「哪个物理键」是 Godot 自己的配置：game-core 只给「动作名 -> 通道位」，
## 不含按键（改键不影响逻辑）。这里的表只用于首次注册动作时的默认绑定；
## 已存在的动作不覆盖，改键请直接在 Project Settings → Input Map 里改。
const KEY_BY_BINDING := {
	"W": KEY_W,
	"A": KEY_A,
	"S": KEY_S,
	"D": KEY_D,
	"Space": KEY_SPACE,
	"Shift": KEY_SHIFT,
	"F": KEY_F,
	"E": KEY_E,
	"Ctrl": KEY_CTRL,
	"C": KEY_C,
	"R": KEY_R,
	"Q": KEY_Q,
}

const MOUSE_BY_BINDING := {
	"MouseLeft": MOUSE_BUTTON_LEFT,
	"MouseRight": MOUSE_BUTTON_RIGHT,
}

## 动作名 -> 首次注册时的默认绑定令牌（game-core 的动作表只有 name / bit）。
## 表里没有的动作（例如 mod 新增）只注册动作名，请在编辑器里手动绑定。
const DEFAULT_BINDING_BY_ACTION := {
	"move_forward": "W",
	"move_back": "S",
	"move_left": "A",
	"move_right": "D",
	"jump": "Space",
	"glide": "Shift",
	"flight_toggle": "F",
	"interact": "E",
	"primary_tool": "MouseLeft",
	"secondary_tool": "MouseRight",
	"sprint": "Ctrl",
	"crouch": "C",
	"reload": "R",
	"drop_item": "Q",
}

## 本帧原始轴值（死区/曲线在 game-core，Godot 传原始值）。
var move: Vector2 = Vector2.ZERO
## 本帧累计的鼠标像素位移。
var look_delta: Vector2 = Vector2.ZERO
## 输入源 ID；单人固定为 1。
var source_id: int = 1

# _manager 不能标注成 Node —— 扩展方法在 Node 上不存在。
var _manager = null
var _action_names := PackedStringArray()
var _held := {}
var _pressed := {}
var _released := {}
var _look_accum := Vector2.ZERO


func setup(manager, p_source_id: int = 1) -> void:
	_manager = manager
	source_id = p_source_id
	_register_actions()


## 由 BevyClient 在 _input 中转发。
func on_mouse_motion(relative: Vector2) -> void:
	_look_accum += relative


func is_held(action: String) -> bool:
	return _held.has(action)


func just_pressed(action: String) -> bool:
	return _pressed.has(action)


func just_released(action: String) -> bool:
	return _released.has(action)


func action_names() -> PackedStringArray:
	return _action_names


## 采集一帧并提交给后台 Bevy；由 BevyClient 在 _process 中每帧调用一次。
func collect_and_submit() -> void:
	if _manager == null:
		return

	_held.clear()
	_pressed.clear()
	_released.clear()

	for action_name in _action_names:
		if Input.is_action_pressed(action_name):
			_held[action_name] = true
		if Input.is_action_just_pressed(action_name):
			_pressed[action_name] = true
			action_pressed.emit(action_name)
		if Input.is_action_just_released(action_name):
			_released[action_name] = true
			action_released.emit(action_name)

	# deadzone 传 0：死区/曲线是 game-core 的职责，避免两层死区叠加。
	move = Input.get_vector("move_left", "move_right", "move_back", "move_forward", 0.0)
	look_delta = _look_accum
	_look_accum = Vector2.ZERO

	_manager.submit_input_frame(
		PackedStringArray(_held.keys()),
		PackedStringArray(_pressed.keys()),
		PackedStringArray(_released.keys()),
		move,
		look_delta,
		Time.get_ticks_msec(),
		source_id
	)


## 用 game-core 的动作表注册 / 校验 InputMap（已存在的动作不覆盖，尊重改键）。
func _register_actions() -> void:
	var table: Array = _manager.get_action_table()
	_action_names.clear()
	for row in table:
		var action_name: String = row["name"]
		_action_names.append(action_name)
		if InputMap.has_action(action_name):
			continue
		InputMap.add_action(action_name)
		var binding: String = String(DEFAULT_BINDING_BY_ACTION.get(action_name, ""))
		if binding.is_empty():
			push_warning(
				"[BevyInput] 动作「%s」没有默认绑定，请在 Project Settings → Input Map 里手动绑定"
				% action_name
			)
			continue
		var event: InputEvent = _make_event(binding)
		if event == null:
			push_warning("[BevyInput] 未知绑定令牌「%s」（动作 %s）" % [binding, action_name])
			continue
		InputMap.action_add_event(action_name, event)


func _make_event(binding: String) -> InputEvent:
	if KEY_BY_BINDING.has(binding):
		var key := InputEventKey.new()
		key.physical_keycode = KEY_BY_BINDING[binding]
		return key
	if MOUSE_BY_BINDING.has(binding):
		var button := InputEventMouseButton.new()
		button.button_index = MOUSE_BY_BINDING[binding]
		return button
	return null
