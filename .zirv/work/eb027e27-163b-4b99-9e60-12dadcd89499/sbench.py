#!/usr/bin/env python3
"""Interactive-session subagent cost benchmark: arm G (general-purpose) vs arm W (lean worker)."""
import argparse, collections, glob, json, os, pty, re, select, shutil, signal, struct, fcntl, termios
import subprocess, sys, time, uuid
from pathlib import Path

HARNESS = "/tmp/claude-501/fafo-r2/harness"
BENCH = Path("/tmp/claude-501/fafo-r2/bench")
SB = Path("/tmp/claude-501/fafo-r2/sbench")
sys.path.insert(0, HARNESS)
import run as H  # noqa: E402

BODY = (SB / "general-purpose-body.txt").read_text(encoding="utf-8")
DISALLOWED = ["Artifact", "Agent", "ScheduleWakeup", "ShareOnboardingGuide", "ListAgents", "ReportFindings", "Workflow"]
TYPES = {"G": "general-purpose", "W": "worker"}
PRICE = {"input": 2.0, "read": 0.2, "w5m": 2.5, "w1h": 4.0, "output": 10.0}
ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*\x07|\x1b[()][A-Z0-9]")
HOME = os.path.expanduser("~")


def prepare_repo(task, run_dir):
    """Same copy-from-template step as run.do_one_run (pristine check + copytree)."""
    template = BENCH / "template"
    dirty = subprocess.run([H.GIT_EXE, "-C", str(template), "status", "--porcelain"],
                           capture_output=True, text=True).stdout.strip()
    if dirty:
        raise RuntimeError("template not pristine:\n" + dirty)
    if run_dir.exists():
        H.rmtree_robust(run_dir)
    run_dir.mkdir(parents=True)
    repo = run_dir / "repo"
    shutil.copytree(template, repo)
    return repo


def clean_env():
    tmp = SB / "tmp"
    tmp.mkdir(exist_ok=True)
    return {"HOME": HOME, "USER": os.environ.get("USER", "jonathansolskov"),
            "LOGNAME": os.environ.get("USER", "jonathansolskov"), "SHELL": "/bin/zsh",
            "TERM": "xterm-256color", "LANG": "en_US.UTF-8", "TMPDIR": str(tmp),
            "PATH": f"{HOME}/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"}


