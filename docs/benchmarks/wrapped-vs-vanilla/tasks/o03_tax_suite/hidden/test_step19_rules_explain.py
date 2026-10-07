import unittest

from ._h import StoreCase, txn


class TestRulesExplain(StoreCase):
    def add(self, pattern, category, priority=None):
        args = ["rules", "add", "--pattern", pattern, "--category", category]
        if priority is not None:
            args += ["--priority", str(priority)]
        r = self.cli(*args)
        self.assertEqual(r.returncode, 0, r.stderr)

    def explain(self, text):
        r = self.cli("rules", "explain", text)
        self.assertEqual(r.returncode, 0, r.stderr)
        return r.stdout.strip().splitlines()

    def test_winner_then_the_rest_in_resolution_order(self):
        self.add("coffee", "short")
        self.add("coffee shop", "long")
        self.add("corner", "location", 3)
        self.add("nomatch", "never")
        self.assertEqual(
            self.explain("Corner Coffee Shop"),
            [
                "winner: 3: corner -> location (priority 3)",
                "also: 2: coffee shop -> long (priority 0)",
                "also: 1: coffee -> short (priority 0)",
            ],
        )

    def test_equal_priority_newest_rule_is_the_winner(self):
        self.add("shop", "first", 1)
        self.add("coffee shop", "second", 1)
        self.add("coffee", "third", 1)
        self.assertEqual(
            self.explain("COFFEE SHOP"),
            [
                "winner: 3: coffee -> third (priority 1)",
                "also: 2: coffee shop -> second (priority 1)",
                "also: 1: shop -> first (priority 1)",
            ],
        )

    def test_a_single_match(self):
        self.add("uber", "rides")
        self.assertEqual(self.explain("UBER TRIP"), ["winner: 1: uber -> rides (priority 0)"])

    def test_no_match_and_no_rules(self):
        self.assertEqual(self.explain("anything"), ["No user rule matches"])
        self.add("uber", "rides")
        self.assertEqual(self.explain("lyft"), ["No user rule matches"])

    def test_winner_agrees_with_what_categorize_does(self):
        self.write_store([txn(1, "2024-01-03", "-9.00", "CORNER COFFEE SHOP")])
        self.add("coffee shop", "long")
        self.add("coffee", "short")
        self.add("corner", "location", 1)
        self.assertTrue(self.explain("CORNER COFFEE SHOP")[0].startswith("winner: 3:"))
        self.cli("categorize")
        self.assertEqual(self.by_id(1)["category"], "location")

    def test_explain_never_modifies_the_store(self):
        self.add("uber", "rides")
        before = self.read_store()
        self.explain("uber")
        self.assertEqual(self.read_store(), before)


if __name__ == "__main__":
    unittest.main()
