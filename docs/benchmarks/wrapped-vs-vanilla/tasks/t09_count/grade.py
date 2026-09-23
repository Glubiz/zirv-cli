#!/usr/bin/env python3
"""Grader for t09_count (kind=answer).

Ground truth was computed by applying ledgerlite.rules.DEFAULT_RULES to
every row of examples/sample.csv (see tasks/README.md for the row-by-row
breakdown): 11 of the 48 rows categorise as "groceries".
"""
import json
import re
import sys
from pathlib import Path

REQUIRED_PATTERNS = [r"\b11\b"]


def main():
    result = {"score": 0.0, "passed": 0, "total": len(REQUIRED_PATTERNS), "visible_ok": True, "details": ""}
    try:
        text_path = Path(sys.argv[2])
        text = text_path.read_text(encoding="utf-8", errors="replace")
        matched = [pat for pat in REQUIRED_PATTERNS if re.search(pat, text, re.IGNORECASE)]
        n = len(matched)
        result["passed"] = n
        result["score"] = round(n / len(REQUIRED_PATTERNS), 4)
        result["details"] = f"matched {n}/{len(REQUIRED_PATTERNS)} required patterns: {matched}"
    except Exception as exc:  # pragma: no cover - defensive
        result["details"] = f"grade.py error: {exc!r}"
    print(json.dumps(result))


if __name__ == "__main__":
    main()
