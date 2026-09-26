# Autoresearch for zirv (#799-#805) -- design

Status: accepted for implementation, 2026-09-26. Parent issue #799; children #800-#805.

## What we take from karpathy/autoresearch

Upstream (`program.md`): one editable surface (`train.py`), a fixed evaluator it
may not touch (`prepare.py`), a fixed per-run budget (5 min; kill at 10), baseline
first, one experiment per commit, a `results.tsv` ledger
(`commit, metric, memory, status keep|discard|crash, description`), keep if the
metric improves else reset, a simplicity criterion ("0.001 better with 20 hacky
lines is not worth it"), crash triage, and an indefinite loop.

zirv keeps the contract and replaces the parts that do not transfer:

| upstream | zirv |
|---|---|
| edit `train.py` | schema-validated policy overlay (allowlisted `ZIRV_CTX_*` keys) or an operator-declared, path-scoped source patch |
| fixed `prepare.py` evaluator | protected evaluator set (hash-pinned at campaign start, re-verified before every trial and before promotion) |
| `val_bpb`, lower is better | correctness + quality floors first, then cost and wall time as separate axes, paired statistics, no weighted score |
| single run per idea | paired baseline/candidate repetitions, screen (dev) -> validate (validation) -> one holdout confirmation |
| `results.tsv` | append-only `ledger.jsonl` + immutable `lock.json`; resumable |
| loop forever | explicit spend / wall / call / trial / retry / concurrency budgets with in-flight reservation |
| keep = `git commit` on the branch | a report + reviewable proposal (overlay snippet or patch) + rollback; never merged or applied |

## Architecture

```
zirv workflow research plan|run|status|report|spend   (Rust, src/commands/workflow/research/)
        |  manifest.toml  ->  lock.json + ledger.jsonl in <campaign dir>
        |  per trial: spec.json -> backend command -> <trial>/trial.json
        v
backend "command"  e.g. python docs/benchmarks/wrapped-vs-vanilla/run.py --trial spec.json --out DIR
                         python docs/benchmarks/autoresearch/decision_trial.py --trial spec.json --out DIR
backend "fixture"  scripted trial results (deterministic tests and the demo campaign)
        |
        v  every zirv process in a trial runs with ZIRV_CTX_STATE_DIR=<trial>/state and ZIRV_ATTR_* ids
existing usage seams (delegations.jsonl, jev-decisions/effects.jsonl, proxy-decisions.jsonl, outcomes)
        -> `zirv workflow research spend --state-dir <trial>/state --json` (the one reconciler)
```

No new billing ledger: attribution ids ride on the existing records; the
reconciler folds existing records.

## #800 Attribution and cost accounting (Rust)

- `src/commands/ctx/attribution.rs`: `Attribution { campaign, candidate, trial, task }`
  read from `ZIRV_ATTR_CAMPAIGN|CANDIDATE|TRIAL|TASK`. Opaque ids only:
  `[A-Za-z0-9._:-]{1,64}`, anything else is dropped (never raw task text).
  Serialized only when non-empty. Env propagates to child workers by inheritance.
- Route env exported by the launch seam to the agent process:
  `ZIRV_ROUTE_HARNESS|MODEL|TIER|EFFORT` (actual values, unset when unknown).
  (`ZIRV_CTX_MODEL` is already taken by `[handoff].model`.)
- `log::Delegation` rows gain `attribution` (filled in `append_delegation` from env)
  -> covers helpers, workers, intake proxy and Jev decision spend rows at once.
- Jev `DecisionRecord`/`EffectRecord` and `ProxyDecision` gain `attribution`.
- `OutcomeRow` schema v2, v1 rows still read (`#[serde(default)]`, `read_all` accepts
  1..=2): `kind: workflow|direct`, `session`, `attribution`, `harness`, `model`,
  `seat_tier` (actual, from `ZIRV_ROUTE_TIER`, else derived from the model via the
  handover ladder, else None), `effort`, `policy` fingerprint. Headless
  `zirv ctx exec` appends a `direct` row at exit when the session ran no workflow.
  Collection stays best-effort and never makes a provider call.
- Policy fingerprint: sha256 (16 hex) over the policy-relevant config subset
  (`[jev]` gates/floors/ttl, `[proxy]` confidence/margin, `[handover]` ladder,
  `[headless]` effort, `[score]` thresholds).
- Reconciler (pure): receipts -> `SpendReport { execution, overhead, by_source,
  completeness, duplicates_dropped, cached_calls }`.
  - Sources: `agent, intake, jev, helper, worker, judge, proposer`; `execution` =
    agent+intake+jev+helper+worker, `overhead` = judge+proposer.
  - Each money value keeps `reported_usd`, `estimated_usd` (+ `price_as_of`),
    `unknown_count`; unknown is never 0. `billing: metered|subscription|unknown`
    is carried, not inferred; a subscription request still counts its tokens.
  - Exactly once: dedupe by receipt id; for one session prefer the harness-
    reported agent cost over a token estimate of the same session; cumulative
    receipts (resume) are converted to increments per session; cached Jev hits
    count as calls with zero provider spend.
  - Filtering by attribution makes concurrent campaigns sharing a state dir
    unable to cross-attribute.

## #801 Evaluation contract

- `docs/benchmarks/wrapped-vs-vanilla/corpus.toml` (versioned): every task's
  `family`, `class` (mechanical|bounded|bug|feature|architecture|ambiguous|
  sensitive|long_session), `split` (dev|validation|holdout). Splits are by task
  group so near-duplicates stay on one side. Today there is one project family
  (ledgerlite); reports flag `single_family`.
- Protected evaluator set (manifest `evaluator.protected` globs + compiled-in
  defaults: run.py, grade.py, hidden/, rubric*, reference*, quality_rubric.md,
  corpus.toml, decision-case labels). Hash drift -> campaign stops
  `evaluator_tampered`, later trials are invalid. Hidden tests are removed from a
  trial repo right after grading.
- Holdout: only the single final candidate runs there; each use is logged;
  `holdout.max_uses` exceeded -> refuse and ask for a refreshed holdout.
  The proposer never sees validation/holdout task ids or per-task results.
- Paired design: pair = (task, rep, cohort); both arms run the same backend
  command with the same env except the overlay; the per-pair order is shuffled
  with a seeded RNG and pairs run adjacently. An env-fingerprint mismatch
  between arms excludes the pair.
- Failed/timeout trials stay in the denominators: correctness 0, quality 0,
  cost counted (actual or the declared ceiling if unknown -> cost incomplete).
- Cohorts (never pooled): runtime (meta|native), harness, model, cache mode
  (cold|warm), pressure (natural|forced), task class. Native is reported
  `unmeasured` while `zirv native` is release-gated. `seat_mode = single`
  results are labelled single-seat and never orchestration evidence; team /
  fan-out keys are refused unless `seat_mode = orchestration` (no suite yet ->
  unmeasured).
- Promotion gate per cohort (paired bootstrap, seeded splitmix64, percentile CI;
  validation confidence Bonferroni-adjusted by the number of validated
  candidates):
  1. `n_pairs < min_pairs` -> inconclusive.
  2. candidate mean correctness < `correctness_floor` or quality < `quality_floor` -> reject.
  3. non-inferiority: CI upper of delta-correctness < -margin -> reject; CI lower < -margin -> inconclusive (same for quality).
  4. benefit: relative delta CI upper < -`min_effect` on cost or wall. Cost is only
     usable when complete in both arms. No material win -> inconclusive (or
     reject if the point estimate is worse).
  5. a material win on one axis with a material loss on the other -> `tradeoff`
     (reported, never auto-accepted).
  6. required receipts missing -> trial `untriggered`, excluded; all excluded -> `unmeasured`.
- Report per cohort: n, success/timeout/error rates, correctness, quality, cost per
  successful task, median wall (p90 when n >= 10), execution vs experiment spend,
  completeness, exclusions, CI, decision + reasons, criteria, versions.
- Grader self-check (`run.py --check-graders`, no provider call): reference
  solution scores full, untouched template scores below full.

## #802 Runner

- Manifest (TOML, `deny_unknown_fields`, schema 1): id, baseline commit,
  evaluator {version, protected, self_check}, corpus, runtime, cache_mode,
  seat_mode, backend {kind, command|file, per_trial_ceiling_usd, timeout_secs,
  calls_per_trial}, route {harness, model}, budgets {max_spend_usd,
  max_wall_secs, max_calls, max_trials, max_retries, concurrency}, stages
  {screen, validate, holdout}, criteria, candidate_space {allow_env,
  allowed_models, source_patch {allowed_paths, build}}, `[[candidates]]`
  {id, hypothesis, mechanism, env, patch, requires_receipts}, optional proposer
  {harness, model, max_proposals, per_call_ceiling_usd}. Candidates cannot set
  budgets, criteria or evaluator fields.
- Compiled-in env allowlist (candidate keys must be in it AND in the manifest's
  `allow_env`): non-safety Jev gates, Jev floors for tunable sites,
  `ZIRV_CTX_JEV_CACHE_TTL_SECS`, `ZIRV_CTX_PROXY_MIN_CONFIDENCE|MIN_MARGIN`,
  `ZIRV_CTX_HANDOVER_<AGENT>_<TIER>` (value must be in `allowed_models`),
  `ZIRV_CTX_HEADLESS_EFFORT_*`, `ZIRV_CTX_SCORE_TOKEN_FLOOR_RATIO|TOKEN_CEILING_RATIO`.
  Always refused: approve/approve_allow/inject_screen/stop_verify/missing_tests/
  review/gates Jev gates, anything permission/sandbox/safety/credential/base_url/
  budget related.
- Source patches: `git apply --check` in a disposable detached worktree at the
  baseline commit; every touched path must match `allowed_paths` and none may be
  protected; then `build`; the built binary dir is passed as `{zirv_dir}`.
  The user's working tree and global settings are never touched.
- Loop (autoresearch, bounded): self-check -> baseline (dev) -> per candidate:
  screen on dev (paired with the baseline trials) -> discard or survive ->
  survivors validate (paired, interleaved) -> gate -> simplest accepted
  candidate (fewest overlay keys + patch lines among CI-overlapping winners) ->
  one holdout confirmation -> promoted | not promoted. No improvement is a
  valid result.
- Proposer (optional): `zirv agent <harness>` in an empty temp dir with its own
  state dir; sees only the candidate-space schema, dev aggregates and the
  results table; may only propose env overlays; output validated like any
  declared candidate; its spend is overhead.
- Budgets: before each dispatch reserve the declared ceiling (spend, calls) and
  the timeout (wall); stop scheduling when a reservation would exceed a cap;
  in-flight work finishes; unknown final spend is charged at the ceiling.
  Per-trial timeout kills the process tree.
- Ledger: `lock.json` (resolved manifest + hash, baseline sha, evaluator hashes,
  zirv version, price table as_of) written once; `ledger.jsonl` events
  (`trial_scheduled` before external execution, `trial_finished|failed`,
  `candidate_*`, `stage_decision`, `campaign_stopped`) appended + flushed.
  Resume replays the ledger, refuses a changed manifest, never re-runs a
  finished trial, and reconciles scheduled-but-unfinished trials from their
  `trial.json` (present -> finished; absent -> failed/unknown spend, retried only
  within `max_retries` and budget).
- Outputs in `<state>/research/<id>/` (or `--dir`): `report.md`, `report.json`,
  `results.tsv` (autoresearch-style summary), `proposal/overlay.toml` or
  `proposal/candidate.patch`, `proposal/ROLLBACK.md`. Never applied, merged,
  pushed or turned into a PR by the runner.

## #803 Jev calibration

- New `[jev.floors.<site>] min_confidence, min_margin` (REPO_FORBIDDEN, env
  `ZIRV_CTX_JEV_FLOOR_<SITE>_MIN_CONFIDENCE|_MIN_MARGIN`) for tunable sites only:
  memory, context, harvest_screen, handoff_select, compaction_select, dispatch,
  launch_effort, classify, inject. Defaults equal today's constants (byte-identical
  behaviour when unset). Safety/verification sites keep compiled constants.
