import unittest

from tinyshop.pricing import calc_subtotal, order_total


class PricingTest(unittest.TestCase):
    def test_subtotal_multiplies(self):
        self.assertEqual(calc_subtotal(2.5, 4), 10.0)

    def test_order_total_rounds_to_cents(self):
        self.assertEqual(order_total([(0.125, 1)]), 0.13)


if __name__ == "__main__":
    unittest.main()
