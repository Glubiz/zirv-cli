import unittest

from ledgerlite.merchants import normalize_payee

from ._h import StoreCase, txn


class TestNormalizePayee(unittest.TestCase):
    def test_store_number_tokens_are_dropped(self):
        self.assertEqual(normalize_payee("WHOLE FOODS MARKET #221"), "WHOLE FOODS MARKET")
        self.assertEqual(normalize_payee("STARBUCKS #4521 SEATTLE"), "STARBUCKS SEATTLE")

    def test_trailing_digit_token_of_three_or_more_is_dropped(self):
        self.assertEqual(normalize_payee("LYFT RIDE 001"), "LYFT RIDE")
        self.assertEqual(normalize_payee("ALDI 2044"), "ALDI")

    def test_short_trailing_numbers_and_mixed_tokens_stay(self):
        self.assertEqual(normalize_payee("SHELL 12"), "SHELL 12")
        self.assertEqual(normalize_payee("TARGET STORE T-1234"), "TARGET STORE T-1234")

    def test_whitespace_is_collapsed_and_case_is_preserved(self):
        self.assertEqual(normalize_payee("  Blue   Bottle  Coffee "), "Blue Bottle Coffee")
        self.assertEqual(normalize_payee("amazon.com   marketplace"), "amazon.com marketplace")

    def test_never_returns_nothing_for_a_nonempty_payee(self):
        self.assertEqual(normalize_payee("#55"), "#55")
        self.assertEqual(normalize_payee("  1234 "), "1234")

    def test_already_clean_payee_is_unchanged(self):
        self.assertEqual(normalize_payee("DOWNTOWN CAFE"), "DOWNTOWN CAFE")


class TestImportNormalizes(StoreCase):
    def test_import_stores_clean_payee_and_keeps_raw(self):
        csv_path = self.write_csv([
            ("2024-01-03", "-45.67", "WHOLE FOODS MARKET #221", "weekly"),
            ("2024-01-04", "-6.75", "DOWNTOWN CAFE", "coffee"),
        ])
        r = self.cli("import", csv_path)
        self.assertEqual(r.returncode, 0, r.stderr)
        t1, t2 = self.by_id(1), self.by_id(2)
        self.assertEqual(t1["payee"], "WHOLE FOODS MARKET")
        self.assertEqual(t1["raw_payee"], "WHOLE FOODS MARKET #221")
        self.assertEqual(t2["payee"], "DOWNTOWN CAFE")
        self.assertEqual(t2["raw_payee"], "")

    def test_import_still_categorizes_normalized_rows(self):
        csv_path = self.write_csv([("2024-01-05", "-30.00", "TRADER JOES #58", "snacks")])
        self.cli("import", csv_path)
        self.assertEqual(self.by_id(1)["category"], "groceries")


class TestNormalizeCommand(StoreCase):
    def test_normalizes_existing_rows_and_reports_count(self):
        self.write_store([
            txn(1, "2024-01-03", "-9.00", "ALDI 2044"),
            txn(2, "2024-01-04", "-9.00", "CLEAN NAME"),
            txn(3, "2024-01-05", "-9.00", "LYFT RIDE 002", raw_payee="keep original"),
        ], rules=[{"id": 1, "pattern": "x", "category": "y", "priority": 0}])
        r = self.cli("normalize")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "Normalized 2 payees")
        self.assertEqual(self.by_id(1)["payee"], "ALDI")
        self.assertEqual(self.by_id(1)["raw_payee"], "ALDI 2044")
        self.assertEqual(self.by_id(2)["payee"], "CLEAN NAME")
        self.assertEqual(self.by_id(2)["raw_payee"], "")
        self.assertEqual(self.by_id(3)["payee"], "LYFT RIDE")
        self.assertEqual(self.by_id(3)["raw_payee"], "keep original")
        self.assertEqual(len(self.read_store()["rules"]), 1)

    def test_is_idempotent(self):
        self.write_store([txn(1, "2024-01-03", "-9.00", "ALDI 2044")])
        self.cli("normalize")
        r = self.cli("normalize")
        self.assertEqual(r.stdout.strip(), "Normalized 0 payees")
        self.assertEqual(self.by_id(1)["raw_payee"], "ALDI 2044")

    def test_old_store_without_raw_payee_loads(self):
        self.write_store([txn(1, "2024-01-03", "-9.00", "PLAIN")])
        r = self.cli("list")
        self.assertEqual(r.returncode, 0, r.stderr)


if __name__ == "__main__":
    unittest.main()
