#!/usr/bin/env python3
"""Solvability proof for an orchestration task (kind=orch).

    python verify_orch_reference.py [task_id ...]     # default: every kind=orch task

For each task, in throwaway copies of `template/` (never touching it):

  - REFERENCE: apply `tasks/<task>/reference/NN_<source>.patch` in order (one patch
    per source task, in `sources.txt` order -- each is that source's own
    `reference.patch` re-based onto the state the earlier patches leave, because
    the sources all edit ledgerlite/cli.py and ledgerlite/store.py and the
    originals do not stack), then run EVERY source's own grade.py. Every
    source must score 1.0 with the visible suite green, so the task's mean
    score is 1.0.
  - PRISTINE: the untouched template must score 0 on the task's mean score
    (the task is not already solved).

Exit code 0 only when every task passes both checks. Run in the foreground.
"""
from __future__ import annotations

import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import orch  # noqa: E402
import run as H  # noqa: E402

BENCH = Path(__file__).resolve().parent


def orch_tasks():
    return sorted(p.name for p in (BENCH / "tasks").iterdir()
                  if (p / "kind.txt").exists() and H.read_text(p / "kind.txt").strip() == orch.ORCH_KIND)


def apply_patches(repo, patches):
    for patch in patches:
        proc = subprocess.run([H.GIT_EXE, "apply", "--whitespace=nowarn", str(patch)], cwd=str(repo),
                              capture_output=True, text=True)
        if proc.returncode != 0:
            return f"{patch.name} did not apply: {proc.stderr.strip()[:300]}"
    return None


def verify(task):
    task_dir = BENCH / "tasks" / task
    sources = orch.read_sources(task_dir)
    patches = sorted((task_dir / "reference").glob("*.patch"))
    ok = True
    print(f"== {task}: sources {sources}")
    if len(patches) != len(sources):
        print(f"  FAIL: {len(patches)} reference patches for {len(sources)} sources")
        return False
    for patch, src in zip(patches, sources):
        if src not in patch.name:
            print(f"  FAIL: {patch.name} is not the patch for source {src}")
            ok = False
    prompt_ok = (task_dir / "prompt.txt").read_text(encoding="utf-8") == orch.compose_task_prompt(BENCH, sources)
    print(f"  prompt.txt == composition of the sources' prompts: {'ok' if prompt_ok else 'STALE'}")
    ok = ok and prompt_ok
    work = Path(tempfile.mkdtemp(prefix="orchverify_"))
    try:
        ref = work / "reference"
        shutil.copytree(BENCH / "template", ref)
        err = apply_patches(ref, patches)
        if err:
            print(f"  FAIL: {err}")
            return False
        g = orch.grade_sources(BENCH, task_dir, ref, ref / "NONEXISTENT_RESULT.txt")
        ref_ok = g["score"] >= 0.999 and g["visible_ok"] and all(s >= 0.999 for s in g["per_source"].values())
        print(f"  reference: score {g['score']} ({g['passed']}/{g['total']} hidden), visible_ok={g['visible_ok']}, "
              f"per source {g['per_source']} -> {'ok' if ref_ok else 'FAIL'}")
        plain = work / "pristine"
        shutil.copytree(BENCH / "template", plain)
        p = orch.grade_sources(BENCH, task_dir, plain, plain / "NONEXISTENT_RESULT.txt")
        plain_ok = p["score"] == 0.0
        print(f"  pristine:  score {p['score']} ({p['passed']}/{p['total']} hidden), per source "
              f"{p['per_source']} -> {'ok' if plain_ok else 'FAIL (expected 0)'}")
        ok = ok and ref_ok and plain_ok
    finally:
        H.rmtree_robust(work)
    return ok


def main(argv):
    tasks = argv or orch_tasks()
    if not tasks:
        print("no kind=orch tasks found")
        return 1
    results = {t: verify(t) for t in tasks}
    print("OVERALL:", "PASS" if all(results.values()) else "FAIL")
    return 0 if all(results.values()) else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
