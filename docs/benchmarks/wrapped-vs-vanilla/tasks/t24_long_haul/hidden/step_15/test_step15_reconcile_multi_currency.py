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
        {"id": 1, "date": "2024-01-01", "amount": "-17.50", "payee": "LONDON SHOP",
         "memo": "", "category": None, "currency": "GBP"},
    ]}), encoding="utf-8")
    _run("reconcile", "mark", "1", "--store", str(store))
    return str(store)


class TestReconcileMultiCurrency(unittest.TestCase):
    def test_usd_flag_converts_with_established_rounding_rule(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = _seeded_store(tmp)
            # 17.50 GBP * 1.27 = 22.225 exactly -> ROUND_HALF_UP -> 22.23
            # (banker's rounding would give 22.22 -- discriminating case).
            r = _run("reconcile", "status", "--statement-balance", "-22.23", "--store", store, "--usd")
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("Cleared total: -22.23", r.stdout)
            self.assertIn("Status: reconciled", r.stdout)

    def test_default_uses_raw_amount(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = _seeded_store(tmp)
            r = _run("reconcile", "status", "--statement-balance", "-17.50", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("Cleared total: -17.50", r.stdout)
            self.assertIn("Status: reconciled", r.stdout)


if __name__ == "__main__":
    unittest.main()
