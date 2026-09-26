#!/usr/bin/env python3
"""Unit tests for decision_trial.py (issue #803). stdlib `unittest` only.

Every test here stubs `zirv ctx proxy`'s output as a plain Python dict --
none calls a provider, none invokes `zirv` at all. Runnable standalone or
via `python -m unittest discover -s docs/benchmarks -p "test_*.py"` from
the repo root.
"""
import json
import sys
import tempfile
import unittest
from pathlib import Path

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
            lambda prompt, zirv_bin, state_dir, env_extra, timeout_s=120: (
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
            spec = {"task": "not-a-real-case", "trial_id": "t1", "campaign": "c1"}
            spec_path.write_text(json.dumps(spec), encoding="utf-8")
            with self.assertRaises(ValueError):
                decision_trial.run_trial(str(spec_path), str(Path(tmp) / "out"))


if __name__ == "__main__":
    unittest.main()
