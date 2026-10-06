"""Classify each API round (assistant message.id) of each run by what its tool calls do.
python rounds_by_kind.py <runs_dir>:<cond> [...]"""
import glob, json, os, re, statistics as st, sys
from collections import Counter, defaultdict

def kind(name, inp):
    if name == "Bash":
        c = (inp.get("command") or "").strip()
        if re.search(r"\bzirv\s+skill\b", c): return "zirv-skill"
        if re.search(r"\bzirv\s+ctx\s+(status|inbox|recall|search)\b", c): return "zirv-ctx-meta"
        if re.search(r"\bzirv\s+workflow\b", c): return "zirv-workflow"
        if re.search(r"\bzirv\s+ctx\s+run\b", c) and re.search(r"pytest|unittest|python -m", c): return "test-run"
        if re.search(r"pytest|unittest|python3? -m (pytest|unittest)|python3? tests?/", c): return "test-run"
        if re.search(r"\bgit\s+(diff|status|log|show)\b", c): return "git-read"
        if re.search(r"^(sed -n|cat|head|tail|nl|awk|grep|rg|ls|find|wc)\b", c): return "shell-read"
        if re.search(r"python3? - <<|python3? -c", c): return "py-script"
        return "bash-other"
    if name in ("Read", "Grep", "Glob"): return "read"
    if name in ("Edit", "MultiEdit", "Write"):
        p = inp.get("file_path") or ""
        return "edit-test" if re.search(r"(^|[\\/])tests?[\\/]|test_", p) else "edit-src"
    if name in ("TodoWrite", "TaskCreate", "TaskUpdate"): return "todo"
    if name == "Skill": return "skill-tool"
    if name.startswith("mcp__"): return "mcp"
    return name

for spec in (sys.argv[1:] if __name__ == "__main__" else []):
    runs, cond = spec.rsplit(":", 1)
    per_run = []
    for rd in sorted(glob.glob(os.path.join(runs, f"*__{cond}__r*"))):
        if not os.path.exists(os.path.join(rd, "result.json")): continue
        rounds = defaultdict(list); out_tok = Counter()
        for f in glob.glob(os.path.join(rd, "transcripts", "*.jsonl")):
            for line in open(f, encoding="utf-8", errors="ignore"):
                try: e = json.loads(line)
                except Exception: continue
                if e.get("isSidechain") or e.get("type") != "assistant": continue
                m = e.get("message") or {}
                mid = m.get("id")
                u = m.get("usage") or {}
                out_tok[mid] = max(out_tok[mid], u.get("output_tokens") or 0)
                for b in m.get("content") or []:
                    if isinstance(b, dict) and b.get("type") == "tool_use":
                        rounds[mid].append(kind(b.get("name"), b.get("input") or {}))
                    elif isinstance(b, dict) and b.get("type") in ("text", "thinking"):
                        rounds.setdefault(mid, [])
        c = Counter()
        for mid, ks in rounds.items():
            if not ks: c["(text-only)"] += 1
            else:
                # attribute the round to its dominant kind
                c[Counter(ks).most_common(1)[0][0]] += 1
        calls = Counter(k for ks in rounds.values() for k in ks)
        per_run.append((c, calls, sum(out_tok.values()), len(rounds)))
    n = len(per_run)
    if not n: print(spec, "no runs"); continue
    keys = sorted({k for c, calls, _, _ in per_run for k in list(c) + list(calls)})
    print(f"\n== {spec}  n={n}  rounds/run={st.fmean(r for *_, r in per_run):.2f}  out_tok/run={st.fmean(o for _, _, o, _ in per_run):.0f}")
    print(f"{'kind':16} {'rounds/run':>10} {'calls/run':>10}")
    for k in keys:
        print(f"{k:16} {st.fmean(c[k] for c, *_ in per_run):10.2f} {st.fmean(calls[k] for _, calls, *_ in per_run):10.2f}")
