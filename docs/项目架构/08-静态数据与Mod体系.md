---
title: 静态数据与 Mod 体系
type: 架构文档
status: active
created: 2026-10-19
updated: 2026-11-12
tags:
  - 项目架构
  - 静态数据
  - Mod
source: "代码阅读 @ commit 36cd288 + JSONC 数据层实现；mod 动作表 / 插件注册表 / 存档冻结 / 插件选择入口 @ 2026-11-11；读档拒绝与 SaveBlocked 语义 @ 2026-11-11；mod 表现子种类（presentation / sub_kind）@ 2026-11-12；只读打开（read_only / readonly_plugin_selection / SavePaths::read_only / LoadMode::ReadOnly）@ 2026-11-12；目录式 mod 包（package.rs / scan_package_root / scan_mod_packages）@ 2026-11-12；BMS Lua 脚本层（scripting feature / ScriptSources / ScriptingPlugin）@ 2026-11-12；运行期校验和（verify::simulation_checksum / diagnostics.checksum）@ 2026-11-12；参数 schema 面板（param_schema.rs / get_param_schema / set_param_overrides / ParamPanel）@ 2026-11-12；导入验证（verify_mod.rs / package_hash / verify_mod / verify_mods）@ 2026-11-12"
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

## 插件注册表与 profile 解析

世界档的插件清单不再写死在 `add_default_plugins`：每个插件在 `game-core/src/world/profile.rs` 的 `PLUGIN_REGISTRY` 登记一条 `PluginEntry { id, requires, default_on, build }`。profile 本身只是一组 id，解析器负责补齐依赖、检测环并做确定性排序。

id 列表（按注册表顺序，也是同层拓扑排序的 tie-break）：

`input`、`presentation`、`stable_id`、`spatial`、`aoi`、`gameplay`、`movement`、`death`、`camera`、`block_edit`、`world`、`game_voxel`、`terrain`、`streaming`、`voxel_mesh_refresh`、`prediction`、`sync_register`。

`requires` 只登记代码里能证实的硬依赖：

- `aoi -> spatial`
- `movement` / `death` / `camera -> gameplay`（系统挂在 `GameplaySet` 的阶段链上）
- `block_edit -> world, game_voxel, camera, gameplay`（系统参数要体素资源，且 `.after(camera_system)`）
- `terrain -> game_voxel, stable_id`（build 里的 `assert!(app.is_plugin_added::<GameVoxelPlugin>())`，Startup 生成还要稳定 ID）
- `streaming -> game_voxel, terrain, stable_id, world`（build 断言 + 启动引导要 `IslandRegistry`）
- `voxel_mesh_refresh -> game_voxel`
- `prediction -> presentation, input`（回滚采样接表现管线，系统读 `SimulationTick`）
- `sync_register -> presentation`
- 其余（`input` / `presentation` / `stable_id` / `spatial` / `gameplay` / `world` / `game_voxel`）无硬依赖。

这些边来自 `assert!(app.is_plugin_added::<...>())`、系统参数需要的资源与 `.after(...)` 的排序前提，不是凭插件名字猜的。新增插件时必须同时补 `requires`；只改默认清单会让「默认能跑、裁掉依赖才晚崩」的问题重新出现。

解析规则（`resolve_plugins`）：

- `selection` 里的 id 必须存在，否则 `ProfileError::UnknownPlugin`；
- `requires` 递归自动补齐（cargo feature 语义），同一 id 只装一次；
- `requires` 指向注册表外报 `MissingDependency`；
- DFS 检测环，报 `Cycle { chain }`（形如 `a -> b -> a`）；
- 通过后 Kahn 拓扑排序；同一层（当前入度为 0）按 `PLUGIN_REGISTRY` 下标取最小，保证同一 profile 每次得到完全相同的装配序列；
- `default_plugin_selection()` 返回全部 `default_on` 的 id（当前 17 个全开）；`streaming` 的 `build` 仍自己判断 `TerrainMode::Streaming`，Eager 下它在序列里但不装任何插件，旧行为零变化。

装配顺序仍是硬约束。`build_with_profile` 依次执行：

1. `resolve_plugins(selection)?`——失败时 App 还没有写入任何东西；
2. `StaticDataPlugin` → `ParamTable::resolve` → 插入参数表 → `ModParamsPlugin`；
3. `DataRegistry::new()` → `mods.apply` → 插入数据注册表；
4. 按解析结果逐个调用 `(entry.build)(app)`。

`build_with_mods` 保持原签名，行为等价于以 `default_plugin_selection()` 调 `build_with_profile`：两者共用 `apply_mods` 与「解析后逐个 build」的路径，`add_default_plugins` 只做「默认 id 解析 + 逐个 build」。

|`code`|`WorldBuildError` 来源|
| --- | --- |
|`profile.unknown_plugin`|profile 选了注册表里不存在的 id|
|`profile.missing_dependency`|`requires` 指向不存在的 id|
|`profile.cycle`|插件依赖成环|
|`mod.circular_dependency` / `mod.conflict` / `mod.exclusive_group` / `mod.duplicate_id` / `mod.param` / `mod.actions`|`WorldBuildError::Mod` 对 `ModError` 的映射|
|`backend.panic`|runner 捕获到未预期 panic|

