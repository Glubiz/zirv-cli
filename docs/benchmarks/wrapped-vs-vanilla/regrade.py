"""Re-grade judge-kind runs in place (after a grader fix). Usage: python regrade.py <runs_root> [tasks...]"""
import json, sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent))
import run as R
root = Path(sys.argv[1]); only = set(sys.argv[2:])
bench = Path(__file__).resolve().parent
for rd in sorted(root.iterdir()):
    rj = rd / "result.json"
    if not rj.exists(): continue
    res = json.loads(rj.read_text(encoding="utf-8"))
    task = res["task"]
    if only and task not in only: continue
    td = bench / "tasks" / task
    kind = (td / "kind.txt").read_text(encoding="utf-8").strip()
    if kind != "judge": continue
    prompt = (td / "prompt.txt").read_text(encoding="utf-8")
    g = R.grade(td, kind, rd / "repo", rd / "result.txt", prompt)
    old = res["score"]
    for k in ("score", "passed", "total", "visible_ok", "details", "judge_score", "judge_reasoning"):
        res[k] = g.get(k)
    rj.write_text(json.dumps(res, indent=1), encoding="utf-8")
    print(f"{rd.name}: {old} -> {res['score']}  {(res['judge_reasoning'] or '')[:90]}")
