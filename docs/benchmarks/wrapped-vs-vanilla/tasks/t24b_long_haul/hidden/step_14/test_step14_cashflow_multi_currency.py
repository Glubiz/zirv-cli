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


class TestCashflowFunctionUsd(unittest.TestCase):
    def test_to_usd_flag_converts_with_established_rounding_rule(self):
        import datetime
        txns = [
            Transaction(1, datetime.date(2024, 1, 1), Decimal("100.00"), "Client", "pay", currency="USD"),
            Transaction(2, datetime.date(2024, 1, 2), Decimal("-22.50"), "Shop", "x",
                        category=None, currency="EUR"),
        ]
        raw = cashflow(txns, 2024, 1, 2024, 1, to_usd=False)
        self.assertEqual(raw[0][2], Decimal("22.50"))  # expense, unconverted

        usd = cashflow(txns, 2024, 1, 2024, 1, to_usd=True)
        # 22.50 EUR * 1.09 = 24.525 exactly -> ROUND_HALF_UP -> 24.53
        # (banker's rounding would give 24.52 -- discriminating case).
        self.assertEqual(usd[0][2], Decimal("24.53"))
        self.assertEqual(usd[0][1], Decimal("100.00"))  # USD income unaffected
        self.assertEqual(usd[0][3], Decimal("75.47"))  # net = 100.00 - 24.53


class TestCashflowCliUsdFlag(unittest.TestCase):
    def test_cli_usd_flag(self):
        with tempfile.TemporaryDirectory() as tmp:
            csv_path = Path(tmp) / "sample.csv"
            csv_path.write_text(
                "date,amount,payee,memo,currency\n"
                "2024-01-01,100.00,Client,pay,USD\n"
                "2024-01-02,-22.50,Shop,x,EUR\n",
                encoding="utf-8",
            )
            store = str(Path(tmp) / "ledger.json")
            _run("import", str(csv_path), "--store", store)

            r_raw = _run("report", "cashflow", "--from", "2024-01", "--to", "2024-01", "--store", store)
            self.assertIn("22.50", r_raw.stdout)

            r_usd = _run("report", "cashflow", "--from", "2024-01", "--to", "2024-01",
                          "--store", store, "--usd")
            self.assertEqual(r_usd.returncode, 0, r_usd.stderr)
            self.assertIn("24.53", r_usd.stdout)
            self.assertNotIn("22.50", r_usd.stdout)


if __name__ == "__main__":
    unittest.main()
