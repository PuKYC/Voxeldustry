---
title: 静态数据与 Mod 体系
type: 架构文档
status: active
created: 2026-10-19
updated: 2026-10-19
tags:
  - 项目架构
  - 静态数据
  - Mod
source: "代码阅读 @ commit 67d5a1f"
---

# 静态数据与 Mod 体系

## 静态数据的边界

`game-core/src/static_data` 是对象原型与物品定义的真相源：

| 模块 | 内容 |
| --- | --- |
| `item` | 物品定义、分类、子类、标签索引、堆叠 |
| `prototype` | 对象原型表 |
| `voxel` | 方块 id 与名字、生物群系、体素边长、LOD 距离阈值、`WorldTables`（材质外观不在这里） |

规则：

- 只依赖 `game_engine::ids`。不依赖 `presentation`，不依赖 `godot`。
- 数值一律用 `FixedPoint` 或整数表达，避免 f32 进入逻辑层。
- 表现层只消费只读快照：`godot-client-ext` 把表搬给 GDScript 的 `StaticTables`。

体素世界的关键常量：方块 id 0 到 7 依次是空气、石、土、草、沙、雪、木、叶；Rust 只导出 `block_names` 给 Godot 对账，材质外观与 block→面绑定都在 Godot 侧（见 [[06-体素世界与材质]]）。

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
| M0 | 数值与概率 | DefinitionPatch、数据补丁 |
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

`ModManifest` 声明 `id`、`version`、`engine_api`、`load_after`、`conflicts`。`engine_api` 是整数版本，只增不改。

## 改内容时的落点

| 想改什么 | 改哪里 |
| --- | --- |
| 材质外观（颜色 / 粗糙度 / 金属度 / 自发光 / 图案） | `godot-project/bevy_client/voxel_material/defs/*.tres`（VoxelMaterial），改完跑 `godot-project/tools/bake_voxel_materials.gd` 重新烘焙 |
| block → 六个面的材质绑定 | `godot-project/bevy_client/voxel_material/voxel_material_library.tres` 的 `blocks`（VoxelBlockMaterial） |
| 材质瓦片像素 | `godot-project/bevy_client/voxel_material/tiles/*.res`（bake 产物）；美术源图放 `godot-project/art/voxel_materials/`（该目录带 `.gdignore`，不导入、不进导出包，只做 bake 输入） |
| 新增材质 | 加 `godot-project/bevy_client/voxel_material/defs/<key>.tres`，追加到库的 `materials`，再重跑 bake（layer 顺序即材质 id） |
| 物品定义 | `static_data/item` |
| LOD 距离阈值 | `static_data/voxel.rs` 的 `LOD_DISTANCE_THRESHOLDS` |
| 表现语义名 | 运行期 `SemanticRegistry` 或 GDScript 的 `semantic_overrides` |
