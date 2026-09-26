import contextlib
import io
import json
import tempfile
import unittest
from datetime import date
from decimal import Decimal
from pathlib import Path

from ledgerlite.cli import main
from ledgerlite.goals import Goal, progress, reached_milestones
from ledgerlite.models import Transaction


def _run(args):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = main(args)
    return code, out.getvalue(), err.getvalue()


def _txn(id, amount, category):
    return Transaction(id=id, date=date(2024, 1, id), amount=Decimal(amount), payee="P", memo="", category=category)


# ---- Phase 1: model -------------------------------------------------------

class TestPhase1Model(unittest.TestCase):
    def test_progress_sums_abs_amount_for_matching_categories(self):
        goal = Goal(name="Vacation", target=Decimal("500"), categories=["travel", "flights"])
        txns = [
            _txn(1, "-100.00", "travel"),
            _txn(2, "-50.00", "flights"),
            _txn(3, "-999.00", "groceries"),
        ]
        self.assertEqual(progress(goal, txns), Decimal("150.00"))

    def test_progress_zero_with_no_matches(self):
        goal = Goal(name="Vacation", target=Decimal("500"), categories=["travel"])
        self.assertEqual(progress(goal, []), Decimal("0"))

    def test_goal_defaults(self):
        goal = Goal(name="G", target=Decimal("100"), categories=["x"])
        self.assertIsNone(goal.target_date)
        self.assertEqual(goal.milestones, [])


# ---- Phase 2: persistence + add/list --------------------------------------

