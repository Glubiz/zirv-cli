import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


class TestNoteLengthCap(unittest.TestCase):
    def _run(self, *args):
        return subprocess.run(
            [sys.executable, "-m", "ledgerlite", *args],
            capture_output=True, text=True,
        )

    def test_exactly_200_chars_is_accepted(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "notes.json")
            text = "x" * 200
            r = self._run("note-add", text, "--notes-store", store)
            self.assertEqual(r.returncode, 0, r.stderr)

    def test_201_chars_is_rejected_with_exit_2(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = Path(tmp) / "notes.json"
            text = "x" * 201
            r = self._run("note-add", text, "--notes-store", str(store))
            self.assertEqual(r.returncode, 2)
            self.assertFalse(store.exists())

    def test_rejected_note_does_not_modify_existing_store(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = Path(tmp) / "notes.json"
            self._run("note-add", "short one", "--notes-store", str(store))
            before = store.read_text(encoding="utf-8")

            too_long = "y" * 500
            r = self._run("note-add", too_long, "--notes-store", str(store))
            self.assertEqual(r.returncode, 2)

            after = store.read_text(encoding="utf-8")
            self.assertEqual(before, after)

    def test_negative_ids_still_hold_for_an_accepted_note(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "notes.json")
            r = self._run("note-add", "fine", "--notes-store", store)
            self.assertIn("-1", r.stdout)


if __name__ == "__main__":
    unittest.main()
