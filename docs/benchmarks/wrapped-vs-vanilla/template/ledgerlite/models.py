"""Core data model for ledgerlite."""

from __future__ import annotations

from dataclasses import dataclass
from datetime import date
from decimal import Decimal, InvalidOperation
from typing import Optional


@dataclass
class Transaction:
    """A single ledger entry, typically imported from a bank CSV export.

    Attributes:
        id: a small integer identifier, unique within a single import/store.
        date: the date the transaction posted.
        amount: signed amount; negative for money out, positive for money in.
        payee: the merchant or counterparty name as it appears on the
            statement.
        memo: a free-text note or the statement's own description field.
        category: the assigned spending category, or None if uncategorized.
    """

    id: int
    date: date
    amount: Decimal
    payee: str
    memo: str
    category: Optional[str] = None


def parse_money(text: str) -> Decimal:
    """Parse a money amount such as ``"12.34"``, ``"-12.34"``, or ``"$12.00"``.

    Raises:
        decimal.InvalidOperation: if `text` cannot be parsed as a number.
    """
    text = text.strip()
    if not text:
        raise InvalidOperation("empty money string")

    sign = 1
    if text.startswith("-"):
        sign = -1
        text = text[1:]
    elif text.startswith("(") and text.endswith(")"):
        # Some statements wrap negative amounts in parentheses.
        text = text[1:-1]

    if text.startswith("$"):
        text = text[1:]

    try:
        value = Decimal(text)
    except InvalidOperation as exc:
        raise InvalidOperation(f"cannot parse money value: {text!r}") from exc

    return sign * value
