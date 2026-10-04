"""Per-rule worker compliance metrics from a run's archived transcripts.

    python compliance.py <run dir>

Reads <run dir>/transcripts/*.jsonl (Claude Code transcript JSONL) and counts
tool-use patterns that the worker-prompt rules target. Works on any finished
run (stdlib only); a missing or empty transcripts dir gives all zeros.
"""
import json
import re
import sys
from collections import defaultdict
from pathlib import Path

EDIT_TOOLS = {"Edit", "MultiEdit", "Write"}
SHELL_READ = ("sed -n", "cat ", "head ", "tail ", "nl ", "awk ")
INTERPRETER = re.compile(r"\b(?:python3?|py|node|ruby|perl)(?:\.exe)?\b")
INLINE = re.compile(r"<<|\s-[ce]\b")
FILE_WRITE = re.compile(
    r"\.write\(|write_text\(|write_bytes\(|writeFileSync\(|writeFile\("
    r"|\bopen\([^)]*['\"](?:[wax]|r\+)b?\+?['\"]")


def _text(content):
    if isinstance(content, str):
        return content
    try:
        return json.dumps(content, ensure_ascii=False)
    except Exception:
        return str(content)


def _is_shell_read(cmd):
    return cmd.startswith(SHELL_READ) and ">" not in cmd and "<<" not in cmd


def _is_script_write(cmd):
    return bool(INTERPRETER.search(cmd) and INLINE.search(cmd) and FILE_WRITE.search(cmd))


def scan(run_dir):
    out = {"api_rounds": 0, "calls_per_round": 0.0, "read_tool": 0, "edit_tool": 0,
           "shell_reads": 0, "script_writes": 0, "syntax_errors_after_script_write": 0,
           "edit_guard_denials": 0, "zirv_ctx_run": 0, "parallel_rounds": 0}
    try:
        files = sorted((Path(run_dir) / "transcripts").glob("*.jsonl"))
    except Exception:
        return out
    calls_by_round = defaultdict(int)
    total_calls = 0
    pending = set()  # script-write tool_use ids still awaiting a SyntaxError result
    for f in files:
        try:
            lines = f.read_text(encoding="utf-8", errors="replace").splitlines()
        except Exception:
            continue
        for n, line in enumerate(lines):
            try:
                e = json.loads(line)
            except Exception:
                continue
            if not isinstance(e, dict) or e.get("isSidechain") is True:
                continue
            msg = e.get("message")
            if not isinstance(msg, dict):
                continue
            content = msg.get("content")
            blocks = content if isinstance(content, list) else []
            if e.get("type") == "assistant":
                rid = msg.get("id") or f"{f.name}:{n}"
                calls_by_round[rid] += 0
                for b in blocks:
                    if not isinstance(b, dict) or b.get("type") != "tool_use":
                        continue
                    calls_by_round[rid] += 1
                    total_calls += 1
                    name = b.get("name")
                    inp = b.get("input") if isinstance(b.get("input"), dict) else {}
                    if name == "Read":
                        out["read_tool"] += 1
                    elif name in EDIT_TOOLS:
                        out["edit_tool"] += 1
                        pending.clear()
                    elif name == "Bash":
                        cmd = _text(inp.get("command") or "").strip()
                        if "zirv ctx run" in cmd:
                            out["zirv_ctx_run"] += 1
                        if _is_shell_read(cmd):
                            out["shell_reads"] += 1
                        if _is_script_write(cmd):
                            out["script_writes"] += 1
                            pending.clear()
                            if b.get("id"):
                                pending.add(b["id"])
            elif e.get("type") == "user":
                for b in blocks:
                    if not isinstance(b, dict) or b.get("type") != "tool_result":
                        continue
                    text = _text(b.get("content"))
                    if "zirv edit guard:" in text:
                        out["edit_guard_denials"] += 1
                    tid = b.get("tool_use_id")
                    if tid in pending and ("SyntaxError" in text or "IndentationError" in text):
                        out["syntax_errors_after_script_write"] += 1
                        pending.discard(tid)
    out["api_rounds"] = len(calls_by_round)
    out["calls_per_round"] = total_calls / len(calls_by_round) if calls_by_round else 0.0
    out["parallel_rounds"] = sum(1 for c in calls_by_round.values() if c >= 2)
    return out


if __name__ == "__main__":
    print(json.dumps(scan(sys.argv[1]), indent=2))
