import csv
import tempfile
import unittest
from decimal import Decimal
from pathlib import Path

from ledgerlite.models import parse_money
from ledgerlite.parse import read_csv


class TestParseMoneyHidden(unittest.TestCase):
    def test_parenthesized_amount_is_negative(self):
        self.assertEqual(parse_money("(12.00)"), Decimal("-12.00"))

    def test_parenthesized_amount_with_dollar_sign(self):
        self.assertEqual(parse_money("($45.67)"), Decimal("-45.67"))

    def test_thousands_separator_is_parsed(self):
        self.assertEqual(parse_money("1,234.50"), Decimal("1234.50"))

    def test_negative_thousands_separator(self):
        self.assertEqual(parse_money("-1,234.50"), Decimal("-1234.50"))

    def test_plain_amount_still_works(self):
        self.assertEqual(parse_money("12.34"), Decimal("12.34"))


class TestReadCsvHidden(unittest.TestCase):
    def test_read_csv_with_edge_case_amounts(self):
        tmp = tempfile.NamedTemporaryFile(mode="w", suffix=".csv", delete=False, newline="")
        writer = csv.writer(tmp)
        writer.writerow(["date", "amount", "payee", "memo", "category"])
        writer.writerow(["2024-01-01", "(12.00)", "Refund Co", "return", ""])
        writer.writerow(["2024-01-02", "1,234.50", "Big Ticket Store", "purchase", ""])
        writer.writerow(["2024-01-03", "-45.00", "Normal Store", "groceries", ""])
        tmp.close()
        self.addCleanup(lambda: Path(tmp.name).unlink(missing_ok=True))

        txns = read_csv(tmp.name)
        self.assertEqual(len(txns), 3)
        self.assertEqual(txns[0].amount, Decimal("-12.00"))
        self.assertEqual(txns[1].amount, Decimal("1234.50"))
        self.assertEqual(txns[2].amount, Decimal("-45.00"))


if __name__ == "__main__":
    unittest.main()
