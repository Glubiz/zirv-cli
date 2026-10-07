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


class TestBalance(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.store = str(Path(self._tmp.name) / "ledger.json")

    def members(self, *names):
        for n in names:
            cli(self.store, "member", "add", n)

    def add(self, payer, amount, split=None):
        args = ["expense", "add", "--payer", payer, "--amount", amount,
                "--desc", "x", "--date", "2024-03-01"]
        if split:
            args += ["--split", split]
        r = cli(self.store, *args)
        self.assertEqual(r.returncode, 0, r.stderr)

    def balance(self):
        r = cli(self.store, "balance")
        self.assertEqual(r.returncode, 0, r.stderr)
        return r.stdout.splitlines()

    def test_no_members(self):
        self.assertEqual(self.balance(), ["No members."])

    def test_members_without_expenses_are_zero(self):
        self.members("Ana", "Ben")
        self.assertEqual(self.balance(), ["Ana: 0.00", "Ben: 0.00"])

    def test_single_expense(self):
        self.members("Ana", "Ben", "Cara")
        self.add("Ana", "30.00")
        self.assertEqual(self.balance(), ["Ana: 20.00", "Ben: -10.00", "Cara: -10.00"])

    def test_balances_with_cent_rounding(self):
        self.members("Ana", "Ben", "Cara")
        self.add("Ben", "10.00")
        self.assertEqual(self.balance(), ["Ana: -3.34", "Ben: 6.67", "Cara: -3.33"])

    def test_multiple_expenses_net_out(self):
        self.members("Ana", "Ben")
        self.add("Ana", "10.00")
        self.add("Ben", "10.00")
        self.assertEqual(self.balance(), ["Ana: 0.00", "Ben: 0.00"])

    def test_weighted_expense(self):
        self.members("Ana", "Ben", "Cara")
        self.add("Cara", "30.00", "Ana=1,Ben=2")
        self.assertEqual(self.balance(), ["Ana: -10.00", "Ben: -20.00", "Cara: 30.00"])

    def test_balances_sum_to_zero(self):
        self.members("Ana", "Ben", "Cara")
        self.add("Ana", "12.34")
        self.add("Cara", "99.99", "Ben=3,Cara=2")
        self.add("Ben", "0.05")
        total = 0
        for line in self.balance():
            total += round(float(line.split(": ")[1]) * 100)
        self.assertEqual(total, 0)


if __name__ == "__main__":
    unittest.main()
