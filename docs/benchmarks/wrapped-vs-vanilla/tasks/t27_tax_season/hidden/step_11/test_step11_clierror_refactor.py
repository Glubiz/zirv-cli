import contextlib
import io
import unittest

from ledgerlite.cli import main
from ledgerlite.errors import CliError

from ._h import StoreCase, txn


class TestCliErrorClass(unittest.TestCase):
    def test_is_an_exception_carrying_the_message(self):
        self.assertTrue(issubclass(CliError, Exception))
        self.assertEqual(str(CliError("boom")), "boom")


class TestBehaviourUnchanged(StoreCase):
    def call(self, *args):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = main(list(args) + ["--store", self.store])
        return code, out.getvalue(), err.getvalue()

    def setUp(self):
        super().setUp()
        self.write_store([
            txn(1, "2024-05-01", "-100.01", "COSTCO #12", category="groceries"),
            txn(2, "2024-05-02", "-10.00", "COFFEE", category="dining"),
        ])
        self.missing_file = str(self.dir / "absent.pdf")

    def test_every_error_is_printed_with_prefix_and_exit_two(self):
        cases = [
            (("tax", "mark", "99"), "error: no transaction with id 99\n"),
            (("tax", "unmark", "99"), "error: no transaction with id 99\n"),
            (("split", "1", "--part", "a=100"), "error: need at least two --part options\n"),
            (("split", "1", "--part", "a=10", "--part", "b=10"),
             "error: percentages must sum to 100\n"),
            (("split", "1", "--part", "a=50", "--part", "zzz"),
             "error: bad --part 'zzz' (expected CATEGORY=PERCENT)\n"),
            (("split", "7", "--part", "a=50", "--part", "b=50"),
             "error: no transaction with id 7\n"),
            (("split-show", "7"), "error: no transaction with id 7\n"),
            (("receipt", "show", "7"), "error: no transaction with id 7\n"),
            (("receipt", "attach", "1", "PLACEHOLDER"), None),
            (("rules", "add", "--pattern", " ", "--category", "x"),
             "error: pattern must not be empty\n"),
        ]
        for args, expected in cases:
            if expected is None:
                args = ("receipt", "attach", "1", self.missing_file)
                expected = f"error: no such file: {self.missing_file}\n"
            code, out, err = self.call(*args)
            self.assertEqual(code, 2, args)
            self.assertEqual(out, "", args)
            self.assertEqual(err, expected, args)

    def test_merge_negative_window(self):
        csv_path = self.write_csv([("2024-05-01", "-1.00", "X", "y")])
        code, out, err = self.call("merge", csv_path, "--window", "-3")
        self.assertEqual((code, out, err), (2, "", "error: window must be >= 0\n"))

    def test_successful_commands_print_exactly_what_they_did(self):
        self.assertEqual(self.call("rules", "add", "--pattern", "costco", "--category", "bulk"),
                         (0, "Added rule 1: costco -> bulk (priority 0)\n", ""))
        self.assertEqual(self.call("categorize"),
                         (0, "Re-categorized 2 of 2 transactions\n", ""))
        self.assertEqual(self.call("normalize"), (0, "Normalized 1 payees\n", ""))
        self.assertEqual(self.call("split", "1", "--part", "a=50", "--part", "b=50"),
                         (0, "Split transaction 1 into 2 parts\n", ""))
        self.assertEqual(self.call("split-show", "1"), (0, "a: -50.01\nb: -50.00\n", ""))
        self.assertEqual(self.call("tax", "mark", "1", "2"),
                         (0, "Marked 2 transactions as tax-deductible\n", ""))
        self.assertEqual(self.call("tax", "summary", "2024"),
                         (0, "a: 50.01\nb: 50.00\nuncategorized: 10.00\nTOTAL: 110.01\n", ""))

    def test_doctor_and_receipt_exit_codes_unchanged(self):
        self.assertEqual(self.call("doctor")[0], 0)
        self.assertEqual(self.call("receipt", "verify"),
                         (0, "No receipts attached.\n", ""))
        self.call("tax", "mark", "2")
        self.write_store([txn(1, "2024-05-01", "-1.00", "A", deductible=True)])
        code, out, err = self.call("doctor")
        self.assertEqual((code, out), (1, "uncategorized-deductible id=1\n1 issues found\n"))


if __name__ == "__main__":
    unittest.main()
