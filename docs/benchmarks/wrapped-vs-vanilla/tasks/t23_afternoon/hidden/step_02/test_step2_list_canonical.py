import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


class TestListCanonical(unittest.TestCase):
    def _run(self, *args):
        return subprocess.run(
            [sys.executable, "-m", "ledgerlite", *args],
            capture_output=True, text=True,
        )

    def _seeded_store(self, tmp):
        csv_path = Path(tmp) / "sample.csv"
        csv_path.write_text(
            "date,amount,payee,memo\n"
            "2024-01-01,-1.00,DUMMY ROW,n/a\n"
            "2024-01-02,-12.34,WHOLE FOODS MKT 221,groceries\n"
            "2024-01-03,-4.50,STARBUCKS #4,coffee\n",
            encoding="utf-8",
        )
        store = str(Path(tmp) / "ledger.json")
        r = self._run("import", str(csv_path), "--store", store)
        self.assertEqual(r.returncode, 0, r.stderr)
        r = self._run("alias", "add", "--pattern", "WHOLE FOODS", "--canonical", "Whole Foods", "--store", store)
        self.assertEqual(r.returncode, 0, r.stderr)
        return store

    def test_default_shows_raw_payee(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = self._seeded_store(tmp)
            r = self._run("list", "--store", store)
            self.assertIn("WHOLE FOODS MKT 221", r.stdout)
            self.assertNotIn("Whole Foods ", r.stdout)

    def test_canonical_flag_shows_canonical_name(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = self._seeded_store(tmp)
            r = self._run("list", "--canonical", "--store", store)
            self.assertIn("Whole Foods", r.stdout)
            self.assertNotIn("WHOLE FOODS MKT 221", r.stdout)

    def test_canonical_flag_unmatched_payee_shown_as_is(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = self._seeded_store(tmp)
            r = self._run("list", "--canonical", "--store", store)
            self.assertIn("STARBUCKS #4", r.stdout)

    def test_empty_page_message_unaffected(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = self._seeded_store(tmp)
            r = self._run("list", "--canonical", "--page", "99", "--store", store)
            self.assertEqual(r.stdout.strip(), "No transactions on this page.")


if __name__ == "__main__":
    unittest.main()
