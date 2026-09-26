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


def _write_csv(path, rows):
    lines = ["date,amount,payee,memo"]
    lines.extend(rows)
    Path(path).write_text("\n".join(lines) + "\n", encoding="utf-8")


class TestAppendImport(unittest.TestCase):
    def test_append_continues_ids(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            first = Path(tmp) / "first.csv"
            _write_csv(first, [
                "2024-01-01,-1.00,A,x",
                "2024-01-02,-2.00,B,y",
                "2024-01-03,-3.00,C,z",
            ])
            r1 = _run("import", str(first), "--store", store)
            self.assertEqual(r1.returncode, 0, r1.stderr)
            self.assertIn("Imported 3 transactions", r1.stdout)

            second = Path(tmp) / "second.csv"
            _write_csv(second, [
                "2024-01-04,-4.00,D,w",
                "2024-01-05,-5.00,E,v",
            ])
            r2 = _run("import", str(second), "--store", store, "--append")
            self.assertEqual(r2.returncode, 0, r2.stderr)
            # N in the message is rows in THIS file, not the running total.
            self.assertIn("Imported 2 transactions", r2.stdout)

            data = json.loads(Path(store).read_text(encoding="utf-8"))
            ids = [t["id"] for t in data["transactions"]]
            self.assertEqual(ids, [1, 2, 3, 4, 5])
            payees = [t["payee"] for t in data["transactions"]]
            self.assertEqual(payees, ["A", "B", "C", "D", "E"])

    def test_default_still_replaces(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            first = Path(tmp) / "first.csv"
            _write_csv(first, ["2024-01-01,-1.00,A,x", "2024-01-02,-2.00,B,y"])
            _run("import", str(first), "--store", store)

            second = Path(tmp) / "second.csv"
            _write_csv(second, ["2024-01-03,-3.00,C,z"])
            r2 = _run("import", str(second), "--store", store)
            self.assertEqual(r2.returncode, 0, r2.stderr)

            data = json.loads(Path(store).read_text(encoding="utf-8"))
            payees = [t["payee"] for t in data["transactions"]]
            self.assertEqual(payees, ["C"])
            self.assertEqual(data["transactions"][0]["id"], 1)

    def test_append_onto_missing_store_starts_at_one(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            csv_path = Path(tmp) / "only.csv"
            _write_csv(csv_path, ["2024-01-01,-1.00,A,x"])
            r = _run("import", str(csv_path), "--store", store, "--append")
            self.assertEqual(r.returncode, 0, r.stderr)
            data = json.loads(Path(store).read_text(encoding="utf-8"))
            self.assertEqual(data["transactions"][0]["id"], 1)

    def test_save_does_not_clobber_unrelated_top_level_keys(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = Path(tmp) / "ledger.json"
            store.write_text(json.dumps({
                "transactions": [],
                "__future_feature__": {"kept": True},
            }), encoding="utf-8")
            csv_path = Path(tmp) / "one.csv"
            _write_csv(csv_path, ["2024-01-01,-1.00,A,x"])

            r = _run("import", str(csv_path), "--store", str(store))
            self.assertEqual(r.returncode, 0, r.stderr)
            data = json.loads(store.read_text(encoding="utf-8"))
            self.assertEqual(data.get("__future_feature__"), {"kept": True})


if __name__ == "__main__":
    unittest.main()
