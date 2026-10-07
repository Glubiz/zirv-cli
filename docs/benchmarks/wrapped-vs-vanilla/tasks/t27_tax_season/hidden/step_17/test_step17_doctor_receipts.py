import unittest

from ._h import StoreCase, txn

RECEIPT = {"path": "/tmp/r.pdf", "sha256": "ab" * 32}


class TestDoctorMissingReceipt(StoreCase):
    def issues(self):
        r = self.cli("doctor")
        return r.returncode, r.stdout.strip().splitlines()

    def test_big_deductible_without_receipt_is_flagged(self):
        self.write_store([
            txn(1, "2024-05-01", "-75.00", "A", category="x", deductible=True),
            txn(2, "2024-05-01", "-74.99", "B", category="x", deductible=True),
            txn(3, "2024-05-01", "-500.00", "C", category="x", deductible=True, receipt=RECEIPT),
            txn(4, "2024-05-01", "-500.00", "D", category="x"),  # not deductible
            txn(5, "2024-05-01", "900.00", "E", category="x", deductible=True),  # big money in
        ])
        code, lines = self.issues()
        self.assertEqual(code, 1)
        self.assertEqual(lines, ["missing-receipt id=1", "missing-receipt id=5", "2 issues found"])

    def test_receipt_attached_through_the_cli_clears_the_issue(self):
        self.write_store([txn(1, "2024-05-01", "-120.00", "A", category="x", deductible=True)])
        self.assertEqual(self.issues(), (1, ["missing-receipt id=1", "1 issues found"]))
        f = self.dir / "r.txt"
        f.write_text("receipt", encoding="utf-8")
        self.cli("receipt", "attach", "1", str(f))
        self.assertEqual(self.issues(), (0, ["No issues found"]))

    def test_split_transactions_are_judged_on_their_whole_amount(self):
        self.write_store([
            txn(1, "2024-05-01", "-100.00", "A", category="x", deductible=True,
                splits=[{"category": "a", "percent": "50", "amount": "-50.00"},
                        {"category": "b", "percent": "50", "amount": "-50.00"}]),
        ])
        self.assertEqual(self.issues(), (1, ["missing-receipt id=1", "1 issues found"]))

    def test_codes_for_one_id_are_sorted_alphabetically_with_the_others(self):
        self.write_store([
            txn(2, "2024-05-01", "-100.00", "A", deductible=True),
            txn(2, "2024-05-02", "-1.00", "B", category="x"),
            txn(1, "2024-05-03", "-80.00", "C", category="x", deductible=True,
                splits=[{"category": "a", "percent": "100", "amount": "-79.99"}]),
        ])
        code, lines = self.issues()
        self.assertEqual(code, 1)
        self.assertEqual(
            lines,
            ["missing-receipt id=1", "split-mismatch id=1",
             "duplicate-id id=2", "missing-receipt id=2", "uncategorized-deductible id=2",
             "5 issues found"],
        )

    def test_clean_ledger_still_clean_and_store_unmodified(self):
        self.write_store([txn(1, "2024-05-01", "-10.00", "A", category="x", deductible=True)])
        before = self.read_store()
        self.assertEqual(self.issues(), (0, ["No issues found"]))
        self.assertEqual(self.read_store(), before)


if __name__ == "__main__":
    unittest.main()
