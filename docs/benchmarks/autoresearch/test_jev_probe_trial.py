#!/usr/bin/env python3
"""Unit tests for jev_probe_trial.py. stdlib `unittest` only.

Every test here stubs `zirv ctx jev probe`'s output as a plain Python dict
-- none calls a provider, none invokes `zirv` at all. Runnable standalone or
via `python -m unittest discover -s docs/benchmarks -p "test_*.py"` from the
repo root.
"""
import json
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).resolve().parent))
import jev_probe_trial  # noqa: E402

SITE_ACTIONS = {
    "memory-rerank": {"keep", "prune"},
    "memory-harvest": {"keep", "skip"},
    "harvest-screen": {"skip", "run"},
    "context-report": {"omit", "keep"},
    "context-skill": {"omit", "keep"},
    "handoff-thin": {"demote", "keep"},
    "handoff-select": {"drop", "keep"},
    "compaction-select": {"keep", "omit"},
    "dispatch": {"cheap", "standard", "frontier", "deny"},
    "launch-effort": {"high", "classifier"},
    "classify-domain": {"tag", "none"},
    "inject": {"defer", "inject_now"},
}


def probe_result(site="memory-rerank", floor_site="memory", label=None,
                  floor=None, reps=None, calls=None, errors=0):
    return {
        "site": site,
        "floor_site": floor_site,
        "label": label or site,
        "floor": floor or {"min_confidence": 0.0, "min_margin": 0.2},
        "reps": reps if reps is not None else [],
        "calls": calls if calls is not None else (len(reps) if reps else 0),
        "errors": errors,
    }


class FindCaseTests(unittest.TestCase):
    def test_finds_case_across_site_dirs(self):
        row, labels_row, floor_site = jev_probe_trial.find_case("mem-001")
        self.assertIsNotNone(row)
        self.assertEqual(row["id"], "mem-001")
        self.assertEqual(floor_site, "memory")
        self.assertIsNotNone(labels_row)
        self.assertIn("labels", labels_row)

    def test_unknown_case_returns_none_triple(self):
        row, labels_row, floor_site = jev_probe_trial.find_case("not-a-real-case-id")
        self.assertIsNone(row)
        self.assertIsNone(labels_row)
        self.assertIsNone(floor_site)

    def test_missing_cases_dir_returns_none_triple(self):
        with tempfile.TemporaryDirectory() as tmp:
            row, labels_row, floor_site = jev_probe_trial.find_case(
                "anything", cases_dir=Path(tmp) / "does-not-exist")
        self.assertIsNone(row)
        self.assertIsNone(labels_row)
        self.assertIsNone(floor_site)


class WriteCaseFileTests(unittest.TestCase):
    def test_writes_id_state_and_n(self):
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "case.json"
            case_row = {"id": "x1", "site": "memory-rerank", "floor_site": "memory",
                        "state": {"_zirv_metadata_only": True, "facts": [[0, 1, 2]]}, "n": 3}
            jev_probe_trial.write_case_file(case_row, dest)
            payload = json.loads(dest.read_text(encoding="utf-8"))
            self.assertEqual(set(payload), {"id", "state", "n"})
            self.assertEqual(payload["id"], "x1")
            self.assertEqual(payload["n"], 3)

    def test_omits_n_when_absent(self):
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "case.json"
            case_row = {"id": "x2", "site": "inject", "state": {"_zirv_metadata_only": True, "facts": [[1]]}}
            jev_probe_trial.write_case_file(case_row, dest)
            payload = json.loads(dest.read_text(encoding="utf-8"))
            self.assertEqual(set(payload), {"id", "state"})


