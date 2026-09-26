import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


class TestAliasRemove(unittest.TestCase):
    def _run(self, *args):
        return subprocess.run(
            [sys.executable, "-m", "ledgerlite", *args],
            capture_output=True, text=True,
        )

    def test_remove_existing_alias(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            self._run("alias", "add", "--pattern", "WHOLE FOODS", "--canonical", "Whole Foods", "--store", store)
            self._run("alias", "add", "--pattern", "TRADER JOE", "--canonical", "Trader Joe's", "--store", store)
            r = self._run("alias", "remove", "WHOLE FOODS", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("Removed alias WHOLE FOODS", r.stdout)
            r_list = self._run("alias", "list", "--store", store)
            lines = [ln for ln in r_list.stdout.splitlines() if ln.strip()]
            self.assertEqual(lines, ["TRADER JOE -> Trader Joe's"])

    def test_remove_unknown_pattern_exits_2_and_no_changes(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            self._run("alias", "add", "--pattern", "WHOLE FOODS", "--canonical", "Whole Foods", "--store", store)
            r = self._run("alias", "remove", "NOPE", "--store", store)
            self.assertEqual(r.returncode, 2)
            r_list = self._run("alias", "list", "--store", store)
            lines = [ln for ln in r_list.stdout.splitlines() if ln.strip()]
            self.assertEqual(lines, ["WHOLE FOODS -> Whole Foods"])


if __name__ == "__main__":
    unittest.main()
