class_name StaticTables
extends RefCounted
## 启动时一次性拉取静态表；只做 id <-> 名字 / 定义搬运，不做业务判断。
##
## 物品表保留旧门面的双向映射（item_id -> row / name -> item_id），
## 语义表支持 mod 覆盖与运行时注册（写本地表，可选回写 game-core）。

var prototype_name: Dictionary[int, String] = {}
var item_by_id: Dictionary[int, Dictionary] = {}
var item_name_to_id: Dictionary[String, int] = {}
var semantic_name: Dictionary[String, Dictionary] = {}
var action_bit: Dictionary[String, int] = {}

# _mgr 不能标注成 Node —— 扩展方法在 Node 上不存在。
var _mgr = null


func _init(mgr: Object) -> void:
	_mgr = mgr

	for row: Dictionary in mgr.get_prototype_table():
		prototype_name[int(row["prototype_id"])] = String(row["name"])

	var rows: Array = []
	if mgr.has_method("get_item_tables"):
		var item_tables: Dictionary = mgr.get_item_tables()
		rows = item_tables.get("items", [])
	elif mgr.has_method("get_item_table"):
		rows = mgr.get_item_table()
	for row: Dictionary in rows:
		var item_id := int(row["item_id"])
		item_by_id[item_id] = row
		item_name_to_id[String(row["name"])] = item_id

	_load_semantics(mgr)

	for action_row: Dictionary in mgr.get_action_table():
		action_bit[String(action_row["name"])] = int(action_row["bit"])


func _load_semantics(mgr: Object) -> void:
	if not mgr.has_method("get_semantic_tables"):
		push_warning("[StaticTables] 扩展未提供 get_semantic_tables()，语义名将退化为 #id")
		return
	var sem: Dictionary = mgr.get_semantic_tables()
	for domain in sem:
		var m: Dictionary = {}
		for row: Dictionary in sem[domain]:
			m[int(row["id"])] = String(row["name"])
		semantic_name[String(domain)] = m


## 合并 mod 语义覆盖：domain -> { id: name }。
func apply_overrides(overrides: Dictionary) -> void:
	for domain in overrides:
		var m: Dictionary = semantic_name.get(String(domain), {})
		var override: Dictionary = overrides[domain]
		for key in override:
			m[int(key)] = String(override[key])
		semantic_name[String(domain)] = m


## 运行时注册 / 覆盖一条语义名，并（若扩展支持）回写 game-core 使其对 Bevy 可见。
## 返回 true 表示新增运行时条目，false 表示覆盖已有条目。
func register_semantic(domain: String, id: int, name: String) -> bool:
	var created := true
	if _mgr != null and _mgr.has_method("register_semantic"):
		created = bool(_mgr.register_semantic(domain, id, name))
	var m: Dictionary = semantic_name.get(domain, {})
	m[id] = name
	semantic_name[domain] = m
	return created


## 用名字哈希注册（推荐）：返回生成的 ID（>= 10000，落在 mod hashed 分区）。
func register_semantic_named(domain: String, namespaced_name: String) -> int:
	var id := 0
	if _mgr != null and _mgr.has_method("register_semantic_named"):
		id = int(_mgr.register_semantic_named(domain, namespaced_name))
	elif _mgr != null and _mgr.has_method("mod_semantic_id"):
		id = int(_mgr.mod_semantic_id(namespaced_name))
	var m: Dictionary = semantic_name.get(domain, {})
	m[id] = namespaced_name
	semantic_name[domain] = m
	return id


## id -> 语义名；未知回退到 "#<id>"（前向兼容：老客户端遇到新 id 不崩）。
func name_of(domain: String, id: int) -> String:
	var m: Dictionary = semantic_name.get(domain, {})
	if m.has(id):
		return String(m[id])
	return "#%d" % id


## item_id -> 物品静态定义行；未知返回空 Dictionary。
func item_def(item_id: int) -> Dictionary:
	return item_by_id.get(item_id, {})


## item_id -> 物品名；未知回退到 "#<id>"。
func item_name_of(item_id: int) -> String:
	var row: Dictionary = item_by_id.get(item_id, {})
	if row.is_empty():
		return "#%d" % item_id
	return String(row["name"])


## 物品名 -> item_id；未知返回 0。
func item_id_of(item_name: String) -> int:
	return int(item_name_to_id.get(item_name, 0))
