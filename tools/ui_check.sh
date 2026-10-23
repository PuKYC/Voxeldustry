#!/bin/bash
# UI 静态检查 / headless 冒烟（无需编辑器）。
#
# 用法:
#   tools/ui_check.sh
#   GODOT_BIN=/path/to/godot tools/ui_check.sh
#
# 做的事情：
#   1. 生成 .godot/global_script_class_cache.cfg（无编辑器时的 class_name 缓存）；
#   2. 依次运行 UI 相关 headless 测试，任一失败即非零退出：
#      - ui_smoke_test.gd        HUD / 方案 / 槽位 / 动作按钮 / 暂停菜单
#      - ui_layout_test.gd       HudSlot 区域层多视口布局（844x390 / 1280x800 / 1920x1080）
#      - input_bindings_test.gd  语义绑定层 + 动作簇弧形布局
#      - ui_settings_test.gd     UiState 设置落盘 / 左手 / 浮动摇杆 / 暂停菜单设置区
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GODOT_BIN="${GODOT_BIN:-godot}"
PROJECT_DIR="$REPO_ROOT/godot-project"

if ! command -v "$GODOT_BIN" >/dev/null 2>&1 && [ ! -x "$GODOT_BIN" ]; then
    echo "找不到 Godot：GODOT_BIN=$GODOT_BIN" >&2
    exit 127
fi

run_test() {
    local script_path="res://test/$1"
    if [ ! -f "$PROJECT_DIR/test/$1" ]; then
        echo "[ui_check] 跳过（不存在）：$1"
        return 0
    fi
    echo "[ui_check] ==== $1 ===="
    "$GODOT_BIN" --headless --path "$PROJECT_DIR" --script "$script_path"
}

"$GODOT_BIN" --headless --path "$PROJECT_DIR" --script res://test/gen_class_cache.gd
run_test ui_smoke_test.gd
run_test ui_layout_test.gd
run_test input_bindings_test.gd
run_test ui_settings_test.gd

# 可选：真实场景集成探针（需要 godot-project/bin/debug 下已构建 GDExtension，会启动 Bevy 后端）。
if [ "${1:-}" = "--gameplay" ]; then
    run_test ui_gameplay_probe.gd
fi
