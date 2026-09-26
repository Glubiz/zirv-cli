import contextlib
import csv
import io
import json
import tempfile
import unittest
from datetime import date
from decimal import Decimal
from pathlib import Path

from ledgerlite.cli import main
from ledgerlite.envelopes import Envelope, balance_through_month, spent_in_month
from ledgerlite.allocation import suggest_allocations
from ledgerlite.models import Transaction


def _run(args):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = main(args)
    return code, out.getvalue(), err.getvalue()


def _txn(id, y, m, d, amount, category):
    return Transaction(id=id, date=date(y, m, d), amount=Decimal(amount), payee="P", memo="", category=category)


# ---- Phase 1: model --------------------------------------------------------

class TestPhase1Model(unittest.TestCase):
    def test_spent_in_month_filters_category_and_month(self):
        txns = [
            _txn(1, 2024, 1, 5, "-30.00", "food"),
            _txn(2, 2024, 1, 10, "-999.00", "rent"),
            _txn(3, 2024, 2, 1, "-999.00", "food"),
        ]
        self.assertEqual(spent_in_month(Envelope("Food", ["food"], Decimal("100"), "2024-01"),
                                         txns, 2024, 1), Decimal("30.00"))

    def test_spent_in_month_no_matches_returns_zero(self):
        env = Envelope("Food", ["food"], Decimal("100"), "2024-01")
        self.assertEqual(spent_in_month(env, [], 2024, 1), Decimal("0"))

    def test_balance_non_rollover_ignores_other_months(self):
        env = Envelope("Food", ["food"], Decimal("100"), "2024-01", rollover=False)
        txns = [_txn(1, 2024, 1, 5, "-30.00", "food"), _txn(2, 2024, 2, 5, "-500.00", "food")]
        self.assertEqual(balance_through_month(env, txns, 2024, 1), Decimal("70.00"))
        self.assertEqual(balance_through_month(env, txns, 2024, 2), Decimal("-400.00"))

    def test_balance_non_rollover_applies_adjustment(self):
        env = Envelope("Food", ["food"], Decimal("100"), "2024-01",
                        adjustments={"2024-01": Decimal("20")})
        self.assertEqual(balance_through_month(env, [], 2024, 1), Decimal("120"))

    def test_balance_rollover_accumulates_across_months(self):
        env = Envelope("Food", ["food"], Decimal("100"), "2024-01", rollover=True)
        txns = [_txn(1, 2024, 1, 5, "-30.00", "food"), _txn(2, 2024, 2, 5, "-120.00", "food"),
                _txn(3, 2024, 3, 5, "-10.00", "food")]
        self.assertEqual(balance_through_month(env, txns, 2024, 1), Decimal("70.00"))
        self.assertEqual(balance_through_month(env, txns, 2024, 2), Decimal("50.00"))
        self.assertEqual(balance_through_month(env, txns, 2024, 3), Decimal("140.00"))

    def test_balance_rollover_uses_closed_through_as_new_base(self):
        env = Envelope("Food", ["food"], Decimal("100"), "2024-01", rollover=True,
                        starting_balance=Decimal("70.00"), closed_through="2024-01")
        txns = [_txn(1, 2024, 2, 5, "-120.00", "food"), _txn(2, 2024, 3, 5, "-10.00", "food")]
        self.assertEqual(balance_through_month(env, txns, 2024, 2), Decimal("50.00"))
        self.assertEqual(balance_through_month(env, txns, 2024, 3), Decimal("140.00"))

    def test_balance_at_closed_through_returns_starting_balance(self):
        env = Envelope("Food", ["food"], Decimal("100"), "2024-01", rollover=True,
                        starting_balance=Decimal("70.00"), closed_through="2024-01")
        self.assertEqual(balance_through_month(env, [], 2024, 1), Decimal("70.00"))

    def test_balance_raises_before_start_month(self):
        env = Envelope("Food", ["food"], Decimal("100"), "2024-03")
        with self.assertRaises(ValueError):
            balance_through_month(env, [], 2024, 2)

    def test_balance_rollover_raises_before_closed_through(self):
        env = Envelope("Food", ["food"], Decimal("100"), "2024-01", rollover=True,
                        closed_through="2024-02")
        with self.assertRaises(ValueError):
            balance_through_month(env, [], 2024, 1)


