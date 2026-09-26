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
    csv_path = Path(tmp) / "sample.csv"
    csv_path.write_text(
        "date,amount,payee,memo,category,currency\n"
        "2024-01-01,-1.00,DUMMY,n/a,other,USD\n"
        "2024-01-02,-42.50,BERLIN SHOP,gift,shopping,EUR\n",
        encoding="utf-8",
    )
    store = str(Path(tmp) / "ledger.json")
    r = _run("import", str(csv_path), "--store", store)
    return store


class TestSummaryDefaultUsd(unittest.TestCase):
    def test_default_converts_with_established_rounding_rule(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = _seeded_store(tmp)
            r = _run("summary", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            shopping_line = next(ln for ln in r.stdout.splitlines() if ln.startswith("shopping"))
            # 42.50 EUR * 1.09 = 46.325 exactly -> ROUND_HALF_UP -> 46.33
            # (banker's rounding would give 46.32 -- discriminating case).
            self.assertIn("46.33", shopping_line)
            self.assertNotIn("42.50", shopping_line)

    def test_raw_currency_flag_restores_old_behaviour(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = _seeded_store(tmp)
            r = _run("summary", "--raw-currency", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            shopping_line = next(ln for ln in r.stdout.splitlines() if ln.startswith("shopping"))
            self.assertIn("42.50", shopping_line)
            self.assertNotIn("46.33", shopping_line)

    def test_vs_budget_unaffected(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = _seeded_store(tmp)
            _run("budget", "set", "--category", "shopping", "--limit", "100.00", "--store", store)
            r = _run("summary", "--vs-budget", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            shopping_line = next(ln for ln in r.stdout.splitlines() if ln.startswith("shopping"))
            self.assertIn("46.33", shopping_line)


if __name__ == "__main__":
    unittest.main()
