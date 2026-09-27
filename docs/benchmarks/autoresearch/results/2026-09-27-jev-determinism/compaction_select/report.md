# Autoresearch campaign: jev-determinism-compaction_select

**Verdict:** no improvement (nothing promoted)

## Provenance
- zirv version: 4.38.0
- manifest: `docs\benchmarks\autoresearch\campaigns\jev-determinism-compaction_select.toml`
- manifest sha256: 6cca39fd67bbe24d824b76e540fc09ba4f980bae7cbe10aefd6ab4188312c0b7
- repo: `<repo>`
- baseline commit: b7469607ea5c0ed43285a01952ea0cc1a7414b5d
- corpus: `docs/benchmarks/autoresearch/jev-cases/compaction_select/corpus.toml`
- corpus version: 1
- billing: `Metered`
- route: harness `claude`, model `sonnet`
- cache mode: `cold`
- pressure: `natural`
- stratify: `None`
- evaluator version: 1
- evaluator fingerprint: 9ff150d45a1c60f274e2a5765f2b92a7f37af8d9e303f7a4838507fad9f94470
- price table as_of: 2026-09-01
- started at: 1790507282 (2026-09-27 11:08:02 UTC)
- finished at: 1790507330 (2026-09-27 11:08:50 UTC)

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
| spend (execution) | (part of total spend, below) | $0.0181 |
| spend (overhead) | (part of total spend, below) | $0.0000 |
| spend (total) | $0.35 | $0.0181 |
| calls | 1000 | 400 |
| trials | 200 | 80 |
| retries (per-trial cap: 1) | max attempt seen: 0 | 0 retry dispatches total |
| wall | 3600s | 48s |

## Spend
- execution: $0.0181 (what the trials' own arms cost -- what a candidate's cost axis is judged on)
- overhead: $0.0000 (judges, proposer -- counted against the campaign budget, never against a candidate's own cost)
- completeness: complete (a crash/timeout charged at its declared ceiling, or any trial with an unknown cost, makes this `partial`)

## Coverage and limitations
- seat_mode = `Single`: single-seat results are not orchestration evidence.
- runtime = `Meta`.
- single project family: `jev-compaction_select` -- this result does not generalize across project families.

## Candidates
| candidate | stage | verdict | rel_cost | rel_wall | d_correctness | d_quality |
|---|---|---|---|---|---|---|
| margin-010 | screen | discard | 0.0000 | -0.0135 | -0.2375 | -0.0546 |
| margin-030 | screen | discard | 0.0000 | -0.0049 | 0.0000 | 0.0000 |
| margin-040 | screen | discard | 0.0000 | 0.0378 | 0.0000 | 0.0000 |
| confidence-raised | screen | discard | 0.0000 | 0.0049 | 0.0000 | 0.0000 |

### `margin-010`
A much lower margin floor (compaction_select) trades away stability for more decisive Jev answers.

**screen**: discard
- reasons:
  - correctness regression vs baseline
- exclusions: none
- retries: 0 (across all stages)

### `margin-030`
A moderately higher margin floor (compaction_select) buys some stability without much lost coverage.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

### `margin-040`
A high margin floor (compaction_select) maximizes stability at the cost of coverage/correctness.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

### `confidence-raised`
Raising the confidence floor (compaction_select) above its compiled default (0.0) improves stability without an unacceptable correctness cost.

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
zirv workflow research run "docs\benchmarks\autoresearch\campaigns\jev-determinism-compaction_select.toml" --repo "<repo>" --dir "<campaigns>/compaction_select" --resume
```
