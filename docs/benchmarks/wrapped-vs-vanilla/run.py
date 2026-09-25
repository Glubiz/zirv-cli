#!/usr/bin/env python3
"""Benchmark runner: vanilla `claude -p` vs `zirv ctx exec` (with or without
the zirv proxy / Jev intake layer) wrapping claude.

Conditions:
  vanilla        -- claude -p directly, prompt on stdin.
  zirv           -- zirv ctx exec --agent claude --prompt <prompt> -- ...
  zirv-proxy     -- runs `zirv ctx proxy --json <prompt>` first (headless
                     `zirv ctx exec` skips the `zirv chat` Jev intake step),
                     maps the decided seat_tier to a claude model, optionally
                     starts a workflow, prepends a `[zirv proxy]` layer to the
                     prompt, then launches exactly like `zirv`.
  zirv-jev-full  -- identical to zirv-proxy (same proxy call, same launch),
                     with every `[jev]` advisory gate (issue #537's site
                     list: memory, supervisor, dispatch, review, gates,
                     context, intake_savings, review_reuse, harvest_screen,
                     admin_dispatch) turned on via `ZIRV_CTX_JEV_*` env vars
                     for the proxy call, the workflow start, and the exec
                     launch alike -- see `jev.rs`/`config.rs` for the gate
                     list. Isolates every advisory site's combined effect
                     beyond zirv-proxy's own model/seat/workflow routing.
  zirv-jev-<gate> -- same as zirv-jev-full but with only that one `[jev]`
                     gate on, e.g. zirv-jev-memory, zirv-jev-dispatch. Cheap
                     per-gate ablations for deciding which gates earn
                     default-on status (issue #758).

Every `zirv-jev-*` condition needs a Jev credential (`TYPESAFE_API_KEY` by
default) exported in the environment this script runs in, same as
zirv-proxy -- gate env vars alone never make an advisory site active,
`jev::available` also has to see the credential.

See CONTRACT.md in this directory for the full spec. stdlib only (3.11).
"""
import argparse
import concurrent.futures
import datetime
import glob
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import threading
import time
import traceback
from pathlib import Path

CLAUDE_EXE = r"C:\Users\josj\.local\bin\claude.exe"
ZIRV_FALLBACK = r"C:\ProgramData\chocolatey\bin\zirv.exe"
PYTHON_EXE = r"C:\Python311\python.exe"
GIT_EXE = r"C:\Program Files\Git\cmd\git.exe"
TASKKILL_EXE = r"C:\Windows\System32\taskkill.exe"
DEFAULT_TIMEOUT_MIN = 20

# Issue #758: every `[jev]` advisory gate (config.rs::JevConfig / jev.rs),
# named the same as the config key -- `ZIRV_CTX_JEV_<KEY.upper()>` is the
# env var mechanism config.rs's REPO_FORBIDDEN table already reserves for
# the operator to set these from outside a repo checkout (see config.rs
# around "ZIRV_CTX_JEV_MEMORY" etc.).
JEV_GATE_KEYS = [
    "memory", "supervisor", "dispatch", "review", "gates", "context",
    "intake_savings", "review_reuse", "harvest_screen", "admin_dispatch",
]
JEV_FULL_COND = "zirv-jev-full"
# zirv-nojev launches exactly like zirv but with Jev fully off: every gate
# forced false AND the Jev credential removed from the child environment, so
# `jev::available` is false at every site regardless of ~/.zirv/ctx.toml.
NOJEV_COND = "zirv-nojev"
JEV_CREDENTIAL_ENV = "TYPESAFE_API_KEY"
JEV_GATE_CONDS = [f"zirv-jev-{g}" for g in JEV_GATE_KEYS]
JEV_ABLATION_CONDS = [JEV_FULL_COND] + JEV_GATE_CONDS
# zirv-jev-full/zirv-jev-<gate> launch exactly like zirv-proxy: a
# `zirv ctx proxy --json` call first, then the same `zirv ctx exec` shape.
JEV_PROXY_LIKE_CONDS = {"zirv-proxy", *JEV_ABLATION_CONDS}

CANONICAL_CONDS = ["vanilla", "zirv", NOJEV_COND, "zirv-proxy", *JEV_ABLATION_CONDS]
JUDGE_DISALLOWED = "Write,Edit,Bash,NotebookEdit,Read,Glob,Grep,Agent,WebFetch,WebSearch"
# The operator's "smarter, not just more hidden tests passed" target: a
# second blind judge, on every tests-kind run, scoring things a hidden
# unittest suite structurally cannot -- see quality_rubric.md. A pricier
# model than the score-vs-score judge above on purpose: this one is read
# for every tests-kind run in the grid, not just judge-kind tasks.
QUALITY_JUDGE_MODEL = "opus"
QUALITY_DIFF_CAP_BYTES = 80_000
SEAT_MODEL_MAP = {"cheap": "haiku", "standard": "sonnet", "frontier": "opus"}
PROXY_CALL_TIMEOUT_S = 120
WORKFLOW_START_TIMEOUT_S = 60
PROXY_COST_PER_INPUT_TOKEN = 0.042 / 1_000_000

# Chain tasks (kind.txt == "chain"): a sequence of follow-up prompts sent to
# the SAME agent session (see "Long-session chain" in README.md). The
# template's one known baseline-red visible test -- same one every other
# task's grade.py tolerates -- is centralised here because chain grading
# happens directly in this file, not in a per-task grade.py.
BASELINE_VISIBLE_FAILURES = {"test_regex_rule_case_insensitive"}
UNITTEST_TIMEOUT_S = 180


def jev_env_var(gate_key):
    return "ZIRV_CTX_JEV_" + gate_key.upper()


def jev_gate_env_for(cond):
    """The `ZIRV_CTX_JEV_*` env additions a condition's subprocesses need:
    every gate for `zirv-jev-full`, one gate for `zirv-jev-<gate>`, nothing
    for every other condition (including plain `zirv-proxy`, which must stay
    byte-identical to today -- no gate on means every `[jev]`-gated site's
    deterministic path runs same as always, see jev.rs::advise_detailed).
    """
    if cond == JEV_FULL_COND:
        return {jev_env_var(g): "true" for g in JEV_GATE_KEYS}
    if cond == NOJEV_COND:
        # None = remove the variable from the child environment (child_env).
        return {**{jev_env_var(g): "false" for g in JEV_GATE_KEYS}, JEV_CREDENTIAL_ENV: None}
    if cond in JEV_GATE_CONDS:
        gate = cond[len("zirv-jev-"):]
        return {jev_env_var(gate): "true"}
    return {}

# The launching shell's PATH can be mangled (mixed `:`/`;` separators); give every
# child -- and therefore both conditions equally -- one clean Windows PATH.
_CLEAN_PATH = [
    r"C:\Users\josj\.local\bin", r"C:\ProgramData\chocolatey\bin",
    r"C:\Program Files\Git\cmd", r"C:\Program Files\Git\usr\bin",
    r"C:\Python311", r"C:\Python311\Scripts", r"C:\Program Files\nodejs",
    r"C:\Users\josj\.cargo\bin", r"C:\Windows\System32", r"C:\Windows",
    r"C:\Windows\System32\WindowsPowerShell\v1.0", r"C:\Program Files\PowerShell\7",
]
os.environ["PATH"] = ";".join(_CLEAN_PATH) + ";" + ";".join(
    p for p in os.environ.get("PATH", "").split(";") if p and ":" not in p[2:]
)


def child_env(env_extra):
    """The child environment for env_extra: None means inherit unchanged; a
    None value removes that variable."""
    if not env_extra:
        return None
    env = {**os.environ, **{k: v for k, v in env_extra.items() if v is not None}}
    for k, v in env_extra.items():
        if v is None:
            env.pop(k, None)
    return env


# --stagger-s: minimum gap between two agent launches, so parallel workers
# don't start together (API rate limits, local CPU congestion).
STAGGER_S = 0.0
_stagger_lock = threading.Lock()
_next_launch_at = 0.0


# A subscription usage limit ("You've hit your session limit · resets
# 4:50pm") ends a run in seconds with score 0. Such a run is retried after the
# reset instead of being recorded, and every worker pauses until then.
USAGE_LIMIT_MARKERS = ("hit your session limit", "hit your usage limit", "usage limit reached")
USAGE_LIMIT_RETRIES = 3
_pause_until = 0.0


def usage_limit_resume_at(run_dir):
    """Epoch seconds to resume at if this run hit the usage limit, else None:
    the stated reset time plus a minute, or 15 minutes if it can't be parsed."""
    for p in sorted(Path(run_dir).glob("stdout*.json")):
        try:
            text = p.read_text(encoding="utf-8", errors="ignore").lower()
        except OSError:
            continue
        if not any(m in text for m in USAGE_LIMIT_MARKERS):
            continue
        m = re.search(r"resets (\d{1,2})(?::(\d{2}))?\s*(am|pm)", text)
        if not m:
            return time.time() + 15 * 60
        hour = int(m.group(1)) % 12 + (12 if m.group(3) == "pm" else 0)
        now = time.localtime()
        reset = time.mktime((now.tm_year, now.tm_mon, now.tm_mday, hour,
                             int(m.group(2) or 0), 0, 0, 0, -1))
        if reset < time.time() - 60:
            reset += 24 * 3600
        return reset + 60
    return None


def wait_until_unpaused():
    while time.time() < _pause_until:
        time.sleep(min(60.0, _pause_until - time.time()))


def wait_for_launch_slot():
    global _next_launch_at
    wait_until_unpaused()
    with _stagger_lock:
        now = time.time()
        wait = max(0.0, _next_launch_at - now)
        _next_launch_at = max(now, _next_launch_at) + STAGGER_S
    if wait:
        time.sleep(wait)


def zirv_exe():
    found = shutil.which("zirv")
    return found if found else ZIRV_FALLBACK


def read_text(path):
    return Path(path).read_text(encoding="utf-8")


