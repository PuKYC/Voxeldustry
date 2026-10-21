extends SceneTree
## 无编辑器环境下的全局类名缓存生成器。
##
## 背景：Godot 只在编辑器导入时写 .godot/global_script_class_cache.cfg；
## 本机没有可用编辑器（headless 编辑器会崩），所以用本脚本扫描 class_name，
## 生成运行时所需的缓存。生成后 --script / 场景运行才能解析 HudWidget 等类名。
##
## 运行：
##   godot --headless --path godot-project --script res://test/gen_class_cache.gd
##
## 该文件由 tools/ui_check.sh 自动调用，不需要提交 .godot/。

const PROJECT_DIR := "res://"


func _initialize() -> void:
	var root_dir := ProjectSettings.globalize_path(PROJECT_DIR).trim_suffix("/")
	var classes: Array[Dictionary] = []
	_scan(root_dir, root_dir, classes)
	classes.sort_custom(func(a, b): return String(a["class"]) < String(b["class"]))
	var cache := ConfigFile.new()
	cache.set_value("", "list", classes)
	DirAccess.make_dir_recursive_absolute(root_dir + "/.godot")
	var path := root_dir + "/.godot/global_script_class_cache.cfg"
	var err := cache.save(path)
	print("CLASS_CACHE classes=%d err=%d path=%s" % [classes.size(), err, path])

	# 无编辑器时 GDExtension 也不会自动启用：手动写 extension_list.cfg，
	# 否则场景里的 BevyAppManager 会退化成 Node（所有扩展方法都不存在）。
	var extension_path := root_dir + "/.godot/extension_list.cfg"
	if FileAccess.file_exists(root_dir + "/main.gdextension"):
		var extension_file := FileAccess.open(extension_path, FileAccess.WRITE)
		if extension_file != null:
			extension_file.store_line("res://main.gdextension")
			extension_file.close()
			print("EXTENSION_LIST %s" % extension_path)

	quit(0 if err == OK else 1)


func _scan(root_dir: String, dir: String, out: Array[Dictionary]) -> void:
	var handle := DirAccess.open(dir)
	if handle == null:
		return
	handle.list_dir_begin()
	while true:
		var entry := handle.get_next()
		if entry == "":
			break
		if entry.begins_with("."):
			continue
		var full := dir.path_join(entry)
		if handle.current_is_dir():
			_scan(root_dir, full, out)
		elif entry.ends_with(".gd"):
			var text := FileAccess.get_file_as_string(full)
			var class_value := _extract(text, "class_name")
			if class_value.is_empty():
				continue
			var base := _extract(text, "extends")
			if base.is_empty():
				base = "RefCounted"
			out.append({
				"class": StringName(class_value),
				"language": StringName("GDScript"),
				"path": "res://" + full.trim_prefix(root_dir + "/"),
				"base": StringName(base),
				"icon": "",
				"is_abstract": false,
				"is_tool": false,
			})
	handle.list_dir_end()


func _extract(text: String, key: String) -> String:
	var re := RegEx.new()
	re.compile("(?m)^" + key + "\\s+(\\w+)")
	var match := re.search(text)
	if match == null:
		return ""
	return match.get_string(1)
