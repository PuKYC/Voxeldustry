# Voxeldustry

体素（voxel）沙盒游戏。核心是 Rust 写的确定性模拟，跑在后台线程的 Bevy ECS 里；Godot 4 只负责输入采集与画面表现，两侧通过纯数据通道交换信息——Godot 侧不认识 ECS，Rust 侧不认识 Godot 节点。

## 架构速览

依赖严格单向，反向依赖视为违规：

```text
godot-project (GDScript 表现层)
      │  GDExtension / Variant
godot-client-ext (cdylib，唯一桥接 crate)
      │  Rust 纯数据
game-core (游戏规则 · 装配 · 载荷)
      │
game-engine (确定性 ECS 机制)
      │  feature = voxel
crates/voxel (整数 SVO-DAG 体素)
```

逻辑层只用定点数（I40F24），不出现 f32 与 HashMap 迭代序，因此同一输入序列在任何机器上得到同一结果。

## 构建与测试

依赖 **Rust 1.95**（`workspace.package.rust-version`，CI 固定同一版本）、**Godot 4.7.2**（客户端运行与导出）。

```bash
cargo build --workspace          # 构建全部 crate
cargo test --workspace           # 单元测试 + 集成测试
bash ./build.sh                  # 构建 godot-client-ext 并复制进 godot-project/bin/debug
```

`godot-client-ext` 是 GDExtension 动态库，依赖 `godot` crate 的绑定，只有放进 Godot 4.7.2 才有意义（用 Godot 打开 `godot-project`，主场景 `test/node_3d.tscn`）。只验证 Rust 逻辑时跳过它：

```bash
cargo test --workspace --exclude godot-client-ext
```

## 目录结构

四个 workspace 成员加一个 Godot 工程：

| 路径 | 产物 | 职责 |
| --- | --- | --- |
| `crates/voxel` | rlib | 整数 SVO-DAG 体素存储、贪婪网格、LOD 四叉树、确定性存档 |
| `game-engine` | rlib | 通用确定性 ECS 机制：装配接缝、输入、表现管线、空间与 AOI、感知、回滚、Mod、体素世界层 |
| `game-core` | rlib | 本游戏的装配与规则：`CoreGame`、玩法、静态数据、体素游戏层、表现载荷、预测绑定、后台入口 |
| `godot-client-ext` | cdylib | 唯一同时认识 Godot 与 `game-core` 的 crate，只做 `Variant ↔ game_core` 机械转换 |
| `godot-project` | pck | GDScript 表现层、场景、`.gdextension` |

根目录另有 `build.sh`（跨平台构建脚本）、`tools/ui_check.sh`（UI headless 冒烟）、`.github/workflows/`（CI 与发布）。`crates/chunky-bevy` 是 vendored 参考实现，不在 workspace members 里，不参与构建。

## 文档入口

- [`docs/项目架构/`](docs/项目架构/)：分层与依赖、线程模型、ECS 与调度、输入链路、表现管线、体素世界与材质、网络与回滚、静态数据与 Mod、构建测试与 CI、Godot UI、当前状态与风险（编号 00-11）。
- [`docs/ci.md`](docs/ci.md)：工作流、版本固定、发布矩阵、Android 签名、性能门禁与故障排查。
- [`docs/UI适配改造变更说明.md`](docs/UI适配改造变更说明.md)：一轮 UI 适配改造的变更说明。

重构类的演进过程目前记录在提交历史（`git log --grep=refactor`）与 `docs/项目架构/11-当前状态与风险.md` 里；`docs/重构/` 目录尚未建立。`docs/待办/` 是本地临时文档目录，已 gitignore，克隆后不存在属正常。

## 常用命令

```bash
cargo fmt --all                                        # 格式化
cargo clippy --workspace --all-targets                 # 静态检查
cargo test --workspace --exclude godot-client-ext      # 纯 Rust 测试
cargo bench -p game-core --bench voxel_terrain         # criterion 墙钟基准
```

基准分两套：criterion 看墙钟耗时（`mesh_rebuild` / `voxel_rebuild` / `voxel_collider` / `voxel_terrain`），gungraun 统计 Valgrind Callgrind 指令数（`terrain_inst` / `voxel_collider_inst`，仅 Linux，需要 `valgrind` 与版本对齐的 `gungraun-runner`）；具体命令与门禁阈值见 [`docs/ci.md`](docs/ci.md) 第 9 节。

## 贡献

提交前检查项、代码规模上限、提交信息格式见 [CONTRIBUTING.md](CONTRIBUTING.md)。