def parse_last_json(text):
    """Return the last top-level JSON value found in text, or None.

    Handles a stray leading non-JSON line (or several) by scanning forward
    and retrying, and handles multiple JSON objects concatenated by lines
    by keeping the last successfully parsed one.
    """
    if not text:
        return None
    dec = json.JSONDecoder()
    n = len(text)
    i = 0
    results = []
    while i < n:
        while i < n and text[i] in " \t\r\n":
            i += 1
        if i >= n:
            break
        try:
            obj, end = dec.raw_decode(text, i)
            results.append(obj)
            i = end
        except json.JSONDecodeError:
            i += 1
    return results[-1] if results else None


def build_run_list(tasks, conds, reps):
    ordered_conds = [c for c in CANONICAL_CONDS if c in conds]
    runs = []
    for rep in range(1, reps + 1):
        for task in tasks:
            for cond in ordered_conds:
                runs.append((task, cond, rep))
    return runs


RUNS_SUBDIR = "runs"
# Same notice for every condition: headless -p has nobody to answer an approval question.
NONINTERACTIVE = False
NONINTERACTIVE_NOTE = ("You are running non-interactively: nobody will answer questions or approve plans. "
                       "Make reasonable decisions yourself and complete the task end to end.\n\n")
VANILLA_PLUGIN_DIR = None


def run_dir_for(bench_root, task, cond, rep):
    return Path(bench_root) / RUNS_SUBDIR / f"{task}__{cond}__r{rep}"


def result_is_valid(run_dir):
    rj = run_dir / "result.json"
    if not rj.exists():
        return False
    try:
        obj = json.loads(rj.read_text(encoding="utf-8"))
    except Exception:
        return False
    return not obj.get("is_error", True)


def operator_plugin_settings():
    """The operator's user-level `enabledPlugins` as a `--settings` JSON, so
    vanilla (which drops the user layer to keep zirv's hooks out) still loads
    the same plugins the zirv conditions get from that layer. None if unset."""
    try:
        user = json.loads((Path.home() / ".claude" / "settings.json").read_text(encoding="utf-8"))
    except Exception:
        return None
    enabled = {k: True for k, v in (user.get("enabledPlugins") or {}).items() if v}
    return json.dumps({"enabledPlugins": enabled}, separators=(",", ":")) if enabled else None


def build_argv(cond, model, prompt_text, resume_session_id=None):
    """`resume_session_id`: None for a fresh launch (default -- byte-identical
    to the pre-chain argv for every existing caller). Set for step 2+ of a
    chain run: appends claude's own verified `--resume <id>` (see
    `exec.rs::RESUME_FLAGS_WITH_VALUE`/`adapter_has_resume_flags`) so the
    SAME conversation continues -- for vanilla directly on `claude -p`'s own
    argv, for zirv/zirv-proxy/zirv-jev-* inside the `-- ...` agent command
    zirv passes straight through to claude (see build_run_list's docstring
    in README's "Long-session chain" section for why this is the mechanism
    picked for both conditions alike)."""
    if cond == "vanilla":
        if VANILLA_PLUGIN_DIR:
            # Vanilla + a plugin (e.g. superpowers): drop the user settings layer
            # (where the operator's global zirv hooks live) instead of disabling
            # all hooks, so the plugin's own SessionStart hook still runs. The
            # user layer's bypassPermissions default is restated explicitly.
            argv = [CLAUDE_EXE, "-p", "--output-format", "json", "--model", model,
                    "--setting-sources", "project,local",
                    "--permission-mode", "bypassPermissions",
                    "--plugin-dir", VANILLA_PLUGIN_DIR]
            plugins = operator_plugin_settings()
            if plugins:
                argv += ["--settings", plugins]
        else:
            settings = json.dumps({"disableAllHooks": True}, separators=(",", ":"))
            argv = [CLAUDE_EXE, "-p", "--output-format", "json", "--model", model,
                    "--settings", settings]
        if resume_session_id:
            argv += ["--resume", resume_session_id]
        return argv
    elif cond in ("zirv", NOJEV_COND) or cond in JEV_PROXY_LIKE_CONDS:
        # zirv-proxy and every zirv-jev-* condition launch exactly like zirv:
        # same shape, different (proxy-decided) model and a prompt with the
        # proxy layer prepended. Which `[jev]` gates are on is carried by the
        # subprocess environment (see jev_gate_env_for), never argv.
        #
        # Fairness (probe, 2026-09-24): zirv runs keep the operator's user
        # settings layer, because that is where `zirv setup` installs zirv's
        # own Stop/UserPromptSubmit/PreCompact/safety hooks -- dropping it
        # (`--setting-sources project,local`) silently ran zirv without most
        # of its hooks. The user layer's unrelated plugins are instead given
        # to vanilla too (operator_plugin_settings), so both sides carry the
        # same plugin set.
        if resume_session_id:
            # Chain step 2+ -- two shapes were tried live and rejected before
            # this one (2026-09-24 probe, see README "Long-session chain"):
            #
            # 1. `--prompt <text> -- ... --resume <old-id>`: `zirv ctx exec`'s
            #    own `headless_cmd` (used whenever `--prompt` makes zirv build
            #    the launch itself) ALWAYS mints and injects a FRESH claude
            #    `--session-id` ahead of the trailing args, for its own
            #    transcript tracking (adapters/claude.rs::ClaudeAdapter::
            #    headless_cmd) -- unconditionally, not only on a restart. The
            #    result: claude sees a fresh `--session-id` and a `--resume`
            #    naming a DIFFERENT id and silently starts a brand-new,
            #    unrelated conversation -- no error, `parentUuid: null` on the
            #    first turn, session_id in the JSON result not matching the
            #    one asked to resume.
            # 2. Also passing zirv's own top-level `--session-id <old-id>` (so
            #    both ids agree): claude itself then hard-errors ("Session ID
            #    <id> is already in use") -- `--session-id` names a FRESH id
            #    to mint, not an existing one to reopen; it is never a resume
            #    mechanism.
            #
            # What actually works: skip `--prompt` (and zirv's own
            # `--session-id`) entirely and hand `zirv ctx exec` the FULL
            # claude invocation after `--`, exactly as an operator would type
            # `claude -p ... --resume <id> ...` by hand. With a real program
            # name (not a bare flag) leading the `-- ...` command, zirv passes
            # it straight through (`adapter_builds_launch` is false) instead
            # of building its own `headless_cmd` -- so claude's own
            # `--session-id`/`--resume` are never double-set. Verified live:
            # `session_id` in the JSON result matches the resumed id exactly,
            # `num_turns` reflects the accumulated conversation, and zirv's
            # own supervision lines ("sandbox posture", "system prompt
            # composed", the safety-hook `--settings <launch-settings>` file)
            # still appear on stderr -- supervision stays attached even
            # though zirv isn't the one building the launch.
            return [zirv_exe(), "ctx", "exec", "--agent", "claude",
                    "--", CLAUDE_EXE, "-p", prompt_text, "--resume", resume_session_id,
                    "--output-format", "json", "--model", model]
        return [zirv_exe(), "ctx", "exec", "--agent", "claude", "--prompt", prompt_text,
                "--", "--output-format", "json", "--model", model]
    else:
        raise ValueError(f"unknown cond: {cond}")


def _rmtree_onerror(func, path, exc_info):
    """shutil.rmtree onerror handler: git's .git/objects/* are read-only on
    Windows, so a plain rmtree of a copied repo fails with PermissionError.
    Clear the read-only bit and retry once."""
    try:
        os.chmod(path, stat.S_IWRITE)
        func(path)
    except Exception:
        pass


def rmtree_robust(path):
    shutil.rmtree(path, onerror=_rmtree_onerror)
    if Path(path).exists():
        # a file is still locked (e.g. an orphan from an interrupted run): move it aside
        os.replace(path, f"{path}.stale-{int(time.time())}")


