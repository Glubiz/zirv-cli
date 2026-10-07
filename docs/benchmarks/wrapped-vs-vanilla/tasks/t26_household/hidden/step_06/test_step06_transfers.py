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


class TestTransfers(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.store = str(Path(self._tmp.name) / "ledger.json")
        for n in ("Ana", "Ben", "Cara"):
            cli(self.store, "member", "add", n)

    def transfer(self, sender, receiver, amount, note=None, date="2024-03-02"):
        args = ["transfer", "add", "--from", sender, "--to", receiver, "--amount", amount,
                "--date", date]
        if note is not None:
            args += ["--note", note]
        return cli(self.store, *args)

    def data(self):
        return json.loads(Path(self.store).read_text(encoding="utf-8"))

    def test_add_output_and_ids(self):
        r = self.transfer("Ben", "Ana", "10.00")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "Added transfer 1: 10.00 Ben -> Ana")
        r = self.transfer("ana", "CARA", "2.5")
        self.assertEqual(r.stdout.strip(), "Added transfer 2: 2.50 Ana -> Cara")

    def test_paired_entries_layout_superseded_by_13(self):
        self.transfer("Ben", "Ana", "10.25", note="pizza")
        t = self.data()["transfers"][0]
        self.assertEqual(t, {
            "id": 1, "date": "2024-03-02", "note": "pizza",
            "entries": [{"member": "Ben", "amount": -1025}, {"member": "Ana", "amount": 1025}],
        })

    def test_default_note_is_empty(self):
        self.transfer("Ben", "Ana", "1.00")
        self.assertEqual(self.data()["transfers"][0]["note"], "")

    def test_validation_errors(self):
        cases = [
            (("Zed", "Ana", "1.00"), "error: unknown member: Zed"),
            (("Ana", "Zed", "1.00"), "error: unknown member: Zed"),
            (("Zed", "Yan", "1.00"), "error: unknown member: Zed"),
            (("Ana", "ana", "1.00"), "error: cannot transfer to the same member"),
            (("Ana", "Ben", "abc"), "error: invalid amount: abc"),
            (("Ana", "Ben", "0"), "error: amount must be positive"),
            (("Ana", "Ben", "-3"), "error: amount must be positive"),
        ]
        for args, message in cases:
            r = self.transfer(*args)
            self.assertEqual(r.returncode, 2, args)
            self.assertEqual(r.stdout.strip(), "", args)
            self.assertEqual(r.stderr.strip(), message, args)
        r = self.transfer("Ana", "Ben", "1.00", date="nope")
        self.assertEqual(r.stderr.strip(), "error: invalid date: nope")
        self.assertNotIn("transfers", self.data())

    def test_list(self):
        self.assertEqual(cli(self.store, "transfer", "list").stdout.strip(), "No transfers.")
        self.transfer("Ben", "Ana", "10.00", note="pizza")
        self.transfer("Cara", "Ben", "3.5", date="2024-03-09")
        self.assertEqual(cli(self.store, "transfer", "list").stdout.splitlines(), [
            "1: 2024-03-02 10.00 Ben -> Ana (pizza)",
            "2: 2024-03-09 3.50 Cara -> Ben",
        ])

    def test_account_entries(self):
        self.transfer("Ben", "Ana", "10.00")
        self.transfer("Cara", "Ben", "3.50")
        self.transfer("Ana", "Cara", "1.25")
        self.assertEqual(cli(self.store, "account", "Ben").stdout.splitlines(),
                         ["T1: -10.00 Ana", "T2: 3.50 Cara"])
        self.assertEqual(cli(self.store, "account", "ana").stdout.splitlines(),
                         ["T1: 10.00 Ben", "T3: -1.25 Cara"])

    def test_account_empty_and_unknown(self):
        r = cli(self.store, "account", "Ben")
        self.assertEqual(r.stdout.strip(), "No entries.")
        r = cli(self.store, "account", "Zed")
        self.assertEqual(r.returncode, 2)
        self.assertEqual(r.stderr.strip(), "error: unknown member: Zed")

    def test_transfer_changes_balance_superseded_by_13(self):
        self.transfer("Ben", "Ana", "10.00")
        self.assertEqual(cli(self.store, "balance").stdout.splitlines(),
                         ["Ana: -10.00", "Ben: 10.00", "Cara: 0.00"])

    def test_transfer_settles_debt_superseded_by_13(self):
        cli(self.store, "expense", "add", "--payer", "Ana", "--amount", "30.00",
            "--desc", "x", "--date", "2024-03-01")
        self.transfer("Ben", "Ana", "10.00")
        self.assertEqual(cli(self.store, "balance").stdout.splitlines(),
                         ["Ana: 10.00", "Ben: 0.00", "Cara: -10.00"])
        self.assertEqual(cli(self.store, "settle", "suggest").stdout.splitlines(),
                         ["Cara pays Ana 10.00"])

    def test_transfers_survive_other_saves(self):
        self.transfer("Ben", "Ana", "10.00")
        cli(self.store, "member", "add", "Dan")
        cli(self.store, "expense", "add", "--payer", "Ana", "--amount", "4.00",
            "--desc", "x", "--date", "2024-03-01")
        self.assertEqual(len(self.data()["transfers"]), 1)


if __name__ == "__main__":
    unittest.main()
