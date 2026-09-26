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
    # ids 1..5 in insertion order == today's "legacy" order. id 1 and id 2
    # share a date, to check the tie-break rule.
    rows = [
        (1, "2024-01-01", "AAA_EARLY1"),
        (2, "2024-01-01", "BBB_EARLY2"),
        (3, "2024-01-02", "CCC_MID"),
        (4, "2024-01-03", "DDD_LATE"),
        (5, "2024-01-04", "EEE_LATEST"),
    ]
    Path(store_path).write_text(json.dumps({"transactions": [
        {"id": i, "date": d, "amount": "-1.00", "payee": p, "memo": "",
         "category": None, "currency": "USD"}
        for i, d, p in rows
    ]}), encoding="utf-8")


class TestListSortOrder(unittest.TestCase):
    def test_default_is_most_recent_first_with_id_tiebreak(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            _seed(store)
            r = _run("list", "--store", store, "--page-size", "10")
            self.assertEqual(r.returncode, 0, r.stderr)
            out = r.stdout
            # Page 1's leading-row quirk drops whichever row is first in
            # the ACTIVE order -- here that's the most recent (EEE_LATEST).
            self.assertNotIn("EEE_LATEST", out)
            for a, b in [("DDD_LATE", "CCC_MID"), ("CCC_MID", "AAA_EARLY1"),
                         ("AAA_EARLY1", "BBB_EARLY2")]:
                self.assertLess(out.index(a), out.index(b), f"{a} should appear before {b}")

    def test_legacy_order_flag_restores_old_behaviour(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            _seed(store)
            r = _run("list", "--store", store, "--page-size", "10", "--legacy-order")
            self.assertEqual(r.returncode, 0, r.stderr)
            out = r.stdout
            # Under legacy order, index 0 is AAA_EARLY1 (lowest id) and
            # THAT is what the leading-row quirk drops instead.
            self.assertNotIn("AAA_EARLY1", out)
            for a, b in [("BBB_EARLY2", "CCC_MID"), ("CCC_MID", "DDD_LATE"),
                         ("DDD_LATE", "EEE_LATEST")]:
                self.assertLess(out.index(a), out.index(b), f"{a} should appear before {b}")


if __name__ == "__main__":
    unittest.main()