def kill_tree(pid):
    subprocess.run([TASKKILL_EXE, "/T", "/F", "/PID", str(pid)],
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def launch(cond, model, prompt_text, prompt_path, cwd, stdout_path, stderr_path, env_extra=None,
           resume_session_id=None):
    argv = build_argv(cond, model, prompt_text, resume_session_id=resume_session_id)
    # env_extra carries the ZIRV_CTX_JEV_* gate vars for a zirv-jev-* run
    # (jev_gate_env_for); {} for every other condition, so the child
    # inherits this process's own environment unchanged, same as before
    # issue #758. Built per-call, never via os.environ mutation: do_one_run
    # runs inside a thread pool with interleaved conditions, and mutating
    # the shared process environment would race across threads.
    env = child_env(env_extra)
    stdout_f = open(stdout_path, "wb")
    stderr_f = open(stderr_path, "wb")
    stdin_f = None
    try:
        if cond == "vanilla":
            stdin_f = open(prompt_path, "rb")
            proc = subprocess.Popen(argv, cwd=str(cwd), stdin=stdin_f,
                                     stdout=stdout_f, stderr=stderr_f, env=env)
        else:
            proc = subprocess.Popen(argv, cwd=str(cwd), stdin=subprocess.DEVNULL,
                                     stdout=stdout_f, stderr=stderr_f, env=env)
    except Exception:
        stdout_f.close()
        stderr_f.close()
        if stdin_f:
            stdin_f.close()
        raise
    return proc, argv, stdout_f, stderr_f, stdin_f


def classify_zirv_cmd(cmd, zirv_cmds):
    if cmd.startswith("zirv workflow"):
        zirv_cmds["workflow"] += 1
    elif cmd.startswith("zirv skill"):
        zirv_cmds["skill"] += 1
    elif cmd.startswith("zirv agent"):
        zirv_cmds["agent"] += 1
    elif cmd.startswith("zirv ctx"):
        zirv_cmds["ctx"] += 1
    elif cmd.startswith("zirv "):
        zirv_cmds["other"] += 1


def scan_transcript(session_id):
    zirv_cmds = {"workflow": 0, "skill": 0, "agent": 0, "ctx": 0, "other": 0}
    tool_calls = 0
    if not session_id:
        return 0, zirv_cmds, "no session_id; transcript not scanned"
    home = Path(os.path.expanduser("~"))
    pattern = str(home / ".claude" / "projects" / "*" / f"{session_id}.jsonl")
    matches = glob.glob(pattern)
    if not matches:
        return 0, zirv_cmds, "transcript not found for session"
    path = matches[0]
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    ev = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if not isinstance(ev, dict):
                    continue
                message = ev.get("message")
                content = message.get("content") if isinstance(message, dict) else None
                if not isinstance(content, list):
                    continue
                for item in content:
                    if not isinstance(item, dict):
                        continue
                    if item.get("type") == "tool_use":
                        tool_calls += 1
                        if item.get("name") == "Bash":
                            cmd = ((item.get("input") or {}).get("command") or "").strip()
                            classify_zirv_cmd(cmd, zirv_cmds)
    except Exception as e:
        return tool_calls, zirv_cmds, f"transcript scan error: {e}"
    return tool_calls, zirv_cmds, None


# Fallback price-per-weighted-token (USD) used to cost an abandoned
# conversation's partial spend on a chain session switch (`do_one_chain_run`)
# when no earlier non-switch step in the same run exists to calibrate a
# self-estimated price from. Chosen as a rough Sonnet-class order of
# magnitude; only ever used as a last resort, and always noted when it is.
FALLBACK_PRICE_PER_WEIGHTED_TOKEN = 3e-6


def project_slug(cwd):
    """Mirror `project_slug` in zirv's `adapters/claude.rs`: Claude Code
    stores transcripts under a slug of the cwd with every character outside
    `[A-Za-z0-9-]` replaced by `-`. `os.path.normpath` first so separator
    style/trailing-slash differences don't change the slug."""
    normalized = os.path.normpath(str(cwd))
    return re.sub(r"[^A-Za-z0-9-]", "-", normalized)


def find_session_transcript(repo_dir, session_id):
    """The `.jsonl` transcript for `session_id` under the project slug for
    `repo_dir`, falling back to a `scan_transcript`-style glob across every
    project dir if the exact slug doesn't match (e.g. a differently-cased or
    already-moved cwd)."""
    home = Path(os.path.expanduser("~"))
    candidate = home / ".claude" / "projects" / project_slug(repo_dir) / f"{session_id}.jsonl"
    if candidate.exists():
        return candidate
    matches = glob.glob(str(home / ".claude" / "projects" / "*" / f"{session_id}.jsonl"))
    return Path(matches[0]) if matches else None


def iso_utc_ms(epoch_s):
    """`time.time()`-style epoch seconds as an ISO-8601 UTC string matching
    Claude Code transcript `timestamp` fields (millisecond precision, `Z`
    suffix), so the two can be compared lexicographically."""
    dt = datetime.datetime.fromtimestamp(epoch_s, tz=datetime.timezone.utc)
    return dt.strftime("%Y-%m-%dT%H:%M:%S.") + f"{dt.microsecond // 1000:03d}Z"


def weighted_tokens(usage):
    """The self-calibrated-price weighting from CONTRACT's chain session-
    switch cost fix: input + 0.1*cache_read + 1.25*cache_write_5m +
    2.0*cache_write_1h + 5.0*output. Applies equally to a step's own
    `usage` (from `--output-format json`) and to a transcript assistant
    entry's `message.usage` -- same shape either way. `cache_creation`'s
    ephemeral 1h/5m split is used when present; otherwise every
    `cache_creation_input_tokens` is treated as a 5m write."""
    usage = usage or {}
    input_tokens = usage.get("input_tokens", 0) or 0
    cache_read = usage.get("cache_read_input_tokens", 0) or 0
    output_tokens = usage.get("output_tokens", 0) or 0
    cache_creation = usage.get("cache_creation") or {}
    h1 = cache_creation.get("ephemeral_1h_input_tokens")
    m5 = cache_creation.get("ephemeral_5m_input_tokens")
    if h1 is not None or m5 is not None:
        cache_write_1h = h1 or 0
        cache_write_5m = m5 or 0
    else:
        cache_write_5m = usage.get("cache_creation_input_tokens", 0) or 0
        cache_write_1h = 0
    return (input_tokens + 0.1 * cache_read + 1.25 * cache_write_5m +
            2.0 * cache_write_1h + 5.0 * output_tokens)


def transcript_weighted_tokens_since(transcript_path, start_iso, end_iso=None):
    """Sum of `weighted_tokens` over assistant entries in `transcript_path`
    with `timestamp` in `[start_iso, end_iso)` (`end_iso=None` = unbounded
    above), de-duplicated by `message.id` (a streamed transcript can repeat
    the same message id verbatim across lines). Returns 0.0 when the
    transcript is missing or unreadable -- this only ever contributes an
    *estimate*, so a read failure degrades to "no partial spend found"
    rather than raising.
    """
    if transcript_path is None:
        return 0.0
    transcript_path = Path(transcript_path)
    if not transcript_path.exists():
        return 0.0
    seen_ids = set()
    total = 0.0
    try:
        with open(transcript_path, "r", encoding="utf-8", errors="replace") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    ev = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if not isinstance(ev, dict) or ev.get("type") != "assistant":
                    continue
                ts = ev.get("timestamp")
                if not ts or ts < start_iso:
                    continue
                if end_iso is not None and ts >= end_iso:
                    continue
                message = ev.get("message")
                if not isinstance(message, dict):
                    continue
                msg_id = message.get("id")
                usage = message.get("usage")
                if not msg_id or not usage or msg_id in seen_ids:
                    continue
                seen_ids.add(msg_id)
                total += weighted_tokens(usage)
    except Exception:
        return total
    return total


def get_capped_diff(repo_dir, cap_bytes=60_000, exclude=None):
    """`git diff` of `repo_dir` against its root commit, capped to `cap_bytes`.

    `git add -N .` first so an agent's brand-new (still untracked) files show
    up as additions instead of being invisible to the diff -- needed for
    both the existing judge-kind rubric and the quality judge below.
    `exclude` (a list of pathspecs, e.g. `["tests_hidden"]`) is appended as
    `:!<path>` exclusions -- the quality judge always excludes
    `tests_hidden/`, the grader's own copied hidden test files, which are
    NOT part of the agent's work and may already be sitting untracked in
    `repo_dir` (a live run's tests-kind grading happens after this is
    normally called, but `regrade.py --rejudge-quality` re-diffs a repo a
    tests-kind grader already ran in, so this exclusion is not optional
    there). `exclude=None` (the judge-kind rubric's default) reproduces the
    exact argv this function always used before `exclude` existed.
    """
    subprocess.run([GIT_EXE, "add", "-N", "."], cwd=str(repo_dir), capture_output=True)
    # Diff against the template's root commit, not HEAD: an agent that commits
    # its own work must not make its change invisible to the judge.
    root = subprocess.run([GIT_EXE, "rev-list", "--max-parents=0", "HEAD"], cwd=str(repo_dir),
                          capture_output=True, text=True).stdout.split()
    base = root[-1] if root else "HEAD"
    argv = [GIT_EXE, "diff", base]
    if exclude:
        argv += ["--", "."] + [f":!{p}" for p in exclude]
    proc = subprocess.run(argv, cwd=str(repo_dir), capture_output=True)
    data = proc.stdout
    if len(data) > cap_bytes:
        data = data[:cap_bytes]
    return data.decode("utf-8", errors="replace")


def parse_judge_json(text):
    text = (text or "").strip()
    if text.startswith("```"):
        lines = text.split("\n")
        if lines and lines[0].startswith("```"):
            lines = lines[1:]
        if lines and lines[-1].strip().startswith("```"):
            lines = lines[:-1]
        text = "\n".join(lines).strip()
    try:
        return json.loads(text)
    except Exception:
        return parse_last_json(text)


def call_judge(prompt_text, model="sonnet"):
    """One judge call, retried once when the reply has no usable score: a
    transient bad reply must not be recorded as a 0 against any condition."""
    judge_obj, text = _call_judge_once(prompt_text, model)
    if not _has_numeric_score(judge_obj):
        judge_obj, text = _call_judge_once(prompt_text, model)
    return judge_obj, text


def _has_numeric_score(judge_obj):
    try:
        float(judge_obj.get("score"))
        return True
    except Exception:
        return False


def _call_judge_once(prompt_text, model):
    argv = [CLAUDE_EXE, "-p", "--output-format", "json", "--model", model,
            "--settings", json.dumps({"disableAllHooks": True}, separators=(",", ":")),
            "--max-turns", "1",
            f"--disallowedTools={JUDGE_DISALLOWED}"]
    try:
        proc = subprocess.run(argv, input=prompt_text.encode("utf-8"),
                               capture_output=True, timeout=300)
    except Exception:
        return None, None
    stdout_text = proc.stdout.decode("utf-8", errors="replace")
    obj = parse_last_json(stdout_text)
    if obj is None:
        return None, stdout_text
    result_text = obj.get("result", "")
    judge_obj = parse_judge_json(result_text)
    return judge_obj, result_text


_QUALITY_RUBRIC_PATH = Path(__file__).resolve().parent / "quality_rubric.md"


def call_quality_judge(prompt_text, repo_dir, result_text):
    """The blind work-quality judge, run for every kind=tests task.

    Same blind-judge machinery as `call_judge` (disallowed tools, condition
    never named) but: `quality_rubric.md` instead of a per-task rubric.md,
    `QUALITY_JUDGE_MODEL` ("opus") instead of "sonnet", and a diff that
    excludes `tests_hidden/` (the grader's own copied files, never the
    agent's work -- see `get_capped_diff`'s doc comment). Returns
    (quality_score 0..1 or None, quality_reasoning str).
    """
    rubric = read_text(_QUALITY_RUBRIC_PATH) if _QUALITY_RUBRIC_PATH.exists() else ""
    diff_text = get_capped_diff(repo_dir, cap_bytes=QUALITY_DIFF_CAP_BYTES, exclude=["tests_hidden"])
    judge_prompt = (
        rubric + "\n\n## Task prompt\n" + prompt_text +
        "\n\n## Diff (git diff HEAD, may be truncated, excludes tests_hidden/)\n" + diff_text +
        "\n\n## Agent's final message\n" + (result_text or "") +
        "\n\n## Your answer\nScore the change against the rubric. Reply with ONLY a JSON object "
        "on one line: {\"score\": <integer 0-10>, \"reasoning\": \"<one or two sentences>\"}. "
        "No prose before or after it, no code fence."
    )
    judge_obj, raw_judge_text = call_judge(judge_prompt, model=QUALITY_JUDGE_MODEL)
    if judge_obj is None:
        return None, raw_judge_text or "quality judge call failed / no parsable output"
    try:
        score = float(judge_obj.get("score", 0))
    except Exception:
        return None, "quality judge returned an unparsable score"
    return score / 10.0, judge_obj.get("reasoning", "")


def call_quality_judge_chain(step_prompts, repo_dir, step_texts):
    """The end-of-chain work-quality judge (spec item 2): same machinery as
    `call_quality_judge`, but over the WHOLE chain -- every step's prompt
    shown, every step's final response concatenated, one diff of the
    repo's final state against the pristine template. Runs once, after the
    last step, never per step."""
    rubric = read_text(_QUALITY_RUBRIC_PATH) if _QUALITY_RUBRIC_PATH.exists() else ""
    diff_text = get_capped_diff(repo_dir, cap_bytes=QUALITY_DIFF_CAP_BYTES, exclude=["tests_hidden"])
    prompts_block = "\n\n".join(f"### Step {i+1}\n{p}" for i, p in enumerate(step_prompts))
    responses_block = "\n\n".join(f"### Step {i+1} response\n{t}" for i, t in enumerate(step_texts))
    judge_prompt = (
        rubric + "\n\n## Task prompts (one long session, sent one after another)\n" + prompts_block +
        "\n\n## Diff (git diff HEAD, final state, may be truncated, excludes tests_hidden/)\n" + diff_text +
        "\n\n## Agent's final messages, one per step\n" + responses_block +
        "\n\n## Your answer\nScore the whole session's work against the rubric. Reply with ONLY a JSON "
        "object on one line: {\"score\": <integer 0-10>, \"reasoning\": \"<one or two sentences>\"}. "
        "No prose before or after it, no code fence."
    )
    judge_obj, raw_judge_text = call_judge(judge_prompt, model=QUALITY_JUDGE_MODEL)
    if judge_obj is None:
        return None, raw_judge_text or "quality judge call failed / no parsable output"
    try:
        score = float(judge_obj.get("score", 0))
    except Exception:
        return None, "quality judge returned an unparsable score"
    return score / 10.0, judge_obj.get("reasoning", "")


def run_unittest_discover(repo_dir, start_dir, timeout_s=UNITTEST_TIMEOUT_S):
    """Mirror t22_envelopes/grade.py's `_run_unittest`: run
    `python -m unittest discover -s <start_dir> -t <repo_dir>` and parse the
    passed/total/failure-name summary out of its output. Shared by every
    chain step's grading (chain tasks have no per-task grade.py of their own
    -- the step boundary IS the grading boundary, so grading lives here)."""
    try:
        proc = subprocess.run(
            [PYTHON_EXE, "-m", "unittest", "discover", "-s", start_dir, "-t", str(repo_dir)],
            cwd=str(repo_dir), capture_output=True, text=True, timeout=timeout_s,
        )
    except Exception as exc:
        return 0, 0, [], f"failed to run {start_dir}: {exc!r}"
    output = (proc.stdout or "") + "\n" + (proc.stderr or "")
    m = re.search(r"Ran (\d+) tests?", output)
    total = int(m.group(1)) if m else 0
    fail_names = re.findall(r"^(?:FAIL|ERROR): .*?\(([\w.]+)\)", output, re.MULTILINE)
    passed = max(total - len(fail_names), 0)
    return passed, total, fail_names, output


def read_chain_steps(task_dir):
    """A chain task's ordered steps: `prompts/01.txt, 02.txt, ...`. Each
    step is graded `tests` (a `hidden/step_<NN>/*.py` dir exists), `judge`
    (no hidden dir but a `rubric/step_<NN>.md` exists -- pure doc/summary
    steps per spec item 3), or `none` (neither -- excluded from the
    per-step score mean, still timed/costed and fed to the final quality
    judge)."""
    prompts_dir = task_dir / "prompts"
    steps = []
    for p in sorted(prompts_dir.glob("*.txt")):
        label = p.stem
        hidden_dir = task_dir / "hidden" / f"step_{label}"
        rubric_path = task_dir / "rubric" / f"step_{label}.md"
        if hidden_dir.is_dir():
            kind = "tests"
        elif rubric_path.exists():
            kind = "judge"
        else:
            kind = "none"
        steps.append({
            "label": label, "prompt_path": p,
            "hidden_dir": hidden_dir if kind == "tests" else None,
            "rubric_path": rubric_path if kind == "judge" else None,
            "kind": kind,
        })
    return steps


def grade_step_tests(repo_dir, hidden_dir):
    """One chain step's `tests`-kind grading: copy ONLY this step's hidden
    tests in, run them plus the visible suite, then remove tests_hidden/
    again before the next prompt is sent -- the agent must never see a
    hidden test, including one from a step it already finished (spec item
    1: "don't let the agent see hidden tests")."""
    tests_hidden = repo_dir / "tests_hidden"
    if tests_hidden.exists():
        rmtree_robust(tests_hidden)
    tests_hidden.mkdir(parents=True, exist_ok=True)
    (tests_hidden / "__init__.py").touch()
    for f in sorted(hidden_dir.glob("*.py")):
        (tests_hidden / f.name).write_text(read_text(f), encoding="utf-8")
    try:
        h_passed, h_total, _h_fail, _h_out = run_unittest_discover(repo_dir, "tests_hidden")
        _v_passed, v_total, v_fail_names, _v_out = run_unittest_discover(repo_dir, "tests")
    finally:
        rmtree_robust(tests_hidden)
    v_fail_short = {n.rsplit(".", 1)[-1] for n in v_fail_names}
    visible_ok = v_fail_short.issubset(BASELINE_VISIBLE_FAILURES)
    score = (h_passed / h_total) if h_total else 0.0
    return {
        "score": round(score, 4), "passed": h_passed, "total": h_total, "visible_ok": visible_ok,
        "details": f"hidden {h_passed}/{h_total} passed; visible total={v_total}, "
                   f"failures beyond baseline={sorted(v_fail_short - BASELINE_VISIBLE_FAILURES)}",
    }


def grade_step_judge(rubric_path, step_prompt_text, repo_dir, step_result_text):
    """One chain step's `judge`-kind grading (pure doc/summary steps): the
    same blind-judge machinery as a top-level kind=judge task, scoped to
    this one step's prompt/response, diffed against the pristine template
    so far (cumulative -- a doc step is judged on the whole session's state,
    not just its own turn)."""
    rubric = read_text(rubric_path) if rubric_path.exists() else ""
    diff_text = get_capped_diff(repo_dir, cap_bytes=60_000, exclude=["tests_hidden"])
    judge_prompt = (
        rubric + "\n\n## Step prompt\n" + step_prompt_text +
        "\n\n## Diff so far (git diff against the pristine template, may be truncated)\n" + diff_text +
        "\n\n## Agent's response for this step\n" + (step_result_text or "") +
        "\n\n## Your answer\nScore this step against the rubric. Reply with ONLY a JSON object "
        "on one line: {\"score\": <integer 0-10>, \"reasoning\": \"<one or two sentences>\"}. "
        "No prose before or after it, no code fence."
    )
    judge_obj, raw_judge_text = call_judge(judge_prompt, model="sonnet")
    if judge_obj is None:
        return {"score": 0.0, "passed": None, "total": None, "visible_ok": True,
                "details": raw_judge_text or "judge call failed / no parsable output"}
    try:
        js = float(judge_obj.get("score", 0))
    except Exception:
        return {"score": 0.0, "passed": None, "total": None, "visible_ok": True,
                "details": "judge returned an unparsable score"}
    return {"score": js / 10.0, "passed": None, "total": None, "visible_ok": True,
            "details": judge_obj.get("reasoning", "")}


def run_grade_py(grade_py, repo_dir, result_txt_path):
    proc = subprocess.run(
        [PYTHON_EXE, str(grade_py), str(repo_dir), str(result_txt_path)],
        capture_output=True, text=True,
    )
    obj = parse_last_json(proc.stdout)
    return obj, proc


def grade(task_dir, kind, repo_dir, result_txt_path, prompt_text):
    out = {"score": 0.0, "passed": 0, "total": 0, "visible_ok": False, "details": "",
           "judge_score": None, "judge_reasoning": None}
    grade_py = task_dir / "grade.py"
    if kind in ("tests", "answer"):
        if not grade_py.exists():
            out["details"] = "grade.py missing"
            return out
        obj, proc = run_grade_py(grade_py, repo_dir, result_txt_path)
        if obj is None:
            out["details"] = "grade.py produced no parsable JSON"
            return out
        for k in ("score", "passed", "total", "visible_ok", "details"):
            if k in obj:
                out[k] = obj[k]
        return out
    elif kind == "judge":
        rubric = read_text(task_dir / "rubric.md") if (task_dir / "rubric.md").exists() else ""
        diff_text = get_capped_diff(repo_dir)
        try:
            result_text = Path(result_txt_path).read_text(encoding="utf-8")
        except Exception:
            result_text = ""
        judge_prompt = (
            rubric + "\n\n## Task prompt\n" + prompt_text +
            "\n\n## Diff (git diff HEAD, may be truncated)\n" + diff_text +
            "\n\n## Agent's final message\n" + result_text +
            "\n\n## Your answer\nScore the change against the rubric. Reply with ONLY a JSON object "
            "on one line: {\"score\": <integer 0-10>, \"reasoning\": \"<one or two sentences>\"}. "
            "No prose before or after it, no code fence."
        )
        judge_obj, raw_judge_text = call_judge(judge_prompt)
        if judge_obj is None:
            out["judge_score"] = 0
            out["judge_reasoning"] = raw_judge_text or "judge call failed / no parsable output"
            out["score"] = 0.0
        else:
            try:
                js = float(judge_obj.get("score", 0))
            except Exception:
                js = 0.0
            out["judge_score"] = js
            out["judge_reasoning"] = judge_obj.get("reasoning", "")
            out["score"] = js / 10.0
        if grade_py.exists():
            gobj, _ = run_grade_py(grade_py, repo_dir, result_txt_path)
            if gobj is not None:
                out["visible_ok"] = gobj.get("visible_ok", False)
                if "passed" in gobj:
                    out["passed"] = gobj["passed"]
                if "total" in gobj:
                    out["total"] = gobj["total"]
        return out
    else:
        out["details"] = f"unknown kind: {kind}"
        return out


def default_proxy_meta():
    return {
        "complexity": None, "risk": None, "execution": None, "seat_role": None,
        "seat_tier": None, "worker_tier": None, "workflow": None, "workflow_id": None, "domains": [],
        "decider": None, "elapsed_ms": None, "input_tokens": 0, "output_tokens": 0,
        "cost_usd": 0.0, "wall_s": 0.0,
    }


def call_proxy(repo_dir, prompt_text, timeout_s, env_extra=None):
    """Run `zirv ctx proxy --json <prompt>`. Returns (obj, elapsed_s, error_note, raw_stdout)."""
    argv = [zirv_exe(), "ctx", "proxy", "--json", prompt_text]
    env = child_env(env_extra)
    t0 = time.time()
    try:
        proc = subprocess.run(argv, cwd=str(repo_dir), capture_output=True, timeout=timeout_s, env=env)
    except subprocess.TimeoutExpired:
        return None, time.time() - t0, "proxy call timed out", ""
    except Exception as e:
        return None, time.time() - t0, f"proxy call failed: {e}", ""
    elapsed = time.time() - t0
    stdout_text = proc.stdout.decode("utf-8", errors="replace")
    obj = parse_last_json(stdout_text)
    if obj is None:
        stderr_text = proc.stderr.decode("utf-8", errors="replace")
        return None, elapsed, f"proxy produced no parsable JSON (exit={proc.returncode})", stdout_text + stderr_text
    return obj, elapsed, None, stdout_text


def start_workflow(repo_dir, workflow, prompt_text, complexity, risk, env_extra=None):
    """Run `zirv workflow start <workflow> --json ...`.

    Returns (started, note, elapsed_s, workflow_id). `--json` (see
    `write_start_outcome`/`WorkflowState` in engine.rs) prints the persisted
    `WorkflowState` -- possibly preceded by a plain `warning: ...` line, so
    the JSON object is found with the same `parse_last_json` scan used
    elsewhere in this file rather than assumed to be the whole of stdout --
    which carries the `id` field `build_proxy_layer` needs to match
    `proxy::mod::prompt_layer`'s `(started <id>)` text exactly.
    """
    argv = [zirv_exe(), "workflow", "start", workflow, "--task", prompt_text,
            "--repo", str(repo_dir), "--complexity", str(complexity), "--risk", str(risk),
            "--json"]
    env = child_env(env_extra)
    t0 = time.time()
    try:
        proc = subprocess.run(argv, cwd=str(repo_dir), capture_output=True,
                               timeout=WORKFLOW_START_TIMEOUT_S, env=env)
    except subprocess.TimeoutExpired:
        return False, "workflow start timed out", time.time() - t0, None
    except Exception as e:
        return False, f"workflow start failed: {e}", time.time() - t0, None
    elapsed = time.time() - t0
    stdout_text = proc.stdout.decode("utf-8", errors="replace").strip()
    stderr_text = proc.stderr.decode("utf-8", errors="replace").strip()
    started = proc.returncode == 0
    workflow_id = None
    if started:
        obj = parse_last_json(stdout_text)
        if isinstance(obj, dict):
            workflow_id = obj.get("id")
    first_line = ""
    if stdout_text:
        first_line = stdout_text.splitlines()[0]
    elif stderr_text:
        first_line = stderr_text.splitlines()[0]
    note = f"exit={proc.returncode}: {first_line}" if not started else f"exit=0: started {workflow_id or '(no id)'}"
    return started, note, elapsed, workflow_id


def build_proxy_layer(proxy_obj, model, started_workflow_id=None):
    """Mirror `proxy::mod::prompt_layer` (src/commands/ctx/proxy/mod.rs,
    ~438-468) byte-for-byte on the lines this harness can reconstruct from
    `zirv ctx proxy --json`'s decision object plus `start_workflow`'s own
    result: header, execution/complexity/risk, seat(s), workflow (see
    below), domains, and the single-seat instruction line.

    `started_workflow_id`: the id `start_workflow` actually started for
    THIS launch (`None` when no workflow was named, the start was skipped,
    or it failed) -- when both a workflow kind and a started id are known,
    the line names the concrete instance and tells the seat to consult it,
    exactly matching `prompt_layer`'s `(Some(kind), Some(id))` arm.
    """
    execution = proxy_obj.get("execution")
    complexity = proxy_obj.get("complexity")
    risk = proxy_obj.get("risk")
    seat_role = proxy_obj.get("seat_role")
    seat_tier = proxy_obj.get("seat_tier")
    worker_tier = proxy_obj.get("worker_tier")
    workflow = proxy_obj.get("workflow")
    domains = proxy_obj.get("domains") or []

    lines = ["[zirv proxy]", f"execution: {execution} (complexity {complexity}, risk {risk})"]
    if seat_role == "single":
        lines.append(f"seat: claude/{model} ({seat_tier})")
    else:
        lines.append(f"seats: orchestrator claude/{model} ({seat_tier}) \u00b7 workers {worker_tier}")
    if workflow and started_workflow_id:
        lines.append(
            f"workflow: {workflow} (started {started_workflow_id}) -- run `zirv workflow status` "
            "and follow its current step"
        )
    elif workflow:
        lines.append(f"workflow: {workflow}")
    else:
        lines.append("workflow: none")
    if domains:
        lines.append("domains: " + ", ".join(str(d) for d in domains))
    if seat_role == "single":
        lines.append("You are the single seat for this request: do the work here yourself; do not delegate.")
    return "\n".join(lines)


def write_result(run_dir, result):
    (run_dir / "result.json").write_text(json.dumps(result, indent=2), encoding="utf-8")


def finish_line(k, total, task, cond, rep, result):
    cost = result.get("total_cost_usd") or 0.0
    score = result.get("score") or 0.0
    wall = result.get("wall_s") or 0.0
    return f"[{k}/{total}] {task} {cond} r{rep} -> score {score:.2f} cost ${cost:.2f} wall {wall:.0f}s"


def do_one_run(bench_root, task, cond, rep, model, timeout_s, resume, k, total):
    bench_root = Path(bench_root)
    task_dir = bench_root / "tasks" / task
    template_dir = bench_root / "template"
    run_dir = run_dir_for(bench_root, task, cond, rep)
    prompt_path = task_dir / "prompt.txt"
    prompt_text = read_text(prompt_path)
    kind = read_text(task_dir / "kind.txt").strip()

    if resume and result_is_valid(run_dir):
        print(f"[{k}/{total}] {task} {cond} r{rep} -> skip (resume, already ok)")
        return None

    print(f"[{k}/{total}] {task} {cond} r{rep} -> starting")

    if run_dir.exists():
        rmtree_robust(run_dir)
    run_dir.mkdir(parents=True, exist_ok=True)
    repo_dir = run_dir / "repo"
    dirty = subprocess.run([GIT_EXE, "-C", str(template_dir), "status", "--porcelain"],
                           capture_output=True, text=True).stdout.strip()
    if dirty:
        raise RuntimeError(f"template is not pristine, refusing to copy:\n{dirty}")
    shutil.copytree(template_dir, repo_dir)

    if NONINTERACTIVE:
        prompt_text = NONINTERACTIVE_NOTE + prompt_text
        prompt_path = run_dir / "prompt.txt"
        prompt_path.write_text(prompt_text, encoding="utf-8")

    stdout_path = run_dir / "stdout.json"
    stderr_path = run_dir / "stderr.txt"

    result = {
        "task": task, "cond": cond, "rep": rep, "model": model,
        "wall_s": None, "duration_ms": None, "duration_api_ms": None, "num_turns": None,
        "total_cost_usd": None, "agent_cost_usd": None,
        "input_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
        "output_tokens": 0,
        "subagents_spawned": 0, "permission_denials": 0, "is_error": False, "exit_code": None,
        "zirv_cmds": {"workflow": 0, "skill": 0, "agent": 0, "ctx": 0, "other": 0},
        "tool_calls": 0,
        "score": 0.0, "passed": 0, "total": 0, "visible_ok": False, "details": "",
        "judge_score": None, "judge_reasoning": None,
        "quality_score": None, "quality_reasoning": None,
        "proxy": default_proxy_meta(), "workflow_started": False, "workflow_note": None,
        "model_used": model,
    }

    wait_for_launch_slot()
    start = time.time()
    model_used = model
    prompt_for_launch = prompt_text
    remaining_budget = timeout_s
    # Issue #758: zirv-jev-full/zirv-jev-<gate> set these on top of the
    # already-empty {} every other condition gets; threaded through the
    # proxy call, the workflow start, and the final launch below so a gate
    # is on for the whole run, not just part of it.
    env_extra = jev_gate_env_for(cond)

    if cond in JEV_PROXY_LIKE_CONDS:
        proxy_obj, proxy_elapsed, proxy_err, _raw = call_proxy(
            repo_dir, prompt_text, timeout_s=min(PROXY_CALL_TIMEOUT_S, timeout_s),
            env_extra=env_extra)
        if proxy_obj is None:
            result["proxy"]["wall_s"] = proxy_elapsed
            result["wall_s"] = time.time() - start
            result["is_error"] = True
            result["score"] = 0.0
            result["exit_code"] = -1
            result["details"] = f"proxy failure: {proxy_err}"
            write_result(run_dir, result)
            print(finish_line(k, total, task, cond, rep, result))
            return result

        seat_tier = proxy_obj.get("seat_tier")
        mapped_model = SEAT_MODEL_MAP.get(seat_tier)
        fallback_note = None
        if not mapped_model:
            mapped_model = model
            fallback_note = f"unknown seat_tier {seat_tier!r}; fell back to --model {model}"
        model_used = mapped_model

        usage = proxy_obj.get("usage", {}) or {}
        proxy_input_tokens = usage.get("input_tokens", 0) or 0
        proxy_output_tokens = usage.get("output_tokens", 0) or 0
        proxy_cost = proxy_input_tokens * PROXY_COST_PER_INPUT_TOKEN

        workflow = proxy_obj.get("workflow")
        workflow_elapsed = 0.0
        workflow_started = False
        workflow_note = None
        workflow_id = None
        if workflow:
            complexity = proxy_obj.get("complexity")
            risk = proxy_obj.get("risk")
            workflow_started, workflow_note, workflow_elapsed, workflow_id = start_workflow(
                repo_dir, workflow, prompt_text, complexity, risk, env_extra=env_extra)

        # `started_workflow_id` is the id this launch's own start actually
        # produced -- None when there was no workflow to start, the start
        # was skipped, or it failed (see build_proxy_layer/prompt_layer).
        started_workflow_id = workflow_id if workflow_started else None
        layer_text = build_proxy_layer(proxy_obj, model_used, started_workflow_id)
        prompt_for_launch = layer_text + "\n\n" + prompt_text

        result["proxy"] = {
            "complexity": proxy_obj.get("complexity"),
            "risk": proxy_obj.get("risk"),
            "execution": proxy_obj.get("execution"),
            "seat_role": proxy_obj.get("seat_role"),
            "seat_tier": seat_tier,
            "worker_tier": proxy_obj.get("worker_tier"),
            "workflow": workflow,
            "workflow_id": started_workflow_id,
            "domains": proxy_obj.get("domains") or [],
            "decider": proxy_obj.get("decider"),
            "elapsed_ms": proxy_obj.get("elapsed_ms"),
            "input_tokens": proxy_input_tokens,
            "output_tokens": proxy_output_tokens,
            "cost_usd": proxy_cost,
            "wall_s": proxy_elapsed + workflow_elapsed,
        }
        result["workflow_started"] = workflow_started
        result["workflow_note"] = workflow_note
        result["model_used"] = model_used
        if fallback_note:
            result["details"] = fallback_note
        remaining_budget = max(30.0, timeout_s - (proxy_elapsed + workflow_elapsed))

    proc = stdout_f = stderr_f = stdin_f = None
    exit_code = None
    timed_out = False
    try:
        proc, argv, stdout_f, stderr_f, stdin_f = launch(
            cond, model_used, prompt_for_launch, prompt_path, repo_dir, stdout_path, stderr_path,
            env_extra=env_extra)
        try:
            exit_code = proc.wait(timeout=remaining_budget)
        except subprocess.TimeoutExpired:
            timed_out = True
            kill_tree(proc.pid)
            try:
                exit_code = proc.wait(timeout=15)
            except Exception:
                exit_code = -1
    except Exception:
        exit_code = -1
        result["details"] = (result["details"] + "; " if result["details"] else "") + \
            "launch failure: " + repr(traceback.format_exc(limit=3))
    finally:
        for f in (stdout_f, stderr_f, stdin_f):
            if f:
                try:
                    f.close()
                except Exception:
                    pass

    wall_s = time.time() - start
    result["wall_s"] = wall_s
    result["exit_code"] = exit_code

    if timed_out:
        result["is_error"] = True
        result["score"] = 0.0
        note = f"timeout after {timeout_s:.0f}s total budget"
        result["details"] = (result["details"] + "; " if result["details"] else "") + note
        write_result(run_dir, result)
        print(finish_line(k, total, task, cond, rep, result))
        return result

    stdout_text = ""
    try:
        stdout_text = stdout_path.read_text(encoding="utf-8", errors="replace")
    except Exception:
        pass
    obj = parse_last_json(stdout_text)
    session_id = None
    result_text = ""
    if obj is None:
        result["is_error"] = True
        note = "no JSON found on stdout"
        result["details"] = (result["details"] + "; " if result["details"] else "") + note
    else:
        usage = obj.get("usage", {}) or {}
        result["duration_ms"] = obj.get("duration_ms")
        result["duration_api_ms"] = obj.get("duration_api_ms")
        result["num_turns"] = obj.get("num_turns")
        agent_cost_usd = obj.get("total_cost_usd")
        result["agent_cost_usd"] = agent_cost_usd
        result["total_cost_usd"] = (agent_cost_usd or 0.0) + result["proxy"].get("cost_usd", 0.0)
        result["input_tokens"] = usage.get("input_tokens", 0) or 0
        result["cache_creation_input_tokens"] = usage.get("cache_creation_input_tokens", 0) or 0
        result["cache_read_input_tokens"] = usage.get("cache_read_input_tokens", 0) or 0
        result["output_tokens"] = usage.get("output_tokens", 0) or 0
        subagent_stats = obj.get("subagent_stats", {}) or {}
        result["subagents_spawned"] = subagent_stats.get("spawned", 0) or 0
        pd = obj.get("permission_denials", [])
        result["permission_denials"] = len(pd) if isinstance(pd, list) else (pd or 0)
        result["is_error"] = bool(obj.get("is_error", False))
        result_text = obj.get("result", "") or ""
        session_id = obj.get("session_id")

    (run_dir / "result.txt").write_text(result_text, encoding="utf-8")

    tool_calls, zirv_cmds, transcript_note = scan_transcript(session_id)
    result["tool_calls"] = tool_calls
    result["zirv_cmds"] = zirv_cmds
    if transcript_note:
        result["details"] = (result["details"] + "; " if result["details"] else "") + transcript_note

    grading = grade(task_dir, kind, repo_dir, run_dir / "result.txt", prompt_text)
    result["score"] = grading.get("score", 0.0)
    result["passed"] = grading.get("passed", 0)
    result["total"] = grading.get("total", 0)
    result["visible_ok"] = grading.get("visible_ok", False)
    if grading.get("details"):
        result["details"] = (result["details"] + "; " if result["details"] else "") + str(grading["details"])
    result["judge_score"] = grading.get("judge_score")
    result["judge_reasoning"] = grading.get("judge_reasoning")

    if kind == "tests":
        # Second, independent blind judge on every tests-kind run: the
        # hidden-test score above answers "did it pass", this answers "was
        # it good work" -- see quality_rubric.md. Runs regardless of the
        # hidden-test score (a 0-score attempt can still get useful
        # feedback, e.g. "no changes were made at all").
        quality_score, quality_reasoning = call_quality_judge(prompt_text, repo_dir, result_text)
        result["quality_score"] = quality_score
        result["quality_reasoning"] = quality_reasoning

    write_result(run_dir, result)
    print(finish_line(k, total, task, cond, rep, result))
    return result


def chain_finish_line(k, total, task, cond, rep, result):
    cost = result.get("total_cost_usd") or 0.0
    score = result.get("score") or 0.0
    wall = result.get("wall_s") or 0.0
    n_steps = len(result.get("steps") or [])
    return (f"[{k}/{total}] {task} {cond} r{rep} -> {n_steps} steps, mean score {score:.2f} "
            f"cost ${cost:.2f} wall {wall:.0f}s")


def do_one_chain_run(bench_root, task, cond, rep, model, timeout_s, resume, k, total):
    """A `kind=chain` task: `read_chain_steps` in order, sent one after
    another to the SAME agent session (see README's "Long-session chain").

    Session continuation (see `build_argv`'s docstring): step 1 launches
    exactly like a normal run and its `session_id` is read back from
    stdout.json; every later step adds claude's own `--resume <session_id>`
    -- on `claude -p` directly for vanilla, inside the `-- ...` agent
    command for zirv/zirv-proxy/zirv-jev-*. Same mechanism both sides, so
    neither condition gets an unfair continuity advantage, and zirv's own
    supervision (rot scoring, pace, hooks) stays attached across every step
    because it is the same `zirv ctx exec` adapter resuming the same
    session, not a fresh one.

    The harness-proxy intake (zirv-proxy/zirv-jev-*) runs ONLY before step 1,
    never on a later step -- mirroring `chat.rs::proxy_intake`, which
    unconditionally skips the intake view whenever `zirv chat` is resuming an
    existing conversation (`args.resume` -- see `proxy_intake`'s own early
    `if args.simple || args.resume` return). A follow-up message in an
    already-routed conversation does not get re-routed; only a brand-new one
    does.
    """
    bench_root = Path(bench_root)
    task_dir = bench_root / "tasks" / task
    template_dir = bench_root / "template"
    run_dir = run_dir_for(bench_root, task, cond, rep)

    if resume and result_is_valid(run_dir):
        print(f"[{k}/{total}] {task} {cond} r{rep} -> skip (resume, already ok)")
        return None

    print(f"[{k}/{total}] {task} {cond} r{rep} -> starting chain")

    if run_dir.exists():
        rmtree_robust(run_dir)
    run_dir.mkdir(parents=True, exist_ok=True)
    repo_dir = run_dir / "repo"
    dirty = subprocess.run([GIT_EXE, "-C", str(template_dir), "status", "--porcelain"],
                           capture_output=True, text=True).stdout.strip()
    if dirty:
        raise RuntimeError(f"template is not pristine, refusing to copy:\n{dirty}")
    shutil.copytree(template_dir, repo_dir)

    steps = read_chain_steps(task_dir)
    result = {
        "task": task, "cond": cond, "rep": rep, "model": model, "kind": "chain",
        "wall_s": 0.0, "duration_ms": 0, "duration_api_ms": 0, "num_turns": 0,
        "total_cost_usd": 0.0, "agent_cost_usd": 0.0,
        "input_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
        "output_tokens": 0,
        "subagents_spawned": 0, "permission_denials": 0, "is_error": False, "exit_code": 0,
        "zirv_cmds": {"workflow": 0, "skill": 0, "agent": 0, "ctx": 0, "other": 0},
        "tool_calls": 0,
        "score": 0.0, "passed": 0, "total": 0, "visible_ok": True, "details": "",
        "judge_score": None, "judge_reasoning": None,
        "quality_score": None, "quality_reasoning": None,
        "proxy": default_proxy_meta(), "workflow_started": False, "workflow_note": None,
        "model_used": model, "steps": [], "session_switches": [],
    }

    env_extra = jev_gate_env_for(cond)
    session_id = None
    model_used = model
    remaining_budget = timeout_s
    chain_start = time.time()
    step_prompts_all = []
    step_texts_all = []
    step_scores = []

    prev_session_cost = 0.0
    # Calibration data for a session-switch step's cost estimate (see below):
    # accumulated cost and weighted-token totals from this run's own earlier
    # non-switch steps only -- a switch step's cost is itself partly
    # estimated, so it never feeds the calibration.
    calib_cost_sum = 0.0
    calib_weighted_sum = 0.0
    for idx, step in enumerate(steps):
        is_first = idx == 0
        if not is_first and session_id is None:
            # Step 1 never produced a session id (e.g. its own launch
            # errored before grading): every later step would otherwise
            # silently launch as a fresh, unrelated session instead of
            # resuming -- stop the chain here, before that happens, rather
            # than after this step has already run the wrong way.
            result["is_error"] = True
            result["details"] = (result["details"] + "; " if result["details"] else "") + \
                f"no session_id from step 1; chain cannot continue at step {step['label']}"
            break
        raw_prompt = read_text(step["prompt_path"])
        step_prompts_all.append(raw_prompt)
        if is_first and NONINTERACTIVE:
            raw_prompt = NONINTERACTIVE_NOTE + raw_prompt

        step_prompt_path = run_dir / f"prompt_step_{step['label']}.txt"
        step_prompt_path.write_text(raw_prompt, encoding="utf-8")

        prompt_for_launch = raw_prompt
        step_record = {
            "label": step["label"], "kind": step["kind"], "wall_s": 0.0, "cost_usd": 0.0,
            "num_turns": None, "input_tokens": 0, "output_tokens": 0,
            "score": None, "passed": None, "total": None, "visible_ok": None,
            "details": "", "is_error": False, "cost_estimated_part_usd": 0.0,
        }
        step_t0 = time.time()

        if is_first and cond in JEV_PROXY_LIKE_CONDS:
            proxy_obj, proxy_elapsed, proxy_err, _raw = call_proxy(
                repo_dir, raw_prompt, timeout_s=min(PROXY_CALL_TIMEOUT_S, remaining_budget),
                env_extra=env_extra)
            if proxy_obj is None:
                step_record["is_error"] = True
                step_record["details"] = f"proxy failure: {proxy_err}"
                step_record["wall_s"] = time.time() - step_t0
                result["steps"].append(step_record)
                result["is_error"] = True
                result["details"] = step_record["details"]
                break
            seat_tier = proxy_obj.get("seat_tier")
            mapped_model = SEAT_MODEL_MAP.get(seat_tier)
            fallback_note = None
            if not mapped_model:
                mapped_model = model
                fallback_note = f"unknown seat_tier {seat_tier!r}; fell back to --model {model}"
            model_used = mapped_model

            usage = proxy_obj.get("usage", {}) or {}
            proxy_input_tokens = usage.get("input_tokens", 0) or 0
            proxy_output_tokens = usage.get("output_tokens", 0) or 0
            proxy_cost = proxy_input_tokens * PROXY_COST_PER_INPUT_TOKEN

            workflow = proxy_obj.get("workflow")
            workflow_elapsed = 0.0
            workflow_started = False
            workflow_note = None
            workflow_id = None
            if workflow:
                complexity = proxy_obj.get("complexity")
                risk = proxy_obj.get("risk")
                workflow_started, workflow_note, workflow_elapsed, workflow_id = start_workflow(
                    repo_dir, workflow, raw_prompt, complexity, risk, env_extra=env_extra)
            started_workflow_id = workflow_id if workflow_started else None
            layer_text = build_proxy_layer(proxy_obj, model_used, started_workflow_id)
            prompt_for_launch = layer_text + "\n\n" + raw_prompt

            result["proxy"] = {
                "complexity": proxy_obj.get("complexity"), "risk": proxy_obj.get("risk"),
                "execution": proxy_obj.get("execution"), "seat_role": proxy_obj.get("seat_role"),
                "seat_tier": seat_tier, "worker_tier": proxy_obj.get("worker_tier"),
                "workflow": workflow, "workflow_id": started_workflow_id,
                "domains": proxy_obj.get("domains") or [], "decider": proxy_obj.get("decider"),
                "elapsed_ms": proxy_obj.get("elapsed_ms"), "input_tokens": proxy_input_tokens,
                "output_tokens": proxy_output_tokens, "cost_usd": proxy_cost,
                "wall_s": proxy_elapsed + workflow_elapsed,
            }
            result["workflow_started"] = workflow_started
            result["workflow_note"] = workflow_note
            if fallback_note:
                result["details"] = fallback_note
            result["total_cost_usd"] += proxy_cost
            remaining_budget = max(30.0, remaining_budget - (proxy_elapsed + workflow_elapsed))

        result["model_used"] = model_used
        resume_session_id = None if is_first else session_id
        stdout_path = run_dir / f"stdout_step_{step['label']}.json"
        stderr_path = run_dir / f"stderr_step_{step['label']}.txt"

        proc = stdout_f = stderr_f = stdin_f = None
        exit_code = None
        timed_out = False
        wait_for_launch_slot()
        try:
            proc, argv, stdout_f, stderr_f, stdin_f = launch(
                cond, model_used, prompt_for_launch, step_prompt_path, repo_dir,
                stdout_path, stderr_path, env_extra=env_extra, resume_session_id=resume_session_id)
            try:
                exit_code = proc.wait(timeout=remaining_budget)
            except subprocess.TimeoutExpired:
                timed_out = True
                kill_tree(proc.pid)
                try:
                    exit_code = proc.wait(timeout=15)
                except Exception:
                    exit_code = -1
        except Exception:
            exit_code = -1
            step_record["details"] = "launch failure: " + repr(traceback.format_exc(limit=3))
        finally:
            for f in (stdout_f, stderr_f, stdin_f):
                if f:
                    try:
                        f.close()
                    except Exception:
                        pass

        step_wall = time.time() - step_t0
        step_record["wall_s"] = step_wall
        remaining_budget = max(0.0, remaining_budget - step_wall)

        if timed_out:
            step_record["is_error"] = True
            step_record["details"] = (step_record["details"] + "; " if step_record["details"] else "") + \
                f"timeout after chain budget exhausted (~{timeout_s:.0f}s total)"
            result["steps"].append(step_record)
            result["is_error"] = True
            result["details"] = (result["details"] + "; " if result["details"] else "") + step_record["details"]
            break

        stdout_text = ""
        try:
            stdout_text = stdout_path.read_text(encoding="utf-8", errors="replace")
        except Exception:
            pass
        obj = parse_last_json(stdout_text)
        step_result_text = ""
        if obj is None:
            step_record["is_error"] = True
            step_record["details"] = "no JSON found on stdout"
        else:
            usage = obj.get("usage", {}) or {}
            step_record["num_turns"] = obj.get("num_turns")
            session_cost = obj.get("total_cost_usd") or 0.0
            new_session_id = obj.get("session_id")
            old_session_id = session_id
            is_switch = bool(
                not is_first and new_session_id and old_session_id
                and new_session_id != old_session_id)
            if is_switch:
                # The supervisor legitimately detected context rot mid-step,
                # killed the old conversation, and continued the step in a
                # brand-new one seeded with a handoff. `session_cost` here is
                # the NEW conversation's own cumulative total (it starts from
                # 0), so it already IS this step's cost for that share -- but
                # the OLD conversation's spend during this step, before the
                # kill, never made it into any `--output-format json` result
                # (its process never got to print one), so it has to be
                # estimated from its own transcript instead of dropped.
                if calib_weighted_sum > 0:
                    price_per_weighted_token = calib_cost_sum / calib_weighted_sum
                    price_note = None
                else:
                    price_per_weighted_token = FALLBACK_PRICE_PER_WEIGHTED_TOKEN
                    price_note = (
                        "no earlier non-switch step to calibrate price; fell back to "
                        f"${FALLBACK_PRICE_PER_WEIGHTED_TOKEN:g}/weighted-token")
                old_transcript = find_session_transcript(repo_dir, old_session_id)
                old_weighted = transcript_weighted_tokens_since(
                    old_transcript, iso_utc_ms(step_t0), iso_utc_ms(time.time()))
                estimated_part = old_weighted * price_per_weighted_token
                agent_cost = session_cost + estimated_part
                prev_session_cost = session_cost
                step_record["cost_estimated_part_usd"] = estimated_part
                switch_note = f"session switched {old_session_id} -> {new_session_id}"
                if price_note:
                    switch_note += f"; {price_note}"
                step_record["details"] = (step_record["details"] + "; " if step_record["details"] else "") + \
                    switch_note
                result["session_switches"].append(
                    {"step": step["label"], "from": old_session_id, "to": new_session_id})
                session_id = new_session_id
            else:
                # `claude --resume` reports total_cost_usd for the whole
                # session so far, so a step's own cost is the delta from the
                # previous step. Only a clean (non-switch) step's cost is a
                # trustworthy cost-per-weighted-token sample, so only these
                # feed the switch-step price calibration above.
                agent_cost = max(0.0, session_cost - prev_session_cost)
                prev_session_cost = max(prev_session_cost, session_cost)
                calib_cost_sum += agent_cost
                calib_weighted_sum += weighted_tokens(usage)
                if is_first:
                    session_id = new_session_id
            step_record["cost_usd"] = agent_cost
            step_record["input_tokens"] = usage.get("input_tokens", 0) or 0
            step_record["output_tokens"] = usage.get("output_tokens", 0) or 0
            result["input_tokens"] += step_record["input_tokens"]
            result["cache_creation_input_tokens"] += usage.get("cache_creation_input_tokens", 0) or 0
            result["cache_read_input_tokens"] += usage.get("cache_read_input_tokens", 0) or 0
            result["output_tokens"] += step_record["output_tokens"]
            result["num_turns"] += step_record["num_turns"] or 0
            result["duration_ms"] += obj.get("duration_ms") or 0
            result["duration_api_ms"] += obj.get("duration_api_ms") or 0
            result["agent_cost_usd"] += agent_cost
            result["total_cost_usd"] += agent_cost
            subagent_stats = obj.get("subagent_stats", {}) or {}
            result["subagents_spawned"] += subagent_stats.get("spawned", 0) or 0
            pd = obj.get("permission_denials", [])
            result["permission_denials"] += len(pd) if isinstance(pd, list) else (pd or 0)
            step_record["is_error"] = bool(obj.get("is_error", False))
            step_result_text = obj.get("result", "") or ""

        step_texts_all.append(step_result_text)
        (run_dir / f"result_step_{step['label']}.txt").write_text(step_result_text, encoding="utf-8")

        if step["kind"] == "tests":
            grading = grade_step_tests(repo_dir, step["hidden_dir"])
        elif step["kind"] == "judge":
            grading = grade_step_judge(step["rubric_path"], raw_prompt, repo_dir, step_result_text)
        else:
            grading = None

        if grading is not None:
            step_record["score"] = grading["score"]
            step_record["passed"] = grading.get("passed")
            step_record["total"] = grading.get("total")
            step_record["visible_ok"] = grading.get("visible_ok")
            step_record["details"] = (step_record["details"] + "; " if step_record["details"] else "") + \
                str(grading.get("details") or "")
            step_scores.append(grading["score"])
            if grading.get("visible_ok") is False:
                result["visible_ok"] = False
            if grading.get("passed") is not None:
                result["passed"] += grading["passed"]
            if grading.get("total") is not None:
                result["total"] += grading["total"]

        result["steps"].append(step_record)
        print(f"    step {step['label']} ({step['kind']}) -> "
              f"score {step_record['score']}, cost ${step_record['cost_usd']:.2f}, "
              f"wall {step_record['wall_s']:.0f}s")

        if remaining_budget <= 0:
            result["is_error"] = True
            result["details"] = (result["details"] + "; " if result["details"] else "") + \
                f"chain budget ({timeout_s:.0f}s) exhausted after step {step['label']}"
            break

    result["wall_s"] = time.time() - chain_start

    tool_calls, zirv_cmds, transcript_note = scan_transcript(session_id)
    result["tool_calls"] = tool_calls
    result["zirv_cmds"] = zirv_cmds
    if transcript_note:
        result["details"] = (result["details"] + "; " if result["details"] else "") + transcript_note

    result["score"] = statistics_mean_or_zero(step_scores)

    if not result["is_error"]:
        # Spec item 2: the end-of-chain work-quality judge runs once, over
        # the WHOLE session -- every prompt shown, every step's final
        # response concatenated, one diff of the repo's final state.
        quality_score, quality_reasoning = call_quality_judge_chain(
            step_prompts_all, repo_dir, step_texts_all)
        result["quality_score"] = quality_score
        result["quality_reasoning"] = quality_reasoning

    write_result(run_dir, result)
    print(chain_finish_line(k, total, task, cond, rep, result))
    return result


def statistics_mean_or_zero(values):
    return (sum(values) / len(values)) if values else 0.0


def task_kind(bench_root, task):
    return read_text(Path(bench_root) / "tasks" / task / "kind.txt").strip()


def dispatch_with_usage_retry(bench_root, task, cond, rep, model, timeout_s, resume, k, total):
    global _pause_until
    for attempt in range(USAGE_LIMIT_RETRIES + 1):
        result = dispatch_run(bench_root, task, cond, rep, model, timeout_s, resume, k, total)
        resume_at = usage_limit_resume_at(run_dir_for(bench_root, task, cond, rep))
        if resume_at is None or attempt == USAGE_LIMIT_RETRIES:
            return result
        with _stagger_lock:
            _pause_until = max(_pause_until, resume_at)
        print(f"[{k}/{total}] {task} {cond} r{rep} -> usage limit; pausing until "
              f"{time.strftime('%H:%M', time.localtime(_pause_until))}, then retrying", flush=True)
        wait_until_unpaused()
        resume = False
    return result


def dispatch_run(bench_root, task, cond, rep, model, timeout_s, resume, k, total):
    """Single dispatch point: a `kind=chain` task runs the multi-step chain
    runner, every other kind runs the ordinary single-shot one. Kept as a
    thin wrapper (rather than teaching `do_one_run` about chains) so a
    single-shot run's code path is untouched by this feature."""
    if task_kind(bench_root, task) == "chain":
        return do_one_chain_run(bench_root, task, cond, rep, model, timeout_s, resume, k, total)
    return do_one_run(bench_root, task, cond, rep, model, timeout_s, resume, k, total)


def first_prompt_text(task_dir):
    """The prompt shown in `--dry-run`'s sample argv: `prompt.txt` for an
    ordinary task, step 1 of `prompts/` for a chain task."""
    prompt_path = task_dir / "prompt.txt"
    if prompt_path.exists():
        return read_text(prompt_path)
    steps = sorted((task_dir / "prompts").glob("*.txt")) if (task_dir / "prompts").is_dir() else []
    if steps:
        return read_text(steps[0])
    raise FileNotFoundError(f"no prompt.txt or prompts/*.txt under {task_dir}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tasks", required=True, help="'all' or comma-separated task ids")
    ap.add_argument("--conds", required=True,
                     help="comma-separated subset of " + ",".join(CANONICAL_CONDS))
    ap.add_argument("--reps", type=int, required=True)
    ap.add_argument("--model", required=True,
                     help="model for vanilla/zirv; ignored for zirv-proxy (seat_tier decides)")
    ap.add_argument("--parallel", type=int, default=1)
    ap.add_argument("--timeout-min", type=float, default=DEFAULT_TIMEOUT_MIN)
    ap.add_argument("--resume", action="store_true")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--vanilla-plugin-dir", default=None, help="load this plugin dir into the vanilla condition (user settings layer dropped)")
    ap.add_argument("--zirv-dir", default=None, help="directory holding the zirv.exe to test; prepended to PATH so hooks resolve to it too")
    ap.add_argument("--runs-subdir", default="runs", help="subdirectory of bench root for run outputs")
    ap.add_argument("--bench-root", default=None,
                     help="defaults to this script's directory ($BENCH)")
    ap.add_argument("--stagger-s", type=float, default=0.0, help="minimum seconds between two run starts")
    ap.add_argument("--noninteractive", action="store_true", help="prefix every condition's prompt with NONINTERACTIVE_NOTE")
    args = ap.parse_args()

    global RUNS_SUBDIR, VANILLA_PLUGIN_DIR, NONINTERACTIVE, STAGGER_S
    STAGGER_S = args.stagger_s
    RUNS_SUBDIR = args.runs_subdir
    NONINTERACTIVE = args.noninteractive
    VANILLA_PLUGIN_DIR = args.vanilla_plugin_dir
    if args.zirv_dir:
        os.environ["PATH"] = str(Path(args.zirv_dir).resolve()) + ";" + os.environ["PATH"]
        print("zirv under test:", shutil.which("zirv"))
    bench_root = Path(args.bench_root) if args.bench_root else Path(__file__).resolve().parent
    tasks_dir = bench_root / "tasks"

    if args.tasks == "all":
        tasks = sorted(p.name for p in tasks_dir.iterdir() if p.is_dir())
    else:
        tasks = [t.strip() for t in args.tasks.split(",") if t.strip()]

    conds = [c.strip() for c in args.conds.split(",") if c.strip()]
    unknown = [c for c in conds if c not in CANONICAL_CONDS]
    if unknown:
        print(f"unknown conds: {unknown} (allowed: {CANONICAL_CONDS})", file=sys.stderr)
        sys.exit(2)

    run_list = build_run_list(tasks, conds, args.reps)
    total = len(run_list)

    if args.dry_run:
        print(f"Run list ({total} runs):")
        for (task, cond, rep) in run_list:
            print(f"  {task} {cond} r{rep}")
        shown = set()
        for cond in CANONICAL_CONDS:
            if cond in conds and cond not in shown and tasks:
                task_dir = tasks_dir / tasks[0]
                try:
                    prompt_text = first_prompt_text(task_dir)
                except Exception:
                    prompt_text = "<PROMPT>"
                if cond in JEV_PROXY_LIKE_CONDS:
                    print(f"\nFirst {cond} argv (model is decided at run time by the "
                          "proxy's seat_tier; shown here WITHOUT calling the proxy):")
                    argv = build_argv(cond, "<seat-tier-mapped-model>", prompt_text)
                else:
                    print(f"\nFirst {cond} argv:")
                    argv = build_argv(cond, args.model, prompt_text)
                print("  " + " ".join(repr(a) for a in argv))
                env_extra = jev_gate_env_for(cond)
                if env_extra:
                    print("  env additions: " + ", ".join(f"{k}={'<removed>' if v is None else v}" for k, v in sorted(env_extra.items())))
                shown.add(cond)
        return

    timeout_s = args.timeout_min * 60
    (bench_root / RUNS_SUBDIR).mkdir(parents=True, exist_ok=True)

    results = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.parallel)) as ex:
        futs = [
            ex.submit(dispatch_with_usage_retry, bench_root, task, cond, rep, args.model,
                      timeout_s, args.resume, k, total)
            for k, (task, cond, rep) in enumerate(run_list, start=1)
        ]
        for fut in concurrent.futures.as_completed(futs):
            r = fut.result()
            if r:
                results.append(r)

    n_err = sum(1 for r in results if r.get("is_error"))
    print(f"Done: {len(results)} runs completed, {n_err} errored/timed out.")


if __name__ == "__main__":
    main()
