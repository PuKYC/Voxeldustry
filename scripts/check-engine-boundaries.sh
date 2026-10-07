#!/usr/bin/env bash
# P7 边界验收：
#   1) game-engine 依赖树不得包含 replicon / godot；
#   2) game-core 不得再暴露旧 shim 路径；
#   3) engine 单测 + 最小 App 集成测试通过。
set -euo pipefail
cd "$(dirname "$0")/.."

echo "[1/3] engine 依赖树不含 replicon / godot"
if cargo tree -p game-engine -e normal | grep -iE 'replicon|godot'; then
  echo "FAIL: game-engine 依赖了 replicon / godot" >&2
  exit 1
fi

echo "[2/3] game-core 不再暴露旧 shim"
if grep -nE 'pub use game_engine::(math|rng|ids|aoi|base_components|spatial as spatial_partition|identity as stable_id)' game-core/src/lib.rs; then
  echo "FAIL: game-core 仍暴露旧 shim" >&2
  exit 1
fi

echo "[3/3] engine 单测 + 最小 App 集成测试"
cargo test -p game-engine --quiet

echo "OK"
