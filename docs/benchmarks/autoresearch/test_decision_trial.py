#!/usr/bin/env python3
"""Unit tests for decision_trial.py (issue #803). stdlib `unittest` only.

Every test here stubs `zirv ctx proxy`'s output as a plain Python dict --
none calls a provider, none invokes `zirv` at all. Runnable standalone or
via `python -m unittest discover -s docs/benchmarks -p "test_*.py"` from
the repo root.
"""
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).resolve().parent))
import decision_trial  # noqa: E402


def decision(seat_tier="standard", needs_clarification=0.0, decisive=False,
             decider="typesafe", model="sonnet"):
    return {
        "seat_tier": seat_tier,
        "needs_clarification": needs_clarification,
        "needs_clarification_decisive": decisive,
        "decider": decider,
        "orchestrator": {"harness": "claude", "model": model},
    }


def label(seat_tier="standard", clarify=False, tags=None, weight=1.0):
    return {"seat_tier": seat_tier, "clarify": clarify, "tags": tags or [], "weight": weight}


class DecisionClarifyTests(unittest.TestCase):
    def test_clarify_true_when_confident_and_above_threshold(self):
        d = decision(needs_clarification=0.8, decisive=True)
        self.assertTrue(decision_trial.decision_clarify(d))

    def test_clarify_false_when_not_decisive(self):
        d = decision(needs_clarification=0.9, decisive=False)
        self.assertFalse(decision_trial.decision_clarify(d))

    def test_clarify_false_below_threshold(self):
        d = decision(needs_clarification=0.2, decisive=True)
        self.assertFalse(decision_trial.decision_clarify(d))

    def test_clarify_false_when_fields_missing(self):
        self.assertFalse(decision_trial.decision_clarify({"seat_tier": "standard"}))

    def test_clarify_false_for_none(self):
        self.assertFalse(decision_trial.decision_clarify(None))


class DecisionAbstainedTests(unittest.TestCase):
    def test_not_abstained_for_typesafe(self):
        self.assertFalse(decision_trial.decision_abstained(decision(decider="typesafe")))

    def test_not_abstained_for_helper(self):
        self.assertFalse(decision_trial.decision_abstained(decision(decider="helper")))

    def test_abstained_for_deterministic(self):
        self.assertTrue(decision_trial.decision_abstained(decision(decider="deterministic")))

    def test_abstained_for_missing_decider(self):
        self.assertTrue(decision_trial.decision_abstained({"seat_tier": "standard"}))

    def test_abstained_for_none(self):
        self.assertTrue(decision_trial.decision_abstained(None))


