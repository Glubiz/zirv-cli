import json
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from ledgerlite import report


class TestNotesSummaryFunction(unittest.TestCase):
    def test_counts_pinned_and_unpinned(self):
        from ledgerlite.notes import Note

        notes = [
            Note(id=-1, text="a", pinned=True),
            Note(id=-2, text="b", pinned=False),
            Note(id=-3, text="c", pinned=True),
        ]
        self.assertEqual(report.notes_summary(notes), {"pinned": 2, "unpinned": 1})

    def test_empty(self):
        self.assertEqual(report.notes_summary([]), {"pinned": 0, "unpinned": 0})


class TestNoteListSortOrder(unittest.TestCase):
    def _run(self, *args):
        return subprocess.run(
            [sys.executable, "-m", "ledgerlite", *args],
            capture_output=True, text=True,
        )

    def _line_order(self, stdout, needles):
        positions = []
        for needle in needles:
            m = re.search(re.escape(needle), stdout)
            self.assertIsNotNone(m, f"{needle!r} not found in output:\n{stdout}")
            positions.append(m.start())
        return positions == sorted(positions)

    def test_list_is_insertion_order_even_when_stored_out_of_order(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = Path(tmp) / "notes.json"
            # Write the store directly, deliberately OUT of creation order,
            # to prove note-list sorts on read rather than happening to
            # rely on already-sorted storage.
            store.write_text(json.dumps({"notes": [
                {"id": -3, "text": "third", "pinned": False},
                {"id": -1, "text": "first", "pinned": False},
                {"id": -2, "text": "second", "pinned": False},
            ]}), encoding="utf-8")

            r = self._run("note-list", "--notes-store", str(store))
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertTrue(self._line_order(r.stdout, ["first", "second", "third"]))

    def test_pinned_filter_also_respects_insertion_order(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = Path(tmp) / "notes.json"
            store.write_text(json.dumps({"notes": [
                {"id": -3, "text": "third-pinned", "pinned": True},
                {"id": -1, "text": "first-pinned", "pinned": True},
                {"id": -2, "text": "second-unpinned", "pinned": False},
            ]}), encoding="utf-8")

            r = self._run("note-list", "--pinned", "--notes-store", str(store))
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertNotIn("second-unpinned", r.stdout)
            self.assertTrue(self._line_order(r.stdout, ["first-pinned", "third-pinned"]))


class TestRegressionOfEarlierConstraints(unittest.TestCase):
    def _run(self, *args):
        return subprocess.run(
            [sys.executable, "-m", "ledgerlite", *args],
            capture_output=True, text=True,
        )

    def test_negative_ids_and_length_cap_still_hold(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "notes.json")
            r_ok = self._run("note-add", "still fine", "--notes-store", store)
            self.assertEqual(r_ok.returncode, 0, r_ok.stderr)
            self.assertIn("-1", r_ok.stdout)

            r_too_long = self._run("note-add", "z" * 250, "--notes-store", store)
            self.assertEqual(r_too_long.returncode, 2)

    def test_summary_still_uses_notes_summary_and_reports_correctly(self):
        with tempfile.TemporaryDirectory() as tmp:
            csv_path = Path(tmp) / "sample.csv"
            csv_path.write_text(
                "date,amount,payee,memo\n2024-01-01,-5.00,COFFEE SHOP,n/a\n", encoding="utf-8",
            )
            ledger = str(Path(tmp) / "ledger.json")
            notes_store = str(Path(tmp) / "notes.json")
            self._run("import", str(csv_path), "--store", ledger)
            self._run("note-add", "a", "--pinned", "--notes-store", notes_store)
            self._run("note-add", "b", "--notes-store", notes_store)

            r_summary = self._run(
                "summary", "--store", ledger, "--notes-store", notes_store)
            self.assertEqual(r_summary.returncode, 0, r_summary.stderr)
            self.assertRegex(r_summary.stdout, re.compile(r"pinned.*?\b1\b", re.IGNORECASE))


if __name__ == "__main__":
    unittest.main()
