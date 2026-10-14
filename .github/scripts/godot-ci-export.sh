#!/usr/bin/env bash
# 在 godot-ci 容器内运行，用于诊断 Godot 启动/导入崩溃。
# 由 .github/workflows/ci.yml 调用；输出由宿主侧捕获并作为注解暴露。
echo "=== env ==="
uname -a
godot --version || true
grep -m1 '^flags' /proc/cpuinfo | tr ' ' '\n' | grep -E '^(sse4_2|avx|avx2|fma|bmi2|avx512f)$' | tr '\n' ' '
echo ""

echo "=== probe 1: open project ==="
godot --headless --path godot-project --quit
echo "OPEN_EXIT=$?"

echo "=== probe 2: import WITH extension ==="
godot --headless --verbose --path godot-project --import
echo "IMPORT_EXIT=$?"

echo "=== probe 3: import WITHOUT extension ==="
mv godot-project/main.gdextension /tmp/main.gdextension.bak
godot --headless --verbose --path godot-project --import
echo "IMPORT_NOEXT_EXIT=$?"
mv /tmp/main.gdextension.bak godot-project/main.gdextension

echo "=== probe 4: export debug ==="
godot --headless --verbose --path godot-project --export-debug "Linux x86_64" "build/linux/game.x86_64"
echo "EXPORT_EXIT=$?"

if [ -f build/linux/game.x86_64 ]; then
  exit 0
fi
exit 1
