import unittest
from datetime import date
from decimal import Decimal

from ledgerlite import report
from ledgerlite.models import Transaction


def _txn(i, month, amount, category=None):
    return Transaction(id=i, date=date(2024, month, 15), amount=Decimal(amount), payee="x", memo="", category=category)


class TestTotals(unittest.TestCase):
    def test_monthly_totals(self):
        txns = [_txn(1, 1, "-10.00"), _txn(2, 1, "-5.00"), _txn(3, 2, "20.00")]
        totals = report.monthly_totals(txns)
        self.assertEqual(totals["2024-01"], Decimal("-15.00"))
        self.assertEqual(totals["2024-02"], Decimal("20.00"))

    def test_category_totals_groups_uncategorized(self):
        txns = [_txn(1, 1, "-10.00", "groceries"), _txn(2, 1, "-5.00", None)]
        totals = report.category_totals(txns)
        self.assertEqual(totals["groceries"], Decimal("-10.00"))
        self.assertEqual(totals["uncategorized"], Decimal("-5.00"))

    def test_summarize_counts_and_net(self):
        txns = [_txn(1, 1, "-10.00", "groceries"), _txn(2, 1, "30.00", "income")]
        summary = report.summarize(txns)
        self.assertEqual(summary["count"], 2)
        self.assertEqual(summary["net"], Decimal("20.00"))
        self.assertIn("groceries", summary["by_category"])
        self.assertIn("income", summary["by_category"])


class TestPage(unittest.TestCase):
    def test_middle_page(self):
        items = list(range(1, 26))  # 25 items
        self.assertEqual(report.page(items, 2, 10), list(range(11, 21)))

    def test_partial_final_page(self):
        items = list(range(1, 26))  # 25 items, page_size 10 -> final page has 5
        self.assertEqual(report.page(items, 3, 10), list(range(21, 26)))

    def test_out_of_range_page_is_empty(self):
        items = list(range(1, 6))
        self.assertEqual(report.page(items, 99, 10), [])


if __name__ == "__main__":
    unittest.main()
