import tempfile
import unittest
from datetime import date
from decimal import Decimal
from pathlib import Path

from ledgerlite.models import Transaction
from ledgerlite.store import load, save


class TestStore(unittest.TestCase):
    def test_round_trip(self):
        txns = [
            Transaction(id=1, date=date(2024, 1, 1), amount=Decimal("-12.34"), payee="Coffee", memo="latte", category="dining"),
            Transaction(id=2, date=date(2024, 1, 2), amount=Decimal("100.00"), payee="Employer", memo="", category=None),
        ]
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "ledger.json"
            save(path, txns)
            loaded = load(path)
        self.assertEqual(len(loaded), 2)
        self.assertEqual(loaded[0].amount, Decimal("-12.34"))
        self.assertEqual(loaded[0].category, "dining")
        self.assertIsNone(loaded[1].category)
        self.assertEqual(loaded[1].date, date(2024, 1, 2))

    def test_load_missing_transactions_key_is_empty(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "ledger.json"
            path.write_text("{}", encoding="utf-8")
            self.assertEqual(load(path), [])


if __name__ == "__main__":
    unittest.main()
