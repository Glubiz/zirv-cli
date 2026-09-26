import subprocess
import sys
import unittest
from decimal import Decimal

from ledgerlite.currency import UnknownCurrencyError, convert, from_usd, to_usd


def _run(*args):
    return subprocess.run(
        [sys.executable, "-m", "ledgerlite", *args],
        capture_output=True, text=True,
    )


class TestUnknownCurrencyError(unittest.TestCase):
    def test_to_usd_raises_unknown_currency_error(self):
        with self.assertRaises(UnknownCurrencyError):
            to_usd(Decimal("1"), "CAD")

    def test_from_usd_raises_unknown_currency_error(self):
        with self.assertRaises(UnknownCurrencyError):
            from_usd(Decimal("1"), "CAD")

    def test_convert_raises_unknown_currency_error(self):
        with self.assertRaises(UnknownCurrencyError):
            convert(Decimal("1"), "USD", "CAD")
        with self.assertRaises(UnknownCurrencyError):
            convert(Decimal("1"), "CAD", "USD")

    def test_message_names_the_code(self):
        try:
            to_usd(Decimal("1"), "CAD")
            self.fail("expected UnknownCurrencyError")
        except UnknownCurrencyError as exc:
            self.assertIn("CAD", str(exc))

    def test_known_currency_rounding_unaffected(self):
        # Recall: same rounding rule established earlier this session,
        # re-checked here with a halfway value that a naive banker's-
        # rounding fix would get wrong (17.145 exact -> 17.15, not 17.14).
        self.assertEqual(to_usd(Decimal("13.50"), "GBP"), Decimal("17.15"))


class TestFxCliCleanFailure(unittest.TestCase):
    def test_unknown_currency_exits_cleanly(self):
        r = _run("fx", "convert", "100", "USD", "CAD")
        self.assertEqual(r.returncode, 2)
        self.assertIn("CAD", r.stderr)
        self.assertNotIn("Traceback", r.stderr)

    def test_known_currency_still_works(self):
        r = _run("fx", "convert", "100", "USD", "EUR")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "91.74")


if __name__ == "__main__":
    unittest.main()
