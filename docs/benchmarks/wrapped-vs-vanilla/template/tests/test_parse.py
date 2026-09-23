import csv
import tempfile
import unittest
from decimal import Decimal
from pathlib import Path

from ledgerlite.parse import read_csv


class TestReadCsv(unittest.TestCase):
    def _write_csv(self, rows, header=("date", "amount", "payee", "memo", "category")):
        tmp = tempfile.NamedTemporaryFile(mode="w", suffix=".csv", delete=False, newline="")
        writer = csv.writer(tmp)
        writer.writerow(header)
        writer.writerows(rows)
        tmp.close()
        self.addCleanup(lambda: Path(tmp.name).unlink(missing_ok=True))
        return tmp.name

    def test_iso_dates(self):
        path = self._write_csv([["2024-01-15", "-12.34", "Coffee Shop", "latte", ""]])
        txns = read_csv(path)
        self.assertEqual(len(txns), 1)
        self.assertEqual(txns[0].date.isoformat(), "2024-01-15")
        self.assertEqual(txns[0].amount, Decimal("-12.34"))

    def test_ddmmyyyy_dates(self):
        path = self._write_csv([["15-01-2024", "20.00", "Employer", "payroll", "income"]])
        txns = read_csv(path)
        self.assertEqual(txns[0].date.isoformat(), "2024-01-15")
        self.assertEqual(txns[0].category, "income")

    def test_missing_category_is_none(self):
        path = self._write_csv([["2024-02-01", "-5.00", "Cafe", "snack", ""]])
        txns = read_csv(path)
        self.assertIsNone(txns[0].category)

    def test_ids_are_sequential(self):
        path = self._write_csv(
            [
                ["2024-01-01", "-1.00", "A", "", ""],
                ["2024-01-02", "-2.00", "B", "", ""],
            ]
        )
        txns = read_csv(path)
        self.assertEqual([t.id for t in txns], [1, 2])


if __name__ == "__main__":
    unittest.main()
