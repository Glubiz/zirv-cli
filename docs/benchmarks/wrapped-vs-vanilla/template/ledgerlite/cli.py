"""Command-line interface for ledgerlite: `python -m ledgerlite <command>`."""

from __future__ import annotations

import argparse
from decimal import Decimal
from typing import Optional, Sequence

from . import report
from .parse import read_csv
from .rules import DEFAULT_RULES, categorize
from .store import load, save

DEFAULT_STORE = "ledger.json"


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="ledgerlite", description="A tiny personal-finance ledger."
    )
    sub = parser.add_subparsers(dest="command", required=True)

    p_import = sub.add_parser("import", help="import transactions from a CSV file")
    p_import.add_argument("csv_path", help="path to a bank-export CSV file")
    p_import.add_argument(
        "--store", default=DEFAULT_STORE, help="ledger JSON file to write (default: %(default)s)"
    )

    p_list = sub.add_parser("list", help="list stored transactions")
    p_list.add_argument(
        "--store", default=DEFAULT_STORE, help="ledger JSON file to read (default: %(default)s)"
    )
    p_list.add_argument("--page", type=int, default=1, help="page number, 1-indexed (default: %(default)s)")
    p_list.add_argument(
        "--page-size", type=int, default=10, dest="page_size", help="rows per page (default: %(default)s)"
    )

    p_summary = sub.add_parser("summary", help="show per-category totals")
    p_summary.add_argument(
        "--store", default=DEFAULT_STORE, help="ledger JSON file to read (default: %(default)s)"
    )

    p_categorize = sub.add_parser(
        "categorize", help="(re)apply the default rules to stored transactions"
    )
    p_categorize.add_argument(
        "--store", default=DEFAULT_STORE, help="ledger JSON file to update (default: %(default)s)"
    )

    return parser


def _cmd_import(args: argparse.Namespace) -> int:
    txns = read_csv(args.csv_path)
    for txn in txns:
        if txn.category is None:
            txn.category = categorize(txn, DEFAULT_RULES)
    save(args.store, txns)
    print(f"Imported {len(txns)} transactions into {args.store}")
    return 0


def _cmd_list(args: argparse.Namespace) -> int:
    txns = load(args.store)
    rows = report.page(txns, args.page, args.page_size)
    if not rows:
        print("No transactions on this page.")
        return 0
    print(f"{'ID':>4}  {'Date':<10}  {'Amount':>10}  {'Payee':<24}  {'Category':<14}  Memo")
    for txn in rows:
        print(
            f"{txn.id:>4}  {txn.date.isoformat():<10}  {txn.amount:>10}  "
            f"{txn.payee:<24}  {(txn.category or ''):<14}  {txn.memo}"
        )
    return 0


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


def _cmd_categorize(args: argparse.Namespace) -> int:
    txns = load(args.store)
    changed = 0
    for txn in txns:
        new_category = categorize(txn, DEFAULT_RULES)
        if new_category != txn.category:
            txn.category = new_category
            changed += 1
    save(args.store, txns)
    print(f"Re-categorized {changed} of {len(txns)} transactions")
    return 0


_HANDLERS = {
    "import": _cmd_import,
    "list": _cmd_list,
    "summary": _cmd_summary,
    "categorize": _cmd_categorize,
}


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(argv)
    handler = _HANDLERS[args.command]
    return handler(args)


if __name__ == "__main__":
    raise SystemExit(main())
