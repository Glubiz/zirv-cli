import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


def cli(store, *args):
    return subprocess.run(
        [sys.executable, "-m", "ledgerlite", *args, "--store", store],
        capture_output=True, text=True,
    )


class TestMembers(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.dir = Path(self._tmp.name)
        self.store = str(self.dir / "ledger.json")

    def test_add_and_list_in_order_added(self):
        for name in ("Ana", "Ben", "Cara"):
            r = cli(self.store, "member", "add", name)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual(r.stdout.strip(), f"Added member {name}")
        r = cli(self.store, "member", "list")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.splitlines(), ["Ana", "Ben", "Cara"])

    def test_list_empty(self):
        r = cli(self.store, "member", "list")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "No members.")

    def test_names_are_stripped(self):
        r = cli(self.store, "member", "add", "  Dana  ")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "Added member Dana")
        self.assertEqual(cli(self.store, "member", "list").stdout.splitlines(), ["Dana"])

    def test_duplicate_is_case_insensitive(self):
        cli(self.store, "member", "add", "Ana")
        r = cli(self.store, "member", "add", "ANA")
        self.assertEqual(r.returncode, 2)
        self.assertEqual(r.stdout.strip(), "")
        self.assertEqual(r.stderr.strip(), "error: member already exists: ANA")
        self.assertEqual(cli(self.store, "member", "list").stdout.splitlines(), ["Ana"])

    def test_empty_name_rejected(self):
        r = cli(self.store, "member", "add", "   ")
        self.assertEqual(r.returncode, 2)
        self.assertEqual(r.stderr.strip(), "error: member name cannot be empty")
        self.assertFalse(Path(self.store).exists() and json.loads(Path(self.store).read_text()).get("members"))

    def test_import_keeps_members(self):
        cli(self.store, "member", "add", "Ana")
        csv_path = self.dir / "s.csv"
        csv_path.write_text("date,amount,payee,memo\n2024-01-01,-5.00,Shop,x\n", encoding="utf-8")
        r = cli(self.store, "import", str(csv_path))
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(cli(self.store, "member", "list").stdout.splitlines(), ["Ana"])
        data = json.loads(Path(self.store).read_text(encoding="utf-8"))
        self.assertEqual(len(data["transactions"]), 1)

    def test_categorize_keeps_members(self):
        csv_path = self.dir / "s.csv"
        csv_path.write_text("date,amount,payee,memo\n2024-01-01,-5.00,Uber ride,x\n", encoding="utf-8")
        cli(self.store, "import", str(csv_path))
        cli(self.store, "member", "add", "Ana")
        r = cli(self.store, "categorize")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(cli(self.store, "member", "list").stdout.splitlines(), ["Ana"])


if __name__ == "__main__":
    unittest.main()
