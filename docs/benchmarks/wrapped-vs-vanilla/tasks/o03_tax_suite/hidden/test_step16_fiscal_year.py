import unittest

from ._h import StoreCase, txn


class TestFiscalYear(StoreCase):
    def setUp(self):
        super().setUp()
        self.write_store([
            txn(1, "2024-03-31", "-1.00", "A", category="a", deductible=True),
            txn(2, "2024-04-01", "-10.00", "B", category="b", deductible=True),
            txn(3, "2025-03-31", "-100.00", "C", category="c", deductible=True),
            txn(4, "2025-04-01", "-1000.00", "D", category="d", deductible=True),
            txn(5, "2024-07-01", "-5.00", "E", category="b", deductible=False),
        ])

    def lines(self, *args):
        r = self.cli("tax", "summary", *args)
        self.assertEqual(r.returncode, 0, r.stderr)
        return r.stdout.strip().splitlines()

    def test_default_year_is_april_to_march(self):
        self.assertEqual(self.lines("2024"), ["c: 100.00", "b: 10.00", "TOTAL: 110.00"])

    def test_neighbouring_fiscal_years(self):
        self.assertEqual(self.lines("2023"), ["a: 1.00", "TOTAL: 1.00"])
        self.assertEqual(self.lines("2025"), ["d: 1000.00", "TOTAL: 1000.00"])

    def test_calendar_flag_restores_january_to_december(self):
        self.assertEqual(self.lines("2024", "--calendar"),
                         ["b: 10.00", "a: 1.00", "TOTAL: 11.00"])
        self.assertEqual(self.lines("--calendar", "2025"),
                         ["d: 1000.00", "c: 100.00", "TOTAL: 1100.00"])
        self.assertEqual(self.lines("2019", "--calendar"), ["TOTAL: 0.00"])

    def test_splits_and_ordering_carry_over(self):
        self.write_store([
            txn(1, "2024-09-01", "-100.01", "COSTCO", category="g", deductible=True,
                splits=[{"category": "g", "percent": "30", "amount": "-30.01"},
                        {"category": "o", "percent": "70", "amount": "-70.00"}]),
            txn(2, "2025-02-01", "-30.01", "X", category="g", deductible=True),
        ])
        self.assertEqual(self.lines("2024"), ["o: 70.00", "g: 60.02", "TOTAL: 130.02"])

    def test_tax_list_does_not_depend_on_the_year(self):
        r = self.cli("tax", "list")
        self.assertEqual([l.split()[0] for l in r.stdout.strip().splitlines()],
                         ["1", "2", "3", "4"])


if __name__ == "__main__":
    unittest.main()