class ScoreProbeResultTests(unittest.TestCase):
    def test_perfectly_stable_and_correct(self):
        reps = [{"actions": {"c0": "keep", "c1": "prune"}, "error": None} for _ in range(5)]
        result = probe_result(reps=reps)
        labels = {"c0": "keep", "c1": "prune"}
        correctness, quality, per_item = jev_probe_trial.score_probe_result(result, labels)
        self.assertEqual(correctness, 1.0)
        self.assertEqual(quality, 1.0)
        self.assertEqual(per_item["c0"]["stability"], 1.0)
        self.assertEqual(per_item["c0"]["correct_count"], 5)

    def test_unstable_item_scores_partial_quality(self):
        reps = [
            {"actions": {"c0": "keep"}, "error": None},
            {"actions": {"c0": "keep"}, "error": None},
            {"actions": {"c0": "prune"}, "error": None},
            {"actions": {"c0": "keep"}, "error": None},
        ]
        result = probe_result(reps=reps)
        correctness, quality, per_item = jev_probe_trial.score_probe_result(
            result, {"c0": "keep"})
        self.assertAlmostEqual(quality, 0.75)
        self.assertAlmostEqual(correctness, 0.75)

    def test_missing_label_excluded_from_correctness_not_quality(self):
        reps = [{"actions": {"c0": "keep", "c1": "keep"}, "error": None} for _ in range(3)]
        result = probe_result(reps=reps)
        # c1 has no label
        correctness, quality, per_item = jev_probe_trial.score_probe_result(
            result, {"c0": "keep"})
        self.assertEqual(correctness, 1.0)  # only c0 counted
        self.assertEqual(quality, 1.0)  # both c0 and c1 fully stable
        self.assertIsNone(per_item["c1"]["label"])

    def test_no_reps_yields_none_metrics(self):
        result = probe_result(reps=[])
        correctness, quality, per_item = jev_probe_trial.score_probe_result(result, {})
        self.assertIsNone(correctness)
        self.assertIsNone(quality)
        self.assertEqual(per_item, {})

    def test_errored_rep_still_contributes_its_fallback_action(self):
        reps = [
            {"actions": {"novel": "run"}, "error": None},
            {"actions": {"novel": "run"}, "error": "jev call failed, used fallback"},
        ]
        result = probe_result(reps=reps)
        correctness, quality, per_item = jev_probe_trial.score_probe_result(
            result, {"novel": "run"})
        self.assertEqual(quality, 1.0)
        self.assertEqual(correctness, 1.0)
        self.assertEqual(len(per_item["novel"]["actions"]), 2)


class CallJevProbeTests(unittest.TestCase):
    """`subprocess.run` itself stubbed -- proves `call_jev_probe`'s exit code
    handling and argv shape without invoking `zirv`."""

    def setUp(self):
        self._orig_run = jev_probe_trial.decision_trial.subprocess.run

    def tearDown(self):
        jev_probe_trial.decision_trial.subprocess.run = self._orig_run

    def test_nonzero_exit_with_parsable_stdout_is_treated_as_failed(self):
        # Review finding: a non-zero exit must be a failed call even when
        # stdout happens to parse as JSON.
        stdout = json.dumps(probe_result()).encode("utf-8")

        def fake_run(argv, capture_output, timeout, env):
            return SimpleNamespace(stdout=stdout, stderr=b"boom", returncode=1)

        jev_probe_trial.decision_trial.subprocess.run = fake_run
        result, _elapsed_ms, _raw, error_note, returncode = jev_probe_trial.call_jev_probe(
            "memory-rerank", "case.json", 3, "zirv", None, {})
        self.assertIsNone(result)
        self.assertIsNotNone(error_note)
        self.assertIn("exit=1", error_note)
        self.assertEqual(returncode, 1)

    def test_zero_exit_with_parsable_stdout_succeeds(self):
        stdout = json.dumps(probe_result()).encode("utf-8")

        def fake_run(argv, capture_output, timeout, env):
            return SimpleNamespace(stdout=stdout, stderr=b"", returncode=0)

        jev_probe_trial.decision_trial.subprocess.run = fake_run
        result, _elapsed_ms, _raw, error_note, returncode = jev_probe_trial.call_jev_probe(
            "memory-rerank", "case.json", 3, "zirv", None, {})
        self.assertIsNotNone(result)
        self.assertIsNone(error_note)
        self.assertEqual(returncode, 0)

    def test_argv_never_passes_json_flag(self):
        # zirv ctx jev probe has no --json flag; stdout is always JSON.
        captured = {}

        def fake_run(argv, capture_output, timeout, env):
            captured["argv"] = argv
            return SimpleNamespace(stdout=json.dumps(probe_result()).encode("utf-8"),
                                    stderr=b"", returncode=0)

        jev_probe_trial.decision_trial.subprocess.run = fake_run
        jev_probe_trial.call_jev_probe("memory-rerank", "case.json", 3, "zirv", None, {})
        self.assertNotIn("--json", captured["argv"])


