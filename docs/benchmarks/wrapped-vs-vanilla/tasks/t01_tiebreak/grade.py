#!/usr/bin/env python3
"""Grader for t01_tiebreak (kind=answer).

Checks that the agent's final answer names where the tie-break happens
(rules.py / categorize), that it mentions priority, and that it states the
tie-break rule correctly (earliest/first/lowest-index rule wins).
"""
import json
import re
import sys
from pathlib import Path

REQUIRED_PATTERNS = [
    r"(rules\.py|categorize)",
    r"priorit",
    r"(earliest|first|lowest index|lowest-index|list order|order (?:it|they)|declared order|order in (?:the )?list)",
]


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
