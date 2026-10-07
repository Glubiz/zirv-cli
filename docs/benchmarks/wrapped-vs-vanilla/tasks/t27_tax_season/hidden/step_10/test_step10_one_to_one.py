import unittest

from ._h import StoreCase, txn


def coffee(id, date, memo=""):
    return txn(id, date, "-4.50", "BLUE BOTTLE", memo, category="dining")


class TestMergeMatchesOneToOne(StoreCase):
    def merge(self, rows, *extra):
        r = self.cli("merge", self.write_csv(rows), *extra)
        self.assertEqual(r.returncode, 0, r.stderr)
        return r.stdout.strip()

    def test_two_identical_rows_one_existing_adds_one(self):
        self.write_store([coffee(1, "2024-03-10")])
        out = self.merge([("2024-03-10", "-4.50", "BLUE BOTTLE", "a"),
                          ("2024-03-10", "-4.50", "BLUE BOTTLE", "b")])
        self.assertEqual(out, "Merged 1 new transactions, skipped 1 duplicates")
        ids = [t["id"] for t in self.read_store()["transactions"]]
        self.assertEqual(ids, [1, 2])

    def test_the_first_row_in_file_order_is_the_one_skipped(self):
        self.write_store([coffee(1, "2024-03-10")])
        self.merge([("2024-03-10", "-4.50", "BLUE BOTTLE", "first"),
                    ("2024-03-10", "-4.50", "BLUE BOTTLE", "second")])
        self.assertEqual(self.by_id(2)["memo"], "second")

    def test_two_existing_absorb_two_rows_but_not_three(self):
        self.write_store([coffee(1, "2024-03-10"), coffee(2, "2024-03-10")])
        row = ("2024-03-10", "-4.50", "BLUE BOTTLE", "x")
        self.assertEqual(self.merge([row, row]),
                         "Merged 0 new transactions, skipped 2 duplicates")
        self.assertEqual(self.merge([row, row, row]),
                         "Merged 1 new transactions, skipped 2 duplicates")

    def test_identical_rows_in_the_file_are_both_new_when_ledger_has_none(self):
        self.write_store([txn(1, "2024-03-10", "-9.99", "OTHER")])
        out = self.merge([("2024-03-10", "-4.50", "BLUE BOTTLE", "a"),
                          ("2024-03-10", "-4.50", "BLUE BOTTLE", "b")])
        self.assertEqual(out, "Merged 2 new transactions, skipped 0 duplicates")

    def test_reimporting_the_same_statement_twice_adds_nothing(self):
        rows = [("2024-03-10", "-4.50", "BLUE BOTTLE", "a"),
                ("2024-03-10", "-4.50", "BLUE BOTTLE", "b"),
                ("2024-03-11", "-60.00", "SHELL", "c")]
        self.assertEqual(self.merge(rows), "Merged 3 new transactions, skipped 0 duplicates")
        self.assertEqual(self.merge(rows), "Merged 0 new transactions, skipped 3 duplicates")
        self.assertEqual(len(self.read_store()["transactions"]), 3)

    def test_closest_date_is_claimed_so_far_away_rows_still_find_a_partner(self):
        self.write_store([coffee(1, "2024-03-10"), coffee(2, "2024-03-12")])
        out = self.merge([("2024-03-12", "-4.50", "BLUE BOTTLE", "a"),
                          ("2024-03-09", "-4.50", "BLUE BOTTLE", "b")])
        self.assertEqual(out, "Merged 0 new transactions, skipped 2 duplicates")

    def test_equal_distance_goes_to_the_lowest_id(self):
        self.write_store([coffee(1, "2024-03-09"), coffee(2, "2024-03-11")])
        out = self.merge([("2024-03-10", "-4.50", "BLUE BOTTLE", "a"),
                          ("2024-03-08", "-4.50", "BLUE BOTTLE", "b")])
        self.assertEqual(out, "Merged 1 new transactions, skipped 1 duplicates")
        self.assertEqual(self.by_id(3)["memo"], "b")

    def test_window_flag_still_applies(self):
        self.write_store([coffee(1, "2024-03-10")])
        out = self.merge([("2024-03-14", "-4.50", "BLUE BOTTLE", "a")], "--window", "3")
        self.assertEqual(out, "Merged 1 new transactions, skipped 0 duplicates")


if __name__ == "__main__":
    unittest.main()
