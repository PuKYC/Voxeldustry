---
title: 静态数据与 Mod 体系
type: 架构文档
status: active
created: 2026-10-19
updated: 2026-11-08
tags:
  - 项目架构
  - 静态数据
  - Mod
source: "代码阅读 @ commit 36cd288 + JSONC 数据层实现"
---

# 静态数据与 Mod 体系

## 静态数据的边界

`game-core/src/static_data` 是对象原型与物品定义的真相源：

| 模块 | 内容 |
| --- | --- |
| `item` | 物品定义、分类、子类、标签索引、质量/堆叠、`places` 放置目标 |
| `prototype` | 实体对象表、放置规格 `PlacementSpec` |
| `voxel` | 方块对象表、生物群系、体素边长、LOD 距离阈值 |
| `mod.rs` | JSONC 加载、校验、mod 补丁、重载与 `ObjectTables` |

规则：

- 只依赖 `game_engine::ids`、`game_engine::modding`（补丁层资源）、`game_engine::sim::EngineSet` 与 `game_engine::voxel`（`VoxelSet` 排序锚点、`VoxelSolidity` 碰撞 LUT）。不依赖 `presentation`，不依赖 `godot`。
- 数值一律用 `FixedPoint` 或整数表达，避免 f32 进入逻辑层。
- 表现层只消费只读快照：`godot-client-ext` 把表搬给 GDScript 的 `StaticTables`。
- 逻辑层消费运行期表：`Res<ObjectTables>`。编译期 const 表仍保留给世界调色板与启动前回退，新代码不得再扩展它们。

体素世界的关键常量：方块 id 0 到 7 依次是空气、石、土、草、沙、雪、木、叶；Rust 只导出 `block_names` 给 Godot 对账，材质外观与 block→面绑定都在 Godot 侧（见 [[06-体素世界与材质]]）。

## JSONC 数据层

### 文件

| 文件 | 根键 | 内容 |
| --- | --- | --- |
| `static_data/data/voxel_objects.jsonc` | `blocks` | 方块对象 id / 名字 / solid / replaceable / support / collision / 调色板 slot |
| `static_data/data/biomes.jsonc` | `biomes` | 生物群系高度参数与地表 / 地下 / 石层方块 |
| `static_data/data/entity_objects.jsonc` | `prototypes` | 实体原型 id / 名字 / version / placement |
| `static_data/data/item_placement.jsonc` | `items` | 物品完整定义 + `places`（放下后变成什么） |

基础层用 `include_str!` 嵌入二进制。启动与重载走同一条解析路径：

```text
基础层（嵌入或 user:// 覆盖文件）→ mod 层 1..N（load_order 拓扑序）
  → serde_json::Value merge-patch → 强类型反序列化（deny_unknown_fields）
  → validate → cross-reference → ObjectTables / ItemTagIndex
```

- 任一步失败，整体失败；启动失败 panic，重载失败保留旧表。
- 错误文本带来源（文件 / 表 / 条目名）：基础层形如 `voxel_objects.jsonc: 未知字段 "replacable"`，mod 补丁层形如 `mod mymod (voxel_objects.jsonc): 未知字段 "replacable"`；JSON 语法错误带行列号。
- 解析器不信任字段顺序：表按 id 严格升序校验，反查索引在加载后构建。

### 方块对象

```jsonc
{ "id": 3, "name": "grass", "solid": true, "replaceable": true,
  "support": true, "collision": true }
```

- 核心 id 0..=7 必须存在，且名字与 `BLOCK_TABLE` 一致。
- 非命名空间 id 上限 `CORE_ID_MAX`；mod 条目名必须是 `命名空间:名字`，id 必须等于 `mod_hash_id(name)`（>= 10000）。
- mod 方块在加载时分配 8..=255 的调色板 slot，可进入世界、参与拾取与碰撞；核心方块 slot 恒等于 id。槽位用尽时整表加载失败。
- 地形生成仍只使用核心方块与数据驱动生物群系，不自动生成 mod 方块。

