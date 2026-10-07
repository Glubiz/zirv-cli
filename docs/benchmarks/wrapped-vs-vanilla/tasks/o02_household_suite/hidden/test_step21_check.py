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


def members(*names):
    return [{"name": n, "archived": False} for n in names]


CLEAN = {
    "version": 2,
    "members": members("Ana", "Ben", "Cara"),
    "expenses": [{
        "id": 1, "date": "2024-03-01", "payer": "Ana", "amount": 3000, "description": "x",
        "weights": {"Ana": 1, "Ben": 1, "Cara": 1},
        "shares": {"Ana": 1000, "Ben": 1000, "Cara": 1000},
    }],
    "transfers": [{
        "id": 1, "date": "2024-03-02", "note": "", "settles": True,
        "entries": [{"member": "Ben", "amount": -400}, {"member": "Ana", "amount": 400}],
    }],
}


class TestCheck(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.store = str(Path(self._tmp.name) / "ledger.json")

    def ok(self, *args):
        r = cli(self.store, *args)
        self.assertEqual(r.returncode, 0, r.stderr)
        return r.stdout

    def write(self, payload):
        Path(self.store).write_text(json.dumps(payload, indent=2), encoding="utf-8")

    def raw(self):
        return Path(self.store).read_text(encoding="utf-8")

    def check(self):
        r = cli(self.store, "check")
        return r.returncode, r.stdout.splitlines(), r.stderr.strip()

    def test_missing_and_empty_store_are_ok(self):
        self.assertEqual(self.check(), (0, ["ok"], ""))
        self.assertFalse(Path(self.store).exists())
        self.write({})
        self.assertEqual(self.check(), (0, ["ok"], ""))

    def test_clean_data_is_ok_and_file_untouched(self):
        self.write(CLEAN)
        before = self.raw()
        self.assertEqual(self.check(), (0, ["ok"], ""))
        self.assertEqual(self.raw(), before)

    def test_v1_file_is_checked_without_rewriting(self):
        legacy = copy.deepcopy(CLEAN)
        legacy.pop("version")
        legacy["members"] = ["Ana", "Ben", "Cara"]
        self.write(legacy)
        before = self.raw()
        self.assertEqual(self.check(), (0, ["ok"], ""))
        self.assertEqual(self.raw(), before)
        legacy["expenses"][0]["shares"]["Cara"] = 999
        self.write(legacy)
        self.assertEqual(self.check()[1], ["expense 1: shares total 29.99 but amount is 30.00"])

    def test_shares_total_mismatch(self):
        data = copy.deepcopy(CLEAN)
        data["expenses"][0]["shares"]["Ana"] = 1005
        self.write(data)
        self.assertEqual(self.check(), (1, ["expense 1: shares total 30.05 but amount is 30.00"], ""))

    def test_unknown_members_in_expense(self):
        data = copy.deepcopy(CLEAN)
        exp = data["expenses"][0]
        exp["payer"] = "ana"
        exp["weights"] = {"Ana": 1, "Zed": 1, "Cara": 1, "Yan": 1}
        exp["shares"] = {"Ana": 1000, "Zed": 1000, "Cara": 500, "Xia": 500}
        self.write(data)
        self.assertEqual(self.check(), (1, [
            "expense 1: unknown member ana",
            "expense 1: unknown member Zed",
            "expense 1: unknown member Yan",
            "expense 1: unknown member Xia",
        ], ""))

    def test_mismatch_line_comes_before_unknown_lines(self):
        data = copy.deepcopy(CLEAN)
        exp = data["expenses"][0]
        exp["shares"] = {"Ana": 1000, "Zed": 1000}
        self.write(data)
        self.assertEqual(self.check()[1], [
            "expense 1: shares total 20.00 but amount is 30.00",
            "expense 1: unknown member Zed",
        ])

    def test_transfer_problems(self):
        data = copy.deepcopy(CLEAN)
        data["transfers"] = [
            {"id": 1, "date": "2024-03-02", "note": "", "settles": True,
             "entries": [{"member": "Ben", "amount": -400}, {"member": "Ana", "amount": 300}]},
            {"id": 2, "date": "2024-03-02", "note": "", "settles": False,
             "entries": [{"member": "Ben", "amount": -400}]},
            {"id": 3, "date": "2024-03-02", "note": "", "settles": False,
             "entries": [{"member": "Zed", "amount": -100}, {"member": "Ana", "amount": 100}]},
            {"id": 4, "date": "2024-03-02", "note": "", "settles": False,
             "entries": [{"member": "Zed", "amount": -100}, {"member": "Zed", "amount": 50}]},
        ]
        self.write(data)
        self.assertEqual(self.check(), (1, [
            "transfer 1: entries do not balance",
            "transfer 2: entries do not balance",
            "transfer 3: unknown member Zed",
            "transfer 4: entries do not balance",
            "transfer 4: unknown member Zed",
        ], ""))

    def test_expenses_are_reported_before_transfers_in_stored_order(self):
        data = copy.deepcopy(CLEAN)
        second = copy.deepcopy(data["expenses"][0])
        second["id"] = 2
        second["shares"]["Ben"] = 0
        data["expenses"].insert(0, second)
        data["transfers"][0]["entries"][1]["amount"] = 1
        self.write(data)
        self.assertEqual(self.check()[1], [
            "expense 2: shares total 20.00 but amount is 30.00",
            "transfer 1: entries do not balance",
        ])

    def test_archived_members_are_known(self):
        data = copy.deepcopy(CLEAN)
        data["members"][2]["archived"] = True
        self.write(data)
        self.assertEqual(self.check(), (0, ["ok"], ""))

    def test_check_does_not_touch_history(self):
        self.ok("member", "add", "Ana")
        before = self.raw()
        self.check()
        self.assertEqual(self.raw(), before)
        self.assertEqual(self.ok("history").splitlines(), ["1: member add Ana"])

    def test_everything_the_other_commands_do_stays_consistent(self):
        for n in ("Ana", "Ben", "Cara", "Dan"):
            self.ok("member", "add", n)
        self.ok("expense", "add", "--payer", "Ana", "--amount", "10.00", "--desc", "a", "--date", "2024-03-01")
        self.ok("expense", "add", "--payer", "Ben", "--amount", "0.01", "--desc", "b", "--date", "2024-03-02")
        self.ok("expense", "add", "--payer", "Cara", "--amount", "99.99", "--desc", "c",
                "--date", "2024-03-03", "--split", "Dan=3,Cara=2,Ana=7")
        self.ok("expense", "add", "--payer", "Dan", "--amount", "5.55", "--desc", "d",
                "--date", "2024-03-04", "--split", "Ben=1,Cara=1")
        self.ok("expense", "edit", "3", "--amount", "100.01")
        self.ok("expense", "edit", "2", "--amount", "7.07")
        self.ok("transfer", "add", "--from", "Ana", "--to", "Ben", "--amount", "3.33", "--settles")
        self.ok("transfer", "add", "--from", "Dan", "--to", "Cara", "--amount", "1.00")
        self.ok("member", "rename", "Dan", "Daniel")
        self.assertEqual(self.check(), (0, ["ok"], ""))
        self.ok("settle", "apply")
        self.ok("member", "archive", "Ben")
        self.ok("member", "archive", "Cara")
        self.ok("member", "rename", "Ana", "Anna")
        self.ok("expense", "add", "--payer", "Anna", "--amount", "10.01", "--desc", "e", "--date", "2024-04-01")
        self.assertEqual(self.check(), (0, ["ok"], ""))
        for _ in range(4):
            self.ok("undo")
        self.assertEqual(self.check(), (0, ["ok"], ""))


if __name__ == "__main__":
    unittest.main()
