#!/usr/bin/env python3
"""Unit tests for compliance.scan. stdlib `unittest` only (CONTRACT.md forbids
pytest); synthetic transcripts in a throwaway tempdir, no billed commands."""
import json
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import compliance  # noqa: E402


def use(mid, *calls, side=False):
    blocks = [{"type": "tool_use", "id": i, "name": n, "input": inp} for i, n, inp in calls]
    e = {"type": "assistant", "message": {"id": mid, "content": blocks}}
    if side:
        e["isSidechain"] = True
    return e


def result(tid, text):
    return {"type": "user", "message": {"content": [
        {"type": "tool_result", "tool_use_id": tid, "content": text}]}}


SCRIPT = "python - <<'EOF'\nopen('a.py', 'w').write('x')\nEOF"


class ScanTests(unittest.TestCase):
    def setUp(self):
        self.dir = Path(tempfile.mkdtemp())
        (self.dir / "transcripts").mkdir()

    def tearDown(self):
        shutil.rmtree(self.dir, ignore_errors=True)

    def scan(self, *entries):
        lines = [json.dumps(e) for e in entries]
        (self.dir / "transcripts" / "t.jsonl").write_text("not json\n" + "\n".join(lines), encoding="utf-8")
        return compliance.scan(str(self.dir))

    def test_empty_dir_is_all_zeros(self):
        r = compliance.scan(str(self.dir))
        self.assertTrue(all(v == 0 for v in r.values()))
        self.assertEqual(compliance.scan(str(self.dir / "missing"))["api_rounds"], 0)

    def test_script_write_then_syntax_error_counts_once(self):
        r = self.scan(
            use("m1", ("a", "Bash", {"command": SCRIPT})),
            result("a", "SyntaxError: bad\nIndentationError: worse"),
            use("m2", ("b", "Edit", {})),
            result("b", "ok"))
        self.assertEqual(r["script_writes"], 1)
        self.assertEqual(r["syntax_errors_after_script_write"], 1)
        self.assertEqual(r["edit_tool"], 1)

    def test_syntax_error_after_an_edit_does_not_count(self):
        r = self.scan(
            use("m1", ("a", "Bash", {"command": SCRIPT})),
            use("m2", ("b", "Write", {})),
            result("a", "SyntaxError: late"))
        self.assertEqual(r["syntax_errors_after_script_write"], 0)

    def test_sidechain_entries_ignored(self):
        r = self.scan(use("m1", ("a", "Read", {}), side=True), use("m2", ("b", "Read", {})))
        self.assertEqual((r["api_rounds"], r["read_tool"]), (1, 1))

    def test_duplicate_message_ids_are_one_round(self):
        r = self.scan(use("m1", ("a", "Read", {})), use("m1", ("b", "Read", {})))
        self.assertEqual(r["api_rounds"], 1)
        self.assertEqual(r["calls_per_round"], 2.0)
        self.assertEqual(r["parallel_rounds"], 1)

    def test_parallel_round_counts_only_multi_call_rounds(self):
        r = self.scan(use("m1", ("a", "Read", {}), ("b", "Read", {})), use("m2", ("c", "Read", {})))
        self.assertEqual((r["api_rounds"], r["parallel_rounds"]), (2, 1))

    def test_shell_reads_and_ctx_run_and_guard_denials(self):
        r = self.scan(
            use("m1", ("a", "Bash", {"command": "sed -n 1,5p f"}),
                ("b", "Bash", {"command": "cat f > g"}),
                ("c", "Bash", {"command": "zirv ctx run x"})),
            result("a", [{"type": "text", "text": "zirv edit guard: use Edit"}]))
        self.assertEqual(r["shell_reads"], 1)
        self.assertEqual(r["zirv_ctx_run"], 1)
        self.assertEqual(r["edit_guard_denials"], 1)


if __name__ == "__main__":
    unittest.main()
