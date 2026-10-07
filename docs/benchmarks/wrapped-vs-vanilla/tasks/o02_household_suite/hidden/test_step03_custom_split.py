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


class TestCustomSplit(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.store = str(Path(self._tmp.name) / "ledger.json")
        for n in ("Ana", "Ben", "Cara"):
            cli(self.store, "member", "add", n)

    def add(self, payer, amount, split=None, desc="stuff"):
        args = ["expense", "add", "--payer", payer, "--amount", amount,
                "--desc", desc, "--date", "2024-03-01"]
        if split is not None:
            args += ["--split", split]
        return cli(self.store, *args)

    def data(self):
        return json.loads(Path(self.store).read_text(encoding="utf-8"))

    def shares(self, expense_id=1):
        r = cli(self.store, "expense", "show", str(expense_id))
        self.assertEqual(r.returncode, 0, r.stderr)
        return r.stdout.splitlines()[1:]

    def test_weighted_shares(self):
        r = self.add("Ana", "30.00", "Ana=1,Ben=2")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "Added expense 1: 30.00 paid by Ana")
        self.assertEqual(self.shares(), ["Ana: 10.00", "Ben: 20.00"])

    def test_only_listed_members_participate(self):
        self.add("Cara", "9.00", "Cara=1,Ana=1")
        self.assertEqual(self.shares(), ["Cara: 4.50", "Ana: 4.50"])

    def test_leftover_goes_to_first_listed(self):
        self.add("Ana", "100.00", "Ana=1,Ben=2")
        self.assertEqual(self.shares(), ["Ana: 33.34", "Ben: 66.66"])

    def test_listed_order_is_participant_order(self):
        self.add("Ana", "100.00", "Ben=2,Ana=1")
        self.assertEqual(self.shares(), ["Ben: 66.67", "Ana: 33.33"])

    def test_equal_weights_leftover_in_listed_order(self):
        self.add("Ana", "0.02", "Cara=1,Ben=1,Ana=1")
        self.assertEqual(self.shares(), ["Cara: 0.01", "Ben: 0.01", "Ana: 0.00"])

    def test_payer_need_not_participate(self):
        self.add("Cara", "10.00", "Ana=1,Ben=1")
        self.assertEqual(self.shares(), ["Ana: 5.00", "Ben: 5.00"])

    def test_names_case_insensitive_stored_canonical(self):
        r = self.add("Ana", "10.00", "ben=3,ANA=2")
        self.assertEqual(r.returncode, 0, r.stderr)
        exp = self.data()["expenses"][0]
        self.assertEqual(list(exp["weights"].items()), [("Ben", 3), ("Ana", 2)])
        self.assertEqual(list(exp["shares"].items()), [("Ben", 600), ("Ana", 400)])
        self.assertEqual(sum(exp["shares"].values()), exp["amount"])

    def test_unknown_member(self):
        r = self.add("Ana", "10.00", "Ana=1,Zed=1")
        self.assertEqual(r.returncode, 2)
        self.assertEqual(r.stdout.strip(), "")
        self.assertEqual(r.stderr.strip(), "error: unknown member: Zed")
        self.assertNotIn("expenses", self.data())

    def test_invalid_split_text(self):
        for bad in ("Ana", "Ana=0", "Ana=-1", "Ana=x,Ben=1", "Ana=1.5", "Ana=1,,Ben=1"):
            r = self.add("Ana", "10.00", bad)
            self.assertEqual(r.returncode, 2, bad)
            self.assertEqual(r.stderr.strip(), f"error: invalid split: {bad}", bad)
        self.assertNotIn("expenses", self.data())

    def test_duplicate_member(self):
        r = self.add("Ana", "10.00", "Ana=1,ana=2")
        self.assertEqual(r.returncode, 2)
        self.assertEqual(r.stderr.strip(), "error: duplicate member in split: ana")

    def test_without_split_unchanged(self):
        self.add("Ana", "10.00")
        self.assertEqual(self.shares(), ["Ana: 3.34", "Ben: 3.33", "Cara: 3.33"])
        self.assertEqual(self.data()["expenses"][0]["weights"], {"Ana": 1, "Ben": 1, "Cara": 1})


if __name__ == "__main__":
    unittest.main()
