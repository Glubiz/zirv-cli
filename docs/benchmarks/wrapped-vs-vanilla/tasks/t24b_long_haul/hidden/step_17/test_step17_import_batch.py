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


class TestImportBatch(unittest.TestCase):
    def test_batch_imports_in_order_with_continuing_ids(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            jan = Path(tmp) / "jan.csv"
            _write_csv(jan, ["2024-01-01,-1.00,A,x", "2024-01-02,-2.00,B,y"])
            feb = Path(tmp) / "feb.csv"
            _write_csv(feb, ["2024-02-01,-3.00,C,z"])
            mar = Path(tmp) / "mar.csv"
            _write_csv(mar, ["2024-03-01,-4.00,D,w"])

            r = _run("import-batch", str(jan), str(feb), str(mar), "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            out_lines = [ln for ln in r.stdout.splitlines() if ln.strip()]
            self.assertEqual(len(out_lines), 3)
            self.assertIn("Imported 2 transactions", out_lines[0])
            self.assertIn("Imported 1 transactions", out_lines[1])
            self.assertIn("Imported 1 transactions", out_lines[2])

            data = json.loads(Path(store).read_text(encoding="utf-8"))
            ids = [t["id"] for t in data["transactions"]]
            self.assertEqual(ids, [1, 2, 3, 4])
            payees = [t["payee"] for t in data["transactions"]]
            self.assertEqual(payees, ["A", "B", "C", "D"])

    def test_batch_onto_existing_store_continues_ids(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = Path(tmp) / "ledger.json"
            store.write_text(json.dumps({"transactions": [
                {"id": 1, "date": "2023-12-31", "amount": "-9.00", "payee": "PRIOR",
                 "memo": "", "category": None, "currency": "USD"},
            ]}), encoding="utf-8")
            csv1 = Path(tmp) / "one.csv"
            _write_csv(csv1, ["2024-01-01,-1.00,A,x"])
            r = _run("import-batch", str(csv1), "--store", str(store))
            self.assertEqual(r.returncode, 0, r.stderr)
            data = json.loads(store.read_text(encoding="utf-8"))
            ids = [t["id"] for t in data["transactions"]]
            self.assertEqual(ids, [1, 2])


if __name__ == "__main__":
    unittest.main()
