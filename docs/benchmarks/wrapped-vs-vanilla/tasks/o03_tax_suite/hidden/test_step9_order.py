import unittest

from ._h import StoreCase, txn


class TestTaxSummaryOrder(StoreCase):
    def test_biggest_first_ties_by_name_total_last(self):
        self.write_store([
            txn(1, "2024-06-01", "-20.00", "A", category="office", deductible=True),
            txn(2, "2024-06-02", "-300.00", "B", category="travel", deductible=True),
            txn(3, "2024-06-03", "-20.00", "C", category="books", deductible=True),
            txn(4, "2024-06-04", "-1.00", "D", category="zzz", deductible=True),
        ])
        r = self.cli("tax", "summary", "2024")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(
            r.stdout.strip().splitlines(),
            ["travel: 300.00", "books: 20.00", "office: 20.00", "zzz: 1.00", "TOTAL: 341.00"],
        )

    def test_split_parts_are_ordered_by_part_total(self):
        self.write_store([
            txn(1, "2024-05-01", "-100.01", "COSTCO", category="groceries", deductible=True,
                splits=[{"category": "groceries", "percent": "30", "amount": "-30.01"},
                        {"category": "office", "percent": "70", "amount": "-70.00"}]),
        ])
        r = self.cli("tax", "summary", "2024")
        self.assertEqual(
            r.stdout.strip().splitlines(),
            ["office: 70.00", "groceries: 30.01", "TOTAL: 100.01"],
        )


class TestUntouchedOrderings(StoreCase):
    def test_plain_summary_stays_alphabetical(self):
        self.write_store([
            txn(1, "2024-02-01", "-10.00", "A", category="zebra"),
            txn(2, "2024-02-02", "-30.00", "B", category="apple"),
            txn(3, "2024-02-03", "-60.00", "C", category="mango"),
        ])
        r = self.cli("summary")
        self.assertEqual(r.returncode, 0, r.stderr)
        lines = r.stdout.strip().splitlines()
        self.assertEqual(lines[0].split(), ["Category", "Total", "Share"])
        self.assertEqual([l.split()[0] for l in lines[1:]], ["apple", "mango", "zebra"])
        self.assertEqual(lines[1].split(), ["apple", "-30.00", "30.0%"])

    def test_rules_list_stays_in_id_order(self):
        self.cli("rules", "add", "--pattern", "zeta", "--category", "z", "--priority", "9")
        self.cli("rules", "add", "--pattern", "alpha", "--category", "a", "--priority", "1")
        r = self.cli("rules", "list")
        self.assertEqual(
            r.stdout.strip().splitlines(),
            ["1: zeta -> z (priority 9)", "2: alpha -> a (priority 1)"],
        )

    def test_tax_list_stays_in_id_order(self):
        self.write_store([
            txn(3, "2024-02-03", "-1.00", "C", category="x", deductible=True),
            txn(1, "2024-02-01", "-500.00", "A", category="x", deductible=True),
        ])
        r = self.cli("tax", "list")
        self.assertEqual([l.split()[0] for l in r.stdout.strip().splitlines()], ["1", "3"])

    def test_split_show_keeps_part_order(self):
        self.write_store([txn(1, "2024-02-01", "-10.00", "A", category="x")])
        self.cli("split", "1", "--part", "small=10", "--part", "big=90")
        r = self.cli("split-show", "1")
        self.assertEqual(r.stdout.strip().splitlines(), ["small: -1.00", "big: -9.00"])


if __name__ == "__main__":
    unittest.main()
