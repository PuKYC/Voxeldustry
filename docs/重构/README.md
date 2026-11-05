---
title: 质量重构文档
type: 重构文档
status: active
created: 2026-11-04
updated: 2026-11-04
tags:
  - 重构
  - 质量
  - 索引
source: git 与代码实测 @ refactor/quality d710c44（2026-11-05）
---

# 质量重构文档

> 范围：`crates/voxel`、`game-engine`、`game-core`、`godot-client-ext`。代码基线 `1c14c71`，集成分支 `refactor/quality`（tip `d710c44`）。

## 这次重构做什么

质量重构：不新增玩法、不改对外行为，只处理结构退化与工程门禁缺失。

| 问题 | 基线表现 | 目标 |
| --- | --- | --- |
| 文件过大 | `codec.rs` 3109 行、`terrain/mod.rs` 2184 行、`load.rs` 1447 行、`occupancy.rs` 1200 行 | 按职责拆成子模块，单文件回到可读规模 |
| 函数过长 | `settle_chunks` 187 行、`sync_voxel_collision_chunks` 245 行、`update_aoi` 166 行、`run_headless` 155 行、`extract_block_raw` 136 行 | 热点函数 ≤60 行（`settle_chunks` 为硬验收线） |
| 参数过多 | `too_many_arguments` 20 处；`settle_chunks` 13 参 | 全仓 ≤5 处；热点函数 ≤5 参 |
| clippy 未清零 | 149 条 warning（`4b1b016` 实测） | 全仓 0 条 |
| 文档 / 门禁 | 关键模块缺 rustdoc；CI lint 非阻塞 | 文档覆盖关键模块；lint 阻塞 |

## 一句话现状

八个重构单元、三路 clippy 清理与文档都已合入 `refactor/quality`（tip `d710c44`）：`settle_chunks` 187→55 行，`codec` 3109 行拆成 6 个文件，occupancy / terrain / load / stream / collision / backend / aoi 均完成拆分；929 passed / 12 ignored，`cargo fmt --all --check` 与 `cargo clippy --workspace --all-targets --all-features -- -D warnings` 均退出码 0（clippy 149→0）；`too_many_arguments` 20→1（只保留 Godot `#[func]` 签名契约这一处，带 reason）。CI lint 已改为阻塞门禁。

## 阅读顺序

| 顺序 | 文档 | 内容 |
| --- | --- | --- |
| 1 | [基线](基线.md) | 重构前的量化数据与统计口径 |
| 2 | [日志](日志.md) | 每个单元的提交、改法、前后指标、验证与偏差 |
| 3 | [当前状态与风险](11-当前状态与风险.md) | 验收达成表、剩余风险、后续建议 |

## 分支与提交

| 用途 | 位置 |
| --- | --- |
| 仓库 | `/home/dptui/ecs_game` |
| 代码基线 | `1c14c71`（fmt 全量 + clippy.toml + erasing_op 修复） |
| 集成分支 | `refactor/quality` @ `d710c44`（含全部重构与门禁提交） |
| 本文档分支 | `refactor/docs2` |
| 收尾分支（已合并） | `refactor/clippy-engine` @ `60a5a33` → `f8e6f8e`；`refactor/clippy-core` @ `62748df` → `acfe12a` |

## 单元总览

| 步骤 | 分支 | 合并提交 | 主题 |
| --- | --- | --- | --- |
| 0 基线准备 | — | `ff6eed3` / `621c3d7` / `1c14c71` | fmt 全量、clippy.toml、erasing_op |
| 1 流式加载 | `refactor/step1-stream` | `8dad901` | `settle_chunks` 拆分与 `SettleError` |
| 2 存档编解码 | `refactor/step2-codec` | `6b37684` | `codec.rs` 拆目录模块，修 2 个存量缺陷 |
| 3 碰撞同步 | `refactor/step3-collision` | `4b1b016` | 六函数拆分、确定性与基准 |
| 4a 体素存储 | `refactor/step4a` | `b21728e` | tracy 宏、occupancy 拆分与文档 |
| 4b 地形 | `refactor/step4b` | `13560cb` | terrain 拆子模块、降复杂度 |
| 4c 引擎热点 | `refactor/step4c` | `05a6dee` | `run_headless` / `update_aoi` 拆分 |
| 4d 世界加载 | `refactor/step4d` | `ef442f2` | `load.rs` 拆分、stream/load 测试外移 |
| 5 clippy（voxel） | `refactor/clippy-voxel` | `15be085` | voxel 52→0 |
| 5 收尾 | `refactor/clippy-engine`、`refactor/clippy-core` | `f8e6f8e` / `acfe12a` | game-engine / game-core / godot 清零、参数收拢 |
| 6 文档 | `refactor/docs2` | 本文档提交 | 基线、日志、现状 |
| 7 CI 门禁 | — | `d710c44` | lint job 改为阻塞，test 口径对齐（`--exclude godot-client-ext`） |

## 复现命令

```bash
# 格式
cargo fmt --all --check

# 测试（Godot 相关 crate 需要 Godot 头文件，本机排除）
cargo test --workspace --exclude godot-client-ext

# game-engine 的 codec / collision 在 voxel feature 下
cargo test -p game-engine --features voxel

# clippy（当前应为 0 警告）
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

## 口径

- 行数用 `wc -l`；函数行数含签名与注释；"参数个数"按顶层逗号切分。
- 性能数字来自本机 A/B（8 核 7.2G），场景与常量见 [日志](日志.md) 步骤 3；噪声内不宣称提升。
- clippy 基线 149 条在 `4b1b016` 实测，`1c14c71` 未单独全量复测（原因见 [基线](基线.md#口径)）。
- 收尾数字已回填：clippy 149→0，`too_many_arguments` 20→1，workspace 内 `#[allow]` 25→8。
- 本文档只记录已验证数据；未测量的项明确写"未测量"。
