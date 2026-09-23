"""CSV import for ledgerlite."""

from __future__ import annotations

import csv
from datetime import date, datetime
from pathlib import Path
from typing import List, Union

from .models import Transaction, parse_money

#: Date formats accepted in the `date` column, tried in order.
_DATE_FORMATS = ("%Y-%m-%d", "%d-%m-%Y")


def _parse_date(text: str) -> date:
    text = text.strip()
    for fmt in _DATE_FORMATS:
        try:
            return datetime.strptime(text, fmt).date()
        except ValueError:
            continue
    raise ValueError(f"unrecognised date format: {text!r}")


def read_csv(path: Union[str, Path]) -> List[Transaction]:
    """Read a bank-export CSV into a list of `Transaction` records.

    The file must have a header row with at least `date`, `amount`, `payee`,
    and `memo` columns; `category` is optional. Dates may be ISO
    (`YYYY-MM-DD`) or `DD-MM-YYYY`. Amounts are parsed with `parse_money`,
    so a leading `$` or parenthesized negatives are accepted.

    Transaction ids are assigned sequentially starting at 1, in file order.
    """
    path = Path(path)
    txns: List[Transaction] = []
    with path.open("r", newline="", encoding="utf-8") as fh:
        reader = csv.DictReader(fh)
        for i, row in enumerate(reader, start=1):
            category = (row.get("category") or "").strip() or None
            txns.append(
                Transaction(
                    id=i,
                    date=_parse_date(row["date"]),
                    amount=parse_money(row["amount"]),
                    payee=(row.get("payee") or "").strip(),
                    memo=(row.get("memo") or "").strip(),
                    category=category,
                )
            )
    return txns
