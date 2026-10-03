#!/usr/bin/env python3
"""Unit tests for run.py's autoresearch trial-mode additions (issues
#800-#805, Lane C). stdlib `unittest` only -- CONTRACT.md forbids pytest.

Runnable standalone (`python -m unittest docs/benchmarks/wrapped-vs-vanilla/test_trial.py`,
cwd anywhere) or via the repo-wide discovery command:
`python -m unittest discover -s docs/benchmarks -p "test_*.py"` from the
repo root. No test here calls a model provider, `claude`, `zirv ctx proxy`,
or any other billed command -- every function under test is pure, or
touches only a throwaway tempdir.
"""
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import run as run_module  # noqa: E402


class MergeSpecEnvTests(unittest.TestCase):
    def test_overlay_wins_on_conflict(self):
        base = {"A": "1", "B": "2"}
        overlay = {"B": "3", "C": "4"}
        merged = run_module.merge_spec_env(base, overlay)
        self.assertEqual(merged, {"A": "1", "B": "3", "C": "4"})

    def test_overlay_none_value_kept_as_removal_marker(self):
        # merge_spec_env itself just merges dicts (child_env is what turns a
        # None value into an actual removal) -- but the None must survive
        # the merge so child_env still sees it.
        merged = run_module.merge_spec_env({"A": "1"}, {"A": None})
        self.assertIsNone(merged["A"])

    def test_none_inputs_are_safe(self):
        self.assertEqual(run_module.merge_spec_env(None, None), {})
        self.assertEqual(run_module.merge_spec_env({"A": "1"}, None), {"A": "1"})
        self.assertEqual(run_module.merge_spec_env(None, {"A": "1"}), {"A": "1"})

    def test_base_dict_not_mutated(self):
        base = {"A": "1"}
        run_module.merge_spec_env(base, {"A": "2"})
        self.assertEqual(base, {"A": "1"})


class ReceiptBuildingTests(unittest.TestCase):
    def test_agent_receipt_shape_and_billing(self):
        r = run_module.agent_receipt_from_result(
            session="sess-1", cost_usd=0.42, model="claude-sonnet", receipt_id="agent:1",
            input_tokens=100, output_tokens=50)
        self.assertEqual(r["source"], "agent")
        self.assertEqual(r["session"], "sess-1")
        self.assertEqual(r["receipt_id"], "agent:1")
        self.assertFalse(r["cumulative"])
        self.assertEqual(r["reported_usd"], 0.42)
        self.assertEqual(r["billing"], "metered")
        self.assertEqual(r["input_tokens"], 100)
        self.assertEqual(r["output_tokens"], 50)
        self.assertFalse(r["cached"])

    def test_agent_receipt_unknown_cost_is_unknown_billing(self):
        r = run_module.agent_receipt_from_result(session="s", cost_usd=None, model="m")
        self.assertIsNone(r["reported_usd"])
        self.assertEqual(r["billing"], "unknown")

    def test_judge_receipt_from_result_never_cumulative(self):
        obj = {"session_id": "judge-sess", "total_cost_usd": 0.01, "model": "claude-opus",
               "usage": {"input_tokens": 10, "output_tokens": 5}}
        r = run_module.judge_receipt_from_result(obj, source="judge")
        self.assertEqual(r["source"], "judge")
        self.assertFalse(r["cumulative"])
        self.assertEqual(r["reported_usd"], 0.01)
        self.assertEqual(r["model"], "claude-opus")
        self.assertEqual(r["input_tokens"], 10)

    def test_judge_receipt_from_none_obj_is_safe(self):
        r = run_module.judge_receipt_from_result(None)
        self.assertIsNone(r["reported_usd"])
        self.assertEqual(r["billing"], "unknown")


def _base_single_result(**overrides):
    result = {
        "task": "t02_pagination", "cond": "zirv-proxy", "rep": 1, "model": "sonnet",
        "wall_s": 12.0, "session_id": "sess-a", "agent_cost_usd": 0.5, "model_used": "sonnet",
        "input_tokens": 100, "output_tokens": 40,
        "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
        "score": 1.0, "quality_score": 0.8, "judge_score": None,
        "judge_receipts": [], "quality_receipts": [],
    }
    result.update(overrides)
    return result