class RunTrialStubbedTests(unittest.TestCase):
    def setUp(self):
        self._orig_call = jev_probe_trial.call_jev_probe
        self._orig_spend = jev_probe_trial.decision_trial.call_spend_command

    def tearDown(self):
        jev_probe_trial.call_jev_probe = self._orig_call
        jev_probe_trial.decision_trial.call_spend_command = self._orig_spend

    def _spec(self, tmp, task="mem-001", reps=3):
        spec = {
            "schema": 1, "campaign": "c1", "candidate": "baseline", "trial_id": "t1",
            "task": task, "rep": 1, "split": "dev", "stage": "screen",
            "route": {"harness": "claude", "model": None}, "env": {},
            "state_dir": str(Path(tmp) / "state"), "timeout_secs": 30,
            "zirv_dir": None, "strategy": None, "cache_mode": "cold", "pressure": "natural",
        }
        spec_path = Path(tmp) / "spec.json"
        spec_path.write_text(json.dumps(spec), encoding="utf-8")
        return spec_path

    def test_ok_status_with_stubbed_stable_result(self):
        reps = [{"actions": {"c0": "keep", "c1": "prune", "c2": "keep", "c3": "prune"},
                  "error": None} for _ in range(3)]

        def fake_call(site, case_path, reps_n, zirv_bin, state_dir, env_extra,
                      timeout_s=120, attribution=None):
            return probe_result(site=site, reps=reps), 15, "", None, 0

        jev_probe_trial.call_jev_probe = fake_call
        jev_probe_trial.decision_trial.call_spend_command = lambda *a, **k: None

        with tempfile.TemporaryDirectory() as tmp:
            spec_path = self._spec(tmp)
            out_dir = Path(tmp) / "out"
            trial = jev_probe_trial.run_trial(str(spec_path), str(out_dir), reps=3)

            self.assertEqual(trial["status"], "ok")
            self.assertIsNotNone(trial["quality"])
            self.assertIsNotNone(trial["correctness"])
            self.assertTrue((out_dir / "trial.json").exists())
            self.assertTrue((out_dir / "details.json").exists())
            details = json.loads((out_dir / "details.json").read_text(encoding="utf-8"))
            self.assertIn("items", details)
            self.assertEqual(details["reps"], 3)

    def test_all_reps_errored_yields_error_status(self):
        reps = [{"actions": {"c0": "keep"}, "error": "boom"} for _ in range(3)]

        def fake_call(site, case_path, reps_n, zirv_bin, state_dir, env_extra,
                      timeout_s=120, attribution=None):
            return probe_result(site=site, reps=reps, errors=3), 15, "", None, 0

        jev_probe_trial.call_jev_probe = fake_call
        jev_probe_trial.decision_trial.call_spend_command = lambda *a, **k: None

        with tempfile.TemporaryDirectory() as tmp:
            spec_path = self._spec(tmp)
            out_dir = Path(tmp) / "out"
            trial = jev_probe_trial.run_trial(str(spec_path), str(out_dir), reps=3)
            self.assertEqual(trial["status"], "error")
            self.assertIsNone(trial["correctness"])
            self.assertIsNone(trial["quality"])

    def test_partial_errors_still_scored(self):
        reps = [
            {"actions": {"c0": "keep"}, "error": None},
            {"actions": {"c0": "keep"}, "error": "transient failure, used fallback"},
            {"actions": {"c0": "keep"}, "error": None},
        ]

        def fake_call(site, case_path, reps_n, zirv_bin, state_dir, env_extra,
                      timeout_s=120, attribution=None):
            return probe_result(site=site, reps=reps, errors=1), 15, "", None, 0

        jev_probe_trial.call_jev_probe = fake_call
        jev_probe_trial.decision_trial.call_spend_command = lambda *a, **k: None

        with tempfile.TemporaryDirectory() as tmp:
            spec_path = self._spec(tmp)
            out_dir = Path(tmp) / "out"
            trial = jev_probe_trial.run_trial(str(spec_path), str(out_dir), reps=3)
            self.assertEqual(trial["status"], "ok")
            self.assertIsNotNone(trial["correctness"])
            details = json.loads((out_dir / "details.json").read_text(encoding="utf-8"))
            self.assertEqual(len(details["rep_errors"]), 1)

    def test_probe_call_produces_no_json_is_error(self):
        def fake_call(site, case_path, reps_n, zirv_bin, state_dir, env_extra,
                      timeout_s=120, attribution=None):
            return None, 5, "", "jev probe produced no parsable JSON (exit=2)", 2

        jev_probe_trial.call_jev_probe = fake_call
        jev_probe_trial.decision_trial.call_spend_command = lambda *a, **k: None

        with tempfile.TemporaryDirectory() as tmp:
            spec_path = self._spec(tmp)
            out_dir = Path(tmp) / "out"
            trial = jev_probe_trial.run_trial(str(spec_path), str(out_dir), reps=3)
            self.assertEqual(trial["status"], "error")
            details = json.loads((out_dir / "details.json").read_text(encoding="utf-8"))
            self.assertIn("error", details)
            self.assertEqual(details["returncode"], 2)

    def test_unknown_case_raises(self):
        with tempfile.TemporaryDirectory() as tmp:
            spec_path = Path(tmp) / "spec.json"
            spec_path.write_text(json.dumps({
                "task": "nope", "trial_id": "t1", "campaign": "c1",
                "state_dir": str(Path(tmp) / "state"),
            }), encoding="utf-8")
            with self.assertRaises(ValueError):
                jev_probe_trial.run_trial(str(spec_path), str(Path(tmp) / "out"), reps=2)

    def test_missing_state_dir_raises(self):
        # Review finding: a spec with no state_dir (or an empty one) must
        # never let the child zirv fall back to the operator's real state
        # dir.
        with tempfile.TemporaryDirectory() as tmp:
            spec_path = Path(tmp) / "spec.json"
            spec_path.write_text(json.dumps({"task": "mem-001", "trial_id": "t1", "campaign": "c1"}),
                                  encoding="utf-8")
            with self.assertRaises(ValueError):
                jev_probe_trial.run_trial(str(spec_path), str(Path(tmp) / "out"), reps=2)

    def test_empty_state_dir_raises(self):
        with tempfile.TemporaryDirectory() as tmp:
            spec_path = Path(tmp) / "spec.json"
            spec_path.write_text(json.dumps({
                "task": "mem-001", "trial_id": "t1", "campaign": "c1", "state_dir": "",
            }), encoding="utf-8")
            with self.assertRaises(ValueError):
                jev_probe_trial.run_trial(str(spec_path), str(Path(tmp) / "out"), reps=2)

    def test_case_file_written_under_state_dir(self):
        captured = {}

        def fake_call(site, case_path, reps_n, zirv_bin, state_dir, env_extra,
                      timeout_s=120, attribution=None):
            captured["case_path"] = Path(case_path)
            captured["exists"] = Path(case_path).exists()
            return probe_result(site=site, reps=[]), 5, "", None, 0

        jev_probe_trial.call_jev_probe = fake_call
        jev_probe_trial.decision_trial.call_spend_command = lambda *a, **k: None

        with tempfile.TemporaryDirectory() as tmp:
            spec_path = self._spec(tmp)
            jev_probe_trial.run_trial(str(spec_path), str(Path(tmp) / "out"), reps=2)
            self.assertTrue(captured["exists"])
            self.assertIn(str(Path(tmp) / "state"), str(captured["case_path"]))


