#!/usr/bin/env python3
"""Unit tests for orch.py's pure parts (orchestration lane). stdlib `unittest`
only (CONTRACT.md forbids pytest); synthetic transcript records in the shape
Claude Code writes, no PTY, no billed commands."""
import json
import os
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import orch  # noqa: E402
import run as run_module  # noqa: E402

BENCH = Path(__file__).resolve().parent


def asst(rid, model="claude-opus-5", stop=None, blocks=None, usage=None, ts="2026-10-07T10:00:00.000Z"):
    u = {"input_tokens": 10, "cache_read_input_tokens": 1000, "cache_creation_input_tokens": 100,
         "cache_creation": {"ephemeral_5m_input_tokens": 100, "ephemeral_1h_input_tokens": 0},
         "output_tokens": 50}
    u.update(usage or {})
    return {"type": "assistant", "requestId": rid, "timestamp": ts,
            "message": {"model": model, "id": "msg_" + rid, "stop_reason": stop,
                        "content": blocks or [{"type": "text", "text": "hi"}], "usage": u}}


def tool_use(tid, name="Bash", **inp):
    return {"type": "tool_use", "id": tid, "name": name, "input": inp}


def user_result(tid):
    return {"type": "user", "message": {"role": "user",
                                        "content": [{"type": "tool_result", "tool_use_id": tid, "content": "ok"}]}}


def user_text(text):
    return {"type": "user", "message": {"role": "user", "content": text}}


class PriceTests(unittest.TestCase):
    def test_price_keys_follow_the_catalogue(self):
        self.assertEqual(orch.price_key("claude-opus-5"), "opus-5")
        self.assertEqual(orch.price_key("claude-opus-5-20260915"), "opus-5")
        self.assertEqual(orch.price_key("claude-opus-5-5"), "opus-5-5")
        self.assertEqual(orch.price_key("claude-sonnet-5-5"), "sonnet-5")
        self.assertEqual(orch.price_key("claude-haiku-4-5-20251001"), "haiku-4-5")
        self.assertIsNone(orch.price_key("gpt-6-sol"))
        self.assertIsNone(orch.price_key(None))

    def test_one_million_of_each_bucket(self):
        bucket = {"input": 1_000_000, "read": 1_000_000, "write_5m": 1_000_000, "write_1h": 1_000_000,
                  "output": 1_000_000}
        # opus-5: 5 + 0.5 + 6.25 + (1h write = 2 x input = 10) + 25
        self.assertAlmostEqual(orch.usage_cost(bucket, "claude-opus-5"), 46.75)
        # haiku-4-5: 1 + 0.1 + 1.25 + 2 + 5
        self.assertAlmostEqual(orch.usage_cost(bucket, "claude-haiku-4-5"), 9.35)
        self.assertIsNone(orch.usage_cost(bucket, "mystery-model"))


