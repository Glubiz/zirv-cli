"""Per-arm means and paired-over-task bootstrap diffs vs a reference arm.
python stats.py REF ARM [ARM...]   (arm = runs-subdir:cond, relative to bench/)"""
import glob, json, os, random, re, statistics as st, sys
from collections import Counter, defaultdict

B = os.path.join(os.path.dirname(os.path.abspath(__file__)), "bench")
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from rounds_by_kind import kind  # noqa: E402


def transcript_stats(rd):
    rounds = defaultdict(list)
    for f in glob.glob(os.path.join(rd, "transcripts", "*.jsonl")):
        for line in open(f, encoding="utf-8", errors="ignore"):
            try:
                e = json.loads(line)
            except Exception:
                continue
            if e.get("isSidechain") or e.get("type") != "assistant":
                continue
            m = e.get("message") or {}
            rounds.setdefault(m.get("id"), [])
            for b in m.get("content") or []:
                if isinstance(b, dict) and b.get("type") == "tool_use":
                    rounds[m.get("id")].append(kind(b.get("name"), b.get("input") or {}))
    dom = Counter(Counter(ks).most_common(1)[0][0] if ks else "text" for ks in rounds.values())
    calls = Counter(k for ks in rounds.values() for k in ks)
    return {"rounds": len(rounds), "r_edit_test": dom["edit-test"], "r_edit_src": dom["edit-src"],
            "r_test_run": dom["test-run"], "c_edit_test": calls["edit-test"],
            "r_read": dom["read"] + dom["shell-read"]}


def load(arm):
    subs, cond = arm.rsplit(":", 1)
    rows = defaultdict(list)
    paths = [p for sub in subs.split("+") for p in glob.glob(os.path.join(B, sub, f"*__{cond}__r*", "result.json"))]
    for p in paths:
        r = json.load(open(p, encoding="utf-8"))
        if r.get("is_error") or r.get("jev_invalid"):
            continue
        m = transcript_stats(os.path.dirname(p))
        m.update({"cost": r.get("total_cost_usd") or 0, "agent_s": (r.get("duration_ms") or 0) / 1000,
                  "api_s": (r.get("duration_api_ms") or 0) / 1000, "out_tok": r.get("output_tokens") or 0,
                  "tests": 100 * (r.get("score") or 0),
                  "judge": 100 * r["quality_score"] if r.get("quality_score") is not None else None})
        rows[r["task"]].append(m)
    return rows


KEYS = ["rounds", "r_edit_test", "r_edit_src", "r_test_run", "r_read", "c_edit_test", "out_tok", "cost", "agent_s", "api_s", "tests", "judge"]
random.seed(11)


def paired(a, b, k):
    def tmean(rows, t):
        v = [m[k] for m in rows[t] if m[k] is not None]
        return st.fmean(v) if v else None
    pairs = [(tmean(a, t), tmean(b, t)) for t in sorted(set(a) & set(b))]
    pairs = [(x, y) for x, y in pairs if x is not None and y is not None]
    if len(pairs) < 3:
        return None
    d = [x - y for x, y in pairs]
    boots = sorted(st.fmean(random.choice(d) for _ in d) for _ in range(4000))
    return st.fmean(d), boots[100], boots[3899], len(pairs)


ref, arms = sys.argv[1], sys.argv[2:]
data = {a: load(a) for a in [ref] + arms}
print(f"{'arm':28} {'n':>3} " + " ".join(f"{k:>11}" for k in KEYS))
for a, rows in data.items():
    allm = [m for ms in rows.values() for m in ms]
    if not allm:
        print(f"{a:28}   0"); continue
    print(f"{a:28} {len(allm):3} " + " ".join(
        f"{st.fmean([m[k] for m in allm if m[k] is not None]):11.2f}" for k in KEYS))
for a in arms:
    print(f"\n{a} minus {ref} (paired over tasks, mean diff [95% CI], * = CI excludes 0):")
    for k in KEYS:
        res = paired(data[a], data[ref], k)
        if res:
            mean, lo, hi, n = res
            print(f"   {k:12} {mean:+9.2f}  [{lo:+.2f}, {hi:+.2f}]  tasks={n}{' *' if lo > 0 or hi < 0 else ''}")
