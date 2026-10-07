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


class Base(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.store = str(Path(self._tmp.name) / "ledger.json")

    def members(self, *names):
        for n in names:
            r = cli(self.store, "member", "add", n)
            self.assertEqual(r.returncode, 0, r.stderr)

    def add(self, payer, amount, desc="stuff", date="2024-03-01"):
        return cli(self.store, "expense", "add", "--payer", payer, "--amount", amount,
                   "--desc", desc, "--date", date)

    def data(self):
        return json.loads(Path(self.store).read_text(encoding="utf-8"))

    def shares(self, expense_id):
        r = cli(self.store, "expense", "show", str(expense_id))
        self.assertEqual(r.returncode, 0, r.stderr)
        return r.stdout.splitlines()[1:]


class TestExpenseAdd(Base):
    def test_equal_split_and_output(self):
        self.members("Ana", "Ben", "Cara")
        r = self.add("Ana", "30.00", "groceries")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "Added expense 1: 30.00 paid by Ana")
        self.assertEqual(self.shares(1), ["Ana: 10.00", "Ben: 10.00", "Cara: 10.00"])

    def test_leftover_cents_go_to_first_participants(self):
        self.members("Ana", "Ben", "Cara")
        self.add("Cara", "10.00")
        self.assertEqual(self.shares(1), ["Ana: 3.34", "Ben: 3.33", "Cara: 3.33"])

    def test_two_leftover_cents(self):
        self.members("Ana", "Ben", "Cara")
        self.add("Ben", "0.02")
        self.assertEqual(self.shares(1), ["Ana: 0.01", "Ben: 0.01", "Cara: 0.00"])

    def test_ids_are_sequential(self):
        self.members("Ana", "Ben")
        self.add("Ana", "2.00")
        r = self.add("Ben", "4.00")
        self.assertEqual(r.stdout.strip(), "Added expense 2: 4.00 paid by Ben")

    def test_payer_matched_case_insensitively(self):
        self.members("Ana", "Ben")
        r = self.add("ana", "5.00")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "Added expense 1: 5.00 paid by Ana")
        self.assertEqual(self.data()["expenses"][0]["payer"], "Ana")

    def test_unknown_payer(self):
        self.members("Ana")
        r = self.add("Zed", "5.00")
        self.assertEqual(r.returncode, 2)
        self.assertEqual(r.stdout.strip(), "")
        self.assertEqual(r.stderr.strip(), "error: unknown member: Zed")
        self.assertNotIn("expenses", self.data())

    def test_invalid_amount(self):
        self.members("Ana")
        r = self.add("Ana", "abc")
        self.assertEqual(r.returncode, 2)
        self.assertEqual(r.stderr.strip(), "error: invalid amount: abc")

    def test_non_positive_amount(self):
        self.members("Ana")
        for amount in ("0", "-4.00"):
            r = self.add("Ana", amount)
            self.assertEqual(r.returncode, 2, amount)
            self.assertEqual(r.stderr.strip(), "error: amount must be positive")
        self.assertNotIn("expenses", self.data())

    def test_invalid_date(self):
        self.members("Ana")
        r = self.add("Ana", "5.00", date="2024-13-45")
        self.assertEqual(r.returncode, 2)
        self.assertEqual(r.stderr.strip(), "error: invalid date: 2024-13-45")

    def test_later_joiners_do_not_share_old_expenses(self):
        self.members("Ana", "Ben")
        self.add("Ana", "10.00")
        self.members("Cara")
        self.assertEqual(self.shares(1), ["Ana: 5.00", "Ben: 5.00"])
        self.add("Ana", "30.00")
        self.assertEqual(self.shares(2), ["Ana: 10.00", "Ben: 10.00", "Cara: 10.00"])

    def test_storage_layout_superseded_by_16(self):
        self.members("Ana", "Ben", "Cara")
        self.add("Ben", "10.00", "taxi", "2024-03-02")
        exp = self.data()["expenses"][0]
        self.assertEqual(set(exp), {"id", "date", "payer", "amount", "description", "weights", "shares"})
        self.assertEqual(exp["id"], 1)
        self.assertEqual(exp["date"], "2024-03-02")
        self.assertEqual(exp["payer"], "Ben")
        self.assertEqual(exp["amount"], 1000)
        self.assertEqual(exp["description"], "taxi")
        self.assertEqual(exp["weights"], {"Ana": 1, "Ben": 1, "Cara": 1})
        self.assertEqual(list(exp["shares"].items()), [("Ana", 334), ("Ben", 333), ("Cara", 333)])
        self.assertEqual(self.data()["members"], ["Ana", "Ben", "Cara"])


class TestExpenseListShow(Base):
    def test_list_empty(self):
        r = cli(self.store, "expense", "list")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "No expenses.")

    def test_list_format(self):
        self.members("Ana", "Ben")
        self.add("Ana", "30.00", "groceries", "2024-03-01")
        self.add("Ben", "4.50", "cinema night", "2024-03-05")
        r = cli(self.store, "expense", "list")
        self.assertEqual(r.stdout.splitlines(), [
            "1: 2024-03-01 30.00 Ana groceries",
            "2: 2024-03-05 4.50 Ben cinema night",
        ])

    def test_show_format(self):
        self.members("Ana", "Ben")
        self.add("Ben", "9.99", "pizza", "2024-03-05")
        r = cli(self.store, "expense", "show", "1")
        self.assertEqual(r.stdout.splitlines(), [
            "Expense 1: 9.99 paid by Ben on 2024-03-05 (pizza)",
            "Ana: 5.00",
            "Ben: 4.99",
        ])

    def test_show_unknown_id(self):
        r = cli(self.store, "expense", "show", "7")
        self.assertEqual(r.returncode, 2)
        self.assertEqual(r.stderr.strip(), "error: no such expense: 7")

    def test_expenses_survive_import(self):
        self.members("Ana")
        self.add("Ana", "5.00")
        csv_path = Path(self.store).parent / "s.csv"
        csv_path.write_text("date,amount,payee,memo\n2024-01-01,-5.00,Shop,x\n", encoding="utf-8")
        cli(self.store, "import", str(csv_path))
        self.assertEqual(len(self.data()["expenses"]), 1)


if __name__ == "__main__":
    unittest.main()
