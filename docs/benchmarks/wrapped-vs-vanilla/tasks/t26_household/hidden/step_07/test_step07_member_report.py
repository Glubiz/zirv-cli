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


class TestMemberReport(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.store = str(Path(self._tmp.name) / "ledger.json")
        for n in ("Ana", "Ben", "Cara"):
            cli(self.store, "member", "add", n)

    def expense(self, payer, amount, split=None):
        args = ["expense", "add", "--payer", payer, "--amount", amount,
                "--desc", "x", "--date", "2024-03-01"]
        if split:
            args += ["--split", split]
        r = cli(self.store, *args)
        self.assertEqual(r.returncode, 0, r.stderr)

    def transfer(self, sender, receiver, amount):
        r = cli(self.store, "transfer", "add", "--from", sender, "--to", receiver,
                "--amount", amount, "--date", "2024-03-02")
        self.assertEqual(r.returncode, 0, r.stderr)

    def report(self, name):
        r = cli(self.store, "report", "member", name)
        self.assertEqual(r.returncode, 0, r.stderr)
        return r.stdout.splitlines()

    def test_report_for_payer(self):
        self.expense("Ana", "30.00")
        self.assertEqual(self.report("Ana"), [
            "Member: Ana", "Paid: 30.00", "Share: 10.00",
            "Sent: 0.00", "Received: 0.00", "Net: 20.00",
        ])

    def test_report_for_debtor_with_cents(self):
        self.expense("Ana", "10.00")
        self.assertEqual(self.report("cara"), [
            "Member: Cara", "Paid: 0.00", "Share: 3.33",
            "Sent: 0.00", "Received: 0.00", "Net: -3.33",
        ])

    def test_report_empty_member(self):
        self.assertEqual(self.report("Ben"), [
            "Member: Ben", "Paid: 0.00", "Share: 0.00",
            "Sent: 0.00", "Received: 0.00", "Net: 0.00",
        ])

    def test_sent_and_received_totals_superseded_by_13(self):
        self.expense("Ana", "30.00")
        self.transfer("Ben", "Ana", "10.00")
        self.transfer("Ben", "Cara", "2.50")
        self.transfer("Cara", "Ben", "1.00")
        self.assertEqual(self.report("Ben")[3:5], ["Sent: 12.50", "Received: 1.00"])
        self.assertEqual(self.report("Ana")[3:5], ["Sent: 0.00", "Received: 10.00"])

    def test_net_always_matches_balance(self):
        self.expense("Ana", "12.34")
        self.expense("Cara", "99.99", "Ben=3,Cara=2")
        self.transfer("Ben", "Ana", "4.20")
        self.transfer("Ana", "Cara", "1.01")
        balance = dict(line.split(": ") for line in cli(self.store, "balance").stdout.splitlines())
        for name in ("Ana", "Ben", "Cara"):
            net = self.report(name)[-1]
            self.assertEqual(net, f"Net: {balance[name]}", name)

    def test_unknown_member(self):
        r = cli(self.store, "report", "member", "Zed")
        self.assertEqual(r.returncode, 2)
        self.assertEqual(r.stdout.strip(), "")
        self.assertEqual(r.stderr.strip(), "error: unknown member: Zed")


if __name__ == "__main__":
    unittest.main()
