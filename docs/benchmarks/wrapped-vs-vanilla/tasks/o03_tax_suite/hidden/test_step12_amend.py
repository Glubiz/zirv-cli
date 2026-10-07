import unittest
from decimal import Decimal

from ._h import StoreCase, txn


class TestAmend(StoreCase):
    def setUp(self):
        super().setUp()
        self.write_store(
            [
                txn(1, "2024-05-01", "-100.01", "COSTCO", category="groceries"),
                txn(2, "2024-05-02", "-10.00", "COFFEE", category="dining"),
            ],
            rules=[{"id": 1, "pattern": "x", "category": "y", "priority": 0}],
        )

    def parts(self, id):
        return [s["amount"] for s in self.by_id(id)["splits"]]

    def test_amend_unsplit_changes_only_the_amount(self):
        self.cli("tax", "mark", "2")
        r = self.cli("amend", "2", "--amount", "-12.5")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "Amended transaction 2: -10.00 -> -12.50")
        t = self.by_id(2)
        self.assertEqual(Decimal(t["amount"]), Decimal("-12.50"))
        self.assertEqual(t["payee"], "COFFEE")
        self.assertTrue(t["deductible"])
        self.assertEqual(t["splits"], [])
        self.assertEqual(self.by_id(1)["amount"], "-100.01")
        self.assertEqual(len(self.read_store()["rules"]), 1)

    def test_amending_a_split_reallocates_with_the_same_percentages_and_cents(self):
        self.cli("split", "1", "--part", "a=50", "--part", "b=50")
        r = self.cli("amend", "1", "--amount", "-10.01")
        self.assertEqual(r.stdout.strip(), "Amended transaction 1: -100.01 -> -10.01")
        self.assertEqual(self.parts(1), ["-5.01", "-5.00"])
        self.assertEqual([s["category"] for s in self.by_id(1)["splits"]], ["a", "b"])
        self.assertEqual([s["percent"] for s in self.by_id(1)["splits"]], ["50", "50"])

    def test_three_way_split_distributes_leftover_cents_to_first_parts(self):
        self.cli("split", "1", "--part", "a=33.33", "--part", "b=33.33", "--part", "c=33.34")
        self.cli("amend", "1", "--amount", "-20.02")
        self.assertEqual(self.parts(1), ["-6.68", "-6.67", "-6.67"])
        self.cli("amend", "1", "--amount", "-10.01")
        self.assertEqual(self.parts(1), ["-3.34", "-3.34", "-3.33"])

    def test_sign_change_and_exact_sum(self):
        self.cli("split", "1", "--part", "a=70", "--part", "b=30")
        self.cli("amend", "1", "--amount", "99.99")
        parts = [Decimal(p) for p in self.parts(1)]
        self.assertEqual(sum(parts), Decimal("99.99"))
        self.assertTrue(all(p > 0 for p in parts))
        self.assertEqual(self.parts(1), ["70.00", "29.99"])

    def test_summary_and_doctor_follow_the_amendment(self):
        self.cli("split", "1", "--part", "a=50", "--part", "b=50")
        self.cli("tax", "mark", "1")
        self.cli("amend", "1", "--amount", "-10.01")
        r = self.cli("tax", "summary", "2024")
        self.assertEqual(r.stdout.strip().splitlines(), ["a: 5.01", "b: 5.00", "TOTAL: 10.01"])
        self.assertEqual(self.cli("doctor").returncode, 0)

    def test_errors(self):
        before = self.read_store()
        r = self.cli("amend", "9", "--amount", "-1")
        self.assertEqual(r.returncode, 2)
        self.assertTrue(r.stderr.startswith("error: no transaction with id 9"), r.stderr)
        r = self.cli("amend", "1", "--amount", "lots")
        self.assertEqual(r.returncode, 2)
        self.assertTrue(r.stderr.startswith("error: invalid amount 'lots'"), r.stderr)
        self.assertEqual(self.read_store(), before)


if __name__ == "__main__":
    unittest.main()
