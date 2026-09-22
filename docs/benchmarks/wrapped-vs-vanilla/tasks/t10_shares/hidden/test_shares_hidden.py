import unittest
from datetime import date
from decimal import Decimal

from ledgerlite import report
from ledgerlite.models import Transaction


def _txn(i, amount, category):
    return Transaction(id=i, date=date(2024, 1, 1), amount=Decimal(amount), payee="x", memo="", category=category)


class TestCategoryShares(unittest.TestCase):
    def test_three_way_tie_naive_rounding_would_give_99_99(self):
        txns = [_txn(1, "-10", "aaa"), _txn(2, "-10", "bbb"), _txn(3, "-10", "ccc")]
        result = report.category_shares(txns)
        self.assertEqual(
            result,
            {"aaa": Decimal("33.34"), "bbb": Decimal("33.33"), "ccc": Decimal("33.33")},
        )
        self.assertEqual(sum(result.values()), Decimal("100.00"))

    def test_two_way_tie_naive_rounding_would_give_100_01(self):
        txns = [
            _txn(1, "-333.35", "north"),
            _txn(2, "-333.35", "south"),
            _txn(3, "-333.30", "east"),
        ]
        result = report.category_shares(txns)
        self.assertEqual(
            result,
            {"north": Decimal("33.34"), "south": Decimal("33.33"), "east": Decimal("33.33")},
        )
        self.assertEqual(sum(result.values()), Decimal("100.00"))

    def test_tie_break_is_alphabetical(self):
        txns = [_txn(1, "-1000.10", "alpha"), _txn(2, "-999.90", "zulu")]
        result = report.category_shares(txns)
        self.assertEqual(result, {"alpha": Decimal("50.01"), "zulu": Decimal("49.99")})
        self.assertEqual(sum(result.values()), Decimal("100.00"))

    def test_no_spending_returns_empty_dict(self):
        txns = [_txn(1, "50.00", "income")]
        self.assertEqual(report.category_shares(txns), {})

    def test_single_spending_category_is_100(self):
        txns = [_txn(1, "-42.37", "rent")]
        self.assertEqual(report.category_shares(txns), {"rent": Decimal("100.00")})

    def test_positive_only_category_excluded(self):
        txns = [
            _txn(1, "-30.00", "groceries"),
            _txn(2, "-70.00", "rent"),
            _txn(3, "500.00", "income"),
        ]
        result = report.category_shares(txns)
        self.assertNotIn("income", result)
        self.assertEqual(result, {"groceries": Decimal("30.00"), "rent": Decimal("70.00")})

    def test_uncategorised_transactions_are_grouped(self):
        txns = [_txn(1, "-40.00", None), _txn(2, "-60.00", "rent")]
        result = report.category_shares(txns)
        self.assertEqual(result, {"uncategorised": Decimal("40.00"), "rent": Decimal("60.00")})


if __name__ == "__main__":
    unittest.main()