后台入口 `CoreGameModule::build` 不再 `expect`：`build_with_profile` 失败时把 `ModuleBuildFailure { code, message }` 插进 App 并提前返回。`run_headless` 在 `build_app` 之后、发 `Started` 之前检查该资源，回送 `BevyLifecycleMsg::Error { code, message }` 与 `Stopped(1)`，直接返回 `ExitCode::FAILURE`——后台线程不 panic，也不进入主循环。Godot 侧 `bev_error(code, message)` 改为两参信号，`bevy_client.gd` 原样透传为 `bevy_error(code, message)`，测试脚本按 code + message 输出。

边界：**建档时解析一次、解析结果随存档冻结**。解析只在 `build_with_profile` 里发生一次，运行期不提供重解析接口；`world.meta` 存的是**解析后的插件装配序列**（`ActivePlugins`，按构建顺序），不是建档时的 selection，也不在运行期重算。`FORMAT_VERSION` 仍为 2，在 v2 扩展段末尾追加插件段，读档阶段一逐项比对。

为什么存序列而不是存 selection：

- 版本号只能兜住「字节布局变了」。布局没变、但 `PLUGIN_REGISTRY` 里的 `requires` / 默认集合 / 排序 tie-break 变了这类改动不升 `FORMAT_VERSION`，迁移钩子根本不会被触发；只存 selection 再重算就会静默漂移（旧世界凭空多装一个插件、系统调度顺序变了）。
- 迁移只能使用档里已有的信息。存序列时迁移有 ground truth（旧序列 → 新序列的映射规则），存 selection 则连「当年解析出了什么」都不知道。
- 代价是注册表任何变动都会让旧档打不开（`MissingPlugin` / `UnexpectedPlugin` / `PluginOrderMismatch`），这是刻意选的硬门槛，与动作表同一路线；迁移工具将来再做。

装配点早于读档：后台线程启动时 `CoreGameModule::build` 就按 profile 建好 App，`StreamingWorldPlugin` 在 `Startup` 才真正 `load_world`。因此「用哪些插件装配」必须来自档本身或建档选择，不能等读完档再决定。

### 只读打开（`read_only`）

只读会话不是另一种 App，而是同一套装配上的三个开关同时打开：

1. `BevyBackendConfig.read_only = true` 插入 `ReadOnlySession` 资源；输入链整体不注册（不 tick、不消费暂存区，见 `04-输入链路.md`）。
2. 未显式给 `plugins` 时，选择 `readonly_plugin_selection()`：从 `default_plugin_selection()` 里剔除 `gameplay` / `movement` / `death` / `camera` / `block_edit` / `prediction`，保留 `input` / `presentation` / `stable_id` / `spatial` / `aoi` / `world` / `game_voxel` / `terrain` / `streaming` / `voxel_mesh_refresh` / `sync_register`，`requires` 仍由解析器补齐。显式 `plugins` 优先，只读不额外过滤——调用方自己负责。
3. 存档路径用 `SavePaths::read_only(root)`：读档可用，所有写入口（`write_encoded` / `remove_region` / `backup_corrupt` / `reclaim_*` / 卸载落盘）直接拒绝或跳过；`load_world_with_mode(..., LoadMode::ReadOnly)` 跳过 mod 清单、动作表、插件序列与参数的身份校验（只 `warn!`），但保留摘要 / region / 负载结构校验。

只读**绝不降级新建世界**：`world.meta` 缺失或结构损坏时直接走读档拒绝路径（`world_load_failed` 或 `bev_error`），不会静默给一个空世界。身份不符的档（例如装了本机没有的 mod）在只读下可以打开查看，这正是只读存在的意义。

Godot 入口是 `start_bevy_streaming_readonly(fixed_hz, runner_hz, save_dir)`（`save_dir` 必填），`bevy_client.gd` 对应 `read_only = true` 的 STREAMING 模式与 `open_world_readonly(directory)`。

只读打开成功后，`compute_load_diff` 会把存档的 mod 清单、插件序列、动作表、世界参数与当前 `AppliedModSet` / `ActivePlugins` / `ResolvedActions` / `ParamTable` 逐项比较，结果写进 `WorldLoadDiff` 资源；只读 profile 主动剔除的 6 个玩法插件（gameplay / movement / death / camera / block_edit / prediction）不计入缺失与顺序差异；`StreamingDiagnostics.diff` 与 FFI `get_world_load_diff()` 把它送到 Godot，`ReadOnlyDiffPanel` 负责展示。正常读档会移除该资源，所以正常档不会弹面板。


插件选择从哪来：后台启动配置 `BevyBackendConfig.plugins: Option<Vec<String>>`（经 `CoreGameModule` 搬运到 `build_with_profile`）。`None` = 默认 profile（`default_plugin_selection()`），既有入口行为零变化；`Some` 先经 `intern_plugin_selection` 把运行期 String 收敛成注册表里的 `&'static str`。

`Some(vec![])` **不是**「用默认」，而是「一个可选插件都不装」——空集合是合法的显式选择，不会回退。

