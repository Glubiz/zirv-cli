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


class TestSettleSuggest(unittest.TestCase):
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

    def suggest(self):
        r = cli(self.store, "settle", "suggest")
        self.assertEqual(r.returncode, 0, r.stderr)
        return r.stdout.splitlines()

    def test_nobody_owes(self):
        self.members("Ana", "Ben")
        self.assertEqual(self.suggest(), ["Everyone is settled up."])

    def test_no_members(self):
        self.assertEqual(self.suggest(), ["Everyone is settled up."])

    def test_balanced_after_expenses(self):
        self.members("Ana", "Ben")
        self.add("Ana", "10.00")
        self.add("Ben", "10.00")
        self.assertEqual(self.suggest(), ["Everyone is settled up."])

    def test_ties_go_to_earlier_member(self):
        self.members("Ana", "Ben", "Cara")
        self.add("Ana", "30.00")
        self.assertEqual(self.suggest(), ["Ben pays Ana 10.00", "Cara pays Ana 10.00"])

    def test_creditor_ties_go_to_earlier_member(self):
        self.members("Ana", "Ben", "Cara")
        self.add("Ben", "10.00", "Ana=1")
        self.add("Cara", "10.00", "Ana=1")
        self.assertEqual(self.suggest(), ["Ana pays Ben 10.00", "Ana pays Cara 10.00"])

    def test_greedy_largest_first(self):
        self.members("Ana", "Ben", "Cara", "Dan")
        # Ana +50, Ben +30, Cara -45, Dan -35
        self.add("Ana", "50.00", "Cara=45,Dan=5")
        self.add("Ben", "30.00", "Dan=1")
        self.assertEqual(self.suggest(), [
            "Cara pays Ana 45.00",
            "Dan pays Ben 30.00",
            "Dan pays Ana 5.00",
        ])

    def test_cent_amounts(self):
        self.members("Ana", "Ben", "Cara")
        self.add("Ben", "10.00")
        # Ana -3.34, Ben +6.67, Cara -3.33
        self.assertEqual(self.suggest(), ["Ana pays Ben 3.34", "Cara pays Ben 3.33"])

    def test_does_not_modify_store(self):
        self.members("Ana", "Ben")
        self.add("Ana", "10.00")
        before = Path(self.store).read_text(encoding="utf-8")
        self.suggest()
        self.assertEqual(Path(self.store).read_text(encoding="utf-8"), before)

    def test_plan_brings_everyone_to_zero(self):
        self.members("Ana", "Ben", "Cara", "Dan")
        self.add("Ana", "12.34")
        self.add("Cara", "99.99", "Ben=3,Cara=2,Dan=7")
        self.add("Dan", "0.05")
        net = {}
        for line in cli(self.store, "balance").stdout.splitlines():
            name, amount = line.split(": ")
            net[name] = round(float(amount) * 100)
        for line in self.suggest():
            payer, rest = line.split(" pays ")
            receiver, amount = rest.rsplit(" ", 1)
            cents = round(float(amount) * 100)
            net[payer] += cents
            net[receiver] -= cents
        self.assertEqual(set(net.values()), {0})


if __name__ == "__main__":
    unittest.main()
