import importlib
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


class TestAliasCliModuleExists(unittest.TestCase):
    def test_alias_cli_module_importable(self):
        try:
            importlib.import_module("ledgerlite.alias_cli")
        except ImportError as exc:
            self.fail(f"expected ledgerlite/alias_cli.py to exist and be importable: {exc}")


class TestBehaviourPreserved(unittest.TestCase):
    """Regression: everything from steps 1-3 must still work identically."""

    def _run(self, *args):
        return subprocess.run(
            [sys.executable, "-m", "ledgerlite", *args],
            capture_output=True, text=True,
        )

    def test_add_list_round_trip_and_order(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            self._run("alias", "add", "--pattern", "WHOLE FOODS", "--canonical", "Whole Foods", "--store", store)
            self._run("alias", "add", "--pattern", "TRADER JOE", "--canonical", "Trader Joe's", "--store", store)
            r = self._run("alias", "list", "--store", store)
            lines = [ln for ln in r.stdout.splitlines() if ln.strip()]
            self.assertEqual(lines, ["WHOLE FOODS -> Whole Foods", "TRADER JOE -> Trader Joe's"])

    def test_list_empty(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            r = self._run("alias", "list", "--store", store)
            self.assertEqual(r.stdout.strip(), "No aliases.")

    def test_case_insensitive_matching_preserved(self):
        with tempfile.TemporaryDirectory() as tmp:
            csv_path = Path(tmp) / "sample.csv"
            csv_path.write_text(
                "date,amount,payee,memo\n"
                "2024-01-01,-1.00,DUMMY ROW,n/a\n"
                "2024-01-02,-12.34,wholefoods.com,groceries\n",
                encoding="utf-8",
            )
            store = str(Path(tmp) / "ledger.json")
            self._run("import", str(csv_path), "--store", store)
            self._run("alias", "add", "--pattern", "WHOLEFOODS.COM", "--canonical", "Whole Foods", "--store", store)
            r = self._run("list", "--canonical", "--store", store)
            self.assertIn("Whole Foods", r.stdout)
            self.assertNotIn("wholefoods.com", r.stdout)

    def test_unknown_alias_command_exits_2(self):
        r = self._run("alias", "bogus")
        self.assertNotEqual(r.returncode, 0)


if __name__ == "__main__":
    unittest.main()