class GradeDecisionTests(unittest.TestCase):
    def test_perfect_match_scores_full(self):
        d = decision(seat_tier="standard", needs_clarification=0.0, decisive=False)
        lbl = label(seat_tier="standard", clarify=False)
        correctness, details = decision_trial.grade_decision(d, lbl)
        self.assertEqual(correctness, 1.0)
        self.assertFalse(details["false_escalation"])
        self.assertFalse(details["unsafe_under_selection"])
        self.assertFalse(details["unnecessary_clarification"])

    def test_tier_mismatch_only_scores_half(self):
        d = decision(seat_tier="deep", needs_clarification=0.0, decisive=False)
        lbl = label(seat_tier="standard", clarify=False)
        correctness, details = decision_trial.grade_decision(d, lbl)
        self.assertEqual(correctness, 0.5)

    def test_clarify_mismatch_only_scores_half(self):
        d = decision(seat_tier="standard", needs_clarification=0.9, decisive=True)
        lbl = label(seat_tier="standard", clarify=False)
        correctness, details = decision_trial.grade_decision(d, lbl)
        self.assertEqual(correctness, 0.5)
        self.assertTrue(details["unnecessary_clarification"])

    def test_both_wrong_scores_zero(self):
        d = decision(seat_tier="cheap", needs_clarification=0.9, decisive=True)
        lbl = label(seat_tier="frontier", clarify=False)
        correctness, _details = decision_trial.grade_decision(d, lbl)
        self.assertEqual(correctness, 0.0)

    def test_false_escalation_when_predicted_tier_above_label(self):
        d = decision(seat_tier="frontier")
        lbl = label(seat_tier="cheap")
        _correctness, details = decision_trial.grade_decision(d, lbl)
        self.assertTrue(details["false_escalation"])
        self.assertFalse(details["unsafe_under_selection"])

    def test_unsafe_under_selection_on_costly_error_when_tier_too_low(self):
        d = decision(seat_tier="cheap")
        lbl = label(seat_tier="deep", tags=["costly_error"])
        _correctness, details = decision_trial.grade_decision(d, lbl)
        self.assertTrue(details["unsafe_under_selection"])
        self.assertFalse(details["false_escalation"])

    def test_tier_too_low_without_costly_error_tag_is_not_unsafe(self):
        # A plain wrong-but-not-flagged-as-costly case is a tier mismatch,
        # not specifically "unsafe under selection" -- that label is
        # reserved for cases where under-provisioning is known to be
        # dangerous (the corpus's costly_error tag).
        d = decision(seat_tier="cheap")
        lbl = label(seat_tier="deep", tags=["ambiguous"])
        _correctness, details = decision_trial.grade_decision(d, lbl)
        self.assertFalse(details["unsafe_under_selection"])

    def test_none_decision_scores_zero_and_is_abstained(self):
        lbl = label(seat_tier="standard", tags=["costly_error"])
        correctness, details = decision_trial.grade_decision(None, lbl)
        self.assertEqual(correctness, 0.0)
        self.assertTrue(details["abstained"])
        self.assertTrue(details["unsafe_under_selection"])

    def test_weight_is_exposed_not_folded_into_correctness(self):
        d = decision(seat_tier="deep")
        lbl = label(seat_tier="standard", weight=2.0)
        correctness, details = decision_trial.grade_decision(d, lbl)
        self.assertEqual(correctness, 0.5)  # not scaled by weight
        self.assertEqual(details["weight"], 2.0)

    def test_abstained_flag_reflects_decider(self):
        d = decision(seat_tier="standard", decider="deterministic")
        lbl = label(seat_tier="standard", clarify=False)
        _correctness, details = decision_trial.grade_decision(d, lbl)
        self.assertTrue(details["abstained"])