class JevCasesCorpusTests(unittest.TestCase):
    """Sanity checks over the real, committed jev-cases corpora -- no
    provider call, pure file loading. One of these doubles as the delivered
    proof that every site's fixtures are internally consistent."""

    FLOOR_SITES = ["memory", "context", "harvest_screen", "handoff_select",
                    "compaction_select", "dispatch", "launch_effort", "classify", "inject"]

    def test_every_floor_site_dir_exists_with_three_files(self):
        for site in self.FLOOR_SITES:
            d = jev_probe_trial.JEV_CASES_DIR / site
            self.assertTrue((d / "cases.jsonl").exists(), site)
            self.assertTrue((d / "labels.jsonl").exists(), site)
            self.assertTrue((d / "corpus.toml").exists(), site)

    def test_every_case_id_unique_and_matches_labels_and_corpus(self):
        import jev_probe_trial as jpt
        import tomllib
        for site in self.FLOOR_SITES:
            d = jpt.JEV_CASES_DIR / site
            cases = jpt.decision_trial.load_jsonl(d / "cases.jsonl")
            labels = jpt.decision_trial.load_jsonl(d / "labels.jsonl")
            corpus = tomllib.loads((d / "corpus.toml").read_text(encoding="utf-8"))
            corpus_ids = [t["id"] for t in corpus["task"]]
            self.assertEqual(len(corpus_ids), len(set(corpus_ids)), site)
            self.assertEqual(set(cases), set(labels), site)
            self.assertEqual(set(cases), set(corpus_ids), site)
            self.assertEqual(len(cases), 16, site)

    def test_split_counts_are_8_5_3(self):
        for site in self.FLOOR_SITES:
            d = jev_probe_trial.JEV_CASES_DIR / site
            cases = jev_probe_trial.decision_trial.load_jsonl(d / "cases.jsonl")
            counts = {"dev": 0, "validation": 0, "holdout": 0}
            for row in cases.values():
                counts[row["split"]] += 1
            self.assertEqual(counts, {"dev": 8, "validation": 5, "holdout": 3}, site)

    def test_every_item_labelled_with_a_valid_action(self):
        for site in self.FLOOR_SITES:
            d = jev_probe_trial.JEV_CASES_DIR / site
            cases = jev_probe_trial.decision_trial.load_jsonl(d / "cases.jsonl")
            labels = jev_probe_trial.decision_trial.load_jsonl(d / "labels.jsonl")
            for cid, row in cases.items():
                probe_site = row["site"]
                allowed = SITE_ACTIONS[probe_site]
                item_labels = labels[cid]["labels"]
                self.assertTrue(item_labels, f"{site}/{cid} has no labelled items")
                for item, action in item_labels.items():
                    self.assertIn(action, allowed, f"{site}/{cid}/{item} -> {action!r}")

    def test_two_site_floors_mix_both_sites_in_every_split(self):
        two_site = {
            "memory": {"memory-rerank", "memory-harvest"},
            "context": {"context-report", "context-skill"},
            "handoff_select": {"handoff-thin", "handoff-select"},
        }
        for floor_site, expected_sites in two_site.items():
            d = jev_probe_trial.JEV_CASES_DIR / floor_site
            cases = jev_probe_trial.decision_trial.load_jsonl(d / "cases.jsonl")
            by_split = {"dev": set(), "validation": set(), "holdout": set()}
            for row in cases.values():
                by_split[row["split"]].add(row["site"])
            for split, sites_seen in by_split.items():
                self.assertEqual(sites_seen, expected_sites,
                                  f"{floor_site}/{split} only saw {sites_seen}")

    def test_borderline_share_is_roughly_40_percent(self):
        for site in self.FLOOR_SITES:
            d = jev_probe_trial.JEV_CASES_DIR / site
            cases = jev_probe_trial.decision_trial.load_jsonl(d / "cases.jsonl")
            border = sum(1 for row in cases.values() if row["class"] == "ambiguous")
            self.assertGreaterEqual(border, 5, site)
            self.assertLessEqual(border, 7, site)

    def test_case_ids_prefixed_per_site(self):
        prefix = {
            "memory": "mem-", "context": "ctx-", "harvest_screen": "hs-",
            "handoff_select": "hsel-", "compaction_select": "cs-", "dispatch": "disp-",
            "launch_effort": "le-", "classify": "cls-", "inject": "inj-",
        }
        for site, pfx in prefix.items():
            d = jev_probe_trial.JEV_CASES_DIR / site
            cases = jev_probe_trial.decision_trial.load_jsonl(d / "cases.jsonl")
            for cid in cases:
                self.assertTrue(cid.startswith(pfx), f"{site}: {cid!r} missing prefix {pfx!r}")

    def test_all_case_ids_globally_unique(self):
        seen = set()
        for site in self.FLOOR_SITES:
            d = jev_probe_trial.JEV_CASES_DIR / site
            cases = jev_probe_trial.decision_trial.load_jsonl(d / "cases.jsonl")
            for cid in cases:
                self.assertNotIn(cid, seen, f"duplicate case id across sites: {cid!r}")
                seen.add(cid)


if __name__ == "__main__":
    unittest.main()
