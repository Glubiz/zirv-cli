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


class TestUndo(unittest.TestCase):
    def test_undo_import_restores_prior_state(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            csv1 = Path(tmp) / "one.csv"
            csv1.write_text("date,amount,payee,memo\n2024-01-01,-1.00,A,x\n", encoding="utf-8")
            _run("import", str(csv1), "--store", store)

            csv2 = Path(tmp) / "two.csv"
            csv2.write_text("date,amount,payee,memo\n2024-01-02,-2.00,B,y\n", encoding="utf-8")
            _run("import", str(csv2), "--store", store, "--append")

            data = json.loads(Path(store).read_text(encoding="utf-8"))
            self.assertEqual(len(data["transactions"]), 2)

            r = _run("undo", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn(f"Undid: import {csv2}", r.stdout)

            data = json.loads(Path(store).read_text(encoding="utf-8"))
            payees = [t["payee"] for t in data["transactions"]]
            self.assertEqual(payees, ["A"])

    def test_undo_list_most_recent_first(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            csv1 = Path(tmp) / "one.csv"
            csv1.write_text("date,amount,payee,memo\n2024-01-01,-1.00,A,x\n", encoding="utf-8")
            _run("import", str(csv1), "--store", store)

            _run("recurring", "add", "--payee", "Netflix", "--amount", "-15.49",
                 "--day", "5", "--store", store)
            r_apply = _run("recurring", "apply", "--from", "2024-01", "--to", "2024-01", "--store", store)
            self.assertEqual(r_apply.returncode, 0, r_apply.stderr)

            r_list = _run("undo", "--list", "--store", store)
            lines = [ln for ln in r_list.stdout.splitlines() if ln.strip()]
            self.assertEqual(lines, ["recurring apply 2024-01..2024-01", f"import {csv1}"])

    def test_undo_empty_stack(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            r = _run("undo", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual(r.stdout.strip(), "Nothing to undo.")
            r2 = _run("undo", "--list", "--store", store)
            self.assertEqual(r2.stdout.strip(), "Nothing to undo.")

    def test_two_undos_go_back_two_steps(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            csv1 = Path(tmp) / "one.csv"
            csv1.write_text("date,amount,payee,memo\n2024-01-01,-1.00,A,x\n", encoding="utf-8")
            _run("import", str(csv1), "--store", store)
            csv2 = Path(tmp) / "two.csv"
            csv2.write_text("date,amount,payee,memo\n2024-01-02,-2.00,B,y\n", encoding="utf-8")
            _run("import", str(csv2), "--store", store, "--append")

            _run("undo", "--store", store)
            r = _run("undo", "--store", store)
            self.assertIn(f"Undid: import {csv1}", r.stdout)

            data = json.loads(Path(store).read_text(encoding="utf-8"))
            self.assertEqual(data["transactions"], [])

            r3 = _run("undo", "--store", store)
            self.assertEqual(r3.stdout.strip(), "Nothing to undo.")

    def test_budget_and_reconcile_not_undoable(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            _run("budget", "set", "--category", "groceries", "--limit", "100", "--store", store)
            r = _run("undo", "--list", "--store", store)
            self.assertEqual(r.stdout.strip(), "Nothing to undo.")


if __name__ == "__main__":
    unittest.main()
