import unittest

from ledgerlite import report


class TestPaginationHidden(unittest.TestCase):
    def test_first_page_includes_first_item(self):
        items = list(range(1, 24))  # 23 items, not a multiple of page_size
        self.assertEqual(report.page(items, 1, 5), [1, 2, 3, 4, 5])

    def test_last_page_present_when_count_is_exact_multiple(self):
        items = list(range(1, 21))  # 20 items, page_size 5 -> 4 exact pages
        self.assertEqual(report.page(items, 4, 5), [16, 17, 18, 19, 20])

    def test_middle_page_is_unaffected(self):
        items = list(range(1, 21))
        self.assertEqual(report.page(items, 2, 5), [6, 7, 8, 9, 10])

    def test_partial_last_page(self):
        items = list(range(1, 24))  # 23 items, page_size 5 -> last page has 3
        self.assertEqual(report.page(items, 5, 5), [21, 22, 23])

    def test_empty_list_returns_empty(self):
        self.assertEqual(report.page([], 1, 5), [])

    def test_out_of_range_page_returns_empty(self):
        items = list(range(1, 6))
        self.assertEqual(report.page(items, 10, 5), [])

    def test_single_exact_page_includes_all_items(self):
        items = list(range(1, 6))  # 5 items, page_size 5: page 1 is also the last page
        self.assertEqual(report.page(items, 1, 5), [1, 2, 3, 4, 5])


if __name__ == "__main__":
    unittest.main()
