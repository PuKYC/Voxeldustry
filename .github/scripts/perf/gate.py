#!/usr/bin/env python3
"""对比 criterion 的 A/B 结果，判定是否出现性能回归。

ab.sh 会在同一台机器上交替跑「基线代码」和「当前代码」若干轮，每跑完一轮就把
criterion 新写出的 new/estimates.json 快照成：

    <results>/base/<轮次>/<基准 id>/estimates.json
    <results>/head/<轮次>/<基准 id>/estimates.json

本脚本按同一轮次配对，算 Δ = (当前 / 基线 - 1) × 100，取各轮 Δ 里最小的那个：
每一轮都变慢才算回归。整台机器在一轮里整体变慢会被「顺序对调」抵消掉，
而真实回归在每一轮里都存在。最后按 thresholds.json 的阈值判定。

同时输出 Markdown 报告（可写进 GitHub Step Summary）与 JSON 明细（artifact）。
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path


def fmt_time(ns: float) -> str:
    """纳秒 -> 人类可读。criterion 的 estimates 统一是 ns。"""
    if ns < 1_000:
        return f"{ns:.1f} ns"
    if ns < 1_000_000:
        return f"{ns / 1_000:.2f} µs"
    if ns < 1_000_000_000:
        return f"{ns / 1_000_000:.2f} ms"
    return f"{ns / 1_000_000_000:.3f} s"


def fmt_pct(pct: float | None) -> str:
    return "—" if pct is None else f"{pct:+.1f}%"


def load_json(path: Path) -> dict:
    with path.open(encoding="utf-8") as fh:
        return json.load(fh)


def load_estimates(path: Path) -> tuple[float, float | None]:
    """返回 (mean_ns, 变异系数 std_dev/mean)。"""
    data = load_json(path)
    mean = float(data["mean"]["point_estimate"])
    std_dev = data.get("std_dev", {}).get("point_estimate")
    cv = float(std_dev) / mean if mean and std_dev is not None else None
    return mean, cv


def matches(bench_id: str, pattern: str) -> bool:
    """id 与 pattern 相等，或 pattern 是它的某级前缀（'terrain_pack' 命中 'terrain_pack/0'）。"""
    return bench_id == pattern or bench_id.startswith(pattern + "/")


def resolve_threshold(
    bench_id: str, default: float, overrides: list[dict]
) -> tuple[float, str | None]:
    """取最长匹配的覆盖阈值；没有就用默认值。"""
    best: tuple[float, str | None, int] | None = None
    for rule in overrides:
        pattern = rule["match"]
        if matches(bench_id, pattern) and (best is None or len(pattern) > best[2]):
            best = (float(rule["max_regression_pct"]), rule.get("reason"), len(pattern))
    if best is None:
        return default, None
    return best[0], best[1]


def collect_side(results: Path, side: str) -> dict[str, dict[int, tuple[float, float | None]]]:
    """<results>/<side>/<轮次>/<id>/estimates.json -> {id: {轮次: (mean, cv)}}"""
    side_dir = results / side
    runs: dict[str, dict[int, tuple[float, float | None]]] = {}
    if not side_dir.is_dir():
        return runs
    for pass_dir in sorted(p for p in side_dir.iterdir() if p.is_dir()):
        if not pass_dir.name.isdigit():
            continue
        pass_no = int(pass_dir.name)
        for est in pass_dir.rglob("estimates.json"):
            bench_id = est.parent.relative_to(pass_dir).as_posix()
            runs.setdefault(bench_id, {})[pass_no] = load_estimates(est)
    return runs


def collect(results: Path) -> list[dict]:
    """按同一轮次配对，取各轮 Δ 里最小的那个：每一轮都变慢才算回归。

    「每轮取最小值」比「两侧各取最小值再相除」更抗漂移：整台机器在一轮里整体变慢，
    会在另一轮对调顺序时被抵消；而真实回归在两轮里都会出现。
    """
    base = collect_side(results, "base")
    head = collect_side(results, "head")

    rows: list[dict] = []
    for bench_id in sorted(head):
        head_runs = head[bench_id]
        base_runs = base.get(bench_id, {})
        common = sorted(set(base_runs) & set(head_runs))

        pairs: list[tuple[float, float, float, float]] = []  # (base_mean, head_mean, base_cv, head_cv)
        if common:
            for pass_no in common:
                base_mean, base_cv = base_runs[pass_no]
                head_mean, head_cv = head_runs[pass_no]
                pairs.append((base_mean, head_mean, base_cv or 0.0, head_cv or 0.0))
        elif base_runs:
            # 轮次对不上（极少见）：退回「各侧取最小值」。
            pairs.append(
                (
                    min(m for m, _ in base_runs.values()),
                    min(m for m, _ in head_runs.values()),
                    0.0,
                    0.0,
                )
            )

        # 理论上不会出现 0ns，但别让除零把整个 job 搞挂。
        pairs = [p for p in pairs if p[0] > 0]

        if not pairs:
            rows.append(
                {
                    "id": bench_id,
                    "baseline_ns": None,
                    "current_ns": min(m for m, _ in head_runs.values()),
                    "change_pct": None,
                    "noise_pct": max((c or 0.0) for _, c in head_runs.values()) * 100.0,
                    "baseline_runs": 0,
                    "current_runs": len(head_runs),
                }
            )
            continue

        base_mean, head_mean, base_cv, head_cv = min(
            pairs, key=lambda p: p[1] / p[0]
        )
        rows.append(
            {
                "id": bench_id,
                "baseline_ns": base_mean,
                "current_ns": head_mean,
                "change_pct": (head_mean / base_mean - 1.0) * 100.0,
                "noise_pct": max(base_cv, head_cv) * 100.0,
                "baseline_runs": len(base_runs),
                "current_runs": len(head_runs),
            }
        )
    return rows


def classify(rows: list[dict], cfg: dict) -> list[dict]:
    default = float(cfg.get("default_max_regression_pct", 10.0))
    overrides = cfg.get("overrides", [])
    ignored_patterns = [item["match"] for item in cfg.get("ignore", [])]

    for row in rows:
        row["ignored"] = any(matches(row["id"], p) for p in ignored_patterns)
        threshold, reason = resolve_threshold(row["id"], default, overrides)
        row["threshold_pct"] = threshold
        row["threshold_reason"] = reason
        pct = row["change_pct"]
        if row["ignored"]:
            row["verdict"] = "skipped"
        elif pct is None:
            row["verdict"] = "no-baseline"
        elif pct > threshold:
            row["verdict"] = "regression"
        elif pct < -3.0:
            row["verdict"] = "improved"
        else:
            row["verdict"] = "ok"
    return rows


def render_markdown(rows: list[dict], meta: dict) -> str:
    regressions = [r for r in rows if r["verdict"] == "regression"]
    improved = [r for r in rows if r["verdict"] == "improved"]
    noisy = [r for r in rows if r["verdict"] == "ok" and (r["noise_pct"] or 0) > 10.0]
    compared = [r for r in rows if r["verdict"] not in ("skipped", "no-baseline")]
    rounds = max([r["current_runs"] for r in rows] + [r["baseline_runs"] for r in rows] + [0])

    lines: list[str] = []
    lines.append("## 性能基准 A/B 对比")
    lines.append("")
    if meta.get("base_ref"):
        lines.append(
            f"基线 `{meta['base_ref'][:10]}` → 当前 `{(meta.get('head_ref') or '')[:10]}`，"
            f"同机交替跑 {rounds} 轮，按轮次配对取最小 Δ（每轮都变慢才算回归）。"
        )
    else:
        lines.append("未找到基线（本次只记录耗时，不做门禁）。")
    lines.append("")

    if regressions:
        lines.append(
            f"**结果：{len(regressions)} 项超过阈值，判定回归。** "
            "阈值见 [`.github/scripts/perf/thresholds.json`]"
            "(../blob/main/.github/scripts/perf/thresholds.json)。"
        )
    else:
        lines.append("**结果：未发现超过阈值的回归。**")
    lines.append("")
    lines.append(
        f"对比 {len(compared)} 项 / 采集 {len(rows)} 项；"
        f"变快 {len(improved)} 项，回归 {len(regressions)} 项，"
        f"波动偏大 {len(noisy)} 项。"
    )
    lines.append("")

    if rows:
        lines.append("| 基准 | 基线 | 当前 | Δ | 波动 | 阈值 | 判定 |")
        lines.append("| --- | ---: | ---: | ---: | ---: | ---: | --- |")
        ordered = sorted(
            rows,
            key=lambda r: (
                0 if r["verdict"] == "regression" else 1,
                -(r["change_pct"] if r["change_pct"] is not None else 0.0),
                r["id"],
            ),
        )
        icons = {
            "regression": "🔴 回归",
            "improved": "🟢 变快",
            "ok": "✅ 持平",
            "skipped": "⏭️ 已忽略",
            "no-baseline": "⚪ 无基线",
        }
        for row in ordered:
            base = fmt_time(row["baseline_ns"]) if row["baseline_ns"] is not None else "—"
            noise = "—" if row["noise_pct"] is None else f"{row['noise_pct']:.0f}%"
            if row["verdict"] == "ok" and (row["noise_pct"] or 0) > 10.0:
                noise = f"⚠️ {noise}"
            lines.append(
                f"| `{row['id']}` | {base} | {fmt_time(row['current_ns'])} | "
                f"{fmt_pct(row['change_pct'])} | {noise} | {row['threshold_pct']:.0f}% | "
                f"{icons[row['verdict']]} |"
            )
        lines.append("")

    lines.append(
        "<sub>Δ 为 mean 的相对变化（正数 = 变慢）。波动 = 各轮测量里最大的变异系数 "
        "(std_dev / mean)，⚠️ 表示该次测量不够稳，Δ 仅供参考。</sub>"
    )
    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description="对比 criterion A/B 结果并判定回归")
    parser.add_argument(
        "--results",
        default="target/perf-out/results",
        help="ab.sh 生成的快照目录（base/ 与 head/ 各含若干轮次）",
    )
    parser.add_argument(
        "--thresholds",
        default=".github/scripts/perf/thresholds.json",
        help="阈值配置",
    )
    parser.add_argument("--base-ref", default="", help="报告里展示的基线 commit")
    parser.add_argument("--head-ref", default="", help="报告里展示的当前 commit")
    parser.add_argument("--report", default="", help="Markdown 报告输出路径")
    parser.add_argument("--json", default="", help="JSON 明细输出路径")
    parser.add_argument(
        "--require-baseline",
        action="store_true",
        help="找不到任何可比记录时视为错误（CI 用，防止静默放过）",
    )
    args = parser.parse_args()

    results = Path(args.results)
    if not results.is_dir():
        print(f"错误：找不到结果目录 {results}", file=sys.stderr)
        return 2

    cfg = load_json(Path(args.thresholds)) if Path(args.thresholds).is_file() else {}
    rows = collect(results)

    if args.require_baseline and not any(r["change_pct"] is not None for r in rows):
        print(
            f"错误：{results} 里没有成套的 base/ 与 head/ 结果，A/B 对比没有发生。",
            file=sys.stderr,
        )
        return 2

    classify(rows, cfg)

    meta = {"base_ref": args.base_ref, "head_ref": args.head_ref}
    regressions = [r for r in rows if r["verdict"] == "regression"]

    for row in regressions:
        print(
            f"[回归] {row['id']}: {fmt_pct(row['change_pct'])} "
            f"(阈值 {row['threshold_pct']:.0f}%, "
            f"{fmt_time(row['baseline_ns'])} -> {fmt_time(row['current_ns'])})"
        )
    print(
        f"共 {len(rows)} 项，回归 {len(regressions)} 项，"
        f"默认阈值 {cfg.get('default_max_regression_pct', 10.0)}%。"
    )

    markdown = render_markdown(rows, meta)
    print()
    print(markdown)

    if args.report:
        Path(args.report).parent.mkdir(parents=True, exist_ok=True)
        Path(args.report).write_text(markdown, encoding="utf-8")
    if args.json:
        Path(args.json).parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "meta": meta,
            "thresholds": cfg,
            "regressions": [r["id"] for r in regressions],
            "rows": rows,
        }
        Path(args.json).write_text(
            json.dumps(payload, indent=2, ensure_ascii=False), encoding="utf-8"
        )
    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(summary, "a", encoding="utf-8") as fh:
            fh.write(markdown)

    return 1 if regressions else 0


if __name__ == "__main__":
    sys.exit(main())