未知 id 不 panic：`intern_plugin_selection` 返回 `ProfileError::UnknownPlugin`，`CoreGameModule::build` 把它写成 `ModuleBuildFailure { code: "profile.unknown_plugin", .. }`，由 runner 转成 `bev_error(code, message)` + `Stopped(1)`。Godot 入口是 `start_bevy_streaming_with_plugins(fixed_hz, runner_hz, save_dir, plugins)`，其余参数与 `start_bevy_streaming` 一致。

同一结构里还有 `read_only: bool`（默认 `false`）：`true` 时插入 `ReadOnlySession`、`plugins: None` 改选只读 profile、存档路径转只读，对应 Godot 入口 `start_bevy_streaming_readonly`。`plugins` 显式给出时按原样解析，只读不再过滤。

## 注册表

`Registry<T>` 的确定性约定：

- 条目带命名空间，冲突直接拒绝，不静默覆盖。
- 覆盖用 `OverridePriority` 显式声明。
- 迭代按升序，保证同样注册集合得到同样顺序。

## Mod 清单

`ModManifest` 声明 `id`、`version`、`engine_api`、`load_after`、`load_after_optional`、`conflicts`、`exclusive_groups`、`params`、`actions`、`presentation`。

- `load_after`：硬依赖，缺失报 `MissingDependency`，同时作为拓扑排序边。
- `load_after_optional`：可选依赖，存在时排在自己之前，缺失不报错。
- `conflicts`：单向声明，对方存在即报 `Conflict`。
- `exclusive_groups`：同一组最多加载一个 mod，冲突报 `ExclusiveGroup`。
- `engine_api` 是整数版本，只增不改。
- `params`：参数 schema（`ParamSpec`）。key 在 mod 内局部，全 key 为 `<mod_id>.<key>`；`ParamScope::World` 在 `PostStartup` 冻结并随存档保存，`ParamScope::Debug` 不进存档。客户端本地设置（画质、按键）不进入本表。目录式包的 `params` 与它**同表**合并（见下节）。
- `actions`：本 mod 声明的输入动作名（`&'static [&'static str]`，不声明则 `&[]`）。解析规则见下节。
- `presentation`：本 mod 的表现子种类声明（`&'static [ModPayloadSpec]`，不声明则 `&[]`）。
  每项 `ModPayloadSpec { name, fields }`，字段 `ModFieldSpec { key, name, visibility, required_bits }`，
  `ModFieldVis` 取 `All` / `Required` / `ServerOnly`（与扩展袋可见性同义）。解析规则见「表现子种类分配」小节。

参数表由 `ParamTable::resolve` 在 mod `install` 之前解析：未知覆盖 key 直接拒绝，默认值 / 范围 / 布尔 / 枚举逐项校验；`CoreGame::build_with_mods` / `build_with_profile` 负责这个装配点。

### 参数 schema 面板（装配前只读）

Godot 通用参数面板不经过运行期 `ParamTable`，而是在装配前用 `game-core/src/param_schema.rs` 采集一份**只读**合并 schema：

```rust
pub fn collect_param_schema(
    mods: &CoreMods,
    packages: &[LoadedPackage],
) -> Result<Vec<ParamSchemaEntry>, ParamError>;
pub fn validate_param_overrides(
    entries: &[ParamSchemaEntry],
    overrides: &BTreeMap<String, i64>,
) -> Result<(), ParamError>;
```

- 编译期 `ModManifest.params` 与包 `PackageManifest.params` **同表**合并，key 一律 `<mod_id>.<local_key>`，结果按全 key 升序（`BTreeMap` 序）稳定。
- 重复全 key 报 `DuplicateKey`；编译期条目走 `ParamSpec::validate`（内部 `ParamType::validate_spec` + 默认值校验）；包侧另校验 `min <= max`、`step > 0`、`scale > 0`、默认在范围、Enum 默认下标合法、Enum 值非空，`visible_if.key` 必须是**同一 mod 内**已声明的 local key（跨 mod 引用报 `InvalidSpec`）。前向引用合法。
- `ParamSchemaEntry` 字段：`key`、`mod_id`、`local_key`、`scope`、`kind`（`Bool` / `Int` / `Fixed` / `Enum { values }`）、`default`、`min` / `max` / `step`（`Option<i64>`）、`scale`（`Option<u32>`）、`group`、`visible_if: Option<(String, i64)>`。`visible_if` 的 key 与 `ParamSpec` 一致，**始终是 local key**；面板自己用 `mod_id` 拼全 key，避免跨 mod 误判。
- `validate_param_overrides` 只校验取值：未知 key、Bool 非 0/1、Int / Fixed 越界、Enum 下标越界都拒绝；**不按作用域拒绝**，World 与 Debug 都允许出现在覆盖表里。它不是运行期路径，`ParamTable::resolve` 的装配逻辑不变。

Godot FFI（`godot-client-ext/src/bevy_runtime_node.rs`，`BevyAppManager`）：

