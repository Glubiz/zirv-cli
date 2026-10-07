#!/usr/bin/env python3
"""Orchestration lane: an interactive Claude Code session (an Opus seat that may
delegate to subagents) is driven through a Windows ConPTY (pywinpty) on ONE long
multi-feature request, `vanilla` (claude + superpowers) vs `zirv ctx wrap`.

`run.py` measures single-seat headless `claude -p` runs on short tasks, where
zirv's orchestration (zirv:worker subagents, model tiers, lean subagents) never
fires. This lane keeps the same template/grading/judge machinery (imported from
run.py, never duplicated) but drives the interactive TUI instead. See README.md,
"Orchestration lane".

    python orch.py --bench-root . --tasks o01_ledger_suite --conds vanilla,zirv-nojev \
        --reps 1 --vanilla-plugin-dir <superpowers> --zirv-dir <dir with zirv.exe>
    python orch.py --bench-root . --report

Pure parts (price table, transcript parsing, completion detection, dialog
detection, prompt composition, report) are unit-tested in test_orch.py; the PTY
driver itself is only exercised by a live run.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import run as H  # noqa: E402  (template copy, graders, judge, env helpers)

ORCH_KIND = "orch"
DEFAULT_CONDS = "vanilla,zirv-nojev"
ORCH_CONDS = ["vanilla", H.NOJEV_COND, H.JEV_FULL_COND]
DEFAULT_TIMEOUT_MIN = 60
DEFAULT_IDLE_S = 20.0
DEFAULT_RUNS_SUBDIR = "orch-runs"
SCREEN_ROWS, SCREEN_COLS = 50, 200

# --------------------------------------------------------------------------
# Prices (USD per million tokens), copied from zirv's own catalogue
# (src/commands/ctx/catalogue.rs: OPUS_5, OPUS_5_5, SONNET_5, HAIKU_4_5; the
# fable rungs too). zirv's table has ONE cache-write rate (the 5-minute one);
# Anthropic's 1-hour write is billed at 2x the input rate, which zirv does not
# model, so `write_1h` here is 2 * input (an assumption, flagged in the README).
# --------------------------------------------------------------------------
PRICES = {
    "opus-5": {"input": 5.0, "write_5m": 6.25, "read": 0.5, "output": 25.0},
    "opus-5-5": {"input": 4.0, "write_5m": 5.0, "read": 0.2, "output": 20.0},
    "sonnet-5": {"input": 2.0, "write_5m": 2.5, "read": 0.2, "output": 10.0},
    "haiku-4-5": {"input": 1.0, "write_5m": 1.25, "read": 0.1, "output": 5.0},
    "fable-5": {"input": 10.0, "write_5m": 12.5, "read": 1.0, "output": 50.0},
    "fable-5-1": {"input": 10.0, "write_5m": 12.5, "read": 0.25, "output": 50.0},
}


def price_key(model):
    """The PRICES key for a transcript model id (`claude-opus-5-20260915`,
    `claude-sonnet-5-5`, `claude-haiku-4-5-20251001`, ...), or None when unknown."""
    m = (model or "").lower()
    if "opus" in m:
        return "opus-5-5" if re.search(r"opus-5-5", m) else "opus-5"
    if "sonnet" in m:
        return "sonnet-5"
    if "haiku" in m:
        return "haiku-4-5"
    if "fable" in m or "mythos" in m:
        return "fable-5-1" if "fable-5-1" in m else "fable-5"
    return None


def price_of(model):
    key = price_key(model)
    if key is None:
        return None
    p = dict(PRICES[key])
    p["write_1h"] = 2 * p["input"]
    return p


def usage_cost(bucket, model):
    """USD cost of one token bucket (input/read/write_5m/write_1h/output) for
    `model`; None when the model has no price."""
    p = price_of(model)
    if p is None:
        return None
    return (bucket.get("input", 0) * p["input"] + bucket.get("read", 0) * p["read"]
            + bucket.get("write_5m", 0) * p["write_5m"] + bucket.get("write_1h", 0) * p["write_1h"]
            + bucket.get("output", 0) * p["output"]) / 1e6


# --------------------------------------------------------------------------
# Transcripts
# --------------------------------------------------------------------------
def read_records(path):
    out = []
    try:
        text = Path(path).read_text(encoding="utf-8", errors="replace")
    except OSError:
        return out
    for ln in text.splitlines():
        try:
            obj = json.loads(ln)
        except ValueError:
            continue
        if isinstance(obj, dict):
            out.append(obj)
    return out


def _blocks(rec):
    content = (rec.get("message") or {}).get("content")
    return content if isinstance(content, list) else []


def collect_usage(record_lists):
    """Per-model token totals over the given record lists (parent + subagent
    transcripts). Claude Code writes one record per content block of an API
    response, all carrying the same `requestId` and usage, so requests are
    de-duplicated by requestId keeping the record with the most output tokens.
    Returns {model: {requests, input, read, write_5m, write_1h, output, cost_usd}}."""
    reqs = {}
    for records in record_lists:
        for o in records:
            if o.get("type") != "assistant":
                continue
            msg = o.get("message") or {}
            u = msg.get("usage")
            model = msg.get("model")
            if not u or not model or model.startswith("<"):
                continue
            rid = o.get("requestId") or msg.get("id") or o.get("uuid")
            prev = reqs.get(rid)
            if prev is None or (u.get("output_tokens") or 0) >= (prev[0].get("output_tokens") or 0):
                reqs[rid] = (u, model)
    per_model = {}
    for u, model in reqs.values():
        cc = u.get("cache_creation") or {}
        total_w = u.get("cache_creation_input_tokens") or 0
        w5, w1 = cc.get("ephemeral_5m_input_tokens"), cc.get("ephemeral_1h_input_tokens")
        if w5 is None and w1 is None:
            w5, w1 = total_w, 0
        b = per_model.setdefault(model, {"requests": 0, "input": 0, "read": 0, "write_5m": 0,
                                         "write_1h": 0, "output": 0})
        b["requests"] += 1
        b["input"] += u.get("input_tokens") or 0
        b["read"] += u.get("cache_read_input_tokens") or 0
        b["write_5m"] += w5 or 0
        b["write_1h"] += w1 or 0
        b["output"] += u.get("output_tokens") or 0
    for model, b in per_model.items():
        b["cost_usd"] = usage_cost(b, model)
    return per_model


def total_cost(per_model):
    """(sum of priced cost, [models with no price])."""
    cost, unpriced = 0.0, []
    for model, b in per_model.items():
        if b.get("cost_usd") is None:
            unpriced.append(model)
        else:
            cost += b["cost_usd"]
    return cost, unpriced


AGENT_TOOLS = ("Agent", "Task")


def main_agent_launches(main_records):
    """[{subagent_type, model, description, tool_use_id}] for every Agent/Task
    tool_use in the parent transcript, in order."""
    out = []
    for o in main_records:
        if o.get("type") != "assistant":
            continue
        for c in _blocks(o):
            if isinstance(c, dict) and c.get("type") == "tool_use" and c.get("name") in AGENT_TOOLS:
                inp = c.get("input") or {}
                out.append({"tool_use_id": c.get("id"), "subagent_type": inp.get("subagent_type"),
                            "model": inp.get("model"), "description": inp.get("description")})
    return out


def final_text(main_records):
    """Text of the last assistant message that carries any text block."""
    for o in reversed(main_records):
        if o.get("type") == "assistant":
            texts = [c.get("text") for c in _blocks(o) if isinstance(c, dict) and c.get("type") == "text"]
            texts = [t for t in texts if t]
            if texts:
                return "\n".join(texts)
    return ""


def _last_turn_record(records):
    """The last record that decides whether the agent is mid-turn: an assistant or
    user message, or an enqueued notification. Bookkeeping types are skipped."""
    for o in reversed(records):
        t = o.get("type")
        if t in ("assistant", "user"):
            return o
        if t == "queue-operation" and o.get("operation") == "enqueue":
            return o
    return None


def transcript_state(main_records, sub_records_by_id):
    """Pure snapshot of "is the agent finished" from transcript content:
    - main_ended: the last turn record of the parent is an assistant message with
      stop_reason end_turn (not a tool_use, not a user/tool_result, no queued
      notification after it);
    - pending_tools: parent tool_use ids with no tool_result yet;
    - subs_running: subagent ids whose last turn record is not an assistant end_turn
      (a subagent that wrote nothing yet counts as running)."""
    last = _last_turn_record(main_records)
    main_ended = bool(last and last.get("type") == "assistant"
                      and (last.get("message") or {}).get("stop_reason") == "end_turn")
    uses, results = set(), set()
    for o in main_records:
        for c in _blocks(o):
            if not isinstance(c, dict):
                continue
            if c.get("type") == "tool_use" and o.get("type") == "assistant":
                uses.add(c.get("id"))
            elif c.get("type") == "tool_result":
                results.add(c.get("tool_use_id"))
    subs_running = []
    for sid, recs in sub_records_by_id.items():
        lr = _last_turn_record(recs)
        ended = bool(lr and lr.get("type") == "assistant"
                     and (lr.get("message") or {}).get("stop_reason") == "end_turn")
        if not ended:
            subs_running.append(sid)
    return {"main_ended": main_ended, "pending_tools": sorted(uses - results),
            "subs_running": sorted(subs_running), "subagents": len(sub_records_by_id),
            "turns_seen": sum(1 for o in main_records if o.get("type") == "assistant")}


def is_complete(state, idle_for_s, idle_s=DEFAULT_IDLE_S):
    """True when the run is over: main transcript ended with end_turn, no pending
    tool call, no subagent still running, and nothing (screen or transcript) moved
    for `idle_s` seconds."""
    return bool(state["main_ended"] and not state["pending_tools"] and not state["subs_running"]
                and idle_for_s >= idle_s)


# --------------------------------------------------------------------------
# Screen / dialogs
# --------------------------------------------------------------------------
ANSI_RE = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[()][A-Z0-9]|\x1b[=>]")
SELECTED_RE = re.compile(r"^[\s│|]*(?:[❯›]\s*(?:\d+[.)]\s*)?|>\s*\d+[.)]\s*)(\S.*?)[\s│|]*$")
DOWN, ENTER = "\x1b[B", "\r"


def strip_ansi(text):
    return ANSI_RE.sub("", text)


def detect_dialog(screen_text):
    """Keystrokes ([(text, delay_after_s)]) that answer a startup/permission dialog
    visible in `screen_text`, or None when no dialog is showing. The affirmative
    option is chosen: if the cursor sits on a No/Exit/Deny option and a Yes/accept
    option exists, Down then Enter, otherwise just Enter."""
    low = screen_text.lower()
    squeezed = re.sub(r"\s+", "", low)
    selected = None
    for ln in screen_text.splitlines():
        m = SELECTED_RE.match(ln)
        if m:
            selected = m.group(1).strip().lower()
    has_menu = selected is not None
    # A menu alone is not a dialog (the ready input box also draws a cursor line);
    # it must be accompanied by the dialogs' own confirm/cancel footer or question.
    footer = any(w in low for w in ("enter to confirm", "esc to cancel", "do you want", "i accept",
                                    "enter to select"))
    if not ((has_menu and footer) or "pressenter" in squeezed or "entertocontinue" in squeezed):
        return None
    if has_menu:
        label = selected
        negative = label.startswith(("no", "exit", "deny", "quit"))
        has_yes = bool(re.search(r"(^|\n)[\s│|❯›]*(\d+[.)]\s*)?(yes|accept)", low))
        if negative and has_yes:
            return [(DOWN, 0.4), (ENTER, 0.0)]
    return [(ENTER, 0.0)]


def input_box_ready(screen_text):
    """True when the TUI shows its prompt box (ready for a pasted request)."""
    low = screen_text.lower()
    return "for shortcuts" in low or bool(re.search(r"^\s*[│|]?\s*[>❯]\s", screen_text, re.M)) \
        and "do you" not in low and "trust" not in low


class Screen:
    """A virtual terminal (pyte, when installed) so dialog/idle checks read what is
    actually drawn rather than the raw escape stream."""

    def __init__(self, rows=SCREEN_ROWS, cols=SCREEN_COLS):
        try:
            import pyte
            self.screen = pyte.Screen(cols, rows)
            self.stream = pyte.Stream(self.screen)
        except Exception:
            self.screen = self.stream = None
        self.raw_tail = ""

    def feed(self, data):
        self.raw_tail = (self.raw_tail + strip_ansi(data))[-6000:]
        if self.stream is not None:
            try:
                self.stream.feed(data)
            except Exception:
                pass

    def text(self):
        if self.screen is not None:
            return "\n".join(line.rstrip() for line in self.screen.display)
        return self.raw_tail

    def clear_tail(self):
        self.raw_tail = ""


# --------------------------------------------------------------------------
# Tasks, prompts, grading
# --------------------------------------------------------------------------
def read_sources(task_dir):
    p = Path(task_dir) / "sources.txt"
    return [ln.strip() for ln in p.read_text(encoding="utf-8").splitlines()
            if ln.strip() and not ln.strip().startswith("#")]


def compose_task_prompt(bench_root, sources):
    """The single request an orch task sends: its source tasks' prompts as numbered
    parts under a short shared intro. Used to generate tasks/<orch>/prompt.txt and
    asserted equal to the committed file by test_orch.py."""
    n = len(sources)
    parts = []
    for i, src in enumerate(sources, 1):
        body = H.read_text(Path(bench_root) / "tasks" / src / "prompt.txt").replace("\r\n", "\n").strip()
        parts.append(f"=== Part {i} of {n} ===\n\n{body}")
    intro = (f"This is one request with {n} separate parts, all for the ledgerlite repo in the current "
             f"directory. The parts share files (notably ledgerlite/cli.py and ledgerlite/store.py), so "
             f"when you are done all {n} must work together, and every existing test must still pass. "
             f"Each part's contract is exact. Finish with a short summary of what you did per part.")
    return intro + "\n\n" + "\n\n".join(parts) + "\n"


def launch_prompt(task_prompt):
    return H.NONINTERACTIVE_NOTE + task_prompt


def grade_sources(bench_root, task_dir, repo, result_txt):
    """Run every source task's own grade.py against `repo`. Returns
    {score (mean of per-source scores), per_source {id: score}, passed, total,
    visible_ok, details}."""
    per, passed, total, visible, details = {}, 0, 0, True, []
    for src in read_sources(task_dir):
        grade_py = Path(bench_root) / "tasks" / src / "grade.py"
        obj, _proc = H.run_grade_py(grade_py, repo, result_txt)
        obj = obj or {}
        per[src] = float(obj.get("score") or 0.0)
        passed += int(obj.get("passed") or 0)
        total += int(obj.get("total") or 0)
        visible = visible and bool(obj.get("visible_ok"))
        details.append(f"{src}: {obj.get('passed')}/{obj.get('total')}")
    H.cleanup_hidden_tests(repo)
    mean = sum(per.values()) / len(per) if per else 0.0
    return {"score": round(mean, 4), "per_source": per, "passed": passed, "total": total,
            "visible_ok": visible, "details": "; ".join(details)}


# --------------------------------------------------------------------------
# Child process: argv, env
# --------------------------------------------------------------------------
# What marks this process as running inside an outer agent session (see
# sessions/nesting.rs: SUPERVISION_ENV + nested_session_evidence), plus the outer
# session's effort override, so both arms start from the same clean slate.
ENV_SCRUB_EXACT = {"CLAUDECODE", "CLAUDE_PID", "CLAUDE_EFFORT", "ZIRV_CTX_HEADLESS"}
ENV_SCRUB_PREFIXES = ("CLAUDE_CODE_", "ZIRV_CTX_SESSION", "ZIRV_CTX_PARENT", "ZIRV_CTX_SOCKET",
                      "ZIRV_CTX_DASH", "ZIRV_CTX_SEAT", "ZIRV_CTX_LAUNCH_MODE", "ZIRV_CTX_TRANSCRIPT",
                      "ZIRV_CTX_PROXY_DECIDED", "ZIRV_CTX_INTERNAL", "ZIRV_CTX_RESULT_",
                      "ZIRV_CTX_SAFETY_", "ZIRV_CTX_AGENT")


def build_argv(cond, seat_model, plugin_dir, zirv_exe=None, claude_exe=None):
    claude_exe = claude_exe or H.CLAUDE_EXE
    if cond == "vanilla":
        argv = [claude_exe, "--model", seat_model, "--setting-sources", "project,local",
                "--permission-mode", "dontAsk", f"--allowedTools={H.VANILLA_ALLOWED_TOOLS}"]
        if plugin_dir:
            argv += ["--plugin-dir", str(plugin_dir)]
        plugins = H.operator_plugin_settings()
        if plugins:
            argv += ["--settings", plugins]
        return argv
    if cond in (H.NOJEV_COND, H.JEV_FULL_COND):
        return [zirv_exe or H.zirv_exe(), "ctx", "wrap", "--force-pace", "--",
                claude_exe, "--model", seat_model]
    raise ValueError(f"unknown orchestration cond: {cond} (allowed: {ORCH_CONDS})")


def build_env(cond, run_dir, base=None):
    """Child environment: the inherited one minus agent-nesting markers (so wrap's
    nesting guard does not refuse), plus the cond's env WITHOUT the headless-only
    cost levers (this is the interactive path), with the run's own zirv state dir."""
    env = dict(os.environ if base is None else base)
    for k in list(env):
        if k in ENV_SCRUB_EXACT or k.startswith(ENV_SCRUB_PREFIXES):
            env.pop(k)
    extra = {k: v for k, v in H.cond_env_for(cond).items() if k not in H.ZIRV_HEADLESS_LEVERS}
    extra = H.isolate_state(extra, run_dir, cond)
    for k, v in extra.items():
        if v is None:
            env.pop(k, None)
        else:
            env[k] = v
    return env


