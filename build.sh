#!/bin/bash

# 默认配置
PROFILE="debug"
RELEASE_FLAG=""
TARGET_TRIPLE=""
BUILD_ALL=false

# 定义所有支持的目标平台
ALL_TARGETS=(
    "x86_64-unknown-linux-gnu"
    "aarch64-unknown-linux-gnu"
    "x86_64-pc-windows-gnu"
    "aarch64-linux-android"
    "armv7-linux-androideabi"
    "x86_64-linux-android"
    "aarch64-apple-darwin"
    "x86_64-apple-darwin"
)

# 解析命令行参数
while [[ $# -gt 0 ]]; do
    case $1 in
        --release) PROFILE="release"; RELEASE_FLAG="--release"; shift ;;
        --target) TARGET_TRIPLE="$2"; shift 2 ;;
        --all) BUILD_ALL=true; shift ;;
        *) echo "❌ 未知参数: $1"; exit 1 ;;
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
                    x86_64) arch="x86_64" ;;
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
    local dest_dir="target/$PROFILE"
    
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
    echo "=========================================="

    # 执行 Cargo 构建 (使用局部变量捕获状态，避免 set -e 导致直接退出)
    local BUILD_SUCCESS=true
    if [ -n "$current_target" ]; then
        cargo build $RELEASE_FLAG --target "$current_target" || BUILD_SUCCESS=false
    else
        cargo build $RELEASE_FLAG || BUILD_SUCCESS=false    
    fi

    if [ "$BUILD_SUCCESS" = false ]; then
        echo "❌ 构建失败: $os $arch"
        return 1
    fi

    # 复制并重命名产物
    mkdir -p "$dest_dir"
    if [ ! -f "$built_lib" ]; then
        echo "❌ 错误: 未找到构建产物 $built_lib"
        return 1
    fi

    cp "$built_lib" "$dest_lib"
    echo "✅ 成功: $dest_lib"
    echo ""
}

# 执行构建逻辑
if [ "$BUILD_ALL" = true ]; then
    echo "🔄 开始批量构建所有平台..."
    for t in "${ALL_TARGETS[@]}"; do
        build_target "$t"
    done
    echo "🎉 所有平台构建任务完成！"
elif [ -n "$TARGET_TRIPLE" ]; then
    build_target "$TARGET_TRIPLE"
else
    # 默认执行原生编译
    build_target ""
fi