"""Regression coverage for the shared-aggregation refactor. Checks the one
structural fact the prompt actually pins down (a `converted_amount(txn,
to_usd)` function in `ledgerlite/report.py`), plus that
summary/cashflow/reconcile still agree on the same converted amount for
the same transaction."""

import json
import subprocess
import sys
import tempfile
import unittest
from datetime import date
from decimal import Decimal
from pathlib import Path

from ledgerlite.models import Transaction
from ledgerlite.report import converted_amount


def _run(*args):
    return subprocess.run(
        [sys.executable, "-m", "ledgerlite", *args],
        capture_output=True, text=True,
    )


class TestConvertedAmountHelper(unittest.TestCase):
    def test_converts_when_asked(self):
        txn = Transaction(1, date(2024, 1, 1), Decimal("-17.50"), "LONDON SHOP", "", currency="GBP")
        # 17.50 GBP * 1.27 = 22.225 exactly -> ROUND_HALF_UP -> 22.23.
        self.assertEqual(converted_amount(txn, to_usd=True), Decimal("-22.23"))

    def test_raw_when_not_asked(self):
        txn = Transaction(1, date(2024, 1, 1), Decimal("-17.50"), "LONDON SHOP", "", currency="GBP")
        self.assertEqual(converted_amount(txn, to_usd=False), Decimal("-17.50"))


def _seeded_store(tmp):
    # 17.50 GBP -> 22.225 exactly -> ROUND_HALF_UP -> 22.23 (banker's
    # rounding would give 22.22): reused across all three commands below
    # to prove they all still apply the exact same conversion rule after
    # being pointed at a shared helper.
    store = Path(tmp) / "ledger.json"
    store.write_text(json.dumps({"transactions": [
        {"id": 1, "date": "2024-01-01", "amount": "-17.50", "payee": "LONDON SHOP",
         "memo": "", "category": "shopping", "currency": "GBP"},
    ]}), encoding="utf-8")
    return str(store)


class TestSharedConversionAgreement(unittest.TestCase):
    def test_summary_cashflow_reconcile_agree_on_usd_conversion(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = _seeded_store(tmp)

            r_summary = _run("summary", "--store", store)
            self.assertEqual(r_summary.returncode, 0, r_summary.stderr)
            shopping_line = next(ln for ln in r_summary.stdout.splitlines() if ln.startswith("shopping"))
            self.assertIn("22.23", shopping_line)

            r_cashflow = _run("report", "cashflow", "--from", "2024-01", "--to", "2024-01",
                               "--store", store, "--usd")
            self.assertEqual(r_cashflow.returncode, 0, r_cashflow.stderr)
            self.assertIn("22.23", r_cashflow.stdout)

            _run("reconcile", "mark", "1", "--store", store)
            r_reconcile = _run("reconcile", "status", "--statement-balance", "-22.23",
                                "--store", store, "--usd")
            self.assertEqual(r_reconcile.returncode, 0, r_reconcile.stderr)
            self.assertIn("Status: reconciled", r_reconcile.stdout)

    def test_raw_currency_paths_still_unconverted(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = _seeded_store(tmp)

            r_summary = _run("summary", "--raw-currency", "--store", store)
            self.assertIn("17.50", r_summary.stdout)

            r_cashflow = _run("report", "cashflow", "--from", "2024-01", "--to", "2024-01", "--store", store)
            self.assertIn("17.50", r_cashflow.stdout)

            _run("reconcile", "mark", "1", "--store", store)
            r_reconcile = _run("reconcile", "status", "--statement-balance", "-17.50", "--store", store)
            self.assertIn("Status: reconciled", r_reconcile.stdout)


if __name__ == "__main__":
    unittest.main()
