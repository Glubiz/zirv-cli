class InsufficientStock(Exception):
    """Raised when a reservation asks for more units than are available."""


class Inventory:
    """In-memory stock levels keyed by SKU."""

    def __init__(self, low_stock_threshold=5):
        self.levels = {}
        self.low_stock_threshold = low_stock_threshold

    def restock(self, sku, quantity):
        if quantity <= 0:
            raise ValueError("restock quantity must be positive")
        self.levels[sku] = self.levels.get(sku, 0) + quantity

    def reserve(self, sku, quantity):
        available = self.levels.get(sku, 0)
        if quantity > available:
            raise InsufficientStock(f"{sku}: wanted {quantity}, have {available}")
        self.levels[sku] = available - quantity

    def is_low_stock(self, sku):
        return self.levels.get(sku, 0) <= self.low_stock_threshold
