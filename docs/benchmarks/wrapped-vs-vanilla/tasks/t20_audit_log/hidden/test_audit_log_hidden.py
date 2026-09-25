import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path

from ledgerlite.audit import append_entry, read_log
from ledgerlite.cli import main


def _run(args):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = main(args)
    return code, out.getvalue(), err.getvalue()


class TestAuditModule(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")
        Path(self.store_path).write_text(json.dumps({"transactions": []}), encoding="utf-8")

    def test_append_entry_starts_at_sequence_1(self):
        entry = append_entry(self.store_path, "import", "did a thing")
        self.assertEqual(entry.sequence, 1)
        self.assertEqual(entry.action, "import")
        self.assertEqual(entry.detail, "did a thing")

    def test_append_entry_increments_sequence(self):
        append_entry(self.store_path, "import", "first")
        second = append_entry(self.store_path, "categorize", "second")
        self.assertEqual(second.sequence, 2)

    def test_append_entry_preserves_transactions_key(self):
        Path(self.store_path).write_text(json.dumps({"transactions": [
            {"id": 1, "date": "2024-01-01", "amount": "1.00", "payee": "X", "memo": "", "category": None},
        ]}), encoding="utf-8")
        append_entry(self.store_path, "import", "did a thing")
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertEqual(len(raw["transactions"]), 1)

    def test_read_log_empty_file_has_no_audit_key(self):
        self.assertEqual(read_log(self.store_path), [])

    def test_read_log_returns_entries_in_order(self):
        append_entry(self.store_path, "import", "first")
        append_entry(self.store_path, "categorize", "second")
        entries = read_log(self.store_path)
        self.assertEqual([e.action for e in entries], ["import", "categorize"])
        self.assertEqual([e.sequence for e in entries], [1, 2])

    def test_read_log_missing_file_returns_empty(self):
        missing = str(Path(self.tmpdir.name) / "nope.json")
        self.assertEqual(read_log(missing), [])


class TestCliWiring(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")
        self.csv_path = str(Path(self.tmpdir.name) / "in.csv")

    def test_import_writes_audit_entry(self):
        Path(self.csv_path).write_text(
            "date,amount,payee,memo\n2024-01-01,-5.00,Whole Foods,\n2024-01-02,10.00,Payroll Inc,\n",
            encoding="utf-8",
        )
        _run(["import", self.csv_path, "--store", self.store_path])
        entries = read_log(self.store_path)
        self.assertEqual(len(entries), 1)
        self.assertEqual(entries[0].action, "import")
        self.assertEqual(entries[0].detail, f"Imported 2 transactions from {self.csv_path}")

    def test_categorize_writes_audit_entry(self):
        Path(self.store_path).write_text(json.dumps({"transactions": [
            {"id": 1, "date": "2024-01-01", "amount": "-5.00", "payee": "Whole Foods", "memo": "", "category": None},
            {"id": 2, "date": "2024-01-02", "amount": "1.00", "payee": "Unmatched Zzz", "memo": "", "category": None},
        ]}), encoding="utf-8")
        _run(["categorize", "--store", self.store_path])
        entries = read_log(self.store_path)
        self.assertEqual(len(entries), 1)
        self.assertEqual(entries[0].action, "categorize")
        self.assertEqual(entries[0].detail, "Re-categorized 1 of 2 transactions")

    def test_sequence_keeps_counting_across_commands(self):
        Path(self.csv_path).write_text("date,amount,payee,memo\n2024-01-01,-5.00,Whole Foods,\n", encoding="utf-8")
        _run(["import", self.csv_path, "--store", self.store_path])
        _run(["categorize", "--store", self.store_path])
        entries = read_log(self.store_path)
        self.assertEqual([e.sequence for e in entries], [1, 2])
        self.assertEqual([e.action for e in entries], ["import", "categorize"])

    def test_list_does_not_write_audit_entry(self):
        Path(self.store_path).write_text(json.dumps({"transactions": []}), encoding="utf-8")
        _run(["list", "--store", self.store_path])
        self.assertEqual(read_log(self.store_path), [])

    def test_summary_does_not_write_audit_entry(self):
        Path(self.store_path).write_text(json.dumps({"transactions": []}), encoding="utf-8")
        _run(["summary", "--store", self.store_path])
        self.assertEqual(read_log(self.store_path), [])


class TestAuditLogCommand(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")
        Path(self.store_path).write_text(json.dumps({"transactions": []}), encoding="utf-8")

    def test_log_empty_message(self):
        code, out, _err = _run(["audit", "log", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "No audit entries.")

    def test_log_format(self):
        append_entry(self.store_path, "import", "Imported 2 transactions from x.csv")
        append_entry(self.store_path, "categorize", "Re-categorized 1 of 2 transactions")
        code, out, _err = _run(["audit", "log", "--store", self.store_path])
        self.assertEqual(code, 0)
        lines = out.strip().splitlines()
        self.assertEqual(lines[0], "1. import: Imported 2 transactions from x.csv")
        self.assertEqual(lines[1], "2. categorize: Re-categorized 1 of 2 transactions")


class TestAuditSummaryCommand(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")
        Path(self.store_path).write_text(json.dumps({"transactions": []}), encoding="utf-8")

    def test_summary_empty_message(self):
        code, out, _err = _run(["audit", "summary", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip(), "No audit entries.")

    def test_summary_counts_alphabetically(self):
        append_entry(self.store_path, "import", "a")
        append_entry(self.store_path, "import", "b")
        append_entry(self.store_path, "categorize", "c")
        code, out, _err = _run(["audit", "summary", "--store", self.store_path])
        self.assertEqual(code, 0)
        self.assertEqual(out.strip().splitlines(), ["categorize: 1", "import: 2"])


if __name__ == "__main__":
    unittest.main()
