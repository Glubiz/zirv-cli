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
from ledgerlite.tagging import normalize_tags, parse_tag_list
from ledgerlite import store


def _run(args):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = main(args)
    return code, out.getvalue(), err.getvalue()


class TestNormalizeTags(unittest.TestCase):
    def test_strips_lowercases_dedupes_sorts(self):
        self.assertEqual(
            normalize_tags([" Vacation ", "vacation", "Reimbursable"]),
            ["reimbursable", "vacation"],
        )

    def test_drops_blank_tags(self):
        self.assertEqual(normalize_tags(["  ", "", "food"]), ["food"])

    def test_empty_input(self):
        self.assertEqual(normalize_tags([]), [])


class TestParseTagList(unittest.TestCase):
    def test_splits_on_comma_and_normalizes(self):
        self.assertEqual(parse_tag_list("Vacation, food ,vacation"), ["food", "vacation"])

    def test_empty_string_returns_empty_list(self):
        self.assertEqual(parse_tag_list(""), [])

    def test_blank_only_returns_empty_list(self):
        self.assertEqual(parse_tag_list("   "), [])


class TestTransactionDefaultTags(unittest.TestCase):
    def test_default_tags_is_empty_list(self):
        txn = Transaction(id=1, date=date(2024, 1, 1), amount=Decimal("1"), payee="X", memo="")
        self.assertEqual(txn.tags, [])


