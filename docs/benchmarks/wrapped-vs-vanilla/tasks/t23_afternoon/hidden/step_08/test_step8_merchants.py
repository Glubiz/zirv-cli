import subprocess
import sys
import tempfile
import unittest
from datetime import date
from decimal import Decimal
from pathlib import Path

from ledgerlite.models import Transaction
from ledgerlite.report import top_merchants


def _txn(id_, d, amount, payee):
    return Transaction(id=id_, date=d, amount=Decimal(amount), payee=payee, memo="", category=None)


class TestTopMerchants(unittest.TestCase):
    def test_sums_by_canonical_name_descending(self):
        txns = [
            _txn(1, date(2024, 3, 1), "-10.00", "WHOLE FOODS MKT"),
            _txn(2, date(2024, 3, 2), "-5.00", "wholefoods.com"),
            _txn(3, date(2024, 3, 3), "-20.00", "STARBUCKS"),
        ]
        aliases = [("WHOLE FOODS", "Whole Foods"), ("wholefoods.com", "Whole Foods")]
        result = top_merchants(txns, aliases, 2024, 3)
        self.assertEqual(result, [("STARBUCKS", Decimal("20.00")), ("Whole Foods", Decimal("15.00"))])

    def test_filters_to_month(self):
        txns = [
            _txn(1, date(2024, 3, 1), "-10.00", "A"),
            _txn(2, date(2024, 4, 1), "-99.00", "A"),
        ]
        result = top_merchants(txns, [], 2024, 3)
        self.assertEqual(result, [("A", Decimal("10.00"))])

    def test_tie_break_alphabetical(self):
        txns = [
            _txn(1, date(2024, 3, 1), "-10.00", "ZEBRA"),
            _txn(2, date(2024, 3, 1), "-10.00", "APPLE"),
        ]
        result = top_merchants(txns, [], 2024, 3)
        self.assertEqual(result, [("APPLE", Decimal("10.00")), ("ZEBRA", Decimal("10.00"))])

    def test_limit_caps_result(self):
        txns = [
            _txn(1, date(2024, 3, 1), "-30.00", "A"),
            _txn(2, date(2024, 3, 1), "-20.00", "B"),
            _txn(3, date(2024, 3, 1), "-10.00", "C"),
        ]
        result = top_merchants(txns, [], 2024, 3, limit=2)
        self.assertEqual(result, [("A", Decimal("30.00")), ("B", Decimal("20.00"))])

    def test_no_transactions_that_month(self):
        self.assertEqual(top_merchants([], [], 2024, 3), [])


class TestMerchantsCli(unittest.TestCase):
    def _run(self, *args):
        return subprocess.run(
            [sys.executable, "-m", "ledgerlite", *args],
            capture_output=True, text=True,
        )

    def test_cli_prints_ranked_lines(self):
        with tempfile.TemporaryDirectory() as tmp:
            csv_path = Path(tmp) / "sample.csv"
            csv_path.write_text(
                "date,amount,payee,memo\n"
                "2024-03-01,-10.00,WHOLE FOODS MKT,groceries\n"
                "2024-03-02,-20.00,STARBUCKS,coffee\n",
                encoding="utf-8",
            )
            store = str(Path(tmp) / "ledger.json")
            self.assertEqual(self._run("import", str(csv_path), "--store", store).returncode, 0)
            self._run("alias", "add", "--pattern", "WHOLE FOODS", "--canonical", "Whole Foods", "--store", store)
            r = self._run("merchants", "--year", "2024", "--month", "3", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            lines = [ln for ln in r.stdout.splitlines() if ln.strip()]
            self.assertEqual(lines, ["STARBUCKS: 20.00", "Whole Foods: 10.00"])

    def test_cli_no_transactions_message(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            r = self._run("merchants", "--year", "2024", "--month", "3", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual(r.stdout.strip(), "No transactions for 2024-03.")

    def test_cli_limit(self):
        with tempfile.TemporaryDirectory() as tmp:
            csv_path = Path(tmp) / "sample.csv"
            csv_path.write_text(
                "date,amount,payee,memo\n"
                "2024-03-01,-10.00,A,x\n"
                "2024-03-02,-20.00,B,x\n"
                "2024-03-03,-5.00,C,x\n",
                encoding="utf-8",
            )
            store = str(Path(tmp) / "ledger.json")
            self._run("import", str(csv_path), "--store", store)
            r = self._run("merchants", "--year", "2024", "--month", "3", "--limit", "1", "--store", store)
            lines = [ln for ln in r.stdout.splitlines() if ln.strip()]
            self.assertEqual(lines, ["B: 20.00"])


if __name__ == "__main__":
    unittest.main()
