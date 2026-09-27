#!/usr/bin/env python3
"""Trial backend for issue #803's intake decision-case corpus.

`decision_trial.py --trial <spec.json> --out <dir> [--zirv PATH]` looks up
the case `spec.task` names in `decision-cases/{inputs,labels}.jsonl`, runs
`zirv ctx proxy --json --headless <prompt>` against it (the SAME production
path the real headless launch uses -- this drives the intake decider, never
a re-implementation of it), and grades the decision against the case's
protected label. Writes `<out>/trial.json` and `<out>/details.json`; no
`receipts.jsonl` unless a real receipt file is observed under the trial's
`ZIRV_CTX_STATE_DIR` (see `find_receipts_file`).

Stdlib only (Python 3.11+, `tomllib`). Never calls a provider itself, but
`--trial` DOES call `zirv ctx proxy`, which is a real (usually cheap, but
not free) decision call -- this file's own tests stub that call out
entirely (see test_decision_trial.py) so `python -m unittest` never spends
anything.
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from collections import Counter
from pathlib import Path

HERE = Path(__file__).resolve().parent
CASES_DIR = HERE / "decision-cases"
DEFAULT_TIMEOUT_S = 120

# Mirrors run.py's own CLARIFY_THRESHOLD (proxy::mod::CLARIFY_THRESHOLD) --
# kept as a literal constant here too rather than importing run.py, since
# decision_trial.py has no other dependency on the wrapped-vs-vanilla
# harness and must stay usable on its own.
CLARIFY_THRESHOLD = 0.5

SEAT_TIER_RANK = {"cheap": 0, "standard": 1, "deep": 2, "frontier": 3}


def zirv_exe(explicit=None):
    if explicit:
        return explicit
    found = shutil.which("zirv")
    return found if found else "zirv"


def load_jsonl(path):
    """{id: row} for every line of a JSONL file -- both inputs.jsonl and
    labels.jsonl are keyed by `id`."""
    rows = {}
    with open(path, "r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            row = json.loads(line)
            rows[row["id"]] = row
    return rows


def load_case(case_id, cases_dir=CASES_DIR):
    """(input_row, label_row) for `case_id`, or (None, None) if unknown."""
    inputs = load_jsonl(Path(cases_dir) / "inputs.jsonl")
    labels = load_jsonl(Path(cases_dir) / "labels.jsonl")
    return inputs.get(case_id), labels.get(case_id)


def parse_last_json(text):
    """Same tolerant scan as run.py's `parse_last_json`: the last top-level
    JSON object in `text`, or None. Duplicated here (not imported) so this
    file has zero dependency on run.py."""
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
            if isinstance(obj, dict):
                results.append(obj)
            i = end
        except json.JSONDecodeError:
            i += 1
    return results[-1] if results else None


def build_child_env(state_dir, attribution, env_extra):
    """The full child environment for a `zirv` subprocess call, built
    explicitly from `os.environ` -- never by mutating the parent process's
    own environment (issue: a prior version set `ZIRV_ATTR_*`/
    `ZIRV_CTX_STATE_DIR` via `os.environ[...] = ...` in `run_trial` and
    never cleared an unset key, so a later trial in the same process could
    inherit a PREVIOUS trial's attribution). `attribution` is a plain
    `{env_var: value_or_None}` dict (see `attribution_env_for`); `env_extra`
    (a candidate's overlay) is applied LAST and wins on conflict, same
    null-removes-the-key convention `run.py`'s `child_env`/`merge_spec_env`
    already use."""
    env = dict(os.environ)
    if state_dir:
        env["ZIRV_CTX_STATE_DIR"] = str(state_dir)
    for k, v in (attribution or {}).items():
        if v is None:
            env.pop(k, None)
        else:
            env[k] = v
    for k, v in (env_extra or {}).items():
        if v is None:
            env.pop(k, None)
        else:
            env[k] = v
    return env


def attribution_env_for(spec):
    """`{"ZIRV_ATTR_CAMPAIGN": ..., ...}` from `spec`'s own campaign/
    candidate/trial_id/task fields, `None` for any field `spec` doesn't
    carry (so `build_child_env` removes rather than inherits a stale key)."""
    return {
        "ZIRV_ATTR_CAMPAIGN": spec.get("campaign"),
        "ZIRV_ATTR_CANDIDATE": spec.get("candidate"),
        "ZIRV_ATTR_TRIAL": spec.get("trial_id"),
        "ZIRV_ATTR_TASK": spec.get("task"),
    }


def call_proxy_decision(prompt, zirv_bin, state_dir, env_extra, timeout_s=DEFAULT_TIMEOUT_S,
                         attribution=None):
    """Runs `zirv ctx proxy --json --headless <prompt>`. Returns (decision
    dict or None, elapsed_ms, raw_stdout, error_note or None). The child's
    environment is built explicitly by `build_child_env` -- this function
    never reads or writes `os.environ` directly beyond that one call."""
    argv = [zirv_bin, "ctx", "proxy", "--json", "--headless", prompt]
    env = build_child_env(state_dir, attribution, env_extra)
    t0 = time.time()
    try:
        proc = subprocess.run(argv, capture_output=True, timeout=timeout_s, env=env)
    except subprocess.TimeoutExpired:
        return None, int((time.time() - t0) * 1000), "", "proxy call timed out"
    except Exception as exc:
        return None, int((time.time() - t0) * 1000), "", f"proxy call failed: {exc}"
    elapsed_ms = int((time.time() - t0) * 1000)
    stdout_text = proc.stdout.decode("utf-8", errors="replace")
    decision = parse_last_json(stdout_text)
    if proc.returncode != 0:
        stderr_text = proc.stderr.decode("utf-8", errors="replace")
        return None, elapsed_ms, stdout_text + stderr_text, (
            f"proxy exited non-zero (exit={proc.returncode})"
        )
    if decision is None:
        stderr_text = proc.stderr.decode("utf-8", errors="replace")
        return None, elapsed_ms, stdout_text + stderr_text, (
            f"proxy produced no parsable JSON (exit={proc.returncode})"
        )
    return decision, elapsed_ms, stdout_text, None


def decision_clarify(decision):
    """The boolean `prompt_layer`/`build_proxy_layer` itself gates a
    clarify line on: `needs_clarification >= CLARIFY_THRESHOLD` AND
    `needs_clarification_decisive`. Missing either key reads as False
    (never claims a clarify the decision didn't actually assert)."""
    if decision is None:
        return False
    if "needs_clarification" not in decision or "needs_clarification_decisive" not in decision:
        return False
    return (
        decision.get("needs_clarification", 0.0) >= CLARIFY_THRESHOLD
        and bool(decision.get("needs_clarification_decisive"))
    )


def decision_abstained(decision):
    """Whether Jev was NOT decisive for this decision: the deterministic
    decider is what `decide()` falls back to when nothing else (`typesafe`/
    `helper`) produced a decisive answer -- see decision.rs's `Decider`
    enum. A missing `decider` field (an older build, or a malformed
    decision) also counts as abstained: it never claims Jev decided
    something it can't see evidence for."""
    if decision is None:
        return True
    return decision.get("decider") != "typesafe" and decision.get("decider") != "helper"


def grade_decision(decision, label):
    """Grades one `zirv ctx proxy --json` decision against its protected
    label. Returns (correctness 0..1, details dict). `quality` is always
    None at the caller (#803: this axis has no independent quality judge).

    correctness = weight-adjusted agreement on seat_tier and clarify: 0.5
    for an exact seat_tier match, 0.5 for a clarify match, summed -- the
    label's own `weight` (case importance) is exposed in `details`, not
    folded into this per-case score, so a caller can compute a
    weight-adjusted AGGREGATE across many cases without this function
    needing to know the whole corpus.

    `false_escalation`: predicted tier ranks ABOVE the label (over-cautious,
    costs more than warranted). `unsafe_under_selection`: predicted tier
    ranks BELOW the label on a `costly_error` (or otherwise high-risk-
    tagged) case -- the dangerous direction, called out on its own even
    though it's also a subset of a plain tier mismatch. `unnecessary_
    clarification`: predicted clarify=True where the label says False.
    `abstained`: see `decision_abstained`.
    """
    label_tier = label.get("seat_tier")
    label_clarify = bool(label.get("clarify"))
    tags = label.get("tags") or []

    if decision is None:
        return 0.0, {
            "false_escalation": False, "unsafe_under_selection": bool(
                set(tags) & {"costly_error"}
            ),
            "unnecessary_clarification": False, "abstained": True,
            "predicted_seat_tier": None, "predicted_clarify": None,
            "label_seat_tier": label_tier, "label_clarify": label_clarify,
            "weight": label.get("weight", 1.0),
        }

    predicted_tier = decision.get("seat_tier")
    predicted_clarify = decision_clarify(decision)

    tier_match = predicted_tier == label_tier
    clarify_match = predicted_clarify == label_clarify
    correctness = (0.5 if tier_match else 0.0) + (0.5 if clarify_match else 0.0)

    pred_rank = SEAT_TIER_RANK.get(predicted_tier)
    label_rank = SEAT_TIER_RANK.get(label_tier)
    false_escalation = (
        pred_rank is not None and label_rank is not None and pred_rank > label_rank
    )
    is_high_risk_case = "costly_error" in tags
    unsafe_under_selection = bool(
        is_high_risk_case and pred_rank is not None and label_rank is not None
        and pred_rank < label_rank
    )
    unnecessary_clarification = bool(predicted_clarify and not label_clarify)

    details = {
        "false_escalation": false_escalation,
        "unsafe_under_selection": unsafe_under_selection,
        "unnecessary_clarification": unnecessary_clarification,
        "abstained": decision_abstained(decision),
        "predicted_seat_tier": predicted_tier,
        "predicted_clarify": predicted_clarify,
        "label_seat_tier": label_tier,
        "label_clarify": label_clarify,
        "weight": label.get("weight", 1.0),
    }
    return correctness, details


def read_proxy_decisions(state_dir):
    """Every row of `<state_dir>/proxy-decisions.jsonl` -- the production
    receipt file `zirv ctx proxy` itself appends to for every decision it
    computes (`src/commands/ctx/proxy/mod.rs`'s `PROXY_DECISIONS_FILE`,
    each row the flattened `ProxyDecision` struct), in file order. Tolerant
    of a missing file or a malformed line (skipped, never raises) -- an
    older build or a proxy call that failed before persisting anything
    yields an empty list, not an error."""
    if not state_dir:
        return []
    path = Path(state_dir) / "proxy-decisions.jsonl"
    if not path.exists():
        return []
    rows = []
    with open(path, "r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                row = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(row, dict):
                rows.append(row)
    return rows


def jev_ran_from_proxy_decisions(rows, reps=1):
    """Whether the production intake path actually invoked Jev/helper
    decisively for THIS trial, per the persisted `proxy-decisions.jsonl`
    receipt. For `reps<=1` this is just the last row (a trial's state dir is
    per-trial and isolated -- see the design spec's
    `ZIRV_CTX_STATE_DIR=<trial>/state`). For `reps>1` it reflects the WHOLE
    trial: true only when the trial's last `reps` rows (or all rows, if
    fewer are present) are all typesafe/helper -- one deterministic-only rep
    among several Jev-decisive ones must not read as "Jev ran" for the
    trial -- ground truth independent of whatever this process parsed from
    the subprocess's own stdout (`decision_abstained` above answers a
    related but different question from the live decision object; this one
    answers it from the receipt).

    `False` for an empty list: a missing Jev credential or an off gate
    means the production path never even attempted the call, which must
    never be read as the SAME thing as "attempted but not decisive"
    (`decision_abstained`'s `True`) -- this is exactly the distinction the
    review that added this function asked for. This is a plain boolean
    signal, not a promotion decision: a `False` here does not null
    `correctness` (a deterministic-only arm is a legitimate #803 baseline)
    -- the campaign runner separately excludes a trial missing a required
    `proxy:decider:typesafe`-style receipt via its own `requires_receipts`
    gate, which is the actual promotion-relevant exclusion mechanism.
    """
    if not rows:
        return False
    tail = rows[-reps:] if reps > 1 else rows[-1:]
    return all(row.get("decider") in ("typesafe", "helper") for row in tail)


def find_receipts_file(state_dir):
    """A trial's state dir MAY carry a receipts file the runner already
    knows how to read (mirrors run.py's own `receipts.jsonl` convention);
    decision_trial.py does not itself write one unless it finds real
    receipt-shaped data to report -- an intake decision's own spend is
    reconciled by `zirv workflow spend` from the state dir's own delegation/
    jev records, not from anything this file invents."""
    if not state_dir:
        return None
    candidate = Path(state_dir) / "receipts.jsonl"
    return candidate if candidate.exists() else None


def call_spend_command(zirv_bin, state_dir, receipts_path, campaign, trial_id):
    argv = [zirv_bin, "workflow", "spend", "--state-dir", str(state_dir),
            "--campaign", str(campaign), "--trial", str(trial_id), "--json"]
    if receipts_path:
        argv[3:3] = ["--receipts", str(receipts_path)]
    try:
        proc = subprocess.run(argv, capture_output=True, text=True, timeout=60)
    except Exception:
        return None
    if proc.returncode != 0:
        return None
    obj = parse_last_json(proc.stdout)
    return obj if isinstance(obj, dict) else None


def _empty_money(unknown_count=0):
    return {"reported_usd": None, "estimated_usd": None, "unknown_count": unknown_count,
            "price_as_of": None, "calls": 0}


def fallback_spend_report():
    """No receipts of its own and no working `zirv workflow spend` --
    everything about this trial's spend is unknown, never reported as 0."""
    return {
        "schema": 1, "execution": _empty_money(unknown_count=1), "overhead": _empty_money(),
        "by_source": {}, "calls": 0, "cached_calls": 0, "duplicates_dropped": 0,
        "completeness": "unknown", "billing": "unknown",
        "tokens": {"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0},
        "receipts": {},
    }


def run_trial(spec_path, out_dir, zirv_bin=None, reps=1):
    """`reps=1` (the default) is byte-identical to this function's original,
    single-call behaviour -- that code path is untouched below. `reps>1`
    (issue: Jev determinism tuning) runs `reps` intake proxy calls in the
    SAME trial state dir and folds them into one trial: `correctness` is the
    mean of each rep's own `grade_decision` score (an errored rep grades
    `None` against the label, same as today, and still counts); `quality` is
    the modal share of the (predicted_seat_tier, predicted_clarify) tuple --
    the exact fields `grade_decision` itself compares against the label --
    across ONLY the reps whose decision came back (a failed rep never counts
    as "agreeing" with another failed rep; if every rep failed, `status` is
    `"error"` and `quality`/`correctness` are both `None`, same as today)
    (the same "how often does the modal answer recur" stability measure
    `jev_probe_trial.py` uses, applied here to the intake decision instead
    of a Jev site's per-item actions). Raises `ValueError` if the spec's own
    `state_dir` is missing or empty, so a child `zirv` can never silently
    fall back to the operator's real state dir."""
    spec = json.loads(Path(spec_path).read_text(encoding="utf-8"))
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    state_dir = spec.get("state_dir")
    if not state_dir:
        raise ValueError(
            "trial spec is missing state_dir -- refusing to let a child zirv "
            "fall back to the operator's real state dir"
        )
    attribution = attribution_env_for(spec)

    case_id = spec["task"]
    input_row, label_row = load_case(case_id)
    if input_row is None or label_row is None:
        raise ValueError(f"unknown decision case id: {case_id!r}")

    if spec.get("zirv_dir"):
        os.environ["PATH"] = str(Path(spec["zirv_dir"]).resolve()) + ";" + os.environ["PATH"]
    zirv_bin = zirv_exe(zirv_bin)

    timeout_s = spec.get("timeout_secs") or DEFAULT_TIMEOUT_S
    env_extra = spec.get("env") or {}

    if reps <= 1:
        decision, elapsed_ms, _raw, error_note = call_proxy_decision(
            input_row["prompt"], zirv_bin, state_dir, env_extra, timeout_s=timeout_s,
            attribution=attribution)

        correctness, details = grade_decision(decision, label_row)
        details["latency_ms"] = elapsed_ms
        # issue #803 review: distinguish "Jev abstained" (`details["abstained"]`,
        # from the live decision object) from "Jev never ran" (missing
        # credential / gate off) using the production's OWN persisted receipt --
        # never nulls `correctness` (a deterministic-only arm is a legitimate
        # baseline); see `jev_ran_from_proxy_decisions`'s docstring.
        details["jev_ran"] = jev_ran_from_proxy_decisions(read_proxy_decisions(state_dir), reps=1)
        if error_note:
            details["error"] = error_note

        status = "error" if decision is None else "ok"
        trial_correctness = correctness if decision is not None else None
        quality = None
        route_model = (decision or {}).get("orchestrator", {}).get("model")
        route_tier = (decision or {}).get("seat_tier")
    else:
        rep_details = []
        total_elapsed = 0
        any_decision = False
        for _ in range(reps):
            decision, elapsed_ms, _raw, error_note = call_proxy_decision(
                input_row["prompt"], zirv_bin, state_dir, env_extra, timeout_s=timeout_s,
                attribution=attribution)
            rep_correctness, rep_grade = grade_decision(decision, label_row)
            rep_grade["latency_ms"] = elapsed_ms
            if error_note:
                rep_grade["error"] = error_note
            total_elapsed += elapsed_ms
            any_decision = any_decision or decision is not None
            rep_details.append({
                "correctness": rep_correctness, "grade": rep_grade,
                "decision_ok": decision is not None,
            })

        status = "ok" if any_decision else "error"
        if any_decision:
            trial_correctness = sum(r["correctness"] for r in rep_details) / len(rep_details)
            # Stability (quality) is the modal share over reps whose decision
            # actually came back -- a failed rep (predicted_seat_tier/
            # predicted_clarify both None) must never count as "agreeing"
            # with another failed rep; correctness above still folds in every
            # rep, failed or not, same as before.
            tuples = [
                (r["grade"]["predicted_seat_tier"], r["grade"]["predicted_clarify"])
                for r in rep_details if r["decision_ok"]
            ]
            top_count = Counter(tuples).most_common(1)[0][1]
            quality = top_count / len(tuples)
        else:
            trial_correctness = None
            quality = None

        details = {
            "reps": [r["grade"] for r in rep_details],
            "latency_ms": total_elapsed,
            "jev_ran": jev_ran_from_proxy_decisions(read_proxy_decisions(state_dir), reps=reps),
        }
        elapsed_ms = total_elapsed
        last_decision_grade = rep_details[-1]["grade"]
        route_model = None  # multi-rep: no single decision object to read a model from
        route_tier = last_decision_grade.get("predicted_seat_tier")

    (out_dir / "details.json").write_text(json.dumps(details, indent=2), encoding="utf-8")

    receipts_path = find_receipts_file(state_dir)
    spend = None
    if state_dir:
        spend = call_spend_command(zirv_bin, state_dir, receipts_path,
                                    spec.get("campaign") or "", spec.get("trial_id") or "")
    if spend is None:
        spend = fallback_spend_report()

    trial = {
        "schema": 1,
        "trial_id": spec.get("trial_id") or "",
        "status": status,
        "correctness": trial_correctness,
        "quality": quality,
        "wall_ms": elapsed_ms,
        "spend": spend,
        "route": {
            "harness": "claude",
            "model": route_model,
            "tier": route_tier,
            "effort": None,
        },
        "env_fingerprint": "0" * 16,
        "details": "details.json",
    }
    (out_dir / "trial.json").write_text(json.dumps(trial, indent=2), encoding="utf-8")
    print(f"decision trial {case_id} -> status={status} correctness={trial_correctness} "
          f"quality={quality} reps={reps} wall_ms={elapsed_ms}")
    return trial


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--trial", required=True, help="spec.json path")
    ap.add_argument("--out", required=True, help="trial output directory")
    ap.add_argument("--reps", type=int, default=1,
                     help="intake proxy calls to fold into one trial (default 1, byte-identical "
                          "to the original single-call output)")
    ap.add_argument("--zirv", default=None, help="path to the zirv executable (default: PATH)")
    args = ap.parse_args()
    run_trial(args.trial, args.out, zirv_bin=args.zirv, reps=args.reps)


if __name__ == "__main__":
    main()
