import unittest

from ._h import StoreCase, txn


class TestNewestRuleWinsTies(StoreCase):
    def test_equal_priority_newer_rule_wins_even_with_shorter_pattern(self):
        self.write_store([txn(1, "2024-01-03", "-9.00", "CORNER COFFEE SHOP")])
        self.cli("rules", "add", "--pattern", "coffee shop", "--category", "long")
        self.cli("rules", "add", "--pattern", "coffee", "--category", "short")
        self.cli("categorize")
        self.assertEqual(self.by_id(1)["category"], "short")

    def test_equal_priority_newer_rule_wins_when_it_is_longer_too(self):
        self.write_store([txn(1, "2024-01-03", "-9.00", "CORNER COFFEE SHOP")])
        self.cli("rules", "add", "--pattern", "coffee", "--category", "short")
        self.cli("rules", "add", "--pattern", "coffee shop", "--category", "long")
        self.cli("categorize")
        self.assertEqual(self.by_id(1)["category"], "long")

    def test_newest_among_three_equal_priority_matches(self):
        self.write_store([txn(1, "2024-01-03", "-9.00", "COFFEE BAKERY CAFE")])
        self.cli("rules", "add", "--pattern", "coffee", "--category", "one")
        self.cli("rules", "add", "--pattern", "bakery", "--category", "two")
        self.cli("rules", "add", "--pattern", "cafe", "--category", "three")
        self.cli("categorize")
        self.assertEqual(self.by_id(1)["category"], "three")

    def test_priority_still_beats_recency(self):
        self.write_store([txn(1, "2024-01-03", "-9.00", "CORNER COFFEE SHOP")])
        self.cli("rules", "add", "--pattern", "coffee", "--category", "old-but-strong", "--priority", "5")
        self.cli("rules", "add", "--pattern", "shop", "--category", "new-but-weak", "--priority", "1")
        self.cli("categorize")
        self.assertEqual(self.by_id(1)["category"], "old-but-strong")

    def test_user_rules_still_beat_defaults_and_defaults_still_apply(self):
        self.write_store([
            txn(1, "2024-01-03", "-9.00", "WHOLE FOODS MARKET"),
            txn(2, "2024-01-04", "-9.00", "UBER TRIP"),
        ])
        self.cli("rules", "add", "--pattern", "whole foods", "--category", "treats", "--priority", "-3")
        self.cli("categorize")
        self.assertEqual(self.by_id(1)["category"], "treats")
        self.assertEqual(self.by_id(2)["category"], "transport")

    def test_import_and_merge_use_the_new_tie_break(self):
        self.cli("rules", "add", "--pattern", "pottery studio", "--category", "classes")
        self.cli("rules", "add", "--pattern", "pottery", "--category", "hobby")
        r = self.cli("import", self.write_csv([("2024-02-01", "-30.00", "Clay Pottery Studio", "x")]))
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.by_id(1)["category"], "hobby")
        r = self.cli("merge", self.write_csv([("2024-02-09", "-31.00", "Another Pottery Studio", "y")], name="m.csv"))
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(self.by_id(2)["category"], "hobby")


if __name__ == "__main__":
    unittest.main()