| 方法 | 行为与返回 |
| --- | --- |
| `get_param_schema()` | 用 `default_core_mods()` + 节点上 `scan_mod_packages` 的结果采集，返回 `{ ok, schema, errors }`。成功时 schema 每项：`key` / `mod_id` / `local_key` / `scope`（`"world"` / `"debug"`）/ `kind`（`"bool"` / `"int"` / `"fixed"` / `"enum"`）/ `default` / `min` / `max` / `step` / `scale`（不适用为 `-1`）/ `values`（`PackedStringArray`，非 enum 为空数组）/ `group` / `visible_if`（空字典或 `{ key, equals }`，`key` 为 local key）。失败时 `ok=false`、`schema=[]`，`errors` 每项 `{ code, message }` |
| `set_param_overrides(params)` | 解析 `{ key: String, value: i64 }`，用当前 schema 校验；成功替换节点上的待用覆盖表，返回 `{ ok, count, errors }`；失败 `ok=false` 且**保留旧表**。非 String key / 非整数 value 报 `param.invalid_override` |

节点字段 `param_overrides: BTreeMap<String, i64>` 默认空；`start_backend` 构造 `BevyBackendConfig` 时写入 `config.param_overrides`，因此所有 `start_bevy_*` 入口都生效。校验失败不会在启动时 panic，仍由 `build_with_profile` 走结构化失败（`mod.param`）。

GDScript 面板 `godot-project/ui/screens/param_panel.gd`（`class_name ParamPanel extends Control`，纯代码构建，无 `.tscn`）：

- `load_schema(result: Dictionary)` 接受 `get_param_schema()` 的返回；`ok=false` 时显示错误标签并禁用 Apply。`close_requested` 只发信号，面板不自行隐藏。
- `collect_values() -> Dictionary`、`reset_defaults() -> void`、`apply_to(manager: Node) -> bool`（内部用 `manager.call("set_param_overrides", collect_values())` 动态派发，先 `has_method` 检查，成功才返回 true）。
- Bool → `CheckButton`；Int → `SpinBox`；Fixed → `HSlider` + 数值 Label（显示 `落盘整数 / scale`，按 scale 位数格式化，提交 `round(display * scale)`）；Enum → `OptionButton`（values 顺序即下标）。按 `group` 分节，空 group 归「常规」。
- `visible_if` 按同 mod 的 `local_key` 联动，条件不满足时整行隐藏（不是禁用）。隐藏行仍参与 `collect_values()`，切换父开关不会丢输入。
- World 参数行带「建档后不可改」徽标与颜色标记，但建档前仍可编辑，不置灰、不禁用。
- 没有 manager / 方法不存在时 `apply_to` 返回 false 并 `push_error`，不崩溃。`bevy_client.gd` 提供 `get_param_schema()` / `set_param_overrides(values)` 两个薄代理，无 manager 时返回 `ok=false`。
- 真实入口是暂停菜单 Settings 区的 `MOD PARAMS`：`UiRoot` 持有面板、每次打开重拉 schema、`apply_requested` 接给 `apply_to`，关闭后回到暂停面板；关闭不改 `UiState`，菜单与后端暂停状态保持。

错误码统一 `param.*` 前缀（与 `mod.*` / `profile.*` 分开）：`param.duplicate_key`、`param.unknown_key`、`param.invalid_spec`、`param.invalid_default`、`param.out_of_range`、`param.invalid_bool`、`param.invalid_enum`、`param.invalid_override`；客户端代理缺扩展方法时为 `param.unsupported`。

### 目录式 mod 包（运行期加载）

编译期 native mod 的清单全是 `&'static`，Godot 造不出来。运行期包由 `game-core/src/package.rs` 解析成**拥有所有权**的 `LoadedPackage`（可跨 FFI 持有、可单测），装配时再 `to_mod()`：`Box::leak` 成 `'static` 并实现 `Mod<S>`，`install` 把 data 补丁交给 `ModContext::add_object_layer`。泄漏只发生在启动 / 建档冷路径，进程生命周期内一份。

格式是**目录**（不引入 zip 依赖）：

```text
<mods 根>/
  bobber/
    mod.json
    data/blocks.jsonc
    data/items.jsonc
    scripts/bobber.lua
```

- 扫描入口 `scan_package_root(root)`：根目录自身有 `mod.json` 时按单包处理；否则只扫**含 `mod.json` 的子目录**，没有清单的子目录直接跳过。目录名与包顺序都排序后确定。
- 清单字段与 `ModManifest` 同名：`id`、`version`、`engine_api`、`load_after`、`load_after_optional`、`conflicts`、`exclusive_groups`、`actions`、`params`、`presentation`、`data`、`scripts`。`serde` 用 `deny_unknown_fields`，写错字段名直接报 schema 错误，不静默忽略。
- `params` 的元素是 `{ key, type: { kind: "bool" | "int" | "fixed" | "enum", min, max, step, scale, values }, default, scope: "world" | "debug", group?, visible_if? }`；`presentation` 的元素是 `{ name, fields: [{ key, name, visibility: "all" | "required" | "server_only", required_bits? }] }`。
- `data` 是相对包目录的路径数组，只接受 `Component::Normal` 段（拒绝 `..` 逃逸），不重复，≤ 64 个；文件用 JSONC 解析，顶层必须是对象，表名走 `ObjectTableKind::parse`（voxel / biome / entity / item 及其别名），补丁必须是数组。
- `scripts` 是相对包目录的 `.lua` 路径数组，同样只接受 `Component::Normal`、不重复、≤ 64 个；单文件 ≤ 256 KiB，读取为 UTF-8 文本后交给脚本层（见下节）。

