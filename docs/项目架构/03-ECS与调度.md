---
title: ECS 与调度
type: 架构文档
status: active
created: 2026-10-19
updated: 2026-10-19
tags:
  - 项目架构
  - ECS
  - 调度
source: "代码阅读 @ commit 67d5a1f"
---

# ECS 与调度

## 装配单元

`GameModule<S: GameSpec>` 是一个游戏的装配单元，`GameModulePlugin` 把它当插件挂上。Bevy 不支持运行期替换已注册系统，所以换一整块玩法的写法是组合新的 `GameModule`，而不是从调度里抠系统。

## CoreGame 装配顺序

`game-core/src/app.rs` 的默认组合：

| 插件 | 作用 |
| --- | --- |
| `StaticDataPlugin` | 构建物品标签反查索引与 `WorldTables` |
| `GameInputPlugin` | 动作表、输入链、能力通道 |
| `PresentationPlugin` | 表现收集与发布、组件同步表 |
| `StableIdPlugin` | 稳定实体 id 分配 |
| `SpatialPlugin<GlobalTransform, Size>` | 空间索引，区块 32 m |
| `AoIPlugin<GlobalTransform>` | 观察者视野增量维护 |
| `GameplayPlugin` | `EngineSet` 阶段顺序 |
| `MovementPlugin` / `DeathPlugin` / `CameraPlugin` | 位移、死亡、朝向与相机 |
| `WorldPlugin` | 世界种子、岛屿、随机源 |
| `GameVoxelPlugin` | LOD 策略、体素世界层、碰撞 |
| `TerrainPlugin` | 地形生成与流式分块 |
| `PredictionPlugin` | 预测历史、渲染采样 |
| `register_synced_components` | 注册组件到载荷的收集系统 |

## CoreSpec：游戏给引擎的类型绑定

| 关联项 | 绑定 |
| --- | --- |
| `Actions` | `CoreActions` |
| `Payload` | `SyncPayload` |
| `Event` | `PresentationEvent` |
| `Semantics` | `SemanticRegistry` |
| `FIXED_HZ` | 60 |

引擎内部只认这些关联类型，不认识具体文案或数值。

## 逻辑 tick 的阶段链

`FixedUpdate` 按 `EngineSet` 固定顺序执行：

```mermaid
flowchart LR
  I[Input] --> A[Availability] --> D[Decision] --> M[Motion] --> C[Combat] --> R[Reaction] --> K[Cleanup]
```

| 阶段 | 内容 |
| --- | --- |
| `Input` | 采集、折叠输入、控制关系 |
| `Availability` | 能力求解，游戏侧插入 |
| `Decision` | AI 与决策、朝向 |
| `Motion` | 位移 |
| `Combat` | 结算 |
| `Reaction` | 判定，例如死亡检查 |
| `Cleanup` | 清理 |

输入细链在 `FixedPreUpdate` 里另有四个集合：`InputSet::Collect`、`Availability`、`Apply`、`Events`，按序链式执行。

## 表现侧阶段

| 阶段 | 时机 | 内容 |
| --- | --- | --- |
| `RefreshVisibilitySet` | `Update` | 刷新本地玩家、可见集、渲染时钟 |
| `CollectPresentationSet` | `Update` | 每个注册组件一个泛型系统，收集增量 |
| `FinalizePresentationSet` | `Update` | Attach、Add、Update、Remove、Detach、Despawn 整理后发布 |
| `advance_render_transforms` | `FixedPostUpdate` | 按 tick 记录 prev 与 curr |
| `RenderSampleSystems` | `FixedPostUpdate`，在上一项之后 | 按策略写 `PresentedTransform` |
| `RollbackSystems` | `FixedUpdate` 后段 | 每 tick 抽取本地实体状态写历史 |

## 确定性规则

1. 逻辑层数值一律 `FixedPoint`（I40F24，底层 i64），不使用 f32。
2. 会产出字节或决定顺序的容器只用 `BTreeMap` 或排序 `Vec`，禁止用 `HashMap` 迭代序。
3. 实体 id 用稳定分配（`fnv1a32` 与 `StableEntityId`），跨边界身份不用 ECS 的临时 `Entity`。
4. 系统顺序用命名 `SystemSet` 显式声明，不依赖并行调度器的偶然顺序。
5. 固定步长推进，单 tick 计算与帧率无关。

## 空间索引与 AOI

- 空间锚点用世界位置 `GlobalTransform`。逻辑系统写 `Transform`，同帧由 `sync_global_transforms` 推进。
- 区块边长 32 m（`SpatialPlugin::new(32.0)`），区块内一棵 `RTree`，超大包围盒进全局树。
- `SpatialChanges` 记录本帧变化的区块，AOI 只更新受影响的观察者。
- 视野覆盖区块超过 4096 个的观察者被当作全局观察者，不注册反查索引。

## 新增系统的落点

| 需求 | 落点 |
| --- | --- |
| 逻辑行为 | 某个 `EngineSet` 阶段 |
| 输入相关 | `InputSet` 对应集合 |
| 表现翻译 | `ToPresentation` 加一个 `collect_component` 注册 |
| 运行期策略替换 | `Strategy` trait 换资源 |
| 整块玩法 | 新的 `GameModule` |
