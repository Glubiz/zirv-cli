import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


class TestPinnedCli(unittest.TestCase):
    def _run(self, *args):
        return subprocess.run(
            [sys.executable, "-m", "ledgerlite", *args],
            capture_output=True, text=True,
        )

    def test_pinned_add_and_filter(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "notes.json")
            self._run("note-add", "pinned one", "--pinned", "--notes-store", store)
            self._run("note-add", "not pinned", "--notes-store", store)

            r_all = self._run("note-list", "--notes-store", store)
            self.assertIn("pinned one", r_all.stdout)
            self.assertIn("not pinned", r_all.stdout)

            r_pinned = self._run("note-list", "--pinned", "--notes-store", store)
            self.assertEqual(r_pinned.returncode, 0, r_pinned.stderr)
            self.assertIn("pinned one", r_pinned.stdout)
            self.assertNotIn("not pinned", r_pinned.stdout)

    def test_pinned_filter_empty_when_none_pinned(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "notes.json")
            self._run("note-add", "just a note", "--notes-store", store)
            r_pinned = self._run("note-list", "--pinned", "--notes-store", store)
            self.assertEqual(r_pinned.returncode, 0, r_pinned.stderr)
            self.assertNotIn("just a note", r_pinned.stdout)

    def test_summary_reports_pinned_count(self):
        with tempfile.TemporaryDirectory() as tmp:
            csv_path = Path(tmp) / "sample.csv"
            csv_path.write_text(
                "date,amount,payee,memo\n2024-01-01,-5.00,COFFEE SHOP,n/a\n", encoding="utf-8",
            )
            store = str(Path(tmp) / "ledger.json")
            notes_store = str(Path(tmp) / "notes.json")
            r_import = self._run("import", str(csv_path), "--store", store)
            self.assertEqual(r_import.returncode, 0, r_import.stderr)

            self._run("note-add", "a", "--pinned", "--notes-store", notes_store)
            self._run("note-add", "b", "--pinned", "--notes-store", notes_store)
            self._run("note-add", "c", "--notes-store", notes_store)

            r_summary = self._run(
                "summary", "--store", store, "--notes-store", notes_store)
            self.assertEqual(r_summary.returncode, 0, r_summary.stderr)
            self.assertRegex(r_summary.stdout, re.compile(r"pinned.*?\b2\b", re.IGNORECASE))

    def test_step1_negative_id_constraint_still_holds(self):
        # Recall check: a note added via the NEW --pinned path must still
        # get a negative id, exactly like step 1's plain note-add did.
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "notes.json")
            r = self._run("note-add", "pinned note", "--pinned", "--notes-store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("-1", r.stdout)


if __name__ == "__main__":
    unittest.main()
