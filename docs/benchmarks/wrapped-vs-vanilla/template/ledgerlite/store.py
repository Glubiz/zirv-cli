"""JSON persistence for ledgerlite transactions."""

from __future__ import annotations

import json
from datetime import date
from decimal import Decimal
from pathlib import Path
from typing import List, Union

from .models import Transaction


def _txn_to_dict(txn: Transaction) -> dict:
    return {
        "id": txn.id,
        "date": txn.date.isoformat(),
        "amount": str(txn.amount),
        "payee": txn.payee,
        "memo": txn.memo,
        "category": txn.category,
    }


def _txn_from_dict(data: dict) -> Transaction:
    return Transaction(
        id=data["id"],
        date=date.fromisoformat(data["date"]),
        amount=Decimal(data["amount"]),
        payee=data["payee"],
        memo=data["memo"],
        category=data.get("category"),
    )


def save(path: Union[str, Path], txns: List[Transaction]) -> None:
    """Write `txns` to `path` as JSON, overwriting any existing file."""
    payload = {"transactions": [_txn_to_dict(t) for t in txns]}
    Path(path).write_text(json.dumps(payload, indent=2), encoding="utf-8")


def load(path: Union[str, Path]) -> List[Transaction]:
    """Read a list of `Transaction` from a JSON file written by `save`."""
    payload = json.loads(Path(path).read_text(encoding="utf-8"))
    return [_txn_from_dict(d) for d in payload.get("transactions", [])]
