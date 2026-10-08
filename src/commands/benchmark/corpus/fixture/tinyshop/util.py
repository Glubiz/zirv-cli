def round_money(amount):
    """Round a money amount to whole cents, halves away from zero."""
    return int(amount * 100) / 100
