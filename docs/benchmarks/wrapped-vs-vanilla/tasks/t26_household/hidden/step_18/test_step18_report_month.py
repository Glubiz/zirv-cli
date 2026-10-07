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


class TestReportMonth(unittest.TestCase):
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

    def expense(self, payer, amount, date, split=None):
        args = ["expense", "add", "--payer", payer, "--amount", amount,
                "--desc", "x", "--date", date]
        if split:
            args += ["--split", split]
        self.ok(*args)

    def month(self, text):
        return self.ok("report", "month", text).splitlines()

    def test_single_month(self):
        self.expense("Ana", "30.00", "2024-03-01")
        self.assertEqual(self.month("2024-03"), [
            "Month: 2024-03",
            "Ana: paid 30.00 share 10.00 net 20.00",
            "Ben: paid 0.00 share 10.00 net -10.00",
            "Cara: paid 0.00 share 10.00 net -10.00",
        ])

    def test_only_expenses_dated_in_that_month(self):
        self.expense("Ana", "30.00", "2024-03-31")
        self.expense("Ben", "60.00", "2024-04-01")
        self.expense("Ben", "3.00", "2024-04-30", "Ana=1,Ben=1")
        self.assertEqual(self.month("2024-04"), [
            "Month: 2024-04",
            "Ana: paid 0.00 share 21.50 net -21.50",
            "Ben: paid 63.00 share 21.50 net 41.50",
            "Cara: paid 0.00 share 20.00 net -20.00",
        ])
        self.assertEqual(self.month("2024-03")[1], "Ana: paid 30.00 share 10.00 net 20.00")

    def test_year_boundary_not_mixed(self):
        self.expense("Ana", "9.00", "2023-12-15")
        self.expense("Ana", "6.00", "2024-12-15")
        self.assertEqual(self.month("2024-12")[1], "Ana: paid 6.00 share 2.00 net 4.00")

    def test_edited_date_moves_expense_between_months(self):
        self.expense("Ana", "30.00", "2024-03-10")
        self.ok("expense", "edit", "1", "--date", "2024-05-02")
        self.assertEqual(self.month("2024-03"), ["No expenses in 2024-03."])
        self.assertEqual(self.month("2024-05")[1], "Ana: paid 30.00 share 10.00 net 20.00")

    def test_transfers_are_ignored(self):
        self.expense("Ana", "30.00", "2024-03-01")
        self.ok("transfer", "add", "--from", "Ben", "--to", "Ana", "--amount", "10.00",
                "--settles", "--date", "2024-03-02")
        self.assertEqual(self.month("2024-03")[2], "Ben: paid 0.00 share 10.00 net -10.00")

    def test_empty_month(self):
        self.assertEqual(self.month("2024-01"), ["No expenses in 2024-01."])
        self.expense("Ana", "30.00", "2024-03-01")
        self.assertEqual(self.month("2024-02"), ["No expenses in 2024-02."])

    def test_invalid_month(self):
        for text in ("2024-13", "2024-00", "24-03", "2024-3", "March", "2024/03", "2024-03-01"):
            r = cli(self.store, "report", "month", text)
            self.assertEqual(r.returncode, 2, text)
            self.assertEqual(r.stdout.strip(), "", text)
            self.assertEqual(r.stderr.strip(), f"error: invalid month: {text}", text)

    def test_archived_members_appear_only_when_involved(self):
        self.expense("Ana", "30.00", "2024-03-01")
        self.ok("transfer", "add", "--from", "Cara", "--to", "Ana", "--amount", "10.00", "--settles")
        self.ok("member", "archive", "Cara")
        self.expense("Ana", "10.00", "2024-04-01")
        self.assertEqual(self.month("2024-04"), [
            "Month: 2024-04",
            "Ana: paid 10.00 share 5.00 net 5.00",
            "Ben: paid 0.00 share 5.00 net -5.00",
        ])
        self.assertEqual(self.month("2024-03")[3], "Cara: paid 0.00 share 10.00 net -10.00")

    def test_active_member_with_nothing_still_listed(self):
        self.expense("Ana", "10.00", "2024-03-01", "Ana=1,Ben=1")
        self.assertEqual(self.month("2024-03")[3], "Cara: paid 0.00 share 0.00 net 0.00")

    def test_report_member_still_works(self):
        self.expense("Ana", "30.00", "2024-03-01")
        self.assertEqual(self.ok("report", "member", "Ana").splitlines()[0], "Member: Ana")


if __name__ == "__main__":
    unittest.main()
