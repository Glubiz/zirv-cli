"""Paired-over-task comparison for sbench rounds. python sstats.py REF_DIR ARM_DIR [REF_DIR ARM_DIR ...]
(pairs pooled across rounds; dirs are runs-<round>-<arm> under this folder)."""
import glob, json, os, random, statistics as st, sys
from collections import defaultdict

H = os.path.dirname(os.path.abspath(__file__))
KEYS = ["sub_cost", "total_cost", "main_cost", "wall_s", "sub_s", "sub_out", "sub_reqs", "sub_prompt0", "tests", "judge"]


def load(sub):
    rows = defaultdict(list)
    for p in glob.glob(os.path.join(H, sub, "*__r*", "result.json")):
        r = json.load(open(p))
        if r.get("is_error") or r.get("n_subagents") != 1:
            continue
        rows[r["task"]].append({
            "sub_cost": r["sub"]["cost_usd"], "total_cost": r["total"]["cost_usd"], "main_cost": r["main"]["cost_usd"],
            "wall_s": r["wall_s"], "sub_s": r["sub_duration_s"], "sub_out": r["sub"]["output_tokens"],
            "sub_reqs": r["sub"]["requests"], "sub_prompt0": r["sub_first_prompt_tokens"],
            "tests": 100 * (r.get("score") or 0),
            "judge": 100 * r["quality_score"] if r.get("quality_score") is not None else None})
    return rows


def tmean(rows, t, k):
    v = [m[k] for m in rows[t] if m[k] is not None]
    return st.fmean(v) if v else None


args = sys.argv[1:]
pairs = defaultdict(list)
for ref, arm in zip(args[::2], args[1::2]):
    a, b = load(arm), load(ref)
    print(f"{arm} vs {ref}: runs {sum(map(len, a.values()))} / {sum(map(len, b.values()))}, excluded = errors or not exactly one subagent")
    for k in KEYS:
        for t in sorted(set(a) & set(b)):
            x, y = tmean(a, t, k), tmean(b, t, k)
            if x is not None and y is not None:
                pairs[k].append((x, y))
random.seed(11)
print("arm minus ref (paired over task x round, mean diff [95% CI], % of ref, * = CI excludes 0)")
for k in KEYS:
    d = [x - y for x, y in pairs[k]]
    if not d:
        continue
    boots = sorted(st.fmean(random.choice(d) for _ in d) for _ in range(4000))
    lo, hi, base = boots[100], boots[3899], st.fmean(y for _, y in pairs[k])
    print(f"  {k:11} {st.fmean(d):+10.4g} [{lo:+.4g}, {hi:+.4g}] {100 * st.fmean(d) / base:+6.1f}% ref={base:.4g} n={len(d)}{' *' if lo > 0 or hi < 0 else ''}")
