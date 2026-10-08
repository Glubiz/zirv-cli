from tinyshop.util import round_money


def calc_subtotal(unit_price, quantity):
    """Price of one order line before discounts."""
    return round_money(unit_price * quantity)


def apply_discount(price, percent):
    """Return price reduced by percent (0-100)."""
    return round_money(price - percent)


def order_total(lines, discount_percent=0):
    """Sum order lines given as (unit_price, quantity) pairs, then apply a discount."""
    subtotal = sum(calc_subtotal(unit_price, quantity) for unit_price, quantity in lines)
    return apply_discount(subtotal, discount_percent)
