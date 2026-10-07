import json
import unittest
from pathlib import Path

from ._h import StoreCase, run, txn


class TestRulesCommands(StoreCase):
    def test_add_prints_and_numbers_rules(self):
        r1 = self.cli("rules", "add", "--pattern", "starbucks", "--category", "coffee")
        self.assertEqual(r1.returncode, 0, r1.stderr)
        self.assertEqual(r1.stdout.strip(), "Added rule 1: starbucks -> coffee (priority 0)")
        r2 = self.cli("rules", "add", "--pattern", "shell", "--category", "fuel", "--priority", "7")
        self.assertEqual(r2.returncode, 0, r2.stderr)
        self.assertEqual(r2.stdout.strip(), "Added rule 2: shell -> fuel (priority 7)")

    def test_list_in_id_order(self):
        self.cli("rules", "add", "--pattern", "aaa", "--category", "one", "--priority", "5")
        self.cli("rules", "add", "--pattern", "bbb", "--category", "two")
        r = self.cli("rules", "list")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(
            r.stdout.strip().splitlines(),
            ["1: aaa -> one (priority 5)", "2: bbb -> two (priority 0)"],
        )

    def test_list_with_no_rules_or_no_file(self):
        r = self.cli("rules", "list")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "No rules.")
        self.write_store([txn(1, "2024-01-01", "-5.00", "X")])
        r = self.cli("rules", "list")
        self.assertEqual(r.stdout.strip(), "No rules.")

    def test_empty_pattern_is_an_error(self):
        r = self.cli("rules", "add", "--pattern", "  ", "--category", "x")
        self.assertEqual(r.returncode, 2)
        self.assertTrue(r.stderr.startswith("error:"), r.stderr)
        self.assertEqual(r.stdout, "")

    def test_rules_live_next_to_transactions_without_clobbering(self):
        self.write_store([txn(1, "2024-01-01", "-5.00", "X")], notes={"keep": "me"})
        self.cli("rules", "add", "--pattern", "x", "--category", "y")
        data = self.read_store()
        self.assertEqual(len(data["transactions"]), 1)
        self.assertEqual(len(data["rules"]), 1)
        self.assertEqual(data["notes"], {"keep": "me"})


class TestCategorizeWithRules(StoreCase):
    def _cat(self, id):
        return self.by_id(id)["category"]

    def test_user_rule_beats_default_rules_even_with_lower_priority(self):
        self.write_store([txn(1, "2024-01-03", "-45.67", "WHOLE FOODS MARKET")])
        self.cli("rules", "add", "--pattern", "whole foods", "--category", "treats", "--priority", "0")
        r = self.cli("categorize")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "Re-categorized 1 of 1 transactions")
        self.assertEqual(self._cat(1), "treats")

    def test_higher_priority_user_rule_wins(self):
        self.write_store([txn(1, "2024-01-03", "-9.00", "ACME HARDWARE")])
        self.cli("rules", "add", "--pattern", "acme", "--category", "low", "--priority", "1")
        self.cli("rules", "add", "--pattern", "hardware", "--category", "high", "--priority", "9")
        self.cli("categorize")
        self.assertEqual(self._cat(1), "high")

    def test_falls_back_to_default_rules_when_no_user_rule_matches(self):
        self.write_store([txn(1, "2024-01-06", "-14.25", "UBER TRIP")])
        self.cli("rules", "add", "--pattern", "nomatch", "--category", "zzz")
        self.cli("categorize")
        self.assertEqual(self._cat(1), "transport")

    def test_matching_is_case_insensitive_and_checks_memo(self):
        self.write_store([txn(1, "2024-01-03", "-9.00", "SOME SHOP", memo="Birthday Gift")])
        self.cli("rules", "add", "--pattern", "BIRTHDAY", "--category", "gifts")
        self.cli("categorize")
        self.assertEqual(self._cat(1), "gifts")


class TestImportWithRules(StoreCase):
    def test_import_uses_user_rules_and_keeps_them(self):
        self.cli("rules", "add", "--pattern", "pottery", "--category", "hobby")
        csv_path = self.write_csv([
            ("2024-02-01", "-30.00", "Clay Pottery Studio", "class"),
            ("2024-02-02", "-12.00", "Pottery Barn", "mug", "kitchen"),
            ("2024-02-03", "-6.75", "DOWNTOWN CAFE", "coffee"),
        ])
        r = self.cli("import", csv_path)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("Imported 3 transactions", r.stdout)
        data = self.read_store()
        cats = {t["id"]: t["category"] for t in data["transactions"]}
        self.assertEqual(cats[1], "hobby")
        self.assertEqual(cats[2], "kitchen")  # explicit CSV category is left alone
        self.assertEqual(cats[3], "dining")  # default rules still apply
        self.assertEqual(len(data["rules"]), 1)

    def test_import_and_categorize_keep_unrelated_store_keys(self):
        self.write_store([txn(1, "2024-01-01", "-1.00", "Z")], extra={"a": [1, 2]})
        csv_path = self.write_csv([("2024-02-01", "-3.00", "Q", "m")])
        self.cli("import", csv_path)
        self.assertEqual(self.read_store()["extra"], {"a": [1, 2]})
        self.cli("categorize")
        self.assertEqual(self.read_store()["extra"], {"a": [1, 2]})


if __name__ == "__main__":
    unittest.main()
