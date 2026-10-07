"""Shared helpers for t27_tax_season hidden tests (copied into every step dir)."""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


def run(*args):
    return subprocess.run(
        [sys.executable, "-m", "ledgerlite", *args],
        capture_output=True, text=True,
    )


def txn(id, date, amount, payee, memo="", category=None, **extra):
    d = {"id": id, "date": date, "amount": amount, "payee": payee,
         "memo": memo, "category": category}
    d.update(extra)
    return d


class StoreCase(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.dir = Path(self._tmp.name)
        self.store = str(self.dir / "ledger.json")

    def write_store(self, txns, **extra):
        payload = {"transactions": txns}
        payload.update(extra)
        Path(self.store).write_text(json.dumps(payload), encoding="utf-8")

    def read_store(self):
        return json.loads(Path(self.store).read_text(encoding="utf-8"))

    def write_csv(self, rows, name="in.csv"):
        """rows: list of (date, amount, payee, memo[, category]) tuples."""
        lines = ["date,amount,payee,memo,category"]
        for r in rows:
            r = list(r) + [""] * (5 - len(r))
            lines.append(",".join(f'"{c}"' for c in r))
        path = self.dir / name
        path.write_text("\n".join(lines) + "\n", encoding="utf-8")
        return str(path)

    def cli(self, *args):
        return run(*args, "--store", self.store)

    def by_id(self, id):
        for t in self.read_store()["transactions"]:
            if t["id"] == id:
                return t
        self.fail(f"no transaction {id} in store")
