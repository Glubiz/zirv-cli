#!/usr/bin/env python3
"""Aggregator for the zirv-vs-vanilla benchmark.

Reads every runs/<task>__<cond>__r<n>/result.json and writes:
  - results.csv  (one flattened row per run)
  - report.md    (headline table, per-task table, features-used table,
                   zirv-proxy decision table, robustness/bootstrap note)

stdlib only (3.11), no matplotlib.
"""
import argparse
import collections
import csv
import json
import random
import statistics
from pathlib import Path

CANONICAL_CONDS = ["vanilla", "zirv", "zirv-proxy"]
NON_VANILLA_CONDS = ["zirv", "zirv-proxy"]
N_RESAMPLES = 10_000
BOOTSTRAP_SEED = 0

CSV_FIELDS = [
    "task", "cond", "rep", "model", "model_used",
    "wall_s", "duration_ms", "duration_api_ms", "num_turns",
    "total_cost_usd", "agent_cost_usd",
    "input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens", "output_tokens",
    "subagents_spawned", "permission_denials", "is_error", "exit_code",
    "zirv_cmds_workflow", "zirv_cmds_skill", "zirv_cmds_agent", "zirv_cmds_ctx", "zirv_cmds_other",
    "tool_calls",
    "score", "passed", "total", "visible_ok", "details",
    "judge_score", "judge_reasoning",
    "proxy_complexity", "proxy_risk", "proxy_execution", "proxy_seat_role", "proxy_seat_tier",
    "proxy_worker_tier", "proxy_workflow", "proxy_domains", "proxy_decider", "proxy_elapsed_ms",
    "proxy_input_tokens", "proxy_output_tokens", "proxy_cost_usd", "proxy_wall_s",
    "workflow_started", "workflow_note",
]


def load_results(runs_dir):
    rows = []
    for rj in sorted(Path(runs_dir).glob("*/result.json")):
        try:
            obj = json.loads(rj.read_text(encoding="utf-8"))
        except Exception as e:
            print(f"WARN: could not parse {rj}: {e}")
            continue
        obj["_path"] = str(rj)
        rows.append(obj)
    return rows


def flatten(obj):
    zc = obj.get("zirv_cmds") or {}
    px = obj.get("proxy") or {}
    row = {
        "task": obj.get("task"), "cond": obj.get("cond"), "rep": obj.get("rep"),
        "model": obj.get("model"), "model_used": obj.get("model_used"),
        "wall_s": obj.get("wall_s"), "duration_ms": obj.get("duration_ms"),
        "duration_api_ms": obj.get("duration_api_ms"), "num_turns": obj.get("num_turns"),
        "total_cost_usd": obj.get("total_cost_usd"), "agent_cost_usd": obj.get("agent_cost_usd"),
        "input_tokens": obj.get("input_tokens", 0),
        "cache_creation_input_tokens": obj.get("cache_creation_input_tokens", 0),
        "cache_read_input_tokens": obj.get("cache_read_input_tokens", 0),
        "output_tokens": obj.get("output_tokens", 0),
        "subagents_spawned": obj.get("subagents_spawned", 0),
        "permission_denials": obj.get("permission_denials", 0),
        "is_error": obj.get("is_error", False), "exit_code": obj.get("exit_code"),
        "zirv_cmds_workflow": zc.get("workflow", 0), "zirv_cmds_skill": zc.get("skill", 0),
        "zirv_cmds_agent": zc.get("agent", 0), "zirv_cmds_ctx": zc.get("ctx", 0),
        "zirv_cmds_other": zc.get("other", 0),
        "tool_calls": obj.get("tool_calls", 0),
        "score": obj.get("score", 0.0), "passed": obj.get("passed", 0), "total": obj.get("total", 0),
        "visible_ok": obj.get("visible_ok", False), "details": obj.get("details", ""),
        "judge_score": obj.get("judge_score"), "judge_reasoning": obj.get("judge_reasoning"),
        "proxy_complexity": px.get("complexity"), "proxy_risk": px.get("risk"),
        "proxy_execution": px.get("execution"), "proxy_seat_role": px.get("seat_role"),
        "proxy_seat_tier": px.get("seat_tier"), "proxy_worker_tier": px.get("worker_tier"),
        "proxy_workflow": px.get("workflow"),
        "proxy_domains": ";".join(px.get("domains") or []),
        "proxy_decider": px.get("decider"), "proxy_elapsed_ms": px.get("elapsed_ms"),
        "proxy_input_tokens": px.get("input_tokens", 0), "proxy_output_tokens": px.get("output_tokens", 0),
        "proxy_cost_usd": px.get("cost_usd", 0.0), "proxy_wall_s": px.get("wall_s", 0.0),
        "workflow_started": obj.get("workflow_started", False),
        "workflow_note": obj.get("workflow_note"),
    }
    return row


