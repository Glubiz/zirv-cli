#!/usr/bin/env python3
"""Benchmark runner: vanilla `claude -p` vs `zirv ctx exec` (with or without
the zirv proxy / Jev intake layer) wrapping claude.

Conditions:
  vanilla     -- claude -p directly, prompt on stdin.
  zirv        -- zirv ctx exec --agent claude --prompt <prompt> -- ...
  zirv-proxy  -- runs `zirv ctx proxy --json <prompt>` first (headless
                 `zirv ctx exec` skips the `zirv chat` Jev intake step), maps
                 the decided seat_tier to a claude model, optionally starts a
                 workflow, prepends a `[zirv proxy]` layer to the prompt, then
                 launches exactly like `zirv`.

See CONTRACT.md in this directory for the full spec. stdlib only (3.11).
"""
import argparse
import concurrent.futures
import glob
import json
import os
import shutil
import stat
import subprocess
import sys
import time
import traceback
from pathlib import Path

CLAUDE_EXE = r"C:\Users\josj\.local\bin\claude.exe"
ZIRV_FALLBACK = r"C:\ProgramData\chocolatey\bin\zirv.exe"
PYTHON_EXE = r"C:\Python311\python.exe"
GIT_EXE = r"C:\Program Files\Git\cmd\git.exe"
TASKKILL_EXE = r"C:\Windows\System32\taskkill.exe"
DEFAULT_TIMEOUT_MIN = 20
CANONICAL_CONDS = ["vanilla", "zirv", "zirv-proxy"]
JUDGE_DISALLOWED = "Write,Edit,Bash,NotebookEdit,Read,Glob,Grep,Agent,WebFetch,WebSearch"
SEAT_MODEL_MAP = {"cheap": "haiku", "standard": "sonnet", "frontier": "opus"}
PROXY_CALL_TIMEOUT_S = 120
WORKFLOW_START_TIMEOUT_S = 60
PROXY_COST_PER_INPUT_TOKEN = 0.042 / 1_000_000

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


def build_argv(cond, model, prompt_text):
    if cond == "vanilla":
        if VANILLA_PLUGIN_DIR:
            # Vanilla + a plugin (e.g. superpowers): drop the user settings layer
            # (where the operator's global zirv hooks live) instead of disabling
            # all hooks, so the plugin's own SessionStart hook still runs. The
            # user layer's bypassPermissions default is restated explicitly.
            return [CLAUDE_EXE, "-p", "--output-format", "json", "--model", model,
                    "--setting-sources", "project,local",
                    "--permission-mode", "bypassPermissions",
                    "--plugin-dir", VANILLA_PLUGIN_DIR]
        settings = json.dumps({"disableAllHooks": True}, separators=(",", ":"))
        return [CLAUDE_EXE, "-p", "--output-format", "json", "--model", model,
                "--settings", settings]
    elif cond in ("zirv", "zirv-proxy"):
        # zirv-proxy launches exactly like zirv: same shape, different
        # (proxy-decided) model and a prompt with the proxy layer prepended.
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


def launch(cond, model, prompt_text, prompt_path, cwd, stdout_path, stderr_path):
    argv = build_argv(cond, model, prompt_text)
    stdout_f = open(stdout_path, "wb")
    stderr_f = open(stderr_path, "wb")
    stdin_f = None
    try:
        if cond == "vanilla":
            stdin_f = open(prompt_path, "rb")
            proc = subprocess.Popen(argv, cwd=str(cwd), stdin=stdin_f,
                                     stdout=stdout_f, stderr=stderr_f)
        else:
            proc = subprocess.Popen(argv, cwd=str(cwd), stdin=subprocess.DEVNULL,
                                     stdout=stdout_f, stderr=stderr_f)
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


def get_capped_diff(repo_dir, cap_bytes=60_000):
    subprocess.run([GIT_EXE, "add", "-N", "."], cwd=str(repo_dir), capture_output=True)
    # Diff against the template's root commit, not HEAD: an agent that commits
    # its own work must not make its change invisible to the judge.
    root = subprocess.run([GIT_EXE, "rev-list", "--max-parents=0", "HEAD"], cwd=str(repo_dir),
                          capture_output=True, text=True).stdout.split()
    base = root[-1] if root else "HEAD"
    proc = subprocess.run([GIT_EXE, "diff", base], cwd=str(repo_dir), capture_output=True)
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


def call_judge(prompt_text):
    argv = [CLAUDE_EXE, "-p", "--output-format", "json", "--model", "sonnet",
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
        "seat_tier": None, "worker_tier": None, "workflow": None, "domains": [],
        "decider": None, "elapsed_ms": None, "input_tokens": 0, "output_tokens": 0,
        "cost_usd": 0.0, "wall_s": 0.0,
    }


