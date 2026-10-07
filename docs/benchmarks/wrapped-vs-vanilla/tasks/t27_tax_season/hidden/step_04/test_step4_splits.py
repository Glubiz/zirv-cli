import unittest
from decimal import Decimal

from ._h import StoreCase, txn


class TestSplit(StoreCase):
    def setUp(self):
        super().setUp()
        self.write_store(
            [
                txn(1, "2024-05-01", "-100.01", "COSTCO WHOLESALE", category="groceries"),
                txn(2, "2024-05-02", "-10.01", "TARGET", category="household"),
                txn(3, "2024-05-03", "200.00", "REFUND CO"),
            ],
            rules=[{"id": 1, "pattern": "x", "category": "y", "priority": 0}],
        )

    def _parts(self, id):
        return [(s["category"], s["amount"]) for s in self.by_id(id)["splits"]]

    def test_split_prints_confirmation_and_stores_parts(self):
        r = self.cli("split", "1", "--part", "groceries=70", "--part", "household=30")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "Split transaction 1 into 2 parts")
        t = self.by_id(1)
        self.assertEqual(t["category"], "groceries")  # untouched
        self.assertEqual(t["amount"], "-100.01")
        self.assertEqual(self._parts(1), [("groceries", "-70.01"), ("household", "-30.00")])
        self.assertEqual([s["percent"] for s in t["splits"]], ["70", "30"])
        self.assertEqual(len(self.read_store()["rules"]), 1)

    def test_remainder_cent_goes_to_first_part(self):
        self.cli("split", "1", "--part", "a=50", "--part", "b=50")
        self.assertEqual(self._parts(1), [("a", "-50.01"), ("b", "-50.00")])

    def test_remainder_cents_go_one_each_to_first_parts_in_order(self):
        self.cli("split", "2", "--part", "a=33.33", "--part", "b=33.33", "--part", "c=33.34")
        self.assertEqual(self._parts(2), [("a", "-3.34"), ("b", "-3.34"), ("c", "-3.33")])

    def test_parts_always_sum_to_the_amount_and_keep_sign(self):
        self.cli("split", "3", "--part", "x=10", "--part", "y=90")
        self.assertEqual(self._parts(3), [("x", "20.00"), ("y", "180.00")])
        self.cli("split", "2", "--part", "a=1", "--part", "b=2", "--part", "c=97")
        total = sum(Decimal(s["amount"]) for s in self.by_id(2)["splits"])
        self.assertEqual(total, Decimal("-10.01"))

    def test_resplitting_replaces_the_old_split(self):
        self.cli("split", "1", "--part", "a=50", "--part", "b=50")
        self.cli("split", "1", "--part", "c=25", "--part", "d=75")
        self.assertEqual([c for c, _ in self._parts(1)], ["c", "d"])

    def test_split_show_lists_parts_in_order(self):
        self.cli("split", "1", "--part", "groceries=70", "--part", "household=30")
        r = self.cli("split-show", "1")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip().splitlines(), ["groceries: -70.01", "household: -30.00"])

    def test_split_show_on_unsplit_transaction(self):
        r = self.cli("split-show", "2")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "Transaction 2 is not split")

    def test_validation_errors_exit_2_and_change_nothing(self):
        before = self.read_store()
        cases = [
            (("split", "1", "--part", "a=100"), "need at least two --part options"),
            (("split", "1", "--part", "a=60", "--part", "b=30"), "percentages must sum to 100"),
            (("split", "1", "--part", "a=60", "--part", "nonsense"), "bad --part 'nonsense'"),
            (("split", "99", "--part", "a=50", "--part", "b=50"), "no transaction with id 99"),
        ]
        for args, text in cases:
            r = self.cli(*args)
            self.assertEqual(r.returncode, 2, args)
            self.assertTrue(r.stderr.startswith("error: " + text), (args, r.stderr))
        self.assertEqual(self.read_store(), before)

    def test_old_store_without_splits_loads_and_round_trips(self):
        r = self.cli("categorize")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.by_id(1)["splits"], [])


if __name__ == "__main__":
    unittest.main()