- Labelled intake decision cases (`docs/benchmarks/autoresearch/decision-cases/`)
  with tags ambiguous / misleading_metadata / insufficient_facts / costly_error and
  per-split files; `decision_trial.py` drives the production path
  (`zirv ctx proxy --json --headless`) with an isolated state dir and
  `ZIRV_CTX_JEV_CACHE_TTL_SECS=0` for stability reps, and scores false escalation,
  unsafe under-selection, unnecessary clarification, abstention/coverage,
  stability, calls, latency, cost. A trial whose state dir has no Jev decision
  receipt did not run the production Jev path and is `untriggered`.
- Per-gate e2e campaigns reuse run.py's per-gate conditions via overlays.

## #804 Routing

Candidates vary the handover ladder, headless effort and proxy floors (existing
keys); `apply_security_risk_floor` and the frontier floor stay fixed; operator
overrides stay authoritative (the overlay is applied only inside the trial).
run.py trial strategy `escalate` (cheap first, escalate to a stronger model only
on a failed attempt judged by visible checks, never hidden tests) with the full
failed-attempt cost included. Cohorts by task class; single-seat only.

## #805 Context / review / compaction

Candidates over `compaction_select`, `handoff_select`, `review_reuse`, `memory`,
`context` gates + floors and `[score]` token ratios (the knobs #762 will own the
calibration algorithm for; the runner only evaluates them). Receipts
(`effect:compaction-select`, `effect:handoff-select`,
`effect:workflow-review-reuse`) are required per candidate. New long-session
chain task with earlier-decision recall, a changed requirement, an unresolved
failure and multi-module edits. Forced pressure (`pressure = forced`, lowered
ceiling ratio for both arms) is its own cohort. Memory budget (#760) and prompt
size (#772) have no runtime knob today and are reported as not adaptable.

## Non-goals

Automatic merging, applying proposals, changing defaults, a general orchestration
framework, an orchestration (multi-seat) suite, native execution while gated,
new benchmark project families, a paid campaign in this PR.