class LoadJsonlTests(unittest.TestCase):
    def test_loads_keyed_by_id(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "x.jsonl"
            path.write_text('{"id": "a", "v": 1}\n{"id": "b", "v": 2}\n', encoding="utf-8")
            rows = decision_trial.load_jsonl(path)
            self.assertEqual(rows["a"]["v"], 1)
            self.assertEqual(rows["b"]["v"], 2)

    def test_skips_blank_lines(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "x.jsonl"
            path.write_text('{"id": "a"}\n\n\n{"id": "b"}\n', encoding="utf-8")
            rows = decision_trial.load_jsonl(path)
            self.assertEqual(set(rows), {"a", "b"})


class DecisionCasesCorpusTests(unittest.TestCase):
    """Sanity checks over the real, committed decision-cases corpus --
    no provider call, pure file loading."""

    def setUp(self):
        self.cases_dir = decision_trial.CASES_DIR

    def test_every_input_has_a_matching_label_and_vice_versa(self):
        inputs = decision_trial.load_jsonl(self.cases_dir / "inputs.jsonl")
        labels = decision_trial.load_jsonl(self.cases_dir / "labels.jsonl")
        self.assertEqual(set(inputs), set(labels))
        self.assertGreaterEqual(len(inputs), 36)

    def test_every_case_has_a_valid_seat_tier_and_prompt(self):
        inputs = decision_trial.load_jsonl(self.cases_dir / "inputs.jsonl")
        labels = decision_trial.load_jsonl(self.cases_dir / "labels.jsonl")
        for cid, row in labels.items():
            self.assertIn(row["seat_tier"], decision_trial.SEAT_TIER_RANK)
            self.assertIsInstance(row["clarify"], bool)
            self.assertTrue(row["tags"], f"{cid} has no tags")
            self.assertTrue(inputs[cid]["prompt"].strip(), f"{cid} has an empty prompt")

    def test_every_tag_represented_in_every_split(self):
        import tomllib

        corpus = tomllib.loads((self.cases_dir / "corpus.toml").read_text(encoding="utf-8"))
        labels = decision_trial.load_jsonl(self.cases_dir / "labels.jsonl")
        split_by_id = {t["id"]: t["split"] for t in corpus["task"]}
        tags_by_split = {}
        for cid, row in labels.items():
            split = split_by_id[cid]
            tags_by_split.setdefault(split, set()).update(row["tags"])
        all_tags = {"ambiguous", "misleading_metadata", "insufficient_facts", "costly_error"}
        for split, tags in tags_by_split.items():
            missing = all_tags - tags
            self.assertFalse(missing, f"split {split!r} is missing tags {missing}")

    def test_corpus_toml_every_case_present_exactly_once(self):
        import tomllib

        corpus = tomllib.loads((self.cases_dir / "corpus.toml").read_text(encoding="utf-8"))
        ids = [t["id"] for t in corpus["task"]]
        self.assertEqual(len(ids), len(set(ids)))
        labels = decision_trial.load_jsonl(self.cases_dir / "labels.jsonl")
        self.assertEqual(set(ids), set(labels))


class RunTrialStubbedTests(unittest.TestCase):
    """`run_trial`'s own orchestration, with `call_proxy_decision` monkey-
    patched out -- proves spec parsing, case lookup, and trial.json/
    details.json writing without ever invoking `zirv`."""

    def setUp(self):
        self._orig_call_proxy = decision_trial.call_proxy_decision
        self._orig_spend = decision_trial.call_spend_command

    def tearDown(self):
        decision_trial.call_proxy_decision = self._orig_call_proxy
        decision_trial.call_spend_command = self._orig_spend

    def test_run_trial_writes_trial_json_from_stubbed_decision(self):
        decision_trial.call_proxy_decision = (
            lambda prompt, zirv_bin, state_dir, env_extra, timeout_s=120, attribution=None: (
                decision(seat_tier="standard", needs_clarification=0.9, decisive=True),
                42, "", None,
            )
        )
        decision_trial.call_spend_command = lambda *a, **k: None

        with tempfile.TemporaryDirectory() as tmp:
            spec_path = Path(tmp) / "spec.json"
            spec = {
                "schema": 1, "campaign": "c1", "candidate": "baseline", "trial_id": "t1",
                "task": "ic001",  # an ambiguous case, label clarify=True
                "rep": 1, "split": "dev", "stage": "screen",
                "route": {"harness": "claude", "model": None},
                "env": {}, "state_dir": str(Path(tmp) / "state"),
                "timeout_secs": 30, "zirv_dir": None, "strategy": None,
                "cache_mode": "cold", "pressure": "natural",
            }
            spec_path.write_text(json.dumps(spec), encoding="utf-8")
            out_dir = Path(tmp) / "out"

            trial = decision_trial.run_trial(str(spec_path), str(out_dir))

            self.assertEqual(trial["status"], "ok")
            self.assertEqual(trial["schema"], 1)
            self.assertIn(trial["correctness"], (0.5, 1.0))
            self.assertIsNone(trial["quality"])
            self.assertTrue((out_dir / "trial.json").exists())
            self.assertTrue((out_dir / "details.json").exists())
            details = json.loads((out_dir / "details.json").read_text(encoding="utf-8"))
            self.assertIn("latency_ms", details)

    def test_run_trial_unknown_case_raises(self):
        with tempfile.TemporaryDirectory() as tmp:
            spec_path = Path(tmp) / "spec.json"
            spec = {"task": "not-a-real-case", "trial_id": "t1", "campaign": "c1",
                    "state_dir": str(Path(tmp) / "state")}
            spec_path.write_text(json.dumps(spec), encoding="utf-8")
            with self.assertRaises(ValueError):
                decision_trial.run_trial(str(spec_path), str(Path(tmp) / "out"))

    def test_run_trial_missing_state_dir_raises(self):
        # A spec with no state_dir (or an empty one) must never let the
        # child zirv fall back to the operator's real state dir.
        with tempfile.TemporaryDirectory() as tmp:
            spec_path = Path(tmp) / "spec.json"
            spec = {"task": "ic001", "trial_id": "t1", "campaign": "c1"}
            spec_path.write_text(json.dumps(spec), encoding="utf-8")
            with self.assertRaises(ValueError):
                decision_trial.run_trial(str(spec_path), str(Path(tmp) / "out"))

    def test_run_trial_empty_state_dir_raises(self):
        with tempfile.TemporaryDirectory() as tmp:
            spec_path = Path(tmp) / "spec.json"
            spec = {"task": "ic001", "trial_id": "t1", "campaign": "c1", "state_dir": ""}
            spec_path.write_text(json.dumps(spec), encoding="utf-8")
            with self.assertRaises(ValueError):
                decision_trial.run_trial(str(spec_path), str(Path(tmp) / "out"))

    def test_run_trial_never_mutates_os_environ_for_attribution(self):
        # Review finding (P4): run_trial used to set os.environ["ZIRV_ATTR_*"]
        # directly and never cleared it -- a later trial in the same process
        # (or this test suite's own later tests) could inherit a previous
        # trial's attribution. Assert the keys are absent from os.environ
        # both before and after run_trial, proving they were never set there
        # at all (build_child_env carries them into the subprocess call
        # instead).
        attr_keys = ["ZIRV_ATTR_CAMPAIGN", "ZIRV_ATTR_CANDIDATE", "ZIRV_ATTR_TRIAL", "ZIRV_ATTR_TASK"]
        for k in attr_keys:
            self.assertNotIn(k, os.environ, f"{k} leaked into os.environ before this test even ran")

        captured = {}

        def fake_call_proxy_decision(prompt, zirv_bin, state_dir, env_extra, timeout_s=120,
                                      attribution=None):
            captured["attribution"] = attribution
            captured["os_environ_snapshot"] = {k: os.environ.get(k) for k in attr_keys}
            return decision(seat_tier="standard"), 1, "", None

        decision_trial.call_proxy_decision = fake_call_proxy_decision
        decision_trial.call_spend_command = lambda *a, **k: None

        with tempfile.TemporaryDirectory() as tmp:
            spec_path = Path(tmp) / "spec.json"
            spec = {
                "campaign": "c1", "candidate": "cand-x", "trial_id": "t-attr-1", "task": "ic001",
                "state_dir": str(Path(tmp) / "state"),
            }
            spec_path.write_text(json.dumps(spec), encoding="utf-8")
            decision_trial.run_trial(str(spec_path), str(Path(tmp) / "out"))

        # The subprocess call DID receive the attribution values...
        self.assertEqual(captured["attribution"]["ZIRV_ATTR_TRIAL"], "t-attr-1")
        self.assertEqual(captured["attribution"]["ZIRV_ATTR_CAMPAIGN"], "c1")
        # ...but the parent process's own os.environ was never touched to
        # deliver them (both during the call and after run_trial returns).
        for k in attr_keys:
            self.assertIsNone(captured["os_environ_snapshot"][k])
            self.assertNotIn(k, os.environ)

    def test_build_child_env_removes_none_and_applies_env_extra_last(self):
        os.environ["ZIRV_ATTR_TASK_TEST_PROBE_UNSET"] = "should-be-removed"
        try:
            env = decision_trial.build_child_env(
                state_dir="/tmp/some-state",
                attribution={"ZIRV_ATTR_TASK_TEST_PROBE_UNSET": None, "ZIRV_ATTR_TRIAL": "t1"},
                env_extra={"ZIRV_ATTR_TRIAL": "overridden-by-candidate-env"},
            )
        finally:
            os.environ.pop("ZIRV_ATTR_TASK_TEST_PROBE_UNSET", None)
        self.assertNotIn("ZIRV_ATTR_TASK_TEST_PROBE_UNSET", env)
        self.assertEqual(env["ZIRV_ATTR_TRIAL"], "overridden-by-candidate-env")
        self.assertEqual(env["ZIRV_CTX_STATE_DIR"], "/tmp/some-state")
        # The real os.environ is untouched by build_child_env itself.
        self.assertNotIn("ZIRV_ATTR_TASK_TEST_PROBE_UNSET", os.environ)

    def test_attribution_env_for_maps_spec_fields(self):
        spec = {"campaign": "c1", "candidate": "cand-1", "trial_id": "t1", "task": "ic001"}
        attribution = decision_trial.attribution_env_for(spec)
        self.assertEqual(attribution, {
            "ZIRV_ATTR_CAMPAIGN": "c1", "ZIRV_ATTR_CANDIDATE": "cand-1",
            "ZIRV_ATTR_TRIAL": "t1", "ZIRV_ATTR_TASK": "ic001",
        })

    def test_attribution_env_for_missing_fields_are_none(self):
        attribution = decision_trial.attribution_env_for({})
        self.assertEqual(attribution, {
            "ZIRV_ATTR_CAMPAIGN": None, "ZIRV_ATTR_CANDIDATE": None,
            "ZIRV_ATTR_TRIAL": None, "ZIRV_ATTR_TASK": None,
        })


class CallProxyDecisionTests(unittest.TestCase):
    """`subprocess.run` itself stubbed -- proves `call_proxy_decision`'s exit
    code handling without invoking `zirv`."""

    def setUp(self):
        self._orig_run = decision_trial.subprocess.run

    def tearDown(self):
        decision_trial.subprocess.run = self._orig_run

    def test_nonzero_exit_with_parsable_stdout_is_treated_as_failed(self):
        # Review finding: a non-zero exit must be a failed call even when
        # stdout happens to parse as JSON (e.g. a partial/stale decision
        # printed before a crash).
        stdout = json.dumps(decision(seat_tier="standard")).encode("utf-8")

        def fake_run(argv, capture_output, timeout, env):
            return SimpleNamespace(stdout=stdout, stderr=b"boom", returncode=1)

        decision_trial.subprocess.run = fake_run
        result_decision, _elapsed_ms, _raw, error_note = decision_trial.call_proxy_decision(
            "prompt", "zirv", None, {})
        self.assertIsNone(result_decision)
        self.assertIsNotNone(error_note)
        self.assertIn("exit=1", error_note)

    def test_zero_exit_with_parsable_stdout_succeeds(self):
        stdout = json.dumps(decision(seat_tier="standard")).encode("utf-8")

        def fake_run(argv, capture_output, timeout, env):
            return SimpleNamespace(stdout=stdout, stderr=b"", returncode=0)

        decision_trial.subprocess.run = fake_run
        result_decision, _elapsed_ms, _raw, error_note = decision_trial.call_proxy_decision(
            "prompt", "zirv", None, {})
        self.assertIsNotNone(result_decision)
        self.assertIsNone(error_note)


class JevRanFromProxyDecisionsTests(unittest.TestCase):
    """Issue #803 review (P3): `jev_ran` must come from the production's own
    persisted `proxy-decisions.jsonl` receipt, distinguishing "Jev never ran"
    (missing credential / gate off -- an empty file) from "Jev ran but
    abstained" (a row present, `decider: "deterministic"`)."""

    def test_true_for_typesafe(self):
        self.assertTrue(decision_trial.jev_ran_from_proxy_decisions(
            [{"decider": "typesafe"}]))

    def test_true_for_helper(self):
        self.assertTrue(decision_trial.jev_ran_from_proxy_decisions(
            [{"decider": "helper"}]))

    def test_false_for_deterministic_only(self):
        self.assertFalse(decision_trial.jev_ran_from_proxy_decisions(
            [{"decider": "deterministic"}]))

    def test_false_when_no_rows_at_all(self):
        # This is the "Jev never ran" case the review specifically called
        # out -- no receipt at all, not merely a deterministic one.
        self.assertFalse(decision_trial.jev_ran_from_proxy_decisions([]))

    def test_uses_the_last_row(self):
        rows = [{"decider": "deterministic"}, {"decider": "typesafe"}]
        self.assertTrue(decision_trial.jev_ran_from_proxy_decisions(rows))
        rows2 = [{"decider": "typesafe"}, {"decider": "deterministic"}]
        self.assertFalse(decision_trial.jev_ran_from_proxy_decisions(rows2))

    def test_k1_behaviour_unchanged_by_reps_kwarg(self):
        rows = [{"decider": "deterministic"}, {"decider": "typesafe"}]
        self.assertTrue(decision_trial.jev_ran_from_proxy_decisions(rows, reps=1))
        rows2 = [{"decider": "typesafe"}, {"decider": "deterministic"}]
        self.assertFalse(decision_trial.jev_ran_from_proxy_decisions(rows2, reps=1))

    def test_reps_k_true_only_when_last_k_rows_all_decisive(self):
        # Review finding: for K reps, jev_ran must reflect the WHOLE trial --
        # one deterministic-only rep among several decisive ones must not
        # read as "Jev ran" for the trial.
        rows = [{"decider": "typesafe"}, {"decider": "typesafe"}, {"decider": "deterministic"}]
        self.assertFalse(decision_trial.jev_ran_from_proxy_decisions(rows, reps=3))
        rows_all_decisive = [{"decider": "typesafe"}, {"decider": "helper"}, {"decider": "typesafe"}]
        self.assertTrue(decision_trial.jev_ran_from_proxy_decisions(rows_all_decisive, reps=3))

    def test_reps_k_uses_all_rows_when_fewer_than_k_present(self):
        rows = [{"decider": "typesafe"}, {"decider": "helper"}]
        self.assertTrue(decision_trial.jev_ran_from_proxy_decisions(rows, reps=5))
        rows2 = [{"decider": "typesafe"}, {"decider": "deterministic"}]
        self.assertFalse(decision_trial.jev_ran_from_proxy_decisions(rows2, reps=5))


class ReadProxyDecisionsTests(unittest.TestCase):
    def test_missing_file_returns_empty_list(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(decision_trial.read_proxy_decisions(tmp), [])

    def test_missing_state_dir_returns_empty_list(self):
        self.assertEqual(decision_trial.read_proxy_decisions(None), [])

    def test_reads_jsonl_rows_in_order(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "proxy-decisions.jsonl"
            path.write_text(
                '{"decider": "deterministic"}\n\n{"decider": "typesafe"}\n',
                encoding="utf-8",
            )
            rows = decision_trial.read_proxy_decisions(tmp)
            self.assertEqual([r["decider"] for r in rows], ["deterministic", "typesafe"])

    def test_malformed_line_is_skipped_not_raised(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "proxy-decisions.jsonl"
            path.write_text('{"decider": "typesafe"}\nnot json\n', encoding="utf-8")
            rows = decision_trial.read_proxy_decisions(tmp)
            self.assertEqual(len(rows), 1)


class RunTrialJevRanIntegrationTests(unittest.TestCase):
    """`run_trial` end to end (proxy call stubbed) with a REAL
    `proxy-decisions.jsonl` file on disk under the trial's state dir --
    proves `details.json["jev_ran"]` is read from that file, not from the
    stubbed decision object, and that a deterministic-only (or missing)
    receipt never nulls `correctness`."""

    def setUp(self):
        self._orig_call_proxy = decision_trial.call_proxy_decision
        self._orig_spend = decision_trial.call_spend_command
        decision_trial.call_spend_command = lambda *a, **k: None

    def tearDown(self):
        decision_trial.call_proxy_decision = self._orig_call_proxy
        decision_trial.call_spend_command = self._orig_spend

    def _run(self, tmp, proxy_decisions_lines):
        state_dir = Path(tmp) / "state"
        state_dir.mkdir(parents=True, exist_ok=True)
        if proxy_decisions_lines is not None:
            (state_dir / "proxy-decisions.jsonl").write_text(
                "\n".join(json.dumps(r) for r in proxy_decisions_lines) + "\n",
                encoding="utf-8",
            )
        decision_trial.call_proxy_decision = (
            lambda prompt, zirv_bin, sd, env_extra, timeout_s=120, attribution=None: (
                decision(seat_tier="standard", needs_clarification=0.9, decisive=True),
                10, "", None,
            )
        )
        spec_path = Path(tmp) / "spec.json"
        spec = {"campaign": "c1", "candidate": "cand-1", "trial_id": "t1", "task": "ic001",
                "state_dir": str(state_dir)}
        spec_path.write_text(json.dumps(spec), encoding="utf-8")
        out_dir = Path(tmp) / "out"
        trial = decision_trial.run_trial(str(spec_path), str(out_dir))
        details = json.loads((out_dir / "details.json").read_text(encoding="utf-8"))
        return trial, details

    def test_jev_ran_true_when_receipt_says_typesafe(self):
        with tempfile.TemporaryDirectory() as tmp:
            trial, details = self._run(tmp, [{"decider": "typesafe"}])
        self.assertTrue(details["jev_ran"])
        self.assertIsNotNone(trial["correctness"])

    def test_jev_ran_false_and_correctness_not_nulled_when_no_receipt(self):
        with tempfile.TemporaryDirectory() as tmp:
            trial, details = self._run(tmp, None)  # no proxy-decisions.jsonl at all
        self.assertFalse(details["jev_ran"])
        self.assertIsNotNone(trial["correctness"])

    def test_jev_ran_false_and_correctness_not_nulled_when_deterministic(self):
        with tempfile.TemporaryDirectory() as tmp:
            trial, details = self._run(tmp, [{"decider": "deterministic"}])
        self.assertFalse(details["jev_ran"])
        self.assertIsNotNone(trial["correctness"])


class RunTrialRepsTests(unittest.TestCase):
    """`--reps K` (Jev determinism tuning): K=1 must stay byte-identical to
    the original single-call behaviour; K>1 folds K intake calls into one
    trial with a mean correctness and a modal-share quality."""

    def setUp(self):
        self._orig_call_proxy = decision_trial.call_proxy_decision
        self._orig_spend = decision_trial.call_spend_command
        decision_trial.call_spend_command = lambda *a, **k: None

    def tearDown(self):
        decision_trial.call_proxy_decision = self._orig_call_proxy
        decision_trial.call_spend_command = self._orig_spend

    def _spec(self, tmp):
        spec_path = Path(tmp) / "spec.json"
        spec = {"campaign": "c1", "candidate": "cand-1", "trial_id": "t1", "task": "ic001",
                "state_dir": str(Path(tmp) / "state")}
        spec_path.write_text(json.dumps(spec), encoding="utf-8")
        return spec_path

    def test_reps_1_output_is_byte_identical_to_default(self):
        decision_trial.call_proxy_decision = (
            lambda prompt, zirv_bin, state_dir, env_extra, timeout_s=120, attribution=None: (
                decision(seat_tier="standard", needs_clarification=0.9, decisive=True),
                42, "", None,
            )
        )
        with tempfile.TemporaryDirectory() as tmp1, tempfile.TemporaryDirectory() as tmp2:
            spec1 = self._spec(tmp1)
            out1 = Path(tmp1) / "out"
            decision_trial.run_trial(str(spec1), str(out1))  # default reps
            spec2 = self._spec(tmp2)
            out2 = Path(tmp2) / "out"
            decision_trial.run_trial(str(spec2), str(out2), reps=1)  # explicit reps=1

            trial1 = (out1 / "trial.json").read_text(encoding="utf-8")
            trial2 = (out2 / "trial.json").read_text(encoding="utf-8")
            # trial_id differs only via spec's own trial_id (same here); strip
            # nothing -- both specs are identical apart from tmp dir paths
            # baked into spend/state, which fallback_spend_report doesn't carry.
            self.assertEqual(trial1, trial2)
            details1 = (out1 / "details.json").read_text(encoding="utf-8")
            details2 = (out2 / "details.json").read_text(encoding="utf-8")
            self.assertEqual(details1, details2)

    def test_reps_greater_than_1_averages_correctness_and_computes_modal_quality(self):
        calls = {"n": 0}

        def fake_call(prompt, zirv_bin, state_dir, env_extra, timeout_s=120, attribution=None):
            calls["n"] += 1
            # First two reps agree (standard, clarify), third disagrees (deep).
            if calls["n"] <= 2:
                return decision(seat_tier="standard", needs_clarification=0.9, decisive=True), 10, "", None
            return decision(seat_tier="deep", needs_clarification=0.9, decisive=True), 10, "", None

        decision_trial.call_proxy_decision = fake_call

        with tempfile.TemporaryDirectory() as tmp:
            spec_path = self._spec(tmp)
            out_dir = Path(tmp) / "out"
            trial = decision_trial.run_trial(str(spec_path), str(out_dir), reps=3)

            self.assertEqual(trial["status"], "ok")
            self.assertEqual(calls["n"], 3)
            # ic001's label seat_tier/clarify (see decision-cases/labels.jsonl):
            # correctness is the mean of the 3 reps' own grade_decision scores.
            self.assertIsNotNone(trial["correctness"])
            # 2 of 3 reps share the same (seat_tier, clarify) tuple -> modal share 2/3.
            self.assertAlmostEqual(trial["quality"], 2 / 3)
            details = json.loads((out_dir / "details.json").read_text(encoding="utf-8"))
            self.assertEqual(len(details["reps"]), 3)
            self.assertIn("jev_ran", details)

    def test_reps_failed_reps_never_count_as_agreeing_in_quality(self):
        # Review finding: quality (stability) must be computed only over
        # reps whose decision is not None. Two of three reps fail here; if
        # failed reps counted as "agreeing" (both grade to (None, None)),
        # they'd form the modal tuple and quality would read 2/3. The only
        # real decision must be the whole modal share: 1/1 = 1.0.
        calls = {"n": 0}

        def fake_call(prompt, zirv_bin, state_dir, env_extra, timeout_s=120, attribution=None):
            calls["n"] += 1
            if calls["n"] == 1:
                return decision(seat_tier="standard", needs_clarification=0.9, decisive=True), 10, "", None
            return None, 5, "", "proxy call failed"

        decision_trial.call_proxy_decision = fake_call

        with tempfile.TemporaryDirectory() as tmp:
            spec_path = self._spec(tmp)
            out_dir = Path(tmp) / "out"
            trial = decision_trial.run_trial(str(spec_path), str(out_dir), reps=3)

            self.assertEqual(trial["status"], "ok")
            self.assertIsNotNone(trial["correctness"])
            self.assertAlmostEqual(trial["quality"], 1.0)

    def test_quality_sees_a_field_the_old_seat_tier_clarify_metric_could_not(self):
        # Issue: the old K>1 quality metric was the modal share of just
        # (seat_tier, clarify) -- it would have read quality=1.0 here, since
        # every rep agrees on both. But `intent` (a real production-acted
        # decision.rs field -- see ACTED_DECISION_FIELDS) flips on the third
        # rep; the full acted-decision tuple must see that instability.
        calls = {"n": 0}

        def fake_call(prompt, zirv_bin, state_dir, env_extra, timeout_s=120, attribution=None):
            calls["n"] += 1
            d = decision(seat_tier="standard", needs_clarification=0.9, decisive=True)
            d["intent"] = "feature" if calls["n"] <= 2 else "bugfix"
            return d, 10, "", None

        decision_trial.call_proxy_decision = fake_call

        with tempfile.TemporaryDirectory() as tmp:
            spec_path = self._spec(tmp)
            out_dir = Path(tmp) / "out"
            trial = decision_trial.run_trial(str(spec_path), str(out_dir), reps=3)

            self.assertEqual(trial["status"], "ok")
            self.assertEqual(calls["n"], 3)
            # seat_tier and clarify agree on all 3 reps; only intent flips --
            # the old metric would have scored this 1.0.
            self.assertAlmostEqual(trial["quality"], 2 / 3)

            details = json.loads((out_dir / "details.json").read_text(encoding="utf-8"))
            self.assertEqual(len(details["reps"]), 3)
            intents = [r["acted_decision"]["intent"] for r in details["reps"]]
            self.assertEqual(intents, ["feature", "feature", "bugfix"])
            # Every rep's acted_decision also carries the other acted fields
            # (from decision()'s own defaults), not just seat_tier/clarify.
            self.assertEqual(
                details["reps"][0]["acted_decision"]["seat_tier"], "standard"
            )
            self.assertEqual(
                details["reps"][0]["acted_decision"]["orchestrator_model"], "sonnet"
            )

    def test_reps_all_calls_return_none_is_error_status(self):
        def fake_call(prompt, zirv_bin, state_dir, env_extra, timeout_s=120, attribution=None):
            return None, 5, "", "proxy call failed"

        decision_trial.call_proxy_decision = fake_call

        with tempfile.TemporaryDirectory() as tmp:
            spec_path = self._spec(tmp)
            out_dir = Path(tmp) / "out"
            trial = decision_trial.run_trial(str(spec_path), str(out_dir), reps=3)
            self.assertEqual(trial["status"], "error")
            self.assertIsNone(trial["correctness"])
            self.assertIsNone(trial["quality"])


if __name__ == "__main__":
    unittest.main()
