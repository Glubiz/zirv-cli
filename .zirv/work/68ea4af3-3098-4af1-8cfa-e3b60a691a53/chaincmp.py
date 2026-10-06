"""Compare chain arms on totals. python chaincmp.py <bench subdir>:<cond> ... (first = reference)"""
import glob, json, os, statistics as st, sys

B = os.path.join(os.path.dirname(os.path.abspath(__file__)), "bench")
KEYS = ["total_cost_usd", "wall_s", "duration_ms", "duration_api_ms", "output_tokens", "cache_read_input_tokens",
        "cache_creation_input_tokens", "num_turns", "score", "quality_score", "compactions", "stderr_compactions"]
arms = {}
for spec in sys.argv[1:]:
    subs, cond = spec.rsplit(":", 1)
    rows = []
    for sub in subs.split("+"):
        for p in sorted(glob.glob(os.path.join(B, sub, f"*__{cond}__r*", "result.json"))):
            r = json.load(open(p, encoding="utf-8"))
            if r.get("is_error"):
                print("ERROR run", p)
            n = 0
            for f in glob.glob(os.path.join(os.path.dirname(p), "stderr_step_*.txt")):
                n += open(f, encoding="utf-8", errors="ignore").read().count("compaction injected")
            r["stderr_compactions"] = n
            r.setdefault("compactions", 0)
            rows.append(r)
    arms[spec] = rows
ref = sys.argv[1]
print(f"{'arm':34} {'n':>2} " + " ".join(f"{k[:13]:>14}" for k in KEYS))
for a, rows in arms.items():
    print(f"{a:34} {len(rows):2} " + " ".join(f"{st.fmean([(r.get(k) or 0) for r in rows]):14.4g}" for k in KEYS))
    print(f"{'  per run':34}    " + " | ".join(
        f"${r['total_cost_usd']:.2f} {r['wall_s']:.0f}s api{(r['duration_api_ms'] or 0)/1000:.0f}s sc{r['score']:.3f} q{r.get('quality_score')} c{r['compactions']}/{r['stderr_compactions']}" for r in rows))
for a, rows in arms.items():
    if a == ref:
        continue
    print(f"\n{a} vs {ref} (mean % change):")
    for k in ("total_cost_usd", "wall_s", "duration_api_ms", "output_tokens", "cache_read_input_tokens", "score", "quality_score"):
        base = st.fmean([(r.get(k) or 0) for r in arms[ref]])
        new = st.fmean([(r.get(k) or 0) for r in rows])
        print(f"   {k:28} {new:12.4g} vs {base:12.4g}  {100 * (new - base) / base if base else float('nan'):+6.1f}%")