def project_dirs(repo):
    """Claude project dirs a run's transcripts can live in (cwd slug, plus its realpath's)."""
    root = Path(os.path.expanduser("~")) / ".claude" / "projects"
    seen, out = set(), []
    for cand in (H.project_slug(repo), H.project_slug(os.path.realpath(repo))):
        if cand not in seen:
            seen.add(cand)
            out.append(root / cand)
    return out


def find_transcripts(repo):
    """(main session .jsonl files of this run, sorted by size desc). The repo dir is
    unique per run, so every session file in its project dir belongs to it."""
    mains = []
    for d in project_dirs(repo):
        if d.is_dir():
            mains += [p for p in d.glob("*.jsonl")]
    return sorted(mains, key=lambda p: p.stat().st_size, reverse=True)


def subagent_files(main_path):
    sdir = Path(main_path).with_suffix("") / "subagents"
    return sorted(sdir.rglob("agent-*.jsonl")) if sdir.is_dir() else []


def newest_mtime(paths):
    ts = []
    for p in paths:
        try:
            ts.append(Path(p).stat().st_mtime)
        except OSError:
            pass
    return max(ts) if ts else 0.0


# --------------------------------------------------------------------------
# PTY driver (pywinpty)
# --------------------------------------------------------------------------
class PtyRun:
    """One interactive session in a ConPTY. Everything the driver does to the child
    goes through `self.proc` (the pywinpty handle) -- it never kills by name."""

    def __init__(self, argv, cwd, env, log_path):
        from winpty import PtyProcess
        self.proc = PtyProcess.spawn(argv, cwd=str(cwd), env=env, dimensions=(SCREEN_ROWS, SCREEN_COLS))
        self.screen = Screen()
        self.lock = threading.Lock()
        self.last_change = time.time()
        self.eof = False
        self.log = open(log_path, "w", encoding="utf-8", errors="replace")
        self._thread = threading.Thread(target=self._pump, daemon=True)
        self._thread.start()

    def _pump(self):
        while True:
            try:
                data = self.proc.read(65536)
            except EOFError:
                break
            except Exception:
                break
            if not data:
                if not self.proc.isalive():
                    break
                time.sleep(0.05)
                continue
            with self.lock:
                before = self.screen.text()
                self.screen.feed(data)
                self.log.write(data)
                self.log.flush()
                if self.screen.text() != before:
                    self.last_change = time.time()
        self.eof = True

    def screen_text(self):
        with self.lock:
            return self.screen.text()

    def tail_text(self):
        with self.lock:
            return self.screen.raw_tail

    def send(self, text):
        self.proc.write(text)

    def send_keys(self, keys):
        for text, delay in keys:
            self.send(text)
            time.sleep(delay)

    def paste_and_submit(self, text):
        # Bracketed paste keeps the request's newlines from submitting early.
        self.send("\x1b[200~" + text.replace("\r\n", "\n").replace("\n", "\r") + "\x1b[201~")
        time.sleep(2.0)
        self.send(ENTER)

    def alive(self):
        try:
            return self.proc.isalive()
        except Exception:
            return False

    def shutdown(self):
        """Ask the TUI to quit, then close the handle; only this PTY's own process."""
        if self.alive():
            try:
                self.send("/exit")
                time.sleep(1.0)
                self.send(ENTER)
            except Exception:
                pass
            end = time.time() + 20
            while time.time() < end and self.alive():
                time.sleep(0.5)
        forced = self.alive()
        if forced:
            try:
                self.proc.terminate(force=True)
            except Exception:
                pass
        try:
            self.proc.close(force=True)
        except Exception:
            pass
        self._thread.join(timeout=5)
        self.log.close()
        return forced


