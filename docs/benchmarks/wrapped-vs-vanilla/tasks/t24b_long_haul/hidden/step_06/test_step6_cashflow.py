import subprocess
import sys
import tempfile
import unittest
from decimal import Decimal
from pathlib import Path

from ledgerlite.models import Transaction
from ledgerlite.report import cashflow


def _run(*args):
    return subprocess.run(
        [sys.executable, "-m", "ledgerlite", *args],
        capture_output=True, text=True,
    )


class TestCashflowFunction(unittest.TestCase):
    def test_basic_month_totals(self):
        txns = [
            Transaction(1, __import__("datetime").date(2024, 1, 5), Decimal("2500.00"), "Payroll", "pay"),
            Transaction(2, __import__("datetime").date(2024, 1, 10), Decimal("-915.00"), "Shop", "stuff"),
        ]
        rows = cashflow(txns, 2024, 1, 2024, 2)
        self.assertEqual(rows[0], ("2024-01", Decimal("2500.00"), Decimal("915.00"), Decimal("1585.00")))
        # February has no activity but must still be present, all zero.
        self.assertEqual(rows[1], ("2024-02", Decimal("0"), Decimal("0"), Decimal("0")))

    def test_empty_months_included_for_empty_ledger(self):
        rows = cashflow([], 2024, 1, 2024, 3)
        self.assertEqual([r[0] for r in rows], ["2024-01", "2024-02", "2024-03"])


class TestCashflowCli(unittest.TestCase):
    def test_cli_prints_expected_rows(self):
        with tempfile.TemporaryDirectory() as tmp:
            csv_path = Path(tmp) / "sample.csv"
            csv_path.write_text(
                "date,amount,payee,memo,category\n"
                "2024-01-01,-1.00,DUMMY,n/a,other\n"
                "2024-01-05,2500.00,PAYROLL,pay,income\n"
                "2024-01-10,-915.00,BIGSHOP,stuff,other\n",
                encoding="utf-8",
            )
            store = str(Path(tmp) / "ledger.json")
            _run("import", str(csv_path), "--store", store)
            r = _run("report", "cashflow", "--from", "2024-01", "--to", "2024-02", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            lines = r.stdout.splitlines()
            self.assertIn("Month", lines[0])
            self.assertIn("Income", lines[0])
            self.assertIn("Expense", lines[0])
            self.assertIn("Net", lines[0])
            jan_line = next(ln for ln in lines if ln.startswith("2024-01"))
            self.assertIn("2500.00", jan_line)
            self.assertIn("916.00", jan_line)
            self.assertIn("1584.00", jan_line)
            feb_line = next(ln for ln in lines if ln.startswith("2024-02"))
            self.assertIn("0.00", feb_line)


if __name__ == "__main__":
    unittest.main()
