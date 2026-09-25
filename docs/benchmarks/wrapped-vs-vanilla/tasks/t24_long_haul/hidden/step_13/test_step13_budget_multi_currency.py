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


class TestVsBudgetMultiCurrency(unittest.TestCase):
    def _seeded_store(self, tmp):
        csv_path = Path(tmp) / "sample.csv"
        csv_path.write_text(
            "date,amount,payee,memo,category,currency\n"
            "2024-01-01,-1.00,DUMMY,n/a,other,USD\n"
            "2024-01-02,-13.50,LONDON CAFE,lunch,dining,GBP\n",
            encoding="utf-8",
        )
        store = str(Path(tmp) / "ledger.json")
        r = _run("import", str(csv_path), "--store", store)
        self.assertEqual(r.returncode, 0, r.stderr)
        _run("budget", "set", "--category", "dining", "--limit", "100.00", "--store", store)
        return store

    def test_converts_to_usd_with_established_rounding_rule(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = self._seeded_store(tmp)
            r = _run("summary", "--vs-budget", "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            lines = [ln for ln in r.stdout.splitlines() if ln.strip()]
            dining_line = next(ln for ln in lines if ln.startswith("dining"))
            # 13.50 GBP * 1.27 = 17.145 exactly -> ROUND_HALF_UP -> 17.15
            # (a banker's-rounding / naive round() implementation would
            # give 17.14 here instead -- this is the discriminating case).
            self.assertIn("17.15", dining_line)
            self.assertNotIn("13.50", dining_line)
            self.assertIn("82.85", dining_line)  # 100.00 - 17.15

    # NOTE: plain `summary` (no --vs-budget) is deliberately not asserted
    # here -- a later step in this same session changes ITS currency
    # handling too, so pinning its behavior in this step's test would make
    # this test fight a legitimate later change instead of guarding this
    # step's own contract (`--vs-budget`'s conversion).


if __name__ == "__main__":
    unittest.main()
