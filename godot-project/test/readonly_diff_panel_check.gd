extends SceneTree
## ReadOnlyDiffPanel 的静态自检脚本（本机没有 godot / gdlint，未实际运行）。
##
## 用假 diff 字典构造面板，覆盖：空差异文案、四类差异分区与行数、
## 未知字段忽略、Close 信号。数据形状与 get_world_load_diff() 保持一致。
##
## 运行方式（有 Godot 的机器）：
##   godot --headless --path godot-project -s test/readonly_diff_panel_check.gd

const PANEL := preload("res://ui/screens/readonly_diff_panel.gd")

var _failures: Array[String] = []
var _checks := 0
var _close_count := 0
var _continue_count := 0


func _initialize() -> void:
	var panel = PANEL.new()
	panel.continue_requested.connect(_on_panel_continued)
	panel.close_requested.connect(_on_panel_closed)

	# 1) 空差异：不建分区，摘要说明一致。
	panel.load_diff({})
	_check(panel.section_count() == 0, "空差异不应有分区")
	_check(panel.summary_text().contains("一致"), "空差异摘要应说明一致，实际：%s" % panel.summary_text())

	# 2) 只有 mod 差异：missing / extra / version 三种。
	panel.load_diff({
		"mods": [
			{"kind": "missing", "id": "test:gone", "saved_version": 3, "current_version": 0},
			{"kind": "extra", "id": "test:new", "saved_version": 0, "current_version": 1},
			{"kind": "version", "id": "test:old", "saved_version": 2, "current_version": 5},
		],
	})
	_check(panel.section_count() == 1, "只有 mod 差异应有 1 个分区")
	_check(panel.row_count("mods") == 3, "mod 差异应有 3 行")
	_check(panel.row_count("plugins") == 0, "无插件差异时 plugins 分区不应出现")

	# 3) 插件差异：missing / extra / 顺序变化（顺序变化带 saved/current 两行）。
	panel.load_diff({
		"plugins": {
			"missing": ["core.world", "core.terrain"],
			"extra": ["core.streaming"],
			"order_changed": true,
			"saved": ["core.world", "core.terrain"],
			"current": ["core.terrain", "core.world"],
		},
	})
	_check(panel.section_count() == 1, "只有插件差异应有 1 个分区")
	_check(panel.row_count("plugins") == 5, "插件差异应有 2 missing + 1 extra + 2 顺序行")

	# 4) 动作差异：missing / extra / changed 三种，各一行。
	panel.load_diff({
		"actions": [
			{"kind": "missing", "name": "legacy", "saved_action": 900, "saved_channel_bit": 40, "current_action": 0, "current_channel_bit": 0},
			{"kind": "extra", "name": "interact", "saved_action": 0, "saved_channel_bit": 0, "current_action": 15, "current_channel_bit": 14},
			{"kind": "changed", "name": "jump", "saved_action": 105, "saved_channel_bit": 5, "current_action": 5, "current_channel_bit": 5},
		],
	})
	_check(panel.section_count() == 1, "只有动作差异应有 1 个分区")
	_check(panel.row_count("actions") == 3, "动作差异应有 3 行")

	# 5) 参数差异：missing / extra / changed 三种。
	panel.load_diff({
		"params": [
			{"kind": "missing", "key": "test:gone.flag", "saved_value": 1, "current_value": 0, "saved_ty": 0, "current_ty": 0, "saved_scale": 0, "current_scale": 0},
			{"kind": "extra", "key": "test:new.flag", "saved_value": 0, "current_value": 1, "saved_ty": 0, "current_ty": 0, "saved_scale": 0, "current_scale": 0},
			{"kind": "changed", "key": "test:old.flag", "saved_value": 1, "current_value": 0, "saved_ty": 0, "current_ty": 0, "saved_scale": 0, "current_scale": 0},
		],
	})
	_check(panel.section_count() == 1, "只有参数差异应有 1 个分区")
	_check(panel.row_count("params") == 3, "参数差异应有 3 行")

	# 6) 四类同时存在：四个分区，摘要计数覆盖各类。
	panel.load_diff({
		"mods": [{"kind": "version", "id": "test:old", "saved_version": 2, "current_version": 5}],
		"plugins": {"missing": ["core.world"], "extra": [], "order_changed": false, "saved": [], "current": []},
		"actions": [{"kind": "extra", "name": "interact", "saved_action": 0, "saved_channel_bit": 0, "current_action": 15, "current_channel_bit": 14}],
		"params": [{"kind": "changed", "key": "test:old.flag", "saved_value": 1, "current_value": 0, "saved_ty": 0, "current_ty": 0, "saved_scale": 0, "current_scale": 0}],
		"unknown": "ignored",
	})
	_check(panel.section_count() == 4, "四类差异应有 4 个分区")
	_check(panel.row_count("mods") == 1, "mod 行数")
	_check(panel.row_count("plugins") == 1, "插件行数")
	_check(panel.row_count("actions") == 1, "动作行数")
	_check(panel.row_count("params") == 1, "参数行数")
	var summary := panel.summary_text()
	_check(summary.contains("1 项 mod") and summary.contains("1 项插件") and summary.contains("1 条动作") and summary.contains("1 个参数"), "摘要计数应覆盖四类，实际：%s" % summary)

	# 7) 重新 load 会清空旧分区，不累积。
	panel.load_diff({})
	_check(panel.section_count() == 0, "重新 load 空差异应清空旧分区")

	# 8) 两个动作各发各的信号，面板自身不隐藏（由 UiRoot 决定）。
	_check(panel.continue_button() != null, "应有「以只读方式继续」按钮")
	_check(panel.close_button() != null, "应有 Back 按钮")
	panel.continue_button().pressed.emit()
	_check(_continue_count == 1, "Continue 应发一次 continue_requested")
	panel.close_button().pressed.emit()
	_check(_close_count == 1, "Back 应发一次 close_requested")

	_finish(panel)


func _on_panel_continued() -> void:
	_continue_count += 1


func _on_panel_closed() -> void:
	_close_count += 1


func _finish(panel) -> void:
	panel.free()
	if _failures.is_empty():
		print("readonly_diff_panel_check: %d checks passed" % _checks)
		quit(0)
	else:
		for failure in _failures:
			printerr("FAIL: %s" % failure)
		printerr("readonly_diff_panel_check: %d/%d checks failed" % [_failures.size(), _checks])
		quit(1)


func _check(condition: bool, message: String) -> void:
	_checks += 1
	if not condition:
		_failures.append(message)