class UsageTests(unittest.TestCase):
    def test_dedupes_block_records_by_request_id_keeping_max_output(self):
        a = asst("r1", usage={"output_tokens": 4})
        b = asst("r1", stop="end_turn", usage={"output_tokens": 900})
        per = orch.collect_usage([[a, b]])
        self.assertEqual(per["claude-opus-5"]["requests"], 1)
        self.assertEqual(per["claude-opus-5"]["output"], 900)

    def test_sums_parent_and_subagents_per_model(self):
        parent = [asst("p1", model="claude-opus-5"), asst("p2", model="claude-opus-5")]
        sub1 = [asst("s1", model="claude-sonnet-5")]
        sub2 = [asst("s2", model="claude-haiku-4-5", usage={"cache_creation": {
            "ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 100}})]
        per = orch.collect_usage([parent, sub1, sub2])
        self.assertEqual(sorted(per), ["claude-haiku-4-5", "claude-opus-5", "claude-sonnet-5"])
        self.assertEqual(per["claude-opus-5"]["requests"], 2)
        self.assertEqual(per["claude-opus-5"]["input"], 20)
        self.assertEqual(per["claude-haiku-4-5"]["write_1h"], 100)
        self.assertEqual(per["claude-haiku-4-5"]["write_5m"], 0)
        cost, unpriced = orch.total_cost(per)
        self.assertEqual(unpriced, [])
        expected = sum(orch.usage_cost(b, m) for m, b in per.items())
        self.assertAlmostEqual(cost, expected)
        self.assertGreater(cost, 0)

    def test_missing_cache_creation_split_counts_as_5m_and_synthetic_is_ignored(self):
        rec = asst("r1")
        del rec["message"]["usage"]["cache_creation"]
        syn = asst("r2", model="<synthetic>")
        per = orch.collect_usage([[rec, syn]])
        self.assertEqual(list(per), ["claude-opus-5"])
        self.assertEqual(per["claude-opus-5"]["write_5m"], 100)

    def test_unpriced_model_is_reported_not_priced_at_zero(self):
        per = orch.collect_usage([[asst("r1", model="some-new-model")]])
        cost, unpriced = orch.total_cost(per)
        self.assertEqual(unpriced, ["some-new-model"])
        self.assertEqual(cost, 0.0)


class LaunchTests(unittest.TestCase):
    def test_agent_launches_and_final_text(self):
        main = [asst("r1", blocks=[tool_use("t1", "Agent", subagent_type="zirv:worker", model="sonnet",
                                            description="d")]),
                user_result("t1"),
                asst("r2", stop="end_turn", blocks=[{"type": "text", "text": "all done"}])]
        launches = orch.main_agent_launches(main)
        self.assertEqual(len(launches), 1)
        self.assertEqual(launches[0]["subagent_type"], "zirv:worker")
        self.assertEqual(launches[0]["model"], "sonnet")
        self.assertEqual(orch.final_text(main), "all done")


class CompletionTests(unittest.TestCase):
    def state(self, main, subs=None):
        return orch.transcript_state(main, subs or {})

    def test_mid_tool_call_is_not_complete(self):
        main = [user_text("go"), asst("r1", stop="tool_use", blocks=[tool_use("t1")])]
        st = self.state(main)
        self.assertFalse(st["main_ended"])
        self.assertEqual(st["pending_tools"], ["t1"])
        self.assertFalse(orch.is_complete(st, idle_for_s=999))

    def test_end_turn_and_idle_is_complete(self):
        main = [user_text("go"), asst("r1", stop="tool_use", blocks=[tool_use("t1")]), user_result("t1"),
                asst("r2", stop="end_turn")]
        st = self.state(main)
        self.assertTrue(st["main_ended"])
        self.assertEqual(st["pending_tools"], [])
        self.assertFalse(orch.is_complete(st, idle_for_s=3, idle_s=20))
        self.assertTrue(orch.is_complete(st, idle_for_s=20, idle_s=20))

    def test_bookkeeping_records_after_the_last_message_do_not_hide_end_turn(self):
        main = [user_text("go"), asst("r1", stop="end_turn"),
                {"type": "last-prompt"}, {"type": "ai-title"}, {"type": "system"},
                {"type": "queue-operation", "operation": "dequeue"}]
        self.assertTrue(self.state(main)["main_ended"])

    def test_a_subagent_still_running_blocks_completion(self):
        main = [user_text("go"),
                asst("r1", stop="tool_use", blocks=[tool_use("t1", "Agent", subagent_type="worker")]),
                user_result("t1"), asst("r2", stop="end_turn")]
        running = [user_text("task"), asst("s1", stop="tool_use", blocks=[tool_use("u1")])]
        st = self.state(main, {"agent-a": running})
        self.assertEqual(st["subs_running"], ["agent-a"])
        self.assertFalse(orch.is_complete(st, idle_for_s=999))
        finished = running + [user_result("u1"), asst("s2", stop="end_turn")]
        st = self.state(main, {"agent-a": finished})
        self.assertEqual(st["subs_running"], [])
        self.assertTrue(orch.is_complete(st, idle_for_s=999))

    def test_a_subagent_that_wrote_nothing_yet_counts_as_running(self):
        st = self.state([asst("r1", stop="end_turn")], {"agent-a": []})
        self.assertEqual(st["subs_running"], ["agent-a"])

    def test_queued_notification_after_end_turn_means_not_done(self):
        # a background subagent's completion notification arrives after the parent's turn
        main = [user_text("go"), asst("r1", stop="end_turn"),
                {"type": "queue-operation", "operation": "enqueue", "content": "<task-notification>"}]
        self.assertFalse(self.state(main)["main_ended"])

    def test_user_message_after_end_turn_means_not_done(self):
        main = [asst("r1", stop="end_turn"), user_text("<task-notification>done</task-notification>")]
        self.assertFalse(self.state(main)["main_ended"])

    def test_empty_transcript_is_not_complete(self):
        st = self.state([])
        self.assertFalse(orch.is_complete(st, idle_for_s=999))


TRUST_NO_FIRST = """
 Do you trust the files in this folder?

 C:\\Users\\x\\repo

 ❯ 1. No, exit
   2. Yes, proceed

 Enter to confirm · Esc to cancel
"""
TRUST_YES_FIRST = TRUST_NO_FIRST.replace("❯ 1. No, exit\n   2. Yes, proceed", "❯ 1. Yes, proceed\n   2. No, exit")
PERMISSION = """
 Bash command
   python -m unittest
 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, and don't ask again
   3. No, and tell Claude what to do differently
"""
READY = """
╭──────────────────────────────╮
│ >                            │
╰──────────────────────────────╯
  ? for shortcuts
"""


# Screens recorded from claude 2.1.292 through the ConPTY (vanilla and zirv ctx wrap).
REAL_TRUST = """
 Accessing workspace:
 C:/Users/x/repo
 Quick safety check: Is this a project you created or one you trust? (Like your own code, a well-known open source project, or work from your team).
 Claude Code'll be able to read, edit, and execute files here.
 Security guide
 ❯ No, exit
   Yes, I trust this folder
 Enter to confirm · Esc to cancel
 zirv    claude   ✻ –   ◔ 48%·46%   ✉ –   ● supervised
"""
REAL_READY_WRAP = """
 ▐▛███▛█   Claude Code v2.1.292
▝▜██████▜▀  Haiku 4.5 · Claude Max
────────────
❯ Try "fix typecheck errors"
────────────
  repo | main | Haiku 4.5
  ⏵⏵ bypass permissions on (shift+tab to cycle) · ← for agents
"""


class DialogTests(unittest.TestCase):
    def test_real_trust_dialog_moves_down_to_yes(self):
        self.assertEqual(orch.detect_dialog(REAL_TRUST), [(orch.DOWN, 0.4), (orch.ENTER, 0.0)])

    def test_real_ready_screen_is_not_a_dialog_even_with_bypass_text(self):
        self.assertIsNone(orch.detect_dialog(REAL_READY_WRAP))
        self.assertTrue(orch.input_box_ready(REAL_READY_WRAP))

    def test_cursor_on_no_moves_down_then_enter(self):
        self.assertEqual(orch.detect_dialog(TRUST_NO_FIRST), [(orch.DOWN, 0.4), (orch.ENTER, 0.0)])

    def test_cursor_on_yes_is_just_enter(self):
        self.assertEqual(orch.detect_dialog(TRUST_YES_FIRST), [(orch.ENTER, 0.0)])
        self.assertEqual(orch.detect_dialog(PERMISSION), [(orch.ENTER, 0.0)])

    def test_input_box_is_not_a_dialog_and_is_ready(self):
        self.assertIsNone(orch.detect_dialog(READY))
        self.assertTrue(orch.input_box_ready(READY))
        self.assertFalse(orch.input_box_ready(TRUST_YES_FIRST))

    def test_plain_output_is_not_a_dialog(self):
        self.assertIsNone(orch.detect_dialog("Reading file cli.py\nEditing store.py\n"))

    def test_strip_ansi(self):
        self.assertEqual(orch.strip_ansi("\x1b[1mhello\x1b[0m \x1b]0;title\x07world"), "hello world")


class PromptTests(unittest.TestCase):
    def test_composition_numbers_every_source_prompt(self):
        root = Path(tempfile.mkdtemp())
        try:
            for name, body in (("ta", "Do A.\r\nline2"), ("tb", "Do B.")):
                (root / "tasks" / name).mkdir(parents=True)
                (root / "tasks" / name / "prompt.txt").write_text(body, encoding="utf-8", newline="")
            out = orch.compose_task_prompt(root, ["ta", "tb"])
        finally:
            shutil.rmtree(root, ignore_errors=True)
        self.assertIn("=== Part 1 of 2 ===\n\nDo A.\nline2", out)
        self.assertIn("=== Part 2 of 2 ===\n\nDo B.", out)
        self.assertLess(out.index("Part 1"), out.index("Part 2"))
        self.assertTrue(out.endswith("\n"))

    def test_committed_orch_prompts_match_their_sources(self):
        orch_tasks = [p for p in (BENCH / "tasks").iterdir()
                      if (p / "kind.txt").exists() and run_module.read_text(p / "kind.txt").strip() == "orch"]
        self.assertTrue(orch_tasks, "at least one kind=orch task must ship")
        for task_dir in orch_tasks:
            sources = orch.read_sources(task_dir)
            self.assertGreaterEqual(len(sources), 4, task_dir.name)
            self.assertEqual((task_dir / "prompt.txt").read_text(encoding="utf-8"),
                             orch.compose_task_prompt(BENCH, sources), task_dir.name)
            for src in sources:
                self.assertTrue((BENCH / "tasks" / src / "grade.py").exists(), src)

    def test_launch_prompt_prepends_the_noninteractive_note(self):
        self.assertTrue(orch.launch_prompt("X").startswith(run_module.NONINTERACTIVE_NOTE))
        self.assertTrue(orch.launch_prompt("X").endswith("X"))


class ArgvEnvTests(unittest.TestCase):
    def test_vanilla_argv_mirrors_run_py_minus_headless_flags(self):
        argv = orch.build_argv("vanilla", "opus", "C:/sp", claude_exe="claude")
        self.assertEqual(argv[:3], ["claude", "--model", "opus"])
        for flag in ("-p", "--output-format"):
            self.assertNotIn(flag, argv)
        self.assertEqual(argv[argv.index("--setting-sources") + 1], "project,local")
        self.assertEqual(argv[argv.index("--permission-mode") + 1], "dontAsk")
        self.assertIn(f"--allowedTools={run_module.VANILLA_ALLOWED_TOOLS}", argv)
        self.assertEqual(argv[argv.index("--plugin-dir") + 1], "C:/sp")

    def test_wrap_argv_and_never_allow_nested(self):
        argv = orch.build_argv("zirv-nojev", "opus", None, zirv_exe="zirv", claude_exe="claude")
        self.assertEqual(argv, ["zirv", "ctx", "wrap", "--force-pace", "--", "claude", "--model", "opus"])
        self.assertNotIn("--allow-nested", argv)

    def test_unknown_cond_raises(self):
        with self.assertRaises(ValueError):
            orch.build_argv("zirv-proxy", "opus", None)

    def test_env_scrubs_nesting_markers_and_headless_levers(self):
        base = {"PATH": "x", "CLAUDECODE": "1", "CLAUDE_CODE_ENTRYPOINT": "cli", "ZIRV_CTX_SESSION_ID": "s",
                "ZIRV_CTX_PARENT_SESSION": "p", "TYPESAFE_API_KEY": "k", "KEEP": "me",
                "ZIRV_CTX_SOCKET": "s", "ZIRV_CTX_DASH_REQUESTS": "d", "CLAUDE_PID": "1", "CLAUDE_EFFORT": "max"}
        env = orch.build_env("zirv-nojev", "C:/run", base=base)
        for gone in ("CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "ZIRV_CTX_SESSION_ID", "ZIRV_CTX_PARENT_SESSION",
                     "TYPESAFE_API_KEY", "ZIRV_CTX_SOCKET", "ZIRV_CTX_DASH_REQUESTS", "CLAUDE_PID",
                     "CLAUDE_EFFORT"):
            self.assertNotIn(gone, env)
        self.assertEqual(env["KEEP"], "me")
        for lever in run_module.ZIRV_HEADLESS_LEVERS:
            self.assertNotIn(lever, env)
        self.assertEqual(env["ZIRV_CTX_JEV_MEMORY"], "false")
        self.assertEqual(env["ZIRV_CTX_STATE_DIR"], str(Path("C:/run") / "zirv-state"))
        self.assertEqual(env["ZIRV_CTX_FALLBACK"], "false")

    def test_vanilla_env_gets_no_zirv_state_or_gates(self):
        env = orch.build_env("vanilla", "C:/run", base={"PATH": "x", "CLAUDECODE": "1"})
        self.assertNotIn("CLAUDECODE", env)
        self.assertFalse([k for k in env if k.startswith("ZIRV_CTX_JEV")])
        self.assertNotIn("ZIRV_CTX_STATE_DIR", env)


class ReportTests(unittest.TestCase):
    def results(self):
        def r(cond, wall, cost, score, err=False, q=0.5):
            return {"cond": cond, "wall_s": wall, "cost_usd": cost, "turns": 10, "subagents_spawned": 2,
                    "score": score, "quality_score": q, "is_error": err}
        return [r("vanilla", 100, 1.0, 0.5), r("vanilla", 300, 3.0, 1.0), r("vanilla", 900, 9.0, 0.0, err=True),
                r("zirv-nojev", 50, 0.5, 1.0, q=None)]

    def test_mean_and_median_per_cond_skip_errored_runs(self):
        rep = orch.build_report(self.results())
        self.assertEqual(rep["vanilla"]["n"], 2)
        self.assertAlmostEqual(rep["vanilla"]["wall_s"]["mean"], 200.0)
        self.assertAlmostEqual(rep["vanilla"]["cost_usd"]["median"], 2.0)
        self.assertAlmostEqual(rep["vanilla"]["score"]["mean"], 0.75)
        self.assertEqual(rep["zirv-nojev"]["n"], 1)
        self.assertIsNone(rep["zirv-nojev"]["quality_score"])

    def test_format_has_one_row_per_cond(self):
        text = orch.format_report(orch.build_report(self.results()))
        self.assertIn("vanilla", text)
        self.assertIn("zirv-nojev", text)
        self.assertIn("200.000 / 200.000", text)
        self.assertEqual(orch.format_report({}), "no finished orchestration runs")

    def test_load_results_reads_result_json(self):
        root = Path(tempfile.mkdtemp())
        try:
            d = root / "orch-runs" / "t__vanilla__r1"
            d.mkdir(parents=True)
            (d / "result.json").write_text(json.dumps({"cond": "vanilla", "is_error": False}), encoding="utf-8")
            (root / "orch-runs" / "bad").mkdir()
            (root / "orch-runs" / "bad" / "result.json").write_text("{", encoding="utf-8")
            got = orch.load_results(root)
        finally:
            shutil.rmtree(root, ignore_errors=True)
        self.assertEqual(got, [{"cond": "vanilla", "is_error": False}])


class TranscriptDiscoveryTests(unittest.TestCase):
    def test_subagent_files_and_analysis_sum_parent_and_subagents(self):
        home = Path(tempfile.mkdtemp())
        old_home, old_profile = os.environ.get("HOME"), os.environ.get("USERPROFILE")
        os.environ["HOME"] = os.environ["USERPROFILE"] = str(home)
        try:
            repo = home / "run" / "repo"
            repo.mkdir(parents=True)
            proj = home / ".claude" / "projects" / run_module.project_slug(repo)
            sess = "11111111-2222-3333-4444-555555555555"
            (proj / sess / "subagents").mkdir(parents=True)
            main = [user_text("go"),
                    asst("p1", stop="tool_use", blocks=[tool_use("t1", "Agent", subagent_type="zirv:worker",
                                                                 model="sonnet")]),
                    user_result("t1"), asst("p2", stop="end_turn", blocks=[{"type": "text", "text": "fin"}])]
            (proj / f"{sess}.jsonl").write_text("\n".join(json.dumps(x) for x in main), encoding="utf-8")
            sub = [user_text("task"), asst("s1", model="claude-sonnet-5", stop="end_turn")]
            sub_path = proj / sess / "subagents" / "agent-abc.jsonl"
            sub_path.write_text("\n".join(json.dumps(x) for x in sub), encoding="utf-8")
            sub_path.with_suffix(".meta.json").write_text(
                json.dumps({"agentType": "zirv:worker", "model": "sonnet", "toolUseId": "t1"}), encoding="utf-8")
            ana = orch.analyze_transcripts(repo)
        finally:
            for k, v in (("HOME", old_home), ("USERPROFILE", old_profile)):
                if v is None:
                    os.environ.pop(k, None)
                else:
                    os.environ[k] = v
            shutil.rmtree(home, ignore_errors=True)
        self.assertEqual(ana["session_ids"], [sess])
        self.assertEqual(ana["turns"], 2)
        self.assertEqual(ana["subagents_spawned"], 1)
        self.assertEqual(ana["subagents"][0]["type"], "zirv:worker")
        self.assertEqual(ana["subagents"][0]["models_used"], ["claude-sonnet-5"])
        self.assertEqual(sorted(ana["per_model"]), ["claude-opus-5", "claude-sonnet-5"])
        self.assertEqual(ana["final_text"], "fin")
        self.assertGreater(ana["cost_usd"], 0)


SP_CONTEXT = {"type": "attachment", "attachment": {"type": "hook_additional_context", "content": [
    "<EXTREMELY_IMPORTANT>\nYou have superpowers.\n\n**Below is the full content...**"]}}


def skill_call(name):
    return asst("sk" + name, blocks=[tool_use("tu" + name, "Skill", skill=name)])


class SuperpowersTests(unittest.TestCase):
    def test_loaded_and_skill_count(self):
        recs = [SP_CONTEXT, user_text("go"), skill_call("superpowers:brainstorming"),
                skill_call("superpowers:writing-plans"), skill_call("zirv:implement")]
        self.assertEqual(run_module.superpowers_in_records(recs), (True, 2))

    def test_missing_context_is_not_loaded(self):
        other = {"type": "attachment", "attachment": {"type": "hook_additional_context", "content": ["something else"]}}
        self.assertEqual(run_module.superpowers_in_records([other, user_text("go")]), (False, 0))
        # the text merely mentioned in a user message is not the SessionStart attachment
        self.assertEqual(run_module.superpowers_in_records([user_text("<EXTREMELY_IMPORTANT>\nYou have superpowers.")]),
                         (False, 0))

    def test_apply_marks_vanilla_invalid_and_leaves_zirv_alone(self):
        d = Path(tempfile.mkdtemp())
        old = run_module.VANILLA_PLUGIN_DIR
        run_module.VANILLA_PLUGIN_DIR = str(d)
        try:
            bad = d / "bad.jsonl"
            bad.write_text(json.dumps(user_text("go")), encoding="utf-8")
            good = d / "good.jsonl"
            good.write_text("\n".join(json.dumps(x) for x in [SP_CONTEXT, skill_call("superpowers:tdd")]),
                            encoding="utf-8")
            r = {"cond": "vanilla", "is_error": False}
            run_module.apply_superpowers_check(r, [bad])
            self.assertTrue(r["is_error"])
            self.assertFalse(r["superpowers_loaded"])
            self.assertEqual(r["invalid_reason"], "superpowers not loaded")
            r = {"cond": "vanilla", "is_error": False}
            run_module.apply_superpowers_check(r, [good])
            self.assertFalse(r["is_error"])
            self.assertEqual((r["superpowers_loaded"], r["superpowers_skills_invoked"]), (True, 1))
            z = {"cond": "zirv", "is_error": False}
            run_module.apply_superpowers_check(z, [bad])
            self.assertNotIn("superpowers_loaded", z)
        finally:
            run_module.VANILLA_PLUGIN_DIR = old
            shutil.rmtree(d, ignore_errors=True)

    def test_plugin_dir_must_exist(self):
        self.assertIn("required", run_module.check_vanilla_plugin_dir(None) + "required")
        self.assertIsNotNone(run_module.check_vanilla_plugin_dir(None))
        self.assertIsNotNone(run_module.check_vanilla_plugin_dir("C:/definitely/not/here"))
        self.assertIsNone(run_module.check_vanilla_plugin_dir(tempfile.gettempdir()))


if __name__ == "__main__":
    unittest.main()
