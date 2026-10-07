import copy
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


V1 = {
    "members": ["Ana", "Ben", "Cara"],
    "expenses": [{
        "id": 1, "date": "2024-03-01", "payer": "Ana", "amount": 3000,
        "description": "groceries", "weights": {"Ana": 1, "Ben": 1, "Cara": 1},
        "shares": {"Ana": 1000, "Ben": 1000, "Cara": 1000},
    }],
    "transfers": [{
        "id": 1, "date": "2024-03-02", "note": "pizza", "settles": True,
        "entries": [{"member": "Ben", "amount": -400}, {"member": "Ana", "amount": 400}],
    }],
    "transactions": [{
        "id": 1, "date": "2024-01-01", "amount": "-5.00", "payee": "Shop", "memo": "x", "category": None,
    }],
}


def v2_members(*names):
    return [{"name": n, "archived": False} for n in names]


class TestStoreV2(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.dir = Path(self._tmp.name)
        self.store = str(self.dir / "ledger.json")

    def ok(self, *args):
        r = cli(self.store, *args)
        self.assertEqual(r.returncode, 0, r.stderr)
        return r.stdout

    def write(self, payload):
        Path(self.store).write_text(json.dumps(payload, indent=2), encoding="utf-8")

    def raw(self):
        return Path(self.store).read_text(encoding="utf-8")

    def data(self):
        return json.loads(self.raw())

    def test_fresh_store_is_version_2(self):
        self.ok("member", "add", "Ana")
        self.ok("member", "add", "Ben")
        data = self.data()
        self.assertEqual(data["version"], 2)
        self.assertEqual(data["members"], v2_members("Ana", "Ben"))

    def test_import_stamps_version(self):
        csv_path = self.dir / "s.csv"
        csv_path.write_text("date,amount,payee,memo\n2024-01-01,-5.00,Shop,x\n", encoding="utf-8")
        self.ok("import", str(csv_path))
        self.assertEqual(self.data()["version"], 2)
        self.ok("categorize")
        self.assertEqual(self.data()["version"], 2)

    def test_v1_file_reads_like_v2(self):
        self.write(V1)
        self.assertEqual(self.ok("member", "list").splitlines(), ["Ana", "Ben", "Cara"])
        self.assertEqual(self.ok("balance").splitlines(), ["Ana: 16.00", "Ben: -6.00", "Cara: -10.00"])

    def test_v1_reads_do_not_rewrite_the_file(self):
        self.write(V1)
        before = self.raw()
        for args in (("member", "list"), ("balance",), ("expense", "list"), ("expense", "show", "1"),
                     ("transfer", "list"), ("account", "Ben"), ("report", "member", "Ana"),
                     ("settle", "suggest"), ("history",), ("undo",), ("list",), ("summary",)):
            cli(self.store, *args)
        self.assertEqual(self.raw(), before)

    def test_v1_balances_and_details(self):
        self.write(V1)
        # Ana paid 30, owes 10, received 4 from Ben -> +16; Ben owes 10, paid 4 -> -6; Cara -10
        self.assertEqual(self.ok("balance").splitlines(), ["Ana: 16.00", "Ben: -6.00", "Cara: -10.00"])
        self.assertEqual(self.ok("expense", "show", "1").splitlines()[1:],
                         ["Ana: 10.00", "Ben: 10.00", "Cara: 10.00"])
        self.assertEqual(self.ok("account", "Ben").splitlines(), ["T1: -4.00 Ana"])
        self.assertEqual(self.ok("settle", "suggest").splitlines(),
                         ["Cara pays Ana 10.00", "Ben pays Ana 6.00"])

    def test_write_migrates_and_keeps_everything_else(self):
        self.write(V1)
        self.ok("member", "add", "Dan")
        data = self.data()
        self.assertEqual(data["version"], 2)
        self.assertEqual(data["members"], v2_members("Ana", "Ben", "Cara", "Dan"))
        for key in ("expenses", "transfers", "transactions"):
            self.assertEqual(data[key], V1[key], key)

    def test_import_on_v1_file_migrates_members(self):
        self.write(V1)
        csv_path = self.dir / "s.csv"
        csv_path.write_text("date,amount,payee,memo\n2024-02-01,-9.00,Cafe,y\n", encoding="utf-8")
        self.ok("import", str(csv_path))
        data = self.data()
        self.assertEqual(data["version"], 2)
        self.assertEqual(data["members"], v2_members("Ana", "Ben", "Cara"))
        self.assertEqual(data["expenses"], V1["expenses"])
        self.assertEqual(len(data["transactions"]), 1)
        self.assertEqual(data["transactions"][0]["payee"], "Cafe")

    def test_explicit_version_1_is_v1(self):
        payload = copy.deepcopy(V1)
        payload["version"] = 1
        self.write(payload)
        self.assertEqual(self.ok("member", "list").splitlines(), ["Ana", "Ben", "Cara"])
        self.ok("expense", "add", "--payer", "Ben", "--amount", "3.00", "--desc", "x", "--date", "2024-03-03")
        data = self.data()
        self.assertEqual(data["version"], 2)
        self.assertEqual(data["members"], v2_members("Ana", "Ben", "Cara"))
        self.assertEqual(len(data["expenses"]), 2)

    def test_v2_file_loads_as_is(self):
        payload = copy.deepcopy(V1)
        payload["version"] = 2
        payload["members"] = v2_members("Ana", "Ben", "Cara")
        self.write(payload)
        self.assertEqual(self.ok("member", "list").splitlines(), ["Ana", "Ben", "Cara"])
        self.assertEqual(self.ok("balance").splitlines(), ["Ana: 16.00", "Ben: -6.00", "Cara: -10.00"])

    def test_undo_works_across_migration(self):
        self.write(V1)
        self.ok("member", "add", "Dan")
        self.assertEqual(self.ok("undo").strip(), "Undid #1: member add Dan")
        data = self.data()
        self.assertEqual(data["members"], v2_members("Ana", "Ben", "Cara"))
        self.assertEqual(self.ok("member", "list").splitlines(), ["Ana", "Ben", "Cara"])

    def test_newer_version_is_refused_everywhere(self):
        payload = copy.deepcopy(V1)
        payload["version"] = 3
        payload["members"] = v2_members("Ana", "Ben", "Cara")
        self.write(payload)
        before = self.raw()
        csv_path = self.dir / "s.csv"
        csv_path.write_text("date,amount,payee,memo\n2024-02-01,-9.00,Cafe,y\n", encoding="utf-8")
        for args in (("member", "list"), ("member", "add", "Dan"), ("balance",), ("history",),
                     ("undo",), ("import", str(csv_path)), ("list",), ("summary",), ("categorize",),
                     ("expense", "add", "--payer", "Ana", "--amount", "1", "--desc", "x")):
            r = cli(self.store, *args)
            self.assertEqual(r.returncode, 2, args)
            self.assertEqual(r.stdout.strip(), "", args)
            self.assertEqual(r.stderr.strip(), "error: unsupported store version: 3", args)
        self.assertEqual(self.raw(), before)


if __name__ == "__main__":
    unittest.main()