def call_proxy(repo_dir, prompt_text, timeout_s):
    """Run `zirv ctx proxy --json <prompt>`. Returns (obj, elapsed_s, error_note, raw_stdout)."""
    argv = [zirv_exe(), "ctx", "proxy", "--json", prompt_text]
    t0 = time.time()
    try:
        proc = subprocess.run(argv, cwd=str(repo_dir), capture_output=True, timeout=timeout_s)
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


def start_workflow(repo_dir, workflow, prompt_text, complexity, risk):
    """Run `zirv workflow start <workflow> ...`. Returns (started, note, elapsed_s)."""
    argv = [zirv_exe(), "workflow", "start", workflow, "--task", prompt_text,
            "--repo", str(repo_dir), "--complexity", str(complexity), "--risk", str(risk)]
    t0 = time.time()
    try:
        proc = subprocess.run(argv, cwd=str(repo_dir), capture_output=True,
                               timeout=WORKFLOW_START_TIMEOUT_S)
    except subprocess.TimeoutExpired:
        return False, "workflow start timed out", time.time() - t0
    except Exception as e:
        return False, f"workflow start failed: {e}", time.time() - t0
    elapsed = time.time() - t0
    stdout_text = proc.stdout.decode("utf-8", errors="replace").strip()
    stderr_text = proc.stderr.decode("utf-8", errors="replace").strip()
    first_line = ""
    if stdout_text:
        first_line = stdout_text.splitlines()[0]
    elif stderr_text:
        first_line = stderr_text.splitlines()[0]
    started = proc.returncode == 0
    note = f"exit={proc.returncode}: {first_line}"
    return started, note, elapsed


def build_proxy_layer(proxy_obj, model):
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
    lines.append(f"workflow: {workflow if workflow else 'none'}")
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
        "proxy": default_proxy_meta(), "workflow_started": False, "workflow_note": None,
        "model_used": model,
    }

    start = time.time()
    model_used = model
    prompt_for_launch = prompt_text
    remaining_budget = timeout_s

    if cond == "zirv-proxy":
        proxy_obj, proxy_elapsed, proxy_err, _raw = call_proxy(
            repo_dir, prompt_text, timeout_s=min(PROXY_CALL_TIMEOUT_S, timeout_s))
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
        if workflow:
            complexity = proxy_obj.get("complexity")
            risk = proxy_obj.get("risk")
            workflow_started, workflow_note, workflow_elapsed = start_workflow(
                repo_dir, workflow, prompt_text, complexity, risk)

        layer_text = build_proxy_layer(proxy_obj, model_used)
        prompt_for_launch = layer_text + "\n\n" + prompt_text

        result["proxy"] = {
            "complexity": proxy_obj.get("complexity"),
            "risk": proxy_obj.get("risk"),
            "execution": proxy_obj.get("execution"),
            "seat_role": proxy_obj.get("seat_role"),
            "seat_tier": seat_tier,
            "worker_tier": proxy_obj.get("worker_tier"),
            "workflow": workflow,
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
            cond, model_used, prompt_for_launch, prompt_path, repo_dir, stdout_path, stderr_path)
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

    write_result(run_dir, result)
    print(finish_line(k, total, task, cond, rep, result))
    return result


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tasks", required=True, help="'all' or comma-separated task ids")
    ap.add_argument("--conds", required=True,
                     help="comma-separated subset of vanilla,zirv,zirv-proxy")
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
    ap.add_argument("--noninteractive", action="store_true", help="prefix every condition's prompt with NONINTERACTIVE_NOTE")
    args = ap.parse_args()

    global RUNS_SUBDIR, VANILLA_PLUGIN_DIR, NONINTERACTIVE
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
                    prompt_text = read_text(task_dir / "prompt.txt")
                except Exception:
                    prompt_text = "<PROMPT>"
                if cond == "zirv-proxy":
                    print("\nFirst zirv-proxy argv (model is decided at run time by the "
                          "proxy's seat_tier; shown here WITHOUT calling the proxy):")
                    argv = build_argv(cond, "<seat-tier-mapped-model>", prompt_text)
                else:
                    print(f"\nFirst {cond} argv:")
                    argv = build_argv(cond, args.model, prompt_text)
                print("  " + " ".join(repr(a) for a in argv))
                shown.add(cond)
        return

    timeout_s = args.timeout_min * 60
    (bench_root / RUNS_SUBDIR).mkdir(parents=True, exist_ok=True)

    results = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.parallel)) as ex:
        futs = [
            ex.submit(do_one_run, bench_root, task, cond, rep, args.model,
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