class TestPhase2AddList(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")

    def test_add_persists_under_goals_key(self):
        code, out, _err = _run([
            "goal", "add", "--name", "Vacation", "--target", "500", "--category", "travel",
            "--store", self.store_path,
        ])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "Added goal Vacation")
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertEqual(len(raw["goals"]), 1)
        self.assertEqual(raw["goals"][0]["name"], "Vacation")

    def test_add_with_target_date_and_multiple_categories(self):
        _run(["goal", "add", "--name", "Trip", "--target", "1000", "--category", "travel",
              "--category", "flights", "--target-date", "2024-12-01", "--store", self.store_path])
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertEqual(raw["goals"][0]["categories"], ["travel", "flights"])
        self.assertEqual(raw["goals"][0]["target_date"], "2024-12-01")

    def test_list_format_with_and_without_target_date(self):
        _run(["goal", "add", "--name", "Trip", "--target", "1000", "--category", "travel",
              "--target-date", "2024-12-01", "--store", self.store_path])
        _run(["goal", "add", "--name", "Emergency", "--target", "2000", "--category", "savings",
              "--store", self.store_path])
        code, out, _err = _run(["goal", "list", "--store", self.store_path])
        self.assertEqual(code, 0)
        lines = out.strip().splitlines()
        self.assertEqual(lines[0], "Trip: target 1000 categories travel by 2024-12-01")
        self.assertEqual(lines[1], "Emergency: target 2000 categories savings")

    def test_list_no_goals_message(self):
        code, out, _err = _run(["goal", "list", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "No goals.")


# ---- Phase 3: progress -----------------------------------------------------

class TestPhase3Progress(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")
        Path(self.store_path).write_text(json.dumps({
            "transactions": [
                {"id": 1, "date": "2024-01-01", "amount": "-100.00", "payee": "P", "memo": "", "category": "travel"},
                {"id": 2, "date": "2024-01-02", "amount": "-150.00", "payee": "P", "memo": "", "category": "travel"},
            ],
            "goals": [
                {"name": "Vacation", "target": "500", "categories": ["travel"], "target_date": None, "milestones": []},
            ],
        }), encoding="utf-8")

    def test_progress_output_format(self):
        code, out, _err = _run(["goal", "progress", "Vacation", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "Vacation: 250.00/500.00 (50.0%)")

    def test_progress_unknown_name_errors(self):
        code, out, err = _run(["goal", "progress", "Nope", "--store", self.store_path])
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertTrue(err.strip())


# ---- Phase 4: milestones ----------------------------------------------------

class TestPhase4Milestones(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")

    def test_add_with_milestones_deduped_and_sorted(self):
        _run(["goal", "add", "--name", "Vacation", "--target", "500", "--category", "travel",
              "--milestone", "50", "--milestone", "25", "--milestone", "50",
              "--store", self.store_path])
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertEqual(raw["goals"][0]["milestones"], [25, 50])

    def test_reached_milestones_exact_boundary(self):
        goal = Goal(name="G", target=Decimal("200"), categories=["x"], milestones=[25, 50, 75, 100])
        txns = [_txn(1, "-100.00", "x")]  # exactly 50%
        self.assertEqual(reached_milestones(goal, txns), [25, 50])

    def test_reached_milestones_none_reached(self):
        goal = Goal(name="G", target=Decimal("200"), categories=["x"], milestones=[25, 50])
        self.assertEqual(reached_milestones(goal, []), [])

    def test_progress_command_shows_milestones_reached(self):
        Path(self.store_path).write_text(json.dumps({
            "transactions": [
                {"id": 1, "date": "2024-01-01", "amount": "-100.00", "payee": "P", "memo": "", "category": "travel"},
            ],
            "goals": [
                {"name": "Vacation", "target": "200", "categories": ["travel"], "target_date": None,
                 "milestones": [25, 50, 75]},
            ],
        }), encoding="utf-8")
        code, out, _err = _run(["goal", "progress", "Vacation", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "Vacation: 100.00/200.00 (50.0%) milestones reached: 25,50")

    def test_progress_command_shows_none_reached(self):
        Path(self.store_path).write_text(json.dumps({
            "transactions": [],
            "goals": [
                {"name": "Vacation", "target": "200", "categories": ["travel"], "target_date": None,
                 "milestones": [25, 50]},
            ],
        }), encoding="utf-8")
        code, out, _err = _run(["goal", "progress", "Vacation", "--store", self.store_path])
        self.assertEqual(out.strip(), "Vacation: 0.00/200.00 (0.0%) milestones reached: none")

    def test_progress_command_no_suffix_without_milestones(self):
        Path(self.store_path).write_text(json.dumps({
            "transactions": [],
            "goals": [
                {"name": "Vacation", "target": "200", "categories": ["travel"], "target_date": None,
                 "milestones": []},
            ],
        }), encoding="utf-8")
        code, out, _err = _run(["goal", "progress", "Vacation", "--store", self.store_path])
        self.assertEqual(out.strip(), "Vacation: 0.00/200.00 (0.0%)")


# ---- Phase 5: report --------------------------------------------------------

class TestPhase5Report(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")

    def test_report_header_only_when_empty(self):
        code, out, _err = _run(["goal", "report", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip("\n"), "Goal            Progress")

    def test_report_sorted_by_percent_desc_then_name(self):
        Path(self.store_path).write_text(json.dumps({
            "transactions": [
                {"id": 1, "date": "2024-01-01", "amount": "-100.00", "payee": "P", "memo": "", "category": "a"},
                {"id": 2, "date": "2024-01-02", "amount": "-10.00", "payee": "P", "memo": "", "category": "b"},
            ],
            "goals": [
                {"name": "Alpha", "target": "1000", "categories": ["a"], "target_date": None, "milestones": []},
                {"name": "Beta", "target": "100", "categories": ["a"], "target_date": None, "milestones": []},
                {"name": "Gamma", "target": "1000", "categories": ["b"], "target_date": None, "milestones": []},
            ],
        }), encoding="utf-8")
        code, out, _err = _run(["goal", "report", "--store", self.store_path])
        self.assertEqual(code, 0)
        lines = out.strip("\n").splitlines()
        names_in_order = [ln.split()[0] for ln in lines[1:]]
        self.assertEqual(names_in_order, ["Beta", "Alpha", "Gamma"])

    def test_report_row_format(self):
        Path(self.store_path).write_text(json.dumps({
            "transactions": [
                {"id": 1, "date": "2024-01-01", "amount": "-50.00", "payee": "P", "memo": "", "category": "a"},
            ],
            "goals": [
                {"name": "Alpha", "target": "100", "categories": ["a"], "target_date": None, "milestones": []},
            ],
        }), encoding="utf-8")
        code, out, _err = _run(["goal", "report", "--store", self.store_path])
        lines = out.strip("\n").splitlines()
        self.assertEqual(lines[1], f"{'Alpha':<16}{50.0:>6.1f}%  50.00/100.00")


# ---- Phase 6: overall total --------------------------------------------------

class TestPhase6Overall(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")

    def test_overall_total_no_goals(self):
        code, out, _err = _run(["goal", "report", "--store", self.store_path, "--overall"])
        self.assertEqual(code, 0)
        lines = out.strip("\n").splitlines()
        self.assertEqual(lines[-1], "TOTAL: 0.0%")

    def test_overall_total_combines_goals(self):
        Path(self.store_path).write_text(json.dumps({
            "transactions": [
                {"id": 1, "date": "2024-01-01", "amount": "-100.00", "payee": "P", "memo": "", "category": "a"},
                {"id": 2, "date": "2024-01-02", "amount": "-100.00", "payee": "P", "memo": "", "category": "b"},
            ],
            "goals": [
                {"name": "Alpha", "target": "200", "categories": ["a"], "target_date": None, "milestones": []},
                {"name": "Beta", "target": "200", "categories": ["b"], "target_date": None, "milestones": []},
            ],
        }), encoding="utf-8")
        code, out, _err = _run(["goal", "report", "--store", self.store_path, "--overall"])
        self.assertEqual(code, 0)
        lines = out.strip("\n").splitlines()
        # total progress 200 / total target 400 = 50.0%
        self.assertEqual(lines[-1], "TOTAL: 50.0%")

    def test_report_without_overall_has_no_total_line(self):
        code, out, _err = _run(["goal", "report", "--store", self.store_path])
        self.assertNotIn("TOTAL", out)


# ---- Phase 7: validation + remove -------------------------------------------

class TestPhase7ValidationRemove(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")

    def test_add_rejects_nonpositive_target(self):
        code, out, err = _run(["goal", "add", "--name", "Bad", "--target", "0", "--category", "x",
                                "--store", self.store_path])
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertTrue(err.strip())
        self.assertFalse(Path(self.store_path).exists())

    def test_add_rejects_negative_target(self):
        code, out, err = _run(["goal", "add", "--name", "Bad", "--target", "-5", "--category", "x",
                                "--store", self.store_path])
        self.assertEqual(code, 2)
        self.assertEqual(out, "")

    def test_add_rejects_duplicate_name(self):
        _run(["goal", "add", "--name", "Vacation", "--target", "100", "--category", "x",
              "--store", self.store_path])
        code, out, err = _run(["goal", "add", "--name", "Vacation", "--target", "200",
                                "--category", "y", "--store", self.store_path])
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertEqual(len(raw["goals"]), 1)

    def test_remove_existing_goal(self):
        _run(["goal", "add", "--name", "Vacation", "--target", "100", "--category", "x",
              "--store", self.store_path])
        code, out, _err = _run(["goal", "remove", "Vacation", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "Removed goal Vacation")
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertEqual(raw["goals"], [])

    def test_remove_unknown_goal_errors(self):
        code, out, err = _run(["goal", "remove", "Nope", "--store", self.store_path])
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertTrue(err.strip())


if __name__ == "__main__":
    unittest.main()
