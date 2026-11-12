extends SceneTree
## ParamPanel 的静态自检脚本（本机没有 godot / gdlint，未实际运行）。
##
## 用假 schema 构造面板，覆盖：控件数量与类型、collect_values、set_value、
## reset_defaults、visible_if 隐藏（含跨 mod 同名 local key）、失败 schema 禁用
## Apply、apply_to 成功 / 后端拒绝 / manager 缺失。
##
## 运行方式（有 Godot 的机器）：
##   godot --headless --path godot-project -s test/param_panel_check.gd

const PARAM_PANEL := preload("res://ui/screens/param_panel.gd")

var _failures: Array[String] = []
var _checks := 0
var _close_count := 0


class FakeManager:
	extends Node

	var received: Dictionary = {}
	var next_ok := true

	func set_param_overrides(values: Dictionary) -> Dictionary:
		received = values
		if not next_ok:
			return {
				"ok": false,
				"count": 0,
				"errors": [{"code": "param.out_of_range", "message": "测试拒绝"}],
			}
		return {"ok": true, "count": values.size(), "errors": []}


func _initialize() -> void:
	var panel = PARAM_PANEL.new()
	var manager := FakeManager.new()

	# 假 schema：bool / int / fixed / enum / visible_if / 分组 / 世界作用域，
	# 以及另一个 mod 的同名 local key（enable），用来验证不会跨 mod 误判。
	var schema := [
		{
			"key": "test:alpha.enable", "mod_id": "test:alpha", "local_key": "enable",
			"scope": "world", "kind": "bool", "default": 1,
			"min": -1, "max": -1, "step": -1, "scale": -1,
			"values": PackedStringArray(), "group": "常规", "visible_if": {},
		},
		{
			"key": "test:alpha.count", "mod_id": "test:alpha", "local_key": "count",
			"scope": "world", "kind": "int", "default": 3,
			"min": 0, "max": 10, "step": 1, "scale": -1,
			"values": PackedStringArray(), "group": "常规",
			"visible_if": {"key": "enable", "equals": 1},
		},
		{
			"key": "test:alpha.ratio", "mod_id": "test:alpha", "local_key": "ratio",
			"scope": "debug", "kind": "fixed", "default": 50,
			"min": 0, "max": 100, "step": 5, "scale": 100,
			"values": PackedStringArray(), "group": "画面", "visible_if": {},
		},
		{
			"key": "test:alpha.mode", "mod_id": "test:alpha", "local_key": "mode",
			"scope": "debug", "kind": "enum", "default": 1,
			"min": -1, "max": -1, "step": -1, "scale": -1,
			"values": PackedStringArray(["低", "中", "高"]), "group": "画面",
			"visible_if": {},
		},
		{
			"key": "test:alpha.hidden", "mod_id": "test:alpha", "local_key": "hidden",
			"scope": "debug", "kind": "int", "default": 0,
			"min": 0, "max": 5, "step": 1, "scale": -1,
			"values": PackedStringArray(), "group": "画面",
			"visible_if": {"key": "mode", "equals": 2},
		},
		{
			"key": "test:beta.enable", "mod_id": "test:beta", "local_key": "enable",
			"scope": "world", "kind": "bool", "default": 0,
			"min": -1, "max": -1, "step": -1, "scale": -1,
			"values": PackedStringArray(), "group": "常规", "visible_if": {},
		},
	]

	panel.load_schema({"ok": true, "schema": schema, "errors": []})

	_check(panel.schema_entries().size() == 6, "schema_entries 应为 6")
	_check(panel.rows().size() == 6, "rows 应为 6")
	_check(panel.apply_button().disabled == false, "成功 schema 应启用 Apply")
	_check(panel.close_button() != null, "应有 Close 按钮")
	panel.close_requested.connect(_on_panel_closed)
	panel.close_button().pressed.emit()
	_check(_close_count == 1, "按 Close 应发 close_requested")

	var rows: Dictionary = panel.rows()
	_check(rows["test:alpha.enable"]["control"] is CheckButton, "bool -> CheckButton")
	_check(rows["test:alpha.count"]["control"] is SpinBox, "int -> SpinBox")
	_check(rows["test:alpha.ratio"]["control"] is HSlider, "fixed -> HSlider")
	_check(rows["test:alpha.mode"]["control"] is OptionButton, "enum -> OptionButton")
	_check(
		is_equal_approx(float(rows["test:alpha.ratio"]["value_label"].text), 0.5),
		"fixed 默认值应显示 0.5（实际 %s）" % rows["test:alpha.ratio"]["value_label"].text
	)

	var values: Dictionary = panel.collect_values()
	_check(int(values.get("test:alpha.enable", -1)) == 1, "bool 默认 1")
	_check(int(values.get("test:alpha.count", -1)) == 3, "int 默认 3")
	_check(int(values.get("test:alpha.ratio", -1)) == 50, "fixed 默认 50")
	_check(int(values.get("test:alpha.mode", -1)) == 1, "enum 默认下标 1")
	_check(int(values.get("test:alpha.hidden", -1)) == 0, "int 隐藏行默认 0")

	# set_value 走控件。
	panel.set_value("test:alpha.count", 7)
	panel.set_value("test:alpha.ratio", 25)
	panel.set_value("test:alpha.mode", 2)
	values = panel.collect_values()
	_check(int(values["test:alpha.count"]) == 7, "set_value int")
	_check(int(values["test:alpha.ratio"]) == 25, "set_value fixed")
	_check(int(values["test:alpha.mode"]) == 2, "set_value enum")
	_check(
		is_equal_approx(float(panel.rows()["test:alpha.ratio"]["value_label"].text), 0.25),
		"fixed 显示值应随 set_value 更新"
	)

	# visible_if：mode==2 时 hidden 行可见；改回 1 后隐藏。
	_check(panel.rows()["test:alpha.hidden"]["row"].visible == true, "mode=2 时 hidden 可见")
	panel.set_value("test:alpha.mode", 1)
	_check(panel.rows()["test:alpha.hidden"]["row"].visible == false, "mode=1 时 hidden 隐藏")

	# visible_if 用同一 mod 的 local key：alpha.enable=0 隐藏 count，
	# 此时把 beta.enable 改 1 不应影响 alpha.count。
	panel.set_value("test:alpha.enable", 0)
	_check(panel.rows()["test:alpha.count"]["row"].visible == false, "alpha.enable=0 隐藏 count")
	panel.set_value("test:beta.enable", 1)
	_check(panel.rows()["test:alpha.count"]["row"].visible == false, "跨 mod 同名 key 不误判")

	# reset_defaults 回到假 schema 的默认值。
	panel.reset_defaults()
	values = panel.collect_values()
	_check(int(values["test:alpha.enable"]) == 1, "reset bool")
	_check(int(values["test:alpha.count"]) == 3, "reset int")
	_check(int(values["test:alpha.ratio"]) == 50, "reset fixed")
	_check(int(values["test:alpha.mode"]) == 1, "reset enum")
	_check(panel.rows()["test:alpha.count"]["row"].visible == true, "reset 后 count 重新可见")

	# apply_to：成功时把 collect_values 原样交给 manager。
	_check(panel.apply_to(manager) == true, "apply_to 成功应返回 true")
	_check(manager.received.size() == 6, "manager 应收到 6 项")
	_check(int(manager.received.get("test:alpha.ratio", -1)) == 50, "manager 收到 fixed 落盘整数")

	# 后端拒绝：返回 false，且不应抛异常。
	manager.next_ok = false
	_check(panel.apply_to(manager) == false, "后端 ok=false 时 apply_to 返回 false")

	# manager 为空：返回 false（push_error 会打印，属预期）。
	_check(panel.apply_to(null) == false, "manager 为空时 apply_to 返回 false")

	# 失败 schema：清空行、显示错误、禁用 Apply。
	panel.load_schema({
		"ok": false,
		"schema": [],
		"errors": [{"code": "param.duplicate_key", "message": "重复 key"}],
	})
	_check(panel.rows().is_empty(), "失败 schema 应清空 rows")
	_check(panel.schema_entries().is_empty(), "失败 schema 应清空 schema_entries")
	_check(panel.apply_button().disabled == true, "失败 schema 应禁用 Apply")

	manager.free()
	panel.free()

	if _failures.is_empty():
		print("param_panel_check: %d 项断言全部通过" % _checks)
		quit(0)
	else:
		for failure in _failures:
			push_error("[param_panel_check] " + failure)
		quit(1)


func _on_panel_closed() -> void:
	_close_count += 1


func _check(condition: bool, message: String) -> void:
	_checks += 1
	if not condition:
		_failures.append(message)
