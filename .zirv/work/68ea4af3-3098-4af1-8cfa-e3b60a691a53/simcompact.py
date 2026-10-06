"""Simulate cache-read savings of compacting at step boundaries once context >= T.
After a compaction, later rounds keep their growth but restart from C0 tokens."""
import glob, json, os, sys

C0 = 35000  # system + tools + compact summary, from the probe and the r10 first-round context
COMPACT_COST_TOK = 0  # the compact call itself: one read of the context (added below)
runs, cond = sys.argv[1].rsplit(":", 1)
for T in (100000, 130000, 160000):
    tot_base = tot_new = 0
    n_comp = 0
    for rd in sorted(glob.glob(os.path.join(runs, f"*__{cond}__r*"))):
        ev = []  # (timestamp, kind, ctx) kind = 'user' prompt boundary or 'asst'
        seen = set()
        for f in glob.glob(os.path.join(rd, "transcripts", "*.jsonl")):
            for line in open(f, encoding="utf-8", errors="ignore"):
                e = json.loads(line)
                if e.get("isSidechain"):
                    continue
                m = e.get("message") or {}
                if e.get("type") == "user" and isinstance(m.get("content"), str) and not e.get("isMeta"):
                    ev.append((e["timestamp"], "user", 0))
                elif e.get("type") == "assistant" and m.get("id") not in seen:
                    seen.add(m.get("id"))
                    u = m.get("usage") or {}
                    ev.append((e["timestamp"], "asst", (u.get("input_tokens") or 0) + (u.get("cache_read_input_tokens") or 0) + (u.get("cache_creation_input_tokens") or 0)))
        ev.sort()
        offset = 0
        last = 0
        for _, k, c in ev:
            if k == "user":
                if last - offset >= T:
                    tot_new += last - offset  # the compact call reads the context once
                    offset = last - C0
                    n_comp += 1
                continue
            last = c
            tot_base += c
            tot_new += max(c - offset, C0)
    print(f"T={T//1000}k: compactions/run={n_comp/3:.1f} context-sum base={tot_base/3e6:.2f}M new={tot_new/3e6:.2f}M saving={100*(1-tot_new/tot_base):.0f}% of reads")
