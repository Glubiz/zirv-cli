"""Regression coverage for the cli.py -> commands/ split. Deliberately does
not care WHICH module inside the package a given command lives in, only
that the `ledgerlite.commands` package itself exists (the one structural
fact the prompt actually pins down) and that every command from steps 1-7
still behaves identically."""

import importlib
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


def _run(*args):
    return subprocess.run(
        [sys.executable, "-m", "ledgerlite", *args],
        capture_output=True, text=True,
    )


class TestCommandsPackageExists(unittest.TestCase):
    def test_commands_package_importable(self):
        try:
            importlib.import_module("ledgerlite.commands")
        except ImportError as exc:
            self.fail(f"expected a ledgerlite/commands/ package to exist and be importable: {exc}")


class TestEveryCommandStillWorks(unittest.TestCase):
    def test_import_append_and_currency(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            csv1 = Path(tmp) / "one.csv"
            csv1.write_text("date,amount,payee,memo\n2024-01-01,-10.00,A,x\n", encoding="utf-8")
            r1 = _run("import", str(csv1), "--store", store, "--currency", "EUR")
            self.assertEqual(r1.returncode, 0, r1.stderr)
            self.assertIn("Imported 1 transactions", r1.stdout)

            csv2 = Path(tmp) / "two.csv"
            csv2.write_text("date,amount,payee,memo\n2024-01-02,-5.00,B,y\n", encoding="utf-8")
            r2 = _run("import", str(csv2), "--store", store, "--append")
            self.assertEqual(r2.returncode, 0, r2.stderr)

            data = json.loads(Path(store).read_text(encoding="utf-8"))
            ids = [t["id"] for t in data["transactions"]]
            self.assertEqual(ids, [1, 2])
            self.assertEqual(data["transactions"][0]["currency"], "EUR")

    def test_fx_convert(self):
        r = _run("fx", "convert", "100", "USD", "EUR")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "91.74")

    def test_list_sort_and_legacy_order(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = Path(tmp) / "ledger.json"
            store.write_text(json.dumps({"transactions": [
                {"id": 1, "date": "2024-01-01", "amount": "-1.00", "payee": "FIRST",
                 "memo": "", "category": None, "currency": "USD"},
                {"id": 2, "date": "2024-01-02", "amount": "-2.00", "payee": "SECOND",
                 "memo": "", "category": None, "currency": "USD"},
            ]}), encoding="utf-8")
            r_default = _run("list", "--store", str(store), "--page-size", "10")
            self.assertEqual(r_default.returncode, 0, r_default.stderr)
            r_legacy = _run("list", "--store", str(store), "--page-size", "10", "--legacy-order")
            self.assertEqual(r_legacy.returncode, 0, r_legacy.stderr)
            # Different orders -> different output (default drops SECOND,
            # legacy drops FIRST, given the page-1 leading-row quirk).
            self.assertNotIn("SECOND", r_default.stdout)
            self.assertNotIn("FIRST", r_legacy.stdout)

    def test_summary_and_vs_budget(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            csv_path = Path(tmp) / "s.csv"
            csv_path.write_text(
                "date,amount,payee,memo,category\n"
                "2024-01-01,-1.00,DUMMY,n/a,other\n"
                "2024-01-02,-50.00,SHOP,x,groceries\n",
                encoding="utf-8",
            )
            _run("import", str(csv_path), "--store", store)
            _run("budget", "set", "--category", "groceries", "--limit", "100", "--store", store)

            r1 = _run("summary", "--store", store)
            self.assertIn("Share", r1.stdout)
            r2 = _run("summary", "--vs-budget", "--store", store)
            self.assertIn("Budget", r2.stdout)
            self.assertIn("50.00", r2.stdout)

    def test_categorize(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            csv_path = Path(tmp) / "s.csv"
            csv_path.write_text(
                "date,amount,payee,memo\n2024-01-01,-45.00,WHOLE FOODS,x\n",
                encoding="utf-8",
            )
            _run("import", str(csv_path), "--store", store)
            r = _run("categorize", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("Re-categorized", r.stdout)

    def test_recurring_family(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            _run("recurring", "add", "--payee", "Netflix", "--amount", "-15.49",
                 "--day", "5", "--store", store)
            r_list = _run("recurring", "list", "--store", store)
            self.assertIn("Netflix", r_list.stdout)
            r_apply = _run("recurring", "apply", "--from", "2024-01", "--to", "2024-01", "--store", store)
            self.assertIn("Applied 1 recurring transactions", r_apply.stdout)

    def test_budget_family(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            _run("budget", "set", "--category", "dining", "--limit", "50", "--store", store)
            r = _run("budget", "list", "--store", store)
            self.assertEqual(r.stdout.strip(), "dining: 50.00")

    def test_report_cashflow(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            csv_path = Path(tmp) / "s.csv"
            csv_path.write_text("date,amount,payee,memo\n2024-01-01,100.00,X,y\n", encoding="utf-8")
            _run("import", str(csv_path), "--store", store)
            r = _run("report", "cashflow", "--from", "2024-01", "--to", "2024-01", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("100.00", r.stdout)

    def test_reconcile_family(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = Path(tmp) / "ledger.json"
            store.write_text(json.dumps({"transactions": [
                {"id": 1, "date": "2024-01-01", "amount": "-10.00", "payee": "A",
                 "memo": "", "category": None, "currency": "USD"},
            ]}), encoding="utf-8")
            _run("reconcile", "mark", "1", "--store", str(store))
            r = _run("reconcile", "status", "--statement-balance", "-10.00", "--store", str(store))
            self.assertIn("Status: reconciled", r.stdout)


if __name__ == "__main__":
    unittest.main()