校验（任一失败整包拒绝，错误码 `mod.package.*`）：

| 类别 | 规则 | code |
| --- | --- | --- |
| 清单 | id / 依赖名 1..=64 字节，仅 ASCII 字母数字与 `_ - .`；`version > 0`；`engine_api == CURRENT_ENGINE_API`（当前 1） | `mod.package.manifest` / `mod.package.engine_api` |
| 关系 | `load_after` / `load_after_optional` / `conflicts` 不能引用自己，列表内不重复 | `mod.package.schema` |
| 动作 | 名字非空、≤ `ACTION_NAME_MAX_BYTES`、无控制字符，包内不重名 | `mod.package.schema` |
| 参数 | key 包内唯一；Bool 默认 0/1；Int/Fixed `min <= max`、`step > 0`、默认在范围内；Fixed `scale > 0`；Enum 非空且默认下标合法；`visible_if.key` 必须已声明 | `mod.package.schema` |
| 表现 | 子种类名 / 字段名非空且 ≤ 64 字节；包内子种类不重名；字段 key 与名字在 spec 内不重名；`required` 必须有非 0 `required_bits`，其它可见性必须为 0 | `mod.package.schema` |
| 数据 | 文件存在、≤ 1 MiB（单文件）/ 8 MiB（整包）、JSONC 可解析、表名可识别、补丁是数组 | `mod.package.data` |
| 脚本 | 路径为包内相对路径且以 `.lua` 结尾、不重复、文件存在、≤ 256 KiB、UTF-8 可读 | `mod.package.schema` / `mod.package.data` |
| IO | 目录 / 文件读失败 | `mod.package.io` |

上限：单包清单 ≤ 256 KiB、data ≤ 64 个、扫描结果 ≤ 64 个包（`MAX_PACKAGES`）。

装配链与编译期 mod 完全共用：`Mods::push(package.to_mod())` → 拓扑排序 / 冲突 / `ResolvedActions` / `ResolvedModPayloads` 全部照旧，因此包声明的动作与表现子种类自动获得通道位与 `sub_kind`。包**不加载 native 可执行代码**；`scripts` 里的 Lua 由 BMS 解释执行，同一份源码跨平台，不参与 ABI。

Godot 侧流程：`BevyAppManager.scan_mod_packages(root)` 读目录并解析，返回 `{ ok, count, packages, errors }`（每个包带 `id`、`version`、`source`、`actions`、`params`、`presentation`、`data_files`、`scripts`）（`ok=false` 时 `errors` 为 `{ path, code, message }`）；成功结果留在节点上，之后**任一** `start_bevy_*` 都把它带进 `BevyBackendConfig.packages`。`bevy_client.gd` 的 `mod_dir` 非空时在 `_start_backend` 前自动扫描，失败则取消本次启动（不装半套）。

包不参与存档身份：`world.meta` 冻结的是 mod `id/version` 与解析后的插件序列，**不含**包文件哈希。改了包内容却没升 `version`，旧档仍会放行；发布者必须靠 `version` 表达内容变更。运行期漂移由下面的校验和发现；安装期验证与包哈希见下节「导入验证（verify_mod）」。


### 导入验证（verify_mod）

`game-core/src/verify_mod.rs` 是 P0-2 的后端：给定一个 `LoadedPackage`，产出结构化 `VerificationReport`。验证过程不写存档、不碰 `SaveRoot`，也不调用 `publish_active_actions` / `publish_mod_payloads` 等全局发布入口；两个验证 App 与游戏本体完全隔离。

