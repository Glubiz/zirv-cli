import unittest

from ._h import StoreCase, txn


class TestMerge(StoreCase):
    def _existing(self):
        self.write_store(
            [
                txn(5, "2024-03-10", "-4.50", "STARBUCKS CAFE", category="dining"),
                txn(9, "2024-03-11", "-60.00", "SHELL OIL", category="fuel"),
            ],
            rules=[{"id": 1, "pattern": "pottery", "category": "hobby", "priority": 0}],
        )

    def test_exact_duplicate_is_skipped_and_new_row_added_with_next_id(self):
        self._existing()
        csv_path = self.write_csv([
            ("2024-03-10", "-4.50", "STARBUCKS CAFE", "latte"),
            ("2024-03-12", "-20.00", "Clay Pottery Studio", "class"),
        ])
        r = self.cli("merge", csv_path)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "Merged 1 new transactions, skipped 1 duplicates")
        data = self.read_store()
        self.assertEqual([t["id"] for t in data["transactions"]], [5, 9, 10])
        self.assertEqual(data["transactions"][2]["payee"], "Clay Pottery Studio")
        self.assertEqual(data["transactions"][2]["category"], "hobby")
        self.assertEqual(len(data["rules"]), 1)

    def test_default_window_is_two_days(self):
        self._existing()
        after = self.write_csv([("2024-03-12", "-4.50", "STARBUCKS CAFE", "x")], name="a.csv")
        r = self.cli("merge", after)  # 2 days after: duplicate
        self.assertEqual(r.stdout.strip(), "Merged 0 new transactions, skipped 1 duplicates")
        before = self.write_csv([("2024-03-08", "-4.50", "STARBUCKS CAFE", "y")], name="b.csv")
        r = self.cli("merge", before)  # 2 days before: duplicate
        self.assertEqual(r.stdout.strip(), "Merged 0 new transactions, skipped 1 duplicates")
        csv2 = self.write_csv([("2024-03-13", "-4.50", "STARBUCKS CAFE", "z")], name="two.csv")
        r = self.cli("merge", csv2)
        self.assertEqual(r.stdout.strip(), "Merged 1 new transactions, skipped 0 duplicates")

    def test_window_flag_widens_and_narrows(self):
        self._existing()
        csv_path = self.write_csv([("2024-03-15", "-4.50", "STARBUCKS CAFE", "x")])
        r = self.cli("merge", csv_path, "--window", "5")
        self.assertEqual(r.stdout.strip(), "Merged 0 new transactions, skipped 1 duplicates")
        csv2 = self.write_csv([("2024-03-11", "-4.50", "STARBUCKS CAFE", "x")], name="b.csv")
        r = self.cli("merge", csv2, "--window", "0")
        self.assertEqual(r.stdout.strip(), "Merged 1 new transactions, skipped 0 duplicates")

    def test_payee_compared_after_normalisation_ignoring_case(self):
        self._existing()
        csv_path = self.write_csv([("2024-03-10", "-4.50", "Starbucks Cafe #4521", "x")])
        r = self.cli("merge", csv_path)
        self.assertEqual(r.stdout.strip(), "Merged 0 new transactions, skipped 1 duplicates")

    def test_existing_rows_with_noisy_payees_still_match(self):
        self.write_store([txn(1, "2024-03-10", "-4.50", "STARBUCKS CAFE #12")])
        csv_path = self.write_csv([("2024-03-10", "-4.50", "STARBUCKS CAFE", "x")])
        r = self.cli("merge", csv_path)
        self.assertEqual(r.stdout.strip(), "Merged 0 new transactions, skipped 1 duplicates")

    def test_different_amount_or_sign_or_payee_is_not_a_duplicate(self):
        self._existing()
        csv_path = self.write_csv([
            ("2024-03-10", "-4.51", "STARBUCKS CAFE", "x"),
            ("2024-03-10", "4.50", "STARBUCKS CAFE", "refund"),
            ("2024-03-10", "-4.50", "PEETS COFFEE", "x"),
        ])
        r = self.cli("merge", csv_path)
        self.assertEqual(r.stdout.strip(), "Merged 3 new transactions, skipped 0 duplicates")
        self.assertEqual([t["id"] for t in self.read_store()["transactions"]], [5, 9, 10, 11, 12])

    def test_new_rows_are_normalised_with_raw_payee(self):
        self._existing()
        csv_path = self.write_csv([("2024-04-01", "-33.00", "ALDI 2044", "food")])
        self.cli("merge", csv_path)
        t = self.by_id(10)
        self.assertEqual(t["payee"], "ALDI")
        self.assertEqual(t["raw_payee"], "ALDI 2044")

    def test_merge_into_missing_store_starts_ids_at_one(self):
        csv_path = self.write_csv([
            ("2024-04-01", "-33.00", "ALDI", "food"),
            ("2024-04-02", "-3.00", "UBER TRIP", "ride"),
        ])
        r = self.cli("merge", csv_path)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "Merged 2 new transactions, skipped 0 duplicates")
        data = self.read_store()
        self.assertEqual([t["id"] for t in data["transactions"]], [1, 2])
        self.assertEqual(data["transactions"][1]["category"], "transport")

    def test_negative_window_is_an_error_and_leaves_store_alone(self):
        self._existing()
        before = self.read_store()
        csv_path = self.write_csv([("2024-04-01", "-33.00", "ALDI", "food")])
        r = self.cli("merge", csv_path, "--window", "-1")
        self.assertEqual(r.returncode, 2)
        self.assertTrue(r.stderr.startswith("error: window must be >= 0"), r.stderr)
        self.assertEqual(self.read_store(), before)


if __name__ == "__main__":
    unittest.main()