class ReceiptsFromResultTests(unittest.TestCase):
    def test_single_shot_agent_plus_quality_receipt(self):
        result = _base_single_result(quality_receipts=[
            {"source": "judge", "session": "q1", "receipt_id": None, "cumulative": False,
             "reported_usd": 0.02, "model": "opus", "input_tokens": 1, "output_tokens": 1,
             "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
             "cached": False, "billing": "metered"},
        ])
        receipts = run_module.receipts_from_result(result)
        sources = [r["source"] for r in receipts]
        self.assertEqual(sources, ["agent", "judge"])
        # every receipt got a fresh, non-empty id
        ids = [r["receipt_id"] for r in receipts]
        self.assertEqual(len(ids), len(set(ids)))
        self.assertTrue(all(ids))
        self.assertEqual(receipts[0]["reported_usd"], 0.5)
        self.assertFalse(receipts[0]["cumulative"])

    def test_single_shot_no_session_no_cost_produces_no_agent_receipt(self):
        result = _base_single_result(session_id=None, agent_cost_usd=None)
        receipts = run_module.receipts_from_result(result)
        self.assertEqual([r["source"] for r in receipts], [])

    def test_single_shot_timeout_still_emits_an_unknown_agent_receipt(self):
        # Review finding P1: a killed run has session_id/agent_cost_usd both
        # None, but real (unknown-amount) spend may already have happened
        # before the kill -- it must never vanish from receipts.jsonl.
        result = _base_single_result(session_id=None, agent_cost_usd=None, timed_out=True)
        receipts = run_module.receipts_from_result(result)
        self.assertEqual([r["source"] for r in receipts], ["agent"])
        self.assertIsNone(receipts[0]["reported_usd"])
        self.assertEqual(receipts[0]["billing"], "unknown")
        self.assertIsNone(receipts[0]["session"])

    def test_chain_one_agent_receipt_per_step_plus_quality(self):
        result = {
            "kind": "chain", "model_used": "sonnet", "wall_s": 30.0,
            "steps": [
                {"session_id": "s1", "cost_usd": 0.10, "input_tokens": 50, "output_tokens": 20,
                 "judge_receipts": []},
                {"session_id": "s1", "cost_usd": 0.07, "input_tokens": 30, "output_tokens": 15,
                 "judge_receipts": [{"source": "judge", "session": "s1", "receipt_id": None,
                                      "cumulative": False, "reported_usd": 0.005, "model": "sonnet",
                                      "input_tokens": 1, "output_tokens": 1,
                                      "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
                                      "cached": False, "billing": "metered"}]},
            ],
            "quality_receipts": [{"source": "judge", "session": None, "receipt_id": None,
                                   "cumulative": False, "reported_usd": 0.03, "model": "opus",
                                   "input_tokens": 1, "output_tokens": 1,
                                   "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
                                   "cached": False, "billing": "metered"}],
        }
        receipts = run_module.receipts_from_result(result)
        sources = [r["source"] for r in receipts]
        # step1 agent, step2 agent, step2's judge, then the one chain-level quality judge
        self.assertEqual(sources, ["agent", "agent", "judge", "judge"])
        # Chain step receipts are already delta-corrected by the chain
        # stepper (do_one_chain_run) before they ever reach here -- never
        # cumulative.
        self.assertTrue(all(not r["cumulative"] for r in receipts if r["source"] == "agent"))
        self.assertAlmostEqual(receipts[0]["reported_usd"], 0.10)
        self.assertAlmostEqual(receipts[1]["reported_usd"], 0.07)

    def test_chain_step_killed_by_timeout_still_emits_an_unknown_agent_receipt(self):
        # Review finding P2: a chain step killed by timeout has
        # session_id=None and (after the P2 fix) cost_usd=None too -- it
        # must still produce a receipt, and that receipt must be unknown,
        # never a confirmed $0.00.
        result = {
            "kind": "chain", "model_used": "sonnet", "wall_s": 30.0,
            "steps": [
                {"session_id": "s1", "cost_usd": 0.10, "input_tokens": 50, "output_tokens": 20,
                 "judge_receipts": []},
                {"session_id": None, "cost_usd": None, "input_tokens": 0, "output_tokens": 0,
                 "judge_receipts": [], "timed_out": True},
            ],
            "quality_receipts": [],
        }
        receipts = run_module.receipts_from_result(result)
        agent_receipts = [r for r in receipts if r["source"] == "agent"]
        self.assertEqual(len(agent_receipts), 2)
        self.assertAlmostEqual(agent_receipts[0]["reported_usd"], 0.10)
        self.assertIsNone(agent_receipts[1]["reported_usd"])
        self.assertEqual(agent_receipts[1]["billing"], "unknown")

    def test_chain_step_not_reached_and_not_timed_out_emits_no_receipt(self):
        # A step never even attempted (e.g. the chain broke on an earlier
        # step) must not fabricate a receipt for it.
        result = {
            "kind": "chain", "model_used": "sonnet", "wall_s": 1.0,
            "steps": [
                {"session_id": None, "cost_usd": None, "input_tokens": 0, "output_tokens": 0,
                 "judge_receipts": [], "timed_out": False},
            ],
            "quality_receipts": [],
        }
        receipts = run_module.receipts_from_result(result)
        self.assertEqual(receipts, [])

    def test_escalate_two_attempts_two_agent_receipts(self):
        result = {
            "model_used": "opus", "wall_s": 40.0, "escalated": True,
            "attempts": [
                {"model": "sonnet", "session_id": "att1", "cost_usd": 0.20},
                {"model": "opus", "session_id": "att2", "cost_usd": 0.55},
            ],
            "judge_receipts": [], "quality_receipts": [],
        }
        receipts = run_module.receipts_from_result(result)
        agent_receipts = [r for r in receipts if r["source"] == "agent"]
        self.assertEqual(len(agent_receipts), 2)
        self.assertEqual(agent_receipts[0]["model"], "sonnet")
        self.assertEqual(agent_receipts[1]["model"], "opus")
        self.assertEqual(agent_receipts[0]["reported_usd"], 0.20)
        self.assertEqual(agent_receipts[1]["reported_usd"], 0.55)

    def test_escalate_attempt_killed_by_timeout_still_emits_an_unknown_agent_receipt(self):
        # Review finding P1: run_escalate_trial's do_attempt already leaves
        # a timed-out attempt's session_id/cost_usd at None but DOES append
        # it to result["attempts"] with timed_out=True -- receipts_from_
        # result must not drop it just because both are None.
        result = {
            "model_used": "sonnet", "wall_s": 20.0, "escalated": False,
            "attempts": [
                {"model": "sonnet", "session_id": None, "cost_usd": None, "timed_out": True},
            ],
            "judge_receipts": [], "quality_receipts": [],
        }
        receipts = run_module.receipts_from_result(result)
        agent_receipts = [r for r in receipts if r["source"] == "agent"]
        self.assertEqual(len(agent_receipts), 1)
        self.assertIsNone(agent_receipts[0]["reported_usd"])
        self.assertEqual(agent_receipts[0]["billing"], "unknown")
        self.assertEqual(agent_receipts[0]["model"], "sonnet")


