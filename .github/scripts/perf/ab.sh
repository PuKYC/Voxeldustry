#!/usr/bin/env bash
# 性能基准 A/B 对比：在同一台机器上交替跑「基线代码」和「当前代码」。
#
# 为什么不比「入库基线」：GitHub 托管 runner 的 CPU 型号/频率/邻居负载每次都不同，
# 跨机器的绝对耗时能差 20% 以上，门禁只会变成 flaky。
#
# 为什么不是「基线跑一遍、当前跑一遍」就完事：整台机器的状态会漂移（温度、内存压力、
# 频率调节），一整轮可能整体快或慢 30%，两次一前一后就变成假回归。这里改成：
#
#   第 1 轮：基线 → 当前
#   第 2 轮：当前 → 基线        （顺序对调，抵消「后跑的那侧吃亏」）
#
# 判定时按同一轮次配对算 Δ，取各轮里最小的 Δ：**每一轮都变慢才算回归**。
# 真实回归在顺序对调后依然存在，而整机抖动不会。
#
# 用法:
#   bash .github/scripts/perf/ab.sh                 # 自动选基线：origin/main → main → HEAD^
#   bash .github/scripts/perf/ab.sh origin/main     # 指定基线 ref / sha
#
# 环境变量:
#   PERF_PROFILE=ci|full   采样强度。ci 默认（约 4.5s/项）；full 用 criterion 默认值
#   PERF_REPEAT=<n>        每侧重复轮数，默认 2（想快速看结果就设 1）
#   PERF_ONLY=<子串>       只跑 <包>/<bench target> 匹配的项，例：PERF_ONLY=voxel_collider
#   PERF_OUT=<dir>         报告/日志目录，默认 target/perf-out
#   CARGO_TARGET_DIR       构建目录，默认 <repo>/target
#
# 退出码: 0 = 无回归；1 = 有基准超过阈值；2 = 配置/环境错误。
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$REPO_ROOT"

PROFILE="${PERF_PROFILE:-ci}"
REPEAT="${PERF_REPEAT:-2}"
OUT_DIR="${PERF_OUT:-$REPO_ROOT/target/perf-out}"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
GATE="$REPO_ROOT/.github/scripts/perf/gate.py"
THRESHOLDS="$REPO_ROOT/.github/scripts/perf/thresholds.json"

# 基准清单：<package>:<features|->:<bench target>
# 与各 crate 的 [[bench]] 声明一一对应；新增 bench 时在这里补一行。
BENCHES=(
  "voxel:-:mesh_rebuild"
  "game-engine:voxel:voxel_rebuild"
  "game-engine:voxel,physics:voxel_collider"
  "game-core:-:voxel_terrain"
)

case "$CARGO_TARGET_DIR" in
  /*) ;;
  *) CARGO_TARGET_DIR="$REPO_ROOT/$CARGO_TARGET_DIR" ;;
esac
export CARGO_TARGET_DIR

case "$REPEAT" in
  ''|*[!0-9]*) echo "错误: PERF_REPEAT 必须是正整数（当前 '$REPEAT'）" >&2; exit 2 ;;
  *) [ "$REPEAT" -ge 1 ] || { echo "错误: PERF_REPEAT 至少为 1" >&2; exit 2; } ;;
esac

mkdir -p "$OUT_DIR"
rm -rf "${OUT_DIR:?}/results"
mkdir -p "$OUT_DIR/results"

# criterion 参数。ci profile 缩短采样时间；full 用 criterion 默认值（本地深挖用）。
case "$PROFILE" in
  ci)   CRIT=(--noplot --warm-up-time 1 --measurement-time 3 --sample-size 30) ;;
  full) CRIT=(--noplot) ;;
  *)    echo "错误: PERF_PROFILE 只能是 ci 或 full（当前 '$PROFILE'）" >&2; exit 2 ;;
esac

# ---- 选择要跑的基准 --------------------------------------------------------
SELECTED=()
for entry in "${BENCHES[@]}"; do
  IFS=: read -r pkg feats bench <<< "$entry"
  if [ -n "${PERF_ONLY:-}" ]; then
    case "$pkg/$bench" in
      *"$PERF_ONLY"*) SELECTED+=("$entry") ;;
      *) ;;
    esac
  else
    SELECTED+=("$entry")
  fi
done
if [ "${#SELECTED[@]}" -eq 0 ]; then
  echo "错误: PERF_ONLY='${PERF_ONLY:-}' 没有匹配到任何基准" >&2
  exit 2
fi

bench_args() {
  local entry="$1"
  local pkg feats bench
  IFS=: read -r pkg feats bench <<< "$entry"
  BENCH_CMD=(cargo bench -p "$pkg" --bench "$bench")
  if [ "$feats" != "-" ]; then
    BENCH_CMD+=(--features "$feats")
  fi
}

# 先把两侧的 bench 二进制编好，测量阶段就只剩跑，不会被编译抢占 CPU。
# 基线侧允许缺 target（本 PR 新增的 bench 在旧 commit 上不存在），当前侧不允许。
prebuild() {
  local dir="$1" tolerant="$2"
  local entry status
  for entry in "${SELECTED[@]}"; do
    bench_args "$entry"
    echo "==> [build] $dir: ${BENCH_CMD[*]}"
    status=0
    ( cd "$dir" && "${BENCH_CMD[@]}" --no-run ) || status=$?
    if [ "$status" -ne 0 ]; then
      if [ "$tolerant" = "1" ]; then
        echo "::warning:: 基线侧没有 ${entry}（可能是本 PR 新增的 bench），跳过"
        continue
      fi
      echo "错误: 当前侧编不过 ${entry}" >&2
      return "$status"
    fi
  done
}

# 跑一侧的所有基准。基线侧跑不通就跳过；当前侧跑不通直接失败，避免静默少测。
run_benches() {
  local dir="$1" logfile="$2" label="${3:-run}" tolerant="${4:-0}"
  local entry status
  for entry in "${SELECTED[@]}"; do
    bench_args "$entry"
    echo "::group::[$label] ${BENCH_CMD[*]}"
    status=0
    ( cd "$dir" && "${BENCH_CMD[@]}" -- "${CRIT[@]}" ) 2>&1 | tee -a "$logfile" || status=$?
    echo "::endgroup::"
    if [ "$status" -ne 0 ]; then
      if [ "$tolerant" = "1" ]; then
        echo "::warning:: 基线侧没有跑通 ${entry}，跳过该项对比"
        continue
      fi
      echo "错误: 当前侧跑挂了 ${entry}" >&2
      return "$status"
    fi
  done
}

# 把这一轮 criterion 新写出的 new/estimates.json 快照到 results/<side>/<pass>/。
snapshot() {
  local side="$1" pass="$2" marker="$3"
  local dest="$OUT_DIR/results/$side/$pass"
  rm -rf "$dest"
  while IFS= read -r f; do
    local id="${f#"$CARGO_TARGET_DIR/criterion/"}"
    id="${id%/new/estimates.json}"
    mkdir -p "$dest/$id"
    cp "$f" "$dest/$id/estimates.json"
  done < <(find "$CARGO_TARGET_DIR/criterion" -type f -path '*/new/estimates.json' -newer "$marker" 2>/dev/null || true)
}