### 实体对象与放置规格

```jsonc
{ "id": 30, "name": "torch_placed", "version": 1,
  "placement": {
    "anchor": "voxel",          // feet | center | voxel
    "footprint": [1, 1, 1],     // 体素，三轴 >= 1
    "offset": [0, 0, 0],        // 相对锚点，最多 ±1024
    "requires_support": true
  } }
```

Rust 代码里硬引用的核心实体 1（player）/ 2（drifter）/ 3（gated_drifter）必须保留，且名字不可改；契约登记在 `prototype.rs` 的 `CORE_ENTITIES`（只有 `(id, name)`），由 `EntityObjectDef::validate` 检查。`version` / `placement` 与 20..24、30 等其它实体只以 jsonc 为准；物品对实体的引用由 `ObjectTables::from_parts` 的跨表校验负责。导出给 Godot 的 `(id, name, version)` 来自运行期表，启动前回退到内嵌 jsonc。

### 生物群系

```jsonc
{ "id": 0, "name": "meadow", "base_height": 64, "amplitude": 8,
  "surface_block": "grass", "subsurface_block": "dirt", "stone_block": "stone" }
```

- 核心 id 0..=3 必须存在且名字与 `BIOME_TABLE` 一致。
- 方块字段按名字引用 `voxel_objects`，加载时解析成调色板 slot。
- 生成器每次生成块时读取当前形状表；热重载后新生成的块使用新参数，已生成块不自动重建。

### 物品与 places

`item_placement.jsonc` 保存完整 `ItemDef`，`places` 按稳定名字引用目标：

```jsonc
{ "kind": "entity", "name": "torch_placed" }   // 或 block / voxel_object
```

加载时解析成 `PlacedObject::{Block(u8), VoxelObject(u32), Entity(PrototypeId)}`。
`placeable` 标签与 `places` 必须同时存在。物品定义完全以 `item_placement.jsonc`（及其 mod 补丁层）为准：Rust 侧不保留核心物品 id 契约，运行期查不到的物品按未知物品安全降级。

每个物品用两个质量字段描述堆叠：

```jsonc
{
  "mass_grams": 100,            // 单件质量（克），供负重与堆叠计算
  "stack_capacity_grams": 6400  // 一叠能容纳的质量（克）
}
```

堆叠上限 = `stack_capacity_grams / mass_grams`（向下取整，至少 1）。
加载时校验 `mass_grams > 0`、容量不小于单件质量、算出的数量不超过 `u16`；
`stackable` 标签必须与算出的堆叠数量一致。Godot 物品表仍导出 `max_stack`
兼容键，同时新增 `mass_grams` 与 `stack_capacity_grams`。

## Mod 补丁

mod 通过 `ModContext::add_object_layer(src)` 登记补丁文本，`ObjectLayers` 资源按
`(load_order, mod_id)` 保存并在每次重载时重放，所以热重载不会丢 mod 修改。

```jsonc
{
  "version": 1,
  "table": "voxel_objects",       // 或 biomes / entity_objects / item_placement
  "patch": [
    { "name": "stone", "set": { "replaceable": true } },
    { "name": "mymod:glow_block", "add": { "solid": true } }
  ]
}
```

- `set`：按 `name` 定位条目，做 RFC 7396 merge-patch；字段值 `null` 删除。
- `add`：名字必须带命名空间；`id` 自动取 `mod_hash_id(name)`，并做碰撞检测。
- 禁止补丁改 `name` / `id`。
- 两个 mod 改同一叶子字段默认冲突；带 `priority` 时高者胜，低者字段被剪掉。
- 新增字段不需要改补丁代码；未知字段由 `deny_unknown_fields` 拒绝。

## 热重载

Rust 侧：

