import contextlib
import csv
import io
import json
import tempfile
import unittest
from pathlib import Path

from ledgerlite.cli import main


def _write_store(path, txns):
    Path(path).write_text(json.dumps({"transactions": txns}), encoding="utf-8")


def _txn(id, date, amount, payee="Payee", memo="", category="cat"):
    return {"id": id, "date": date, "amount": amount, "payee": payee, "memo": memo, "category": category}


def _run(args):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = main(args)
    return code, out.getvalue(), err.getvalue()


class TestExportHidden(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")

    def test_csv_quotes_special_characters_in_memo(self):
        tricky_memo = 'has, a comma and "quotes"\nand a newline'
        _write_store(self.store_path, [_txn(1, "2024-01-01", "-12.00", memo=tricky_memo)])
        code, out, _err = _run(["export", "--format", "csv", "--store", self.store_path])
        self.assertEqual(code, 0)
        rows = list(csv.reader(io.StringIO(out)))
        self.assertEqual(rows[0], ["id", "date", "amount", "payee", "memo", "category"])
        self.assertEqual(rows[1][4], tricky_memo)

    def test_unicode_payee_is_not_escaped(self):
        _write_store(self.store_path, [_txn(1, "2024-01-01", "-12.00", payee="Café Møller")])
        code, out, _err = _run(["export", "--format", "json", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertIn("Café Møller", out)
        self.assertNotIn("\\u00e9", out)
        data = json.loads(out)
        self.assertEqual(data[0]["payee"], "Café Møller")

    def test_inclusive_date_bounds(self):
        _write_store(
            self.store_path,
            [
                _txn(1, "2023-12-31", "-1.00"),  # just before range
                _txn(2, "2024-01-01", "-2.00"),  # on --since boundary
                _txn(3, "2024-01-15", "-3.00"),  # inside range
                _txn(4, "2024-01-31", "-4.00"),  # on --until boundary
                _txn(5, "2024-02-01", "-5.00"),  # just after range
            ],
        )
        code, out, _err = _run(
            [
                "export", "--format", "csv", "--store", self.store_path,
                "--since", "2024-01-01", "--until", "2024-01-31",
            ]
        )
        self.assertEqual(code, 0)
        rows = list(csv.reader(io.StringIO(out)))
        ids = [r[0] for r in rows[1:]]
        self.assertEqual(ids, ["2", "3", "4"])

    def test_empty_selection_csv_prints_only_header(self):
        _write_store(self.store_path, [_txn(1, "2024-01-01", "-1.00")])
        code, out, _err = _run(
            [
                "export", "--format", "csv", "--store", self.store_path,
                "--since", "2030-01-01", "--until", "2030-12-31",
            ]
        )
        self.assertEqual(code, 0)
        rows = list(csv.reader(io.StringIO(out)))
        self.assertEqual(rows, [["id", "date", "amount", "payee", "memo", "category"]])

    def test_empty_selection_json_prints_empty_array(self):
        _write_store(self.store_path, [_txn(1, "2024-01-01", "-1.00")])
        code, out, _err = _run(
            [
                "export", "--format", "json", "--store", self.store_path,
                "--since", "2030-01-01", "--until", "2030-12-31",
            ]
        )
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "[]")

    def test_bad_range_exits_2_with_no_stdout(self):
        _write_store(self.store_path, [_txn(1, "2024-01-01", "-1.00")])
        code, out, err = _run(
            [
                "export", "--format", "csv", "--store", self.store_path,
                "--since", "2024-02-01", "--until", "2024-01-01",
            ]
        )
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertTrue(err.strip())

    def test_ordering_ties_broken_by_id(self):
        _write_store(
            self.store_path,
            [
                _txn(9, "2024-01-05", "-9.00"),
                _txn(3, "2024-01-05", "-3.00"),
                _txn(6, "2024-01-01", "-6.00"),
            ],
        )
        code, out, _err = _run(["export", "--format", "csv", "--store", self.store_path])
        self.assertEqual(code, 0)
        rows = list(csv.reader(io.StringIO(out)))
        ids = [r[0] for r in rows[1:]]
        self.assertEqual(ids, ["6", "3", "9"])

    def test_json_amounts_are_strings_and_null_category(self):
        _write_store(
            self.store_path,
            [_txn(1, "2024-01-01", "-12.00", category=None)],
        )
        code, out, _err = _run(["export", "--format", "json", "--store", self.store_path])
        self.assertEqual(code, 0)
        data = json.loads(out)
        self.assertEqual(data, [{
            "id": 1, "date": "2024-01-01", "amount": "-12.00",
            "payee": "Payee", "memo": "", "category": None,
        }])
        self.assertIsInstance(data[0]["amount"], str)

    def test_csv_blank_category_when_uncategorized(self):
        _write_store(self.store_path, [_txn(1, "2024-01-01", "-12.00", category=None)])
        code, out, _err = _run(["export", "--format", "csv", "--store", self.store_path])
        self.assertEqual(code, 0)
        rows = list(csv.reader(io.StringIO(out)))
        self.assertEqual(rows[1][5], "")


if __name__ == "__main__":
    unittest.main()
