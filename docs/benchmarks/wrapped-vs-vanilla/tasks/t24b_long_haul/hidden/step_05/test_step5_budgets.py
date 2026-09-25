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


class TestBudgetCli(unittest.TestCase):
    def test_set_and_list(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            r1 = _run("budget", "set", "--category", "groceries", "--limit", "400", "--store", store)
            self.assertEqual(r1.returncode, 0, r1.stderr)
            self.assertIn("Set budget for groceries: 400.00", r1.stdout)

            _run("budget", "set", "--category", "dining", "--limit", "150", "--store", store)
            r2 = _run("budget", "list", "--store", store)
            lines = [ln for ln in r2.stdout.splitlines() if ln.strip()]
            self.assertEqual(lines, ["dining: 150.00", "groceries: 400.00"])

    def test_set_replaces_existing(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            _run("budget", "set", "--category", "groceries", "--limit", "400", "--store", store)
            _run("budget", "set", "--category", "groceries", "--limit", "500", "--store", store)
            r = _run("budget", "list", "--store", store)
            self.assertEqual(r.stdout.strip(), "groceries: 500.00")

    def test_list_empty(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            r = _run("budget", "list", "--store", store)
            self.assertEqual(r.stdout.strip(), "No budgets.")


class TestSummaryVsBudget(unittest.TestCase):
    def _seeded_store(self, tmp):
        csv_path = Path(tmp) / "sample.csv"
        csv_path.write_text(
            "date,amount,payee,memo,category\n"
            "2024-01-01,-1.00,DUMMY,n/a,other\n"
            "2024-01-02,-120.50,WHOLE FOODS,groceries,groceries\n"
            "2024-01-03,-200.00,RESTAURANT,dinner,dining\n",
            encoding="utf-8",
        )
        store = str(Path(tmp) / "ledger.json")
        r = _run("import", str(csv_path), "--store", store)
        self.assertEqual(r.returncode, 0, r.stderr)
        _run("budget", "set", "--category", "groceries", "--limit", "400", "--store", store)
        _run("budget", "set", "--category", "dining", "--limit", "150", "--store", store)
        return store

    def test_vs_budget_shows_budget_and_remaining(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = self._seeded_store(tmp)
            r = _run("summary", "--vs-budget", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            lines = [ln for ln in r.stdout.splitlines() if ln.strip()]
            dining_line = next(ln for ln in lines if ln.startswith("dining"))
            groceries_line = next(ln for ln in lines if ln.startswith("groceries"))
            other_line = next(ln for ln in lines if ln.startswith("other"))
            self.assertIn("150.00", dining_line)
            self.assertIn("-50.00", dining_line)  # 150 - 200 spent = -50 over
            self.assertIn("400.00", groceries_line)
            self.assertIn("279.50", groceries_line)  # 400 - 120.50
            self.assertIn("-", other_line)  # no budget set for "other"

    def test_default_summary_unaffected(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = self._seeded_store(tmp)
            r = _run("summary", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("Share", r.stdout)
            self.assertNotIn("Budget", r.stdout)


if __name__ == "__main__":
    unittest.main()
