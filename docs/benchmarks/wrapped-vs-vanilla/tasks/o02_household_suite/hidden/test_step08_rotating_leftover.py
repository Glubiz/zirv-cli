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


class TestRotatingLeftover(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.store = str(Path(self._tmp.name) / "ledger.json")
        for n in ("Ana", "Ben", "Cara"):
            cli(self.store, "member", "add", n)

    def add(self, payer, amount, split=None):
        args = ["expense", "add", "--payer", payer, "--amount", amount,
                "--desc", "x", "--date", "2024-03-01"]
        if split:
            args += ["--split", split]
        r = cli(self.store, *args)
        self.assertEqual(r.returncode, 0, r.stderr)

    def shares(self, expense_id):
        r = cli(self.store, "expense", "show", str(expense_id))
        self.assertEqual(r.returncode, 0, r.stderr)
        return r.stdout.splitlines()[1:]

    def test_penny_rotates_through_participants(self):
        for _ in range(4):
            self.add("Ana", "0.01")
        self.assertEqual(self.shares(1), ["Ana: 0.01", "Ben: 0.00", "Cara: 0.00"])
        self.assertEqual(self.shares(2), ["Ana: 0.00", "Ben: 0.01", "Cara: 0.00"])
        self.assertEqual(self.shares(3), ["Ana: 0.00", "Ben: 0.00", "Cara: 0.01"])
        self.assertEqual(self.shares(4), ["Ana: 0.01", "Ben: 0.00", "Cara: 0.00"])

    def test_two_leftover_cents_wrap_around(self):
        self.add("Ana", "1.00")
        self.add("Ana", "1.00")
        self.add("Ana", "0.02")
        self.assertEqual(self.shares(3), ["Ana: 0.01", "Ben: 0.00", "Cara: 0.01"])

    def test_second_expense_starts_at_second_participant(self):
        self.add("Ana", "10.00")
        self.add("Ana", "10.00")
        self.assertEqual(self.shares(1), ["Ana: 3.34", "Ben: 3.33", "Cara: 3.33"])
        self.assertEqual(self.shares(2), ["Ana: 3.33", "Ben: 3.34", "Cara: 3.33"])

    def test_custom_split_rotates_in_listed_order(self):
        self.add("Ana", "10.00")
        self.add("Ana", "0.01", "Ben=1,Cara=1,Ana=1")
        self.assertEqual(self.shares(2), ["Ben: 0.00", "Cara: 0.01", "Ana: 0.00"])

    def test_weighted_split_uses_expense_id_offset(self):
        self.add("Ana", "1.00")
        self.add("Ana", "100.00", "Ana=1,Ben=2")
        # floors: 33.33 / 66.66, one leftover cent; expense 2, two participants -> starts at Ben
        self.assertEqual(self.shares(2), ["Ana: 33.33", "Ben: 66.67"])

    def test_offset_depends_on_participants_of_that_expense(self):
        for _ in range(2):
            self.add("Ana", "1.00")
        self.add("Ana", "0.01", "Ana=1,Ben=1")  # id 3, P=2 -> (3-1) % 2 = 0
        self.assertEqual(self.shares(3), ["Ana: 0.01", "Ben: 0.00"])

    def test_recorded_expenses_keep_their_shares(self):
        self.add("Ana", "10.00")
        before = self.shares(1)
        self.add("Ben", "10.00")
        self.add("Cara", "10.00")
        self.assertEqual(self.shares(1), before)
        data = json.loads(Path(self.store).read_text(encoding="utf-8"))
        for exp in data["expenses"]:
            self.assertEqual(sum(exp["shares"].values()), exp["amount"])

    def test_balances_follow_new_rule(self):
        self.add("Ana", "10.00")
        self.add("Ana", "10.00")
        # Ana paid 20.00, owes 3.34 + 3.33 -> +13.33; Ben owes 3.33 + 3.34; Cara 3.33 + 3.33
        self.assertEqual(cli(self.store, "balance").stdout.splitlines(),
                         ["Ana: 13.33", "Ben: -6.67", "Cara: -6.66"])


if __name__ == "__main__":
    unittest.main()
