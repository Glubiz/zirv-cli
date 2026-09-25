"""Re-grade judge-kind runs in place (after a grader fix), or refresh the
work-quality judge's score without re-running any agents.

Usage:
    python regrade.py <runs_root> [tasks...]
    python regrade.py --rejudge-quality <runs_root> [tasks...]

The second form re-runs `call_quality_judge` (quality_rubric.md, opus) for
every existing tests-kind run under <runs_root> (optionally filtered to
`tasks`), updating `quality_score`/`quality_reasoning` in each run's
result.json in place -- no agent is re-run, only the already-produced
`repo/` diff and `result.txt` are re-read.
"""
import json, sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent))
import run as R

argv = sys.argv[1:]
rejudge_quality = bool(argv) and argv[0] == "--rejudge-quality"
if rejudge_quality:
    argv = argv[1:]

root = Path(argv[0])
only = set(argv[1:])
bench = Path(__file__).resolve().parent

for rd in sorted(root.iterdir()):
    rj = rd / "result.json"
    if not rj.exists():
        continue
    res = json.loads(rj.read_text(encoding="utf-8"))
    task = res["task"]
    if only and task not in only:
        continue
    td = bench / "tasks" / task
    kind = (td / "kind.txt").read_text(encoding="utf-8").strip()

    if rejudge_quality:
        if kind != "tests":
            continue
        prompt = (td / "prompt.txt").read_text(encoding="utf-8")
        result_txt = rd / "result.txt"
        result_text = result_txt.read_text(encoding="utf-8") if result_txt.exists() else ""
        old = res.get("quality_score")
        quality_score, quality_reasoning = R.call_quality_judge(prompt, rd / "repo", result_text)
        res["quality_score"] = quality_score
        res["quality_reasoning"] = quality_reasoning
        rj.write_text(json.dumps(res, indent=1), encoding="utf-8")
        print(f"{rd.name}: quality {old} -> {quality_score}  {(quality_reasoning or '')[:90]}")
        continue

    if kind != "judge":
        continue
    prompt = (td / "prompt.txt").read_text(encoding="utf-8")
    g = R.grade(td, kind, rd / "repo", rd / "result.txt", prompt)
    old = res["score"]
    for k in ("score", "passed", "total", "visible_ok", "details", "judge_score", "judge_reasoning"):
        res[k] = g.get(k)
    rj.write_text(json.dumps(res, indent=1), encoding="utf-8")
    print(f"{rd.name}: {old} -> {res['score']}  {(res['judge_reasoning'] or '')[:90]}")
