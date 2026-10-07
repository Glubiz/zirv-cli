import unittest

from ._h import StoreCase, txn


class TestDoctor(StoreCase):
    def test_clean_ledger(self):
        self.write_store([
            txn(1, "2024-01-01", "-10.00", "A", category="x"),
            txn(2, "2024-01-02", "-10.00", "B", category="x", deductible=True),
            txn(3, "2024-01-03", "-10.00", "C", category="x",
                splits=[{"category": "a", "percent": "50", "amount": "-5.00"},
                        {"category": "b", "percent": "50", "amount": "-5.00"}]),
        ])
        r = self.cli("doctor")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "No issues found")

    def test_duplicate_ids_reported_once_per_id(self):
        self.write_store([
            txn(1, "2024-01-01", "-10.00", "A", category="x"),
            txn(2, "2024-01-02", "-10.00", "B", category="x"),
            txn(2, "2024-01-03", "-11.00", "C", category="x"),
            txn(2, "2024-01-04", "-12.00", "D", category="x"),
        ])
        r = self.cli("doctor")
        self.assertEqual(r.returncode, 1)
        self.assertEqual(r.stdout.strip().splitlines(), ["duplicate-id id=2", "1 issues found"])

    def test_split_mismatch(self):
        self.write_store([
            txn(4, "2024-01-03", "-10.01", "C", category="x",
                splits=[{"category": "a", "percent": "50", "amount": "-5.00"},
                        {"category": "b", "percent": "50", "amount": "-5.00"}]),
        ])
        r = self.cli("doctor")
        self.assertEqual(r.returncode, 1)
        self.assertEqual(r.stdout.strip().splitlines(), ["split-mismatch id=4", "1 issues found"])

    def test_uncategorized_deductible(self):
        self.write_store([
            txn(1, "2024-01-01", "-10.00", "A", deductible=True),
            txn(2, "2024-01-01", "-10.00", "B"),  # not deductible: fine
            txn(3, "2024-01-01", "-10.00", "C", deductible=True,
                splits=[{"category": "a", "percent": "50", "amount": "-5.00"},
                        {"category": "b", "percent": "50", "amount": "-5.00"}]),  # split: fine
        ])
        r = self.cli("doctor")
        self.assertEqual(r.returncode, 1)
        self.assertEqual(
            r.stdout.strip().splitlines(), ["uncategorized-deductible id=1", "1 issues found"]
        )

    def test_issues_sorted_by_id_then_code_with_count(self):
        self.write_store([
            txn(7, "2024-01-01", "-10.00", "A", deductible=True),
            txn(3, "2024-01-01", "-10.00", "B", category="x"),
            txn(3, "2024-01-02", "-10.00", "B2", category="x"),
            txn(5, "2024-01-01", "-10.01", "C", category="x",
                splits=[{"category": "a", "percent": "100", "amount": "-10.00"}]),
            txn(5, "2024-01-01", "-1.00", "C2", category="x", deductible=True),
        ])
        r = self.cli("doctor")
        self.assertEqual(r.returncode, 1)
        self.assertEqual(
            r.stdout.strip().splitlines(),
            ["duplicate-id id=3", "duplicate-id id=5", "split-mismatch id=5",
             "uncategorized-deductible id=7", "4 issues found"],
        )

    def test_doctor_does_not_modify_the_store(self):
        self.write_store([txn(1, "2024-01-01", "-10.00", "A", deductible=True)], rules=[])
        before = self.read_store()
        self.cli("doctor")
        self.assertEqual(self.read_store(), before)

    def test_doctor_after_cli_split_is_clean(self):
        self.write_store([txn(1, "2024-05-01", "-10.01", "COSTCO", category="g")])
        self.cli("split", "1", "--part", "a=33.33", "--part", "b=33.33", "--part", "c=33.34")
        r = self.cli("doctor")
        self.assertEqual(r.returncode, 0, r.stdout)


if __name__ == "__main__":
    unittest.main()
