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

## What is here

- `zirv workflow research plan|run|status|report` -- the campaign runner
  (issue #802) -- and `zirv workflow spend`, the spend reconciler (#800).
- `../wrapped-vs-vanilla/run.py --trial <spec.json> --out <dir>` and
  `--check-graders` (#801, #802, #804).
- `decision_trial.py` and the labelled intake decision-case corpus (#803),
  including its `--reps K` determinism mode.
- `jev_probe_trial.py` and the nine per-floor-site `jev-cases/` corpora --
  the Jev determinism probe backend; see "Jev determinism campaigns" below.
- `../wrapped-vs-vanilla/corpus.toml` (the versioned task/split list, #801)
  and `t25_sticky_notes` (#805's long-session chain task).
- The campaign manifests under `campaigns/`; `plan` accepts every one of
  them (the ten `jev-determinism-*.toml` manifests have not themselves been
  run through `plan` here -- see "Limitations").
- `sample-report.md`: the `report.md` the free fixture campaign produces.

The full contract is in
[`docs/superpowers/specs/2026-09-26-autoresearch-design.md`](../../superpowers/specs/2026-09-26-autoresearch-design.md).

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

## Running the backends by hand

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

## `zirv workflow research` and `zirv workflow spend`

```
zirv workflow research plan <manifest> [--repo <dir>] [--json]
zirv workflow research run <manifest> [--repo <dir>] [--dir <campaign-dir>] [--resume] [--json]
zirv workflow research status <campaign-id|campaign-dir> [--json]
zirv workflow research report <campaign-id|campaign-dir> [--json]
```

`plan` resolves a manifest and prints the work and worst-case budget it
would use, without calling a provider; it exits 2 when the manifest is
refused. `run` executes it: baseline, then screen -> validate -> holdout per
candidate, within every budget. Without `--dir`, a campaign lives under
`<ctx state dir>/research/<id>/`. `--resume` continues a stopped campaign
without re-running a finished trial, and refuses a manifest that changed
since the campaign started. `status` shows stage, trial counts and spend
against caps; `report` regenerates the outputs from the ledger.

The free way to see all of it is the fixture campaign, which launches no
process:

```
zirv workflow research run docs/benchmarks/autoresearch/campaigns/fixture-demo.toml --dir fixture-demo-out
```

`zirv workflow spend --state-dir <dir> [--receipts <file>] [--campaign <c>]
[--candidate <c>] [--trial <t>] [--task <t>] [--json]` is the spend
reconciler that both `run.py --trial` and `decision_trial.py` call. When
it is missing they fall back to a `completeness: "partial"`/`"unknown"`
`SpendReport`; `../wrapped-vs-vanilla/CONTRACT.md`'s "Autoresearch trial
mode" section gives the exact shape.

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
  `max_uses`), plus an optional `classes = [...]` -- which `corpus.toml`
  split and how many paired repetitions each stage runs, narrowed (if
  `classes` is set) to only those corpus `class` values; empty/absent runs
  every task of the split, unchanged. `classes` entries are validated
  against the corpus contract's known class set (`mechanical|bounded|bug|
  feature|architecture|ambiguous|sensitive|long_session`) -- an unrecognized
  one is refused at manifest-load time. **Split discipline (#801):**
  `screen` and `validate` must use different splits (letting them share one
  would have validate just re-measure exactly what screen already saw), and
  neither may use `holdout` -- that split is reserved for the single final
  confirmation. `plan` also refuses a campaign whose `validate` or
  `holdout` stage could never reach `criteria.min_pairs` (task count after
  the `classes` filter, times `reps`; with `stratify = "class"`, the
  *smallest* class present, since the gate never pools cohorts) --
  `"stage <x> yields N pairs < criteria.min_pairs=M: no candidate could
  ever be promoted"` -- before any real trial spends money finding that out
  the slow way.
- `[criteria] objective, min_pairs, correctness_floor, quality_floor,
  max_correctness_regression, max_quality_regression, min_effect,
  confidence, bootstrap_resamples` -- the promotion gate (see the design
  spec's #801 section for the exact accept/reject/inconclusive decision
  tree this feeds). `objective` picks which axis step 4 of that tree (the
  "material benefit" check) measures: `"efficiency"` (the default, and
  every campaign before the jev-determinism batch) looks for a material win
  on cost or wall time, with correctness/quality only enforced as
  non-inferiority floors; `"quality"` instead looks for a material win on
  `quality` itself (bootstrap CI lower bound strictly greater than
  `min_effect`, an absolute point on the same `quality` scale the trial
  backend reports -- for the jev-determinism campaigns that scale is
  acted-decision *stability*), with correctness kept as a non-inferiority
  floor and cost/wall reported but not gating. A `"quality"` campaign
  answers "does raising this floor make Jev's acted decisions more
  reproducible, without breaking correctness" -- not "is it cheaper."
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

`decision_trial.py`'s `details.json` (referenced from `trial.json.details`)
carries a `jev_ran` boolean, read from the trial's own `<state_dir>/
proxy-decisions.jsonl` (the production's persisted receipt, not this
script's own parsed decision object): `true` when that file's last row's
`decider` is `"typesafe"` or `"helper"`, `false` for a `"deterministic"`
row OR an empty/missing file. This is deliberately a DIFFERENT signal from
`details.abstained` (computed from the live decision object returned by
this trial's own `zirv ctx proxy` call): `abstained` cannot tell "Jev was
asked but not decisive" apart from "Jev never ran at all" (missing
credential, or every `[jev]` gate off), while `jev_ran` answers exactly
that from the persisted receipt. Neither ever nulls `correctness` -- a
deterministic-only arm is a legitimate #803 baseline to compare against,
not a failure; a candidate that needs proof the production Jev/helper path
specifically fired should use `requires_receipts = ["proxy:decider:
typesafe"]` (see above), which is the runner's own exclusion mechanism for
that.

See `campaigns/*.toml` for sixteen worked examples: `fixture-demo.toml`
(free, scripted, safe to run any time); five real campaigns
(`jev-intake-floors.toml`, `jev-gates-e2e.toml`, `routing-ladder.toml`,
`context-compaction.toml` + its forced-pressure variant); and ten more real
campaigns for Jev *determinism* tuning (`jev-determinism-<floor_site>.toml`
for each of the nine tunable floor sites, plus `jev-determinism-intake.toml`
for the intake proxy) -- see "Jev determinism campaigns" below. All the real
campaigns spend money and are never run in CI.

## Jev determinism campaigns

Every Jev-gated feature acts on a sampled answer only when it clears a
`(min_confidence, min_margin)` floor; below the floor it falls back to a
fixed deterministic default. Raising a floor trades Jev's influence for
stability. The ten `jev-determinism-*.toml` campaigns measure that
trade-off directly, per floor site, using two axes:

- **`quality`** = acted-decision *stability*: for `jev_probe_trial.py`, the
  mean, across a case's items, of "how often does the plurality action
  recur across `K` uncached repetitions" (`jev_probe_trial.py --reps 5`);
  for `decision_trial.py --reps K` (the intake campaign), the modal share
  of the `(seat_tier, clarify)` decision tuple across `K` uncached reps.
  `K` uncached means every rep is a genuinely fresh Jev/proxy call --
  `zirv ctx jev probe` forces its own cache off, and the intake campaign
  sets `ZIRV_CTX_JEV_CACHE_TTL_SECS=0` in `[cohort] env`.
- **`correctness`** = agreement with a labelled expected action: for
  `jev_probe_trial.py`, the fraction of every `(rep, item)` pair whose
  action equals that item's label in `jev-cases/<floor_site>/labels.jsonl`;
  for the intake campaign, the mean of `decision_trial.py`'s existing
  per-rep `grade_decision` score.

`[criteria] objective = "quality"` on all ten manifests: a candidate is
promoted only on a *material* stability win (bootstrap CI lower bound on
the quality delta strictly above `min_effect`), with correctness held to a
non-inferiority floor -- a floor change that only saves cost/wall time
without also improving stability is not what these campaigns are for.

### `jev_probe_trial.py` -- the probe backend

`jev_probe_trial.py --trial <spec.json> --out <dir> --reps <K> [--zirv
PATH]` looks up the case named by `spec.task` by scanning every
`jev-cases/<floor_site>/cases.jsonl` (each case row names its own
production probe `site`, e.g. `"memory-rerank"`), writes a scratch
`{"id","state","n"}` case file into the trial's state dir, and runs `zirv
ctx jev probe --site <site> --case <case.json> --reps <K>` (stdout is
always JSON) -- the same production facts/decision path a real Jev call at that site would
exercise, with the cache forced off by the probe itself and the floor read
from `ZIRV_CTX_JEV_FLOOR_<SITE>_MIN_CONFIDENCE|_MIN_MARGIN` (the same env
a candidate's overlay already sets). It reuses `decision_trial.py`'s zirv
resolution, child-env construction, attribution, and spend reconciliation
by import rather than duplicating them. A rep that itself errors still
contributes its production fallback action to both `quality` and
`correctness` (that IS what production would do, not a hole in the
data); a trial where every rep errored is `status: "error"` so the runner
retries it instead of scoring a probe failure as a real result.
`details.json` carries `site`, `floor_site`, `label`, `floor`, and, per
item, its action list, label, and stability.

### `jev-cases/<floor_site>/` -- the per-site corpora

Nine directories (`memory`, `context`, `harvest_screen`, `handoff_select`,
`compaction_select`, `dispatch`, `launch_effort`, `classify`, `inject`),
each with `cases.jsonl`, `labels.jsonl`, `corpus.toml` (16 cases: 8 `dev`,
5 `validation`, 3 `holdout`, `family = "jev-<floor_site>"`, `kind =
"decision"`). Three floor sites cover two production SITEs each (`memory`:
memory-rerank + memory-harvest; `context`: context-report + context-skill;
`handoff_select`: handoff-thin + handoff-select) and mix both across every
split rather than segregating them. About 40% of cases in every corpus are
deliberately borderline (facts placed near the site's own decision
threshold, `class = "ambiguous"`) -- that is where instability actually
shows up; the rest are clear-cut (`class = "bounded"`).

**Label policy**: every item in every case is labelled in `labels.jsonl`.
A clear-cut case's items get the action a careful engineer would take from
those facts (e.g. a recent, large, frequently-referenced handoff item ->
`"keep"`). A genuinely borderline case's items get that SITE's own
FALLBACK action -- the conservative deterministic default production falls
back to when the floor isn't cleared -- never a guessed decisive action,
since a borderline case's whole point is that no confident answer is
correct by construction.

Action vocabulary per SITE (kept in one place -- `SITE_FALLBACK` in
`jev_probe_trial.py` -- so a rename on the Rust side is a one-line fix):
`memory-rerank`: keep|prune, `memory-harvest`: keep|skip, fallback `keep`;
`harvest-screen`: skip|run, fallback `run`; `context-report`/
`context-skill`: omit|keep, fallback `keep`; `handoff-thin`: demote|keep,
fallback `keep`; `handoff-select`: drop|keep, fallback `keep`;
`compaction-select`: keep|omit, fallback `omit`; `dispatch`:
cheap|standard|frontier, fallback `deny`; `launch-effort`: high|low,
fallback `classifier`; `classify-domain`: tag|none per domain tag id,
fallback `none`; `inject`: defer|inject_now, fallback `inject_now`.

### The ten manifests

`campaigns/jev-determinism-<floor_site>.toml` for each of the nine tunable
floor sites, plus `campaigns/jev-determinism-intake.toml` for the intake
proxy's own floors. Each varies its floor's env var(s)
(`ZIRV_CTX_JEV_FLOOR_<SITE>_MIN_CONFIDENCE|_MIN_MARGIN`, or
`ZIRV_CTX_PROXY_MIN_CONFIDENCE|_MIN_MARGIN` for intake) across four
candidates: three margin values (0.10, 0.30, 0.40, confidence left at the
compiled default) and one raised-confidence candidate (the compiled
default's own value, read from source, +0.1 -- or 0.6 where that default is
0.0). `requires_receipts` is `["jev:<label>"]` only where every case in
that floor site's corpus shares one production receipt label (`memory`,
`harvest_screen` -> `"harvest"` -- the harvest-screen call records its
receipt under site string `"harvest"`, not `"harvest_screen"` --
`compaction_select`, `dispatch`, `launch_effort`, `classify`, `inject`);
it is `[]` for `context` and `handoff_select`, whose two production SITEs
each write a DIFFERENT receipt label (`"context-parent-reports"` vs
`"context-skill-descriptions"`; `"handoff"` vs `"handoff_select"`), so no
single `jev:<label>` could cover every case without silently mis-excluding
half the corpus.

The intake campaign also documents a real gap: `zirv ctx proxy --json
--headless` can run a non-Jev-typesafe `"helper"` decider (`[proxy]
decider` / `ZIRV_CTX_PROXY_DECIDER`), and that key is `REPO_FORBIDDEN` and
absent from the runner's compiled env allowlist -- no campaign manifest can
pin it to `typesafe`. The campaign relies on `ProxyDecider::default()`
already being `typesafe` and uses `requires_receipts =
["proxy:decider:typesafe"]` to exclude (not silently mismeasure) any trial
that ran under an operator-set `helper` decider instead.

Every ten-manifest `max_spend_usd` sums to $3.55, under the $4.00 operator
cap on Jev spend.

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
  {spec} --out {out} [--reps K]` -- any id in `decision-cases/inputs.jsonl`.
  `--reps` defaults to 1 (byte-identical to the original single-call
  output); `K > 1` runs `K` uncached intake calls in the same trial state
  dir and reports a modal-share `quality` alongside the mean `correctness`
  -- see "Jev determinism campaigns" above.
- **`command` (jev_probe_trial.py)**: `python jev_probe_trial.py --trial
  {spec} --out {out} --reps K` -- any id in `jev-cases/<floor_site>/
  cases.jsonl`, driving the (separately built) `zirv ctx jev probe` verb --
  see "Jev determinism campaigns" above.
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

A finished campaign writes `report.md` (see `sample-report.md`),
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
- **No paid campaign has been run** yet -- none of the non-fixture
  manifests here has been executed, so no gain is claimed. The five
  campaigns predating the jev-determinism batch have each passed `plan`;
  the ten `jev-determinism-*.toml` manifests parse as TOML and their
  corpora self-check (ids/labels/split counts), but `zirv workflow
  research plan` has not been run against them here -- they also depend on
  the separately-built `zirv ctx jev probe` verb, which does not exist in
  this worktree yet.
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
