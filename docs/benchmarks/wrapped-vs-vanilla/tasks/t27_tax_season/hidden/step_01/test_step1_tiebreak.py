import unittest

from ._h import StoreCase, txn


class TestTieBreak(StoreCase):
    def test_equal_priority_longer_pattern_wins(self):
        self.write_store([txn(1, "2024-01-03", "-9.00", "CORNER COFFEE SHOP")])
        self.cli("rules", "add", "--pattern", "coffee", "--category", "short")
        self.cli("rules", "add", "--pattern", "coffee shop", "--category", "long")
        self.cli("categorize")
        self.assertEqual(self.by_id(1)["category"], "long")

    def test_longer_pattern_wins_even_if_added_first(self):
        self.write_store([txn(1, "2024-01-03", "-9.00", "CORNER COFFEE SHOP")])
        self.cli("rules", "add", "--pattern", "coffee shop", "--category", "long")
        self.cli("rules", "add", "--pattern", "coffee", "--category", "short")
        self.cli("categorize")
        self.assertEqual(self.by_id(1)["category"], "long")

    def test_equal_priority_and_length_lowest_id_wins(self):
        self.write_store([txn(1, "2024-01-03", "-9.00", "COFFEE BAKERY")])
        self.cli("rules", "add", "--pattern", "bakery", "--category", "first")
        self.cli("rules", "add", "--pattern", "coffee", "--category", "second")
        self.cli("categorize")
        self.assertEqual(self.by_id(1)["category"], "first")

    def test_priority_still_beats_pattern_length(self):
        self.write_store([txn(1, "2024-01-03", "-9.00", "CORNER COFFEE SHOP")])
        self.cli("rules", "add", "--pattern", "coffee shop", "--category", "long", "--priority", "1")
        self.cli("rules", "add", "--pattern", "coffee", "--category", "short", "--priority", "2")
        self.cli("categorize")
        self.assertEqual(self.by_id(1)["category"], "short")


if __name__ == "__main__":
    unittest.main()