def drive(argv, cwd, env, prompt, log_path, timeout_s, idle_s=DEFAULT_IDLE_S, startup_s=120.0):
    """Start the session, answer startup dialogs, submit `prompt`, wait for
    completion (transcript-based, see is_complete) or the timeout, shut down.
    Returns a dict: submit_wall_s, end_wall_s, timed_out, completed, dialogs,
    usage_limit, startup_ok, forced_kill, final_screen."""
    t0 = time.time()
    run = PtyRun(argv, cwd, env, log_path)
    out = {"dialogs": [], "timed_out": False, "completed": False, "usage_limit": False,
           "startup_ok": False, "forced_kill": False, "wall_s": None, "submit_at": None}
    try:
        # 1. startup: dialogs until the input box shows
        quiet_ok_since = None
        while time.time() - t0 < startup_s and run.alive():
            text = run.screen_text()
            keys = detect_dialog(text)
            if keys and time.time() - run.last_change >= 1.0:
                out["dialogs"].append(f"t+{time.time() - t0:.0f}s: startup dialog answered ({len(keys)} key(s))")
                run.send_keys(keys)
                run.screen.clear_tail()
                time.sleep(1.5)
                continue
            if input_box_ready(text) and not keys and time.time() - run.last_change >= 2.0:
                out["startup_ok"] = True
                break
            time.sleep(0.5)
        if not out["startup_ok"]:
            out["final_screen"] = run.screen_text()
            return out
        # 2. submit
        run.paste_and_submit(prompt)
        submit = time.time()
        out["submit_at"] = submit
        # 3. wait for completion
        seen_files = set()
        while True:
            now = time.time()
            if now - submit > timeout_s:
                out["timed_out"] = True
                break
            if not run.alive():
                break
            tail = run.tail_text().lower()
            if any(m in tail for m in H.USAGE_LIMIT_MARKERS):
                out["usage_limit"] = True
                break
            mains = find_transcripts(cwd)
            if mains:
                main = mains[0]
                subs = subagent_files(main)
                state = transcript_state(read_records(main), {s.stem: read_records(s) for s in subs})
                last_activity = max(newest_mtime([main] + subs), run.last_change)
                # a permission/confirmation dialog mid-run: answer it, count it
                keys = detect_dialog(run.screen_text())
                if keys and now - run.last_change >= 3.0:
                    out["dialogs"].append(f"t+{now - t0:.0f}s: mid-run dialog")
                    run.send_keys(keys)
                    run.screen.clear_tail()
                    time.sleep(1.5)
                    continue
                if is_complete(state, now - last_activity, idle_s):
                    out["completed"] = True
                    out["wall_s"] = newest_mtime([main] + subs) - submit
                    break
            time.sleep(2.0)
        if out["wall_s"] is None:
            out["wall_s"] = time.time() - submit
        out["final_screen"] = run.screen_text()
        return out
    finally:
        out["forced_kill"] = run.shutdown()


