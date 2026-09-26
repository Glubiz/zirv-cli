# autoresearch for zirv (issues #799-#805)

Read this if you want to run or understand a zirv autoresearch campaign --
a bounded, budgeted search over small policy changes (Jev gate floors,
model/effort routing, context/compaction/handoff knobs) that keeps a
baseline and reports accept/reject/inconclusive with evidence, never
applying anything automatically. If you're extending the benchmark harness
itself (new tasks, new grading), see `../wrapped-vs-vanilla/CONTRACT.md`
and `README.md` instead -- this page is about running campaigns on top of
that harness, not about the harness's own task format.

## What this is, and what it borrows from karpathy/autoresearch

[karpathy/autoresearch](https://github.com/karpathy/autoresearch)'s
`program.md` is a simple, disciplined loop: one editable file (`train.py`),
a fixed evaluator it may not touch (`prepare.py`), a hard per-run budget,
baseline established first, one change per commit, a `results.tsv` ledger
(`commit, metric, memory, status keep|discard|crash, description`), keep
only if the metric improves, a "0.001 better with 20 hacky lines isn't
worth it" simplicity bar, and an indefinite loop.

zirv keeps that discipline and swaps out the parts that don't transfer to a
CLI agent harness with real dollar costs and safety floors:

| upstream | zirv |
|---|---|
| edit `train.py` | a schema-validated policy overlay (allowlisted `ZIRV_CTX_*` env keys) or an operator-declared, path-scoped source patch |
| fixed `prepare.py` evaluator | a protected evaluator set, hash-pinned at campaign start and re-verified before every trial and before promotion |
| `val_bpb`, lower is better | correctness + quality floors checked first, then cost and wall time compared as separate axes -- no single weighted score |
| one run per idea | paired baseline/candidate repetitions, screen (`dev` split) -> validate (`validation` split) -> one `holdout` confirmation |
| `results.tsv` | an append-only ledger plus a resolved-manifest lock file; resumable without re-running a finished (paid) trial |
| loop forever | explicit spend / wall-clock / call / trial / retry / concurrency budgets, reserved before each dispatch |
| `keep` = commit to the branch | a report plus a reviewable overlay/patch proposal and rollback notes -- never merged, applied, or turned into a PR automatically |

## Status in this worktree

**Evaluator side (this PR, Lane C) -- implemented and tested:**
- `../wrapped-vs-vanilla/run.py --trial <spec.json> --out <dir>` and
  `--check-graders` (issues #801, #802, #804).
- `decision_trial.py` and the labelled intake decision-case corpus (issue
  #803).
- `../wrapped-vs-vanilla/corpus.toml` (the versioned task/split list, issue
  #801) and `t25_sticky_notes` (issue #805's long-session chain task).
- The campaign manifests under `campaigns/` (issue #802's schema, authored
  against the contract below).

**Runner side -- NOT yet in this worktree:** `zirv workflow research
plan|run|status|report` and `zirv workflow spend` (issues #800, #802) are
designed in
[`docs/superpowers/specs/2026-09-26-autoresearch-design.md`](../../superpowers/specs/2026-09-26-autoresearch-design.md)
but not implemented here -- a separate lane owns that Rust command. Every
manifest under `campaigns/` is authored against that design's contract and
validated to parse, but **cannot be run end to end in this worktree yet**;
`run.py --trial`/`--check-graders` and `decision_trial.py` (the backends
those manifests call) work today and are what this page's examples
actually run.

## Setup

Everything here is Python 3.11 stdlib (including `tomllib` for the `.toml`
manifests) -- no install step for the benchmark side. You need:
- `zirv` on `PATH` (or pass `--zirv-dir`/set `spec.zirv_dir` to a specific
  build) for anything that calls `zirv ctx proxy`, `zirv ctx exec`, or
  `zirv workflow spend`.
- A Jev credential in the environment for any campaign whose candidates
  touch `[jev]` gates or the intake proxy (same requirement `run.py`'s grid
  mode already has -- see `../wrapped-vs-vanilla/README.md`).
- Nothing else for `--check-graders` or the unit tests: both run entirely
  locally.

## Running the parts that exist today

Grader self-check (no provider call, safe to run any time):

```
cd docs/benchmarks/wrapped-vs-vanilla
python run.py --check-graders
```

One trial by hand, the same shape a campaign runner would invoke. This
spends real money (it launches a real agent) -- it is shown here as the
documented CLI contract, not something run as part of writing this page:

```
cd docs/benchmarks/wrapped-vs-vanilla
python - <<'PY'
import json, pathlib
pathlib.Path("spec.json").write_text(json.dumps({
    "schema": 1, "campaign": "manual", "candidate": "baseline", "trial_id": "manual-1",
    "task": "t02_pagination", "rep": 1, "split": "dev", "stage": "screen",
    "route": {"harness": "claude", "model": "sonnet"},
    "env": {}, "state_dir": "manual-state", "timeout_secs": 1200,
    "zirv_dir": None, "strategy": None, "cache_mode": "cold", "pressure": "natural",
}))
PY
python run.py --trial spec.json --out manual-out --cond zirv-proxy
cat manual-out/trial.json
```

One intake decision-case trial -- also spends real money (one live `zirv
ctx proxy` call) and, like the example above, is shown as the documented
CLI contract rather than a verified run:

```
cd docs/benchmarks/autoresearch
python - <<'PY'
import json, pathlib
pathlib.Path("spec.json").write_text(json.dumps({
    "schema": 1, "campaign": "manual", "candidate": "baseline", "trial_id": "manual-1",
    "task": "ic001", "state_dir": "manual-state", "timeout_secs": 60,
}))
PY
python decision_trial.py --trial spec.json --out manual-out
cat manual-out/trial.json manual-out/details.json
```

Unit tests (no provider call, ever):

```
python -m unittest discover -s docs/benchmarks -p "test_*.py"
```

## `zirv workflow research plan|run|status|report` and `zirv workflow spend` (designed, not yet built here)

Once the runner lands, the intended flow (see the design spec for the full
contract) is: `plan` resolves a manifest and prints the work/budget it would
do without calling a provider; `run` executes it (screen -> validate ->
holdout, respecting every budget); `status`/`report` inspect an in-progress
or finished campaign's ledger. `zirv workflow spend --state-dir <dir>
--receipts <file> --campaign <c> --trial <t> --json` is the one spend
reconciler both `run.py --trial` and `decision_trial.py` already call (and
fall back from, with a `completeness: "partial"`/`"unknown"` `SpendReport`,
when it's missing) -- see `../wrapped-vs-vanilla/CONTRACT.md`'s
"Autoresearch trial mode" section for that fallback's exact shape.

## Manifest reference

A campaign manifest is TOML, `schema = 1`. Top-level: `id`, `description`,
`runtime` (`meta` -- measured; `native` -- unmeasured while `zirv native` is
release-gated), `seat_mode` (`single`; `orchestration` is refused until an
orchestration suite exists), `cache_mode` (`cold|warm`), `billing`
(`metered|subscription|unknown`), `stratify` (`none` default; `class`
appends each observation's own corpus task `class` to its cohort key --
issue #804's stratification, so a routing/gate candidate that helps
`bounded` work while hurting `architecture` work is reported as two
separate per-class verdicts instead of one averaged, misleading one;
cohorts stay never-pooled either way).

- `[baseline] commit` -- the commit a candidate diffs against (for a source
  patch) and what `zirv_dir`'s baseline build comes from.
- `[evaluator] version, protected, self_check` -- `protected` is a list of
  paths/globs that must not change mid-campaign (hash-pinned at start,
  re-verified before every trial); `self_check` is an argv run once before
  the campaign starts (e.g. `["python", "run.py", "--check-graders"]`).
- `[corpus] file` -- the `corpus.toml` this campaign's `dev`/`validation`/
  `holdout` splits come from.
- `[backend] kind, command|file, per_trial_ceiling_usd, calls_per_trial,
  timeout_secs` -- `kind = "command"` runs `command` (a list with `{spec}`/
  `{out}`/`{zirv_dir}` placeholders) per trial; `kind = "fixture"` reads
  scripted results from `file` instead of calling anything.
- `[route] harness, model` -- the default route a trial's `spec.route` is
  built from before a candidate's env overlay is applied.
- `[budgets] max_spend_usd, max_wall_secs, max_calls, max_trials,
  max_retries, concurrency` -- hard caps, enforced by reserving each
  dispatch's declared ceiling before it starts.
- `[stages.screen|validate|holdout] split, reps` (+ holdout's own
  `max_uses`) -- which `corpus.toml` split and how many paired repetitions
  each stage runs.
- `[criteria] min_pairs, correctness_floor, quality_floor,
  max_correctness_regression, max_quality_regression, min_effect,
  confidence, bootstrap_resamples` -- the promotion gate (see the design
  spec's #801 section for the exact accept/reject/inconclusive decision
  tree this feeds).
- `[cohort] pressure, env` -- `pressure = "natural"|"forced"`; `env` applies
  to BOTH arms (never a candidate-only advantage) -- see
  `campaigns/context-compaction-forced.toml` for a forced-pressure cohort
  variant of `campaigns/context-compaction.toml`'s candidates.
- `[candidate_space] allow_env, allowed_models` (+ optional
  `[candidate_space.source_patch] allowed_paths, build, bin_dir`) -- the
  env keys and model names a `[[candidates]]` entry may actually use; a key
  outside this list (or outside the compiled-in allowlist -- see the design
  spec's #802 section) is refused before any trial runs.
- `[[candidates]] id, hypothesis, mechanism, env, patch?, requires_receipts,
  strategy?` -- one candidate; `requires_receipts` names the receipt
  key(s) (`jev:<site>`, `effect:<name>`, `proxy:decision`, or
  `proxy:decider:<decider>`) that must appear in a trial's receipts for it
  to count as having actually exercised the mechanism under test -- a trial
  missing one is `untriggered`, excluded from that candidate's evidence.
  `plan` refuses a `requires_receipts` entry with an unrecognized prefix
  outright, rather than silently marking every trial untriggered forever.
  `proxy:decision` alone is written even when Jev never ran (the
  deterministic decider appends one too, issue #803); a candidate proving
  the production Jev/helper intake path specifically ran should require
  `proxy:decider:typesafe` or `proxy:decider:helper` instead (from that same
  row's own `decider` field -- see `jev-intake-floors.toml`).
- optional `[proposer] harness, model, max_proposals, per_call_ceiling_usd`
  -- an agent that proposes bounded env-overlay candidates from the dev
  aggregates; its own spend counts as overhead, never execution.

See `campaigns/*.toml` for five worked examples: `fixture-demo.toml` (free,
scripted, safe to run once the runner exists), and four real campaigns
(`jev-intake-floors.toml`, `jev-gates-e2e.toml`, `routing-ladder.toml`,
`context-compaction.toml` + its forced-pressure variant) that spend real
money and are never run in CI.

## Budgets

Every non-fixture manifest here sets deliberately conservative
`[budgets]` -- a few dollars to a couple hundred, hours not days of wall
time, low concurrency. A campaign never loops indefinitely: `max_trials`
and `max_spend_usd` are hard stops, and the runner reserves a trial's
declared cost ceiling before dispatching it, so in-flight work can finish
but new work never starts once a cap would be exceeded.

## Supported backends

- **`command` (run.py)**: `python ../wrapped-vs-vanilla/run.py --trial
  {spec} --out {out}` -- any wrapped-vs-vanilla task, including the
  `escalate` strategy and chain tasks.
- **`command` (decision_trial.py)**: `python decision_trial.py --trial
  {spec} --out {out}` -- any id in `decision-cases/inputs.jsonl`.
- **`fixture`**: a TOML file of scripted `[[result]]` rows (see
  `campaigns/fixtures/demo.toml`) -- no process is launched at all.

## Runtimes

- **meta** (measured): the `claude`/`zirv ctx exec` path this whole harness
  already exercises.
- **native** (`zirv native`): unmeasured while native execution is
  release-gated -- a manifest may declare `runtime = "native"` but a report
  built from it must say so plainly, never blended with `meta` numbers.
- **orchestration** (`seat_mode = "orchestration"`): no suite exists yet: a
  single-seat (`seat_mode = "single"`) result is never orchestration
  evidence, and this repo refuses to accept one as such.

## Promotion

A candidate is `accept`, `reject`, `inconclusive`, `tradeoff` (a material
win on one axis paired with a material loss on the other -- reported,
never auto-accepted), or `unmeasured` (every trial excluded, usually for a
missing required receipt). Promotion is per-cohort (runtime, harness,
model, cache mode, pressure, task class -- never pooled across these) and
uses paired bootstrap confidence intervals, not a raw mean comparison. See
the design spec's #801 section for the full six-step decision tree.

## Outputs

A finished campaign writes (once the runner exists) `report.md`,
`report.json`, `results.tsv` (an autoresearch-style summary line per
candidate), and, for the single simplest accepted candidate (fewest
overlay keys + patch lines among CI-overlapping winners),
`proposal/overlay.toml` or `proposal/candidate.patch` plus
`proposal/ROLLBACK.md`. None of this is applied, merged, pushed, or turned
into a pull request automatically -- every one of those is a separate,
explicit, operator-taken action.

## Limitations

- **Single project family**: every task is `ledgerlite`. A report built
  from this corpus is flagged `single_family`; it is not evidence a
  candidate generalizes to a different codebase or language.
- **No paid campaign has been run** as part of this PR -- every non-fixture
  manifest here is authored against the contract and validated to parse,
  never executed (the runner it targets doesn't exist in this worktree
  yet).
- **Orchestration is unmeasured**: no multi-seat suite exists; every result
  here is single-seat.
- **#762 owns rot-threshold calibration**; this campaign framework only
  evaluates whatever thresholds #762 lands, it does not implement a second
  calibration algorithm of its own.
- **#760 (memory budget) and #772 (prompt size)** have no runtime knob
  today -- a report cannot recommend a value for either; they are reported
  as not adaptable, not silently skipped.
- **Native execution** stays unmeasured while `zirv native` is
  release-gated, regardless of what a manifest declares.
