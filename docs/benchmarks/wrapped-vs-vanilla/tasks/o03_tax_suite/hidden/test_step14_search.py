import unittest

from ._h import StoreCase, txn

SPLIT = [{"category": "office", "percent": "50", "amount": "-50.01"},
         {"category": "Groceries", "percent": "50", "amount": "-50.00"}]


class TestSearch(StoreCase):
    def setUp(self):
        super().setUp()
        self.write_store([
            txn(1, "2024-03-05", "-45.67", "WHOLE FOODS MARKET #221", "weekly shop", "groceries"),
            txn(2, "2024-01-10", "-4.50", "Blue Bottle Coffee", "latte", "dining"),
            txn(3, "2024-03-05", "-100.01", "COSTCO WHOLESALE", "bulk", "household",
                splits=SPLIT, deductible=True),
            txn(4, "2023-12-30", "2500.00", "EMPLOYER PAYROLL", "salary", "income"),
            txn(5, "2024-02-02", "-20.00", "ALDI 2044", "food", "groceries", deductible=True),
        ])

    def ids(self, query):
        r = self.cli("search", query)
        self.assertEqual(r.returncode, 0, r.stderr)
        lines = r.stdout.strip().splitlines()
        count = lines[-1]
        self.assertTrue(count.endswith(" matches"), lines)
        self.assertEqual(int(count.split()[0]), len(lines) - 1)
        return [int(line.split()[0]) for line in lines[:-1]]

    def test_output_rows_use_the_tax_list_row_format_and_date_order(self):
        r = self.cli("search", "category:groceries")
        self.assertEqual(
            r.stdout.strip().splitlines(),
            ["5 2024-02-02 -20.00 ALDI 2044",
             "1 2024-03-05 -45.67 WHOLE FOODS MARKET #221",
             "3 2024-03-05 -100.01 COSTCO WHOLESALE",
             "3 matches"],
        )

    def test_results_ordered_by_date_then_id(self):
        self.assertEqual(self.ids(""), [4, 2, 5, 1, 3])

    def test_payee_term_uses_the_clean_merchant_name_case_insensitively(self):
        self.assertEqual(self.ids("payee:whole"), [1, 3])
        self.assertEqual(self.ids("payee:BLUE"), [2])
        self.assertEqual(self.ids("payee:aldi"), [5])
        self.assertEqual(self.ids("payee:221"), [])
        self.assertEqual(self.ids("payee:2044"), [])

    def test_quoted_values(self):
        self.assertEqual(self.ids('payee:"whole foods"'), [1])
        self.assertEqual(self.ids('"weekly shop"'), [1])

    def test_category_matches_exactly_including_split_parts(self):
        self.assertEqual(self.ids("category:dining"), [2])
        self.assertEqual(self.ids("category:DINING"), [2])
        self.assertEqual(self.ids("category:house"), [])
        self.assertEqual(self.ids("category:office"), [3])
        self.assertEqual(self.ids("category:groceries"), [5, 1, 3])

    def test_amount_comparisons_use_absolute_value(self):
        self.assertEqual(self.ids("amount>40"), [4, 1, 3])
        self.assertEqual(self.ids("amount<10"), [2])
        self.assertEqual(self.ids("amount>=45.67 amount<=100.01"), [1, 3])
        self.assertEqual(self.ids("amount>100.01"), [4])
        self.assertEqual(self.ids("amount<=20"), [2, 5])

    def test_year_and_tax_terms(self):
        self.assertEqual(self.ids("year:2023"), [4])
        self.assertEqual(self.ids("year:2024"), [2, 5, 1, 3])
        self.assertEqual(self.ids("tax:yes"), [5, 3])
        self.assertEqual(self.ids("tax:no"), [4, 2, 1])

    def test_bare_words_search_payee_and_memo_and_terms_are_anded(self):
        self.assertEqual(self.ids("coffee"), [2])
        self.assertEqual(self.ids("LATTE"), [2])
        self.assertEqual(self.ids("groceries"), [])  # category is not payee/memo
        self.assertEqual(self.ids("food"), [5, 1])
        self.assertEqual(self.ids("food tax:yes"), [5])
        self.assertEqual(self.ids("food tax:yes year:2023"), [])

    def test_errors(self):
        for query, text in [
            ("colour:red", "unknown search term 'colour:red'"),
            ("tax:maybe", "bad value in search term 'tax:maybe'"),
            ("year:abc", "bad value in search term 'year:abc'"),
            ("amount>lots", "bad value in search term 'amount>lots'"),
        ]:
            r = self.cli("search", query)
            self.assertEqual(r.returncode, 2, query)
            self.assertTrue(r.stderr.startswith("error: " + text), (query, r.stderr))
            self.assertEqual(r.stdout, "")

    def test_search_never_modifies_the_store(self):
        before = self.read_store()
        self.cli("search", "payee:whole")
        self.assertEqual(self.read_store(), before)


if __name__ == "__main__":
    unittest.main()
