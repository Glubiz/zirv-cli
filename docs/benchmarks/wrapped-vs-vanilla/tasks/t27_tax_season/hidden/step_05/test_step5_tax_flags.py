import unittest

from ._h import StoreCase, txn


class TestTaxFlags(StoreCase):
    def setUp(self):
        super().setUp()
        self.write_store(
            [
                txn(1, "2024-05-01", "-100.00", "OFFICE DEPOT", category="office"),
                txn(2, "2024-05-02", "-10.00", "COFFEE", category="dining"),
                txn(3, "2024-05-03", "-45.50", "ADOBE", category="software"),
            ],
            rules=[{"id": 1, "pattern": "x", "category": "y", "priority": 0}],
        )

    def test_mark_prints_count_and_sets_flag(self):
        r = self.cli("tax", "mark", "1", "3")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "Marked 2 transactions as tax-deductible")
        self.assertTrue(self.by_id(1)["deductible"])
        self.assertFalse(self.by_id(2)["deductible"])
        self.assertTrue(self.by_id(3)["deductible"])
        self.assertEqual(len(self.read_store()["rules"]), 1)

    def test_mark_counts_distinct_ids_and_is_idempotent(self):
        r = self.cli("tax", "mark", "1", "1")
        self.assertEqual(r.stdout.strip(), "Marked 1 transactions as tax-deductible")
        r = self.cli("tax", "mark", "1")
        self.assertEqual(r.stdout.strip(), "Marked 1 transactions as tax-deductible")
        self.assertTrue(self.by_id(1)["deductible"])

    def test_unmark(self):
        self.cli("tax", "mark", "1", "2", "3")
        r = self.cli("tax", "unmark", "2", "3")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "Unmarked 2 transactions")
        self.assertTrue(self.by_id(1)["deductible"])
        self.assertFalse(self.by_id(2)["deductible"])

    def test_unknown_id_is_all_or_nothing(self):
        before = self.read_store()
        r = self.cli("tax", "mark", "1", "42")
        self.assertEqual(r.returncode, 2)
        self.assertTrue(r.stderr.startswith("error: no transaction with id 42"), r.stderr)
        self.assertEqual(self.read_store(), before)
        r = self.cli("tax", "unmark", "42")
        self.assertEqual(r.returncode, 2)

    def test_list_shows_only_flagged_in_id_order(self):
        self.cli("tax", "mark", "3", "1")
        r = self.cli("tax", "list")
        self.assertEqual(r.returncode, 0, r.stderr)
        rows = [line.split() for line in r.stdout.strip().splitlines()]
        self.assertEqual(rows[0][:3], ["1", "2024-05-01", "-100.00"])
        self.assertEqual(rows[0][3:], ["OFFICE", "DEPOT"])
        self.assertEqual(rows[1][:3], ["3", "2024-05-03", "-45.50"])
        self.assertEqual(len(rows), 2)

    def test_list_when_nothing_flagged(self):
        r = self.cli("tax", "list")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "No tax-deductible transactions.")

    def test_old_store_loads_as_not_deductible(self):
        self.assertFalse(self.cli("categorize").returncode)
        self.assertFalse(self.by_id(1)["deductible"])

    def test_flag_survives_import_free_commands(self):
        self.cli("tax", "mark", "2")
        self.cli("split", "2", "--part", "a=50", "--part", "b=50")
        self.cli("normalize")
        self.assertTrue(self.by_id(2)["deductible"])


if __name__ == "__main__":
    unittest.main()
