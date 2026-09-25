"""Plain-language comparison of benchmark conditions (stdlib only, 3.11).

    python compare.py --runs <runs dir> [--baseline vanilla] [--out report.md]

Reads every <runs>/*/result.json and prints one headline table (one row per
metric, one column per condition, change vs the baseline in brackets), a
verdict line per metric, a small-vs-large task split, and a per-task table.
Changes are marked with a paired-bootstrap 95% CI over tasks: "(sig)" when
the interval excludes zero.
"""
import argparse
import glob
import json
import random
import statistics
from collections import defaultdict
from pathlib import Path

XL_TASKS = {"t16_tags", "t17_schema_migration", "t18_ledger_layer", "t19_goals_saga",
            "t20_audit_log", "t21_search", "t22_envelopes"}
COND_LABELS = {
    "vanilla": "Vanilla + superpowers",
    "zirv": "zirv",
    "zirv-nojev": "zirv, Jev off",
    "zirv-proxy": "zirv + proxy",
    "zirv-jev-full": "zirv, Jev on",
}
# (key, label, higher_is_better, formatter, value(run))
# "quality" (Work quality, judge=opus, quality_rubric.md) is None for any
# run without a quality_score -- every non-tests-kind task (judge/answer),
# and any tests-kind run predating this metric or whose judge call failed.
# per_task_means below drops None values rather than averaging them in as 0,
# so those tasks/runs are excluded from this metric's mean, not counted
# against it.
METRICS = [
    ("score", "Quality score (0-100)", True, "{:.0f}", lambda r: 100 * (r.get("score") or 0)),
    ("solved", "Fully solved (%)", True, "{:.0f}%", lambda r: 100.0 if (r.get("score") or 0) >= 0.999 else 0.0),
    ("wall", "Time per task (min)", False, "{:.1f}", lambda r: (r.get("wall_s") or 0) / 60),
    ("cost", "Cost per task (USD)", False, "${:.2f}", lambda r: r.get("total_cost_usd") or 0),
    ("turns", "Agent turns", False, "{:.0f}", lambda r: r.get("num_turns") or 0),
    ("out", "Output tokens (k)", False, "{:.1f}", lambda r: (r.get("output_tokens") or 0) / 1000),
    ("quality", "Work quality (judge, 0-100)", True, "{:.0f}",
     lambda r: 100 * r["quality_score"] if r.get("quality_score") is not None else None),
]


def load(runs_dir):
    rows = []
    for p in glob.glob(str(Path(runs_dir) / "*" / "result.json")):
        try:
            rows.append(json.loads(Path(p).read_text(encoding="utf-8")))
        except Exception:
            pass
    return rows


def per_task_means(rows, fn):
    """{(cond, task): mean} -- tasks weigh equally whatever their rep count.

    A row where `fn` returns None (a metric that doesn't apply to that run,
    e.g. quality_score on a judge/answer-kind task) is skipped entirely
    rather than averaged in as 0.
    """
    acc = defaultdict(list)
    for r in rows:
        v = fn(r)
        if v is not None:
            acc[(r["cond"], r["task"])].append(v)
    return {k: statistics.fmean(v) for k, v in acc.items()}


def cond_mean(means, cond, tasks):
    vals = [means[(cond, t)] for t in tasks if (cond, t) in means]
    return statistics.fmean(vals) if vals else None


def bootstrap_diff(means, cond, base, tasks, n=4000, seed=7):
    paired = [(means[(cond, t)], means[(base, t)]) for t in tasks
              if (cond, t) in means and (base, t) in means]
    if len(paired) < 3:
        return None
    rng = random.Random(seed)
    diffs = sorted(
        statistics.fmean(a - b for a, b in (rng.choice(paired) for _ in paired))
        for _ in range(n))
    return diffs[int(0.025 * n)], diffs[int(0.975 * n)]