- `ReloadObjectTables { voxel_src, biome_src, entity_src, item_src }` 表示四张表的**完整覆盖文本**（不是补丁）。
- `ObjectTablesHandle` 是跨线程句柄：Godot 写 pending，Bevy 在 `FixedUpdate`（tick 边界）读取。`ReloadObjectTables` 消息与 handle pending 都是正式入口；同一 tick 内优先取最后一条消息。
- 成功：替换 `Res<ObjectTables>`，`generation += 1`，重建 `ItemTagIndex`，同步碰撞 LUT，发 `ObjectTablesReloaded`。
- 失败：保留旧表，错误写 `LastReloadError` 与 handle，Godot 可查询。
- 重载只发生在 tick 边界，排在 `EngineSet::Input` 与 `VoxelSet::Apply` 之前，同一 tick 的逻辑系统不会读到新旧混合的表。
- `ObjectTablesHandle` 带运行期开关 `reload_enabled`；默认 dev 构建开启、release 关闭，运行期可通过 Godot 设置。

Godot 侧（`godot-client-ext/src/bevy_runtime_node.rs`）：

- `reload_object_tables()`：读 `user://tables/voxel_objects.jsonc`、`biomes.jsonc`、
  `entity_objects.jsonc`、`item_placement.jsonc`；文件不存在则该表沿用嵌入版本。
- `reload_object_tables_from_strings(voxel, biome, entity, item)`：调试 / 测试入口，空串 = 不覆盖。
- `object_tables_status()`：返回 `{ generation, last_error, reload_enabled }`。
- `set_object_tables_reload_enabled(enabled)`：运行期开关。
- 暂停菜单的 Debug 面板有 `Reload Static Tables` 按钮，不轮询文件修改时间。
- 文件读取在 Godot 侧，Rust 只解析字符串。仅 `debug_assertions` 构建暴露重载入口。

### 当前限制

- 方块 `collision` 已驱动碰撞 LUT，`support` / `replaceable` 已用于放置规则，`solid` 已用于拾取；地形生成与世界初始调色板仍只认核心方块。
- 生物群系已数据驱动；热重载只影响之后生成的块，已生成地形不会自动重建。
- 错误文本带文件 / 表 / 条目名，但没有补丁文件行号。
- 联网会话禁用重载仍是手动开关，不会自动感知联机状态。

## 稳定身份

| 类型 | 用途 |
| --- | --- |
| `StableEntityId` | 跨边界的实体身份，不随 ECS 内部 `Entity` 复用变化 |
| `fnv1a32` | 名字到稳定 id 的哈希，跨平台一致 |
| `ids` | 分区工具，按域划 id 空间 |

`StableIdPlugin` 负责分配。表现层用稳定 id 关联节点，所以实体回收后 id 不被复用。

## 运行期语义注册表

`SemanticRegistry` 是 mod 的基石，引擎不关心其内部形状：

- 引擎侧作为 `Resource` 读取，跨会话保留。
- Godot 侧通过 `semantic_overrides` 覆盖，改的是名字到显示 id 的映射，不改逻辑。
- `action_table_to_variant` 之类导出函数把注册表内容搬给 GDScript。

## Mod 阶梯

| 级 | 修改对象 | 机制 |
| --- | --- | --- |
| M0 | 数值与概率 | DefinitionPatch、JSONC 对象补丁 |
| M1 | 内容定义 | 泛型 `Registry<T>`，冲突拒绝，升序遍历 |
| M2 | 新组件与表现翻译 | 注册制，不注册就不同步 |
| M3 | 新增系统到某阶段 | `ModContext::add_system(EngineSet, ..)` |
| M4 | 子阶段与排序 | `insert_set_before` 加 `configure_sets` |
| M5 | 服务与策略 | `Strategy` trait 加 `set_strategy` |
| M6 | 整块子系统 | 新的 `GameModule`，后续期次 |

