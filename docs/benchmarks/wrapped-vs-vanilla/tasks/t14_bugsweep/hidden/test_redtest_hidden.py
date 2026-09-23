import unittest
from datetime import date
from decimal import Decimal

from ledgerlite.models import Transaction
from ledgerlite.rules import Rule, categorize


def _txn(payee="", memo=""):
    return Transaction(id=1, date=date(2024, 1, 1), amount=Decimal("-10.00"), payee=payee, memo=memo)


class TestRegexCaseInsensitivity(unittest.TestCase):
    def test_regex_rule_case_insensitive(self):
        # Same case as tests/test_rules.py::test_regex_rule_case_insensitive.
        rule = Rule(pattern=r"cafe\d+", category="dining", priority=1, kind="regex")
        txn = _txn(payee="CAFE42 DOWNTOWN")
        self.assertEqual(categorize(txn, [rule]), "dining")

    def test_regex_rule_case_insensitive_uppercase_pattern(self):
        rule = Rule(pattern=r"MARKET", category="groceries", priority=1, kind="regex")
        txn = _txn(payee="corner market square")
        self.assertEqual(categorize(txn, [rule]), "groceries")

    def test_regex_rule_case_insensitive_matches_memo(self):
        rule = Rule(pattern=r"refund", category="income", priority=1, kind="regex")
        txn = _txn(payee="Some Store", memo="REFUND ISSUED")
        self.assertEqual(categorize(txn, [rule]), "income")


if __name__ == "__main__":
    unittest.main()