class MapResultToTrialTests(unittest.TestCase):
    def _spend(self):
        return {"schema": 1, "execution": {}, "overhead": {}, "by_source": {}, "calls": 0,
                "cached_calls": 0, "duplicates_dropped": 0, "completeness": "partial",
                "billing": "unknown", "tokens": {}, "receipts": {}}

    def test_tests_kind_maps_score_and_quality(self):
        result = {"score": 0.75, "quality_score": 0.6, "wall_s": 10.0}
        trial = run_module.map_result_to_trial(
            result, "tests", "trial-1", "ok", self._spend(), {"harness": "claude", "model": "sonnet"},
            "fp0000000000000")
        self.assertEqual(trial["correctness"], 0.75)
        self.assertEqual(trial["quality"], 0.6)
        self.assertEqual(trial["status"], "ok")
        self.assertEqual(trial["wall_ms"], 10000)
        self.assertEqual(trial["schema"], 1)

    def test_answer_kind_quality_is_null(self):
        result = {"score": 1.0, "quality_score": 0.9, "wall_s": 1.0}
        trial = run_module.map_result_to_trial(
            result, "answer", "t", "ok", self._spend(), {}, "fp")
        self.assertEqual(trial["correctness"], 1.0)
        self.assertIsNone(trial["quality"])

    def test_judge_kind_divides_raw_score_by_ten(self):
        result = {"judge_score": 7.0, "wall_s": 1.0}
        trial = run_module.map_result_to_trial(
            result, "judge", "t", "ok", self._spend(), {}, "fp")
        self.assertAlmostEqual(trial["correctness"], 0.7)
        self.assertIsNone(trial["quality"])

    def test_judge_kind_missing_score_is_null_not_zero(self):
        result = {"judge_score": None, "wall_s": 1.0}
        trial = run_module.map_result_to_trial(
            result, "judge", "t", "error", self._spend(), {}, "fp")
        self.assertIsNone(trial["correctness"])

    def test_chain_kind_uses_mean_step_score_and_chain_quality(self):
        result = {"score": 0.83, "quality_score": 0.71, "wall_s": 100.0}
        trial = run_module.map_result_to_trial(
            result, "chain", "t", "ok", self._spend(), {}, "fp")
        self.assertEqual(trial["correctness"], 0.83)
        self.assertEqual(trial["quality"], 0.71)

    def test_escalated_flag_carried_through(self):
        result = {"score": 1.0, "wall_s": 1.0, "escalated": True, "escalate_reason": "error"}
        trial = run_module.map_result_to_trial(
            result, "tests", "t", "ok", self._spend(), {}, "fp")
        self.assertTrue(trial["escalated"])
        self.assertEqual(trial["escalate_reason"], "error")