class TestStoreRoundTrip(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.path = str(Path(self.tmpdir.name) / "ledger.json")

    def test_save_load_preserves_tags(self):
        txn = Transaction(id=1, date=date(2024, 1, 1), amount=Decimal("-5.00"),
                           payee="Cafe", memo="", tags=["food", "reimbursable"])
        store.save(self.path, [txn])
        raw = json.loads(Path(self.path).read_text(encoding="utf-8"))
        self.assertEqual(raw["transactions"][0]["tags"], ["food", "reimbursable"])
        loaded = store.load(self.path)
        self.assertEqual(loaded[0].tags, ["food", "reimbursable"])

    def test_load_missing_tags_key_defaults_empty(self):
        Path(self.path).write_text(json.dumps({"transactions": [
            {"id": 1, "date": "2024-01-01", "amount": "1.00", "payee": "X", "memo": "", "category": None},
        ]}), encoding="utf-8")
        loaded = store.load(self.path)
        self.assertEqual(loaded[0].tags, [])


class TestCsvImportTags(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)

    def _write_csv(self, text):
        path = Path(self.tmpdir.name) / "in.csv"
        path.write_text(text, encoding="utf-8")
        return str(path)

    def test_tags_column_parsed(self):
        from ledgerlite.parse import read_csv
        path = self._write_csv(
            "date,amount,payee,memo,tags\n"
            "2024-01-01,-5.00,Cafe,Coffee,\"Food, Reimbursable\"\n"
        )
        txns = read_csv(path)
        self.assertEqual(txns[0].tags, ["food", "reimbursable"])

    def test_missing_tags_column_defaults_empty(self):
        from ledgerlite.parse import read_csv
        path = self._write_csv(
            "date,amount,payee,memo\n"
            "2024-01-01,-5.00,Cafe,Coffee\n"
        )
        txns = read_csv(path)
        self.assertEqual(txns[0].tags, [])

    def test_blank_tags_cell_defaults_empty(self):
        from ledgerlite.parse import read_csv
        path = self._write_csv(
            "date,amount,payee,memo,tags\n"
            "2024-01-01,-5.00,Cafe,Coffee,\n"
        )
        txns = read_csv(path)
        self.assertEqual(txns[0].tags, [])


class TestListOutputSuffix(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")

    def _seed(self, txns_json):
        Path(self.store_path).write_text(json.dumps({"transactions": txns_json}), encoding="utf-8")

    def test_tagged_row_gets_suffix(self):
        # Two rows: the template's known list-pagination quirk drops the
        # first row of page 1, so check the second row specifically.
        self._seed([
            {"id": 1, "date": "2024-01-01", "amount": "-1.00", "payee": "Dummy", "memo": "",
             "category": None, "tags": []},
            {"id": 2, "date": "2024-01-02", "amount": "-5.00", "payee": "Cafe", "memo": "m",
             "category": None, "tags": ["reimbursable", "food"]},
        ])
        code, out, _err = _run(["list", "--store", self.store_path, "--page-size", "10"])
        self.assertEqual(code, 0)
        self.assertIn("[food,reimbursable]", out)

    def test_untagged_row_has_no_suffix(self):
        self._seed([
            {"id": 1, "date": "2024-01-01", "amount": "-1.00", "payee": "Dummy", "memo": "",
             "category": None, "tags": []},
            {"id": 2, "date": "2024-01-02", "amount": "-5.00", "payee": "Cafe", "memo": "m",
             "category": None, "tags": []},
        ])
        code, out, _err = _run(["list", "--store", self.store_path, "--page-size", "10"])
        self.assertEqual(code, 0)
        self.assertNotIn("[", out)


class TestTagAddRemove(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")
        Path(self.store_path).write_text(json.dumps({"transactions": [
            {"id": 1, "date": "2024-01-01", "amount": "-5.00", "payee": "Cafe", "memo": "",
             "category": None, "tags": ["food"]},
            {"id": 2, "date": "2024-01-02", "amount": "-8.00", "payee": "Bus", "memo": "",
             "category": None, "tags": []},
        ]}), encoding="utf-8")

    def test_tag_add_merges_and_dedupes(self):
        code, out, _err = _run(["tag", "add", "--id", "1", "--tag", "Food", "--tag", "Trip",
                                 "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "Tagged transaction 1 with 2 tag(s): food,trip")
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        txn1 = next(t for t in raw["transactions"] if t["id"] == 1)
        self.assertEqual(txn1["tags"], ["food", "trip"])

    def test_tag_add_unknown_id_errors(self):
        code, out, err = _run(["tag", "add", "--id", "99", "--tag", "food", "--store", self.store_path])
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertTrue(err.strip())

    def test_tag_add_all_blank_errors_without_saving(self):
        before = Path(self.store_path).read_text(encoding="utf-8")
        code, out, err = _run(["tag", "add", "--id", "2", "--tag", "   ", "--store", self.store_path])
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertTrue(err.strip())
        after = Path(self.store_path).read_text(encoding="utf-8")
        self.assertEqual(before, after)

    def test_tag_remove_case_insensitive(self):
        code, out, _err = _run(["tag", "remove", "--id", "1", "--tag", "FOOD", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "Untagged transaction 1, remaining tags: none")

    def test_tag_remove_absent_tag_is_noop_not_error(self):
        code, out, _err = _run(["tag", "remove", "--id", "1", "--tag", "nonexistent",
                                 "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "Untagged transaction 1, remaining tags: food")

    def test_tag_remove_unknown_id_errors(self):
        code, out, err = _run(["tag", "remove", "--id", "99", "--tag", "food", "--store", self.store_path])
        self.assertEqual(code, 2)
        self.assertEqual(out, "")
        self.assertTrue(err.strip())


class TestTagList(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")

    def test_tag_list_counts_alphabetical(self):
        Path(self.store_path).write_text(json.dumps({"transactions": [
            {"id": 1, "date": "2024-01-01", "amount": "1", "payee": "A", "memo": "",
             "category": None, "tags": ["food", "trip"]},
            {"id": 2, "date": "2024-01-02", "amount": "1", "payee": "B", "memo": "",
             "category": None, "tags": ["food"]},
        ]}), encoding="utf-8")
        code, out, _err = _run(["tag", "list", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip().splitlines(), ["food: 2", "trip: 1"])

    def test_tag_list_none_message(self):
        Path(self.store_path).write_text(json.dumps({"transactions": [
            {"id": 1, "date": "2024-01-01", "amount": "1", "payee": "A", "memo": "",
             "category": None, "tags": []},
        ]}), encoding="utf-8")
        code, out, _err = _run(["tag", "list", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "No tags.")


class TestListTagFilter(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")
        Path(self.store_path).write_text(json.dumps({"transactions": [
            {"id": 1, "date": "2024-01-01", "amount": "-5.00", "payee": "Cafe", "memo": "",
             "category": None, "tags": ["food"]},
            {"id": 2, "date": "2024-01-02", "amount": "-8.00", "payee": "Bus", "memo": "",
             "category": None, "tags": []},
            {"id": 3, "date": "2024-01-03", "amount": "-2.00", "payee": "Snack", "memo": "",
             "category": None, "tags": ["Food"]},
        ]}), encoding="utf-8")

    def test_filters_case_insensitively(self):
        code, out, _err = _run(["list", "--store", self.store_path, "--tag", "FOOD"])
        self.assertEqual(code, 0)
        self.assertIn("Cafe", out)
        self.assertIn("Snack", out)
        self.assertNotIn("Bus", out)

    def test_ignores_paging(self):
        code, out, _err = _run(["list", "--store", self.store_path, "--tag", "food",
                                 "--page", "1", "--page-size", "1"])
        self.assertEqual(code, 0)
        self.assertIn("Cafe", out)
        self.assertIn("Snack", out)

    def test_no_match_message(self):
        code, out, _err = _run(["list", "--store", self.store_path, "--tag", "nonexistent"])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "No transactions match that tag.")


if __name__ == "__main__":
    unittest.main()
