import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from ledgerlite.aliases import resolve_alias


class TestResolveAlias(unittest.TestCase):
    def test_first_match_wins(self):
        aliases = [("WHOLE FOODS", "Whole Foods"), ("WHOLEFOODS.COM", "Whole Foods Online")]
        self.assertEqual(resolve_alias("WHOLE FOODS MKT 221", aliases), "Whole Foods")

    def test_no_match_returns_unchanged(self):
        self.assertEqual(resolve_alias("TRADER JOES #58", [("WHOLE FOODS", "Whole Foods")]), "TRADER JOES #58")

    def test_case_sensitive_for_now(self):
        # Step 1 spec: exact-case substring match (case-insensitivity comes later).
        self.assertEqual(resolve_alias("wholefoods.com", [("WHOLEFOODS.COM", "Whole Foods")]), "wholefoods.com")

    def test_empty_aliases(self):
        self.assertEqual(resolve_alias("anything", []), "anything")


class TestAliasCli(unittest.TestCase):
    def _run(self, *args):
        return subprocess.run(
            [sys.executable, "-m", "ledgerlite", *args],
            capture_output=True, text=True,
        )

    def test_add_and_list_round_trip_and_order(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            r1 = self._run("alias", "add", "--pattern", "WHOLE FOODS", "--canonical", "Whole Foods", "--store", store)
            self.assertEqual(r1.returncode, 0, r1.stderr)
            self.assertIn("Added alias WHOLE FOODS -> Whole Foods", r1.stdout)

            r2 = self._run("alias", "add", "--pattern", "TRADER JOE", "--canonical", "Trader Joe's", "--store", store)
            self.assertEqual(r2.returncode, 0, r2.stderr)

            r3 = self._run("alias", "list", "--store", store)
            self.assertEqual(r3.returncode, 0, r3.stderr)
            lines = [ln for ln in r3.stdout.splitlines() if ln.strip()]
            self.assertEqual(lines, ["WHOLE FOODS -> Whole Foods", "TRADER JOE -> Trader Joe's"])

    def test_list_empty(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            r = self._run("alias", "list", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual(r.stdout.strip(), "No aliases.")

    def test_aliases_do_not_clobber_transactions(self):
        with tempfile.TemporaryDirectory() as tmp:
            csv_path = Path(tmp) / "sample.csv"
            # A dummy leading row: report.page's page-1 quirk skips row 0, so
            # the row under test must not be first.
            csv_path.write_text(
                "date,amount,payee,memo\n"
                "2024-01-01,-1.00,DUMMY ROW,n/a\n"
                "2024-01-02,-12.34,WHOLE FOODS MKT 221,groceries\n",
                encoding="utf-8",
            )
            store = str(Path(tmp) / "ledger.json")
            r_import = self._run("import", str(csv_path), "--store", store)
            self.assertEqual(r_import.returncode, 0, r_import.stderr)

            r_add = self._run("alias", "add", "--pattern", "WHOLE FOODS", "--canonical", "Whole Foods", "--store", store)
            self.assertEqual(r_add.returncode, 0, r_add.stderr)

            # Saving transactions again (e.g. via categorize) must not wipe aliases.
            r_cat = self._run("categorize", "--store", store)
            self.assertEqual(r_cat.returncode, 0, r_cat.stderr)

            r_list = self._run("alias", "list", "--store", store)
            self.assertEqual(r_list.stdout.strip(), "WHOLE FOODS -> Whole Foods")

            r_txn_list = self._run("list", "--store", store)
            self.assertIn("WHOLE FOODS MKT 221", r_txn_list.stdout)


if __name__ == "__main__":
    unittest.main()