# ---- Phase 2: persistence + add/list ---------------------------------------

class TestPhase2AddList(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")

    def test_add_persists_under_envelopes_key(self):
        code, out, _err = _run(["envelope", "add", "--name", "Food", "--monthly-budget", "300",
                                 "--category", "food", "--start-month", "2024-01",
                                 "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "Added envelope Food")
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertEqual(len(raw["envelopes"]), 1)
        self.assertEqual(raw["envelopes"][0]["name"], "Food")

    def test_add_rollover_and_starting_balance_stored(self):
        _run(["envelope", "add", "--name", "Food", "--monthly-budget", "300", "--category", "food",
              "--start-month", "2024-01", "--rollover", "--starting-balance", "50",
              "--store", self.store_path])
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertTrue(raw["envelopes"][0]["rollover"])
        self.assertEqual(raw["envelopes"][0]["starting_balance"], "50")

    def test_list_format_with_and_without_rollover(self):
        _run(["envelope", "add", "--name", "Food", "--monthly-budget", "300", "--category", "food",
              "--start-month", "2024-01", "--store", self.store_path])
        _run(["envelope", "add", "--name", "Fun", "--monthly-budget", "50", "--category", "fun",
              "--start-month", "2024-02", "--rollover", "--store", self.store_path])
        code, out, _err = _run(["envelope", "list", "--store", self.store_path])
        self.assertEqual(code, 0)
        lines = out.strip().splitlines()
        self.assertEqual(lines[0], "Food: budget 300/mo categories food from 2024-01")
        self.assertEqual(lines[1], "Fun: budget 50/mo categories fun from 2024-02 (rollover)")

    def test_list_no_envelopes_message(self):
        code, out, _err = _run(["envelope", "list", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "No envelopes.")


# ---- Phase 3: status --------------------------------------------------------

class TestPhase3Status(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")
        Path(self.store_path).write_text(json.dumps({
            "transactions": [
                {"id": 1, "date": "2024-01-05", "amount": "-30.00", "payee": "P", "memo": "", "category": "food"},
            ],
            "envelopes": [
                {"name": "Food", "categories": ["food"], "monthly_budget": "100", "start_month": "2024-01",
                 "rollover": False, "starting_balance": "0", "closed_through": None, "adjustments": {}},
            ],
        }), encoding="utf-8")

    def test_status_output_format(self):
        code, out, _err = _run(["envelope", "status", "Food", "--year", "2024", "--month", "1",
                                 "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "Food: spent 30.00/100.00 for 2024-01, balance 70.00")

    def test_status_unknown_name_errors(self):
        code, out, err = _run(["envelope", "status", "Nope", "--year", "2024", "--month", "1",
                                "--store", self.store_path])
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertTrue(err.strip())

    def test_status_before_start_month_errors(self):
        code, out, err = _run(["envelope", "status", "Food", "--year", "2023", "--month", "12",
                                "--store", self.store_path])
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertTrue(err.strip())


# ---- Phase 4: transfer -------------------------------------------------------

class TestPhase4Transfer(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")
        Path(self.store_path).write_text(json.dumps({
            "transactions": [],
            "envelopes": [
                {"name": "A", "categories": ["a"], "monthly_budget": "100", "start_month": "2024-01",
                 "rollover": False, "starting_balance": "0", "closed_through": None, "adjustments": {}},
                {"name": "B", "categories": ["b"], "monthly_budget": "100", "start_month": "2024-01",
                 "rollover": False, "starting_balance": "0", "closed_through": None, "adjustments": {}},
            ],
        }), encoding="utf-8")

    def test_transfer_moves_amount_between_envelopes(self):
        code, out, _err = _run(["envelope", "transfer", "--from", "A", "--to", "B", "--amount", "25",
                                 "--year", "2024", "--month", "1", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "Transferred 25 from A to B for 2024-01")
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        by_name = {e["name"]: e for e in raw["envelopes"]}
        self.assertEqual(Decimal(by_name["A"]["adjustments"]["2024-01"]), Decimal("-25"))
        self.assertEqual(Decimal(by_name["B"]["adjustments"]["2024-01"]), Decimal("25"))

    def test_transfer_accumulates_existing_adjustment(self):
        _run(["envelope", "transfer", "--from", "A", "--to", "B", "--amount", "10",
              "--year", "2024", "--month", "1", "--store", self.store_path])
        _run(["envelope", "transfer", "--from", "A", "--to", "B", "--amount", "5",
              "--year", "2024", "--month", "1", "--store", self.store_path])
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        by_name = {e["name"]: e for e in raw["envelopes"]}
        self.assertEqual(Decimal(by_name["A"]["adjustments"]["2024-01"]), Decimal("-15"))

    def test_transfer_same_from_to_errors(self):
        code, out, err = _run(["envelope", "transfer", "--from", "A", "--to", "A", "--amount", "5",
                                "--year", "2024", "--month", "1", "--store", self.store_path])
        self.assertEqual(code, 2)
        self.assertEqual(out, "")

    def test_transfer_unknown_name_errors(self):
        code, out, err = _run(["envelope", "transfer", "--from", "A", "--to", "Nope", "--amount", "5",
                                "--year", "2024", "--month", "1", "--store", self.store_path])
        self.assertEqual(code, 2)
        self.assertEqual(out, "")


# ---- Phase 5: allocation ------------------------------------------------------

class TestPhase5Allocation(unittest.TestCase):
    def test_suggest_allocations_sums_exactly_largest_remainder(self):
        envs = [
            Envelope("Alpha", ["a"], Decimal("100"), "2024-01"),
            Envelope("Beta", ["b"], Decimal("200"), "2024-01"),
            Envelope("Gamma", ["c"], Decimal("300"), "2024-01"),
        ]
        result = suggest_allocations(envs, Decimal("100.00"))
        self.assertEqual(sum(result.values()), Decimal("100.00"))
        # exact shares would be 16.666.., 33.333.., 50.00 -> remainders make
        # Alpha and Beta each gain a cent over the floor.
        self.assertEqual(result["Alpha"], Decimal("16.67"))
        self.assertEqual(result["Beta"], Decimal("33.33"))
        self.assertEqual(result["Gamma"], Decimal("50.00"))

    def test_suggest_allocations_tie_break_alphabetical(self):
        envs = [Envelope("Zeta", ["z"], Decimal("1"), "2024-01"), Envelope("Alpha", ["a"], Decimal("1"), "2024-01")]
        result = suggest_allocations(envs, Decimal("0.01"))
        self.assertEqual(result["Alpha"], Decimal("0.01"))
        self.assertEqual(result["Zeta"], Decimal("0.00"))

    def test_suggest_allocations_all_zero_budget(self):
        envs = [Envelope("A", ["a"], Decimal("0"), "2024-01"), Envelope("B", ["b"], Decimal("0"), "2024-01")]
        result = suggest_allocations(envs, Decimal("100.00"))
        self.assertEqual(result, {"A": Decimal("0.00"), "B": Decimal("0.00")})

    def test_cli_suggest_output_alphabetical(self):
        with tempfile.TemporaryDirectory() as tmp:
            store_path = str(Path(tmp) / "ledger.json")
            _run(["envelope", "add", "--name", "Beta", "--monthly-budget", "200", "--category", "b",
                  "--start-month", "2024-01", "--store", store_path])
            _run(["envelope", "add", "--name", "Alpha", "--monthly-budget", "100", "--category", "a",
                  "--start-month", "2024-01", "--store", store_path])
            code, out, _err = _run(["envelope", "suggest", "--income", "300", "--store", store_path])
            self.assertEqual(code, 0)
            lines = out.strip().splitlines()
            self.assertEqual(lines[0], "Alpha: 100.00")
            self.assertEqual(lines[1], "Beta: 200.00")

    def test_cli_suggest_no_envelopes_message(self):
        with tempfile.TemporaryDirectory() as tmp:
            store_path = str(Path(tmp) / "ledger.json")
            code, out, _err = _run(["envelope", "suggest", "--income", "100", "--store", store_path])
            self.assertEqual(out.strip(), "No envelopes.")


# ---- Phase 6: alerts -----------------------------------------------------------

class TestPhase6Alerts(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")

    def test_alerts_sorted_most_overspent_first(self):
        Path(self.store_path).write_text(json.dumps({
            "transactions": [
                {"id": 1, "date": "2024-01-05", "amount": "-150.00", "payee": "P", "memo": "", "category": "a"},
                {"id": 2, "date": "2024-01-06", "amount": "-120.00", "payee": "P", "memo": "", "category": "b"},
            ],
            "envelopes": [
                {"name": "A", "categories": ["a"], "monthly_budget": "100", "start_month": "2024-01",
                 "rollover": False, "starting_balance": "0", "closed_through": None, "adjustments": {}},
                {"name": "B", "categories": ["b"], "monthly_budget": "100", "start_month": "2024-01",
                 "rollover": False, "starting_balance": "0", "closed_through": None, "adjustments": {}},
            ],
        }), encoding="utf-8")
        code, out, _err = _run(["envelope", "alerts", "--year", "2024", "--month", "1",
                                 "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip().splitlines(), ["A: over by 50.00", "B: over by 20.00"])

    def test_alerts_skips_envelope_before_start_month(self):
        Path(self.store_path).write_text(json.dumps({
            "transactions": [],
            "envelopes": [
                {"name": "Future", "categories": ["x"], "monthly_budget": "100", "start_month": "2024-06",
                 "rollover": False, "starting_balance": "0", "closed_through": None, "adjustments": {}},
            ],
        }), encoding="utf-8")
        code, out, _err = _run(["envelope", "alerts", "--year", "2024", "--month", "1",
                                 "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "No envelopes over budget.")

    def test_alerts_none_over_message(self):
        Path(self.store_path).write_text(json.dumps({
            "transactions": [],
            "envelopes": [
                {"name": "A", "categories": ["a"], "monthly_budget": "100", "start_month": "2024-01",
                 "rollover": False, "starting_balance": "0", "closed_through": None, "adjustments": {}},
            ],
        }), encoding="utf-8")
        code, out, _err = _run(["envelope", "alerts", "--year", "2024", "--month", "1",
                                 "--store", self.store_path])
        self.assertEqual(out.strip(), "No envelopes over budget.")


# ---- Phase 7: close-month -------------------------------------------------------

class TestPhase7CloseMonth(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")

    def test_close_month_folds_balance_and_clears_old_adjustments(self):
        Path(self.store_path).write_text(json.dumps({
            "transactions": [
                {"id": 1, "date": "2024-01-05", "amount": "-30.00", "payee": "P", "memo": "", "category": "food"},
            ],
            "envelopes": [
                {"name": "Food", "categories": ["food"], "monthly_budget": "100", "start_month": "2024-01",
                 "rollover": True, "starting_balance": "0", "closed_through": None,
                 "adjustments": {"2024-01": "5", "2024-02": "7"}},
            ],
        }), encoding="utf-8")
        code, out, _err = _run(["envelope", "close-month", "--year", "2024", "--month", "1",
                                 "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "Closed 2024-01 for 1 envelope(s)")
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        env = raw["envelopes"][0]
        # 0 (starting) + 100 - 30 + 5 (Jan adjustment) = 75
        self.assertEqual(Decimal(env["starting_balance"]), Decimal("75"))
        self.assertEqual(env["closed_through"], "2024-01")
        self.assertEqual(list(env["adjustments"].keys()), ["2024-02"])

    def test_close_month_skips_non_rollover(self):
        Path(self.store_path).write_text(json.dumps({
            "transactions": [],
            "envelopes": [
                {"name": "Food", "categories": ["food"], "monthly_budget": "100", "start_month": "2024-01",
                 "rollover": False, "starting_balance": "0", "closed_through": None, "adjustments": {}},
            ],
        }), encoding="utf-8")
        code, out, _err = _run(["envelope", "close-month", "--year", "2024", "--month", "1",
                                 "--store", self.store_path])
        self.assertEqual(out.strip(), "Closed 2024-01 for 0 envelope(s)")

    def test_close_month_idempotent_already_closed(self):
        Path(self.store_path).write_text(json.dumps({
            "transactions": [],
            "envelopes": [
                {"name": "Food", "categories": ["food"], "monthly_budget": "100", "start_month": "2024-01",
                 "rollover": True, "starting_balance": "70", "closed_through": "2024-01", "adjustments": {}},
            ],
        }), encoding="utf-8")
        code, out, _err = _run(["envelope", "close-month", "--year", "2024", "--month", "1",
                                 "--store", self.store_path])
        self.assertEqual(out.strip(), "Closed 2024-01 for 0 envelope(s)")
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertEqual(Decimal(raw["envelopes"][0]["starting_balance"]), Decimal("70"))


# ---- Phase 8: export -----------------------------------------------------------

class TestPhase8Export(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")
        self.csv_path = str(Path(self.tmpdir.name) / "out.csv")
        Path(self.store_path).write_text(json.dumps({
            "transactions": [],
            "envelopes": [
                {"name": "Zeta", "categories": ["z"], "monthly_budget": "50", "start_month": "2024-02",
                 "rollover": True, "starting_balance": "10", "closed_through": None, "adjustments": {}},
                {"name": "Alpha", "categories": ["a", "b"], "monthly_budget": "100.5", "start_month": "2024-01",
                 "rollover": False, "starting_balance": "0", "closed_through": None, "adjustments": {}},
            ],
        }), encoding="utf-8")

    def test_export_writes_csv_alphabetical(self):
        code, out, _err = _run(["envelope", "export", "--store", self.store_path, "--out", self.csv_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "Exported 2 envelope(s) to " + self.csv_path)
        with open(self.csv_path, newline="", encoding="utf-8") as f:
            rows = list(csv.reader(f))
        self.assertEqual(rows[0], ["Name", "Category", "MonthlyBudget", "Rollover", "StartMonth", "StartingBalance"])
        self.assertEqual(rows[1], ["Alpha", "a,b", "100.50", "false", "2024-01", "0.00"])
        self.assertEqual(rows[2], ["Zeta", "z", "50.00", "true", "2024-02", "10.00"])

    def test_export_no_envelopes_header_only(self):
        with tempfile.TemporaryDirectory() as tmp:
            store_path = str(Path(tmp) / "empty.json")
            csv_path = str(Path(tmp) / "empty.csv")
            code, out, _err = _run(["envelope", "export", "--store", store_path, "--out", csv_path])
            self.assertEqual(code, 0)
            self.assertEqual(out.strip(), "Exported 0 envelope(s) to " + csv_path)
            with open(csv_path, newline="", encoding="utf-8") as f:
                rows = list(csv.reader(f))
            self.assertEqual(rows, [["Name", "Category", "MonthlyBudget", "Rollover", "StartMonth",
                                      "StartingBalance"]])


# ---- Phase 9: import-csv --------------------------------------------------------

class TestPhase9ImportCsv(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")
        self.csv_path = str(Path(self.tmpdir.name) / "in.csv")
        Path(self.store_path).write_text(json.dumps({
            "transactions": [],
            "envelopes": [
                {"name": "Food", "categories": ["food"], "monthly_budget": "100", "start_month": "2024-01",
                 "rollover": True, "starting_balance": "70", "closed_through": "2024-01",
                 "adjustments": {"2024-02": "5"}},
            ],
        }), encoding="utf-8")

    def _write_csv(self, rows):
        with open(self.csv_path, "w", newline="", encoding="utf-8") as f:
            w = csv.writer(f)
            w.writerow(["Name", "Category", "MonthlyBudget", "Rollover", "StartMonth", "StartingBalance"])
            for row in rows:
                w.writerow(row)

    def test_import_replaces_existing_envelope_resets_state(self):
        self._write_csv([["Food", "food,dining", "150.00", "false", "2024-03", "0.00"]])
        code, out, _err = _run(["envelope", "import-csv", "--store", self.store_path, "--file", self.csv_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "Imported 1 envelope(s) from " + self.csv_path)
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertEqual(len(raw["envelopes"]), 1)
        env = raw["envelopes"][0]
        self.assertEqual(env["categories"], ["food", "dining"])
        self.assertEqual(Decimal(env["monthly_budget"]), Decimal("150.00"))
        self.assertFalse(env["rollover"])
        self.assertIsNone(env["closed_through"])
        self.assertEqual(env["adjustments"], {})

    def test_import_appends_new_envelope(self):
        self._write_csv([["Fun", "fun", "50.00", "false", "2024-01", "0.00"]])
        _run(["envelope", "import-csv", "--store", self.store_path, "--file", self.csv_path])
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        names = [e["name"] for e in raw["envelopes"]]
        self.assertEqual(names, ["Food", "Fun"])

    def test_import_mixed_replace_and_append_preserves_position(self):
        self._write_csv([
            ["Fun", "fun", "50.00", "false", "2024-01", "0.00"],
            ["Food", "food", "200.00", "false", "2024-04", "0.00"],
        ])
        code, out, _err = _run(["envelope", "import-csv", "--store", self.store_path, "--file", self.csv_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "Imported 2 envelope(s) from " + self.csv_path)
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        names = [e["name"] for e in raw["envelopes"]]
        self.assertEqual(names, ["Food", "Fun"])
        food = next(e for e in raw["envelopes"] if e["name"] == "Food")
        self.assertEqual(food["start_month"], "2024-04")
        self.assertIsNone(food["closed_through"])


# ---- Phase 10: validation + remove ------------------------------------------------

class TestPhase10ValidationRemove(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")

    def test_add_rejects_nonpositive_budget(self):
        code, out, err = _run(["envelope", "add", "--name", "Bad", "--monthly-budget", "0",
                                "--category", "x", "--start-month", "2024-01", "--store", self.store_path])
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertFalse(Path(self.store_path).exists())

    def test_add_rejects_invalid_start_month(self):
        code, out, err = _run(["envelope", "add", "--name", "Bad", "--monthly-budget", "10",
                                "--category", "x", "--start-month", "Jan-2024", "--store", self.store_path])
        self.assertEqual(code, 2)
        self.assertEqual(out, "")

    def test_add_rejects_duplicate_name(self):
        _run(["envelope", "add", "--name", "Food", "--monthly-budget", "100", "--category", "food",
              "--start-month", "2024-01", "--store", self.store_path])
        code, out, err = _run(["envelope", "add", "--name", "Food", "--monthly-budget", "200",
                                "--category", "food2", "--start-month", "2024-02",
                                "--store", self.store_path])
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertEqual(len(raw["envelopes"]), 1)

    def test_remove_existing(self):
        _run(["envelope", "add", "--name", "Food", "--monthly-budget", "100", "--category", "food",
              "--start-month", "2024-01", "--store", self.store_path])
        code, out, _err = _run(["envelope", "remove", "Food", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "Removed envelope Food")
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertEqual(raw["envelopes"], [])

    def test_remove_unknown_errors(self):
        code, out, err = _run(["envelope", "remove", "Nope", "--store", self.store_path])
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertTrue(err.strip())


if __name__ == "__main__":
    unittest.main()
