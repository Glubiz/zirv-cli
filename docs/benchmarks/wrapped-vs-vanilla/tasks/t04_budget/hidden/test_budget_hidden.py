import contextlib
import io
import json
import tempfile
import unittest
from datetime import date
from decimal import Decimal
from pathlib import Path

from ledgerlite.budget import Budget, overspend
from ledgerlite.cli import main
from ledgerlite.models import Transaction


def _txn(i, amount, category):
    return Transaction(id=i, date=date(2024, 1, 1), amount=Decimal(amount), payee="x", memo="", category=category)


class TestBudgetModule(unittest.TestCase):
    def test_overspend_reports_only_categories_over_limit(self):
        txns = [
            _txn(1, "-120.00", "groceries"),
            _txn(2, "-40.00", "dining"),
            _txn(3, "50.00", "income"),
        ]
        budgets = [
            Budget(category="groceries", limit=Decimal("100.00")),
            Budget(category="dining", limit=Decimal("100.00")),
        ]
        self.assertEqual(overspend(txns, budgets), {"groceries": Decimal("20.00")})

    def test_overspend_ignores_categories_without_a_budget(self):
        txns = [_txn(1, "-500.00", "rent")]
        budgets = [Budget(category="groceries", limit=Decimal("10.00"))]
        self.assertEqual(overspend(txns, budgets), {})

    def test_overspend_empty_when_under_budget(self):
        txns = [_txn(1, "-10.00", "groceries")]
        budgets = [Budget(category="groceries", limit=Decimal("100.00"))]
        self.assertEqual(overspend(txns, budgets), {})


class TestBudgetCli(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = Path(self.tmpdir.name) / "ledger.json"
        data = {
            "transactions": [
                {
                    "id": 1,
                    "date": "2024-01-01",
                    "amount": "-120.00",
                    "payee": "Whole Foods",
                    "memo": "",
                    "category": "groceries",
                },
                {
                    "id": 2,
                    "date": "2024-01-02",
                    "amount": "-10.00",
                    "payee": "Cafe",
                    "memo": "",
                    "category": "dining",
                },
            ]
        }
        self.store_path.write_text(json.dumps(data), encoding="utf-8")

    def _run(self, args):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = main(args)
        return code, buf.getvalue()

    def test_set_then_report_over_budget(self):
        code, _ = self._run(["budget", "--set", "groceries=100.00", "--store", str(self.store_path)])
        self.assertEqual(code, 0)
        code, out = self._run(["budget", "--report", "--store", str(self.store_path)])
        self.assertEqual(code, 0)
        self.assertIn("groceries", out)
        self.assertIn("20.00", out)

    def test_report_omits_categories_within_budget(self):
        self._run(["budget", "--set", "dining=100.00", "--store", str(self.store_path)])
        code, out = self._run(["budget", "--report", "--store", str(self.store_path)])
        self.assertEqual(code, 0)
        self.assertNotIn("dining", out)

    def test_budgets_persist_alongside_transactions(self):
        self._run(["budget", "--set", "groceries=100.00", "--store", str(self.store_path)])
        raw = json.loads(self.store_path.read_text(encoding="utf-8"))
        self.assertIn("budgets", raw)
        self.assertEqual(len(raw.get("transactions", [])), 2)


if __name__ == "__main__":
    unittest.main()