- `package_hash(&LoadedPackage) -> u64`：FNV-1a 64，域前缀 `voxeldustry.package.v1`。按声明顺序混入 manifest 的全部语义字段（`id` / `version` / `engine_api` / `load_after` / `load_after_optional` / `conflicts` / `exclusive_groups` / `actions` / `params` / `presentation` / `data` / `scripts`；字符串一律 `u32` 长度前缀，避免拼接歧义），再混入每个**已声明** data / scripts 的相对路径与字节。未声明文件不参与（`load_package` 本来也不读）。`package_hash_hex` 输出 16 位小写十六进制。
- `verify_package(package, context, options)`：`context` 是带依赖的其它已扫描包；它与被测包按 `id` 去重后一起 `to_mod::<CoreSpec>()`。用 `default_plugin_selection()` 构建两个独立 App（`App::new()` + `MinimalPlugins` + `Time::<Fixed>::from_hz(60.0)` + `TimeUpdateStrategy::ManualDuration(1/60 s)`），各插入同一 `WorldRng::from_seed(seed)` 与 8 个固定探针（`StableEntityId(0xFFFF_0000 + i)`、`Transform::from_xyz(i, 0, 0)`、`Velocity { linear: Vec3::new(1.0, 0.0, 0.5) }`），`finish` / `cleanup` 后先 `update()` 一次跑完 Startup / PostStartup，再逐 tick `update()`。
- 报告字段（FFI 同名）：`mod_id`、`version`、`package_hash`、`engine_api`、`engine_version`（game-core 的 `CARGO_PKG_VERSION`）、`ok`、`deterministic`、`rollback_equivalent`、`ticks`、`hashes`、`first_divergence_tick`、`errors`、`cached`；`errors` 每项是 `{ code, message, tick }`，`tick = -1` / `None` 表示无。
- `VerifyOptions` 默认 `ticks = 120`、`rollback_at = 60`、`rollback_ticks = 30`、固定 `seed`。`first_divergence_tick` **从 1 开始**，`None` 表示未分叉。
- 确定性双跑：两个 App 的 `simulation_checksum` 序列逐 tick 比较，第一处不同记 `first_divergence_tick`，`deterministic = false`，并追加 `verify.nondeterministic`。
- 回滚等价：第一个 App 在 `rollback_at` 快照**所有**带 `StableEntityId` 的实体（`Transform` / `Velocity` 都是 `Option`，纯逻辑实体也进快照）与 `WorldRng`，记录当 tick 校验和；继续跑 `rollback_ticks` 得到 future 校验和；随后写回 Transform / Velocity / RNG（快照里没有的组件会被移除），despawn 回滚窗内新生成的 `StableEntityId` 实体，再跑 `rollback_ticks` 比较。`rollback_at = 0` 表示 Startup 之后、第一个 tick 之前的快照。不等记 `verify.rollback_mismatch`（同时保留两个 hash）；快照实体已消失记 `verify.rollback_entity_missing`。
- `ok = deterministic && rollback_equivalent && errors.is_empty()`。构建失败转成 `WorldBuildError::code()`（`mod.*` / `profile.*`）报告，不 panic。
- 缓存 `VerificationCache`：键 `(mod_id, version, package_hash, engine_api, engine_version, context)`。`context` 是参与验证的依赖包 `(id, version, package_hash)` 有序去重列表；同一个包单独验证（缺依赖）与带上下文验证（依赖齐全）结论不同，所以上下文必须进键。命中直接 clone 报告并置 `cached = true`，不重跑；未命中跑完插入。**键不含 `VerifyOptions`**，因此只对默认 options 安全；自定义 tick / seed 应直接调 `verify_package`，或先 `clear()` 缓存。
- FFI：`BevyAppManager.verify_mod(path)` / `verify_mods(paths)` 是**同步**调用，会阻塞调用线程约数秒（两轮 120 tick + 回滚 30 tick），Godot UI 必须放到后台线程或后续异步任务；本轮不提供异步。坏包返回报告形状（`mod_id` 为空、`errors[0].code = mod.package.*`、message 含路径），不抛 panic。字典里 `package_hash` / `final_hash` 是 hex 字符串（`final_hash` 是最后一个 tick 的世界校验和，不是包哈希），`hashes` 按 `i64` 位模式给（保留全部 64 位，但负数不代表排序，比较请用 hex），`first_divergence_tick = -1` 表示无分叉。
- 覆盖边界（刻意保守）：只证明**本机、当前引擎版本、这组固定探针 / seed** 下双跑与回滚一致；`simulation_checksum` 只覆盖世界种子、`StableEntityId` 与 `Transform` 位模式，因此不改位置的内部状态（如仅改 `Velocity` / 自定义资源）在本轮 120 tick 内可能观察不到；**不执行 Lua / BMS 脚本层**，不覆盖跨平台浮点差异；回滚快照覆盖所有 `StableEntityId` 实体的 Transform / Velocity 与 `WorldRng`，但 mod 自定义资源不在快照里，因此带自定义状态的回滚会被判不等价；窗口内新生成的实体会被清理，快照实体消失则无法恢复。
- 策略约定：单机验证失败只警告并允许继续；联机（尚未实现）只放行 `ok == true` 且 `package_hash` 一致的加入者。文案只写「已通过检测」，不承诺反作弊或绝对安全。

### Lua 脚本层（feature = `scripting`）

mod 包可以在 `scripts` 里带 Lua 脚本，由 BMS（bevy_mod_scripting v0.21.1 / Bevy 0.19）解释执行。这一层默认关闭：`game-core` 的 `scripting` feature 才把 BMS 拉进依赖树，插件注册表里的 `scripting` 条目 `default_on = false`，要显式选进 profile（只读 profile 不含它）。

装配路径：

1. `load_package` 把每个 `scripts/*.lua` 读成 `PackageScript { path, source }`（UTF-8；路径 / 大小校验见上表）。
2. `CoreGameModule::build` 在 `build_with_profile` 之前把所有包的脚本塞进 `ScriptSources`（名字是 `<mod_id>/<相对路径>`，顺序按包拓扑序）。插件没被选中时这个资源只是闲置，不会拉起 BMS。
3. `ScriptingPlugin`（`game-core/src/scripting/mod.rs`）缺 `AssetPlugin` 时补一个，加 `BMSPlugin`，然后 Startup 系统把每条源码 `ScriptAsset::new(text).with_language(Language::Lua)` 加进 `Assets<ScriptAsset>`，最后 spawn 一个实体挂 `ScriptComponent`。**所有脚本挂同一实体**，脚本之间用全局环境共享状态。

边界（与《插件化架构方案》一致）：

