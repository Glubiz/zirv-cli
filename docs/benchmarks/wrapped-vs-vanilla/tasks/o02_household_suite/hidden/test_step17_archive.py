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


class TestArchive(unittest.TestCase):
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

    def expense(self, payer, amount, split=None):
        args = ["expense", "add", "--payer", payer, "--amount", amount,
                "--desc", "x", "--date", "2024-03-01"]
        if split:
            args += ["--split", split]
        return self.ok(*args)

    def data(self):
        return json.loads(Path(self.store).read_text(encoding="utf-8"))

    def test_archive_and_list(self):
        self.assertEqual(self.ok("member", "archive", "cara").strip(), "Archived member Cara")
        self.assertEqual(self.ok("member", "list").splitlines(), ["Ana", "Ben"])
        self.assertEqual(self.ok("member", "list", "--all").splitlines(), ["Ana", "Ben", "Cara (archived)"])
        self.assertEqual(self.data()["members"][2], {"name": "Cara", "archived": True})
        self.assertEqual(self.data()["members"][0], {"name": "Ana", "archived": False})

    def test_archive_middle_member_keeps_order_in_all(self):
        self.ok("member", "archive", "Ben")
        self.assertEqual(self.ok("member", "list", "--all").splitlines(),
                         ["Ana", "Ben (archived)", "Cara"])

    def test_no_members_left(self):
        for n in ("Ana", "Ben", "Cara"):
            self.ok("member", "archive", n)
        self.assertEqual(self.ok("member", "list").strip(), "No members.")

    def test_archive_errors(self):
        self.fail("error: unknown member: Zed", "member", "archive", "Zed")
        self.ok("member", "archive", "Cara")
        self.fail("error: member already archived: Cara", "member", "archive", "cara")
        self.expense("Ana", "30.00")
        self.fail("error: member has a non-zero balance: Ana", "member", "archive", "Ana")
        self.fail("error: member has a non-zero balance: Ben", "member", "archive", "Ben")
        self.assertFalse(self.data()["members"][0]["archived"])

    def test_archived_name_stays_reserved(self):
        self.ok("member", "archive", "Cara")
        self.fail("error: member already exists: CARA", "member", "add", "CARA")

    def test_new_expenses_skip_archived_members(self):
        self.ok("member", "archive", "Cara")
        self.expense("Ana", "10.00")
        self.assertEqual(self.ok("expense", "show", "1").splitlines()[1:], ["Ana: 5.00", "Ben: 5.00"])
        self.expense("Ana", "0.01")  # expense 2 over two active members starts at the second one
        self.assertEqual(self.ok("expense", "show", "2").splitlines()[1:], ["Ana: 0.00", "Ben: 0.01"])
        self.assertEqual(list(self.data()["expenses"][0]["weights"]), ["Ana", "Ben"])

    def test_archived_member_cannot_be_used(self):
        self.ok("member", "archive", "Cara")
        self.fail("error: member is archived: Cara", "expense", "add", "--payer", "Cara",
                  "--amount", "5.00", "--desc", "x")
        self.fail("error: member is archived: Cara", "expense", "add", "--payer", "Ana",
                  "--amount", "5.00", "--desc", "x", "--split", "Ana=1,cara=1")
        self.fail("error: member is archived: Cara", "transfer", "add", "--from", "Cara",
                  "--to", "Ana", "--amount", "1.00")
        self.fail("error: member is archived: Cara", "transfer", "add", "--from", "Ana",
                  "--to", "Cara", "--amount", "1.00")
        self.expense("Ana", "10.00")
        self.fail("error: member is archived: Cara", "expense", "edit", "1", "--payer", "Cara")
        self.assertEqual(len(self.data()["expenses"]), 1)

    def test_check_order_for_transfers(self):
        self.ok("member", "archive", "Cara")
        self.fail("error: cannot transfer to the same member", "transfer", "add", "--from", "Cara",
                  "--to", "cara", "--amount", "1.00")
        self.fail("error: member is archived: Cara", "transfer", "add", "--from", "Cara",
                  "--to", "Ana", "--amount", "abc")
        self.fail("error: unknown member: Zed", "transfer", "add", "--from", "Cara",
                  "--to", "Zed", "--amount", "1.00")

    def test_balance_hides_archived_zero_balances(self):
        self.expense("Ana", "30.00")
        self.ok("transfer", "add", "--from", "Cara", "--to", "Ana", "--amount", "10.00", "--settles")
        self.ok("member", "archive", "Cara")
        self.assertEqual(self.ok("balance").splitlines(), ["Ana: 10.00", "Ben: -10.00"])

    def test_balance_shows_archived_member_with_nonzero_balance(self):
        self.expense("Ana", "30.00")
        self.ok("transfer", "add", "--from", "Cara", "--to", "Ana", "--amount", "10.00", "--settles")
        self.ok("member", "archive", "Cara")
        self.ok("expense", "edit", "1", "--amount", "60.00")
        self.assertEqual(self.ok("balance").splitlines(), ["Ana: 30.00", "Ben: -20.00", "Cara: -10.00"])
        self.assertEqual(self.ok("settle", "suggest").splitlines(),
                         ["Ben pays Ana 20.00", "Cara pays Ana 10.00"])
        self.assertEqual(self.data()["expenses"][0]["shares"],
                         {"Ana": 2000, "Ben": 2000, "Cara": 2000})

    def test_reports_still_work_for_archived(self):
        self.ok("member", "archive", "Cara")
        self.assertEqual(self.ok("report", "member", "cara").splitlines()[0], "Member: Cara")
        self.assertEqual(self.ok("account", "Cara").strip(), "No entries.")

    def test_archive_is_recorded_and_undoable(self):
        self.ok("member", "archive", "Cara")
        self.assertEqual(self.ok("history").splitlines()[-1], "4: member archive Cara")
        self.assertEqual(self.ok("undo").strip(), "Undid #4: member archive Cara")
        self.assertEqual(self.ok("member", "list").splitlines(), ["Ana", "Ben", "Cara"])
        self.assertFalse(self.data()["members"][2]["archived"])

    def test_failed_archive_not_recorded(self):
        self.expense("Ana", "30.00")
        cli(self.store, "member", "archive", "Ana")
        cli(self.store, "member", "archive", "Zed")
        self.assertEqual(self.ok("history").splitlines()[-1], "4: expense add 1")


if __name__ == "__main__":
    unittest.main()
