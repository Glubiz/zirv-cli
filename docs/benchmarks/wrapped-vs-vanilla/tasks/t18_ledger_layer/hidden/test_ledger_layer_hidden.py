import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path

from ledgerlite.cli import main
from ledgerlite.ledger import Ledger
from ledgerlite.rules import Rule


def _run(args):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = main(args)
    return code, out.getvalue(), err.getvalue()


class TestLedgerImportCsv(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")
        self.csv_path = str(Path(self.tmpdir.name) / "in.csv")

    def _write_csv(self, text):
        Path(self.csv_path).write_text(text, encoding="utf-8")

    def test_import_csv_returns_count_and_persists(self):
        self._write_csv(
            "date,amount,payee,memo\n"
            "2024-01-01,-5.00,Whole Foods,Groceries\n"
            "2024-01-02,10.00,Payroll Inc,Salary\n"
        )
        ledger = Ledger(self.store_path)
        count = ledger.import_csv(self.csv_path)
        self.assertEqual(count, 2)
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertEqual(len(raw["transactions"]), 2)

    def test_import_csv_applies_default_rules_when_uncategorized(self):
        self._write_csv("date,amount,payee,memo\n2024-01-01,-5.00,Whole Foods,\n")
        ledger = Ledger(self.store_path)
        ledger.import_csv(self.csv_path)
        loaded = ledger.load()
        self.assertEqual(loaded[0].category, "groceries")

    def test_import_csv_preserves_explicit_csv_category(self):
        self._write_csv("date,amount,payee,memo,category\n2024-01-01,-5.00,Whole Foods,,custom\n")
        ledger = Ledger(self.store_path)
        ledger.import_csv(self.csv_path)
        loaded = ledger.load()
        self.assertEqual(loaded[0].category, "custom")

    def test_import_csv_accepts_custom_rules(self):
        self._write_csv("date,amount,payee,memo\n2024-01-01,-5.00,Acme Corp,\n")
        custom_rules = [Rule(pattern="acme", category="business", priority=1)]
        ledger = Ledger(self.store_path)
        ledger.import_csv(self.csv_path, rules=custom_rules)
        loaded = ledger.load()
        self.assertEqual(loaded[0].category, "business")

    def test_import_csv_replaces_previous_store_contents(self):
        Path(self.store_path).write_text(json.dumps({"transactions": [
            {"id": 1, "date": "2020-01-01", "amount": "1.00", "payee": "Old", "memo": "", "category": None},
        ]}), encoding="utf-8")
        self._write_csv("date,amount,payee,memo\n2024-01-01,-5.00,New Payee,\n")
        ledger = Ledger(self.store_path)
        ledger.import_csv(self.csv_path)
        loaded = ledger.load()
        self.assertEqual(len(loaded), 1)
        self.assertEqual(loaded[0].payee, "New Payee")


class TestLedgerCategorizeAll(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")

    def _seed(self, txns):
        Path(self.store_path).write_text(json.dumps({"transactions": txns}), encoding="utf-8")

    def test_returns_number_changed(self):
        self._seed([
            {"id": 1, "date": "2024-01-01", "amount": "-5.00", "payee": "Whole Foods", "memo": "", "category": None},
            {"id": 2, "date": "2024-01-02", "amount": "1.00", "payee": "Unmatched Zzz", "memo": "", "category": None},
        ])
        ledger = Ledger(self.store_path)
        changed = ledger.categorize_all()
        self.assertEqual(changed, 1)

    def test_persists_new_categories(self):
        self._seed([
            {"id": 1, "date": "2024-01-01", "amount": "-5.00", "payee": "Whole Foods", "memo": "", "category": None},
        ])
        ledger = Ledger(self.store_path)
        ledger.categorize_all()
        loaded = ledger.load()
        self.assertEqual(loaded[0].category, "groceries")

    def test_preserves_d6_quirk_clears_unmatched_category(self):
        # Existing (documented, out-of-scope-to-fix) behaviour: categorize
        # unconditionally overwrites category, including clearing one that
        # was manually set when no rule currently matches.
        self._seed([
            {"id": 1, "date": "2024-01-01", "amount": "-5.00", "payee": "Some Random Payee",
             "memo": "", "category": "manually-set"},
        ])
        ledger = Ledger(self.store_path)
        ledger.categorize_all()
        loaded = ledger.load()
        self.assertIsNone(loaded[0].category)

    def test_accepts_custom_rules(self):
        self._seed([
            {"id": 1, "date": "2024-01-01", "amount": "-5.00", "payee": "Acme Corp", "memo": "", "category": None},
        ])
        custom_rules = [Rule(pattern="acme", category="business", priority=1)]
        ledger = Ledger(self.store_path)
        changed = ledger.categorize_all(rules=custom_rules)
        self.assertEqual(changed, 1)
        self.assertEqual(ledger.load()[0].category, "business")

    def test_no_change_returns_zero(self):
        self._seed([
            {"id": 1, "date": "2024-01-01", "amount": "-5.00", "payee": "Whole Foods", "memo": "",
             "category": "groceries"},
        ])
        ledger = Ledger(self.store_path)
        changed = ledger.categorize_all()
        self.assertEqual(changed, 0)


class TestCliUnchangedBehaviour(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")
        self.csv_path = str(Path(self.tmpdir.name) / "in.csv")

    def test_import_command_prints_exact_message(self):
        Path(self.csv_path).write_text(
            "date,amount,payee,memo\n2024-01-01,-5.00,Whole Foods,\n2024-01-02,10.00,Payroll Inc,\n",
            encoding="utf-8",
        )
        code, out, _err = _run(["import", self.csv_path, "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), f"Imported 2 transactions into {self.store_path}")

    def test_categorize_command_prints_exact_message(self):
        Path(self.store_path).write_text(json.dumps({"transactions": [
            {"id": 1, "date": "2024-01-01", "amount": "-5.00", "payee": "Whole Foods", "memo": "", "category": None},
            {"id": 2, "date": "2024-01-02", "amount": "1.00", "payee": "Unmatched Zzz", "memo": "", "category": None},
        ]}), encoding="utf-8")
        code, out, _err = _run(["categorize", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "Re-categorized 1 of 2 transactions")

    def test_categorize_command_preserves_d6_quirk(self):
        Path(self.store_path).write_text(json.dumps({"transactions": [
            {"id": 1, "date": "2024-01-01", "amount": "-5.00", "payee": "Some Random Payee",
             "memo": "", "category": "manually-set"},
        ]}), encoding="utf-8")
        _run(["categorize", "--store", self.store_path])
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertIsNone(raw["transactions"][0]["category"])

    def test_summary_command_unaffected(self):
        Path(self.store_path).write_text(json.dumps({"transactions": [
            {"id": 1, "date": "2024-01-01", "amount": "-10.00", "payee": "X", "memo": "", "category": "food"},
            {"id": 2, "date": "2024-01-02", "amount": "-30.00", "payee": "Y", "memo": "", "category": "rent"},
        ]}), encoding="utf-8")
        code, out, _err = _run(["summary", "--store", self.store_path])
        self.assertEqual(code, 0)
        lines = out.strip().splitlines()
        self.assertEqual(lines[0], f"{'Category':<14}  {'Total':>10}  {'Share':>7}")
        self.assertIn("food", lines[1])
        self.assertIn("rent", lines[2])

    def test_list_command_unaffected(self):
        Path(self.store_path).write_text(json.dumps({"transactions": [
            {"id": 1, "date": "2024-01-01", "amount": "-10.00", "payee": "Dummy", "memo": "", "category": None},
            {"id": 2, "date": "2024-01-02", "amount": "-30.00", "payee": "Real", "memo": "note", "category": "rent"},
        ]}), encoding="utf-8")
        code, out, _err = _run(["list", "--store", self.store_path, "--page-size", "10"])
        self.assertEqual(code, 0)
        self.assertIn("Real", out)
        self.assertIn("note", out)


if __name__ == "__main__":
    unittest.main()
