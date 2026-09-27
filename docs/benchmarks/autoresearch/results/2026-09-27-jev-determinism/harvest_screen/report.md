# Autoresearch campaign: jev-determinism-harvest_screen

**Verdict:** no improvement (nothing promoted)

## Provenance
- zirv version: 4.38.0
- manifest: `docs\benchmarks\autoresearch\campaigns\jev-determinism-harvest_screen.toml`
- manifest sha256: 4e2375c61442645a8fee00de43cddccf082c66c1d2d7927361c2f1c84086b1d3
- repo: `<repo>`
- baseline commit: 7978bf6bbe853d83ce280759eb859f6c21b35120
- corpus: `docs/benchmarks/autoresearch/jev-cases/harvest_screen/corpus.toml`
- corpus version: 1
- billing: `Metered`
- route: harness `claude`, model `sonnet`
- cache mode: `cold`
- pressure: `natural`
- stratify: `None`
- evaluator version: 1
- evaluator fingerprint: eb0a10d1c1012fd08f43ab6c8bfa719aa700af30e27d6434315b055a960b2f05
- price table as_of: 2026-09-01
- started at: 1790511795 (2026-09-27 12:23:15 UTC)
- finished at: 1790511845 (2026-09-27 12:24:05 UTC)

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
| spend (execution) | (part of total spend, below) | $0.0064 |
| spend (overhead) | (part of total spend, below) | $0.0000 |
| spend (total) | $0.10 | $0.0064 |
| calls | 1000 | 400 |
| trials | 200 | 80 |
| retries (per-trial cap: 1) | max attempt seen: 0 | 0 retry dispatches total |
| wall | 3600s | 50s |

## Spend
- execution: $0.0064 (what the trials' own arms cost -- what a candidate's cost axis is judged on)
- overhead: $0.0000 (judges, proposer -- counted against the campaign budget, never against a candidate's own cost)
- completeness: complete (a crash/timeout charged at its declared ceiling, or any trial with an unknown cost, makes this `partial`)

## Coverage and limitations
- seat_mode = `Single`: single-seat results are not orchestration evidence.
- runtime = `Meta`.
- single project family: `jev-harvest_screen` -- this result does not generalize across project families.

## Candidates
| candidate | stage | verdict | rel_cost | rel_wall | d_correctness | d_quality |
|---|---|---|---|---|---|---|
| margin-010 | screen | discard | 0.0000 | -0.0062 | 0.0000 | 0.0000 |
| margin-030 | screen | discard | 0.0000 | -0.0155 | 0.0000 | 0.0000 |
| margin-040 | screen | discard | 0.0000 | -0.0042 | 0.0000 | 0.0000 |
| confidence-raised | screen | discard | 0.0000 | 0.0111 | 0.0000 | 0.0000 |

### `margin-010`
A much lower margin floor (harvest_screen) trades away stability for more decisive Jev answers.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

### `margin-030`
A moderately higher margin floor (harvest_screen) buys some stability without much lost coverage.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

### `margin-040`
A high margin floor (harvest_screen) maximizes stability at the cost of coverage/correctness.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

### `confidence-raised`
Raising the confidence floor (harvest_screen) above its compiled default (0.8) improves stability without an unacceptable correctness cost.

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
zirv workflow research run "docs\benchmarks\autoresearch\campaigns\jev-determinism-harvest_screen.toml" --repo "<repo>" --dir "<campaigns>/harvest_screen" --resume
```