# ---- 解析基线 --------------------------------------------------------------
BASE_REF="${1:-}"
if [ -z "$BASE_REF" ]; then
  for candidate in origin/main main HEAD^; do
    if git rev-parse --verify -q "${candidate}^{commit}" >/dev/null 2>&1; then
      BASE_REF="$candidate"
      break
    fi
  done
fi

BASE_SHA=""
if [ -n "$BASE_REF" ] && BASE_SHA="$(git rev-parse --verify -q "${BASE_REF}^{commit}" 2>/dev/null)"; then
  :
fi

HEAD_SHA="$(git rev-parse HEAD)"
echo "仓库:        $REPO_ROOT"
echo "当前:        $HEAD_SHA"
echo "基线:        ${BASE_SHA:-<无>} ${BASE_REF:+($BASE_REF)}"
echo "采样档位:    $PROFILE × $REPEAT 轮"
echo "criterion:   $CARGO_TARGET_DIR/criterion"
echo "输出:        $OUT_DIR"

: > "$OUT_DIR/head.log"
: > "$OUT_DIR/base.log"

if [ -z "$BASE_SHA" ]; then
  echo "::warning:: 找不到可用的基线 ref，本次只采集当前耗时，不做回归门禁"
  marker="$OUT_DIR/.marker-head"
  touch "$marker"
  run_benches "$REPO_ROOT" "$OUT_DIR/head.log" measure 0
  snapshot head 1 "$marker"
  rm -f "$marker"
  python3 "$GATE" \
    --results "$OUT_DIR/results" \
    --thresholds "$THRESHOLDS" \
    --head-ref "$HEAD_SHA" \
    --report "$OUT_DIR/report.md" \
    --json "$OUT_DIR/report.json"
  exit $?
fi

# ---- 在基线 commit 上开 worktree -------------------------------------------
BASE_WT="$OUT_DIR/base-worktree"
cleanup() {
  git worktree remove --force "$BASE_WT" >/dev/null 2>&1 || rm -rf "$BASE_WT"
  git worktree prune >/dev/null 2>&1 || true
}
trap cleanup EXIT

git worktree prune >/dev/null 2>&1 || true
rm -rf "$BASE_WT"
git worktree add -q --detach --force "$BASE_WT" "$BASE_SHA"

prebuild "$BASE_WT" 1
prebuild "$REPO_ROOT" 0

# ---- 交替跑若干轮 ----------------------------------------------------------
for pass in $(seq 1 "$REPEAT"); do
  if [ $((pass % 2)) -eq 1 ]; then
    order=("base" "head")
  else
    order=("head" "base")
  fi
  for side in "${order[@]}"; do
    if [ "$side" = "base" ]; then
      dir="$BASE_WT"; sha="$BASE_SHA"
    else
      dir="$REPO_ROOT"; sha="$HEAD_SHA"
    fi
    echo "==> 第 $pass/$REPEAT 轮 · $side ($sha)"
    marker="$OUT_DIR/.marker-$side-$pass"
    touch "$marker"
    if [ "$side" = "base" ]; then tolerant=1; else tolerant=0; fi
    run_benches "$dir" "$OUT_DIR/$side.log" "$side#$pass" "$tolerant"
    snapshot "$side" "$pass" "$marker"
    rm -f "$marker"
  done
done

# ---- 判定 ------------------------------------------------------------------
set +e
python3 "$GATE" \
  --results "$OUT_DIR/results" \
  --thresholds "$THRESHOLDS" \
  --base-ref "$BASE_SHA" \
  --head-ref "$HEAD_SHA" \
  --require-baseline \
  --report "$OUT_DIR/report.md" \
  --json "$OUT_DIR/report.json"
STATUS=$?
set -e

echo "报告: $OUT_DIR/report.md"
exit "$STATUS"
