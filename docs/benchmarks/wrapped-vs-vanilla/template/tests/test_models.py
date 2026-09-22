import unittest
from datetime import date
from decimal import Decimal, InvalidOperation

from ledgerlite.models import Transaction, parse_money


class TestTransaction(unittest.TestCase):
    def test_fields(self):
        txn = Transaction(
            id=1,
            date=date(2024, 3, 1),
            amount=Decimal("-12.50"),
            payee="Acme",
            memo="lunch",
            category="dining",
        )
        self.assertEqual(txn.id, 1)
        self.assertEqual(txn.amount, Decimal("-12.50"))
        self.assertEqual(txn.category, "dining")

    def test_category_defaults_to_none(self):
        txn = Transaction(id=2, date=date(2024, 3, 1), amount=Decimal("10.00"), payee="Acme", memo="")
        self.assertIsNone(txn.category)


class TestParseMoney(unittest.TestCase):
    def test_plain_amount(self):
        self.assertEqual(parse_money("12.34"), Decimal("12.34"))

    def test_negative_amount(self):
        self.assertEqual(parse_money("-12.34"), Decimal("-12.34"))

    def test_dollar_sign_prefix(self):
        self.assertEqual(parse_money("$12.00"), Decimal("12.00"))

    def test_whitespace_is_stripped(self):
        self.assertEqual(parse_money("  5.00  "), Decimal("5.00"))

    def test_empty_string_raises(self):
        with self.assertRaises(InvalidOperation):
            parse_money("")


if __name__ == "__main__":
    unittest.main()
