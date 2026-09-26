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


class TestRecurringCli(unittest.TestCase):
    def test_add_and_list(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            r1 = _run("recurring", "add", "--payee", "Netflix", "--amount", "-15.49",
                       "--day", "5", "--category", "subscriptions", "--store", store)
            self.assertEqual(r1.returncode, 0, r1.stderr)
            self.assertIn("Added recurring Netflix: -15.49 on day 5", r1.stdout)

            r2 = _run("recurring", "add", "--payee", "Payroll", "--amount", "2500.00",
                       "--day", "1", "--store", store)
            self.assertEqual(r2.returncode, 0, r2.stderr)

            r3 = _run("recurring", "list", "--store", store)
            lines = [ln for ln in r3.stdout.splitlines() if ln.strip()]
            self.assertEqual(lines, [
                "Netflix: -15.49 USD day 5",
                "Payroll: 2500.00 USD day 1",
            ])

    def test_list_empty(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            r = _run("recurring", "list", "--store", store)
            self.assertEqual(r.stdout.strip(), "No recurring rules.")

    def test_day_out_of_range_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            r = _run("recurring", "add", "--payee", "X", "--amount", "-1.00",
                      "--day", "30", "--store", store)
            self.assertEqual(r.returncode, 2)
            r2 = _run("recurring", "list", "--store", store)
            self.assertEqual(r2.stdout.strip(), "No recurring rules.")

    def test_apply_generates_and_appends(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = Path(tmp) / "ledger.json"
            # Pre-seed the store with one existing transaction (id 1), like
            # an earlier CSV import would have.
            store.write_text(json.dumps({"transactions": [
                {"id": 1, "date": "2023-12-31", "amount": "-9.00", "payee": "PRIOR",
                 "memo": "", "category": None, "currency": "USD"},
            ]}), encoding="utf-8")

            _run("recurring", "add", "--payee", "Netflix", "--amount", "-15.49",
                 "--day", "5", "--category", "subscriptions", "--store", str(store))
            _run("recurring", "add", "--payee", "Payroll", "--amount", "2500.00",
                 "--day", "1", "--category", "income", "--store", str(store))

            r = _run("recurring", "apply", "--from", "2024-01", "--to", "2024-02",
                      "--store", str(store))
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("Applied 4 recurring transactions", r.stdout)

            data = json.loads(store.read_text(encoding="utf-8"))
            txns = data["transactions"]
            # Prior transaction (id 1) must still be there, untouched.
            self.assertEqual(txns[0]["payee"], "PRIOR")
            # New ones continue from id 2, never colliding with id 1.
            ids = [t["id"] for t in txns]
            self.assertEqual(ids, [1, 2, 3, 4, 5])
            dates = [t["date"] for t in txns[1:]]
            self.assertEqual(dates, ["2024-01-05", "2024-01-01", "2024-02-05", "2024-02-01"])


if __name__ == "__main__":
    unittest.main()
