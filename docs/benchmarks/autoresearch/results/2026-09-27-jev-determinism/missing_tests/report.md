# Autoresearch campaign: jev-determinism-missing_tests

**Verdict:** no improvement (nothing promoted)

## Provenance
- zirv version: 4.38.0
- manifest: `docs\benchmarks\autoresearch\campaigns\jev-determinism-missing_tests.toml`
- manifest sha256: 5f516c0aee61be943c9e2a8004463967d26a3c576811c1b136e24b496f25d1d2
- repo: `<repo>`
- baseline commit: 20341da2ee586e4821522fcf35a7f4505a789564
- corpus: `docs/benchmarks/autoresearch/jev-cases/missing_tests/corpus.toml`
- corpus version: 1
- billing: `Metered`
- route: harness `claude`, model `sonnet`
- cache mode: `cold`
- pressure: `natural`
- stratify: `None`
- evaluator version: 1
- evaluator fingerprint: 1efe56e6442f18702e69c12481687ead2887a9bb295b34a9874db4723c4ae50b
- price table as_of: 2026-09-01
- started at: 1790513053 (2026-09-27 12:44:13 UTC)
- finished at: 1790513099 (2026-09-27 12:44:59 UTC)

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
| spend (execution) | (part of total spend, below) | $0.0068 |
| spend (overhead) | (part of total spend, below) | $0.0000 |
| spend (total) | $0.10 | $0.0068 |
| calls | 1000 | 400 |
| trials | 200 | 80 |
| retries (per-trial cap: 1) | max attempt seen: 0 | 0 retry dispatches total |
| wall | 3600s | 46s |

## Spend
- execution: $0.0068 (what the trials' own arms cost -- what a candidate's cost axis is judged on)
- overhead: $0.0000 (judges, proposer -- counted against the campaign budget, never against a candidate's own cost)
- completeness: complete (a crash/timeout charged at its declared ceiling, or any trial with an unknown cost, makes this `partial`)

## Coverage and limitations
- seat_mode = `Single`: single-seat results are not orchestration evidence.
- runtime = `Meta`.
- single project family: `jev-missing_tests` -- this result does not generalize across project families.

## Candidates
| candidate | stage | verdict | rel_cost | rel_wall | d_correctness | d_quality |
|---|---|---|---|---|---|---|
| margin-010 | screen | discard | 0.0000 | -0.0099 | 0.0000 | 0.0000 |
| margin-030 | screen | discard | 0.0000 | -0.0234 | 0.0000 | 0.0000 |
| margin-040 | screen | discard | 0.0000 | 0.0138 | 0.0000 | 0.0000 |
| confidence-raised | screen | discard | 0.0000 | 0.0289 | 0.0000 | 0.0000 |

### `margin-010`
A much lower margin floor (missing_tests) trades away stability for more decisive Jev answers.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

### `margin-030`
A moderately higher margin floor (missing_tests) buys some stability without much lost coverage.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

### `margin-040`
A high margin floor (missing_tests) maximizes stability at the cost of coverage/correctness.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

### `confidence-raised`
Raising the measurement confidence floor (missing_tests) above its compiled default (0.0) improves stability without an unacceptable correctness cost.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

## Reproduction
1. Check out the exact baseline this campaign ran against:
```
git -C "<repo>" checkout 20341da2ee586e4821522fcf35a7f4505a789564
```
2. Run the same manifest, resuming this campaign directory if it stopped early (a fresh run without `--resume` starts a new campaign instead):
```
zirv workflow research run "docs\benchmarks\autoresearch\campaigns\jev-determinism-missing_tests.toml" --repo "<repo>" --dir "<campaigns>/missing_tests" --resume
```
