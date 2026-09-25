import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from ledgerlite.aliases import resolve_alias


class TestResolveAliasCaseInsensitive(unittest.TestCase):
    def test_lowercase_payee_matches_uppercase_pattern(self):
        aliases = [("WHOLEFOODS.COM", "Whole Foods")]
        self.assertEqual(resolve_alias("wholefoods.com", aliases), "Whole Foods")

    def test_mixed_case_pattern_matches_uppercase_payee(self):
        aliases = [("Whole Foods", "Whole Foods")]
        self.assertEqual(resolve_alias("WHOLE FOODS MKT 221", aliases), "Whole Foods")

    def test_first_match_still_wins_case_insensitively(self):
        aliases = [("whole foods", "A"), ("MKT", "B")]
        self.assertEqual(resolve_alias("WHOLE FOODS MKT 221", aliases), "A")

    def test_no_match_returns_unchanged(self):
        self.assertEqual(resolve_alias("TRADER JOES #58", [("whole foods", "Whole Foods")]), "TRADER JOES #58")


class TestAliasCliCaseInsensitive(unittest.TestCase):
    def _run(self, *args):
        return subprocess.run(
            [sys.executable, "-m", "ledgerlite", *args],
            capture_output=True, text=True,
        )

    def test_list_canonical_matches_regardless_of_case(self):
        with tempfile.TemporaryDirectory() as tmp:
            csv_path = Path(tmp) / "sample.csv"
            csv_path.write_text(
                "date,amount,payee,memo\n"
                "2024-01-01,-1.00,DUMMY ROW,n/a\n"
                "2024-01-02,-12.34,wholefoods.com,groceries\n",
                encoding="utf-8",
            )
            store = str(Path(tmp) / "ledger.json")
            self.assertEqual(self._run("import", str(csv_path), "--store", store).returncode, 0)
            self.assertEqual(
                self._run("alias", "add", "--pattern", "WHOLEFOODS.COM", "--canonical", "Whole Foods", "--store", store).returncode,
                0,
            )
            r = self._run("list", "--canonical", "--store", store)
            self.assertIn("Whole Foods", r.stdout)
            self.assertNotIn("wholefoods.com", r.stdout)


if __name__ == "__main__":
    unittest.main()
