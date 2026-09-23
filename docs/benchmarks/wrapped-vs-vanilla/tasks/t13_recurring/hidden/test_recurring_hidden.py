import contextlib
import io
import json
import tempfile
import unittest
from datetime import date
from decimal import Decimal
from pathlib import Path

from ledgerlite.cli import main
from ledgerlite.recurring import Recurrence, expand


def _run(args):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = main(args)
    return code, out.getvalue(), err.getvalue()


class TestExpand(unittest.TestCase):
    def test_weekly_basic(self):
        rec = Recurrence(payee="Gym", amount=Decimal("-20.00"), category=None,
                          start=date(2024, 1, 1), every="weekly", until=None)
        result = expand([rec], date(2024, 1, 1), date(2024, 1, 22))
        self.assertEqual(
            [t.date for t in result],
            [date(2024, 1, 1), date(2024, 1, 8), date(2024, 1, 15), date(2024, 1, 22)],
        )

    def test_monthly_clamps_to_month_end_then_restores(self):
        rec = Recurrence(payee="Rent", amount=Decimal("-800.00"), category="rent",
                          start=date(2024, 1, 31), every="monthly", until=None)
        result = expand([rec], date(2024, 1, 1), date(2024, 4, 30))
        self.assertEqual(
            [t.date for t in result],
            [date(2024, 1, 31), date(2024, 2, 29), date(2024, 3, 31), date(2024, 4, 30)],
        )

    def test_monthly_non_leap_year_clamp(self):
        rec = Recurrence(payee="Rent", amount=Decimal("-800.00"), category=None,
                          start=date(2023, 1, 31), every="monthly", until=None)
        result = expand([rec], date(2023, 1, 1), date(2023, 3, 31))
        self.assertEqual(
            [t.date for t in result],
            [date(2023, 1, 31), date(2023, 2, 28), date(2023, 3, 31)],
        )

    def test_yearly_leap_day_clamped_in_non_leap_years(self):
        rec = Recurrence(payee="Anniversary", amount=Decimal("-1.00"), category=None,
                          start=date(2020, 2, 29), every="yearly", until=None)
        result = expand([rec], date(2020, 1, 1), date(2023, 12, 31))
        self.assertEqual(
            [t.date for t in result],
            [date(2020, 2, 29), date(2021, 2, 28), date(2022, 2, 28), date(2023, 2, 28)],
        )

    def test_yearly_leap_day_exact_in_leap_year(self):
        rec = Recurrence(payee="Anniversary", amount=Decimal("-1.00"), category=None,
                          start=date(2020, 2, 29), every="yearly", until=None)
        result = expand([rec], date(2023, 6, 1), date(2024, 12, 31))
        self.assertEqual([t.date for t in result], [date(2024, 2, 29)])

    def test_until_bound_stops_occurrences(self):
        rec = Recurrence(payee="Gym", amount=Decimal("-20.00"), category=None,
                          start=date(2024, 1, 1), every="weekly", until=date(2024, 1, 15))
        result = expand([rec], date(2024, 1, 1), date(2024, 12, 31))
        self.assertEqual(
            [t.date for t in result],
            [date(2024, 1, 1), date(2024, 1, 8), date(2024, 1, 15)],
        )

    def test_range_start_clips_early_occurrences_and_ids_only_count_yielded(self):
        rec = Recurrence(payee="Gym", amount=Decimal("-20.00"), category=None,
                          start=date(2024, 1, 1), every="weekly", until=None)
        result = expand([rec], date(2024, 1, 10), date(2024, 1, 31))
        self.assertEqual(
            [t.date for t in result],
            [date(2024, 1, 15), date(2024, 1, 22), date(2024, 1, 29)],
        )
        self.assertEqual([t.id for t in result], [-1, -2, -3])

    def test_range_end_clips_late_occurrences(self):
        rec = Recurrence(payee="Gym", amount=Decimal("-20.00"), category=None,
                          start=date(2024, 1, 1), every="weekly", until=None)
        result = expand([rec], date(2024, 1, 1), date(2024, 1, 10))
        self.assertEqual([t.date for t in result], [date(2024, 1, 1), date(2024, 1, 8)])

    def test_recurrence_never_produces_occurrence_before_its_own_start(self):
        rec = Recurrence(payee="Gym", amount=Decimal("-20.00"), category=None,
                          start=date(2024, 3, 1), every="monthly", until=None)
        result = expand([rec], date(2024, 1, 1), date(2024, 12, 31))
        self.assertTrue(all(t.date >= date(2024, 3, 1) for t in result))
        self.assertEqual(result[0].date, date(2024, 3, 1))

    def test_ids_negative_and_ordered_recurrence_by_recurrence(self):
        rent = Recurrence(payee="Rent", amount=Decimal("-800.00"), category="rent",
                           start=date(2024, 1, 31), every="monthly", until=None)
        gym = Recurrence(payee="Gym", amount=Decimal("-20.00"), category=None,
                          start=date(2024, 1, 1), every="weekly", until=None)
        result = expand([rent, gym], date(2024, 1, 1), date(2024, 1, 31))
        self.assertEqual([t.payee for t in result], ["Rent", "Gym", "Gym", "Gym", "Gym", "Gym"])
        ids = [t.id for t in result]
        self.assertEqual(ids, [-1, -2, -3, -4, -5, -6])
        self.assertTrue(all(i < 0 for i in ids))
        self.assertEqual(len(ids), len(set(ids)))

    def test_ids_deterministic_across_calls(self):
        rec = Recurrence(payee="Gym", amount=Decimal("-20.00"), category=None,
                          start=date(2024, 1, 1), every="weekly", until=None)
        first = [(t.date, t.id) for t in expand([rec], date(2024, 1, 1), date(2024, 2, 1))]
        second = [(t.date, t.id) for t in expand([rec], date(2024, 1, 1), date(2024, 2, 1))]
        self.assertEqual(first, second)

    def test_empty_recurrences_returns_empty_list(self):
        self.assertEqual(expand([], date(2024, 1, 1), date(2024, 12, 31)), [])

    def test_generated_transactions_have_empty_memo(self):
        rec = Recurrence(payee="Gym", amount=Decimal("-20.00"), category=None,
                          start=date(2024, 1, 1), every="weekly", until=None)
        result = expand([rec], date(2024, 1, 1), date(2024, 1, 1))
        self.assertEqual(result[0].memo, "")


