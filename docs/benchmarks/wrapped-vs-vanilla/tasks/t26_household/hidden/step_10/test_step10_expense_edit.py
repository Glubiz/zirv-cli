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


class TestExpenseEdit(unittest.TestCase):
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

    def add(self, payer, amount, split=None, date="2024-03-01"):
        args = ["expense", "add", "--payer", payer, "--amount", amount,
                "--desc", "x", "--date", date]
        if split:
            args += ["--split", split]
        return self.ok(*args)

    def show(self, expense_id):
        return self.ok("expense", "show", str(expense_id)).splitlines()

    def data(self):
        return json.loads(Path(self.store).read_text(encoding="utf-8"))

    def test_edit_amount_resplits_equally(self):
        self.add("Ana", "30.00")
        out = self.ok("expense", "edit", "1", "--amount", "60.00")
        self.assertEqual(out.strip(), "Updated expense 1")
        self.assertEqual(self.show(1), [
            "Expense 1: 60.00 paid by Ana on 2024-03-01 (x)",
            "Ana: 20.00", "Ben: 20.00", "Cara: 20.00",
        ])

    def test_edit_uses_original_weights_and_participants(self):
        self.add("Ana", "3.00", "Cara=1,Ben=2")
        self.ok("member", "add", "Dan")
        self.ok("expense", "edit", "1", "--amount", "100.00")
        # weights 1:2 -> 33.33 / 66.66, one leftover cent; expense 1 starts at first listed
        self.assertEqual(self.show(1)[1:], ["Cara: 33.34", "Ben: 66.66"])
        self.assertEqual(self.data()["expenses"][0]["weights"], {"Cara": 1, "Ben": 2})

    def test_edit_leftover_follows_expense_id(self):
        self.add("Ana", "10.00")
        self.add("Ana", "10.00")
        self.assertEqual(self.show(2)[1:], ["Ana: 3.33", "Ben: 3.34", "Cara: 3.33"])
        self.ok("expense", "edit", "2", "--amount", "10.01")
        self.assertEqual(self.show(2)[1:], ["Ana: 3.33", "Ben: 3.34", "Cara: 3.34"])

    def test_edit_without_amount_keeps_shares(self):
        self.add("Ana", "10.00", date="2024-03-01")
        self.add("Ana", "10.00")
        before = self.data()["expenses"][1]["shares"]
        self.ok("expense", "edit", "2", "--payer", "ben", "--desc", "new text", "--date", "2024-04-05")
        exp = self.data()["expenses"][1]
        self.assertEqual(exp["payer"], "Ben")
        self.assertEqual(exp["description"], "new text")
        self.assertEqual(exp["date"], "2024-04-05")
        self.assertEqual(exp["shares"], before)
        self.assertEqual(exp["amount"], 1000)

    def test_balance_reflects_edit(self):
        self.add("Ana", "30.00")
        self.ok("expense", "edit", "1", "--amount", "90.00")
        self.assertEqual(self.ok("balance").splitlines(),
                         ["Ana: 60.00", "Ben: -30.00", "Cara: -30.00"])

    def test_nothing_to_edit(self):
        self.add("Ana", "30.00")
        r = cli(self.store, "expense", "edit", "1")
        self.assertEqual(r.returncode, 2)
        self.assertEqual(r.stdout.strip(), "")
        self.assertEqual(r.stderr.strip(), "error: nothing to edit")

    def test_unknown_expense(self):
        r = cli(self.store, "expense", "edit", "5", "--amount", "1.00")
        self.assertEqual(r.returncode, 2)
        self.assertEqual(r.stderr.strip(), "error: no such expense: 5")

    def test_invalid_values_leave_expense_unchanged(self):
        self.add("Ana", "30.00")
        before = self.data()
        cases = [
            (("--payer", "Zed"), "error: unknown member: Zed"),
            (("--amount", "abc"), "error: invalid amount: abc"),
            (("--amount", "0"), "error: amount must be positive"),
            (("--date", "2024-99-01"), "error: invalid date: 2024-99-01"),
            (("--desc", "changed", "--amount", "-1"), "error: amount must be positive"),
        ]
        for args, message in cases:
            r = cli(self.store, "expense", "edit", "1", *args)
            self.assertEqual(r.returncode, 2, args)
            self.assertEqual(r.stderr.strip(), message, args)
        self.assertEqual(self.data(), before)

    def test_edit_is_recorded_and_undoable(self):
        self.add("Ana", "30.00")
        self.ok("expense", "edit", "1", "--amount", "60.00")
        self.assertEqual(self.ok("history").splitlines()[-1], "5: expense edit 1")
        self.assertEqual(self.ok("undo").strip(), "Undid #5: expense edit 1")
        self.assertEqual(self.show(1)[0], "Expense 1: 30.00 paid by Ana on 2024-03-01 (x)")
        self.assertEqual(self.show(1)[1:], ["Ana: 10.00", "Ben: 10.00", "Cara: 10.00"])

    def test_failed_edit_not_recorded(self):
        self.add("Ana", "30.00")
        cli(self.store, "expense", "edit", "1", "--amount", "x")
        self.assertEqual(self.ok("history").splitlines()[-1], "4: expense add 1")


if __name__ == "__main__":
    unittest.main()
