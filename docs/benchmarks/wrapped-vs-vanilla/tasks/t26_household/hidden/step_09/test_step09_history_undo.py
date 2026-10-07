import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


def cli(store, *args):
    return subprocess.run(
        [sys.executable, "-m", "ledgerlite", *args, "--store", store],
        capture_output=True, text=True,
    )


class TestHistoryUndo(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.dir = Path(self._tmp.name)
        self.store = str(self.dir / "ledger.json")

    def ok(self, *args):
        r = cli(self.store, *args)
        self.assertEqual(r.returncode, 0, r.stderr)
        return r.stdout

    def lines(self, *args):
        return self.ok(*args).splitlines()

    def expense(self, payer="Ana", amount="30.00", split=None):
        args = ["expense", "add", "--payer", payer, "--amount", amount,
                "--desc", "x", "--date", "2024-03-01"]
        if split:
            args += ["--split", split]
        return self.ok(*args)

    def setup_household(self):
        for n in ("Ana", "Ben", "Cara"):
            self.ok("member", "add", n)

    def data(self):
        return json.loads(Path(self.store).read_text(encoding="utf-8"))

    def test_history_empty(self):
        self.assertEqual(self.lines("history"), ["No history."])

    def test_changes_are_recorded_with_labels(self):
        self.setup_household()
        self.expense()
        self.ok("transfer", "add", "--from", "Ben", "--to", "Ana", "--amount", "5.00",
                "--date", "2024-03-02")
        self.assertEqual(self.lines("history"), [
            "1: member add Ana", "2: member add Ben", "3: member add Cara",
            "4: expense add 1", "5: transfer add 1",
        ])

    def test_failed_and_read_only_commands_record_nothing(self):
        self.setup_household()
        self.expense()
        r = cli(self.store, "expense", "add", "--payer", "Zed", "--amount", "1",
                "--desc", "x")
        self.assertEqual(r.returncode, 2)
        cli(self.store, "member", "add", "ana")
        cli(self.store, "transfer", "add", "--from", "Ana", "--to", "Ana", "--amount", "1")
        for args in (("balance",), ("settle", "suggest"), ("report", "member", "Ana"),
                     ("expense", "list"), ("expense", "show", "1"), ("account", "Ana"),
                     ("transfer", "list"), ("member", "list")):
            self.ok(*args)
        self.assertEqual(len(self.lines("history")), 4)

    def test_import_is_not_recorded_and_survives_undo(self):
        csv_path = self.dir / "s.csv"
        csv_path.write_text("date,amount,payee,memo\n2024-01-01,-5.00,Shop,x\n", encoding="utf-8")
        self.ok("import", str(csv_path))
        self.ok("member", "add", "Ana")
        self.ok("categorize")
        self.assertEqual(self.lines("history"), ["1: member add Ana"])
        self.assertEqual(self.lines("undo"), ["Undid #1: member add Ana"])
        self.assertEqual(self.lines("member", "list"), ["No members."])
        self.assertEqual(len(self.data()["transactions"]), 1)

    def test_undo_restores_previous_state(self):
        self.setup_household()
        self.expense("Ana", "30.00")
        balance_before = self.lines("balance")
        self.expense("Ben", "12.34", "Ana=1,Cara=3")
        self.assertNotEqual(self.lines("balance"), balance_before)
        self.assertEqual(self.lines("undo"), ["Undid #5: expense add 2"])
        self.assertEqual(self.lines("balance"), balance_before)
        self.assertEqual(self.lines("expense", "list"), ["1: 2024-03-01 30.00 Ana x"])

    def test_undo_transfer(self):
        self.setup_household()
        self.ok("transfer", "add", "--from", "Ben", "--to", "Ana", "--amount", "5.00")
        self.assertEqual(self.lines("undo"), ["Undid #4: transfer add 1"])
        self.assertEqual(self.lines("transfer", "list"), ["No transfers."])
        self.assertEqual(self.lines("account", "Ana"), ["No entries."])

    def test_undo_steps_back_one_change_at_a_time(self):
        self.setup_household()
        self.assertEqual(self.lines("undo"), ["Undid #3: member add Cara"])
        self.assertEqual(self.lines("undo"), ["Undid #2: member add Ben"])
        self.assertEqual(self.lines("member", "list"), ["Ana"])
        self.assertEqual(self.lines("undo"), ["Undid #1: member add Ana"])
        self.assertEqual(self.lines("member", "list"), ["No members."])

    def test_nothing_to_undo(self):
        r = cli(self.store, "undo")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "Nothing to undo.")
        self.ok("member", "add", "Ana")
        self.ok("undo")
        r = cli(self.store, "undo")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "Nothing to undo.")
        self.assertEqual(self.lines("member", "list"), ["No members."])

    def test_history_is_append_only(self):
        self.setup_household()
        self.ok("undo")
        self.assertEqual(self.lines("history"), [
            "1: member add Ana", "2: member add Ben", "3: member add Cara", "4: undo #3",
        ])
        self.ok("undo")
        self.assertEqual(self.lines("history")[-2:], ["4: undo #3", "5: undo #2"])
        seqs = [e["seq"] for e in self.data()["history"]]
        self.assertEqual(seqs, [1, 2, 3, 4, 5])

    def test_undone_ids_are_reused_and_seq_is_not(self):
        self.setup_household()
        self.expense()
        self.ok("undo")
        out = self.expense("Ben", "9.00")
        self.assertEqual(out.strip(), "Added expense 1: 9.00 paid by Ben")
        self.assertEqual(self.lines("history")[-3:],
                         ["4: expense add 1", "5: undo #4", "6: expense add 1"])

    def test_undo_entries_cannot_be_undone(self):
        self.setup_household()
        self.ok("undo")
        self.ok("undo")
        self.ok("undo")
        self.assertEqual(self.lines("undo"), ["Nothing to undo."])
        self.assertEqual(len(self.lines("history")), 6)
        self.assertEqual(self.lines("member", "list"), ["No members."])

    def test_undo_after_new_change_skips_already_undone(self):
        self.setup_household()
        self.ok("undo")                      # undoes Cara (#3)
        self.ok("member", "add", "Dan")      # #5
        self.assertEqual(self.lines("undo"), ["Undid #5: member add Dan"])
        self.assertEqual(self.lines("undo"), ["Undid #2: member add Ben"])
        self.assertEqual(self.lines("member", "list"), ["Ana"])


if __name__ == "__main__":
    unittest.main()
