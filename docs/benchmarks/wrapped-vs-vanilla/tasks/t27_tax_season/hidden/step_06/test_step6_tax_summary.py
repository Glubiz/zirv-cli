import unittest

from ._h import StoreCase, txn


class TestTaxSummary(StoreCase):
    def lines(self, year="2024"):
        r = self.cli("tax", "summary", year)
        self.assertEqual(r.returncode, 0, r.stderr)
        return r.stdout.strip().splitlines()

    def test_totals_per_category_sorted_by_name_with_total(self):
        self.write_store([
            txn(1, "2024-02-01", "-100.00", "A", category="software", deductible=True),
            txn(2, "2024-03-01", "-20.50", "B", category="office", deductible=True),
            txn(3, "2024-04-01", "-30.25", "C", category="software", deductible=True),
        ])
        self.assertEqual(
            self.lines(), ["office: 20.50", "software: 130.25", "TOTAL: 150.75"]
        )

    def test_only_flagged_expenses_in_the_year_count(self):
        self.write_store([
            txn(1, "2024-02-01", "-10.00", "A", category="x", deductible=True),
            txn(2, "2024-02-02", "-99.00", "B", category="x"),  # not flagged
            txn(3, "2023-12-31", "-5.00", "C", category="x", deductible=True),  # last year
            txn(4, "2025-01-01", "-6.00", "D", category="x", deductible=True),  # next year
            txn(5, "2024-06-01", "300.00", "E", category="x", deductible=True),  # money in
            txn(6, "2024-12-31", "-1.00", "F", category="x", deductible=True),  # last day
            txn(7, "2024-01-01", "-2.00", "G", category="x", deductible=True),  # first day
        ])
        self.assertEqual(self.lines(), ["x: 13.00", "TOTAL: 13.00"])

    def test_uncategorized_group(self):
        self.write_store([txn(1, "2024-02-01", "-7.00", "A", deductible=True)])
        self.assertEqual(self.lines(), ["uncategorized: 7.00", "TOTAL: 7.00"])

    def test_empty_year_prints_just_a_zero_total(self):
        self.write_store([txn(1, "2024-02-01", "-7.00", "A", category="x", deductible=True)])
        self.assertEqual(self.lines("2019"), ["TOTAL: 0.00"])

    def test_split_transactions_count_per_part_with_exact_cents(self):
        self.write_store([
            txn(1, "2024-05-01", "-100.01", "COSTCO", category="groceries", deductible=True,
                splits=[
                    {"category": "groceries", "percent": "50", "amount": "-50.01"},
                    {"category": "office", "percent": "50", "amount": "-50.00"},
                ]),
            txn(2, "2024-05-02", "-10.00", "STAPLES", category="office", deductible=True),
        ])
        self.assertEqual(
            self.lines(), ["groceries: 50.01", "office: 60.00", "TOTAL: 110.01"]
        )

    def test_split_made_through_the_cli_then_summarised(self):
        self.write_store([
            txn(1, "2024-05-01", "-10.01", "COSTCO", category="groceries"),
        ])
        self.cli("split", "1", "--part", "a=33.33", "--part", "b=33.33", "--part", "c=33.34")
        self.cli("tax", "mark", "1")
        self.assertEqual(self.lines(), ["a: 3.34", "b: 3.34", "c: 3.33", "TOTAL: 10.01"])

    def test_unflagged_split_is_ignored(self):
        self.write_store([
            txn(1, "2024-05-01", "-10.00", "COSTCO", category="g",
                splits=[{"category": "a", "percent": "50", "amount": "-5.00"},
                        {"category": "b", "percent": "50", "amount": "-5.00"}]),
        ])
        self.assertEqual(self.lines(), ["TOTAL: 0.00"])


if __name__ == "__main__":
    unittest.main()
