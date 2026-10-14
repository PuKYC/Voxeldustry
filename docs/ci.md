# CI 与发布（GitHub Actions）

本文档描述仓库的持续集成与发布流程，以及本地复现方式。

## 1. 总览

| 工作流 | 触发 | 内容 |
| --- | --- | --- |
| `.github/workflows/ci.yml` | 每次 push（任意分支）、PR | Rust **debug** 构建 + 测试；Godot **debug** 导出 |
| `.github/workflows/release.yml` | 推送 `v*` tag，或手动 `workflow_dispatch` | Rust **release** 构建 + Godot **release** 导出，汇总为 GitHub Release |
| `.github/workflows/generate-android-keystore.yml` | 手动 `workflow_dispatch` | 一次性生成 Android release keystore，并（可选）自动写入 Secrets |

两者都用 `ubuntu-24.04`（Godot 4 需要 24.04），Godot 由 `chickensoft-games/setup-godot` 安装到 runner。

## 2. 版本固定

改动时三者要一起改：

| 项 | 位置 | 当前值 |
| --- | --- | --- |
| Rust 工具链 | 各 workflow 的 `RUST_TOOLCHAIN`，以及 `Cargo.toml` 的 `workspace.package.rust-version` | `1.95` |
| Godot（编辑器/导出模板） | 各 workflow 的 `GODOT_VERSION`（`setup-godot`） | `4.7.2` |
| Godot 项目特性 | `godot-project/project.godot` 的 `config/features` | `4.7` |

> `godot-project/main.gdextension` 用 `compatibility_minimum = 4.2`，Rust 侧 `godot = { features = ["api-4-2"] }`：
> 扩展按 4.2 API 编译，可被更高版本 Godot 加载，因此“兼容旧 Godot”无需额外处理。

## 3. 动态库路径约定

`build.sh` 把产物复制到 **Godot 项目内**：

```
godot-project/bin/<debug|release>/<prefix>godot_client_ext_<os>_<arch>.<ext>
```

例如 `godot-project/bin/debug/libgodot_client_ext_linux_x86_64.so`。
`godot-project/main.gdextension` 里全部用 `res://bin/<debug|release>/...` 引用。

debug 与 release 分目录存放，避免同名互相覆盖。`godot-project/bin/`、`/build/`、`/.android-keystore/` 均已 gitignore。

`./build.sh` 常用参数：

| 命令 | 说明 |
| --- | --- |
| `bash ./build.sh` | 原生 debug |
| `bash ./build.sh --release` | 原生 release |
| `bash ./build.sh --target <triple>` | 指定 target |
| `bash ./build.sh --all` | 所有已映射平台（任一失败即中止） |
| `bash ./build.sh --dest <dir>` | 自定义输出根目录（默认 `godot-project/bin`） |

环境变量：`CARGO_CMD`（可设为 `cross`）、`NDK_ABI`（设置后用 `cargo ndk -t <abi>` 构建 Android）。

## 4. CI（.github/workflows/ci.yml）

- `rust` job：
  1. 安装 Rust `1.95`（含 rustfmt/clippy）+ cargo 缓存
  2. `cargo fmt --all --check` / `cargo clippy --workspace --all-targets -- -D warnings`（**非阻塞**，稳定后可去掉 `continue-on-error`）
  3. `cargo test --workspace`
  4. `bash ./build.sh`（debug）
  5. 上传 `godot-project/bin/debug/`
- `godot` job（依赖 rust）：
  1. 取回 debug 动态库
  2. 用 `chickensoft-games/setup-godot` 安装 Godot 4.7.2 + 导出模板
  3. `godot --headless --path godot-project --import` + `--export-debug "Linux x86_64"`
  4. 上传 `build/`

> Godot 直接装在 runner 上（不再使用 godot-ci 容器），`godot` 命令由 action 加入 PATH；
> 导出模板安装到 `~/.local/share/godot/export_templates/4.7.2.stable/`。

