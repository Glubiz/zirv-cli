import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


class TestSummaryDefaultReverted(unittest.TestCase):
    def _run(self, *args):
        return subprocess.run(
            [sys.executable, "-m", "ledgerlite", *args],
            capture_output=True, text=True,
        )

    def _seeded_store(self, tmp):
        csv_path = Path(tmp) / "sample.csv"
        csv_path.write_text(
            "date,amount,payee,memo,category\n"
            "2024-01-01,-10.00,Cafe One,lunch,Eating Out\n"
            "2024-01-02,-20.00,Cafe Two,dinner,dining\n",
            encoding="utf-8",
        )
        store = str(Path(tmp) / "ledger.json")
        r = self._run("import", str(csv_path), "--store", store)
        self.assertEqual(r.returncode, 0, r.stderr)
        self._run("category-alias", "add", "--pattern", "Eating Out", "--canonical", "dining", "--store", store)
        return store

    def test_default_no_longer_folds_categories(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = self._seeded_store(tmp)
            r = self._run("summary", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            lines = [ln for ln in r.stdout.splitlines() if ln.strip()]
            dining_lines = [ln for ln in lines if ln.startswith("dining")]
            eating_lines = [ln for ln in lines if ln.startswith("Eating Out")]
            self.assertEqual(len(dining_lines), 1)
            self.assertEqual(len(eating_lines), 1)

    def test_flag_still_folds_categories(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = self._seeded_store(tmp)
            r = self._run("summary", "--canonical-category", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            lines = [ln for ln in r.stdout.splitlines() if ln.strip()]
            dining_lines = [ln for ln in lines if ln.startswith("dining")]
            self.assertEqual(len(dining_lines), 1)
            self.assertIn("30.00", dining_lines[0].replace("-", ""))
            eating_lines = [ln for ln in lines if ln.startswith("Eating Out")]
            self.assertEqual(eating_lines, [])

    def test_category_alias_add_list_unaffected(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = self._seeded_store(tmp)
            r = self._run("category-alias", "list", "--store", store)
            self.assertEqual(r.stdout.strip(), "Eating Out -> dining")


if __name__ == "__main__":
    unittest.main()
