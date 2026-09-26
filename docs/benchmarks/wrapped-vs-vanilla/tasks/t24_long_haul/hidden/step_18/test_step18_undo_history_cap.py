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


class TestUndoStillWorks(unittest.TestCase):
    """Regression: undo/undo --list must still behave the same externally
    (see hidden/step_12) after switching to diff-based history."""

    def test_undo_append_and_replace(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            csv1 = Path(tmp) / "one.csv"
            csv1.write_text("date,amount,payee,memo\n2024-01-01,-1.00,A,x\n", encoding="utf-8")
            _run("import", str(csv1), "--store", store)
            csv2 = Path(tmp) / "two.csv"
            csv2.write_text("date,amount,payee,memo\n2024-01-02,-2.00,B,y\n", encoding="utf-8")
            _run("import", str(csv2), "--store", store, "--append")

            r = _run("undo", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn(f"Undid: import {csv2}", r.stdout)
            data = json.loads(Path(store).read_text(encoding="utf-8"))
            self.assertEqual([t["payee"] for t in data["transactions"]], ["A"])

            r2 = _run("undo", "--store", store)
            self.assertIn(f"Undid: import {csv1}", r2.stdout)
            data2 = json.loads(Path(store).read_text(encoding="utf-8"))
            self.assertEqual(data2["transactions"], [])


class TestHistoryCap(unittest.TestCase):
    def test_history_capped_at_20_and_oldest_unrecoverable(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            for i in range(1, 26):  # 25 append operations, 1 row each
                csv_path = Path(tmp) / f"{i}.csv"
                csv_path.write_text(
                    f"date,amount,payee,memo\n2024-01-01,-1.00,ROW{i},x\n", encoding="utf-8"
                )
                r = _run("import", str(csv_path), "--store", store, "--append")
                self.assertEqual(r.returncode, 0, r.stderr)

            data = json.loads(Path(store).read_text(encoding="utf-8"))
            self.assertEqual(len(data["transactions"]), 25)

            r_list = _run("undo", "--list", "--store", store)
            labels = [ln for ln in r_list.stdout.splitlines() if ln.strip()]
            self.assertEqual(len(labels), 20)

            # Undo 20 times: succeeds every time, removing ROW25..ROW6.
            for _ in range(20):
                r = _run("undo", "--store", store)
                self.assertEqual(r.returncode, 0, r.stderr)
                self.assertNotEqual(r.stdout.strip(), "Nothing to undo.")

            data_after = json.loads(Path(store).read_text(encoding="utf-8"))
            remaining = [t["payee"] for t in data_after["transactions"]]
            self.assertEqual(remaining, ["ROW1", "ROW2", "ROW3", "ROW4", "ROW5"])

            # The 21st undo has nothing left to undo -- ROW1..ROW5's
            # imports aged out of the 20-entry history.
            r_final = _run("undo", "--store", store)
            self.assertEqual(r_final.stdout.strip(), "Nothing to undo.")
            data_final = json.loads(Path(store).read_text(encoding="utf-8"))
            self.assertEqual(
                [t["payee"] for t in data_final["transactions"]],
                ["ROW1", "ROW2", "ROW3", "ROW4", "ROW5"],
            )


if __name__ == "__main__":
    unittest.main()
