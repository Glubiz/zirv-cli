# Jev determinism campaigns, 2026-09-27

**Outcome: no floor default changes.** Every tunable Jev floor was swept with
the autoresearch runner under `[criteria] objective = "quality"` (quality =
how often K uncached repetitions of the same request lead production to the
same action). No candidate cleared the promotion gate. For 8 of the 11
measured sites the compiled floors are already fully deterministic on this
corpus. The two sites that still flip, handoff-thin and the intake clarify
decision, flip because Jev's own confidence/margin varies from call to call
on an identical request. No floor removes that without switching the feature
off or regressing correctness.

Total Jev spend for the final run: **$0.137** (11 campaigns, 1,008 trials, all
uncached). Development runs brought the total for the whole exercise to
about $0.31.

## How it was measured

- `zirv ctx jev probe` sends one site's production question(s) for a fixture
  input K times with the cache off. It applies that site's production floor
  and answer-to-action rule, and reports what production would have done on
  each repetition.
- `jev_probe_trial.py` (K=5, K=10 for handoff-thin) scores `quality` as the
  mean per-item modal share of those actions, and `correctness` as agreement
  with the labelled action in `jev-cases/<site>/labels.jsonl`.
- `decision_trial.py --reps 5` does the same for intake. It compares every
  field production acts on (`ACTED_DECISION_FIELDS` plus the derived
  `clarify` boolean), never the raw probabilities.
- Candidates are env overlays of `ZIRV_CTX_JEV_FLOOR_<SITE>_MIN_CONFIDENCE|
  _MIN_MARGIN` or `ZIRV_CTX_PROXY_MIN_CONFIDENCE|_MIN_MARGIN`. Safety and
  verification sites (approve, stop_verify, inject_screen, missing_tests,
  review) keep compiled floors by design and were not swept.
- Model: `jev-1.13.0` (the pinned `[proxy.typesafe] model`).

## Results (final run, dev split screen; stability / correctness)

| campaign | baseline (compiled floors) | best candidate | verdict |
|---|---|---|---|
| memory | 1.000 / 0.71 | all equal | no change |
| context | 1.000 / 0.73 | all equal | no change |
| harvest_screen | 1.000 / 0.50 | all equal | no change |
| compaction_select | 1.000 / 0.65 | margin 0.10 is worse (0.945 / 0.42) | no change |
| dispatch | 1.000 / 0.12 | all equal | no change |
| launch_effort | 1.000 / 0.25 | all equal | no change |
| classify (domain tags) | 1.000 / 0.90 | all equal | no change |
| inject | 1.000 / 0.62 | all equal | no change |
| handoff_select (select + thin) | 0.908 / 0.78 | confidence 0.6: 1.000 / 0.90 | inconclusive at validate (see below) |
| handoff_thin (round 2) | 0.969 / 0.72 | none better | no change |
| intake | 0.983 / 0.62 | margin 0.10/0.15/0.40: 1.000 / 0.62 | discarded at screen (gain < min_effect/2) |

Each campaign's `report.md`, `report.json` and `results.tsv` are in its own
directory here. Low correctness values (dispatch, launch_effort) mean the
hand-written labels disagree with Jev's consistent answer. They are not
instability, and correctness only guards against regression here.

## handoff-thin: noisy confidence, no deterministic floor

`HANDOFF_THIN_FLOOR` (0.9) demotes a distilled handoff when Jev answers
`thin` with confidence at or above it. Jev's answer value is stable for
small handoffs, but its confidence on the identical request spreads widely:

| case | facts [task, next_step, constraints, files, blocked] | answers | `thin` confidence range |
|---|---|---|---|
| hsel-007 | [11, 5, 0, 0, 0] | 50/50 thin | 0.84-0.94 |
| hsel-003 | [14, 5, 0, 0, 0] | 50/50 thin | 0.84-0.93 |
| ht-001 | [18, 6, 0, 1, 0] | 100/100 thin | 0.46-0.79 |
| ht-002 | [25, 9, 5, 0, 1] | 93/93 thin | 0.30-0.67 |
| ht-003 | [35, 12, 0, 0, 0] | 100/100 thin | 0.85-0.95 |
| ht-004 | [35, 12, 0, 4, 0] | 96/96 thin | 0.46-0.81 |
| ht-005 | [85, 25, 0, 0, 0] | 100/100 thin | 0.60-0.90 |
| ht-007 | [190, 65, 20, 1, 0] | 43/100 thin | 0.00-0.19 |
| hsel-004 | [1205, 242, 190, 5, 0] | 14/50 thin | 0.00-0.18 |

Any floor between about 0.3 and 0.95 cuts through at least one case's band.
The 0.9 default flips the tiniest handoffs; 0.5-0.8 stabilise those but
destabilise small ones. The `handoff_select` campaign's apparent win for
confidence 0.6 came from its thin cases all being tiny. The focused
`handoff_thin` round, which spans the size range, refuted it: every lower
floor was less stable than 0.9. A floor above Jev's observed ceiling (0.95)
would be deterministic only because demotion would never happen.

## Intake: the clarify decision's margin sits near 0.2-0.3

Every flip in the intake campaigns was the `clarify` decision; intent,
workflow, complexity, risk, tiers and domains never flipped. Stability by
`ZIRV_CTX_PROXY_MIN_MARGIN`: 0.10 → 1.000, 0.15 → 1.000, **0.20 (default)
→ 0.983**, 0.25 → 0.917, 0.30 → 0.767, 0.40 → 1.000. Jev's clarify margins
cluster around 0.3, so 0.3 is the worst possible floor. An earlier run
(`intake` round 3, validate split) showed the same shape: default 0.954,
0.10 and 0.40 both 1.000, with the correctness-regression CI lower bound at
-0.058 against the 0.05 limit, so inconclusive. Lowering the shared margin
is also ruled out independently: `jev::DEFAULT_MIN_MARGIN` has a
compile-time bound (> 0.14) from the 2026-09-18 measurement of intent,
workflow and architecture flips. Raising it to 0.4 would make most
Jev-driven intake fields fall back.

## What would change these conclusions

- A clarify-specific margin floor (a source change, not an env knob) could
  move clarify off its 0.2-0.3 cluster without touching the other intake
  fields. It can be evaluated as a `[candidate_space.source_patch]` campaign.
- handoff-thin needs a different signal, not a different floor: for
  example, acting on the answer value when it is unanimous, or a majority of
  N calls. Both need source changes.
- Corpora are hand-written, metadata-only fixtures (16-24 cases per site).
  A site that is stable here can still flip on inputs near its own
  thresholds that this corpus does not cover.
