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


class TestDecimalPlaces(unittest.TestCase):
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

    def data(self):
        return json.loads(Path(self.store).read_text(encoding="utf-8"))

    def expense(self, amount):
        return cli(self.store, "expense", "add", "--payer", "Ana", "--amount", amount,
                   "--desc", "x", "--date", "2024-03-01")

    def assert_rejected(self, r, text):
        self.assertEqual(r.returncode, 2, r.stdout)
        self.assertEqual(r.stdout.strip(), "")
        self.assertEqual(r.stderr.strip(), f"error: amount has more than 2 decimal places: {text}")

    def test_expense_add_rejects_extra_decimals(self):
        before = self.data()
        for text in ("10.005", "0.001", "7.123456", "3.141"):
            self.assert_rejected(self.expense(text), text)
        self.assertEqual(self.data(), before)
        self.assertEqual(self.ok("history").splitlines()[-1], "3: member add Cara")

    def test_whole_cents_are_accepted_however_written(self):
        expected = {"10": 1000, "10.5": 1050, "10.50": 1050, "10.500": 1050, "0.01": 1, "7.1": 710}
        for i, (text, cents) in enumerate(expected.items(), start=1):
            r = self.expense(text)
            self.assertEqual(r.returncode, 0, (text, r.stderr))
            self.assertEqual(self.data()["expenses"][i - 1]["amount"], cents, text)

    def test_other_amount_errors_unchanged(self):
        r = self.expense("abc")
        self.assertEqual((r.returncode, r.stderr.strip()), (2, "error: invalid amount: abc"))
        r = self.expense("0")
        self.assertEqual((r.returncode, r.stderr.strip()), (2, "error: amount must be positive"))
        r = self.expense("-5")
        self.assertEqual((r.returncode, r.stderr.strip()), (2, "error: amount must be positive"))

    def test_expense_edit_rejects_extra_decimals(self):
        self.assertEqual(self.expense("30.00").returncode, 0)
        before = self.data()
        r = cli(self.store, "expense", "edit", "1", "--amount", "30.001")
        self.assert_rejected(r, "30.001")
        self.assertEqual(self.data(), before)
        r = cli(self.store, "expense", "edit", "1", "--amount", "45.500")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.data()["expenses"][0]["amount"], 4550)

    def test_transfer_add_rejects_extra_decimals(self):
        before = self.data()
        for flags in ((), ("--settles",)):
            r = cli(self.store, "transfer", "add", "--from", "Ben", "--to", "Ana",
                    "--amount", "2.505", *flags)
            self.assert_rejected(r, "2.505")
        self.assertEqual(self.data(), before)
        r = cli(self.store, "transfer", "add", "--from", "Ben", "--to", "Ana", "--amount", "2.50")
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_decimals_checked_before_other_amount_rules(self):
        r = cli(self.store, "transfer", "add", "--from", "Ben", "--to", "Ana", "--amount", "0.001")
        self.assert_rejected(r, "0.001")

    def test_valid_amounts_still_work_end_to_end(self):
        self.assertEqual(self.expense("12.34").stdout.strip(), "Added expense 1: 12.34 paid by Ana")
        self.assertEqual(self.ok("expense", "show", "1").splitlines()[1:],
                         ["Ana: 4.12", "Ben: 4.11", "Cara: 4.11"])


if __name__ == "__main__":
    unittest.main()
