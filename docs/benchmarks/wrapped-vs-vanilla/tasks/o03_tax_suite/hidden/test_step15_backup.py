import hashlib
import unittest
from pathlib import Path

from ._h import StoreCase, txn


class TestBackupRestore(StoreCase):
    def setUp(self):
        super().setUp()
        self.write_store(
            [txn(1, "2024-05-01", "-100.01", "COSTCO", category="groceries"),
             txn(2, "2024-05-02", "-10.00", "COFFEE", category="dining"),
             txn(3, "2024-05-03", "-1.00", "X")],
            rules=[{"id": 1, "pattern": "x", "category": "y", "priority": 2}],
            notes={"keep": ["me"]},
        )
        self.out = str(self.dir / "backup.json")
        self.sidecar = Path(self.out + ".sha256")

    def test_backup_copies_bytes_and_writes_checksum_file(self):
        original = Path(self.store).read_bytes()
        r = self.cli("backup", "--out", self.out)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), f"Backed up 3 transactions to {self.out}")
        self.assertEqual(Path(self.out).read_bytes(), original)
        self.assertEqual(self.sidecar.read_text(encoding="utf-8").strip(),
                         hashlib.sha256(original).hexdigest())

    def test_restore_brings_back_everything_in_the_file(self):
        original = self.read_store()
        self.cli("backup", "--out", self.out)
        self.write_store([txn(9, "2020-01-01", "-1.00", "JUNK")])
        r = self.cli("restore", self.out)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), f"Restored 3 transactions into {self.store}")
        self.assertEqual(self.read_store(), original)
        self.assertEqual(self.read_store()["rules"][0]["priority"], 2)
        self.assertEqual(self.read_store()["notes"], {"keep": ["me"]})

    def test_restore_creates_the_store_when_missing(self):
        self.cli("backup", "--out", self.out)
        Path(self.store).unlink()
        r = self.cli("restore", self.out)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(len(self.read_store()["transactions"]), 3)

    def test_tampered_backup_is_refused_and_store_untouched(self):
        self.cli("backup", "--out", self.out)
        text = Path(self.out).read_text(encoding="utf-8").replace("COFFEE", "TEA")
        Path(self.out).write_text(text, encoding="utf-8")
        self.write_store([txn(9, "2020-01-01", "-1.00", "JUNK")])
        before = self.read_store()
        r = self.cli("restore", self.out)
        self.assertEqual(r.returncode, 2)
        self.assertTrue(r.stderr.startswith("error: backup integrity check failed"), r.stderr)
        self.assertEqual(r.stdout, "")
        self.assertEqual(self.read_store(), before)

    def test_wrong_checksum_in_sidecar_is_refused(self):
        self.cli("backup", "--out", self.out)
        self.sidecar.write_text("0" * 64 + "\n", encoding="utf-8")
        r = self.cli("restore", self.out)
        self.assertEqual(r.returncode, 2)
        self.assertTrue(r.stderr.startswith("error: backup integrity check failed"), r.stderr)

    def test_missing_checksum_file_is_refused(self):
        self.cli("backup", "--out", self.out)
        self.sidecar.unlink()
        before = self.read_store()
        r = self.cli("restore", self.out)
        self.assertEqual(r.returncode, 2)
        self.assertTrue(r.stderr.startswith("error: missing checksum file "), r.stderr)
        self.assertIn("backup.json.sha256", r.stderr)
        self.assertEqual(self.read_store(), before)

    def test_sidecar_whitespace_and_case_are_tolerated(self):
        self.cli("backup", "--out", self.out)
        digest = self.sidecar.read_text(encoding="utf-8").strip().upper()
        self.sidecar.write_text("  " + digest + "  \n", encoding="utf-8")
        r = self.cli("restore", self.out)
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_missing_files(self):
        r = self.cli("restore", str(self.dir / "nope.json"))
        self.assertEqual(r.returncode, 2)
        self.assertTrue(r.stderr.startswith("error: no such file: "), r.stderr)
        Path(self.store).unlink()
        r = self.cli("backup", "--out", self.out)
        self.assertEqual(r.returncode, 2)
        self.assertTrue(r.stderr.startswith("error: no such file: "), r.stderr)
        self.assertFalse(Path(self.out).exists())


if __name__ == "__main__":
    unittest.main()
