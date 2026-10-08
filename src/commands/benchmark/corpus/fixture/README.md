# tinyshop

tinyshop is a small in-memory library for stock levels and order pricing.

## Usage

- `calc_subtotal(unit_price, quantity)` returns the price of one order line before discounts.
- `order_total(lines, discount_percent=0)` sums `(unit_price, quantity)` pairs and applies a percentage discount.
- `Inventory` tracks stock per SKU: `restock`, `reserve` (raises `InsufficientStock`), and `is_low_stock`.

## Tests

Run the test suite from the repository root:

    python3 -m unittest discover -s tests -t .
