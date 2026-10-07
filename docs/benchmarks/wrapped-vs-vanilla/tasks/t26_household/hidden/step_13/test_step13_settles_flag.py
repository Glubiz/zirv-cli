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


class TestSettlesFlag(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.dir = Path(self._tmp.name)
        self.store = str(self.dir / "ledger.json")
        for n in ("Ana", "Ben", "Cara"):
            self.ok("member", "add", n)

    def ok(self, *args):
        r = cli(self.store, *args)
        self.assertEqual(r.returncode, 0, r.stderr)
        return r.stdout

    def transfer(self, sender, receiver, amount, *extra):
        return self.ok("transfer", "add", "--from", sender, "--to", receiver,
                       "--amount", amount, "--date", "2024-03-02", *extra)

    def expense(self, payer, amount):
        self.ok("expense", "add", "--payer", payer, "--amount", amount,
                "--desc", "x", "--date", "2024-03-01")

    def data(self):
        return json.loads(Path(self.store).read_text(encoding="utf-8"))

    def test_plain_transfer_does_not_move_balances(self):
        self.expense("Ana", "30.00")
        before_balance = self.ok("balance")
        before_suggest = self.ok("settle", "suggest")
        out = self.transfer("Ben", "Ana", "10.00")
        self.assertEqual(out.strip(), "Added transfer 1: 10.00 Ben -> Ana")
        self.assertEqual(self.ok("balance"), before_balance)
        self.assertEqual(self.ok("settle", "suggest"), before_suggest)

    def test_settling_transfer_moves_balances(self):
        self.expense("Ana", "30.00")
        out = self.transfer("Ben", "Ana", "10.00", "--settles")
        self.assertEqual(out.strip(), "Added transfer 1: 10.00 Ben -> Ana")
        self.assertEqual(self.ok("balance").splitlines(),
                         ["Ana: 10.00", "Ben: 0.00", "Cara: -10.00"])
        self.assertEqual(self.ok("settle", "suggest").splitlines(), ["Cara pays Ana 10.00"])

    def test_report_member_counts_only_settling(self):
        self.expense("Ana", "30.00")
        self.transfer("Ben", "Ana", "10.00", "--settles")
        self.transfer("Ben", "Cara", "7.00")
        self.transfer("Cara", "Ben", "2.00")
        self.assertEqual(self.ok("report", "member", "Ben").splitlines(), [
            "Member: Ben", "Paid: 0.00", "Share: 10.00",
            "Sent: 10.00", "Received: 0.00", "Net: 0.00",
        ])
        self.assertEqual(self.ok("report", "member", "Cara").splitlines()[3:],
                         ["Sent: 0.00", "Received: 0.00", "Net: -10.00"])

    def test_stored_flag(self):
        self.transfer("Ben", "Ana", "1.00")
        self.transfer("Ben", "Ana", "2.00", "--settles")
        t1, t2 = self.data()["transfers"]
        self.assertIs(t1["settles"], False)
        self.assertIs(t2["settles"], True)
        self.assertEqual(t2["entries"], [{"member": "Ben", "amount": -200},
                                         {"member": "Ana", "amount": 200}])

    def test_list_marks_settling_transfers(self):
        self.transfer("Ben", "Ana", "10.00", "--note", "pizza", "--settles")
        self.transfer("Cara", "Ben", "3.50", "--note", "ticket")
        self.transfer("Ana", "Cara", "1.00", "--settles")
        self.transfer("Ana", "Ben", "2.00")
        self.assertEqual(self.ok("transfer", "list").splitlines(), [
            "1: 2024-03-02 10.00 Ben -> Ana (pizza) [settles]",
            "2: 2024-03-02 3.50 Cara -> Ben (ticket)",
            "3: 2024-03-02 1.00 Ana -> Cara [settles]",
            "4: 2024-03-02 2.00 Ana -> Ben",
        ])

    def test_account_shows_all_transfers(self):
        self.transfer("Ben", "Ana", "10.00")
        self.transfer("Ben", "Ana", "1.00", "--settles")
        self.assertEqual(self.ok("account", "Ana").splitlines(), ["T1: 10.00 Ben", "T2: 1.00 Ben"])

    def test_legacy_transfers_without_flag_count_as_settling(self):
        Path(self.store).write_text(json.dumps({
            "members": ["Ana", "Ben", "Cara"],
            "expenses": [{
                "id": 1, "date": "2024-03-01", "payer": "Ana", "amount": 3000,
                "description": "x", "weights": {"Ana": 1, "Ben": 1, "Cara": 1},
                "shares": {"Ana": 1000, "Ben": 1000, "Cara": 1000},
            }],
            "transfers": [{
                "id": 1, "date": "2024-03-02", "note": "old",
                "entries": [{"member": "Ben", "amount": -1000}, {"member": "Ana", "amount": 1000}],
            }],
        }), encoding="utf-8")
        self.assertEqual(self.ok("balance").splitlines(),
                         ["Ana: 10.00", "Ben: 0.00", "Cara: -10.00"])
        self.assertEqual(self.ok("transfer", "list").splitlines(),
                         ["1: 2024-03-02 10.00 Ben -> Ana (old) [settles]"])
        self.assertEqual(self.ok("report", "member", "Ben").splitlines()[3:],
                         ["Sent: 10.00", "Received: 0.00", "Net: 0.00"])

    def test_history_label_unchanged_and_undoable(self):
        self.transfer("Ben", "Ana", "10.00", "--settles")
        self.assertEqual(self.ok("history").splitlines()[-1], "4: transfer add 1")
        self.assertEqual(self.ok("undo").strip(), "Undid #4: transfer add 1")
        self.assertEqual(self.ok("transfer", "list").strip(), "No transfers.")


if __name__ == "__main__":
    unittest.main()