关键判断：Bevy 调度不支持运行期删除或替换系统，所以替换行为一律抽成 `Strategy` trait 换资源。

## 注册表

`Registry<T>` 的确定性约定：

- 条目带命名空间，冲突直接拒绝，不静默覆盖。
- 覆盖用 `OverridePriority` 显式声明。
- 迭代按升序，保证同样注册集合得到同样顺序。

## Mod 清单

`ModManifest` 声明 `id`、`version`、`engine_api`、`load_after`、`load_after_optional`、`conflicts`、`exclusive_groups`、`params`。

- `load_after`：硬依赖，缺失报 `MissingDependency`，同时作为拓扑排序边。
- `load_after_optional`：可选依赖，存在时排在自己之前，缺失不报错。
- `conflicts`：单向声明，对方存在即报 `Conflict`。
- `exclusive_groups`：同一组最多加载一个 mod，冲突报 `ExclusiveGroup`。
- `engine_api` 是整数版本，只增不改。
- `params`：参数 schema（`ParamSpec`）。key 在 mod 内局部，全 key 为 `<mod_id>.<key>`；`ParamScope::World` 在 `PostStartup` 冻结并随存档保存，`ParamScope::Debug` 不进存档。客户端本地设置（画质、按键）不进入本表。

参数表由 `ParamTable::resolve` 在 mod `install` 之前解析：未知覆盖 key 直接拒绝，默认值 / 范围 / 布尔 / 枚举逐项校验；`CoreGame::build_with_mods` 负责这个装配点。

保存时，`capture_world` 从 `ParamTable` 取 mod 清单（id + version）与全部 World 参数，写进 `world.meta` v2 扩展段（`ParamTable::mods()` / `world_values()`）；Debug 参数不落盘。读档在阶段一用 `ParamTable::validate_saved` 按当前 schema 校验：缺失 key 保持默认值，未知 key、非 World 作用域、类型 / scale 不符、值越界都拒绝整档；阶段二在 `WorldParamsFrozen` 冻结前用 `apply_saved` 写回。格式版本升到 2，读取同时接受 1..=2；v1 存档视为空清单 / 空参数。mod 清单目前只记录，不做版本协商，也不因缺 mod 拒档。

实现细节（扩展段线格式、上限、校验顺序）见 `docs/待办/实现文档/存档格式规格.md` §2.1 与 §6.1。

后台入口的 `BevyBackendConfig` 也走同一装配点：`mods` 与 `param_overrides` 默认是 `default_core_mods()` 和空表，Godot 后端在启动时原样转交。GDScript 无法构造 `dyn Mod`，需要注入 native mod 时由 Rust 侧在启动前写入这两个字段。

## 改内容时的落点

| 想改什么 | 改哪里 |
| --- | --- |
| 材质外观（颜色 / 粗糙度 / 金属度 / 自发光 / 图案） | `godot-project/bevy_client/voxel_material/defs/*.tres`（VoxelMaterial），改完跑 `godot-project/tools/bake_voxel_materials.gd` 重新烘焙 |
| block → 六个面的材质绑定 | `godot-project/bevy_client/voxel_material/voxel_material_library.tres` 的 `blocks`（VoxelBlockMaterial） |
| 方块对象字段（solid / replaceable / support / collision） | `static_data/data/voxel_objects.jsonc` 或 mod 补丁 |
| 实体原型 / 放置规格 | `static_data/data/entity_objects.jsonc` |
| 物品定义 / 放下后变成什么 | `static_data/data/item_placement.jsonc` |
| 开发期覆盖表 | `user://tables/*.jsonc`（仅 dev 构建，通过 `reload_object_tables()` 应用） |
| LOD 距离阈值 | `static_data/voxel.rs` 的 `LOD_DISTANCE_THRESHOLDS`（尚未数据驱动） |
| 表现语义名 | 运行期 `SemanticRegistry` 或 GDScript 的 `semantic_overrides` |
