import contextlib
import csv
import io
import json
import tempfile
import unittest
from datetime import date
from decimal import Decimal
from pathlib import Path

from ledgerlite import report
from ledgerlite.cli import main
from ledgerlite.models import Transaction


def _txn(i, d, amount, category=None):
    return Transaction(id=i, date=d, amount=Decimal(amount), payee="x", memo="", category=category)


def _run(args):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = main(args)
    return code, out.getvalue(), err.getvalue()


def _run_expect_systemexit(args):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        try:
            main(args)
            code = None
        except SystemExit as exc:
            code = exc.code
    return code, out.getvalue(), err.getvalue()


class TestMonthlyBreakdown(unittest.TestCase):
    def test_groups_by_month_and_category(self):
        txns = [
            _txn(1, date(2024, 1, 5), "-45.67", "groceries"),
            _txn(2, date(2024, 1, 10), "2500.00", "income"),
            _txn(3, date(2024, 1, 20), "-6.75", "groceries"),
        ]
        result = report.monthly_breakdown(txns, 2024)
        self.assertEqual(result[1]["groceries"], Decimal("-52.42"))
        self.assertEqual(result[1]["income"], Decimal("2500.00"))

    def test_gap_month_is_omitted(self):
        txns = [
            _txn(1, date(2024, 1, 5), "-10.00", "groceries"),
            _txn(2, date(2024, 3, 5), "-10.00", "groceries"),
        ]
        result = report.monthly_breakdown(txns, 2024)
        self.assertEqual(set(result.keys()), {1, 3})
        self.assertNotIn(2, result)

    def test_uncategorised_grouping(self):
        txns = [_txn(1, date(2024, 1, 5), "-10.00", None)]
        result = report.monthly_breakdown(txns, 2024)
        self.assertEqual(result[1], {"uncategorised": Decimal("-10.00")})

    def test_filters_by_year_only(self):
        txns = [
            _txn(1, date(2023, 1, 5), "-10.00", "groceries"),
            _txn(2, date(2024, 1, 5), "-20.00", "groceries"),
        ]
        result = report.monthly_breakdown(txns, 2024)
        self.assertEqual(result, {1: {"groceries": Decimal("-20.00")}})

    def test_empty_txns_returns_empty_dict(self):
        self.assertEqual(report.monthly_breakdown([], 2024), {})


class TestTrend(unittest.TestCase):
    def test_basic_trend_with_zero_filled_gap(self):
        txns = [
            _txn(1, date(2023, 12, 31), "-5.00", "groceries"),
            _txn(2, date(2024, 1, 5), "-45.67", "groceries"),
            _txn(3, date(2024, 3, 15), "-30.33", "groceries"),
        ]
        result = report.trend(txns, "groceries", 4)
        self.assertEqual(
            result,
            [
                ("2023-12", Decimal("-5.00")),
                ("2024-01", Decimal("-45.67")),
                ("2024-02", Decimal("0")),
                ("2024-03", Decimal("-30.33")),
            ],
        )

    def test_anchor_is_latest_overall_txn_not_just_matching_category(self):
        txns = [
            _txn(1, date(2024, 1, 5), "-45.67", "groceries"),
            _txn(2, date(2024, 3, 1), "500.00", "income"),
        ]
        result = report.trend(txns, "groceries", 3)
        self.assertEqual([m for m, _ in result], ["2024-01", "2024-02", "2024-03"])
        self.assertEqual(result[-1], ("2024-03", Decimal("0")))

    def test_empty_when_no_txns(self):
        self.assertEqual(report.trend([], "groceries", 3), [])

    def test_result_has_exactly_months_entries(self):
        txns = [_txn(1, date(2024, 6, 15), "-10.00", "rent")]
        result = report.trend(txns, "rent", 6)
        self.assertEqual(len(result), 6)
        self.assertEqual(result[0][0], "2024-01")
        self.assertEqual(result[-1][0], "2024-06")

    def test_no_matching_category_is_all_zero(self):
        txns = [_txn(1, date(2024, 1, 15), "-10.00", "rent")]
        result = report.trend(txns, "groceries", 2)
        self.assertEqual(result, [("2023-12", Decimal("0")), ("2024-01", Decimal("0"))])


class TestReportCli(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")
        txns = [
            {"id": 1, "date": "2024-01-05", "amount": "-45.67", "payee": "Whole Foods", "memo": "", "category": "groceries"},
            {"id": 2, "date": "2024-01-10", "amount": "2500.00", "payee": "Employer", "memo": "", "category": "income"},
            {"id": 3, "date": "2024-01-20", "amount": "-6.75", "payee": "Cafe", "memo": "", "category": None},
            {"id": 4, "date": "2024-03-03", "amount": "-20.00", "payee": "Gym", "memo": "", "category": "fitness"},
            {"id": 5, "date": "2024-03-15", "amount": "-30.33", "payee": "Whole Foods", "memo": "", "category": "groceries"},
        ]
        Path(self.store_path).write_text(json.dumps({"transactions": txns}), encoding="utf-8")

    def test_year_table_format(self):
        code, out, _err = _run(["report", "--year", "2024", "--store", self.store_path])
        self.assertEqual(code, 0)
        lines = out.splitlines()
        self.assertEqual(lines[0], "Month    Category            Amount")
        self.assertIn("2024-01  groceries           -45.67", lines)
        self.assertIn("2024-01  income             2500.00", lines)
        self.assertIn("2024-01  uncategorised        -6.75", lines)
        self.assertIn("2024-03  fitness             -20.00", lines)
        self.assertIn("2024-03  groceries           -30.33", lines)
        # February has no transactions and must not appear.
        self.assertFalse(any(ln.startswith("2024-02") for ln in lines))

    def test_year_csv_format(self):
        code, out, _err = _run(["report", "--year", "2024", "--format", "csv", "--store", self.store_path])
        self.assertEqual(code, 0)
        rows = list(csv.reader(io.StringIO(out)))
        self.assertEqual(rows[0], ["Month", "Category", "Amount"])
        self.assertIn(["2024-01", "groceries", "-45.67"], rows)
        self.assertIn(["2024-01", "income", "2500.00"], rows)
        self.assertIn(["2024-01", "uncategorised", "-6.75"], rows)
        self.assertIn(["2024-03", "fitness", "-20.00"], rows)
        self.assertIn(["2024-03", "groceries", "-30.33"], rows)

    def test_year_with_no_transactions_prints_header_only(self):
        code, out, _err = _run(["report", "--year", "2099", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out, "Month    Category            Amount\n")

    def test_trend_line_format(self):
        code, out, _err = _run(["report", "--trend", "groceries", "--months", "4", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(
            out.strip().splitlines(),
            ["2023-12: 0.00", "2024-01: -45.67", "2024-02: 0.00", "2024-03: -30.33"],
        )

    def test_trend_without_months_exits_2(self):
        code, out, err = _run(["report", "--trend", "groceries", "--store", self.store_path])
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertTrue(err.strip())

    def test_year_and_trend_together_is_rejected(self):
        code, out, _err = _run_expect_systemexit(
            ["report", "--year", "2024", "--trend", "groceries", "--months", "2", "--store", self.store_path]
        )
        self.assertEqual(code, 2)
        self.assertEqual(out, "")

    def test_neither_year_nor_trend_is_rejected(self):
        code, out, _err = _run_expect_systemexit(["report", "--store", self.store_path])
        self.assertEqual(code, 2)
        self.assertEqual(out, "")


if __name__ == "__main__":
    unittest.main()
