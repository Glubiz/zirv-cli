import contextlib
import io
import tempfile
import unittest
from pathlib import Path

from ledgerlite.cli import main

SAMPLE_CSV = """date,amount,payee,memo,category
2024-01-05,-45.20,Whole Foods,groceries run,
2024-01-06,-12.00,Blue Bottle Coffee,latte,
2024-01-07,2500.00,Employer Payroll,salary,
2024-01-08,-9.50,Blue Bottle Coffee,latte,
2024-01-09,-800.00,Landlord Rent,rent,
"""


class TestCli(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.csv_path = Path(self.tmpdir.name) / "sample.csv"
        self.csv_path.write_text(SAMPLE_CSV, encoding="utf-8")
        self.store_path = Path(self.tmpdir.name) / "ledger.json"

    def _run(self, args):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = main(args)
        return code, buf.getvalue()

    def test_import_reports_count(self):
        code, out = self._run(["import", str(self.csv_path), "--store", str(self.store_path)])
        self.assertEqual(code, 0)
        self.assertIn("Imported 5 transactions", out)
        self.assertTrue(self.store_path.exists())

    def test_summary_shows_categories_and_share(self):
        self._run(["import", str(self.csv_path), "--store", str(self.store_path)])
        code, out = self._run(["summary", "--store", str(self.store_path)])
        self.assertEqual(code, 0)
        self.assertIn("income", out)
        self.assertIn("%", out)

    def test_categorize_reports_changes(self):
        self._run(["import", str(self.csv_path), "--store", str(self.store_path)])
        code, out = self._run(["categorize", "--store", str(self.store_path)])
        self.assertEqual(code, 0)
        self.assertIn("Re-categorized", out)

    def test_list_middle_page(self):
        self._run(["import", str(self.csv_path), "--store", str(self.store_path)])
        code, out = self._run(["list", "--store", str(self.store_path), "--page", "2", "--page-size", "2"])
        self.assertEqual(code, 0)
        self.assertIn("Blue Bottle Coffee", out)

    def test_list_empty_store_reports_no_rows(self):
        save_empty = Path(self.tmpdir.name) / "empty.json"
        save_empty.write_text('{"transactions": []}', encoding="utf-8")
        code, out = self._run(["list", "--store", str(save_empty)])
        self.assertEqual(code, 0)
        self.assertIn("No transactions", out)


if __name__ == "__main__":
    unittest.main()
