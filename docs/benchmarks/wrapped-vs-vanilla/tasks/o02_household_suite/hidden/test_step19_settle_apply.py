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


class TestSettleApply(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.store = str(Path(self._tmp.name) / "ledger.json")

    def ok(self, *args):
        r = cli(self.store, *args)
        self.assertEqual(r.returncode, 0, r.stderr)
        return r.stdout

    def members(self, *names):
        for n in names:
            self.ok("member", "add", n)

    def expense(self, payer, amount, split=None):
        args = ["expense", "add", "--payer", payer, "--amount", amount,
                "--desc", "x", "--date", "2024-03-01"]
        if split:
            args += ["--split", split]
        self.ok(*args)

    def data(self):
        return json.loads(Path(self.store).read_text(encoding="utf-8"))

    def test_apply_records_suggested_payments(self):
        self.members("Ana", "Ben", "Cara")
        self.expense("Ana", "30.00")
        suggested = self.ok("settle", "suggest").splitlines()
        self.assertEqual(suggested, ["Ben pays Ana 10.00", "Cara pays Ana 10.00"])
        out = self.ok("settle", "apply", "--date", "2024-04-01")
        self.assertEqual(out.strip(), "Settled 2 payments")
        self.assertEqual(self.ok("transfer", "list").splitlines(), [
            "1: 2024-04-01 10.00 Ben -> Ana (settle apply) [settles]",
            "2: 2024-04-01 10.00 Cara -> Ana (settle apply) [settles]",
        ])
        self.assertEqual(self.ok("balance").splitlines(), ["Ana: 0.00", "Ben: 0.00", "Cara: 0.00"])
        self.assertEqual(self.ok("settle", "suggest").strip(), "Everyone is settled up.")

    def test_stored_like_transfer_add(self):
        self.members("Ana", "Ben")
        self.expense("Ana", "10.00")
        self.ok("settle", "apply", "--date", "2024-04-01")
        self.assertEqual(self.data()["transfers"], [{
            "id": 1, "date": "2024-04-01", "note": "settle apply", "settles": True,
            "entries": [{"member": "Ben", "amount": -500}, {"member": "Ana", "amount": 500}],
        }])

    def test_same_order_as_suggest_for_complex_case(self):
        self.members("Ana", "Ben", "Cara", "Dan")
        self.expense("Ana", "50.00", "Cara=45,Dan=5")
        self.expense("Ben", "30.00", "Dan=1")
        self.ok("settle", "apply", "--date", "2024-04-01")
        self.assertEqual(self.ok("transfer", "list").splitlines(), [
            "1: 2024-04-01 45.00 Cara -> Ana (settle apply) [settles]",
            "2: 2024-04-01 30.00 Dan -> Ben (settle apply) [settles]",
            "3: 2024-04-01 5.00 Dan -> Ana (settle apply) [settles]",
        ])
        self.assertEqual(set(self.ok("balance").splitlines()),
                         {"Ana: 0.00", "Ben: 0.00", "Cara: 0.00", "Dan: 0.00"})

    def test_ids_continue_after_existing_transfers(self):
        self.members("Ana", "Ben", "Cara")
        self.expense("Ana", "30.00")
        self.ok("transfer", "add", "--from", "Cara", "--to", "Ben", "--amount", "1.00")
        out = self.ok("settle", "apply")
        self.assertEqual(out.strip(), "Settled 2 payments")
        ids = [t["id"] for t in self.data()["transfers"]]
        self.assertEqual(ids, [1, 2, 3])

    def test_nothing_to_settle(self):
        self.members("Ana", "Ben")
        before = self.ok("history")
        r = cli(self.store, "settle", "apply")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "Everyone is settled up.")
        self.assertEqual(self.ok("history"), before)
        self.assertEqual(self.ok("transfer", "list").strip(), "No transfers.")

    def test_one_history_entry_and_one_undo(self):
        self.members("Ana", "Ben", "Cara")
        self.expense("Ana", "30.00")
        self.ok("settle", "apply")
        self.assertEqual(self.ok("history").splitlines()[3:], ["4: expense add 1", "5: settle apply"])
        self.assertEqual(self.ok("undo").strip(), "Undid #5: settle apply")
        self.assertEqual(self.ok("transfer", "list").strip(), "No transfers.")
        self.assertEqual(self.ok("balance").splitlines(), ["Ana: 20.00", "Ben: -10.00", "Cara: -10.00"])

    def test_archived_member_with_balance_is_settled_too(self):
        self.members("Ana", "Ben", "Cara")
        self.expense("Ana", "30.00")
        self.ok("transfer", "add", "--from", "Cara", "--to", "Ana", "--amount", "10.00", "--settles")
        self.ok("member", "archive", "Cara")
        self.ok("expense", "edit", "1", "--amount", "60.00")
        self.assertEqual(self.ok("settle", "apply", "--date", "2024-04-01").strip(), "Settled 2 payments")
        self.assertEqual(self.ok("transfer", "list").splitlines()[1:], [
            "2: 2024-04-01 20.00 Ben -> Ana (settle apply) [settles]",
            "3: 2024-04-01 10.00 Cara -> Ana (settle apply) [settles]",
        ])
        self.assertEqual(self.ok("balance").splitlines(), ["Ana: 0.00", "Ben: 0.00"])

    def test_invalid_date_records_nothing(self):
        self.members("Ana", "Ben")
        self.expense("Ana", "10.00")
        before = self.data()
        r = cli(self.store, "settle", "apply", "--date", "soon")
        self.assertEqual(r.returncode, 2)
        self.assertEqual(r.stdout.strip(), "")
        self.assertEqual(r.stderr.strip(), "error: invalid date: soon")
        self.assertEqual(self.data(), before)


if __name__ == "__main__":
    unittest.main()
