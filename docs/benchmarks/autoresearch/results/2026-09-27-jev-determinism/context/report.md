# Autoresearch campaign: jev-determinism-context

**Verdict:** no improvement (nothing promoted)

## Provenance
- zirv version: 4.38.0
- manifest: `docs\benchmarks\autoresearch\campaigns\jev-determinism-context.toml`
- manifest sha256: 4896d687c720a818ae3fd21b5725ac7c4d1f3c7d653156bd7e51e4eea66803dc
- repo: `<repo>`
- baseline commit: 43999822a3bf6f816bdd80949ceb22466b7a0cb8
- corpus: `docs/benchmarks/autoresearch/jev-cases/context/corpus.toml`
- corpus version: 1
- billing: `Metered`
- route: harness `claude`, model `sonnet`
- cache mode: `cold`
- pressure: `natural`
- stratify: `None`
- evaluator version: 1
- evaluator fingerprint: 9463b8345169b47b6cf5b1a902daac82f729fe28dc9c51e60adfb7716fff9db9
- price table as_of: 2026-09-01
- started at: 1790506993 (2026-09-27 11:03:13 UTC)
- finished at: 1790507040 (2026-09-27 11:04:00 UTC)

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
| spend (execution) | (part of total spend, below) | $0.0131 |
| spend (overhead) | (part of total spend, below) | $0.0000 |
| spend (total) | $0.35 | $0.0131 |
| calls | 1000 | 400 |
| trials | 200 | 80 |
| retries (per-trial cap: 1) | max attempt seen: 0 | 0 retry dispatches total |
| wall | 3600s | 47s |

## Spend
- execution: $0.0131 (what the trials' own arms cost -- what a candidate's cost axis is judged on)
- overhead: $0.0000 (judges, proposer -- counted against the campaign budget, never against a candidate's own cost)
- completeness: complete (a crash/timeout charged at its declared ceiling, or any trial with an unknown cost, makes this `partial`)

## Coverage and limitations
- seat_mode = `Single`: single-seat results are not orchestration evidence.
- runtime = `Meta`.
- single project family: `jev-context` -- this result does not generalize across project families.

## Candidates
| candidate | stage | verdict | rel_cost | rel_wall | d_correctness | d_quality |
|---|---|---|---|---|---|---|
| margin-010 | screen | discard | 0.0000 | -0.0442 | 0.0000 | 0.0000 |
| margin-030 | screen | discard | 0.0000 | -0.0158 | 0.0000 | 0.0000 |
| margin-040 | screen | discard | 0.0000 | -0.0350 | 0.0000 | 0.0000 |
| confidence-raised | screen | discard | 0.0000 | -0.0451 | 0.0000 | 0.0000 |

### `margin-010`
A much lower margin floor (context) trades away stability for more decisive Jev answers.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

### `margin-030`
A moderately higher margin floor (context) buys some stability without much lost coverage.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

### `margin-040`
A high margin floor (context) maximizes stability at the cost of coverage/correctness.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

### `confidence-raised`
Raising the confidence floor (context) above its compiled default (0.0) improves stability without an unacceptable correctness cost.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

## Reproduction
1. Check out the exact baseline this campaign ran against:
```
git -C "<repo>" checkout 43999822a3bf6f816bdd80949ceb22466b7a0cb8
```
2. Run the same manifest, resuming this campaign directory if it stopped early (a fresh run without `--resume` starts a new campaign instead):
```
zirv workflow research run "docs\benchmarks\autoresearch\campaigns\jev-determinism-context.toml" --repo "<repo>" --dir "<campaigns>/context" --resume
```
