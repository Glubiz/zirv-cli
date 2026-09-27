#!/usr/bin/env python3
"""Trial backend for the Jev determinism campaigns (`campaigns/jev-
determinism-<floor_site>.toml`).

`jev_probe_trial.py --trial <spec.json> --out <dir> --reps <K> [--zirv PATH]`
looks up the case `spec.task` names by scanning every `jev-cases/<floor_site>/
cases.jsonl` for a matching id (the case row itself names its production
probe `site`, e.g. `"memory-rerank"`), writes a scratch `case.json` (the
`{"id","state","n"}` shape the probe CLI contract expects) into the trial's
own state dir, and runs `zirv ctx jev probe --site <site> --case <case.json>
--reps <K> --json` -- the SAME production facts/decision path a real Jev
call would use, forced uncached by the probe itself. Scores per-item
*stability* (how often the modal action recurs across K reps -> `quality`)
and *correctness* (agreement with the case's labelled expected action per
item, averaged over every (rep, item) pair).

Reuses decision_trial.py's zirv resolution, child-env construction,
attribution, and spend reconciliation by import rather than duplicating
them -- see that file's own docstring for the shared conventions.

Stdlib only (Python 3.11+). Never calls a provider itself; `--trial` DOES
invoke `zirv ctx jev probe`, which is a real (usually cheap, but not free)
decision call -- this file's own tests stub that call out entirely (see
test_jev_probe_trial.py) so `python -m unittest` never spends anything.
"""
import argparse
import json
import sys
import time
from collections import Counter
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import decision_trial  # noqa: E402 -- reused helpers, see module docstring

JEV_CASES_DIR = HERE / "jev-cases"
DEFAULT_TIMEOUT_S = 120

# Fallback action strings, per SITE -- documentation only (the probe's own
# stdout already carries each errored rep's fallback actions; this table is
# never used to invent an action, only referenced in comments/tests).
SITE_FALLBACK = {
    "memory-rerank": "keep",
    "memory-harvest": "keep",
    "harvest-screen": "run",
    "context-report": "keep",
    "context-skill": "keep",
    "handoff-thin": "keep",
    "handoff-select": "keep",
    "compaction-select": "omit",
    "dispatch": "deny",
    "launch-effort": "classifier",
    "classify-domain": "none",
    "inject": "inject_now",
}


def find_case(case_id, cases_dir=JEV_CASES_DIR):
    """Scan every `jev-cases/<floor_site>/cases.jsonl` for `case_id`.
    Returns (case_row, labels_row, floor_site) or (None, None, None)."""
    if not cases_dir.exists():
        return None, None, None
    for site_dir in sorted(p for p in cases_dir.iterdir() if p.is_dir()):
        cases_path = site_dir / "cases.jsonl"
        if not cases_path.exists():
            continue
        cases = decision_trial.load_jsonl(cases_path)
        if case_id in cases:
            labels_path = site_dir / "labels.jsonl"
            labels = decision_trial.load_jsonl(labels_path) if labels_path.exists() else {}
            return cases[case_id], labels.get(case_id), site_dir.name
    return None, None, None


def write_case_file(case_row, dest_path):
    """Writes the `{"id","state","n"}` shape the `zirv ctx jev probe --case`
    contract expects (a subset of the corpus row -- `site`/`floor_site`/
    `class`/`split` are OUR bookkeeping, not part of the probe's own input)."""
    payload = {"id": case_row["id"], "state": case_row["state"]}
    if "n" in case_row and case_row["n"] is not None:
        payload["n"] = case_row["n"]
    dest_path.write_text(json.dumps(payload), encoding="utf-8")
    return dest_path


def call_jev_probe(site, case_path, reps, zirv_bin, state_dir, env_extra,
                    timeout_s=DEFAULT_TIMEOUT_S, attribution=None):
    """Runs `zirv ctx jev probe --site <site> --case <case_path> --reps <K>
    --json`. Returns (result dict or None, elapsed_ms, raw_stdout, error_note
    or None, returncode or None). The child's environment is built by
    `decision_trial.build_child_env` -- the candidate's floor-override env
    (`ZIRV_CTX_JEV_FLOOR_<SITE>_MIN_CONFIDENCE|_MIN_MARGIN`) rides in via
    `env_extra`, same as any other candidate overlay."""
    import subprocess

    argv = [zirv_bin, "ctx", "jev", "probe", "--site", site, "--case", str(case_path),
            "--reps", str(reps), "--json"]
    env = decision_trial.build_child_env(state_dir, attribution, env_extra)
    t0 = time.time()
    try:
        proc = subprocess.run(argv, capture_output=True, timeout=timeout_s, env=env)
    except subprocess.TimeoutExpired:
        return None, int((time.time() - t0) * 1000), "", "jev probe call timed out", None
    except Exception as exc:
        return None, int((time.time() - t0) * 1000), "", f"jev probe call failed: {exc}", None
    elapsed_ms = int((time.time() - t0) * 1000)
    stdout_text = proc.stdout.decode("utf-8", errors="replace")
    result = decision_trial.parse_last_json(stdout_text)
    if result is None:
        stderr_text = proc.stderr.decode("utf-8", errors="replace")
        return None, elapsed_ms, stdout_text + stderr_text, (
            f"jev probe produced no parsable JSON (exit={proc.returncode})"
        ), proc.returncode
    return result, elapsed_ms, stdout_text, None, proc.returncode


