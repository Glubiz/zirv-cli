import contextlib
import csv
import io
import tempfile
import unittest
from datetime import date
from decimal import Decimal
from pathlib import Path

from ledgerlite import report
from ledgerlite.cli import main
from ledgerlite.models import Transaction
from ledgerlite.parse import read_csv
from ledgerlite.store import load, save


class TestCurrencyModel(unittest.TestCase):
    def test_defaults_to_dkk(self):
        txn = Transaction(id=1, date=date(2024, 1, 1), amount=Decimal("10.00"), payee="x", memo="")
        self.assertEqual(txn.currency, "DKK")

    def test_can_override_currency(self):
        txn = Transaction(id=1, date=date(2024, 1, 1), amount=Decimal("10.00"), payee="x", memo="", currency="USD")
        self.assertEqual(txn.currency, "USD")


class TestCurrencyCsv(unittest.TestCase):
    def _write_csv(self, rows, header):
        tmp = tempfile.NamedTemporaryFile(mode="w", suffix=".csv", delete=False, newline="")
        writer = csv.writer(tmp)
        writer.writerow(header)
        writer.writerows(rows)
        tmp.close()
        self.addCleanup(lambda: Path(tmp.name).unlink(missing_ok=True))
        return tmp.name

    def test_missing_currency_column_defaults_to_dkk(self):
        path = self._write_csv(
            [["2024-01-01", "-10.00", "Shop", "memo", ""]],
            header=["date", "amount", "payee", "memo", "category"],
        )
        txns = read_csv(path)
        self.assertEqual(txns[0].currency, "DKK")

    def test_currency_column_is_read(self):
        path = self._write_csv(
            [["2024-01-01", "-10.00", "Shop", "memo", "", "USD"]],
            header=["date", "amount", "payee", "memo", "category", "currency"],
        )
        txns = read_csv(path)
        self.assertEqual(txns[0].currency, "USD")

    def test_blank_currency_column_defaults_to_dkk(self):
        path = self._write_csv(
            [["2024-01-01", "-10.00", "Shop", "memo", "", ""]],
            header=["date", "amount", "payee", "memo", "category", "currency"],
        )
        txns = read_csv(path)
        self.assertEqual(txns[0].currency, "DKK")


class TestStoreRoundTrip(unittest.TestCase):
    def test_currency_round_trips(self):
        txns = [Transaction(id=1, date=date(2024, 1, 1), amount=Decimal("10.00"), payee="x", memo="", currency="EUR")]
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "ledger.json"
            save(path, txns)
            loaded = load(path)
        self.assertEqual(loaded[0].currency, "EUR")


class TestTotalsByCurrency(unittest.TestCase):
    def test_sums_grouped_by_currency(self):
        txns = [
            Transaction(id=1, date=date(2024, 1, 1), amount=Decimal("-10.00"), payee="x", memo="", currency="DKK"),
            Transaction(id=2, date=date(2024, 1, 2), amount=Decimal("-5.00"), payee="y", memo="", currency="DKK"),
            Transaction(id=3, date=date(2024, 1, 3), amount=Decimal("20.00"), payee="z", memo="", currency="USD"),
        ]
        totals = report.totals_by_currency(txns)
        self.assertEqual(totals["DKK"], Decimal("-15.00"))
        self.assertEqual(totals["USD"], Decimal("20.00"))


class TestListShowsCurrency(unittest.TestCase):
    def test_list_output_includes_currency(self):
        # Use two transactions and check the second: the ledger's unrelated
        # pagination behaviour for page 1 with a single row is out of scope
        # for this task, so this asserts on a row unaffected by that.
        with tempfile.TemporaryDirectory() as tmp:
            store_path = Path(tmp) / "ledger.json"
            txns = [
                Transaction(
                    id=1, date=date(2024, 1, 1), amount=Decimal("-10.00"),
                    payee="Domestic Shop", memo="", currency="DKK",
                ),
                Transaction(
                    id=2, date=date(2024, 1, 2), amount=Decimal("-20.00"),
                    payee="Foreign Shop", memo="", currency="USD",
                ),
            ]
            save(store_path, txns)
            buf = io.StringIO()
            with contextlib.redirect_stdout(buf):
                code = main(["list", "--store", str(store_path), "--page-size", "10"])
            self.assertEqual(code, 0)
            self.assertIn("USD", buf.getvalue())


if __name__ == "__main__":
    unittest.main()