def change_cell(v, b, ci, higher_better, pct):
    if v is None or b is None:
        return ""
    if pct:
        if not b:
            return ""
        ch = f"{100 * (v - b) / b:+.0f}%"
    else:
        ch = f"{v - b:+.0f} pts"
    sig = ci is not None and (ci[0] > 0 or ci[1] < 0)
    better = (v > b) == higher_better
    mark = "n/a" if ci is None else ("better" if better else "worse") if sig else "n.s."
    return f"{ch} ({mark})"


def headline(rows, conds, base, tasks):
    out = ["| Metric | " + " | ".join(COND_LABELS.get(c, c) for c in conds) + " |",
           "|---|" + "---:|" * len(conds)]
    verdicts = []
    for key, label, hib, fmt, fn in METRICS:
        means = per_task_means(rows, fn)
        b = cond_mean(means, base, tasks)
        cells = []
        for c in conds:
            v = cond_mean(means, c, tasks)
            cell = fmt.format(v) if v is not None else "-"
            if c != base:
                ch = change_cell(v, b, bootstrap_diff(means, c, base, tasks), hib,
                                 pct=key not in ("score", "solved"))
                cell += f"<br>{ch}" if ch else ""
            cells.append(cell)
        out.append(f"| {label} | " + " | ".join(cells) + " |")
        best = [c for c in conds if cond_mean(means, c, tasks) is not None]
        if best:
            top = (max if hib else min)(cond_mean(means, c, tasks) for c in best)
            picks = [COND_LABELS.get(c, c) for c in best if abs(cond_mean(means, c, tasks) - top) < 1e-9]
            verdicts.append(f"- **{label}**: " + ("best is " + picks[0] if len(picks) == 1 else "tie: " + "; ".join(picks)))
    return out, verdicts


def reliability(rows, conds):
    out = ["| | " + " | ".join(COND_LABELS.get(c, c) for c in conds) + " |",
           "|---|" + "---:|" * len(conds)]
    by = defaultdict(list)
    for r in rows:
        by[r["cond"]].append(r)
    def cell(c, pred):
        rs = by.get(c, [])
        return f"{sum(1 for r in rs if pred(r))}/{len(rs)}"
    out.append("| Runs | " + " | ".join(str(len(by.get(c, []))) for c in conds) + " |")
    out.append("| Errors / timeouts | " + " | ".join(cell(c, lambda r: r.get("is_error")) for c in conds) + " |")
    out.append("| Visible tests still green | " + " | ".join(cell(c, lambda r: r.get("visible_ok")) for c in conds) + " |")
    models = {c: defaultdict(int) for c in conds}
    for r in rows:
        if r["cond"] in models:
            models[r["cond"]][r.get("model_used") or r.get("model")] += 1
    out.append("| Model actually used | " + " | ".join(
        ", ".join(f"{m} x{n}" for m, n in sorted(models[c].items())) for c in conds) + " |")
    return out


def split_table(rows, conds, base, tasks):
    out = ["| Task group | Metric | " + " | ".join(COND_LABELS.get(c, c) for c in conds) + " |",
           "|---|---|" + "---:|" * len(conds)]
    chain = {r["task"] for r in rows if r.get("steps")}
    groups = [("Small/large (t01-t15)", [t for t in tasks if t not in XL_TASKS and t not in chain]),
              ("XL (t16-t22)", [t for t in tasks if t in XL_TASKS]),
              ("Long-session chain", [t for t in tasks if t in chain])]
    for gname, gtasks in groups:
        if not gtasks:
            continue
        for key, label, hib, fmt, fn in METRICS[:4]:
            means = per_task_means(rows, fn)
            b = cond_mean(means, base, gtasks)
            cells = []
            for c in conds:
                v = cond_mean(means, c, gtasks)
                cell = fmt.format(v) if v is not None else "-"
                if c != base and v is not None and b:
                    cell += (f" ({v - b:+.0f} pts)" if key in ("score", "solved")
                             else f" ({100 * (v - b) / b:+.0f}%)")
                cells.append(cell)
            out.append(f"| {gname} | {label} | " + " | ".join(cells) + " |")
    return out


