#!/usr/bin/env python3
"""Grader for t14_bugsweep (kind=tests).

Copies hidden/*.py into <repo>/tests_hidden and runs it with unittest
discovery; score = passed/total. Also runs the repo's visible suite and
reports visible_ok. One of the four symptoms in this task is D4 (regex
rules are case-sensitive), whose root-cause fix is
`tests/test_rules.py::test_regex_rule_case_insensitive`'s known baseline
failure; fixing it must happen in the library code, not by editing that
test, so -- like t07_redtest -- this grader forces score 0 if an existing line
of tests/test_rules.py is changed or removed (`git diff --unified=0 HEAD`
shows a '-' line) or an added line holds skip/skipIf/skipUnless/skipTest/
expectedFailure. Other added lines (new tests) are allowed.
"""
import json
import re
import shutil
import subprocess
import sys
from pathlib import Path

BASELINE_VISIBLE_FAILURES = {"test_regex_rule_case_insensitive"}
TIMEOUT_S = 180


def _short_name(dotted):
    return dotted.rsplit(".", 1)[-1]


def _run_unittest(python, repo_dir, start_dir):
    try:
        proc = subprocess.run(
            [python, "-m", "unittest", "discover", "-s", start_dir, "-t", str(repo_dir)],
            cwd=str(repo_dir),
            capture_output=True,
            text=True,
            timeout=TIMEOUT_S,
        )
    except Exception as exc:
        return 0, 0, [], f"failed to run {start_dir}: {exc!r}"
    output = (proc.stdout or "") + "\n" + (proc.stderr or "")
    m = re.search(r"Ran (\d+) tests?", output)
    total = int(m.group(1)) if m else 0
    fail_names = re.findall(r"^(?:FAIL|ERROR): .*?\(([\w.]+)\)", output, re.MULTILINE)
    passed = max(total - len(fail_names), 0)
    return passed, total, fail_names, output


# Real skip mechanisms only, so a docstring, comment or test name saying "skip blank rows" is not one.
SKIP_RE = re.compile(
    r"@\s*(unittest\.)?(skip\w*|expectedFailure)\b|\bself\.skipTest\s*\(|\bunittest\.expectedFailure\b"
)


def _test_file_lines_removed(repo_dir):
    """True when tests/test_rules.py lost or changed an existing line (a diff
    '-' line) or gained a skip/expectedFailure line (which could disable the
    failing test); other additions are allowed. Fails closed on a git error."""
    try:
        proc = subprocess.run(
            ["git", "diff", "--unified=0", "HEAD", "--", "tests/test_rules.py"],
            cwd=str(repo_dir),
            capture_output=True,
            text=True,
            timeout=30,
        )
    except Exception:
        return True
    if proc.returncode != 0:
        return True
    for line in proc.stdout.splitlines():
        if line.startswith("-") and not line.startswith("---"):
            return True
        if line.startswith("+") and not line.startswith("+++") and SKIP_RE.search(line):
            return True
    return False


def main():
    result = {"score": 0.0, "passed": 0, "total": 0, "visible_ok": False, "details": ""}
    try:
        repo_dir = Path(sys.argv[1]).resolve()
        python = sys.executable or "python"

        if _test_file_lines_removed(repo_dir):
            result["details"] = "tests/test_rules.py had existing lines changed or removed, or a skip added; the D4 fix must not edit or disable the existing tests"
            print(json.dumps(result))
            return

        hidden_src = Path(__file__).resolve().parent / "hidden"
        hidden_dst = repo_dir / "tests_hidden"
        if hidden_dst.exists():
            shutil.rmtree(hidden_dst)
        hidden_dst.mkdir(parents=True, exist_ok=True)
        (hidden_dst / "__init__.py").touch()
        for f in sorted(hidden_src.glob("*.py")):
            (hidden_dst / f.name).write_text(f.read_text(encoding="utf-8"), encoding="utf-8")

        h_passed, h_total, _h_fail_names, _h_output = _run_unittest(python, repo_dir, "tests_hidden")
        _v_passed, v_total, v_fail_names, _v_output = _run_unittest(python, repo_dir, "tests")

        v_fail_short = {_short_name(n) for n in v_fail_names}
        visible_ok = v_fail_short.issubset(BASELINE_VISIBLE_FAILURES)

        score = (h_passed / h_total) if h_total else 0.0
        result.update(
            {
                "score": round(score, 4),
                "passed": h_passed,
                "total": h_total,
                "visible_ok": visible_ok,
                "details": (
                    f"hidden {h_passed}/{h_total} passed; visible total={v_total}, "
                    f"failures beyond baseline={sorted(v_fail_short - BASELINE_VISIBLE_FAILURES)}"
                ),
            }
        )
    except Exception as exc:  # pragma: no cover - defensive
        result["details"] = f"grade.py error: {exc!r}"
    print(json.dumps(result))


if __name__ == "__main__":
    main()
