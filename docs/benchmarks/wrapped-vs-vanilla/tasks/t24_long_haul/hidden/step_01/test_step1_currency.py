import json
import subprocess
import sys
import tempfile
import unittest
from decimal import Decimal
from pathlib import Path

from ledgerlite.currency import RATES, convert, to_usd, from_usd


class TestCurrencyModule(unittest.TestCase):
    def test_usd_identity(self):
        self.assertEqual(convert(Decimal("12.34"), "USD", "USD"), Decimal("12.34"))

    def test_to_usd_eur(self):
        self.assertEqual(to_usd(Decimal("10"), "EUR"), Decimal("10.90"))

    def test_from_usd_eur(self):
        # 100 / 1.09 = 91.7431...  -> rounds to 91.74 under any reasonable rule
        self.assertEqual(from_usd(Decimal("100"), "EUR"), Decimal("91.74"))

    def test_convert_via_usd(self):
        self.assertEqual(convert(Decimal("10"), "EUR", "USD"), Decimal("10.90"))

    def test_rates_values(self):
        self.assertEqual(RATES["EUR"], Decimal("1.09"))
        self.assertEqual(RATES["GBP"], Decimal("1.27"))

    def test_unsupported_currency_raises(self):
        with self.assertRaises(Exception):
            convert(Decimal("1"), "CAD", "USD")


class TestFxCli(unittest.TestCase):
    def _run(self, *args):
        return subprocess.run(
            [sys.executable, "-m", "ledgerlite", *args],
            capture_output=True, text=True,
        )

    def test_fx_convert_cli(self):
        r = self._run("fx", "convert", "100", "USD", "EUR")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "91.74")


class TestImportCurrency(unittest.TestCase):
    def _run(self, *args):
        return subprocess.run(
            [sys.executable, "-m", "ledgerlite", *args],
            capture_output=True, text=True,
        )

    def test_currency_column_used(self):
        with tempfile.TemporaryDirectory() as tmp:
            csv_path = Path(tmp) / "sample.csv"
            csv_path.write_text(
                "date,amount,payee,memo,currency\n"
                "2024-01-01,-10.00,CAFE PARIS,lunch,EUR\n",
                encoding="utf-8",
            )
            store = str(Path(tmp) / "ledger.json")
            r = self._run("import", str(csv_path), "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            data = json.loads(Path(store).read_text(encoding="utf-8"))
            self.assertEqual(data["transactions"][0]["currency"], "EUR")

    def test_default_currency_flag(self):
        with tempfile.TemporaryDirectory() as tmp:
            csv_path = Path(tmp) / "sample.csv"
            csv_path.write_text(
                "date,amount,payee,memo\n"
                "2024-01-01,-10.00,LONDON SHOP,gift\n",
                encoding="utf-8",
            )
            store = str(Path(tmp) / "ledger.json")
            r = self._run("import", str(csv_path), "--store", store, "--currency", "GBP")
            self.assertEqual(r.returncode, 0, r.stderr)
            data = json.loads(Path(store).read_text(encoding="utf-8"))
            self.assertEqual(data["transactions"][0]["currency"], "GBP")

    def test_currency_defaults_to_usd(self):
        with tempfile.TemporaryDirectory() as tmp:
            csv_path = Path(tmp) / "sample.csv"
            csv_path.write_text(
                "date,amount,payee,memo\n"
                "2024-01-01,-10.00,LOCAL SHOP,thing\n",
                encoding="utf-8",
            )
            store = str(Path(tmp) / "ledger.json")
            self._run("import", str(csv_path), "--store", store)
            data = json.loads(Path(store).read_text(encoding="utf-8"))
            self.assertEqual(data["transactions"][0]["currency"], "USD")

    def test_backward_compat_missing_currency_key(self):
        # Uses `categorize` (load -> save round trip), not `list`, so this
        # check stays valid even after a later session step changes
        # `list`'s default ordering -- it only cares that a pre-currency
        # store loads without crashing and gets "currency" backfilled.
        with tempfile.TemporaryDirectory() as tmp:
            store = Path(tmp) / "ledger.json"
            store.write_text(json.dumps({"transactions": [
                {"id": 1, "date": "2024-01-02", "amount": "-5.00", "payee": "OLD ROW",
                 "memo": "pre-currency", "category": None},
            ]}), encoding="utf-8")
            r = self._run("categorize", "--store", str(store))
            self.assertEqual(r.returncode, 0, r.stderr)
            data = json.loads(store.read_text(encoding="utf-8"))
            self.assertEqual(data["transactions"][0]["payee"], "OLD ROW")
            self.assertEqual(data["transactions"][0]["currency"], "USD")


if __name__ == "__main__":
    unittest.main()
