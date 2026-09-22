"""Category rules and matching for ledgerlite."""

from __future__ import annotations

import re
from dataclasses import dataclass
from typing import List, Optional

from .models import Transaction


@dataclass
class Rule:
    """A single categorisation rule.

    `kind="substring"` matches when `pattern` appears anywhere in the
    transaction's payee or memo, case-insensitively. `kind="regex"` matches
    when `pattern` matches (via `re.search`) against the payee or memo, also
    case-insensitively. When several rules match the same transaction, the
    rule with the highest `priority` wins.
    """

    pattern: str
    category: str
    priority: int = 0
    kind: str = "substring"


def _rule_matches(rule: Rule, txn: Transaction) -> bool:
    if rule.kind == "substring":
        needle = rule.pattern.lower()
        return needle in txn.payee.lower() or needle in txn.memo.lower()
    if rule.kind == "regex":
        pattern = re.compile(rule.pattern)
        return bool(pattern.search(txn.payee) or pattern.search(txn.memo))
    raise ValueError(f"unknown rule kind: {rule.kind!r}")


def categorize(txn: Transaction, rules: List[Rule]) -> Optional[str]:
    """Return the category `rules` assigns to `txn`, or None if none match.

    When several rules match, the rule with the highest `priority` wins;
    ties are broken by the rule's position in `rules` (the earliest listed
    rule wins).
    """
    candidates = [
        (rule.priority, -index, rule)
        for index, rule in enumerate(rules)
        if _rule_matches(rule, txn)
    ]
    if not candidates:
        return None
    candidates.sort(key=lambda c: (c[0], c[1]), reverse=True)
    return candidates[0][2].category


#: A small set of sensible starting rules, roughly ordered by specificity.
DEFAULT_RULES: List[Rule] = [
    Rule(pattern="payroll", category="income", priority=10, kind="substring"),
    Rule(pattern="whole foods", category="groceries", priority=8, kind="substring"),
    Rule(pattern="trader joe", category="groceries", priority=8, kind="substring"),
    Rule(pattern="rent", category="rent", priority=6, kind="substring"),
    Rule(pattern="uber", category="transport", priority=5, kind="substring"),
    Rule(pattern="lyft", category="transport", priority=5, kind="substring"),
    Rule(pattern=r"MARKET", category="groceries", priority=5, kind="regex"),
    Rule(pattern=r"REST(AURANT)?|CAFE", category="dining", priority=4, kind="regex"),
    Rule(pattern="netflix", category="subscriptions", priority=3, kind="substring"),
]
