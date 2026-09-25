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


class TestExport(unittest.TestCase):
    def _seeded_store(self, tmp):
        csv_path = Path(tmp) / "sample.csv"
        csv_path.write_text(
            "date,amount,payee,memo,category,currency\n"
            "2024-01-01,-1.00,DUMMY,n/a,other,USD\n"
            "2024-01-02,-13.50,LONDON CAFE,lunch,dining,GBP\n"
            "2024-01-03,2500.00,UNKNOWN VENDOR XYZ,pay,,USD\n",
            encoding="utf-8",
        )
        store = str(Path(tmp) / "ledger.json")
        _run("import", str(csv_path), "--store", store)
        return store

    def test_export_writes_header_and_rows(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = self._seeded_store(tmp)
            out_path = Path(tmp) / "out.csv"
            r = _run("export", "--store", store, "--out", str(out_path))
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("Exported 3 transactions", r.stdout)

            text = out_path.read_text(encoding="utf-8")
            lines = [ln for ln in text.splitlines() if ln.strip()]
            self.assertEqual(lines[0], "date,amount,payee,memo,category,currency")
            self.assertEqual(len(lines), 4)
            self.assertIn("LONDON CAFE", lines[2])
            self.assertIn("GBP", lines[2])
            # Uncategorized row -> empty category field, not "None".
            self.assertIn(",,USD", lines[3])

    def test_export_then_import_round_trips(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = self._seeded_store(tmp)
            out_path = Path(tmp) / "out.csv"
            _run("export", "--store", store, "--out", str(out_path))

            store2 = str(Path(tmp) / "ledger2.json")
            r = _run("import", str(out_path), "--store", store2)
            self.assertEqual(r.returncode, 0, r.stderr)
            data1 = json.loads(Path(store).read_text(encoding="utf-8"))
            data2 = json.loads(Path(store2).read_text(encoding="utf-8"))
            orig = [(t["date"], t["amount"], t["payee"], t["memo"], t["category"], t["currency"])
                    for t in data1["transactions"]]
            round_tripped = [(t["date"], t["amount"], t["payee"], t["memo"], t["category"], t["currency"])
                              for t in data2["transactions"]]
            self.assertEqual(orig, round_tripped)


if __name__ == "__main__":
    unittest.main()
