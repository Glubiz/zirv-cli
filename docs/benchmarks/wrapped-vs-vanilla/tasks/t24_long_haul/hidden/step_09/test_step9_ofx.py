import json
import subprocess
import sys
import tempfile
import unittest
from decimal import Decimal
from pathlib import Path

from ledgerlite.ofx import read_ofx

SAMPLE_OFX = """OFXHEADER:100
DATA:OFXSGML
VERSION:102

<OFX>
<BANKMSGSRSV1>
<STMTTRNRS>
<STMTRS>
<CURDEF>EUR
<BANKTRANLIST>
<STMTTRN>
<TRNTYPE>DEBIT
<DTPOSTED>20240115
<TRNAMT>-45.67
<NAME>CAFE PARIS
<MEMO>lunch
</STMTTRN>
<STMTTRN>
<TRNTYPE>CREDIT
<DTPOSTED>20240120120000
<TRNAMT>1200.00
<NAME>CLIENT PAYMENT
</STMTTRN>
</BANKTRANLIST>
</STMTRS>
</STMTTRNRS>
</BANKMSGSRSV1>
</OFX>
"""

NO_CURDEF_OFX = """<OFX>
<BANKTRANLIST>
<STMTTRN>
<DTPOSTED>20240105
<TRNAMT>-9.00
<NAME>LOCAL SHOP
</STMTTRN>
</BANKTRANLIST>
</OFX>
"""


def _run(*args):
    return subprocess.run(
        [sys.executable, "-m", "ledgerlite", *args],
        capture_output=True, text=True,
    )


class TestReadOfx(unittest.TestCase):
    def test_parses_fields_and_currency(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "sample.ofx"
            path.write_text(SAMPLE_OFX, encoding="utf-8")
            txns = read_ofx(path)
            self.assertEqual(len(txns), 2)
            self.assertEqual(txns[0].payee, "CAFE PARIS")
            self.assertEqual(txns[0].memo, "lunch")
            self.assertEqual(txns[0].amount, Decimal("-45.67"))
            self.assertEqual(txns[0].currency, "EUR")
            self.assertEqual(txns[0].date.isoformat(), "2024-01-15")
            # DTPOSTED with a trailing time component: only the date matters.
            self.assertEqual(txns[1].date.isoformat(), "2024-01-20")
            self.assertEqual(txns[1].memo, "")

    def test_default_currency_used_when_no_curdef(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "nocur.ofx"
            path.write_text(NO_CURDEF_OFX, encoding="utf-8")
            txns = read_ofx(path)
            self.assertEqual(txns[0].currency, "USD")
            txns2 = read_ofx(path, default_currency="GBP")
            self.assertEqual(txns2[0].currency, "GBP")

    def test_ids_start_at_given_offset(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "sample.ofx"
            path.write_text(SAMPLE_OFX, encoding="utf-8")
            txns = read_ofx(path, start_id=10)
            self.assertEqual([t.id for t in txns], [10, 11])


class TestImportOfxCli(unittest.TestCase):
    def test_import_ofx_and_autocategorize(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "sample.ofx"
            path.write_text(SAMPLE_OFX, encoding="utf-8")
            store = str(Path(tmp) / "ledger.json")
            r = _run("import-ofx", str(path), "--store", store)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("Imported 2 transactions", r.stdout)
            data = json.loads(Path(store).read_text(encoding="utf-8"))
            self.assertEqual(data["transactions"][0]["currency"], "EUR")

    def test_append_continues_ids_from_existing_store(self):
        # Recall: this must follow the exact same "continue after the
        # highest existing id" rule established earlier this session for
        # `import --append`, without it being restated here.
        with tempfile.TemporaryDirectory() as tmp:
            store = Path(tmp) / "ledger.json"
            store.write_text(json.dumps({"transactions": [
                {"id": 1, "date": "2023-12-31", "amount": "-1.00", "payee": "PRIOR",
                 "memo": "", "category": None, "currency": "USD"},
                {"id": 2, "date": "2023-12-31", "amount": "-2.00", "payee": "PRIOR2",
                 "memo": "", "category": None, "currency": "USD"},
            ]}), encoding="utf-8")
            path = Path(tmp) / "sample.ofx"
            path.write_text(SAMPLE_OFX, encoding="utf-8")
            r = _run("import-ofx", str(path), "--store", str(store), "--append")
            self.assertEqual(r.returncode, 0, r.stderr)
            data = json.loads(store.read_text(encoding="utf-8"))
            ids = [t["id"] for t in data["transactions"]]
            self.assertEqual(ids, [1, 2, 3, 4])

    def test_default_replaces(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = Path(tmp) / "ledger.json"
            store.write_text(json.dumps({"transactions": [
                {"id": 1, "date": "2023-12-31", "amount": "-1.00", "payee": "PRIOR",
                 "memo": "", "category": None, "currency": "USD"},
            ]}), encoding="utf-8")
            path = Path(tmp) / "sample.ofx"
            path.write_text(SAMPLE_OFX, encoding="utf-8")
            _run("import-ofx", str(path), "--store", str(store))
            data = json.loads(store.read_text(encoding="utf-8"))
            payees = [t["payee"] for t in data["transactions"]]
            self.assertNotIn("PRIOR", payees)
            self.assertEqual(len(data["transactions"]), 2)


if __name__ == "__main__":
    unittest.main()
