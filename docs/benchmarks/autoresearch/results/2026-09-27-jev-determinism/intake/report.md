# Autoresearch campaign: jev-determinism-intake

**Verdict:** no improvement (nothing promoted)

## Provenance
- zirv version: 4.38.0
- manifest: `docs\benchmarks\autoresearch\campaigns\jev-determinism-intake.toml`
- manifest sha256: 9197a45b6b012ba7456d2679e549e50edf275ad60b942d01ae39e7035092f3de
- repo: `<repo>`
- baseline commit: b7469607ea5c0ed43285a01952ea0cc1a7414b5d
- corpus: `docs/benchmarks/autoresearch/decision-cases/corpus.toml`
- corpus version: 1
- billing: `Metered`
- route: harness `claude`, model `sonnet`
- cache mode: `cold`
- pressure: `natural`
- stratify: `None`
- evaluator version: 1
- evaluator fingerprint: 21047688cb20ee5438faace23f7a77a8e671ad8be63b6cf20752afc8502d141a
- price table as_of: 2026-09-01
- started at: 1790507516 (2026-09-27 11:11:56 UTC)
- finished at: 1790507714 (2026-09-27 11:15:14 UTC)

## Promotion criteria
Values actually used, from this manifest's own `[criteria]` table (never a hardcoded default once a manifest sets one):
| key | value |
|---|---|
| min_pairs | 6 |
| correctness_floor | 0.000 |
| quality_floor | 0.000 |
| max_correctness_regression | 0.050 |
| max_quality_regression | 0.020 |
| objective | quality |
| min_effect | 0.050 |
| confidence (base) | 0.900 |
| bootstrap_resamples | 2000 |

## Budgets
| cap | limit | used |
|---|---|---|
| spend (execution) | (part of total spend, below) | $0.0193 |
| spend (overhead) | (part of total spend, below) | $0.0000 |
| spend (total) | $0.40 | $0.0193 |
| calls | 2000 | 840 |
| trials | 400 | 168 |
| retries (per-trial cap: 1) | max attempt seen: 0 | 0 retry dispatches total |
| wall | 3600s | 198s |

## Spend
- execution: $0.0193 (what the trials' own arms cost -- what a candidate's cost axis is judged on)
- overhead: $0.0000 (judges, proposer -- counted against the campaign budget, never against a candidate's own cost)
- completeness: complete (a crash/timeout charged at its declared ceiling, or any trial with an unknown cost, makes this `partial`)

## Coverage and limitations
- seat_mode = `Single`: single-seat results are not orchestration evidence.
- runtime = `Meta`.
- single project family: `intake` -- this result does not generalize across project families.

## Candidates
| candidate | stage | verdict | rel_cost | rel_wall | d_correctness | d_quality |
|---|---|---|---|---|---|---|
| margin-010 | screen | discard | 0.0000 | -0.2768 | 0.0083 | 0.0167 |
| margin-030 | screen | discard | 0.0000 | -0.2781 | -0.0083 | -0.2167 |
| margin-040 | screen | discard | 0.0000 | -0.3161 | 0.0083 | 0.0167 |
| confidence-raised | screen | discard | 0.0000 | -0.2684 | 0.0042 | 0.0083 |
| margin-015 | screen | discard | 0.0000 | -0.2454 | 0.0083 | 0.0167 |
| margin-025 | screen | discard | 0.0000 | -0.2733 | -0.0000 | -0.0667 |

### `margin-010`
A much lower margin floor trades away stability for more decisive Jev/helper answers.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

### `margin-030`
A moderately higher margin floor buys some stability without much lost coverage.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

### `margin-040`
A high margin floor maximizes stability at the cost of coverage/correctness.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

### `confidence-raised`
Raising the confidence floor above its compiled default (0.5 -> 0.6) improves stability without an unacceptable correctness cost.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

### `margin-015`
A slightly lower margin floor keeps every acted field stable while acting on more Jev answers.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

### `margin-025`
A slightly higher margin floor removes the residual clarify flips without entering the ~0.3 margin cluster.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

## Reproduction
1. Check out the exact baseline this campaign ran against:
```
git -C "<repo>" checkout b7469607ea5c0ed43285a01952ea0cc1a7414b5d
```
2. Run the same manifest, resuming this campaign directory if it stopped early (a fresh run without `--resume` starts a new campaign instead):
```
zirv workflow research run "docs\benchmarks\autoresearch\campaigns\jev-determinism-intake.toml" --repo "<repo>" --dir "<campaigns>/intake" --resume
```
