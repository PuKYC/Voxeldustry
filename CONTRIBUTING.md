# 贡献指南

本仓库以「小步重构」为主：先让代码可读、可测，再加功能。下面几条是提交前必须过的最低标准。

## 提交前必须

```bash
cargo fmt --all
cargo clippy --workspace --all-targets
cargo test --workspace --exclude godot-client-ext
```

- `cargo fmt --all`：仓库没有 `rustfmt.toml`，用默认风格，提交前必须已格式化。
- `cargo clippy --workspace --all-targets`：**不新增 warning**。CI 的 lint job 目前是 `continue-on-error`（非阻塞），但门禁迟早会转成阻塞，别把新 warning 留给下一个人。
- `cargo test --workspace --exclude godot-client-ext`：纯 Rust 单元测试与集成测试。`godot-client-ext` 是 GDExtension 桥接 crate，验证它需要 Godot 4.7.2 环境，本地有 Godot 时再单独跑。

改到 `godot-project/` 或 GDScript 时，另外跑：

```bash
tools/ui_check.sh          # UI headless 冒烟，需要 GODOT_BIN 或 PATH 里的 godot
```

## 代码规模上限

| 项 | 上限 | 说明 |
| --- | --- | --- |
| 单个函数 | ≤ 60 行 | 超了先拆函数，不要靠注释分段硬撑 |
| 圈复杂度 | ≤ 25 | 分支嵌套多时优先提取早返回与纯函数 |
| 参数个数 | ≤ 7 | `clippy.toml` 的 `too-many-arguments-threshold = 7` 已生效，超了直接报 warning |

> `clippy.toml` 里另有两项与上表不完全对应：`cognitive-complexity-threshold = 20`（比圈复杂度 25 更严）、`too-many-lines-threshold = 100`（比 60 行更宽）。以本表的人评为准，clippy 只是下限兜底；两者冲突时按更严的那条执行。

## 不要用 lint 抑制换清净

- **不新增** `#[allow(...)]` / `#[expect(...)]`。先改代码；确实改不动（例如 FFI 签名、上游 vendored 代码）才保留，并且必须写 `reason`：

  ```rust
  #[allow(clippy::too_many_arguments, reason = "镜像 Godot 侧按钮导出签名，拆参会破坏 FFI 契约")]
  ```

- 存量里还有二十余处只写了 lint 名的 `#[allow]`，改到附近时顺手补 `reason`；没有 `reason` 的新增一律在评审阶段打回。
- 公开项写文档注释用 `///`，不要用 `//` 顶替；模块级说明用 `//!`。

## 提交信息

格式：`类型(模块): 做了什么`，模块用受影响的目录或板块名（`save`、`voxel`、`streaming`、`presentation`、`input` 等），描述用中文、动词开头、一句话说清「做了什么」而不是「改了哪几行」。

```text
refactor(save): 把 codec.rs 拆成按职责划分的目录模块
fix(voxel): 测试中 erasing_op 的 0*n 索引改成常量
feat(streaming): 装配默认世界物体（树），重复注册先判重
```

- 类型只用 `refactor` / `fix` / `feat`（纯格式化可用 `style`），不要自造前缀。
- 节奏上要求**每三个 `feat` 至少配一个 `refactor`**：功能长在难读的代码上，下一轮就会拖慢所有人。
- 一个提交只做一件事；重构与行为变更不要混在同一个提交里，否则 review 时分不清哪一行改了行为。

## 重构的边界

重构改动**必须保持公共 API 与错误文案不变**：函数签名、类型名、模块路径、`Display` / 错误字符串都算契约。

确需变更时，同一个 PR 里必须同时：

1. 更新受影响的文档（`docs/项目架构/` 对应笔记、必要时 `docs/ci.md`）；
2. 更新或新增契约测试（例如载荷枚举双侧一致性、FFI 装配自检、体素 LOD / 碰撞验收）。

只改代码不改契约测试的「顺手重命名」，视为破坏性变更，评审直接拒绝。

## 文档同步

- 改 CI / 构建流程时同步改 `docs/ci.md`，它是工作流的唯一说明来源。
- 改架构（分层、线程、数据流）时改 `docs/项目架构/` 里对应的那一篇，不要新开重复笔记。
- 一次性交接稿、设计手稿放 `docs/待办/`（已 gitignore），沉淀完回填到正式笔记再删。
