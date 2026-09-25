import subprocess
import sys
import tempfile
import unittest
from decimal import Decimal
from pathlib import Path


class TestCategoryAliasCli(unittest.TestCase):
    def _run(self, *args):
        return subprocess.run(
            [sys.executable, "-m", "ledgerlite", *args],
            capture_output=True, text=True,
        )

    def test_add_and_list_round_trip(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            r = self._run("category-alias", "add", "--pattern", "eating out", "--canonical", "dining", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("Added alias eating out -> dining", r.stdout)
            r_list = self._run("category-alias", "list", "--store", store)
            self.assertEqual(r_list.stdout.strip(), "eating out -> dining")

    def test_list_empty(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            r = self._run("category-alias", "list", "--store", store)
            self.assertEqual(r.stdout.strip(), "No aliases.")

    def test_merchant_aliases_unaffected_by_category_aliases(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            self._run("alias", "add", "--pattern", "WHOLE FOODS", "--canonical", "Whole Foods", "--store", store)
            self._run("category-alias", "add", "--pattern", "eating out", "--canonical", "dining", "--store", store)
            r = self._run("alias", "list", "--store", store)
            self.assertEqual(r.stdout.strip(), "WHOLE FOODS -> Whole Foods")


class TestSummaryUsesCanonicalCategory(unittest.TestCase):
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
        return store

    def test_summary_folds_aliased_categories_together(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = self._seeded_store(tmp)
            self._run("category-alias", "add", "--pattern", "Eating Out", "--canonical", "dining", "--store", store)
            r = self._run("summary", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            lines = [ln for ln in r.stdout.splitlines() if ln.strip()]
            dining_lines = [ln for ln in lines if ln.startswith("dining")]
            self.assertEqual(len(dining_lines), 1, f"expected exactly one folded 'dining' row, got: {lines!r}")
            self.assertIn("30.00", dining_lines[0].replace("-", ""))

    def test_summary_without_category_aliases_is_unchanged(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = self._seeded_store(tmp)
            r = self._run("summary", "--store", store)
            lines = [ln for ln in r.stdout.splitlines() if ln.strip()]
            # No category alias saved: both raw category spellings appear separately.
            joined = "\n".join(lines)
            self.assertIn("dining", joined)
            self.assertIn("Eating Out", joined)


if __name__ == "__main__":
    unittest.main()