class TestRecurringCli(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")
        Path(self.store_path).write_text(
            json.dumps({"transactions": [
                {"id": 1, "date": "2024-01-05", "amount": "-10.00", "payee": "Coffee", "memo": "", "category": "dining"},
            ]}),
            encoding="utf-8",
        )

    def test_recurring_add_persists_under_recurring_key_without_touching_transactions(self):
        code, _out, _err = _run([
            "recurring", "add", "--payee", "Landlord", "--amount", "-800.00",
            "--every", "monthly", "--start", "2024-01-31", "--category", "rent",
            "--store", self.store_path,
        ])
        self.assertEqual(code, 0)
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertIn("recurring", raw)
        self.assertEqual(len(raw["recurring"]), 1)
        self.assertEqual(len(raw["transactions"]), 1)
        self.assertEqual(raw["transactions"][0]["payee"], "Coffee")

    def test_recurring_add_multiple_appends_in_order(self):
        _run(["recurring", "add", "--payee", "Landlord", "--amount", "-800.00",
              "--every", "monthly", "--start", "2024-01-31", "--store", self.store_path])
        _run(["recurring", "add", "--payee", "Gym", "--amount", "-20.00",
              "--every", "weekly", "--start", "2024-01-01", "--store", self.store_path])
        code, out, _err = _run(["recurring", "list", "--store", self.store_path])
        self.assertEqual(code, 0)
        lines = out.strip().splitlines()
        self.assertEqual(len(lines), 2)
        self.assertTrue(lines[0].startswith("Landlord:"))
        self.assertTrue(lines[1].startswith("Gym:"))

    def test_recurring_list_output_format_with_until(self):
        _run(["recurring", "add", "--payee", "Gym", "--amount", "-20.00", "--every", "weekly",
              "--start", "2024-01-01", "--until", "2024-01-31", "--category", "fitness",
              "--store", self.store_path])
        code, out, _err = _run(["recurring", "list", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(
            out.strip(),
            "Gym: -20.00 fitness every weekly from 2024-01-01 until 2024-01-31",
        )

    def test_recurring_list_output_format_without_until_and_uncategorized(self):
        _run(["recurring", "add", "--payee", "Landlord", "--amount", "-800.00", "--every", "monthly",
              "--start", "2024-01-31", "--store", self.store_path])
        code, out, _err = _run(["recurring", "list", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "Landlord: -800.00 uncategorized every monthly from 2024-01-31")


class TestListIncludeRecurring(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")
        Path(self.store_path).write_text(
            json.dumps({"transactions": [
                {"id": 1, "date": "2024-01-05", "amount": "-10.00", "payee": "Coffee", "memo": "", "category": "dining"},
            ]}),
            encoding="utf-8",
        )
        _run(["recurring", "add", "--payee", "Gym", "--amount", "-20.00", "--every", "weekly",
              "--start", "2024-01-01", "--store", self.store_path])

    def test_requires_since_and_until_exits_2(self):
        code, out, err = _run(["list", "--store", self.store_path, "--include-recurring"])
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertTrue(err.strip())

    def test_merges_and_sorts_by_date_then_id(self):
        code, out, _err = _run([
            "list", "--store", self.store_path, "--include-recurring",
            "--since", "2024-01-01", "--until", "2024-01-08",
        ])
        self.assertEqual(code, 0)
        lines = [ln for ln in out.strip().splitlines() if ln and not ln.strip().startswith("ID")]
        payees_in_order = []
        for ln in lines:
            for token in ("Gym", "Coffee"):
                if token in ln:
                    payees_in_order.append((ln, token))
        # Gym (2024-01-01) must appear before Coffee (2024-01-05) which must
        # appear before the second Gym occurrence (2024-01-08).
        idx_gym1 = out.index("2024-01-01")
        idx_coffee = out.index("2024-01-05")
        idx_gym2 = out.index("2024-01-08")
        self.assertTrue(idx_gym1 < idx_coffee < idx_gym2)

    def test_ignores_pagination(self):
        code, out, _err = _run([
            "list", "--store", self.store_path, "--include-recurring",
            "--since", "2024-01-01", "--until", "2024-02-01",
            "--page", "1", "--page-size", "1",
        ])
        self.assertEqual(code, 0)
        # 5 weekly Gym occurrences (Jan 1/8/15/22/29) + 1 stored Coffee txn = 6 rows.
        row_lines = [ln for ln in out.strip().splitlines() if "Gym" in ln or "Coffee" in ln]
        self.assertEqual(len(row_lines), 6)


if __name__ == "__main__":
    unittest.main()
