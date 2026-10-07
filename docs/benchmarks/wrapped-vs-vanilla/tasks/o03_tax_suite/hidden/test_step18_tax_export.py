import json
import unittest
from pathlib import Path

from ._h import StoreCase, txn


class TestTaxExport(StoreCase):
    def setUp(self):
        super().setUp()
        self.write_store(
            [
                txn(1, "2024-05-01", "-100.01", "COSTCO", category="groceries", deductible=True,
                    splits=[{"category": "groceries", "percent": "50", "amount": "-50.01"},
                            {"category": "office", "percent": "50", "amount": "-50.00"}]),
                txn(2, "2024-06-02", "-60.00", "STAPLES", category="office", deductible=True),
                txn(3, "2024-02-02", "-9.00", "OLD", category="books", deductible=True),
                txn(4, "2024-07-02", "-7.50", "BOOKSHOP", category="books", deductible=True),
            ],
            rules=[{"id": 1, "pattern": "x", "category": "y", "priority": 0}],
        )
        self.out = self.dir / "export.out"

    def export(self, fmt, *extra, year="2024"):
        r = self.cli("tax", "export", year, "--format", fmt, "--out", str(self.out), *extra)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), f"Wrote tax summary for {year} to {self.out}")
        return self.out.read_text(encoding="utf-8")

    def test_json_export_uses_fiscal_year_split_parts_and_summary_order(self):
        data = json.loads(self.export("json"))
        self.assertEqual(data["year"], 2024)
        self.assertEqual(data["start"], "2024-04-01")
        self.assertEqual(data["end"], "2025-03-31")
        self.assertEqual(
            data["categories"],
            [{"category": "office", "amount": "110.00"},
             {"category": "groceries", "amount": "50.01"},
             {"category": "books", "amount": "7.50"}],
        )
        self.assertEqual(data["total"], "167.51")

    def test_calendar_flag(self):
        data = json.loads(self.export("json", "--calendar"))
        self.assertEqual(data["start"], "2024-01-01")
        self.assertEqual(data["end"], "2024-12-31")
        self.assertEqual(
            [c["category"] for c in data["categories"]], ["office", "groceries", "books"]
        )
        self.assertEqual(data["categories"][2]["amount"], "16.50")
        self.assertEqual(data["total"], "176.51")

    def test_markdown_export(self):
        text = self.export("markdown")
        self.assertEqual(
            text.splitlines(),
            [
                "# Tax summary 2024",
                "",
                "Period: 2024-04-01 to 2025-03-31",
                "",
                "| Category | Amount |",
                "|---|---|",
                "| office | 110.00 |",
                "| groceries | 50.01 |",
                "| books | 7.50 |",
                "| **Total** | 167.51 |",
            ],
        )
        self.assertTrue(text.endswith("\n"))

    def test_empty_year(self):
        data = json.loads(self.export("json", year="2019"))
        self.assertEqual(data["categories"], [])
        self.assertEqual(data["total"], "0.00")
        text = self.export("markdown", year="2019")
        self.assertEqual(text.splitlines()[-1], "| **Total** | 0.00 |")

    def test_matches_the_on_screen_summary(self):
        shown = self.cli("tax", "summary", "2024").stdout.strip().splitlines()
        data = json.loads(self.export("json"))
        rows = [f"{c['category']}: {c['amount']}" for c in data["categories"]]
        self.assertEqual(shown, rows + [f"TOTAL: {data['total']}"])

    def test_bad_format_is_rejected_and_nothing_written(self):
        r = self.cli("tax", "export", "2024", "--format", "xml", "--out", str(self.out))
        self.assertNotEqual(r.returncode, 0)
        self.assertFalse(self.out.exists())

    def test_export_does_not_touch_the_store(self):
        before = self.read_store()
        self.export("json")
        self.assertEqual(self.read_store(), before)


if __name__ == "__main__":
    unittest.main()
