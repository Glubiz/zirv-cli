import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


def _run(*args):
    return subprocess.run(
        [sys.executable, "-m", "ledgerlite", *args],
        capture_output=True, text=True,
    )


def _seeded_store(tmp):
    store = Path(tmp) / "ledger.json"
    store.write_text(json.dumps({"transactions": [
        {"id": 1, "date": "2024-01-01", "amount": "-100.00", "payee": "A", "memo": "",
         "category": None, "currency": "USD"},
        {"id": 2, "date": "2024-01-02", "amount": "-50.00", "payee": "B", "memo": "",
         "category": None, "currency": "USD"},
        {"id": 3, "date": "2024-01-03", "amount": "200.00", "payee": "C", "memo": "",
         "category": None, "currency": "USD"},
    ]}), encoding="utf-8")
    return str(store)


class TestReconcileCli(unittest.TestCase):
    def test_mark_and_status_reconciled(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = _seeded_store(tmp)
            r1 = _run("reconcile", "mark", "1", "2", "--store", store)
            self.assertEqual(r1.returncode, 0, r1.stderr)
            self.assertIn("Marked 2 transactions as cleared", r1.stdout)

            r2 = _run("reconcile", "status", "--statement-balance", "-150.00", "--store", store)
            self.assertEqual(r2.returncode, 0, r2.stderr)
            self.assertIn("Cleared total: -150.00", r2.stdout)
            self.assertIn("Statement balance: -150.00", r2.stdout)
            self.assertIn("Difference: 0.00", r2.stdout)
            self.assertIn("Status: reconciled", r2.stdout)

    def test_status_out_of_balance(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = _seeded_store(tmp)
            _run("reconcile", "mark", "1", "--store", store)
            r = _run("reconcile", "status", "--statement-balance", "0.00", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("Cleared total: -100.00", r.stdout)
            self.assertIn("Difference: 100.00", r.stdout)
            self.assertIn("Status: out of balance", r.stdout)

    def test_marking_twice_is_idempotent(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = _seeded_store(tmp)
            _run("reconcile", "mark", "1", "--store", store)
            _run("reconcile", "mark", "1", "2", "--store", store)
            r = _run("reconcile", "status", "--statement-balance", "-150.00", "--store", store)
            self.assertIn("Cleared total: -150.00", r.stdout)


if __name__ == "__main__":
    unittest.main()
