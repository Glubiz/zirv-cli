#!/usr/bin/env python3
"""Auxiliary grader for t05_dedupe (kind=judge).

The task's real score comes from the blind judge against rubric.md (see
run.py); this script only computes `visible_ok` by running the repo's
visible test suite. It tolerates the one known baseline-red visible test
(tests/test_rules.py::test_regex_rule_case_insensitive), which is
unrelated to this task, and treats visible_ok as "no NEW failures beyond
that baseline".
"""
import json
import re
import subprocess
import sys
from pathlib import Path

BASELINE_VISIBLE_FAILURES = {"test_regex_rule_case_insensitive"}
TIMEOUT_S = 180


def _short_name(dotted):
    return dotted.rsplit(".", 1)[-1]


def main():
    result = {"score": 0.0, "passed": 0, "total": 0, "visible_ok": False, "details": ""}
    try:
        repo_dir = Path(sys.argv[1]).resolve()
        python = sys.executable or "python"
        proc = subprocess.run(
            [python, "-m", "unittest", "discover", "-s", "tests", "-t", str(repo_dir)],
            cwd=str(repo_dir),
            capture_output=True,
            text=True,
            timeout=TIMEOUT_S,
        )
        output = (proc.stdout or "") + "\n" + (proc.stderr or "")
        m = re.search(r"Ran (\d+) tests?", output)
        total = int(m.group(1)) if m else 0
        fail_names = re.findall(r"^(?:FAIL|ERROR): .*?\(([\w.]+)\)", output, re.MULTILINE)
        fail_short = {_short_name(n) for n in fail_names}
        passed = max(total - len(fail_names), 0)
        visible_ok = fail_short.issubset(BASELINE_VISIBLE_FAILURES)
        result.update(
            {
                "score": round(passed / total, 4) if total else 0.0,
                "passed": passed,
                "total": total,
                "visible_ok": visible_ok,
                "details": (
                    f"visible {passed}/{total} passed; failures beyond baseline="
                    f"{sorted(fail_short - BASELINE_VISIBLE_FAILURES)}"
                ),
            }
        )
    except Exception as exc:  # pragma: no cover - defensive
        result["details"] = f"grade.py error: {exc!r}"
    print(json.dumps(result))


if __name__ == "__main__":
    main()
