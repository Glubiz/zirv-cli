import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path

from ledgerlite.cli import main


def _write_store(path, txns):
    Path(path).write_text(json.dumps({"transactions": txns}), encoding="utf-8")


def _txn(id, payee, category, memo=""):
    return {"id": id, "date": "2024-01-01", "amount": "-10.00", "payee": payee, "memo": memo, "category": category}


def _run(args):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = main(args)
    return code, out.getvalue(), err.getvalue()


class TestCategorizePreservesUnmatchedCategory(unittest.TestCase):
    def setUp(self):
        self.tmpdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmpdir.cleanup)
        self.store_path = str(Path(self.tmpdir.name) / "ledger.json")

    def test_manual_category_survives_when_no_rule_matches(self):
        # "Some Custom Vendor" matches none of DEFAULT_RULES, so the fixed
        # `categorize` command must leave an already-assigned category alone
        # instead of blanking it out to uncategorized.
        _write_store(self.store_path, [_txn(1, "Some Custom Vendor", "hobby")])
        code, out, _err = _run(["categorize", "--store", self.store_path])
        self.assertEqual(code, 0)
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertEqual(raw["transactions"][0]["category"], "hobby")
        self.assertIn("Re-categorized 0 of 1", out)

    def test_categorize_still_fills_in_uncategorized_transactions(self):
        # Regression: a transaction with no category and a payee that DOES
        # match a rule should still get categorized.
        _write_store(self.store_path, [_txn(1, "NETFLIX.COM", None)])
        code, out, _err = _run(["categorize", "--store", self.store_path])
        self.assertEqual(code, 0)
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertEqual(raw["transactions"][0]["category"], "subscriptions")
        self.assertIn("Re-categorized 1 of 1", out)

    def test_categorize_updates_category_when_a_rule_actually_matches(self):
        # A transaction mis-tagged by hand should still be corrected when a
        # default rule clearly matches its payee.
        _write_store(self.store_path, [_txn(1, "WHOLE FOODS MARKET #221", "misc")])
        code, out, _err = _run(["categorize", "--store", self.store_path])
        self.assertEqual(code, 0)
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertEqual(raw["transactions"][0]["category"], "groceries")
        self.assertIn("Re-categorized 1 of 1", out)

    def test_mixed_batch_only_counts_actual_matches_as_changed(self):
        _write_store(
            self.store_path,
            [
                _txn(1, "Some Custom Vendor", "hobby"),  # no rule matches -> preserved
                _txn(2, "NETFLIX.COM", None),  # rule matches -> filled in
            ],
        )
        code, out, _err = _run(["categorize", "--store", self.store_path])
        self.assertEqual(code, 0)
        raw = json.loads(Path(self.store_path).read_text(encoding="utf-8"))
        self.assertEqual(raw["transactions"][0]["category"], "hobby")
        self.assertEqual(raw["transactions"][1]["category"], "subscriptions")
        self.assertIn("Re-categorized 1 of 2", out)


if __name__ == "__main__":
    unittest.main()