# --------------------------------------------------------------------------
# One run
# --------------------------------------------------------------------------
def prepare_repo(bench_root, run_dir):
    template = Path(bench_root) / "template"
    dirty = subprocess.run([H.GIT_EXE, "-C", str(template), "status", "--porcelain"],
                           capture_output=True, text=True).stdout.strip()
    if dirty:
        raise RuntimeError(f"template is not pristine, refusing to copy:\n{dirty}")
    if run_dir.exists():
        H.rmtree_robust(run_dir)
    run_dir.mkdir(parents=True)
    repo = run_dir / "repo"
    shutil.copytree(template, repo)
    return repo


def zirv_version(zirv_exe):
    try:
        p = subprocess.run([zirv_exe, "--version"], capture_output=True, text=True, timeout=30)
        return re.sub(r"^Version:\s*", "", (p.stdout or p.stderr).strip()) or None
    except Exception:
        return None


def analyze_transcripts(repo):
    """Parent + subagent usage, turns, subagents. All `.jsonl` main sessions in the
    run's project dir are summed (a /clear or restart makes more than one)."""
    mains = find_transcripts(repo)
    main_recs = [read_records(p) for p in mains]
    sub_paths = [s for p in mains for s in subagent_files(p)]
    sub_recs = [read_records(s) for s in sub_paths]
    per_model = collect_usage(main_recs + sub_recs)
    cost, unpriced = total_cost(per_model)
    parent_models = collect_usage(main_recs)
    launches = [x for recs in main_recs for x in main_agent_launches(recs)]
    by_tool = {x["tool_use_id"]: x for x in launches}
    subs = []
    for sp, recs in zip(sub_paths, sub_recs):
        meta = {}
        mp = sp.with_suffix(".meta.json")
        try:
            meta = json.loads(mp.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            pass
        used = collect_usage([recs])
        launch = by_tool.get(meta.get("toolUseId")) or {}
        subs.append({"agent_id": sp.stem.replace("agent-", ""), "type": meta.get("agentType") or launch.get("subagent_type"),
                     "model_requested": meta.get("model") or launch.get("model"),
                     "models_used": sorted(used), "requests": sum(b["requests"] for b in used.values()),
                     "description": meta.get("description") or launch.get("description")})
    return {
        "session_ids": [p.stem for p in mains],
        "per_model": per_model,
        "cost_usd": cost, "unpriced_models": unpriced,
        "turns": sum(b["requests"] for b in parent_models.values()),
        "sub_requests": sum(s["requests"] for s in subs),
        "subagents_spawned": len(subs),
        "subagents": subs,
        "agent_launches": [{k: v for k, v in x.items() if k != "tool_use_id"} for x in launches],
        "final_text": final_text(main_recs[0]) if main_recs else "",
        "transcripts": mains, "subagent_paths": sub_paths,
    }


def archive_transcripts(run_dir, ana):
    dest = Path(run_dir) / "transcripts"
    dest.mkdir(exist_ok=True)
    for p in ana["transcripts"]:
        shutil.copyfile(p, dest / p.name)
    for p in ana["subagent_paths"]:
        shutil.copyfile(p, dest / f"{p.parent.parent.name}__{p.name}")


def proxy_decisions(run_dir):
    """The zirv state dir's proxy decisions (rows of proxy-decisions.jsonl) for a zirv arm.
    Interactive wrap sessions usually write none (the proxy call is a headless-path step);
    `zirv_state_summary` carries what wrap does log."""
    return H.read_jsonl(Path(run_dir) / "zirv-state" / "proxy-decisions.jsonl")


def zirv_state_summary(run_dir):
    """Counts from the run's own zirv state dir: `logs/decisions.jsonl` rows by action
    (prompt-injected, workflow-auto-start, ...) and `logs/orchestrator-blocks.jsonl` rows."""
    logs = Path(run_dir) / "zirv-state" / "logs"
    actions = {}
    for row in H.read_jsonl(logs / "decisions.jsonl"):
        a = row.get("action") or "?"
        actions[a] = actions.get(a, 0) + 1
    return {"decisions_by_action": actions,
            "orchestrator_blocks": len(H.read_jsonl(logs / "orchestrator-blocks.jsonl"))}


def do_one_run(bench_root, task, cond, rep, args):
    bench_root = Path(bench_root)
    task_dir = bench_root / "tasks" / task
    run_dir = bench_root / args.runs_subdir / f"{task}__{cond}__r{rep}"
    if args.resume and H.result_is_valid(run_dir):
        print(f"{task} {cond} r{rep}: skip (resume, already ok)", flush=True)
        return None
    if cond == "vanilla":
        err = H.check_vanilla_plugin_dir(args.vanilla_plugin_dir)
        if err:
            raise RuntimeError(err)
    print(f"{task} {cond} r{rep}: starting", flush=True)
    repo = prepare_repo(bench_root, run_dir)
    task_prompt = H.read_text(task_dir / "prompt.txt")
    prompt = launch_prompt(task_prompt)
    (run_dir / "prompt.txt").write_text(prompt, encoding="utf-8")
    zexe = H.zirv_exe() if cond != "vanilla" else None
    argv = build_argv(cond, args.seat_model, args.vanilla_plugin_dir, zirv_exe=zexe)
    env = build_env(cond, run_dir)
    result = {"task": task, "cond": cond, "rep": rep, "seat_model": args.seat_model, "argv": argv[:6],
              "wall_s": None, "timed_out": False, "is_error": True,
              "claude_version": H.claude_version(), "zirv_version": zirv_version(zexe) if zexe else None}
    sources = read_sources(task_dir)
    result["sources"] = sources
    t_start = time.time()
    try:
        d = drive(argv, repo, env, prompt, run_dir / "pty.log", args.timeout_min * 60.0)
    except Exception as exc:  # PTY could not even start
        result["error"] = f"driver failed: {exc!r}"
        H.write_result(run_dir, result)
        return result
    result.update({"wall_s": d["wall_s"], "timed_out": d["timed_out"], "completed": d["completed"],
                   "usage_limit": d["usage_limit"], "startup_ok": d["startup_ok"], "forced_kill": d["forced_kill"],
                   "dialogs": d["dialogs"], "driver_total_s": time.time() - t_start})
    (run_dir / "final_screen.txt").write_text(d.get("final_screen") or "", encoding="utf-8")
    ana = analyze_transcripts(repo)
    archive_transcripts(run_dir, ana)
    result.update({"session_ids": ana["session_ids"], "tokens_by_model": ana["per_model"],
                   "cost_usd": ana["cost_usd"], "unpriced_models": ana["unpriced_models"],
                   "turns": ana["turns"], "sub_requests": ana["sub_requests"],
                   "subagents_spawned": ana["subagents_spawned"], "subagents": ana["subagents"],
                   "agent_launches": ana["agent_launches"]})
    result["is_error"] = not d["completed"]
    if cond == "vanilla":
        loaded, invoked = H.superpowers_in_transcripts(ana["transcripts"])
        result["superpowers_loaded"] = loaded
        result["superpowers_skills_invoked"] = invoked
        if not loaded:
            result["is_error"] = True
            result["invalid_reason"] = H.SUPERPOWERS_INVALID
    result_txt = run_dir / "result.txt"
    result_txt.write_text(ana["final_text"], encoding="utf-8")
    g = grade_sources(bench_root, task_dir, repo, result_txt)
    result.update({"score": g["score"], "per_source": g["per_source"], "passed": g["passed"],
                   "total": g["total"], "visible_ok": g["visible_ok"], "details": g["details"]})
    q, qr, _receipts = H.call_quality_judge(task_prompt, repo, ana["final_text"])
    result.update({"quality_score": q, "quality_reasoning": qr})
    if cond != "vanilla":
        result["proxy_decisions"] = proxy_decisions(run_dir)
        result["zirv_state"] = zirv_state_summary(run_dir)
        H.attach_jev_telemetry(run_dir, result)
    H.write_result(run_dir, result)
    print(f"{task} {cond} r{rep}: wall={result['wall_s']:.0f}s cost=${result['cost_usd']:.2f} "
          f"turns={result['turns']} subagents={result['subagents_spawned']} score={result['score']} "
          f"q={q} completed={result['completed']}", flush=True)
    return result


# --------------------------------------------------------------------------
# Report
# --------------------------------------------------------------------------
REPORT_FIELDS = ["wall_s", "cost_usd", "turns", "subagents_spawned", "score", "quality_score"]


def load_results(bench_root, runs_subdir=DEFAULT_RUNS_SUBDIR):
    out = []
    for rj in sorted((Path(bench_root) / runs_subdir).glob("*/result.json")):
        try:
            out.append(json.loads(rj.read_text(encoding="utf-8")))
        except (OSError, ValueError):
            pass
    return out


def build_report(results, fields=REPORT_FIELDS):
    """{cond: {n, <field>: {mean, median}}} over finished results (is_error false)."""
    by_cond = {}
    for r in results:
        if r.get("is_error", True):
            continue
        by_cond.setdefault(r.get("cond"), []).append(r)
    rep = {}
    for cond, rs in sorted(by_cond.items(), key=lambda kv: str(kv[0])):
        row = {"n": len(rs)}
        for f in fields:
            vals = [float(r[f]) for r in rs if isinstance(r.get(f), (int, float)) and not isinstance(r.get(f), bool)]
            row[f] = {"mean": statistics.mean(vals), "median": statistics.median(vals)} if vals else None
        rep[cond] = row
    return rep


def format_report(rep, fields=REPORT_FIELDS):
    if not rep:
        return "no finished orchestration runs"
    lines = ["cond".ljust(16) + "n".rjust(3) + "".join(f.rjust(26) for f in fields)]
    lines.append(" " * 19 + "".join("mean / median".rjust(26) for _ in fields))
    for cond, row in rep.items():
        cells = []
        for f in fields:
            v = row[f]
            cells.append("-".rjust(26) if v is None else f"{v['mean']:.3f} / {v['median']:.3f}".rjust(26))
        lines.append(cond.ljust(16) + str(row["n"]).rjust(3) + "".join(cells))
    return "\n".join(lines)


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------
def main(argv=None):
    ap = argparse.ArgumentParser(description="Orchestration lane: interactive seat, vanilla vs zirv ctx wrap")
    ap.add_argument("--bench-root", default=str(Path(__file__).resolve().parent))
    ap.add_argument("--tasks", default=None, help="comma list of orch task ids (default: every kind=orch task)")
    ap.add_argument("--conds", default=DEFAULT_CONDS, help=f"comma list from {ORCH_CONDS}")
    ap.add_argument("--seat-model", default="opus")
    ap.add_argument("--reps", type=int, default=1)
    ap.add_argument("--zirv-dir", default=None, help="directory holding the zirv under test (prepended to PATH)")
    ap.add_argument("--vanilla-plugin-dir", default=None, help="plugin dir for the vanilla arm (e.g. superpowers)")
    ap.add_argument("--timeout-min", type=float, default=DEFAULT_TIMEOUT_MIN)
    ap.add_argument("--stagger-s", type=float, default=0.0, help="seconds to wait between two run launches")
    ap.add_argument("--runs-subdir", default=DEFAULT_RUNS_SUBDIR)
    ap.add_argument("--resume", action="store_true", help="skip runs whose result.json is already valid")
    ap.add_argument("--report", action="store_true", help="print per-cond mean/median from finished runs and exit")
    args = ap.parse_args(argv)
    bench_root = Path(args.bench_root).resolve()

    if args.report:
        print(format_report(build_report(load_results(bench_root, args.runs_subdir))))
        return 0

    conds = [c.strip() for c in args.conds.split(",") if c.strip()]
    bad = [c for c in conds if c not in ORCH_CONDS]
    if bad:
        print(f"unknown conds {bad} (allowed: {ORCH_CONDS})", file=sys.stderr)
        return 2
    if args.zirv_dir:
        os.environ["PATH"] = str(Path(args.zirv_dir).resolve()) + os.pathsep + os.environ["PATH"]
        print("zirv under test:", shutil.which("zirv"))
    if "vanilla" in conds:
        err = H.check_vanilla_plugin_dir(args.vanilla_plugin_dir)
        if err:
            print(f"error: {err}", file=sys.stderr)
            return 2
    tasks_dir = bench_root / "tasks"
    if args.tasks:
        tasks = [t.strip() for t in args.tasks.split(",") if t.strip()]
    else:
        tasks = sorted(p.name for p in tasks_dir.iterdir()
                       if (p / "kind.txt").exists() and H.read_text(p / "kind.txt").strip() == ORCH_KIND)
    for t in tasks:
        if H.read_text(tasks_dir / t / "kind.txt").strip() != ORCH_KIND:
            print(f"{t} is not a kind=orch task", file=sys.stderr)
            return 2
    first = True
    for task in tasks:
        for rep in range(1, args.reps + 1):
            for cond in conds:
                if not first and args.stagger_s:
                    time.sleep(args.stagger_s)
                first = False
                do_one_run(bench_root, task, cond, rep, args)
    print(format_report(build_report(load_results(bench_root, args.runs_subdir))))
    return 0


if __name__ == "__main__":
    sys.exit(main())
