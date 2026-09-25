import contextlib
import copy
import io
import json
import tempfile
import unittest
from datetime import date
from decimal import Decimal
from pathlib import Path

from ledgerlite.cli import main
from ledgerlite.models import Transaction
from ledgerlite.schema import CURRENT_VERSION, migrate_payload
from ledgerlite import store


def _run(args):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = main(args)
    return code, out.getvalue(), err.getvalue()


V1_PAYLOAD = {
    "transactions": [
        {"id": 1, "date": "2024-01-01", "amount": "-5.00", "payee": "Cafe", "memo": "", "category": None},
        {"id": 2, "date": "2024-01-02", "amount": "10.00", "payee": "Payroll", "memo": "", "category": "income"},
    ]
}


class TestMigratePayload(unittest.TestCase):
    def test_v1_gets_schema_version_and_default_source(self):
        result = migrate_payload(V1_PAYLOAD)
        self.assertEqual(result["schema_version"], CURRENT_VERSION)
        self.assertEqual([t["source"] for t in result["transactions"]], ["import", "import"])

    def test_does_not_mutate_input(self):
        original = copy.deepcopy(V1_PAYLOAD)
        migrate_payload(V1_PAYLOAD)
        self.assertEqual(V1_PAYLOAD, original)

    def test_preserves_existing_source(self):
        payload = {"schema_version": 2, "transactions": [
            {"id": 1, "date": "2024-01-01", "amount": "1.00", "payee": "X", "memo": "",
             "category": None, "source": "manual"},
        ]}
        result = migrate_payload(payload)
        self.assertEqual(result["transactions"][0]["source"], "manual")

    def test_idempotent_on_current_payload(self):
        once = migrate_payload(V1_PAYLOAD)
        twice = migrate_payload(once)
        self.assertEqual(once, twice)


class TestTransactionDefaultSource(unittest.TestCase):
    def test_default_source_is_manual(self):
        txn = Transaction(id=1, date=date(2024, 1, 1), amount=Decimal("1"), payee="X", memo="")
        self.assertEqual(txn.source, "manual")


class TestStoreMigrationOnLoad(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.path = str(Path(self.tmpdir.name) / "ledger.json")

    def test_load_old_format_defaults_source_import(self):
        Path(self.path).write_text(json.dumps(V1_PAYLOAD), encoding="utf-8")
        txns = store.load(self.path)
        self.assertEqual([t.source for t in txns], ["import", "import"])

    def test_load_does_not_rewrite_file_on_disk(self):
        Path(self.path).write_text(json.dumps(V1_PAYLOAD), encoding="utf-8")
        before = Path(self.path).read_text(encoding="utf-8")
        store.load(self.path)
        after = Path(self.path).read_text(encoding="utf-8")
        self.assertEqual(before, after)

    def test_load_preserves_explicit_source_in_v2_file(self):
        payload = {"schema_version": 2, "transactions": [
            {"id": 1, "date": "2024-01-01", "amount": "1.00", "payee": "X", "memo": "",
             "category": None, "source": "manual"},
        ]}
        Path(self.path).write_text(json.dumps(payload), encoding="utf-8")
        txns = store.load(self.path)
        self.assertEqual(txns[0].source, "manual")

    def test_save_always_writes_schema_version_and_source(self):
        txn = Transaction(id=1, date=date(2024, 1, 1), amount=Decimal("1"), payee="X", memo="", source="manual")
        store.save(self.path, [txn])
        raw = json.loads(Path(self.path).read_text(encoding="utf-8"))
        self.assertEqual(raw["schema_version"], 2)
        self.assertEqual(raw["transactions"][0]["source"], "manual")

    def test_round_trip_preserves_source(self):
        txn = Transaction(id=1, date=date(2024, 1, 1), amount=Decimal("1"), payee="X", memo="", source="manual")
        store.save(self.path, [txn])
        loaded = store.load(self.path)
        self.assertEqual(loaded[0].source, "manual")


class TestCsvImportSource(unittest.TestCase):
    def test_read_csv_sets_source_import(self):
        from ledgerlite.parse import read_csv
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "in.csv"
            path.write_text("date,amount,payee,memo\n2024-01-01,-5.00,Cafe,Coffee\n", encoding="utf-8")
            txns = read_csv(str(path))
        self.assertEqual(txns[0].source, "import")


class TestCliAdd(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")

    def test_add_to_empty_store_gets_id_1(self):
        code, out, _err = _run([
            "add", "--date", "2024-01-01", "--amount", "-5.00", "--payee", "Cafe",
            "--store", self.store_path,
        ])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "Added transaction 1")
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertEqual(raw["transactions"][0]["id"], 1)
        self.assertEqual(raw["transactions"][0]["source"], "manual")
        self.assertEqual(raw["schema_version"], 2)

    def test_add_assigns_max_plus_one(self):
        Path(self.store_path).write_text(json.dumps({"transactions": [
            {"id": 5, "date": "2024-01-01", "amount": "1.00", "payee": "X", "memo": "", "category": None},
        ]}), encoding="utf-8")
        code, out, _err = _run([
            "add", "--date", "2024-02-01", "--amount", "3.00", "--payee", "Y",
            "--store", self.store_path,
        ])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "Added transaction 6")

    def test_add_upgrades_old_file_in_place(self):
        Path(self.store_path).write_text(json.dumps(V1_PAYLOAD), encoding="utf-8")
        _run(["add", "--date", "2024-03-01", "--amount", "1.00", "--payee", "Z",
              "--store", self.store_path])
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertEqual(raw["schema_version"], 2)
        sources = {t["payee"]: t["source"] for t in raw["transactions"]}
        self.assertEqual(sources["Cafe"], "import")
        self.assertEqual(sources["Z"], "manual")

    def test_add_defaults_memo_and_category(self):
        code, _out, _err = _run([
            "add", "--date", "2024-01-01", "--amount", "-5.00", "--payee", "Cafe",
            "--store", self.store_path,
        ])
        self.assertEqual(code, 0)
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertEqual(raw["transactions"][0]["memo"], "")
        self.assertIsNone(raw["transactions"][0]["category"])


