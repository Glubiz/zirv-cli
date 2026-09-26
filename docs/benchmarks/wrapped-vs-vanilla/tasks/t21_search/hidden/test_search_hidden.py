import contextlib
import io
import json
import tempfile
import unittest
from datetime import date
from decimal import Decimal
from pathlib import Path

from ledgerlite.cli import main
from ledgerlite.models import Transaction
from ledgerlite.report import search


def _run(args):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = main(args)
    return code, out.getvalue(), err.getvalue()


def _txn(id, date_, amount, payee, category=None, memo=""):
    return Transaction(id=id, date=date_, amount=Decimal(amount), payee=payee, memo=memo, category=category)


SAMPLE = [
    _txn(1, date(2024, 1, 1), "-10.00", "Whole Foods Market", "groceries"),
    _txn(2, date(2024, 1, 5), "-50.00", "Landlord", "rent"),
    _txn(3, date(2024, 1, 10), "20.00", "Refund Store", None),
    _txn(4, date(2024, 1, 15), "-5.00", "whole foods express", "groceries"),
]


class TestReportSearchFunction(unittest.TestCase):
    def test_no_filters_matches_all(self):
        self.assertEqual(search(SAMPLE), SAMPLE)

    def test_payee_contains_case_insensitive(self):
        result = search(SAMPLE, payee_contains="WHOLE FOODS")
        self.assertEqual([t.id for t in result], [1, 4])

    def test_min_and_max_amount_inclusive_signed(self):
        result = search(SAMPLE, min_amount=Decimal("-10.00"), max_amount=Decimal("-5.00"))
        self.assertEqual([t.id for t in result], [1, 4])

    def test_category_exact_match(self):
        result = search(SAMPLE, category="rent")
        self.assertEqual([t.id for t in result], [2])

    def test_uncategorized_only(self):
        result = search(SAMPLE, uncategorized_only=True)
        self.assertEqual([t.id for t in result], [3])

    def test_date_range_inclusive(self):
        result = search(SAMPLE, date_from=date(2024, 1, 5), date_to=date(2024, 1, 10))
        self.assertEqual([t.id for t in result], [2, 3])

    def test_filters_combine_with_and(self):
        result = search(SAMPLE, payee_contains="whole foods", category="groceries", max_amount=Decimal("-8.00"))
        self.assertEqual([t.id for t in result], [1])

    def test_preserves_original_order(self):
        reversed_sample = list(reversed(SAMPLE))
        result = search(reversed_sample, category="groceries")
        self.assertEqual([t.id for t in result], [4, 1])


class TestCliSearch(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")
        Path(self.store_path).write_text(json.dumps({"transactions": [
            {"id": 1, "date": "2024-01-01", "amount": "-10.00", "payee": "Whole Foods Market", "memo": "m1",
             "category": "groceries"},
            {"id": 2, "date": "2024-01-05", "amount": "-50.00", "payee": "Landlord", "memo": "m2",
             "category": "rent"},
            {"id": 3, "date": "2024-01-10", "amount": "20.00", "payee": "Refund Store", "memo": "m3",
             "category": None},
            {"id": 4, "date": "2024-01-15", "amount": "-5.00", "payee": "whole foods express", "memo": "m4",
             "category": "groceries"},
        ]}), encoding="utf-8")

    def test_no_filters_returns_all_in_date_order(self):
        code, out, _err = _run(["search", "--store", self.store_path])
        self.assertEqual(code, 0)
        lines = [ln for ln in out.strip().splitlines() if not ln.strip().startswith("ID")]
        self.assertEqual(len(lines), 4)

    def test_payee_contains_filter(self):
        code, out, _err = _run(["search", "--store", self.store_path, "--payee-contains", "landlord"])
        self.assertEqual(code, 0)
        self.assertIn("Landlord", out)
        self.assertNotIn("Whole Foods", out)

    def test_category_and_uncategorized_both_given_errors(self):
        code, out, err = _run(["search", "--store", self.store_path, "--category", "rent",
                                "--uncategorized-only"])
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertTrue(err.strip())

    def test_min_greater_than_max_errors(self):
        code, out, err = _run(["search", "--store", self.store_path, "--min-amount", "10",
                                "--max-amount", "-10"])
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertTrue(err.strip())

    def test_no_matches_message(self):
        code, out, _err = _run(["search", "--store", self.store_path, "--payee-contains", "nonexistent"])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "No transactions match those criteria.")

    def test_sort_by_amount_ascending(self):
        code, out, _err = _run(["search", "--store", self.store_path, "--sort", "amount"])
        self.assertEqual(code, 0)
        lines = [ln for ln in out.strip().splitlines() if not ln.strip().startswith("ID")]
        ids_in_order = []
        for ln in lines:
            ids_in_order.append(int(ln.strip().split()[0]))
        self.assertEqual(ids_in_order, [2, 1, 4, 3])

    def test_sort_by_payee_case_insensitive(self):
        code, out, _err = _run(["search", "--store", self.store_path, "--sort", "payee"])
        self.assertEqual(code, 0)
        lines = [ln for ln in out.strip().splitlines() if not ln.strip().startswith("ID")]
        ids_in_order = [int(ln.strip().split()[0]) for ln in lines]
        # Landlord, Refund Store, whole foods express, Whole Foods Market
        # (case-insensitive: "express" < "market")
        self.assertEqual(ids_in_order, [2, 3, 4, 1])

    def test_default_sort_is_date(self):
        code, out, _err = _run(["search", "--store", self.store_path])
        lines = [ln for ln in out.strip().splitlines() if not ln.strip().startswith("ID")]
        ids_in_order = [int(ln.strip().split()[0]) for ln in lines]
        self.assertEqual(ids_in_order, [1, 2, 3, 4])

    def test_limit_caps_rows(self):
        code, out, _err = _run(["search", "--store", self.store_path, "--limit", "2"])
        self.assertEqual(code, 0)
        lines = [ln for ln in out.strip().splitlines() if not ln.strip().startswith("ID")]
        self.assertEqual(len(lines), 2)
        ids_in_order = [int(ln.strip().split()[0]) for ln in lines]
        self.assertEqual(ids_in_order, [1, 2])

    def test_row_format_matches_list_command(self):
        code, search_out, _err = _run(["search", "--store", self.store_path, "--payee-contains", "Landlord"])
        _code, list_out, _err2 = _run(["list", "--store", self.store_path, "--page-size", "10"])
        # The Landlord row text (id, date, amount, payee, category, memo) must
        # appear identically in both outputs.
        landlord_line_search = [ln for ln in search_out.splitlines() if "Landlord" in ln][0]
        landlord_line_list = [ln for ln in list_out.splitlines() if "Landlord" in ln][0]
        self.assertEqual(landlord_line_search, landlord_line_list)

    def test_uncategorized_only_cli(self):
        code, out, _err = _run(["search", "--store", self.store_path, "--uncategorized-only"])
        self.assertEqual(code, 0)
        self.assertIn("Refund Store", out)
        self.assertNotIn("Landlord", out)

    def test_date_range_cli(self):
        code, out, _err = _run(["search", "--store", self.store_path, "--since", "2024-01-05",
                                 "--until", "2024-01-10"])
        self.assertEqual(code, 0)
        self.assertIn("Landlord", out)
        self.assertIn("Refund Store", out)
        self.assertNotIn("Whole Foods Market", out)


if __name__ == "__main__":
    unittest.main()