def run_pty(argv, cwd, env, log_path, marker, session_id, timeout_s):
    """Run argv in a PTY. Returns (wall_s, timed_out, dialogs_answered)."""
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 200, 0, 0))
    proc = subprocess.Popen(argv, cwd=cwd, env=env, stdin=slave, stdout=slave, stderr=slave,
                            start_new_session=True, close_fds=True)
    os.close(slave)
    t0 = time.time()
    buf = ""
    dialogs = []
    last_dialog = 0.0
    timed_out = False
    started = False
    done_at = None
    completed = [False]
    with open(log_path, "wb") as log:
        def pump(wait):
            nonlocal buf
            r, _, _ = select.select([master], [], [], wait)
            if not r:
                return True
            try:
                data = os.read(master, 65536)
            except OSError:
                return False
            if not data:
                return False
            log.write(data)
            log.flush()
            buf = (buf + ANSI.sub("", data.decode("utf-8", "replace")))[-4000:]
            return True
        alive = True
        while alive and proc.poll() is None:
            alive = pump(0.5)
            now = time.time()
            # Agent runs in the background in interactive mode: the first Stop fires right after
            # the launch; the real end is the second Stop (parent's DONE after the subagent returns).
            stops = len(marker.read_text().splitlines()) if marker.exists() else 0
            mt = find_main(cwd, session_id)
            no_sub = stops == 1 and now - marker.stat().st_mtime > 90 and not (
                mt and glob.glob(str(mt.with_suffix("")) + "/subagents/*.jsonl"))
            if stops >= 2 or no_sub:
                done_at = done_at or marker.stat().st_mtime
                if now - done_at >= 3 and now - marker.stat().st_mtime >= 3:
                    break
                continue
            if now - t0 > timeout_s:
                timed_out = True
                break
            if not started:
                started = bool(find_main(cwd, session_id))
            low = buf.lower()
            if not started and now - last_dialog > 4 and len(dialogs) < 6 and (
                    "trust" in low or re.search(r"press enter|enter to (continue|confirm)", low)):
                squeezed = re.sub(r"\s+", "", low)
                if "no,exit" in squeezed and "yes,itrust" in squeezed:
                    # default cursor is on "No, exit": move down to "Yes" first
                    os.write(master, b"\x1b[B")
                    time.sleep(0.4)
                    os.write(master, b"\r")
                    dialogs.append(f"t+{now - t0:.0f}s: trust dialog: Down + Enter (Yes, I trust)")
                else:
                    os.write(master, b"\r")
                    dialogs.append(f"t+{now - t0:.0f}s: sent Enter (matched dialog text)")
                last_dialog = now
                buf = ""
        wall_s = (done_at - t0) if done_at else time.time() - t0
        completed[0] = bool(done_at)
        if proc.poll() is None:
            try:
                os.write(master, b"/exit\r")
            except OSError:
                pass
            end = time.time() + 10
            while time.time() < end and proc.poll() is None:
                pump(0.5)
        if proc.poll() is None:
            try:
                os.killpg(proc.pid, signal.SIGTERM)
                time.sleep(2)
                os.killpg(proc.pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
        proc.wait()
    os.close(master)
    return wall_s, timed_out, dialogs, completed[0]


def find_main(repo, session_id):
    for cand in (H.project_slug(repo), H.project_slug(os.path.realpath(repo))):
        p = Path(HOME) / ".claude/projects" / cand / f"{session_id}.jsonl"
        if p.exists():
            return p
    m = glob.glob(f"{HOME}/.claude/projects/*/{session_id}.jsonl")
    return Path(m[0]) if m else None


def read_lines(path):
    out = []
    for ln in Path(path).read_text(encoding="utf-8", errors="replace").splitlines():
        try:
            out.append(json.loads(ln))
        except Exception:
            pass
    return out


def usage_scope(files):
    """Aggregate usage over transcripts; dedupe by requestId keeping max output_tokens."""
    reqs = {}
    for f in files:
        for o in read_lines(f):
            if o.get("type") != "assistant":
                continue
            u = (o.get("message") or {}).get("usage")
            if not u:
                continue
            rid = o.get("requestId") or (o.get("message") or {}).get("id") or o.get("uuid")
            prev = reqs.get(rid)
            if prev is None or (u.get("output_tokens") or 0) >= (prev[0].get("output_tokens") or 0):
                reqs[rid] = (u, o.get("timestamp"))
    s = dict(requests=len(reqs), input_tokens=0, cache_read_input_tokens=0,
             cache_creation_input_tokens=0, write_5m=0, write_1h=0, output_tokens=0)
    for u, _ in reqs.values():
        cc = u.get("cache_creation") or {}
        w5, w1 = cc.get("ephemeral_5m_input_tokens"), cc.get("ephemeral_1h_input_tokens")
        tot = u.get("cache_creation_input_tokens") or 0
        if w5 is None and w1 is None:
            w5, w1 = tot, 0
        s["input_tokens"] += u.get("input_tokens") or 0
        s["cache_read_input_tokens"] += u.get("cache_read_input_tokens") or 0
        s["cache_creation_input_tokens"] += tot
        s["write_5m"] += w5 or 0
        s["write_1h"] += w1 or 0
        s["output_tokens"] += u.get("output_tokens") or 0
    s["cost_usd"] = (s["input_tokens"] * PRICE["input"] + s["cache_read_input_tokens"] * PRICE["read"]
                     + s["write_5m"] * PRICE["w5m"] + s["write_1h"] * PRICE["w1h"]
                     + s["output_tokens"] * PRICE["output"]) / 1e6
    return s, reqs


def ts(t):
    from datetime import datetime
    return datetime.fromisoformat(t.replace("Z", "+00:00")).timestamp()


def analyze(repo, session_id):
    main = find_main(repo, session_id)
    res = {}
    if not main:
        return {"error": "main transcript not found"}, ""
    sub_files = sorted(glob.glob(str(main.with_suffix("") / "subagents" / "*.jsonl")))
    m, _ = usage_scope([main])
    s, sreqs = usage_scope(sub_files)
    t = {k: m[k] + s[k] for k in m}
    res["main"], res["sub"], res["total"] = m, s, t
    res["n_subagents"] = len(sub_files)
    res["sub_tools"], res["sub_agent_type"] = None, None
    res["sub_first_prompt_tokens"] = None
    res["sub_duration_s"] = 0.0
    calls = collections.Counter()
    last_text = ""
    first_ts = None
    for f in sub_files:
        lines = read_lines(f)
        for o in lines:
            if o.get("type") == "attachment" and (o.get("attachment") or {}).get("type") == "prompt_snapshot" \
                    and (o.get("attachment") or {}).get("tools") and res["sub_tools"] is None:
                tools = o["attachment"].get("tools") or []
                res["sub_tools"] = [x if isinstance(x, str) else (x.get("name") or str(x)) for x in tools]
            if o.get("type") == "assistant":
                for c in (o.get("message") or {}).get("content") or []:
                    if isinstance(c, dict) and c.get("type") == "tool_use":
                        calls[c.get("name")] += 1
                    if isinstance(c, dict) and c.get("type") == "text":
                        last_text = c.get("text") or last_text
    ordered = sorted((v[1], v[0]) for v in sreqs.values() if v[1])
    if ordered:
        u = ordered[0][1]
        res["sub_first_prompt_tokens"] = ((u.get("input_tokens") or 0) + (u.get("cache_read_input_tokens") or 0)
                                          + (u.get("cache_creation_input_tokens") or 0))
        res["sub_duration_s"] = ts(ordered[-1][0]) - ts(ordered[0][0])
    res["sub_tool_calls"] = dict(calls)
    # delegation type from the main transcript's Agent tool_use
    types = []
    for o in read_lines(main):
        if o.get("type") == "assistant":
            for c in (o.get("message") or {}).get("content") or []:
                if isinstance(c, dict) and c.get("type") == "tool_use" and c.get("name") in ("Agent", "Task"):
                    types.append((c.get("input") or {}).get("subagent_type"))
    res["main_agent_calls"] = types
    res["sub_agent_type"] = types[0] if types else None
    return res, last_text


def run_one(task, arm, rep, rnd, timeout_s):
    run_dir = SB / f"runs-{rnd}-{arm}" / f"{task}__r{rep}"
    if (run_dir / "result.json").exists():
        print(f"skip {task} {arm} r{rep}")
        return
    task_dir = BENCH / "tasks" / task
    prompt = H.read_text(task_dir / "prompt.txt")
    kind = H.read_text(task_dir / "kind.txt").strip()
    repo = prepare_repo(task, run_dir)
    sid = str(uuid.uuid4())
    marker = run_dir / "stop.marker"
    agents = {"worker": {"description": "General-purpose worker for delegated implementation tasks.",
                         "prompt": BODY, "disallowedTools": DISALLOWED}}
    settings = json.loads(H.operator_plugin_settings() or "{}")
    stop_hook = {"hooks": [{"type": "command", "command": f"echo stop >> '{marker}'"}]}
    settings.setdefault("hooks", {}).setdefault("Stop", []).append(stop_hook)
    parent = (f'Delegate the task below to exactly one subagent: call the Agent tool once with subagent_type '
              f'"{TYPES[arm]}", description "bench task", and the task text verbatim as the prompt. Do not read, '
              f'edit or run anything yourself. When the subagent returns, reply with the single word DONE.\n\n'
              f'<task>\n{prompt}\n</task>')
    argv = [H.CLAUDE_EXE, "--model", "sonnet", "--session-id", sid, "--setting-sources", "project,local",
            "--permission-mode", "dontAsk", "--allowedTools", H.VANILLA_ALLOWED_TOOLS + ",Agent",
            "--agents", json.dumps(agents), "--settings", json.dumps(settings), parent]
    print(f"run {task} {arm} r{rep} sid={sid}", flush=True)
    wall_s, timed_out, dialogs, completed = run_pty(argv, str(repo), clean_env(), run_dir / "pty.log", marker, sid, timeout_s)
    ana, sub_text = analyze(repo, sid)
    result = {"task": task, "arm": arm, "rep": rep, "session_id": sid, "wall_s": wall_s,
              "timed_out": timed_out, "dialogs": dialogs, "stop_marker": completed,
              "is_error": timed_out or not completed}
    result.update(ana)
    # grade exactly as do_one_run: hidden tests, then blind quality judge (tests-kind)
    result_txt = run_dir / "result.txt"
    result_txt.write_text(sub_text, encoding="utf-8")
    g = H.grade(task_dir, kind, repo, result_txt, prompt)
    result.update({"score": g.get("score"), "passed": g.get("passed"), "total_tests": g.get("total"),
                   "visible_ok": g.get("visible_ok"), "details": g.get("details")})
    q = None
    if kind in ("tests", "answer") or True:
        q, qr, _ = H.call_quality_judge(prompt, repo, sub_text)
        result["quality_reasoning"] = qr
    result["quality_score"] = q
    t = result.get("total") or {}
    result["total_cost_usd"] = t.get("cost_usd")
    result["output_tokens"] = t.get("output_tokens")
    result["num_turns"] = t.get("requests")
    result["duration_ms"] = (result.get("sub_duration_s") or 0) * 1000
    (run_dir / "result.json").write_text(json.dumps(result, indent=1), encoding="utf-8")
    print(f"done {task} {arm} r{rep}: wall={wall_s:.0f}s timeout={timed_out} cost={result['total_cost_usd']} "
          f"score={result['score']} q={q}", flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tasks", required=True)
    ap.add_argument("--arms", default="G,W")
    ap.add_argument("--reps", type=int, default=1)
    ap.add_argument("--round", required=True)
    ap.add_argument("--timeout-s", type=int, default=1500)
    ap.add_argument("--stagger-s", type=int, default=30)
    a = ap.parse_args()
    first = True
    for task in a.tasks.split(","):
        for rep in range(1, a.reps + 1):
            for arm in a.arms.split(","):
                rd = SB / f"runs-{a.round}-{arm}" / f"{task}__r{rep}"
                if not (rd / "result.json").exists() and not first:
                    time.sleep(a.stagger_s)
                first = False
                run_one(task, arm, rep, a.round, a.timeout_s)


if __name__ == "__main__":
    main()
