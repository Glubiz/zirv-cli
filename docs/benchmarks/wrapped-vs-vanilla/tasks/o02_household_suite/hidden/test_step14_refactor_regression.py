import argparse
import contextlib
import io
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HOUSEHOLD_COMMANDS = {"member", "expense", "balance", "settle", "transfer",
                      "account", "report", "history", "undo"}


def cli(store, *args):
    return subprocess.run(
        [sys.executable, "-m", "ledgerlite", *args, "--store", store],
        capture_output=True, text=True,
    )


class TestStructure(unittest.TestCase):
    def test_register_returns_handlers_for_all_household_commands(self):
        from ledgerlite import household_cli
        parser = argparse.ArgumentParser()
        sub = parser.add_subparsers(dest="command", required=True)
        handlers = household_cli.register(sub)
        self.assertTrue(HOUSEHOLD_COMMANDS <= set(handlers), set(handlers))
        for name, handler in handlers.items():
            self.assertTrue(callable(handler), name)

    def test_registered_handler_runs(self):
        from ledgerlite import household_cli
        parser = argparse.ArgumentParser()
        sub = parser.add_subparsers(dest="command", required=True)
        handlers = household_cli.register(sub)
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            args = parser.parse_args(["member", "add", "Ana", "--store", store])
            buf = io.StringIO()
            with contextlib.redirect_stdout(buf):
                code = handlers[args.command](args)
            self.assertEqual(code, 0)
            self.assertEqual(buf.getvalue().strip(), "Added member Ana")

    def test_cli_module_no_longer_holds_household_commands(self):
        source = (Path(__file__).resolve().parent.parent / "ledgerlite" / "cli.py").read_text(encoding="utf-8")
        for name in sorted(HOUSEHOLD_COMMANDS):
            self.assertNotIn(f'"{name}"', source, name)
        self.assertIn("register", source)

    def test_main_still_dispatches_everything(self):
        from ledgerlite.cli import main
        with tempfile.TemporaryDirectory() as tmp:
            store = str(Path(tmp) / "ledger.json")
            out, err = io.StringIO(), io.StringIO()
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                self.assertEqual(main(["member", "add", "Ana", "--store", store]), 0)
                self.assertEqual(main(["member", "add", "ana", "--store", store]), 2)
                self.assertEqual(main(["undo", "--store", store]), 0)
            self.assertEqual(out.getvalue().splitlines(), ["Added member Ana", "Undid #1: member add Ana"])
            self.assertEqual(err.getvalue().strip(), "error: member already exists: ana")


class TestBehaviourUnchanged(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.store = str(Path(self._tmp.name) / "ledger.json")

    def run_cli(self, *args):
        r = cli(self.store, *args)
        return r.returncode, r.stdout.splitlines(), r.stderr.strip()

    def test_full_scenario_outputs(self):
        run = self.run_cli
        for n in ("Ana", "Ben", "Cara"):
            self.assertEqual(run("member", "add", n)[0], 0)
        self.assertEqual(run("expense", "add", "--payer", "Ana", "--amount", "30.00", "--desc", "groceries",
                             "--date", "2024-03-01"), (0, ["Added expense 1: 30.00 paid by Ana"], ""))
        self.assertEqual(run("expense", "add", "--payer", "Ben", "--amount", "10.00", "--desc", "taxi",
                             "--date", "2024-03-02")[0], 0)
        self.assertEqual(run("expense", "add", "--payer", "Cara", "--amount", "100.00", "--desc", "rent",
                             "--date", "2024-03-03", "--split", "Cara=1,Ana=2")[0], 0)
        self.assertEqual(run("expense", "edit", "2", "--amount", "10.01"), (0, ["Updated expense 2"], ""))
        self.assertEqual(run("transfer", "add", "--from", "Ben", "--to", "Ana", "--amount", "4.00",
                             "--note", "pizza", "--settles", "--date", "2024-03-04"),
                         (0, ["Added transfer 1: 4.00 Ben -> Ana"], ""))
        self.assertEqual(run("transfer", "add", "--from", "Cara", "--to", "Ben", "--amount", "2.50",
                             "--date", "2024-03-05")[0], 0)
        self.assertEqual(run("balance"), (0, ["Ana: -53.99", "Ben: 0.67", "Cara: 53.32"], ""))
        self.assertEqual(run("settle", "suggest"),
                         (0, ["Ana pays Cara 53.32", "Ana pays Ben 0.67"], ""))
        self.assertEqual(run("report", "member", "Ben"), (0, [
            "Member: Ben", "Paid: 10.01", "Share: 13.34", "Sent: 4.00", "Received: 0.00", "Net: 0.67",
        ], ""))
        self.assertEqual(run("account", "Ben"), (0, ["T1: -4.00 Ana", "T2: 2.50 Cara"], ""))
        self.assertEqual(run("transfer", "list"), (0, [
            "1: 2024-03-04 4.00 Ben -> Ana (pizza) [settles]",
            "2: 2024-03-05 2.50 Cara -> Ben",
        ], ""))
        self.assertEqual(run("expense", "list", "--payer", "ana"),
                         (0, ["1: 2024-03-01 30.00 Ana groceries"], ""))
        self.assertEqual(run("expense", "show", "3"), (0, [
            "Expense 3: 100.00 paid by Cara on 2024-03-03 (rent)", "Cara: 33.34", "Ana: 66.66",
        ], ""))
        self.assertEqual(run("expense", "show", "2")[1][1:], ["Ana: 3.33", "Ben: 3.34", "Cara: 3.34"])
        self.assertEqual(run("history")[1][-3:], ["7: expense edit 2", "8: transfer add 1", "9: transfer add 2"])
        self.assertEqual(run("undo"), (0, ["Undid #9: transfer add 2"], ""))
        self.assertEqual(run("undo"), (0, ["Undid #8: transfer add 1"], ""))
        self.assertEqual(run("history")[1][-2:], ["10: undo #9", "11: undo #8"])
        self.assertEqual(run("balance"), (0, ["Ana: -49.99", "Ben: -3.33", "Cara: 53.32"], ""))
        self.assertEqual(run("expense", "add", "--payer", "Zed", "--amount", "1", "--desc", "x"),
                         (2, [], "error: unknown member: Zed"))
        self.assertEqual(run("undo"), (0, ["Undid #7: expense edit 2"], ""))
        self.assertEqual(run("expense", "show", "2")[1][0], "Expense 2: 10.00 paid by Ben on 2024-03-02 (taxi)")

    def test_old_commands_still_work(self):
        csv_path = Path(self._tmp.name) / "s.csv"
        csv_path.write_text("date,amount,payee,memo\n2024-01-01,-5.00,Uber ride,x\n", encoding="utf-8")
        r = cli(self.store, "import", str(csv_path))
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), f"Imported 1 transactions into {self.store}")
        self.assertEqual(cli(self.store, "summary").stdout.splitlines()[1].split()[0], "transport")
        self.assertEqual(cli(self.store, "categorize").stdout.strip(), "Re-categorized 0 of 1 transactions")


if __name__ == "__main__":
    unittest.main()