## 5. 发布（.github/workflows/release.yml）

### 触发

- 推送形如 `v0.1.0` 的 tag；或
- 手动 `workflow_dispatch`，填写 `tag` 输入。

### 构建矩阵（每个平台一个 leg）

| name | Rust target | Godot 预设 | 产物 | experimental |
| --- | --- | --- | --- | --- |
| `linux-x86_64` | `x86_64-unknown-linux-gnu` | `Linux x86_64` | `build/linux/game.x86_64` | 否 |
| `linux-arm64` | `aarch64-unknown-linux-gnu`（cross） | `Linux arm64` | `build/linux/game.arm64` | 否 |
| `windows-x86_64` | `x86_64-pc-windows-gnu`（MinGW） | `Windows Desktop x64` | `build/windows/game.exe` | 否 |
| `android-arm64` | `aarch64-linux-android` + `armv7-linux-androideabi` | `Android arm64` | `build/android/game.apk` | **是** |

每个 leg 的步骤：

1. 安装 Rust target（Android 额外装 NDK r25b + `cargo-ndk`，用于交叉编译扩展）
2. **构建宿主动态库**（`bash ./build.sh`，Linux x86_64 debug）——Godot 编辑器导入场景时会实例化原生类 `BevyAppManager`，缺宿主库会导致导入/导出失败
3. 构建目标平台 release 动态库
4. Android 专属：装 JDK 17 + Android SDK（`platform-tools`、`platforms;android-36`、`build-tools;36.1.0`、`ndk;29.0.14206865`，与 Godot 4.7.2 的 `app/config.gradle` 对齐）+ debug keystore
   → 解码 release keystore → 写签名配置 → `--install-android-build-template`
5. 用 `godot --headless` 执行 `--import` + `--export-release "<preset>"`（Android 先 `--install-android-build-template`）
6. 上传 `build/`

### 汇总

`release` job 下载全部 artifact → 每个平台打一个 zip → `softprops/action-gh-release` 发布。
`experimental` 平台失败不会阻断其它平台发布。

## 6. Android release 签名（必填 Secrets）

Android 预设开启了 `gradle_build/use_gradle_build=true` 且 `package/signed=true`，发布 APK 需要 release keystore。

在 `Settings → Secrets and variables → Actions` 添加：

| Secret | 值 |
| --- | --- |
| `ANDROID_KEYSTORE_BASE64` | keystore 文件的单行 base64 |
| `ANDROID_KEYSTORE_ALIAS` | key alias（Godot 的 `keystore/release_user`） |
| `ANDROID_KEYSTORE_PASSWORD` | store / key 密码 |

生成 base64：

```bash
base64 -w 0 release.keystore        # Linux
base64 -i release.keystore          # macOS
```

生成 keystore（若还没有）：

```bash
keytool -genkeypair -v -keystore release.keystore \
  -alias mykey -keyalg RSA -keysize 2048 -validity 10000
```

### 推荐：用 GitHub Actions 生成（无需本地 keytool）

仓库内置一次性工作流 `.github/workflows/generate-android-keystore.yml`：

1. GitHub → **Actions** → **Generate Android release keystore** → **Run workflow**；
   可填 `alias`（默认 `release`）、`password`（留空则自动生成随机强密码）、`validity_days`。
2. 证书主体直接使用 GitHub 上下文：`CN=<github.actor>, OU=<owner/repo>, O=<owner>, L=GitHub, ST=GitHub, C=US`。
3. 结果发布方式：
   - 若已配置 **`GH_PAT`**（有 `Secrets: Read and write` 权限的细粒度 PAT），工作流**自动写入**其余 3 个 Secrets（推荐）；
   - 否则（**仅私有仓库**）把 3 个值打印到该 run 的 **Summary** 供手动复制；公开仓库会直接拒绝，避免私钥外泄。
4. 生成的 keystore 作为 artifact `android-release-keystore` 保留 1 天，作为备份。

