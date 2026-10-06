"""Paired-over-task comparison with cache writes repriced at one TTL, separating
the TTL price effect from token effects. python reprice.py REF ARM [ARM...]
(arm = runs-subdir, cond zirv-nojev). Sonnet 5.5 $/MTok: in 2, read 0.2, 5m 2.5, 1h 4, out 10."""
import glob, json, os, random, statistics as st, sys
from collections import defaultdict

B = os.path.join(os.path.dirname(os.path.abspath(__file__)), "bench")
P = {"in": 2, "read": 0.2, "5m": 2.5, "1h": 4, "out": 10}


def cost(r, ttl):
    w = r["cache_creation_input_tokens"]
    return (r["input_tokens"] * P["in"] + r["cache_read_input_tokens"] * P["read"]
            + w * P[ttl] + r["output_tokens"] * P["out"]) / 1e6


def load(sub):
    rows = defaultdict(list)
    for p in [p for s in sub.split("+") for p in glob.glob(os.path.join(B, s, "*__zirv-nojev__r*", "result.json"))]:
        r = json.load(open(p))
        if r.get("is_error") or r.get("jev_invalid"):
            continue
        rows[r["task"]].append({
            "cost": r["total_cost_usd"], "cost@5m": cost(r, "5m"), "cost@1h": cost(r, "1h"),
            "recon": cost(r, "5m" if r["cache_creation_ephemeral_5m_input_tokens"] else "1h") / r["total_cost_usd"],
            "wall_s": r["wall_s"], "agent_s": r["duration_ms"] / 1000, "out_tok": r["output_tokens"],
            "write_tok": r["cache_creation_input_tokens"], "read_tok": r["cache_read_input_tokens"],
            "turns": r["num_turns"], "tests": 100 * (r.get("score") or 0),
            "judge": 100 * r["quality_score"] if r.get("quality_score") is not None else None})
    return rows


KEYS = ["cost", "cost@5m", "cost@1h", "wall_s", "agent_s", "out_tok", "write_tok", "read_tok", "turns", "tests", "judge"]
random.seed(11)


def paired(a, b, k):
    def tm(rows, t):
        v = [m[k] for m in rows[t] if m[k] is not None]
        return st.fmean(v) if v else None
    pairs = [(tm(a, t), tm(b, t)) for t in sorted(set(a) & set(b))]
    pairs = [(x, y) for x, y in pairs if x is not None and y is not None]
    d = [x - y for x, y in pairs]
    boots = sorted(st.fmean(random.choice(d) for _ in d) for _ in range(4000))
    return st.fmean(d), boots[100], boots[3899], len(pairs), st.fmean(y for _, y in pairs)


ref, arms = sys.argv[1], sys.argv[2:]
data = {a: load(a) for a in [ref] + arms}
for a, rows in data.items():
    allm = [m for ms in rows.values() for m in ms]
    rc = [m["recon"] for m in allm]
    print(f"{a:12} n={len(allm):2} recon={min(rc):.4f}..{max(rc):.4f} " + " ".join(
        f"{k}={st.fmean([m[k] for m in allm if m[k] is not None]):.4g}" for k in KEYS))
for a in arms:
    print(f"\n{a} minus {ref} (paired over tasks, mean diff [95% CI], % of ref, * = CI excludes 0)")
    for k in KEYS:
        mean, lo, hi, n, base = paired(data[a], data[ref], k)
        pct = f"{100 * mean / base:+6.1f}%" if base else ""
        print(f"  {k:10} {mean:+10.4g} [{lo:+.4g}, {hi:+.4g}] {pct} tasks={n}{' *' if lo > 0 or hi < 0 else ''}")
