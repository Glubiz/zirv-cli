# Autoresearch campaign: jev-determinism-intake

**Verdict:** no improvement (nothing promoted)

## Provenance
- zirv version: 4.38.0
- manifest: `docs\benchmarks\autoresearch\campaigns\jev-determinism-intake.toml`
- manifest sha256: 072662fc6bf12707a1a1ab3de7fb60045d048a9b375a00dea21f30402bc93d42
- repo: `<repo>`
- baseline commit: 7978bf6bbe853d83ce280759eb859f6c21b35120
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
- started at: 1790512332 (2026-09-27 12:32:12 UTC)
- finished at: 1790512722 (2026-09-27 12:38:42 UTC)

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
| min_effect | 0.010 |
| confidence (base) | 0.900 |
| bootstrap_resamples | 2000 |

## Budgets
| cap | limit | used |
|---|---|---|
| spend (execution) | (part of total spend, below) | $0.0386 |
| spend (overhead) | (part of total spend, below) | $0.0000 |
| spend (total) | $0.30 | $0.0386 |
| calls | 3000 | 1680 |
| trials | 600 | 336 |
| retries (per-trial cap: 1) | max attempt seen: 0 | 0 retry dispatches total |
| wall | 3600s | 390s |

## Spend
- execution: $0.0386 (what the trials' own arms cost -- what a candidate's cost axis is judged on)
- overhead: $0.0000 (judges, proposer -- counted against the campaign budget, never against a candidate's own cost)
- completeness: complete (a crash/timeout charged at its declared ceiling, or any trial with an unknown cost, makes this `partial`)

## Coverage and limitations
- seat_mode = `Single`: single-seat results are not orchestration evidence.
- runtime = `Meta`.
- single project family: `intake` -- this result does not generalize across project families.

## Candidates
| candidate | stage | verdict | rel_cost | rel_wall | d_correctness | d_quality |
|---|---|---|---|---|---|---|
| margin-010 | screen | survive | 0.0000 | 0.0370 | 0.0125 | 0.0167 |
| margin-010 | validate | inconclusive | - | - | -0.0250 | - |
| margin-030 | screen | discard | 0.0000 | 0.0538 | -0.0208 | -0.2667 |
| margin-040 | screen | survive | 0.0000 | 0.0406 | 0.0125 | 0.0167 |
| margin-040 | validate | inconclusive | - | - | -0.0194 | - |
| confidence-raised | screen | discard | 0.0000 | 0.0329 | -0.0042 | -0.0167 |
| margin-015 | screen | survive | 0.0000 | 0.0122 | 0.0125 | 0.0167 |
| margin-015 | validate | inconclusive | - | - | -0.0167 | - |
| margin-025 | screen | discard | 0.0000 | 0.0377 | -0.0250 | -0.0083 |
| margin-017 | screen | discard | 0.0000 | 0.0412 | 0.0042 | 0.0000 |

### `margin-010`
A much lower margin floor trades away stability for more decisive Jev/helper answers.

**screen**: survive
- exclusions: none
**validate**: inconclusive
- confidence used: 0.9667
- bootstrap seed: 2718484640359507417
- reasons:
  - meta:sonnet:cold:natural: correctness regression CI lower -0.072 below margin 0.050
- exclusions: none
- cohort `meta:sonnet:cold:natural`:
  - baseline: n=36, success_rate=1.000, timeout_rate=0.000, error_rate=0.000, correctness_mean=0.650, quality_mean=0.950, cost_per_success_usd=0.000, wall_median_ms=3251, wall_p90_ms=3546
  - candidate: n=36, success_rate=1.000, timeout_rate=0.000, error_rate=0.000, correctness_mean=0.625, quality_mean=1.000, cost_per_success_usd=0.000, wall_median_ms=3218, wall_p90_ms=3535
  - d_correctness: -0.0250 [-0.0722, 0.0139], d_quality: -, rel_cost: -, rel_wall: - (point [lo, hi])
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

**screen**: survive
- exclusions: none
**validate**: inconclusive
- confidence used: 0.9667
- bootstrap seed: 12964310675060901756
- reasons:
  - meta:sonnet:cold:natural: correctness regression CI lower -0.189 below margin 0.050
- exclusions: none
- cohort `meta:sonnet:cold:natural`:
  - baseline: n=36, success_rate=1.000, timeout_rate=0.000, error_rate=0.000, correctness_mean=0.650, quality_mean=0.950, cost_per_success_usd=0.000, wall_median_ms=3251, wall_p90_ms=3546
  - candidate: n=36, success_rate=1.000, timeout_rate=0.000, error_rate=0.000, correctness_mean=0.631, quality_mean=0.989, cost_per_success_usd=0.000, wall_median_ms=3400, wall_p90_ms=3906
  - d_correctness: -0.0194 [-0.1889, 0.1472], d_quality: -, rel_cost: -, rel_wall: - (point [lo, hi])
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

**screen**: survive
- exclusions: none
**validate**: inconclusive
- confidence used: 0.9667
- bootstrap seed: 5082775844212490992
- reasons:
  - meta:sonnet:cold:natural: correctness regression CI lower -0.058 below margin 0.050
- exclusions: none
- cohort `meta:sonnet:cold:natural`:
  - baseline: n=36, success_rate=1.000, timeout_rate=0.000, error_rate=0.000, correctness_mean=0.650, quality_mean=0.950, cost_per_success_usd=0.000, wall_median_ms=3251, wall_p90_ms=3546
  - candidate: n=36, success_rate=1.000, timeout_rate=0.000, error_rate=0.000, correctness_mean=0.633, quality_mean=0.983, cost_per_success_usd=0.000, wall_median_ms=3416, wall_p90_ms=3695
  - d_correctness: -0.0167 [-0.0583, 0.0167], d_quality: -, rel_cost: -, rel_wall: - (point [lo, hi])
- retries: 0 (across all stages)

### `margin-025`
A slightly higher margin floor removes the residual clarify flips without entering the ~0.3 margin cluster.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

### `margin-017`
The lowest margin that still clears every stable answer the 2026-09-18 measurement saw (>= 0.17) removes the residual clarify flips.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

## Reproduction
1. Check out the exact baseline this campaign ran against:
```
git -C "<repo>" checkout 7978bf6bbe853d83ce280759eb859f6c21b35120
```
2. Run the same manifest, resuming this campaign directory if it stopped early (a fresh run without `--resume` starts a new campaign instead):
```
zirv workflow research run "docs\benchmarks\autoresearch\campaigns\jev-determinism-intake.toml" --repo "<repo>" --dir "<campaigns>/intake" --resume
```
