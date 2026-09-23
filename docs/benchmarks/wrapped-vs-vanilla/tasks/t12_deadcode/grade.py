#!/usr/bin/env python3
"""Grader for t12_deadcode (kind=answer).

Ground truth was computed with an AST walk over the FROZEN template's
ledgerlite/ package (excluding tests/): every `def` was matched against
every `Name`/`Attribute` reference elsewhere in ledgerlite/*.py. Two
functions have zero references anywhere in the package or CLI:

    report.monthly_totals
    report.summarize

(report.category_totals is referenced once, from inside the otherwise-dead
report.summarize -- so by this literal "has at least one caller anywhere in
ledgerlite/" rule it does not count as dead, even though that caller is
itself unreachable from the CLI. See tasks/README.md for the full script
and its output.)

Scoring: score = (# correct dead-name mentions) / 2, matched on the bare
function name with word boundaries (so "report.summarize", "summarize()",
etc. all count) -- minus 0.25 for each mention of a function name that is
actually called somewhere in the package (a hallucinated dead-code claim),
floored at 0 and capped at 1.0. `visible_ok` is always true (this task
does not touch code).
"""
import json
import re
import sys
from pathlib import Path

TRUTH = ["monthly_totals", "summarize"]

# Every other function/method defined in ledgerlite/ -- each has at least
# one real reference elsewhere in the package, per the same AST walk.
LIVE = [
    "_build_parser", "_cmd_import", "_cmd_list", "_cmd_summary", "_cmd_categorize", "main",
    "parse_money", "_parse_date", "read_csv", "category_totals", "page",
    "_rule_matches", "categorize", "_txn_to_dict", "_txn_from_dict", "save", "load",
]


def _mentioned(name, text):
    return re.search(rf"\b{re.escape(name)}\b", text, re.IGNORECASE) is not None


def main():
    result = {"score": 0.0, "passed": 0, "total": len(TRUTH), "visible_ok": True, "details": ""}
    try:
        text_path = Path(sys.argv[2])
        text = text_path.read_text(encoding="utf-8", errors="replace")

        # Grade only the final `DEAD:` line the prompt asks for, so prose that
        # merely discusses a live function is never penalised.
        dead_lines = re.findall(r"^\s*\**\s*DEAD:\s*(.*)$", text, re.IGNORECASE | re.MULTILINE)
        claim = dead_lines[-1] if dead_lines else text
        matched_truth = [n for n in TRUTH if _mentioned(n, claim)]
        matched_live = [n for n in LIVE if _mentioned(n, claim)]

        base = len(matched_truth) / len(TRUTH)
        penalty = 0.25 * len(matched_live)
        score = max(0.0, min(1.0, base - penalty))

        result["passed"] = len(matched_truth)
        result["score"] = round(score, 4)
        result["details"] = (
            f"DEAD line found: {bool(dead_lines)}; matched dead names: {matched_truth}; "
            f"hallucinated live-name mentions (-0.25 each): {matched_live}"
        )
    except Exception as exc:  # pragma: no cover - defensive
        result["details"] = f"grade.py error: {exc!r}"
    print(json.dumps(result))


if __name__ == "__main__":
    main()
