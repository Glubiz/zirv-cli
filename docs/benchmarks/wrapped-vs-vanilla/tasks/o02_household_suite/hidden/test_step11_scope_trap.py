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


CSV = (
    "date,amount,payee,memo,category\n"
    "2024-01-01,-5.00,Shop A,m1,\n"
    "2024-01-02,-6.00,Shop B,m2,\n"
    "2024-01-03,-7.00,corner market,m3,\n"
    "2024-01-04,-8.00,Uber ride,m4,\n"
    "2024-01-05,100.00,Employer Payroll,m5,\n"
)


class TestExpenseListPayer(unittest.TestCase):
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

    def add(self, payer, amount, desc, date):
        self.ok("expense", "add", "--payer", payer, "--amount", amount, "--desc", desc, "--date", date)

    def test_filter_by_payer(self):
        self.add("Ana", "30.00", "groceries", "2024-03-01")
        self.add("Ben", "4.50", "cinema", "2024-03-05")
        self.add("Ana", "12.00", "bus", "2024-03-06")
        self.assertEqual(self.ok("expense", "list", "--payer", "Ana").splitlines(), [
            "1: 2024-03-01 30.00 Ana groceries",
            "3: 2024-03-06 12.00 Ana bus",
        ])
        self.assertEqual(self.ok("expense", "list", "--payer", "ben").splitlines(), [
            "2: 2024-03-05 4.50 Ben cinema",
        ])

    def test_payer_with_no_expenses(self):
        self.add("Ana", "30.00", "groceries", "2024-03-01")
        self.assertEqual(self.ok("expense", "list", "--payer", "Cara").strip(), "No expenses.")

    def test_unknown_payer(self):
        r = cli(self.store, "expense", "list", "--payer", "Zed")
        self.assertEqual(r.returncode, 2)
        self.assertEqual(r.stdout.strip(), "")
        self.assertEqual(r.stderr.strip(), "error: unknown member: Zed")

    def test_without_payer_lists_everything(self):
        self.add("Ana", "30.00", "groceries", "2024-03-01")
        self.add("Ben", "4.50", "cinema", "2024-03-05")
        self.assertEqual(len(self.ok("expense", "list").splitlines()), 2)


class TestOldCodeUntouched(unittest.TestCase):
    """The transaction-side oddities are out of scope and must stay as they are."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.dir = Path(self._tmp.name)
        self.store = str(self.dir / "ledger.json")
        self.csv = self.dir / "t.csv"
        self.csv.write_text(CSV, encoding="utf-8")

    def test_list_paging_unchanged(self):
        r = cli(self.store, "import", str(self.csv))
        self.assertEqual(r.returncode, 0, r.stderr)
        head = "  ID  Date            Amount  Payee                     Category        Memo"
        p1 = cli(self.store, "list", "--page", "1", "--page-size", "2").stdout.splitlines()
        self.assertEqual(p1[0], head)
        self.assertEqual([ln.split()[0] for ln in p1[1:]], ["2"])
        p2 = cli(self.store, "list", "--page", "2", "--page-size", "2").stdout.splitlines()
        self.assertEqual([ln.split()[0] for ln in p2[1:]], ["3", "4"])
        p3 = cli(self.store, "list", "--page", "3", "--page-size", "2").stdout.splitlines()
        self.assertEqual([ln.split()[0] for ln in p3[1:]], ["5"])

    def test_summary_unchanged(self):
        cli(self.store, "import", str(self.csv))
        out = cli(self.store, "summary").stdout.splitlines()
        self.assertEqual(out, [
            "Category             Total    Share",
            "income              100.00    79.4%",
            "transport            -8.00     6.3%",
            "uncategorized       -18.00    14.3%",
        ])

    def test_regex_rules_stay_case_sensitive(self):
        from ledgerlite.parse import read_csv
        from ledgerlite.rules import Rule, categorize
        txns = read_csv(self.csv)
        market = txns[2]
        self.assertEqual(market.payee, "corner market")
        self.assertIsNone(categorize(market, [Rule("MARKET", "groceries", 5, "regex")]))
        self.assertEqual(categorize(market, [Rule("market", "groceries", 5, "regex")]), "groceries")

    def test_report_page_unchanged(self):
        from ledgerlite import report
        self.assertEqual(report.page([1, 2, 3, 4], 1, 2), [2])
        self.assertEqual(report.page([1, 2, 3, 4], 2, 2), [])
        self.assertEqual(report.page([1, 2, 3, 4, 5], 3, 2), [5])

    def test_csv_ids_still_start_at_one(self):
        from ledgerlite.parse import read_csv
        self.assertEqual([t.id for t in read_csv(self.csv)], [1, 2, 3, 4, 5])


if __name__ == "__main__":
    unittest.main()