class FallbackSpendReportTests(unittest.TestCase):
    def test_never_reports_unknown_as_zero(self):
        # Even with every agent/judge receipt carrying a reported_usd, the
        # fallback still counts execution.unknown_count >= 1: this Python
        # process never sees intake/Jev decision spend, so "we know the
        # full execution cost" is never a true claim it's allowed to make.
        receipts = [
            {"source": "agent", "receipt_id": "a1", "reported_usd": 0.5, "billing": "metered",
             "cached": False, "input_tokens": 10, "output_tokens": 5,
             "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0},
        ]
        report = run_module.fallback_spend_report(receipts)
        self.assertEqual(report["completeness"], "partial")
        self.assertGreaterEqual(report["execution"]["unknown_count"], 1)

    def test_empty_receipts_still_partial_and_unknown(self):
        report = run_module.fallback_spend_report([])
        self.assertEqual(report["completeness"], "partial")
        self.assertGreaterEqual(report["execution"]["unknown_count"], 1)
        self.assertEqual(report["calls"], 0)

    def test_judge_receipts_go_to_overhead_not_execution(self):
        receipts = [
            {"source": "agent", "receipt_id": "a1", "reported_usd": 1.0, "billing": "metered",
             "cached": False},
            {"source": "judge", "receipt_id": "j1", "reported_usd": 0.1, "billing": "metered",
             "cached": False},
        ]
        report = run_module.fallback_spend_report(receipts)
        self.assertAlmostEqual(report["execution"]["reported_usd"], 1.0)
        self.assertAlmostEqual(report["overhead"]["reported_usd"], 0.1)

    def test_duplicate_receipt_ids_are_dropped_and_counted(self):
        receipts = [
            {"source": "agent", "receipt_id": "a1", "reported_usd": 1.0, "billing": "metered"},
            {"source": "agent", "receipt_id": "a1", "reported_usd": 1.0, "billing": "metered"},
        ]
        report = run_module.fallback_spend_report(receipts)
        self.assertEqual(report["calls"], 1)
        self.assertEqual(report["duplicates_dropped"], 1)

    def test_receipts_breakdown_counts_by_source(self):
        receipts = [
            {"source": "agent", "receipt_id": "a1", "reported_usd": 1.0, "billing": "metered"},
            {"source": "judge", "receipt_id": "j1", "reported_usd": 0.1, "billing": "metered"},
            {"source": "judge", "receipt_id": "j2", "reported_usd": 0.1, "billing": "metered"},
        ]
        report = run_module.fallback_spend_report(receipts)
        self.assertEqual(report["receipts"], {"agent": 1, "judge": 2})


class EscalateTriggerTests(unittest.TestCase):
    def test_triggers_on_error(self):
        self.assertTrue(run_module.escalate_should_trigger(True, []))

    def test_does_not_trigger_on_baseline_failure_alone(self):
        self.assertFalse(run_module.escalate_should_trigger(
            False, {"test_regex_rule_case_insensitive"}))

    def test_triggers_on_a_new_visible_failure(self):
        self.assertTrue(run_module.escalate_should_trigger(
            False, {"test_regex_rule_case_insensitive", "test_something_else"}))

    def test_triggers_on_new_failure_even_without_baseline_one(self):
        self.assertTrue(run_module.escalate_should_trigger(False, {"test_something_else"}))

    def test_no_failures_does_not_trigger(self):
        self.assertFalse(run_module.escalate_should_trigger(False, set()))


class CleanupHiddenTestsTests(unittest.TestCase):
    def test_removes_existing_tests_hidden_dir(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            hidden = repo / "tests_hidden"
            hidden.mkdir()
            (hidden / "test_x.py").write_text("# hidden test\n", encoding="utf-8")
            self.assertTrue(hidden.exists())
            run_module.cleanup_hidden_tests(repo)
            self.assertFalse(hidden.exists())

    def test_safe_when_absent(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)
            run_module.cleanup_hidden_tests(repo)  # must not raise
            self.assertFalse((repo / "tests_hidden").exists())


class EnvFingerprintTests(unittest.TestCase):
    def test_deterministic_for_same_inputs(self):
        fp1 = run_module.compute_env_fingerprint("claude 1.2.3", "zirv 4.5.6", "3.11.0", ["a", "b"])
        fp2 = run_module.compute_env_fingerprint("claude 1.2.3", "zirv 4.5.6", "3.11.0", ["b", "a"])
        self.assertEqual(fp1, fp2)  # dir name order must not matter (sorted internally)
        self.assertEqual(len(fp1), 16)

    def test_sensitive_to_a_changed_input(self):
        fp1 = run_module.compute_env_fingerprint("claude 1.2.3", "zirv 4.5.6", "3.11.0", [])
        fp2 = run_module.compute_env_fingerprint("claude 1.2.4", "zirv 4.5.6", "3.11.0", [])
        self.assertNotEqual(fp1, fp2)


class CorpusTomlTests(unittest.TestCase):
    def setUp(self):
        self.bench_root = Path(run_module.__file__).resolve().parent
        self.corpus_path = self.bench_root / "corpus.toml"

    def test_corpus_toml_exists_and_loads(self):
        self.assertTrue(self.corpus_path.exists(), "corpus.toml must exist")
        corpus = run_module.load_corpus_toml(self.corpus_path)
        self.assertEqual(corpus.get("schema"), 1)
        self.assertTrue(corpus.get("version"))

    def test_every_task_dir_present_exactly_once_with_valid_split(self):
        corpus = run_module.load_corpus_toml(self.corpus_path)
        tasks_dir = self.bench_root / "tasks"
        all_task_ids = sorted(p.name for p in tasks_dir.iterdir() if p.is_dir())
        problems = run_module.validate_corpus(corpus, all_task_ids)
        self.assertEqual(problems, [], "\n".join(problems))

    def test_validate_corpus_catches_missing_task(self):
        corpus = {"schema": 1, "version": "1", "task": []}
        problems = run_module.validate_corpus(corpus, ["t01_tiebreak"])
        self.assertTrue(any("missing tasks" in p for p in problems))

    def test_validate_corpus_catches_duplicate_id(self):
        corpus = {"schema": 1, "version": "1", "task": [
            {"id": "t01_tiebreak", "family": "f", "class": "mechanical", "split": "dev", "kind": "answer"},
            {"id": "t01_tiebreak", "family": "f", "class": "mechanical", "split": "dev", "kind": "answer"},
        ]}
        problems = run_module.validate_corpus(corpus, ["t01_tiebreak"])
        self.assertTrue(any("appears 2 times" in p for p in problems))

    def test_validate_corpus_catches_invalid_split_and_class(self):
        corpus = {"schema": 1, "version": "1", "task": [
            {"id": "t01_tiebreak", "family": "f", "class": "bogus", "split": "prod", "kind": "answer"},
        ]}
        problems = run_module.validate_corpus(corpus, ["t01_tiebreak"])
        self.assertTrue(any("invalid split" in p for p in problems))
        self.assertTrue(any("invalid class" in p for p in problems))


class CheckGradersSyntheticTaskTests(unittest.TestCase):
    """#801's grader self-check, exercised on a tiny synthetic task instead
    of the real ledgerlite template -- no provider call, no dependency on
    the real corpus."""

    def _make_template(self, root):
        template = root / "template"
        template.mkdir()
        (template / "pkg.py").write_text("def add(a, b):\n    return a - b  # bug: should be a + b\n",
                                          encoding="utf-8")
        (template / "tests").mkdir()
        (template / "tests" / "__init__.py").write_text("", encoding="utf-8")
        subprocess.run([run_module.GIT_EXE, "init", "-q"], cwd=str(template), check=True)
        subprocess.run([run_module.GIT_EXE, "config", "user.email", "t@example.com"],
                        cwd=str(template), check=True)
        subprocess.run([run_module.GIT_EXE, "config", "user.name", "t"], cwd=str(template), check=True)
        subprocess.run([run_module.GIT_EXE, "add", "-A"], cwd=str(template), check=True)
        subprocess.run([run_module.GIT_EXE, "commit", "-q", "-m", "init"], cwd=str(template), check=True)
        return template

    def _make_task(self, root, template, good_patch):
        tasks_dir = root / "tasks"
        tasks_dir.mkdir()
        task_dir = tasks_dir / "tsynth"
        task_dir.mkdir()
        (task_dir / "kind.txt").write_text("tests", encoding="utf-8")
        (task_dir / "prompt.txt").write_text("fix add()\n", encoding="utf-8")
        hidden = task_dir / "hidden"
        hidden.mkdir()
        (hidden / "test_add.py").write_text(
            "import unittest\nfrom pkg import add\n\n"
            "class T(unittest.TestCase):\n"
            "    def test_add(self):\n"
            "        self.assertEqual(add(2, 3), 5)\n",
            encoding="utf-8")
        grade_py = task_dir / "grade.py"
        grade_py.write_text(
            "import json, shutil, subprocess, sys\n"
            "from pathlib import Path\n"
            "def main():\n"
            "    repo = Path(sys.argv[1]).resolve()\n"
            "    hidden_dst = repo / 'tests_hidden'\n"
            "    if hidden_dst.exists():\n"
            "        shutil.rmtree(hidden_dst)\n"
            "    hidden_dst.mkdir()\n"
            "    (hidden_dst / '__init__.py').write_text('')\n"
            "    src = Path(__file__).resolve().parent / 'hidden' / 'test_add.py'\n"
            "    (hidden_dst / 'test_add.py').write_text(src.read_text())\n"
            "    proc = subprocess.run([sys.executable, '-m', 'unittest', 'discover', '-s', "
            "'tests_hidden', '-t', str(repo)], cwd=str(repo), capture_output=True, text=True)\n"
            "    out = proc.stdout + proc.stderr\n"
            "    total = out.count(' ... ')\n"
            "    passed = 0 if 'FAILED' in out or 'ERROR' in out else total\n"
            "    if 'Ran 1 test' in out and 'FAILED' not in out and 'ERROR' not in out:\n"
            "        passed, total = 1, 1\n"
            "    elif 'Ran 1 test' in out:\n"
            "        passed, total = 0, 1\n"
            "    print(json.dumps({'score': (passed / total) if total else 0.0, "
            "'passed': passed, 'total': total, 'visible_ok': True, 'details': ''}))\n"
            "main()\n",
            encoding="utf-8")
        (task_dir / "reference.patch").write_text(good_patch, encoding="utf-8")
        return task_dir

    def _good_patch_text(self, template):
        # Build a real, valid patch by editing a copy and diffing.
        work = template.parent / "work_for_patch"
        shutil.copytree(template, work)
        (work / "pkg.py").write_text("def add(a, b):\n    return a + b\n", encoding="utf-8")
        proc = subprocess.run([run_module.GIT_EXE, "diff", "--no-color"], cwd=str(work),
                               capture_output=True, text=True)
        shutil.rmtree(work, onerror=run_module._rmtree_onerror)
        return proc.stdout

    def test_good_reference_patch_and_untouched_template(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            template = self._make_template(root)
            patch_text = self._good_patch_text(template)
            self._make_task(root, template, patch_text)
            rows, ok = run_module.check_graders(root, tasks=["tsynth"])
        self.assertEqual(len(rows), 1)
        self.assertTrue(ok, rows[0].get("note"))
        self.assertAlmostEqual(rows[0]["ref_score"], 1.0)
        self.assertLess(rows[0]["template_score"], 1.0)

    def test_broken_reference_patch_is_reported_not_swallowed(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            template = self._make_template(root)
            # A patch that doesn't actually fix the bug: grader must catch it.
            broken_patch = "--- a/pkg.py\n+++ b/pkg.py\n@@ -1,2 +1,2 @@\n def add(a, b):\n-    return a - b  # bug: should be a + b\n+    return a - b  # still buggy\n"
            self._make_task(root, template, broken_patch)
            rows, ok = run_module.check_graders(root, tasks=["tsynth"])
        self.assertEqual(len(rows), 1)
        self.assertFalse(ok)
        self.assertIn("expected full", rows[0]["note"])


class FakeCompletedProc:
    """Stands in for the `subprocess.Popen` object `launch()` normally
    returns -- `wait_run` only ever calls `.wait(timeout=...)` and reads
    `.pid` on a timeout path, both satisfied here without spawning
    anything."""
    pid = 999999

    def wait(self, timeout=None):
        return 0


class RunSingleTrialIntegrationTests(unittest.TestCase):
    """Exercises `do_one_run` through the real `--trial` code path
    (`run_dir_override`/`spec_env`, receipts_from_result, map_result_to_trial)
    with ONLY `launch` (the one function that would otherwise spawn `claude`)
    replaced by a fake that writes a canned `-p --output-format json` result
    -- everything else (template copy, `git status`, the task's own
    `grade.py`) runs for real, exactly as `--check-graders` already does.
    Uses `t01_tiebreak` (kind=answer) and `cond="zirv"` specifically because
    neither path calls a judge, so nothing here can reach a provider.
    """

    def setUp(self):
        self._orig_launch = run_module.launch

    def tearDown(self):
        run_module.launch = self._orig_launch

    def _fake_launch(self, cond, model, prompt_text, prompt_path, cwd, stdout_path, stderr_path,
                      env_extra=None, resume_session_id=None):
        canned = {
            "session_id": "sess-fake-1",
            "total_cost_usd": 0.1234,
            "usage": {"input_tokens": 100, "output_tokens": 20},
            "result": (
                "The tie-break happens in rules.py's categorize function, decided by "
                "priority: the earliest rule in list order wins."
            ),
            "is_error": False,
            "duration_ms": 1000,
            "duration_api_ms": 800,
            "num_turns": 3,
        }
        Path(stdout_path).write_text(json.dumps(canned), encoding="utf-8")
        Path(stderr_path).write_text("", encoding="utf-8")
        self.last_env_extra = env_extra
        return FakeCompletedProc(), ["fake", "argv"], None, None, None

    def test_trial_mode_end_to_end_with_launch_faked(self):
        run_module.launch = self._fake_launch
        real_bench_root = Path(run_module.__file__).resolve().parent

        with tempfile.TemporaryDirectory() as tmp:
            # do_one_run refuses to copy a dirty `template/` (git status
            # --porcelain) -- this checked-in `template/` has no `.git` of
            # its own (see CONTRACT.md: the real deployment copies this
            # whole directory tree out and `git init`s template/ there), so
            # `git -C template status` would otherwise resolve to this
            # OUTER repo's own (often dirty, mid-PR) status. Build a fake
            # bench_root with its own freshly-committed template/ copy,
            # exactly what an operator does before ever running run.py.
            bench_root = Path(tmp) / "bench_root"
            shutil.copytree(real_bench_root / "tasks" / "t01_tiebreak",
                             bench_root / "tasks" / "t01_tiebreak")
            shutil.copytree(real_bench_root / "template", bench_root / "template")
            subprocess.run([run_module.GIT_EXE, "init", "-q"], cwd=str(bench_root / "template"), check=True)
            subprocess.run([run_module.GIT_EXE, "config", "user.email", "t@example.com"],
                            cwd=str(bench_root / "template"), check=True)
            subprocess.run([run_module.GIT_EXE, "config", "user.name", "t"],
                            cwd=str(bench_root / "template"), check=True)
            subprocess.run([run_module.GIT_EXE, "add", "-A"], cwd=str(bench_root / "template"), check=True)
            subprocess.run([run_module.GIT_EXE, "commit", "-q", "-m", "init"],
                            cwd=str(bench_root / "template"), check=True)

            out_dir = Path(tmp) / "trial-out"
            spec_env = {"ZIRV_CTX_JEV_MEMORY": "true"}
            result = run_module.do_one_run(
                bench_root, "t01_tiebreak", "zirv", 1, "sonnet", 1200.0,
                resume=False, k=1, total=1, run_dir_override=out_dir, spec_env=spec_env)

            self.assertFalse(result["is_error"])
            self.assertEqual(result["session_id"], "sess-fake-1")
            self.assertEqual(result["agent_cost_usd"], 0.1234)
            self.assertGreater(result["score"], 0.0)
            # spec_env reached cond_env_for's merge (zirv cond always sets
            # the headless levers too, so this just checks our key is IN
            # there, not that it's the only one).
            self.assertEqual(self.last_env_extra.get("ZIRV_CTX_JEV_MEMORY"), "true")
            # run_dir_override was honoured: trial output landed at out_dir,
            # not the grid's own runs/<task>__<cond>__r<rep> naming.
            self.assertTrue((out_dir / "result.json").exists())
            self.assertTrue((out_dir / "repo").is_dir())

            receipts = run_module.receipts_from_result(result)
            self.assertEqual(len(receipts), 1)
            self.assertEqual(receipts[0]["source"], "agent")
            self.assertEqual(receipts[0]["reported_usd"], 0.1234)

            spend = run_module.fallback_spend_report(receipts)
            self.assertEqual(spend["completeness"], "partial")

            trial = run_module.map_result_to_trial(
                result, "answer", "trial-1", "ok", spend,
                {"harness": "claude", "model": "sonnet"}, "0" * 16)
            self.assertEqual(trial["correctness"], result["score"])
            self.assertIsNone(trial["quality"])


def _jsonl(path, rows):
    path.write_text("".join(json.dumps(r) + "\n" for r in rows), encoding="utf-8")


class JevTelemetryTests(unittest.TestCase):
    def test_nojev_forces_every_gate_off_and_drops_credential(self):
        env = run_module.cond_env_for(run_module.NOJEV_COND)
        self.assertTrue(all(env[run_module.jev_env_var(g)] == "false"
                            for g in run_module.JEV_GATE_KEYS))
        self.assertIsNone(env[run_module.JEV_CREDENTIAL_ENV])

    def test_full_arm_skips_inert_gates_and_sets_effort_tier(self):
        env = run_module.cond_env_for(run_module.JEV_FULL_COND)
        self.assertEqual(env["ZIRV_CTX_HEADLESS_EFFORT_TRIVIAL"], "low")
        self.assertEqual(env["ZIRV_CTX_JEV_GATES"], "false")
        self.assertEqual(env["ZIRV_CTX_JEV_APPROVE"], "false")
        self.assertEqual(env["ZIRV_CTX_JEV_MEMORY"], "true")
        self.assertNotIn("zirv-jev-gates", run_module.JEV_GATE_CONDS)

    def test_overlay_gate_over_nojev_restores_credential(self):
        base = run_module.cond_env_for(run_module.NOJEV_COND)
        merged = run_module.merge_spec_env(base, {"ZIRV_CTX_JEV_MEMORY": "true"})
        self.assertNotIn(run_module.JEV_CREDENTIAL_ENV, merged)
        self.assertEqual(merged["ZIRV_CTX_JEV_DISPATCH"], "false")

    def test_isolate_state_sets_run_local_dir_for_zirv_only(self):
        saved = run_module.os.environ.pop("ZIRV_CTX_STATE_DIR", None)
        try:
            env = run_module.isolate_state({}, "/r", "zirv-nojev")
            self.assertEqual(Path(env["ZIRV_CTX_STATE_DIR"]), Path("/r") / "zirv-state")
            self.assertEqual(run_module.isolate_state({}, "/r", "vanilla"), {})
        finally:
            if saved is not None:
                run_module.os.environ["ZIRV_CTX_STATE_DIR"] = saved

    def test_telemetry_counts_sites_live_cached_errors_effects(self):
        with tempfile.TemporaryDirectory() as tmp:
            state = Path(tmp)
            _jsonl(state / "jev-decisions.jsonl", [
                {"site": "intake", "cached": False, "fallbacks": [],
                 "usage": {"input_tokens": 100, "output_tokens": 5}},
                {"site": "memory", "cached": True, "fallbacks": [], "usage": {}},
                {"site": "memory", "cached": False, "fallbacks": ["boom"], "usage": {}},
            ])
            _jsonl(state / "jev-effects.jsonl", [{"site": "memory", "action": "trim"}])
            jev = run_module.jev_telemetry(state)
        self.assertEqual(jev["calls_by_site"], {"intake": 1, "memory": 2})
        self.assertEqual((jev["live"], jev["cached"], jev["errors"]), (1, 1, 1))
        self.assertEqual(jev["fallbacks"], {"boom": 1})
        self.assertEqual(jev["effects_by_site"], {"memory": 1})
        self.assertEqual(jev["non_intake_served"], 1)
        self.assertEqual(jev["spend"]["input_tokens"], 100)

    def test_telemetry_without_logs_is_none_not_zero_spend(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertIsNone(run_module.jev_telemetry(tmp))

    def test_validity_rules(self):
        intake_only = {"calls_by_site": {"intake": 1}, "non_intake_served": 0}
        served = {"calls_by_site": {"intake": 1, "memory": 1}, "non_intake_served": 1}
        self.assertIsNotNone(run_module.jev_validity("zirv-jev-full", intake_only, "typesafe"))
        self.assertIsNotNone(run_module.jev_validity("zirv-jev-full", None, "typesafe"))
        self.assertIsNone(run_module.jev_validity("zirv-jev-full", served, "typesafe"))
        self.assertIsNotNone(run_module.jev_validity("zirv-nojev", served, "deterministic"))
        self.assertIsNotNone(run_module.jev_validity("zirv-nojev", None, "typesafe"))
        self.assertIsNone(run_module.jev_validity("zirv-nojev", None, "deterministic"))
        self.assertIsNone(run_module.jev_validity("vanilla", None, None))

    def test_write_result_attaches_block_and_copies_logs(self):
        saved = run_module.os.environ.pop("ZIRV_CTX_STATE_DIR", None)
        try:
            with tempfile.TemporaryDirectory() as tmp:
                run_dir = Path(tmp)
                (run_dir / "zirv-state").mkdir()
                _jsonl(run_dir / "zirv-state" / "jev-decisions.jsonl",
                       [{"site": "memory", "cached": False, "fallbacks": [], "usage": {}}])
                result = {"cond": "zirv-nojev", "proxy": {"decider": "deterministic"}}
                run_module.write_result(run_dir, result)
                written = json.loads((run_dir / "result.json").read_text(encoding="utf-8"))
                self.assertEqual(written["jev"]["calls_by_site"], {"memory": 1})
                self.assertIn("Jev decisions", written["jev_invalid"])
                self.assertTrue((run_dir / "jev-decisions.jsonl").exists())
        finally:
            if saved is not None:
                run_module.os.environ["ZIRV_CTX_STATE_DIR"] = saved


class JevReviewFixTests(unittest.TestCase):
    def test_grid_isolation_overrides_inherited_state_dir(self):
        saved = run_module.os.environ.get("ZIRV_CTX_STATE_DIR")
        run_module.os.environ["ZIRV_CTX_STATE_DIR"] = "/operator/state"
        run_module.TRIAL_SHARED_STATE = False
        try:
            env = run_module.isolate_state({}, "/r", "zirv-jev-full")
            self.assertEqual(Path(env["ZIRV_CTX_STATE_DIR"]), Path("/r") / "zirv-state")
            with tempfile.TemporaryDirectory() as tmp:
                result = {"cond": "zirv-jev-full", "proxy": {}}
                run_module.attach_jev_telemetry(tmp, result)
                self.assertIsNotNone(result["jev_invalid"])
                self.assertIsNone(result["jev"])
        finally:
            if saved is None:
                run_module.os.environ.pop("ZIRV_CTX_STATE_DIR", None)
            else:
                run_module.os.environ["ZIRV_CTX_STATE_DIR"] = saved

    def test_shared_trial_dir_without_trial_id_is_invalid_not_valid(self):
        saved = (run_module.TRIAL_SHARED_STATE, run_module.os.environ.get("ZIRV_CTX_STATE_DIR"),
                 run_module.os.environ.pop("ZIRV_ATTR_TRIAL", None))
        run_module.TRIAL_SHARED_STATE = True
        run_module.os.environ["ZIRV_CTX_STATE_DIR"] = "/shared"
        try:
            result = {"cond": "zirv-nojev", "proxy": {"decider": "deterministic"}}
            run_module.attach_jev_telemetry("/nowhere", result)
            self.assertIn("unreadable", result["jev_invalid"])
            self.assertEqual(run_module.isolate_state({}, "/r", "zirv"), {})
        finally:
            run_module.TRIAL_SHARED_STATE = saved[0]
            if saved[1] is None:
                run_module.os.environ.pop("ZIRV_CTX_STATE_DIR", None)
            else:
                run_module.os.environ["ZIRV_CTX_STATE_DIR"] = saved[1]
            if saved[2] is not None:
                run_module.os.environ["ZIRV_ATTR_TRIAL"] = saved[2]

    def test_nojev_disables_typesafe_through_zirv_config_env(self):
        env = run_module.cond_env_for(run_module.NOJEV_COND)
        self.assertEqual(env["ZIRV_CTX_PROXY_DECIDER"], "deterministic")
        self.assertNotEqual(env["ZIRV_CTX_PROXY_TYPESAFE_CREDENTIAL_ENV"],
                            run_module.JEV_CREDENTIAL_ENV)
        merged = run_module.merge_spec_env(env, {"ZIRV_CTX_JEV_MEMORY": "true"})
        self.assertNotIn("ZIRV_CTX_PROXY_DECIDER", merged)
        self.assertNotIn("ZIRV_CTX_PROXY_TYPESAFE_CREDENTIAL_ENV", merged)

    def test_gate_keys_match_rust_env_table(self):
        import re
        source = (Path(__file__).resolve().parents[3] / "src/commands/ctx/config/repo_layer.rs"
                  ).read_text(encoding="utf-8")
        table = re.findall(
            r'"ZIRV_CTX_JEV_([A-Z_]+)",\s*&\["jev",\s*"(\w+)"\],\s*EnvKind::Bool', source)
        self.assertTrue(table)
        self.assertTrue(all(env == key.upper() for env, key in table))
        self.assertEqual(sorted(k for _, k in table), sorted(run_module.JEV_GATE_KEYS))
        self.assertIn("retry", run_module.JEV_GATE_KEYS)


class JevGateMirrorTests(unittest.TestCase):
    def test_aggregate_mirrors_run_py_gate_lists(self):
        import aggregate as aggregate_module
        self.assertEqual(aggregate_module.JEV_GATE_KEYS, run_module.JEV_GATE_KEYS)
        self.assertEqual(aggregate_module.JEV_INERT_GATES, run_module.JEV_INERT_GATES)
        self.assertEqual(aggregate_module.JEV_ABLATION_CONDS, run_module.JEV_ABLATION_CONDS)
        self.assertEqual(aggregate_module.CANONICAL_CONDS, run_module.CANONICAL_CONDS)


class CompareJevTests(unittest.TestCase):
    def test_jev_table_shows_site_columns_and_decider_mix(self):
        import compare as compare_module
        rows = [
            {"cond": "zirv-nojev", "proxy": {"decider": "deterministic"}, "jev": None},
            {"cond": "zirv-jev-full", "proxy": {"decider": "typesafe"},
             "jev": {"calls_by_site": {"memory": 3}, "effects_by_site": {"memory": 1}}},
        ]
        table = "\n".join(compare_module.jev_table(rows, ["zirv-nojev", "zirv-jev-full"]))
        self.assertIn("| memory | 0.0 / 0.0 | 3.0 / 1.0 |", table)
        self.assertIn("deterministic x1", table)
        self.assertIn("typesafe x1", table)


class PosixExecutableTests(unittest.TestCase):
    @unittest.skipIf(sys.platform == "win32", "POSIX resolution only")
    def test_resolved_executables_are_not_windows_paths(self):
        for exe in (run_module.CLAUDE_EXE, run_module.ZIRV_FALLBACK,
                    run_module.PYTHON_EXE, run_module.GIT_EXE, run_module.zirv_exe()):
            self.assertNotIn("C:\\", exe)
        self.assertEqual(run_module.resolve_exe("no-such-tool-xyz", r"C:\x.exe"), "no-such-tool-xyz")


if __name__ == "__main__":
    unittest.main()
