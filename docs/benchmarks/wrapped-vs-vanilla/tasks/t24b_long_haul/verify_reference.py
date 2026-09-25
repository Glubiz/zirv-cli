#!/usr/bin/env python
"""Solvability proof for t24_long_haul's reference patches (spec item 5).

For every step N (1..22):

  - FAIL-check (kind=tests steps only): a FRESH copy of the pristine
    template, with step (N-1)'s cumulative reference patch applied (or
    nothing, for N=1), must FAIL step N's own hidden tests.
  - PASS-check: a FRESH copy of the pristine template, with step N's own
    cumulative reference patch applied, must PASS the hidden tests for
    EVERY tests-kind step from 1 through N (not just step N's own -- this
    also catches a later step's patch accidentally regressing an earlier
    one), and the template's own visible suite must stay green (allowing
    only the one pre-existing baseline failure).

Judge-kind steps (11, 22) have no hidden tests, so they only get the
PASS-check's visible-suite half; there is nothing to run a FAIL-check
against.

This never touches the real `template/` directory -- everything happens
in throwaway temp copies, each its own one-commit git repo (mirroring how
the harness's actual `template/` is a standalone git repo), removed when
done. Run this in the foreground:

    C:\\Python311\\python.exe tasks\\t24_long_haul\\verify_reference.py
"""

from __future__ import annotations

import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

PYTHON_EXE = r"C:\Python311\python.exe"
GIT_EXE = r"C:\Program Files\Git\cmd\git.exe"

TASK_DIR = Path(__file__).resolve().parent
BENCH_DIR = TASK_DIR.parent.parent
TEMPLATE_DIR = BENCH_DIR / "template"
REFERENCE_DIR = TASK_DIR / "reference"
HIDDEN_DIR = TASK_DIR / "hidden"
RUBRIC_DIR = TASK_DIR / "rubric"
PROMPTS_DIR = TASK_DIR / "prompts"

BASELINE_VISIBLE_FAILURES = {"test_regex_rule_case_insensitive"}

UNITTEST_TIMEOUT_S = 120


def _python():
    return PYTHON_EXE if Path(PYTHON_EXE).exists() else sys.executable


def _git():
    return GIT_EXE if Path(GIT_EXE).exists() else "git"


def read_steps():
    """Mirror run.py's read_chain_steps: label -> kind ('tests'/'judge'/'none')."""
    steps = []
    for p in sorted(PROMPTS_DIR.glob("*.txt")):
        label = p.stem
        hidden = HIDDEN_DIR / f"step_{label}"
        rubric = RUBRIC_DIR / f"step_{label}.md"
        if hidden.is_dir():
            kind = "tests"
        elif rubric.exists():
            kind = "judge"
        else:
            kind = "none"
        steps.append({"label": label, "kind": kind, "hidden": hidden})
    return steps


def make_pristine_copy(dest: Path) -> None:
    shutil.copytree(TEMPLATE_DIR, dest)
    run_git(dest, ["init", "-q"])
    run_git(dest, ["config", "core.autocrlf", "true"])
    run_git(dest, ["config", "user.email", "verify@local"])
    run_git(dest, ["config", "user.name", "verify"])
    run_git(dest, ["add", "-A"])
    run_git(dest, ["commit", "-q", "-m", "pristine"])


def run_git(cwd: Path, args) -> subprocess.CompletedProcess:
    return subprocess.run([_git(), "-C", str(cwd), *args], capture_output=True, text=True)


def apply_patch(dest: Path, patch_path: Path) -> subprocess.CompletedProcess:
    return run_git(dest, ["apply", str(patch_path)])


def run_unittest(dest: Path, start_dir: str):
    try:
        proc = subprocess.run(
            [_python(), "-m", "unittest", "discover", "-s", start_dir, "-t", "."],
            cwd=str(dest), capture_output=True, text=True, timeout=UNITTEST_TIMEOUT_S,
        )
    except Exception as exc:  # pragma: no cover
        return 0, 0, [], f"failed to run: {exc!r}"
    output = (proc.stdout or "") + "\n" + (proc.stderr or "")
    m = re.search(r"Ran (\d+) tests?", output)
    total = int(m.group(1)) if m else 0
    fail_names = re.findall(r"^(?:FAIL|ERROR): .*?\(([\w.]+)\)", output, re.MULTILINE)
    passed = max(total - len(fail_names), 0)
    return passed, total, fail_names, output


