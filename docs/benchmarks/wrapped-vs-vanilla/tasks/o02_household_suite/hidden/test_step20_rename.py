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


class TestRename(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.store = str(Path(self._tmp.name) / "ledger.json")
        for n in ("Ana", "Ben", "Cara"):
            self.ok("member", "add", n)

    def ok(self, *args):
        r = cli(self.store, *args)
        self.assertEqual(r.returncode, 0, r.stderr)
        return r.stdout

    def fail(self, message, *args):
        r = cli(self.store, *args)
        self.assertEqual(r.returncode, 2, (args, r.stdout))
        self.assertEqual(r.stdout.strip(), "", args)
        self.assertEqual(r.stderr.strip(), message, args)

    def data(self):
        return json.loads(Path(self.store).read_text(encoding="utf-8"))

    def populate(self):
        self.ok("expense", "add", "--payer", "Ben", "--amount", "30.00", "--desc", "groceries",
                "--date", "2024-03-01")
        self.ok("expense", "add", "--payer", "Cara", "--amount", "10.00", "--desc", "taxi",
                "--date", "2024-03-02", "--split", "Cara=1,Ben=2,Ana=1")
        self.ok("transfer", "add", "--from", "Ben", "--to", "Ana", "--amount", "4.00",
                "--settles", "--note", "pizza", "--date", "2024-03-03")
        self.ok("transfer", "add", "--from", "Cara", "--to", "Ben", "--amount", "1.50",
                "--date", "2024-03-04")

    def test_rename_output_and_list(self):
        out = self.ok("member", "rename", "ben", "  Benjamin ")
        self.assertEqual(out.strip(), "Renamed member Ben to Benjamin")
        self.assertEqual(self.ok("member", "list").splitlines(), ["Ana", "Benjamin", "Cara"])

    def test_everything_shows_new_name_with_same_numbers(self):
        self.populate()
        before = {
            "balance": self.ok("balance"),
            "suggest": self.ok("settle", "suggest"),
            "show1": self.ok("expense", "show", "1"),
            "show2": self.ok("expense", "show", "2"),
            "tlist": self.ok("transfer", "list"),
            "account": self.ok("account", "Ben"),
            "report": self.ok("report", "member", "Ben"),
            "month": self.ok("report", "month", "2024-03"),
            "elist": self.ok("expense", "list"),
        }
        self.ok("member", "rename", "Ben", "Benjamin")
        swap = lambda text: text.replace("Ben", "Benjamin")
        self.assertEqual(self.ok("balance"), swap(before["balance"]))
        self.assertEqual(self.ok("settle", "suggest"), swap(before["suggest"]))
        self.assertEqual(self.ok("expense", "show", "1"), swap(before["show1"]))
        self.assertEqual(self.ok("expense", "show", "2"), swap(before["show2"]))
        self.assertEqual(self.ok("transfer", "list"), swap(before["tlist"]))
        self.assertEqual(self.ok("account", "benjamin"), swap(before["account"]))
        self.assertEqual(self.ok("report", "member", "Benjamin"), swap(before["report"]))
        self.assertEqual(self.ok("report", "month", "2024-03"), swap(before["month"]))
        self.assertEqual(self.ok("expense", "list"), swap(before["elist"]))
        self.fail("error: unknown member: Ben", "account", "Ben")

    def test_stored_data_updated_in_place(self):
        self.populate()
        self.ok("member", "rename", "Cara", "Kara")
        data = self.data()
        self.assertEqual([m["name"] for m in data["members"]], ["Ana", "Ben", "Kara"])
        e2 = data["expenses"][1]
        self.assertEqual(e2["payer"], "Kara")
        self.assertEqual(list(e2["weights"].items()), [("Kara", 1), ("Ben", 2), ("Ana", 1)])
        self.assertEqual(list(e2["shares"]), ["Kara", "Ben", "Ana"])
        self.assertEqual(data["transfers"][1]["entries"],
                         [{"member": "Kara", "amount": -150}, {"member": "Ben", "amount": 150}])
        self.assertEqual(list(data["expenses"][0]["shares"].items()),
                         [("Ana", 1000), ("Ben", 1000), ("Kara", 1000)])

    def test_archived_flag_and_position_kept(self):
        self.ok("member", "archive", "Ben")
        self.ok("member", "rename", "Ben", "Benjamin")
        self.assertEqual(self.ok("member", "list", "--all").splitlines(),
                         ["Ana", "Benjamin (archived)", "Cara"])
        self.assertEqual(self.data()["members"][1], {"name": "Benjamin", "archived": True})

    def test_case_only_rename_is_allowed(self):
        self.assertEqual(self.ok("member", "rename", "Ana", "ANA").strip(), "Renamed member Ana to ANA")
        self.assertEqual(self.ok("member", "list").splitlines(), ["ANA", "Ben", "Cara"])

    def test_errors(self):
        self.ok("member", "archive", "Cara")
        self.fail("error: unknown member: Zed", "member", "rename", "Zed", "Zoe")
        self.fail("error: unknown member: Zed", "member", "rename", "Zed", "")
        self.fail("error: member name cannot be empty", "member", "rename", "Ana", "   ")
        self.fail("error: member already exists: ben", "member", "rename", "Ana", "ben")
        self.fail("error: member already exists: Cara", "member", "rename", "Ana", " Cara ")
        self.assertEqual(self.ok("member", "list", "--all").splitlines(), ["Ana", "Ben", "Cara (archived)"])

    def test_history_records_rename_and_is_never_rewritten(self):
        self.ok("member", "rename", "Ana", "Anna")
        self.assertEqual(self.ok("history").splitlines(), [
            "1: member add Ana", "2: member add Ben", "3: member add Cara", "4: member rename Ana Anna",
        ])

    def test_failed_rename_not_recorded(self):
        cli(self.store, "member", "rename", "Ana", "Ben")
        self.assertEqual(len(self.ok("history").splitlines()), 3)

    def test_undo_rename(self):
        self.populate()
        before = self.data()
        self.ok("member", "rename", "Ben", "Benjamin")
        self.assertEqual(self.ok("undo").strip(), "Undid #8: member rename Ben Benjamin")
        after = self.data()
        for key in ("members", "expenses", "transfers"):
            self.assertEqual(after[key], before[key], key)
        self.assertEqual(self.ok("member", "list").splitlines(), ["Ana", "Ben", "Cara"])

    def test_later_commands_use_new_name(self):
        self.ok("member", "rename", "Ben", "Benjamin")
        self.ok("expense", "add", "--payer", "benjamin", "--amount", "9.00", "--desc", "x",
                "--date", "2024-03-01")
        self.assertEqual(self.ok("expense", "show", "1").splitlines()[1:],
                         ["Ana: 3.00", "Benjamin: 3.00", "Cara: 3.00"])
        self.fail("error: unknown member: Ben", "expense", "add", "--payer", "Ben", "--amount", "1",
                  "--desc", "x")
        self.assertEqual(self.ok("history").splitlines()[-1], "5: expense add 1")


if __name__ == "__main__":
    unittest.main()
