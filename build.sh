#!/bin/bash
# 构建 godot-client-ext 动态库，并复制到 Godot 项目内的动态库目录。
#
# 用法:
#   ./build.sh                          # 原生 debug
#   ./build.sh --release                # 原生 release
#   ./build.sh --target <triple>        # 指定 target triple
#   ./build.sh --all                    # 所有已映射的平台
#   ./build.sh --dest <dir>             # 自定义输出根目录（默认 godot-project/bin）
#
# 环境变量:
#   CARGO_CMD   取代 cargo 的命令（CI 交叉编译可设为 cross）
#   NDK_ABI     设置后改用 `cargo ndk -t <abi>` 构建 Android（需安装 cargo-ndk）
#
# 产物命名: <dest>/<debug|release>/<prefix>godot_client_ext_<os>_<arch>.<ext>
set -euo pipefail

# 以脚本位置定位仓库根，保证从任意 cwd 调用都正确。
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

# 默认配置
PROFILE="debug"
RELEASE_FLAG=""
TARGET_TRIPLE=""
BUILD_ALL=false
DEST_DIR="$REPO_ROOT/godot-project/bin"
CARGO_CMD="${CARGO_CMD:-cargo}"
NDK_ABI="${NDK_ABI:-}"

# 定义所有支持的目标平台
ALL_TARGETS=(
    "x86_64-unknown-linux-gnu"
    "aarch64-unknown-linux-gnu"
    "x86_64-pc-windows-gnu"
    "x86_64-pc-windows-msvc"
    "aarch64-linux-android"
    "armv7-linux-androideabi"
    "x86_64-linux-android"
    "aarch64-apple-darwin"
    "x86_64-apple-darwin"
)

usage() {
    sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
}

# 解析命令行参数
while [[ $# -gt 0 ]]; do
    case $1 in
        --release) PROFILE="release"; RELEASE_FLAG="--release"; shift ;;
        --target)
            if [[ $# -lt 2 ]]; then echo "❌ --target 需要一个 target triple"; exit 1; fi
            TARGET_TRIPLE="$2"; shift 2 ;;
        --all) BUILD_ALL=true; shift ;;
        --dest)
            if [[ $# -lt 2 ]]; then echo "❌ --dest 需要一个目录"; exit 1; fi
            DEST_DIR="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "❌ 未知参数: $1"; usage; exit 1 ;;
    esac
done

# 核心构建函数
build_target() {
    local current_target=$1
    local os="" arch="" ext="" prefix=""

    # 精确映射 Target Triple 到 Godot 识别的系统与架构
    case "$current_target" in
        x86_64-pc-windows-gnu|x86_64-pc-windows-msvc) os="windows"; arch="x86_64"; ext="dll"; prefix="" ;;
        x86_64-unknown-linux-gnu) os="linux"; arch="x86_64"; ext="so"; prefix="lib" ;;
        aarch64-unknown-linux-gnu) os="linux"; arch="arm64"; ext="so"; prefix="lib" ;;
        aarch64-linux-android) os="android"; arch="arm64"; ext="so"; prefix="lib" ;;
        armv7-linux-androideabi) os="android"; arch="arm32"; ext="so"; prefix="lib" ;;
        x86_64-linux-android) os="android"; arch="x86_64"; ext="so"; prefix="lib" ;;
        aarch64-apple-darwin) os="macos"; arch="arm64"; ext="dylib"; prefix="lib" ;;
        x86_64-apple-darwin) os="macos"; arch="x86_64"; ext="dylib"; prefix="lib" ;;
        *)
            # 如果是原生编译（无 target），走默认逻辑
            if [ -z "$current_target" ]; then
                HOST_OS="$(uname -s)"
                HOST_ARCH="$(uname -m)"
                case "$HOST_OS" in
                    Linux) os="linux" ;;
                    Darwin) os="macos" ;;
                    MINGW*|MSYS*|CYGWIN*|Windows_NT) os="windows" ;;
                    *) echo "❌ 不支持的宿主机系统: $HOST_OS"; return 1 ;;
                esac
                case "$HOST_ARCH" in
                    x86_64|amd64) arch="x86_64" ;;
                    arm64|aarch64) arch="arm64" ;;
                    *) echo "❌ 不支持的宿主机架构: $HOST_ARCH"; return 1 ;;
                esac
                case "$os" in
                    windows) ext="dll"; prefix="" ;;
                    macos) ext="dylib"; prefix="lib" ;;
                    *) ext="so"; prefix="lib" ;;
                esac
            else
                echo "⚠️ 警告: 未映射的 Target ($current_target)，跳过..."; return 0
            fi
            ;;
    esac

    local built_lib_name="${prefix}godot_client_ext.${ext}"
    local final_lib_name="${prefix}godot_client_ext_${os}_${arch}.${ext}"
    local artifact_dir="target/$current_target/$PROFILE"
    local dest_dir="$DEST_DIR/$PROFILE"

    # 如果是原生编译（无 target），产物直接在 target/$PROFILE 下
    if [ -z "$current_target" ]; then
        artifact_dir="target/$PROFILE"
    fi

    local built_lib="$artifact_dir/$built_lib_name"
    local dest_lib="$dest_dir/$final_lib_name"

    echo "=========================================="
    echo "🚀 构建: $os $arch ($PROFILE)"
    if [ -n "$current_target" ]; then
        echo "Target Triple: $current_target"
    else
        echo "Target: Native (Host)"
    fi
    echo "输出: $dest_lib"
    echo "=========================================="

    # 执行构建。使用 if ! 以兼容 set -e，同时拿到失败状态。
    if [ -n "$NDK_ABI" ]; then
        # cargo-ndk：ABI 由 -t 指定，target triple 仅用于定位产物目录。
        if ! "$CARGO_CMD" ndk -t "$NDK_ABI" build $RELEASE_FLAG; then
            echo "❌ 构建失败: $os $arch"
            return 1
        fi
    elif [ -n "$current_target" ]; then
        if ! "$CARGO_CMD" build $RELEASE_FLAG --target "$current_target"; then
            echo "❌ 构建失败: $os $arch"
            return 1
        fi
    else
        if ! "$CARGO_CMD" build $RELEASE_FLAG; then
            echo "❌ 构建失败: $os $arch"
            return 1
        fi
    fi

    # 复制并重命名产物
    mkdir -p "$dest_dir"
    if [ ! -f "$built_lib" ]; then
        echo "❌ 错误: 未找到构建产物 $built_lib"
        return 1
    fi

    cp "$built_lib" "$dest_lib"

    # release 下尝试 strip（失败不影响构建）
    if [ "$PROFILE" = "release" ] && [ "$os" != "windows" ] && command -v strip >/dev/null 2>&1; then
        if ! strip "$dest_lib" 2>/dev/null; then
            echo "⚠️ strip 失败，保留未裁剪产物"
        fi
    fi

    echo "✅ 成功: $dest_lib"
    echo ""
    return 0
}

# 执行构建逻辑
if [ "$BUILD_ALL" = true ]; then
    echo "🔄 开始批量构建所有平台..."
    failed=()
    for t in "${ALL_TARGETS[@]}"; do
        build_target "$t" || failed+=("$t")
    done
    if [ ${#failed[@]} -gt 0 ]; then
        echo "❌ 以下平台构建失败: ${failed[*]}"
        exit 1
    fi
    echo "🎉 所有平台构建任务完成！"
elif [ -n "$TARGET_TRIPLE" ]; then
    build_target "$TARGET_TRIPLE"
else
    # 默认执行原生编译
    build_target ""
fi
