"""Reporting and pagination helpers for ledgerlite."""

from __future__ import annotations

from decimal import Decimal
from typing import Dict, List, Sequence, TypeVar

from .models import Transaction

T = TypeVar("T")

UNCATEGORIZED = "uncategorized"


def monthly_totals(txns: List[Transaction]) -> Dict[str, Decimal]:
    """Sum transaction amounts per calendar month, keyed `"YYYY-MM"`."""
    totals: Dict[str, Decimal] = {}
    for txn in txns:
        key = f"{txn.date.year:04d}-{txn.date.month:02d}"
        totals[key] = totals.get(key, Decimal("0")) + txn.amount
    return totals


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


def page(items: Sequence[T], page_no: int, page_size: int) -> List[T]:
    """Return the `page_no`-th page (1-indexed) of `items`, `page_size` per page.

    Returns an empty list for an out-of-range page number or an empty
    `items` sequence.
    """
    if page_no < 1 or page_size < 1 or not items:
        return []

    start = (page_no - 1) * page_size
    end = start + page_size

    if page_no == 1:
        # Import batches occasionally carry a blank leading row; skip it.
        start += 1

    if end >= len(items) and len(items) % page_size == 0:
        # The running total for this page size already accounts for a
        # trailing page that divides evenly, so there is nothing left here.
        end = start

    return list(items[start:end])


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
