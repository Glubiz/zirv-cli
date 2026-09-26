import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from ledgerlite.notes import Note, add_note, load_notes, next_note_id, save_notes


class TestNoteIds(unittest.TestCase):
    def test_first_id_is_minus_one(self):
        self.assertEqual(next_note_id([]), -1)

    def test_ids_decrease_by_one_each_time(self):
        notes = []
        n1 = add_note(notes, "a")
        n2 = add_note(notes, "b")
        n3 = add_note(notes, "c")
        self.assertEqual([n1.id, n2.id, n3.id], [-1, -2, -3])

    def test_ids_are_always_negative(self):
        notes = []
        for i in range(5):
            add_note(notes, f"note {i}")
        self.assertTrue(all(n.id < 0 for n in notes))


class TestNotesPersistence(unittest.TestCase):
    def test_save_and_load_round_trip(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "notes.json"
            notes = []
            add_note(notes, "buy milk")
            add_note(notes, "call mom", pinned=True)
            save_notes(notes, path)
            loaded = load_notes(path)
            self.assertEqual(len(loaded), 2)
            self.assertEqual({n.text for n in loaded}, {"buy milk", "call mom"})

    def test_load_missing_file_is_empty_list(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "does_not_exist.json"
            self.assertEqual(load_notes(path), [])


class TestNoteCli(unittest.TestCase):
    def _run(self, *args):
        return subprocess.run(
            [sys.executable, "-m", "ledgerlite", *args],
            capture_output=True, text=True,
        )

    def test_add_and_list_round_trip(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "notes.json")
            r1 = self._run("note-add", "buy milk", "--notes-store", store)
            self.assertEqual(r1.returncode, 0, r1.stderr)
            self.assertIn("-1", r1.stdout)

            r2 = self._run("note-add", "call mom", "--notes-store", store)
            self.assertEqual(r2.returncode, 0, r2.stderr)
            self.assertIn("-2", r2.stdout)

            r3 = self._run("note-list", "--notes-store", store)
            self.assertEqual(r3.returncode, 0, r3.stderr)
            self.assertIn("buy milk", r3.stdout)
            self.assertIn("call mom", r3.stdout)
            self.assertIn("-1", r3.stdout)
            self.assertIn("-2", r3.stdout)

    def test_list_empty(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "notes.json")
            r = self._run("note-list", "--notes-store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertNotIn("Traceback", r.stdout + r.stderr)

    def test_note_add_never_creates_a_ledger_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            notes_store = Path(tmp) / "notes.json"
            r_add = self._run("note-add", "hello", "--notes-store", str(notes_store))
            self.assertEqual(r_add.returncode, 0, r_add.stderr)
            self.assertTrue(notes_store.exists())
            self.assertFalse((Path(tmp) / "ledger.json").exists())


if __name__ == "__main__":
    unittest.main()