> ⚠️ 生成后请删除该 workflow run（含 Summary）与 artifact；keystore 与密码务必离线多处备份。

CI 中由 `.github/scripts/configure_android_keystore.py` 在导出前把下列字段写入
`godot-project/export_presets.cfg` 的 Android 预设（按预设 **名字** 定位，不依赖固定的 `preset.N`）：

```ini
keystore/release="/home/runner/work/<repo>/<repo>/.android-keystore/release.keystore"
keystore/release_user="<alias>"
keystore/release_password="<password>"
```

> 路径是 runner 上的**绝对路径**（CI 用 `${{ github.workspace }}/.android-keystore/release.keystore`），
> keystore 放在仓库根的 `.android-keystore/`（已 gitignore）。缺 secret 时该 leg 会在校验处失败，但不会阻断其它平台。

## 7. 本地复现

```bash
# debug：构建 + 导出（Linux）
bash ./build.sh
godot --headless --path godot-project --import
godot --headless --path godot-project --export-debug "Linux x86_64" build/linux/game.x86_64

# release 某平台（示例：Linux x86_64）
bash ./build.sh --release --target x86_64-unknown-linux-gnu
godot --headless --path godot-project --import
godot --headless --path godot-project --export-release "Linux x86_64" build/linux/game.x86_64

# 手动配置 Android 签名（本地）
mkdir -p .android-keystore && cp release.keystore .android-keystore/
ANDROID_KEYSTORE_PATH="$PWD/.android-keystore/release.keystore" \
ANDROID_KEYSTORE_ALIAS=mykey \
ANDROID_KEYSTORE_PASSWORD=*** \
python3 .github/scripts/configure_android_keystore.py
```

> 本地复现需先自行安装 Godot 4.7.2 与对应导出模板；keystore 路径用仓库内的绝对路径即可。

## 8. 已知注意事项 / 待办

- **Android 仍是 `experimental: true`**：首次跑通后改成 `false` 变为强门禁。
- **Windows 用 MinGW（`x86_64-pc-windows-gnu`）交叉编译**。若遇到加载兼容性问题，可改为
  `runs-on: windows-latest` + `x86_64-pc-windows-msvc`（`build.sh` 已支持该 triple）。
- **macOS 暂未纳入发布矩阵**；`.gdextension` 里保留了 macos 条目，导出预设暂缺。
- `setup-rust-toolchain@v1` 默认 `RUSTFLAGS=-D warnings`；本仓库尚未清完 warning，因此显式设为 `rustflags: ""`，warning 门禁只保留在非阻塞的 clippy 步骤。
- `cargo clippy` 目前 `continue-on-error`，待代码全绿后去掉。
- Godot 版本升级时，同步修改各 workflow 的 `GODOT_VERSION` 与 `project.godot` 的 `config/features`。
- Android 导出不再依赖 godot-ci 镜像，改为 runner 上的 `actions/setup-java`（temurin 17）+ `android-actions/setup-android`（显式安装 android-36 / build-tools 36.1.0 / NDK 29.0.14206865）+ 自建 debug keystore。
  Godot 升级时这三个版本要跟着 `platform/android/java/app/config.gradle` 一起改。

## 9. 故障排查

| 现象 | 可能原因 / 处理 |
| --- | --- |
| `Android build template not installed` | 导出前未执行 `--install-android-build-template`；见第 5/6 节 |
| Android release 签名失败 | 三个 secret 缺失或 alias/密码不匹配 |
| 导出时报找不到 `res://bin/...` | 动态库没有放到对应 profile 目录，或构建 target 与预设平台不一致 |
| 编辑器导入报 `BevyAppManager` 找不到 | 缺宿主平台（linux.debug.x86_64）动态库 |
| `Permission denied: ./build.sh` | 用 `bash ./build.sh`（CI 已如此调用） |
| 需要旧版 Godot | 扩展按 `api-4-2` 编译，可被 ≥4.2 的 Godot 加载；但 CI 导出固定用 4.7.2 |