- 脚本只能「声明组件 / 写系统体」；热路径仍在 Rust。
- `ScriptValue` 目前没有网络 / 存档编码路径，`SyncPayload` 与存档 codec 都只认具体 Rust 类型（见 05-表现管线）。脚本组件**不参与同步、不进存档**，联机时各端各自跑脚本，跨端一致性由 Rust 组件负责。
- Lua 表遍历顺序、浮点行为与 BMS 内部调度未纳入确定性验证；`fixed_tick_simulation_is_bitwise_deterministic` 只覆盖 Rust 玩法。要求确定性的玩法不要把状态写进脚本。
- 脚本编译 / 运行错误由 BMS 的 `ScriptErrorEvent` 记录（`bevy_log::error`），不会 panic 整个 App；坏脚本不会阻止世界加载。

测试：`scripting::tests` 两个用例覆盖「两条源码 → 两个资产 + 一个 ScriptComponent」与「空源不 spawn」；`package::tests` 覆盖脚本路径校验与读取。

### 动作表解析

动作表在建档（mod 装配）时解析一次，随存档冻结，会话内不可更改。`Mods::apply` 在全部 mod `install` 完成后调用 `ResolvedActions::resolve(S::Actions::table(), &mods)`：

- core 动作保持声明里的 `ActionId` 与 `channel_bit` 原样（本游戏 `ACTION_TABLE` 15 条：bit 0–14、id 1–15）。
- mod 动作按 `(mod_id, name)` 全局排序后依次分配：`ActionId = 1000 + 序号`（1000–9999 保留给 mod，超过 9000 个报错）、`channel_bit = 从 0 起第一个未占用的位`（core 占 0..=14，因此 mod 从 15 起；`MAX_INPUT_CHANNELS` = 64，耗尽报错）。
- 名字必须非空、≤ 64 字节、无控制字符；与 core 重名、mod 之间重名、core 占用保留 id 区间、id 或通道位耗尽都让装配失败（`ModError::Actions`）。
- 结果顺序固定：core 在前按 bit 升序，mod 动作按分配顺序。

成功后 `ResolvedActions` 与 `AppliedModSet`（`mod_set_hash` + 按 id 升序的 `ModRef { id, version }`）作为 Resource 注入 App。输入侧 `derive_input_events` 优先用 `ResolvedActions::action_of_bit`，`RawInputFrame::from_names_with_table` 把 Godot 传来的动作名翻成通道位；无该资源时回退静态 `ActionMap`（无 mod 时结果与旧行为逐位相同）。`mod_set_hash` 的算法不变（逐 mod `"id:version"` 的 fnv1a32 串联）。

### 表现子种类分配

mod 自定义表现载荷（表现层 code 12 `custom`）用 `u16 sub_kind` 区分「同一个实体上的不同表现种类」。`Mods::apply` 在全部 mod `install` 完成后调用 `ResolvedModPayloads::resolve(&mods)`：

- 编号按**拓扑排序后的 mod 顺序**、每个 mod **manifest 里的声明顺序**从 1 递增（0 保留给核心 / 未分配），与 `load_after` 排出的实际装配顺序一致，因此两个 mod 的 `Mods::push` 顺序不影响结果。
- 同一 mod 内两个子种类重名、同一 spec 内字段 key 重复、子种类总数超过 `u16::MAX` 都让装配失败（`ModError::Presentation`，错误码 `mod.presentation`）。
- 组内字段按 key 升序规范化后写入 `ResolvedModPayloadKind { sub_kind, mod_id, name, fields }`，作为 `ResolvedModPayloads` 资源注入 App，并由 `bevy_backend` 发布到全局快照 `publish_mod_payloads` / `mod_payload_table()` 供 Godot 读（`BevyRuntimeNode.get_mod_payload_table()`）。
- **sub_kind 是装配期派生值，不进存档**：表现不跨模拟 / 回放边界，mod 清单本身已由 `world.meta` 冻结（见下节），读档时按同一清单重建即得到同一编号，不需要额外保存或比对。

### 随存档冻结与读档校验

保存时，`capture_world` 从 `ParamTable` 取 mod 清单（id + version）与全部 World 参数，从 `ResolvedActions` 取动作表（按 name 严格升序），从 `ActivePlugins` 取插件装配序列（按构建顺序，**不排序**），写进 `world.meta` v2 扩展段（`ParamTable::mods()` / `world_values()`）；Debug 参数不落盘。

读档在阶段一（只读校验，失败时 world 一个字节都不改）逐段比对：

- `AppliedModSet` 存在：`meta.mods` 与它按 id 逐项比对（数量、id、version），不等报 `MissingMod` / `UnexpectedMod` / `ModVersionMismatch`。
- `ResolvedActions` 存在：`meta.actions` 与当前表逐项比对（name / action / channel_bit，顺序敏感），不等报 `ActionCountMismatch` / `ActionTableMismatch`。
- `ActivePlugins` 存在：先比集合（存档有、当前没有报 `MissingPlugin`；当前有、存档没记录报 `UnexpectedPlugin`），再逐项比顺序（报 `PluginOrderMismatch`）。
- 资源缺失：段为空则放行；段非空报 `MissingResource`。

参数段仍用 `ParamTable::validate_saved` 按当前 schema 校验：缺失 key 保持默认值，未知 key、非 World 作用域、类型 / scale 不符、值越界都拒绝整档；阶段二在 `WorldParamsFrozen` 冻结前用 `apply_saved` 写回。

