# Rubric: t05_dedupe

The task prompt asked the agent to remove the duplicated per-category
total/percent-share logic in `ledgerlite/cli.py`'s `summary` command by
delegating to `ledgerlite/report.py` (`category_totals` and/or
`summarize`), without changing the command's printed output.

Score 0-10. Start from 10 and deduct for each issue found in the diff:

- **-10 (score 0), correctness/output change**: the `summary` command's
  printed output changed in any way (different totals, different percent
  values or rounding, different column widths, different row order,
  different wording) for the same input data. Output must be byte-identical
  to before.
- **-6, duplication not actually removed**: `cli.py` still recomputes
  category totals and/or the percent-share formula itself (e.g. still loops
  over transactions accumulating a dict, or still divides by a manually
  computed magnitude sum) instead of calling into `report.py`.
- **-4, dead code left behind**: the old inline computation is left in the
  file (commented out, or as an unused helper) alongside the new delegating
  code.
- **-3, unrelated edits**: changes to files or behavior outside the scope
  of this de-duplication (e.g. touching `import`, `list`, or `categorize`
  commands, changing CLI flags, editing tests).
- **-10 (score 0), broken tests**: any previously-passing visible test now
  fails, or the CLI crashes on the sample workflow (import then summary).
  (Note: `tests/test_rules.py::test_regex_rule_case_insensitive` is a
  pre-existing, unrelated failure in this repo and should NOT be counted
  against the agent.)
- **-2, poor craftsmanship**: the refactor is technically correct but
  awkward -- e.g. it calls `report.summarize()` but then re-derives the
  same percent values again instead of using the returned dict, or it
  duplicates report.py's logic under a new name in cli.py.

A score of 10 requires: `cli.py`'s `summary` command calls
`report.category_totals()` and/or `report.summarize()` for the totals and
percent-share numbers, the printed output is unchanged, no dead code
remains, no unrelated files changed, and all previously-passing tests still
pass.

For reference, `ledgerlite/report.py` (as of the task's starting point)
defines:

```python
def category_totals(txns: List[Transaction]) -> Dict[str, Decimal]:
    """Sum transaction amounts per category.

    Uncategorized transactions (`category is None`) are grouped under the
    key `"uncategorized"`.
    """
    totals: Dict[str, Decimal] = {}
    for txn in txns:
        key = txn.category or UNCATEGORIZED
        totals[key] = totals.get(key, Decimal("0")) + txn.amount
    return totals


def summarize(txns: List[Transaction]) -> dict:
    """Return an overview of `txns`: count, net amount, and per-category share.

    `by_category` maps each category (or `"uncategorized"`) to a dict with
    `total` (signed `Decimal`) and `percent` (share of total absolute
    spending across all categories, as a `Decimal` from 0 to 100).
    """
    net = sum((txn.amount for txn in txns), Decimal("0"))
    totals = category_totals(txns)
    magnitude_sum = sum((abs(v) for v in totals.values()), Decimal("0"))

    by_category = {}
    for category, amount in totals.items():
        percent = (abs(amount) / magnitude_sum * 100) if magnitude_sum else Decimal("0")
        by_category[category] = {"total": amount, "percent": percent}

    return {"count": len(txns), "net": net, "by_category": by_category}
```

And the starting point's `cli.py` `_cmd_summary` (the code to be replaced):

```python
def _cmd_summary(args: argparse.Namespace) -> int:
    txns = load(args.store)

    # NOTE: this re-derives per-category totals and percent share instead of
    # calling report.category_totals()/report.summarize() -- see D5.
    totals: dict = {}
    for txn in txns:
        key = txn.category or report.UNCATEGORIZED
        totals[key] = totals.get(key, Decimal("0")) + txn.amount
    magnitude_sum = sum((abs(v) for v in totals.values()), Decimal("0"))

    print(f"{'Category':<14}  {'Total':>10}  {'Share':>7}")
    for category, amount in sorted(totals.items()):
        percent = (abs(amount) / magnitude_sum * 100) if magnitude_sum else Decimal("0")
        print(f"{category:<14}  {amount:>10}  {percent:>6.1f}%")
    return 0
```

Answer with only a JSON object: `{"score": 0-10, "reasoning": "..."}`.