def run_hidden(dest: Path, hidden_dir: Path):
    tests_hidden = dest / "tests_hidden"
    if tests_hidden.exists():
        shutil.rmtree(tests_hidden)
    tests_hidden.mkdir(parents=True)
    (tests_hidden / "__init__.py").touch()
    for f in sorted(hidden_dir.glob("*.py")):
        (tests_hidden / f.name).write_text(f.read_text(encoding="utf-8"), encoding="utf-8")
    try:
        return run_unittest(dest, "tests_hidden")
    finally:
        shutil.rmtree(tests_hidden, ignore_errors=True)


def run_visible(dest: Path):
    return run_unittest(dest, "tests")


def main() -> int:
    if not TEMPLATE_DIR.is_dir():
        print(f"FATAL: template dir not found at {TEMPLATE_DIR}")
        return 1

    steps = read_steps()
    print(f"Found {len(steps)} steps: "
          f"{sum(1 for s in steps if s['kind'] == 'tests')} tests-kind, "
          f"{sum(1 for s in steps if s['kind'] == 'judge')} judge-kind\n")

    overall_ok = True
    work_root = Path(tempfile.mkdtemp(prefix="t24verify_"))
    try:
        for idx, step in enumerate(steps):
            label = step["label"]
            kind = step["kind"]
            patch_n = REFERENCE_DIR / f"step_{label}.patch"
            prev_patch = REFERENCE_DIR / f"step_{steps[idx - 1]['label']}.patch" if idx > 0 else None

            print(f"--- step {label} ({kind}) ---")
            if not patch_n.exists():
                print(f"  FAIL: missing {patch_n}")
                overall_ok = False
                continue

            # FAIL-check: N-1's patch (or pristine) must NOT satisfy step N's
            # own hidden tests.
            if kind == "tests":
                fail_dir = work_root / f"fail_{label}"
                make_pristine_copy(fail_dir)
                if prev_patch is not None:
                    r = apply_patch(fail_dir, prev_patch)
                    if r.returncode != 0:
                        print(f"  FAIL: could not apply {prev_patch.name} to prove step {label} "
                              f"fails without it: {r.stderr.strip()[:300]}")
                        overall_ok = False
                        shutil.rmtree(fail_dir, ignore_errors=True)
                        continue
                passed, total, fail_names, _out = run_hidden(fail_dir, step["hidden"])
                fail_check_ok = total > 0 and passed < total
                status = "ok" if fail_check_ok else "UNEXPECTED PASS"
                print(f"  fail-check (without step {label}): {passed}/{total} passed -> {status}")
                if not fail_check_ok:
                    overall_ok = False
                shutil.rmtree(fail_dir, ignore_errors=True)

            # PASS-check: step N's own patch must satisfy hidden tests for
            # steps 1..N (cumulative) and keep the visible suite green.
            pass_dir = work_root / f"pass_{label}"
            make_pristine_copy(pass_dir)
            r = apply_patch(pass_dir, patch_n)
            if r.returncode != 0:
                print(f"  FAIL: {patch_n.name} did not apply cleanly: {r.stderr.strip()[:300]}")
                overall_ok = False
                shutil.rmtree(pass_dir, ignore_errors=True)
                continue

            cumulative_ok = True
            for earlier in steps[: idx + 1]:
                if earlier["kind"] != "tests":
                    continue
                passed, total, fail_names, _out = run_hidden(pass_dir, earlier["hidden"])
                ok = total > 0 and passed == total
                if not ok:
                    cumulative_ok = False
                    print(f"  FAIL: with step {label} applied, step {earlier['label']}'s hidden "
                          f"tests are {passed}/{total} (failures: {fail_names})")
            if cumulative_ok:
                n_tests_steps = sum(1 for s in steps[: idx + 1] if s["kind"] == "tests")
                print(f"  pass-check: hidden tests for all {n_tests_steps} tests-kind step(s) "
                      f"1..{label} PASS")
            else:
                overall_ok = False

            v_passed, v_total, v_fail_names, _v_out = run_visible(pass_dir)
            v_fail_short = {n.rsplit(".", 1)[-1] for n in v_fail_names}
            visible_ok = v_fail_short.issubset(BASELINE_VISIBLE_FAILURES)
            print(f"  visible suite: {v_passed}/{v_total} passed, "
                  f"failures={sorted(v_fail_short)} -> {'ok' if visible_ok else 'REGRESSION'}")
            if not visible_ok:
                overall_ok = False

            shutil.rmtree(pass_dir, ignore_errors=True)
            print()
    finally:
        shutil.rmtree(work_root, ignore_errors=True)

    print("=" * 60)
    print("OVERALL:", "PASS" if overall_ok else "FAIL")
    return 0 if overall_ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