def score_probe_result(result, labels):
    """Computes per-item stability/correctness from a probe's parsed JSON
    (`{"site","floor_site","label","floor","reps":[{"actions":{item:action},
    "error":...}], "calls":K, "errors":E}`) against `labels` (`{item:
    expected_action}`).

    stability(item) = count of the most common action for that item across
    reps / K. `quality` = mean item stability (determinism axis).
    `correctness` = fraction of (rep, item) pairs whose action equals the
    item's label (agreement axis). A rep whose own `error` is set still
    contributes its (fallback) actions to both -- that is what production
    would actually do, not a hole in the data.

    Returns (correctness, quality, per_item dict) -- `per_item[item]` =
    {"actions": [...], "label": ..., "stability": ..., "correct_count": n}.
    """
    reps = result.get("reps") or []
    item_ids = sorted({item for rep in reps for item in (rep.get("actions") or {})})
    per_item = {}
    total_pairs = 0
    total_correct = 0
    stabilities = []
    for item in item_ids:
        actions = [rep.get("actions", {}).get(item) for rep in reps]
        actions = [a for a in actions if a is not None]
        if not actions:
            continue
        counts = Counter(actions)
        _, top_count = counts.most_common(1)[0]
        stability = top_count / len(actions)
        stabilities.append(stability)
        label = (labels or {}).get(item)
        correct_count = sum(1 for a in actions if label is not None and a == label)
        if label is not None:
            total_pairs += len(actions)
            total_correct += correct_count
        per_item[item] = {
            "actions": actions,
            "label": label,
            "stability": stability,
            "correct_count": correct_count,
        }
    quality = (sum(stabilities) / len(stabilities)) if stabilities else None
    correctness = (total_correct / total_pairs) if total_pairs else None
    return correctness, quality, per_item


def run_trial(spec_path, out_dir, reps, zirv_bin=None):
    spec = json.loads(Path(spec_path).read_text(encoding="utf-8"))
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    state_dir = spec.get("state_dir")
    attribution = decision_trial.attribution_env_for(spec)

    case_id = spec["task"]
    case_row, labels_row, floor_site = find_case(case_id)
    if case_row is None:
        raise ValueError(f"unknown jev probe case id: {case_id!r}")
    labels = (labels_row or {}).get("labels", {})

    if spec.get("zirv_dir"):
        import os
        os.environ["PATH"] = str(Path(spec["zirv_dir"]).resolve()) + ";" + os.environ["PATH"]
    zirv_bin = decision_trial.zirv_exe(zirv_bin)

    timeout_s = spec.get("timeout_secs") or DEFAULT_TIMEOUT_S
    env_extra = spec.get("env") or {}

    if state_dir:
        Path(state_dir).mkdir(parents=True, exist_ok=True)
        case_path = Path(state_dir) / f"jev-probe-case-{case_id}.json"
    else:
        case_path = out_dir / f"jev-probe-case-{case_id}.json"
    write_case_file(case_row, case_path)

    result, elapsed_ms, _raw, error_note, returncode = call_jev_probe(
        case_row["site"], case_path, reps, zirv_bin, state_dir, env_extra,
        timeout_s=timeout_s, attribution=attribution)

    if result is None:
        correctness, quality, per_item = None, None, {}
        rep_errors = []
        calls = 0
        errors = 0
        status = "error"
    else:
        correctness, quality, per_item = score_probe_result(result, labels)
        reps_list = result.get("reps") or []
        rep_errors = [r.get("error") for r in reps_list if r.get("error")]
        calls = result.get("calls", len(reps_list))
        errors = result.get("errors", len(rep_errors))
        # All reps errored -> status error (the runner retries); partial
        # errors are still scored from the fallback actions each rep
        # reports (what production would actually do), per the campaign
        # contract.
        status = "error" if (reps_list and errors >= len(reps_list)) else "ok"
        if status == "error":
            correctness, quality = None, None

    details = {
        "site": (result or {}).get("site", case_row.get("site")),
        "floor_site": (result or {}).get("floor_site", floor_site),
        "label": (result or {}).get("label"),
        "floor": (result or {}).get("floor"),
        "items": per_item,
        "rep_errors": rep_errors,
        "reps": reps,
        "calls": calls,
        "errors": errors,
        "latency_ms": elapsed_ms,
    }
    if error_note:
        details["error"] = error_note
    if returncode is not None:
        details["returncode"] = returncode
    (out_dir / "details.json").write_text(json.dumps(details, indent=2), encoding="utf-8")

    receipts_path = decision_trial.find_receipts_file(state_dir)
    spend = None
    if state_dir:
        spend = decision_trial.call_spend_command(
            zirv_bin, state_dir, receipts_path, spec.get("campaign") or "",
            spec.get("trial_id") or "")
    if spend is None:
        spend = decision_trial.fallback_spend_report()

    trial = {
        "schema": 1,
        "trial_id": spec.get("trial_id") or "",
        "status": status,
        "correctness": correctness,
        "quality": quality,
        "wall_ms": elapsed_ms,
        "spend": spend,
        "route": {"harness": "claude", "model": None, "tier": None, "effort": None},
        "env_fingerprint": "0" * 16,
        "details": "details.json",
    }
    (out_dir / "trial.json").write_text(json.dumps(trial, indent=2), encoding="utf-8")
    print(f"jev probe trial {case_id} -> status={status} correctness={correctness} "
          f"quality={quality} reps={reps} errors={errors}")
    return trial


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--trial", required=True, help="spec.json path")
    ap.add_argument("--out", required=True, help="trial output directory")
    ap.add_argument("--reps", type=int, required=True, help="repetitions to request from the probe")
    ap.add_argument("--zirv", default=None, help="path to the zirv executable (default: PATH)")
    args = ap.parse_args()
    run_trial(args.trial, args.out, args.reps, zirv_bin=args.zirv)


if __name__ == "__main__":
    main()
