import unittest
from datetime import date
from decimal import Decimal

from ledgerlite.models import Transaction
from ledgerlite.rules import Rule, categorize


def _txn(payee="", memo=""):
    return Transaction(id=1, date=date(2024, 1, 1), amount=Decimal("-10.00"), payee=payee, memo=memo)


class TestCategorize(unittest.TestCase):
    def test_substring_rule_matches_case_insensitively(self):
        rule = Rule(pattern="coffee", category="dining", priority=1, kind="substring")
        txn = _txn(payee="BLUE BOTTLE COFFEE")
        self.assertEqual(categorize(txn, [rule]), "dining")

    def test_regex_rule_matches_same_case(self):
        rule = Rule(pattern=r"CAFE\d+", category="dining", priority=1, kind="regex")
        txn = _txn(payee="CAFE42 DOWNTOWN")
        self.assertEqual(categorize(txn, [rule]), "dining")

    def test_regex_rule_case_insensitive(self):
        # Rule's docstring promises case-insensitive matching for every
        # rule kind, including regex rules.
        rule = Rule(pattern=r"cafe\d+", category="dining", priority=1, kind="regex")
        txn = _txn(payee="CAFE42 DOWNTOWN")
        self.assertEqual(categorize(txn, [rule]), "dining")

    def test_higher_priority_wins_over_earlier_rule(self):
        first_low = Rule(pattern="market", category="convenience", priority=1, kind="substring")
        second_high = Rule(pattern="market", category="groceries", priority=9, kind="substring")
        txn = _txn(payee="CORNER MARKET")
        self.assertEqual(categorize(txn, [first_low, second_high]), "groceries")

    def test_tie_breaks_to_earliest_rule(self):
        earliest = Rule(pattern="market", category="groceries", priority=5, kind="substring")
        later = Rule(pattern="market", category="convenience", priority=5, kind="substring")
        txn = _txn(payee="CORNER MARKET")
        self.assertEqual(categorize(txn, [earliest, later]), "groceries")
        self.assertEqual(categorize(txn, [later, earliest]), "convenience")

    def test_no_match_returns_none(self):
        rule = Rule(pattern="netflix", category="subscriptions", priority=1, kind="substring")
        txn = _txn(payee="ACME WIDGETS")
        self.assertIsNone(categorize(txn, [rule]))


if __name__ == "__main__":
    unittest.main()