class TestCliMigrate(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")

    def test_migrate_old_file(self):
        Path(self.store_path).write_text(json.dumps(V1_PAYLOAD), encoding="utf-8")
        code, out, _err = _run(["migrate", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "Migrated 2 transactions to schema version 2")
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertEqual(raw["schema_version"], 2)
        self.assertTrue(all("source" in t for t in raw["transactions"]))

    def test_migrate_no_backup_by_default(self):
        Path(self.store_path).write_text(json.dumps(V1_PAYLOAD), encoding="utf-8")
        _run(["migrate", "--store", self.store_path])
        self.assertFalse(Path(self.store_path + ".bak").exists())

    def test_migrate_with_backup_preserves_original_bytes(self):
        original_text = json.dumps(V1_PAYLOAD)
        Path(self.store_path).write_text(original_text, encoding="utf-8")
        _run(["migrate", "--store", self.store_path, "--backup"])
        backup_path = Path(self.store_path + ".bak")
        self.assertTrue(backup_path.exists())
        self.assertEqual(backup_path.read_text(encoding="utf-8"), original_text)

    def test_migrate_already_current_is_noop(self):
        payload = {"schema_version": 2, "transactions": [
            {"id": 1, "date": "2024-01-01", "amount": "1.00", "payee": "X", "memo": "",
             "category": None, "source": "manual"},
        ]}
        text = json.dumps(payload)
        Path(self.store_path).write_text(text, encoding="utf-8")
        code, out, _err = _run(["migrate", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "Already up to date.")
        self.assertEqual(Path(self.store_path).read_text(encoding="utf-8"), text)

    def test_migrate_already_current_ignores_backup_flag(self):
        payload = {"schema_version": 2, "transactions": []}
        Path(self.store_path).write_text(json.dumps(payload), encoding="utf-8")
        _run(["migrate", "--store", self.store_path, "--backup"])
        self.assertFalse(Path(self.store_path + ".bak").exists())

    def test_migrate_twice_is_idempotent(self):
        Path(self.store_path).write_text(json.dumps(V1_PAYLOAD), encoding="utf-8")
        _run(["migrate", "--store", self.store_path])
        code, out, _err = _run(["migrate", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "Already up to date.")


class TestExistingCommandsStillWork(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")
        Path(self.store_path).write_text(json.dumps(V1_PAYLOAD), encoding="utf-8")

    def test_list_works_on_old_format_without_rewriting(self):
        before = Path(self.store_path).read_text(encoding="utf-8")
        code, out, _err = _run(["list", "--store", self.store_path, "--page-size", "10"])
        self.assertEqual(code, 0)
        self.assertIn("Payroll", out)
        after = Path(self.store_path).read_text(encoding="utf-8")
        self.assertEqual(before, after)

    def test_categorize_on_old_format_upgrades_file(self):
        code, _out, _err = _run(["categorize", "--store", self.store_path])
        self.assertEqual(code, 0)
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertEqual(raw["schema_version"], 2)


if __name__ == "__main__":
    unittest.main()
