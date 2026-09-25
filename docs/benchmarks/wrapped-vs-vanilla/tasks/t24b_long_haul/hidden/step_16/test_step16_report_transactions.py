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


def _seed(store_path):
    rows = [
        (1, "2024-01-01", "GROCERY_A", "groceries"),
        (2, "2024-01-02", "DINING_A", "dining"),
        (3, "2024-01-03", "GROCERY_B", "groceries"),
        (4, "2024-01-04", "GROCERY_C", "groceries"),
    ]
    Path(store_path).write_text(json.dumps({"transactions": [
        {"id": i, "date": d, "amount": "-1.00", "payee": p, "memo": "",
         "category": c, "currency": "USD"}
        for i, d, p, c in rows
    ]}), encoding="utf-8")


class TestReportTransactions(unittest.TestCase):
    def test_filters_by_exact_category_default_order(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            _seed(store)
            r = _run("report", "transactions", "--category", "groceries", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertNotIn("DINING_A", r.stdout)
            # Most-recent-first: GROCERY_C (01-04) before GROCERY_B (01-03)
            # before GROCERY_A (01-01), and no leading-row quirk here
            # (this isn't paginated) so all three appear.
            self.assertIn("GROCERY_A", r.stdout)
            self.assertIn("GROCERY_B", r.stdout)
            self.assertIn("GROCERY_C", r.stdout)
            self.assertLess(r.stdout.index("GROCERY_C"), r.stdout.index("GROCERY_B"))
            self.assertLess(r.stdout.index("GROCERY_B"), r.stdout.index("GROCERY_A"))

    def test_legacy_order_flag_reused(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            _seed(store)
            r = _run("report", "transactions", "--category", "groceries",
                      "--store", store, "--legacy-order")
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertLess(r.stdout.index("GROCERY_A"), r.stdout.index("GROCERY_B"))
            self.assertLess(r.stdout.index("GROCERY_B"), r.stdout.index("GROCERY_C"))

    def test_no_matches_message(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            _seed(store)
            r = _run("report", "transactions", "--category", "nonexistent", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertEqual(r.stdout.strip(), "No transactions in category nonexistent.")


if __name__ == "__main__":
    unittest.main()