def write_csv(rows, out_path):
    with open(out_path, "w", newline="", encoding="utf-8") as f:
        w = csv.DictWriter(f, fieldnames=CSV_FIELDS)
        w.writeheader()
        for r in rows:
            w.writerow(flatten(r))


def mean_or_none(xs):
    xs = [x for x in xs if x is not None]
    return statistics.mean(xs) if xs else None


def median_or_none(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


def fmt(x, spec="{:.3f}"):
    if x is None:
        return "n/a"
    try:
        return spec.format(x)
    except Exception:
        return str(x)


def pct_change(cond_val, base_val):
    if cond_val is None or base_val is None:
        return None
    if base_val == 0:
        return None
    return (cond_val - base_val) / abs(base_val) * 100.0


def total_tokens(row):
    return (row.get("input_tokens") or 0) + (row.get("cache_creation_input_tokens") or 0) + \
        (row.get("cache_read_input_tokens") or 0) + (row.get("output_tokens") or 0)


def by_cond(rows, exclude_errors=True):
    out = collections.defaultdict(list)
    for r in rows:
        if exclude_errors and r.get("is_error"):
            continue
        out[r.get("cond")].append(r)
    return out


def metric_series(rows_for_cond, metric_fn):
    return [metric_fn(r) for r in rows_for_cond]


def headline_table(rows, conds_present):
    groups = by_cond(rows, exclude_errors=True)
    metrics = [
        ("Speed: mean wall_s (s)", lambda r: r.get("wall_s"), False, "{:.1f}"),
        ("Speed: median wall_s (s)", None, False, "{:.1f}"),  # handled specially (median)
        ("Cost: mean total_cost_usd ($)", lambda r: r.get("total_cost_usd"), False, "{:.3f}"),
        ("Cost: mean total tokens", total_tokens, False, "{:.0f}"),
        ("Cost: mean output_tokens", lambda r: r.get("output_tokens"), False, "{:.0f}"),
        ("Intelligence: mean score", lambda r: r.get("score"), True, "{:.3f}"),
        ("Intelligence: solve rate (score==1.0)", lambda r: 1.0 if (r.get("score") == 1.0) else 0.0, True, "{:.3f}"),
        ("Intelligence: mean visible_ok", lambda r: 1.0 if r.get("visible_ok") else 0.0, True, "{:.3f}"),
    ]

    change_conds = [c for c in NON_VANILLA_CONDS if c in conds_present]
    have_vanilla = "vanilla" in conds_present

    header_cells = ["Metric"] + list(conds_present)
    if have_vanilla:
        for c in change_conds:
            header_cells.append(f"change: {c} vs vanilla (%)")
    header_cells.append("higher is better?")

    lines = []
    lines.append("| " + " | ".join(header_cells) + " |")
    lines.append("|" + "|".join(["---"] * len(header_cells)) + "|")

    for name, fn, higher_better, spec in metrics:
        values = {}
        for c in conds_present:
            rs = groups.get(c, [])
            if name.startswith("Speed: median"):
                vals = [r.get("wall_s") for r in rs]
                values[c] = median_or_none(vals)
            else:
                vals = [fn(r) for r in rs]
                values[c] = mean_or_none(vals)
        row = [name] + [fmt(values[c], spec) for c in conds_present]
        if have_vanilla:
            base = values.get("vanilla")
            for c in change_conds:
                ch = pct_change(values.get(c), base)
                sign = "+" if (ch is not None and ch >= 0) else ""
                row.append(f"{sign}{ch:.1f}%" if ch is not None else "n/a")
        row.append("higher" if higher_better else "lower")
        lines.append("| " + " | ".join(str(x) for x in row) + " |")

    return "\n".join(lines)


def per_task_table(rows, conds_present):
    groups = collections.defaultdict(lambda: collections.defaultdict(list))
    for r in rows:
        if r.get("is_error"):
            continue
        groups[r.get("task")][r.get("cond")].append(r)

    header_cells = ["Task"]
    for c in conds_present:
        header_cells += [f"{c} mean score", f"{c} mean cost", f"{c} mean wall_s", f"{c} n"]
    lines = ["| " + " | ".join(header_cells) + " |",
             "|" + "|".join(["---"] * len(header_cells)) + "|"]

    for task in sorted(groups.keys()):
        row = [task]
        for c in conds_present:
            rs = groups[task].get(c, [])
            n = len(rs)
            mscore = mean_or_none([r.get("score") for r in rs])
            mcost = mean_or_none([r.get("total_cost_usd") for r in rs])
            mwall = mean_or_none([r.get("wall_s") for r in rs])
            row += [fmt(mscore, "{:.3f}"), fmt(mcost, "{:.3f}"), fmt(mwall, "{:.1f}"), str(n)]
        lines.append("| " + " | ".join(row) + " |")
    return "\n".join(lines)


def features_used_table(rows):
    groups = by_cond(rows, exclude_errors=True)
    non_vanilla_present = [c for c in NON_VANILLA_CONDS if c in groups]
    vanilla_tool_calls = mean_or_none([r.get("tool_calls") for r in groups.get("vanilla", [])])

    header = ["Condition", "mean tool_calls", "mean zirv workflow", "mean zirv skill",
              "mean zirv agent", "mean zirv ctx", "mean zirv other",
              "mean subagents_spawned", "mean permission_denials"]
    lines = ["| " + " | ".join(header) + " |", "|" + "|".join(["---"] * len(header)) + "|"]
    for c in non_vanilla_present:
        rs = groups[c]
        row = [
            c,
            fmt(mean_or_none([r.get("tool_calls") for r in rs]), "{:.1f}"),
            fmt(mean_or_none([r.get("zirv_cmds_workflow") for r in rs]), "{:.2f}"),
            fmt(mean_or_none([r.get("zirv_cmds_skill") for r in rs]), "{:.2f}"),
            fmt(mean_or_none([r.get("zirv_cmds_agent") for r in rs]), "{:.2f}"),
            fmt(mean_or_none([r.get("zirv_cmds_ctx") for r in rs]), "{:.2f}"),
            fmt(mean_or_none([r.get("zirv_cmds_other") for r in rs]), "{:.2f}"),
            fmt(mean_or_none([r.get("subagents_spawned") for r in rs]), "{:.2f}"),
            fmt(mean_or_none([r.get("permission_denials") for r in rs]), "{:.2f}"),
        ]
        lines.append("| " + " | ".join(row) + " |")
    lines.append("")
    lines.append(f"vanilla mean tool_calls (for comparison): {fmt(vanilla_tool_calls, '{:.1f}')}")
    return "\n".join(lines)


def zirv_proxy_decision_table(rows):
    rs = [r for r in rows if r.get("cond") == "zirv-proxy"]
    if not rs:
        return None
    by_task = collections.defaultdict(list)
    for r in rs:
        by_task[r.get("task")].append(r)

    def mode(values):
        values = [v for v in values if v is not None]
        if not values:
            return "n/a"
        c = collections.Counter(values)
        return c.most_common(1)[0][0]

    header = ["Task", "mode complexity", "mode seat_tier", "mode model_used", "mode workflow",
              "runs that started a workflow", "n runs"]
    lines = ["| " + " | ".join(header) + " |", "|" + "|".join(["---"] * len(header)) + "|"]
    for task in sorted(by_task.keys()):
        trs = by_task[task]
        n_started = sum(1 for r in trs if r.get("workflow_started"))
        # mode fields come from raw obj values stashed on each row (see main())
        complexities = [r.get("_proxy_complexity") for r in trs]
        seat_tiers = [r.get("_proxy_seat_tier") for r in trs]
        models_used = [r.get("model_used") for r in trs]
        workflows = [r.get("_proxy_workflow") for r in trs]
        row = [
            task,
            str(mode(complexities)),
            str(mode(seat_tiers)),
            str(mode(models_used)),
            str(mode(workflows)),
            f"{n_started}/{len(trs)}",
            str(len(trs)),
        ]
        lines.append("| " + " | ".join(row) + " |")
    return "\n".join(lines)


def paired_bootstrap(rows, cond, metric_fn):
    """Paired-by-(task,rep) bootstrap 95% CI of mean(cond) - mean(vanilla)."""
    vanilla_vals = {}
    cond_vals = {}
    for r in rows:
        if r.get("is_error"):
            continue
        key = (r.get("task"), r.get("rep"))
        if r.get("cond") == "vanilla":
            vanilla_vals[key] = metric_fn(r)
        elif r.get("cond") == cond:
            cond_vals[key] = metric_fn(r)
    keys = sorted(set(vanilla_vals) & set(cond_vals))
    diffs = []
    for key in keys:
        a, b = cond_vals[key], vanilla_vals[key]
        if a is None or b is None:
            continue
        diffs.append(a - b)
    n = len(diffs)
    if n == 0:
        return None
    rng = random.Random(BOOTSTRAP_SEED)
    means = []
    for _ in range(N_RESAMPLES):
        s = 0.0
        for _ in range(n):
            s += diffs[rng.randrange(n)]
        means.append(s / n)
    means.sort()
    lo = means[max(0, int(0.025 * N_RESAMPLES) - 1)]
    hi = means[min(N_RESAMPLES - 1, int(0.975 * N_RESAMPLES))]
    point = sum(diffs) / n
    return point, lo, hi, n


def robustness_note(rows, conds_present):
    lines = []
    metrics = [("score", lambda r: r.get("score")),
               ("total_cost_usd", lambda r: r.get("total_cost_usd")),
               ("wall_s", lambda r: r.get("wall_s"))]
    if "vanilla" in conds_present:
        for cond in [c for c in NON_VANILLA_CONDS if c in conds_present]:
            lines.append(f"\nPaired bootstrap 95% CI, mean({cond}) - mean(vanilla), "
                         f"{N_RESAMPLES} resamples seed {BOOTSTRAP_SEED}, paired by (task,rep):")
            for name, fn in metrics:
                res = paired_bootstrap(rows, cond, fn)
                if res is None:
                    lines.append(f"  - {name}: no matched (task,rep) pairs")
                    continue
                point, lo, hi, n = res
                lines.append(f"  - {name}: diff={point:+.4f}, 95% CI [{lo:+.4f}, {hi:+.4f}] (n={n} pairs)")
    else:
        lines.append("(no vanilla condition present; skipping paired bootstrap)")

    lines.append("\nErrored/timed-out runs per condition:")
    err_counts = collections.Counter()
    total_counts = collections.Counter()
    for r in rows:
        total_counts[r.get("cond")] += 1
        if r.get("is_error"):
            err_counts[r.get("cond")] += 1
    for c in conds_present:
        lines.append(f"  - {c}: {err_counts.get(c, 0)} / {total_counts.get(c, 0)}")
    return "\n".join(lines)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", default=None, help="defaults to <script dir>/runs")
    ap.add_argument("--out", default=None, help="defaults to <script dir>/report.md")
    args = ap.parse_args()

    script_dir = Path(__file__).resolve().parent
    runs_dir = Path(args.runs) if args.runs else script_dir / "runs"
    out_path = Path(args.out) if args.out else script_dir / "report.md"
    csv_path = out_path.parent / "results.csv"

    raw_rows = load_results(runs_dir)
    if not raw_rows:
        print(f"No result.json files found under {runs_dir}")
        out_path.write_text("# Benchmark report\n\nNo results found.\n", encoding="utf-8")
        return

    write_csv(raw_rows, csv_path)

    # Flattened rows used for all table computations, plus a few raw fields
    # stashed under _proxy_* for the zirv-proxy decision table (mode needs
    # the raw, possibly-None values, not the flattened display strings).
    rows = []
    for obj in raw_rows:
        r = flatten(obj)
        px = obj.get("proxy") or {}
        r["_proxy_complexity"] = px.get("complexity")
        r["_proxy_seat_tier"] = px.get("seat_tier")
        r["_proxy_workflow"] = px.get("workflow")
        rows.append(r)

    conds_present = [c for c in CANONICAL_CONDS if any(r.get("cond") == c for r in rows)]

    n_total = len(rows)
    n_err = sum(1 for r in rows if r.get("is_error"))

    parts = []
    parts.append("# zirv-vs-vanilla benchmark report\n")
    parts.append(f"Runs found: {n_total} (errored/timed out: {n_err}). Conditions: {', '.join(conds_present)}.\n")

    parts.append("## Headline\n")
    parts.append(headline_table(rows, conds_present))
    parts.append("")

    parts.append("## Per-task\n")
    parts.append(per_task_table(rows, conds_present))
    parts.append("")

    parts.append("## Features used (zirv / zirv-proxy runs)\n")
    parts.append(features_used_table(rows))
    parts.append("")

    proxy_table = zirv_proxy_decision_table(rows)
    if proxy_table:
        parts.append("## zirv-proxy decisions per task\n")
        parts.append(proxy_table)
        parts.append("")

    parts.append("## Robustness\n")
    parts.append(robustness_note(rows, conds_present))
    parts.append("")

    report_text = "\n".join(parts)
    out_path.write_text(report_text, encoding="utf-8")
    print(f"Wrote {csv_path}")
    print(f"Wrote {out_path}")


if __name__ == "__main__":
    main()