def per_task(rows, conds, tasks):
    score = per_task_means(rows, METRICS[0][4])
    cost = per_task_means(rows, METRICS[3][4])
    wall = per_task_means(rows, METRICS[2][4])
    out = ["| Task | " + " | ".join(f"{COND_LABELS.get(c, c)}<br>score / $ / min" for c in conds) + " |",
           "|---|" + "---:|" * len(conds)]
    for t in tasks:
        cells = []
        for c in conds:
            if (c, t) not in score:
                cells.append("-")
                continue
            cells.append(f"{score[(c, t)]:.0f} / ${cost[(c, t)]:.2f} / {wall[(c, t)]:.1f}")
        out.append(f"| {t} | " + " | ".join(cells) + " |")
    return out


def chain_step_table(rows, conds):
    """Per-step table for `kind=chain` runs (spec item 4): step -> score/cost/
    min per condition, averaged over reps. `rows` without a non-empty
    `"steps"` list (every non-chain run) are ignored. Chain tasks are listed
    in their own sub-heading, since two chain tasks in one `--runs` dir
    would otherwise blend their step numbering together.
    """
    chain_rows = [r for r in rows if r.get("steps")]
    if not chain_rows:
        return []
    tasks = sorted({r["task"] for r in chain_rows})
    out = []
    for task in tasks:
        task_rows = [r for r in chain_rows if r["task"] == task]
        # {(cond, label): [(score, cost, wall), ...]}
        acc = defaultdict(list)
        kinds = {}
        for r in task_rows:
            for step in r["steps"]:
                label = step.get("label")
                if label is None:
                    continue
                kinds[label] = step.get("kind")
                score = step.get("score")
                cost = step.get("cost_usd") or 0.0
                wall = (step.get("wall_s") or 0.0) / 60
                acc[(r["cond"], label)].append((score, cost, wall))
        labels = sorted(kinds)
        out.append(f"### {task}\n")
        out.append("| Step | " + " | ".join(f"{COND_LABELS.get(c, c)}<br>score / $ / min" for c in conds) + " |")
        out.append("|---|" + "---:|" * len(conds))
        for label in labels:
            cells = []
            for c in conds:
                pts = acc.get((c, label), [])
                if not pts:
                    cells.append("-")
                    continue
                scores = [s for s, _, _ in pts if s is not None]
                score_str = f"{100 * statistics.fmean(scores):.0f}" if scores else "-"
                cost_mean = statistics.fmean(cost for _, cost, _ in pts)
                wall_mean = statistics.fmean(wall for _, _, wall in pts)
                cells.append(f"{score_str} / ${cost_mean:.2f} / {wall_mean:.1f}")
            out.append(f"| {label} ({kinds[label]}) | " + " | ".join(cells) + " |")
        out.append("")
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", required=True)
    ap.add_argument("--baseline", default="vanilla")
    ap.add_argument("--out", default=None)
    args = ap.parse_args()

    rows = load(args.runs)
    order = list(COND_LABELS)
    conds = sorted({r["cond"] for r in rows}, key=lambda c: order.index(c) if c in order else 99)
    tasks = sorted({r["task"] for r in rows})
    if args.baseline not in conds:
        raise SystemExit(f"baseline {args.baseline!r} not in runs ({conds})")

    head, verdicts = headline(rows, conds, args.baseline, tasks)
    parts = [
        "## Headline (mean per task, every task weighted equally)\n",
        f"Change vs **{COND_LABELS.get(args.baseline, args.baseline)}** below each value; "
        "*better*/*worse* = paired-bootstrap 95% CI over tasks excludes zero, *n.s.* = within noise.\n",
        *head, "", *verdicts, "",
        "## Reliability\n", *reliability(rows, conds), "",
        "## By task group\n", *split_table(rows, conds, args.baseline, tasks), "",
        "## Per task\n", *per_task(rows, conds, tasks), "",
    ]
    chain_table = chain_step_table(rows, conds)
    if chain_table:
        parts += ["## Long-session chain (per step)\n", *chain_table]
    text = "\n".join(parts)
    if args.out:
        Path(args.out).write_text(text, encoding="utf-8")
    print(text)


if __name__ == "__main__":
    main()
