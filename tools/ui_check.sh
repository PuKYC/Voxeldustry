#!/bin/bash
# UI 静态检查 / headless 冒烟（无需编辑器）。
#
# 用法:
#   tools/ui_check.sh
#   GODOT_BIN=/path/to/godot tools/ui_check.sh
#
# 做两件事：
#   1. 生成 .godot/global_script_class_cache.cfg（无编辑器时的 class_name 缓存）；
#   2. 运行 res://test/ui_smoke_test.gd，失败时非零退出。
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GODOT_BIN="${GODOT_BIN:-godot}"
PROJECT_DIR="$REPO_ROOT/godot-project"

if ! command -v "$GODOT_BIN" >/dev/null 2>&1 && [ ! -x "$GODOT_BIN" ]; then
    echo "找不到 Godot：GODOT_BIN=$GODOT_BIN" >&2
    exit 127
fi

"$GODOT_BIN" --headless --path "$PROJECT_DIR" --script res://test/gen_class_cache.gd
"$GODOT_BIN" --headless --path "$PROJECT_DIR" --script res://test/ui_smoke_test.gd

# 可选：真实场景集成探针（需要 godot-project/bin/debug 下已构建 GDExtension，会启动 Bevy 后端）。
if [ "${1:-}" = "--gameplay" ]; then
    "$GODOT_BIN" --headless --path "$PROJECT_DIR" --script res://test/ui_gameplay_probe.gd
fi
