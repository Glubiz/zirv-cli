import hashlib
import unittest
from pathlib import Path

from ._h import StoreCase, txn


class TestReceipts(StoreCase):
    def setUp(self):
        super().setUp()
        self.write_store(
            [txn(1, "2024-05-01", "-100.00", "OFFICE DEPOT", category="office"),
             txn(2, "2024-05-02", "-10.00", "COFFEE", category="dining")],
            rules=[{"id": 1, "pattern": "x", "category": "y", "priority": 0}],
        )
        self.rfile = self.dir / "receipt1.pdf"
        self.rfile.write_bytes(b"%PDF-1.4 fake receipt bytes\n\x00\x01\x02")
        self.digest = hashlib.sha256(self.rfile.read_bytes()).hexdigest()

    def test_attach_records_path_and_checksum_only(self):
        r = self.cli("receipt", "attach", "1", str(self.rfile))
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "Attached receipt to transaction 1")
        data = self.read_store()
        receipt = self.by_id(1)["receipt"]
        self.assertEqual(receipt, {"path": str(self.rfile), "sha256": self.digest})
        self.assertIsNone(self.by_id(2).get("receipt"))
        self.assertEqual(len(data["rules"]), 1)
        self.assertNotIn("%PDF", Path(self.store).read_text(encoding="utf-8"))

    def test_attach_errors(self):
        r = self.cli("receipt", "attach", "1", str(self.dir / "nope.pdf"))
        self.assertEqual(r.returncode, 2)
        self.assertTrue(r.stderr.startswith("error: no such file: "), r.stderr)
        self.assertIn("nope.pdf", r.stderr)
        r = self.cli("receipt", "attach", "9", str(self.rfile))
        self.assertEqual(r.returncode, 2)
        self.assertTrue(r.stderr.startswith("error: no transaction with id 9"), r.stderr)
        self.assertIsNone(self.by_id(1).get("receipt"))

    def test_show(self):
        self.cli("receipt", "attach", "1", str(self.rfile))
        r = self.cli("receipt", "show", "1")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), f"path={self.rfile} sha256={self.digest}")
        r = self.cli("receipt", "show", "2")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "No receipt for transaction 2")
        r = self.cli("receipt", "show", "9")
        self.assertEqual(r.returncode, 2)

    def test_reattach_replaces(self):
        other = self.dir / "other.png"
        other.write_bytes(b"different")
        self.cli("receipt", "attach", "1", str(self.rfile))
        self.cli("receipt", "attach", "1", str(other))
        self.assertEqual(self.by_id(1)["receipt"]["path"], str(other))

    def test_verify_ok_missing_and_mismatch(self):
        other = self.dir / "other.png"
        other.write_bytes(b"other")
        third = self.dir / "third.png"
        third.write_bytes(b"third")
        self.write_store(
            [txn(1, "2024-05-01", "-1.00", "A"), txn(2, "2024-05-02", "-2.00", "B"),
             txn(3, "2024-05-03", "-3.00", "C"), txn(4, "2024-05-04", "-4.00", "D")]
        )
        self.cli("receipt", "attach", "3", str(third))
        self.cli("receipt", "attach", "1", str(self.rfile))
        self.cli("receipt", "attach", "2", str(other))
        r = self.cli("receipt", "verify")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip().splitlines(), ["OK id=1", "OK id=2", "OK id=3"])
        other.write_bytes(b"tampered")
        third.unlink()
        r = self.cli("receipt", "verify")
        self.assertEqual(r.returncode, 1)
        self.assertEqual(
            r.stdout.strip().splitlines(), ["OK id=1", "MISMATCH id=2", "MISSING id=3"]
        )

    def test_verify_with_nothing_attached(self):
        r = self.cli("receipt", "verify")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "No receipts attached.")

    def test_old_store_loads_and_other_commands_keep_receipts(self):
        self.cli("receipt", "attach", "1", str(self.rfile))
        self.cli("tax", "mark", "1")
        self.cli("split", "1", "--part", "a=50", "--part", "b=50")
        self.cli("normalize")
        t = self.by_id(1)
        self.assertEqual(t["receipt"]["sha256"], self.digest)
        self.assertTrue(t["deductible"])


if __name__ == "__main__":
    unittest.main()
