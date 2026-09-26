import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


class TestNoteRemove(unittest.TestCase):
    def _run(self, *args):
        return subprocess.run(
            [sys.executable, "-m", "ledgerlite", *args],
            capture_output=True, text=True,
        )

    def test_remove_existing_note(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "notes.json")
            self._run("note-add", "keep me", "--notes-store", store)
            self._run("note-add", "remove me", "--notes-store", store)

            r_remove = self._run("note-remove", "-2", "--notes-store", store)
            self.assertEqual(r_remove.returncode, 0, r_remove.stderr)

            r_list = self._run("note-list", "--notes-store", store)
            self.assertIn("keep me", r_list.stdout)
            self.assertNotIn("remove me", r_list.stdout)

    def test_remove_unknown_id_exits_2_and_changes_nothing(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = Path(tmp) / "notes.json"
            self._run("note-add", "only note", "--notes-store", str(store))
            before = store.read_text(encoding="utf-8")

            r_remove = self._run("note-remove", "-99", "--notes-store", str(store))
            self.assertEqual(r_remove.returncode, 2)

            after = store.read_text(encoding="utf-8")
            self.assertEqual(before, after)

    def test_ids_still_negative_and_decreasing_after_a_removal(self):
        # Recall check: removing a note must not disturb the id scheme --
        # the next note added still gets one less than the current minimum.
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "notes.json")
            self._run("note-add", "a", "--notes-store", store)  # -1
            self._run("note-add", "b", "--notes-store", store)  # -2
            self._run("note-remove", "-1", "--notes-store", store)
            r = self._run("note-add", "c", "--notes-store", store)
            self.assertIn("-3", r.stdout)


if __name__ == "__main__":
    unittest.main()