格式版本保持 2（`MIN_SUPPORTED_FORMAT_VERSION` 仍为 1），读取同时接受 1..=2。`world.meta` v2 扩展段的线格式（小端）：

```text
u32 mod_count
  每条：u16 id_len + id 字节 + u32 version
u32 param_count
  每条：u16 key_len + key 字节 + u8 ty + u32 scale + i64 value
u32 action_count
  每条：u16 name_len + name 字节 + u32 action + u32 channel_bit
u32 plugin_count              # 插件装配顺序：不排序，按构建顺序原样写
  每条：u16 id_len + id 字节
u64 digest                    # 覆盖头、body 表与上面全部扩展字节
```

兼容：v1 存档没有扩展段，解码为「空清单 / 空参数 / 空动作表 / 空插件表」。**未发布前不做旧档兼容**：v2 的扩展段（mod 清单、参数、动作表、插件表）全部必填，缺段按坏档拒绝——读档阶段一还会把冻结的动作表与当前装配逐项比对，数量或内容不符直接整档失败。上限 `MAX_MOD_COUNT` = `MAX_PARAM_COUNT` = `MAX_ACTION_COUNT` = 4096、`MAX_PLUGIN_COUNT` = 64，名字长度上限 255 / 255 / 64 / 64。

读档失败按**原因分成两类**，处置完全不同（判据是 `SaveErrorKind::is_world_rejection()`）：

| 类别 | 变体 | 处置 |
| --- | --- | --- |
| 世界身份不符 | `MissingMod` / `UnexpectedMod` / `ModVersionMismatch` / `ActionTableMismatch` / `ActionCountMismatch` / `MissingPlugin` / `UnexpectedPlugin` / `PluginOrderMismatch` / `MissingResource` / `Param` / `Unsupported` | **硬拒绝**：不建新世界，插 `WorldLoadRejection`（code / path / message）与 `SaveBlocked`，经 `bev_error(save.*)` 报给 Godot 后退出；原档**一个字节都不写** |
| 存档损坏 | `Truncated` / `TrailingBytes` / `BadMagic` / `Io` / `DigestMismatch` / `Payload` 等 | **降级建新世界**：只写日志，玩家仍可继续玩（多半只能靠删档恢复） |

拒绝态下 `SaveBlocked` 是最后一道保险：手动 SaveNow、自动存档、退出补跑帧、卸载落盘全部提前返回，所以即使 Godot 立刻 `stop_bevy()`，退出前那一次补跑帧也不会把新世界盖到坏档上。

实现细节（扩展段线格式、上限、校验顺序）见 `docs/待办/实现文档/存档格式规格.md` §2.1 与 §6.1（该文档在 `docs/待办` 下，未纳入版本库）。

后台入口的 `BevyBackendConfig` 也走同一装配点：`mods` 与 `param_overrides` 默认是 `default_core_mods()` 和空表，Godot 后端在启动时原样转交。插件 profile 由 `plugins: Option<Vec<String>>` 暴露（`None` = 默认 profile，`Some(vec![])` = 显式空选择，未知 id 走 `ProfileError::UnknownPlugin` 结构化失败）；`start_bevy_streaming_with_plugins` 是对应的 GDScript 入口。GDScript 无法构造 `dyn Mod`，需要注入 native mod 时由 Rust 侧在启动前写入这两个字段。

### 运行期校验和（确定性验证 / 回滚等价）

`game_core::verify::simulation_checksum(&mut World) -> u64` 是确定性与回滚等价的公共原语：

- FNV-1a 64，混入 `CHECKSUM_VERSION`、`WorldSeed`、实体数，以及按 `(StableEntityId, Transform 位模式)` 排序后的每一项（平移 / 旋转 / 缩放共 10 个 `u32`）。
- 实体按稳定 ID 排序后再混入，所以 spawn 顺序不影响结果；没有 `Transform` 的实体（纯逻辑实体、体素体）不参与，避免把 Bevy 内部实体顺序带进校验和。
- 覆盖范围刻意保守：**不覆盖**体素内容、物品栏与脚本状态。脚本（Lua）不参与确定性，见上节；要覆盖新状态必须同时递增 `CHECKSUM_VERSION`，否则旧值会静默兼容。
- 三类用途：验证双跑（同一存档 / 输入跑两遍）、回滚等价（快照恢复后校验和回到快照值）、联机漂移诊断（客户端上报，主机比对）。

诊断快照 `StreamingDiagnostics.checksum` 每帧采集（`capture` 里调用，O(实体数)），Godot 侧 `streaming_diagnostics_to_dictionary` 以 `i64` 输出（`u64` 按位塞入，`-1` = 未采集），键名 `checksum`。校验和不参与玩法判定，也不写进存档。

测试 `verify::tests` 五个用例：spawn 顺序无关、Transform / 种子变化会改变结果、无稳定 ID 的实体不影响结果、同一批 fixed tick 双跑一致、Transform 快照恢复后校验和回到原值。

导入验证（上节 `verify_mod`）不另造校验和，而是在隔离 App 里复用 `simulation_checksum`，所以覆盖范围与上面完全一致。

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