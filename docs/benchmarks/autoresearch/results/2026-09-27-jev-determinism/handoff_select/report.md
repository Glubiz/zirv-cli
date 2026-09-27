# Autoresearch campaign: jev-determinism-handoff_select

**Verdict:** no improvement (nothing promoted)

## Provenance
- zirv version: 4.38.0
- manifest: `docs\benchmarks\autoresearch\campaigns\jev-determinism-handoff_select.toml`
- manifest sha256: da627db16bc66216d124632299d853fada1dc012b9b1f29d710baf7fa86f22bb
- repo: `<repo>`
- baseline commit: 7978bf6bbe853d83ce280759eb859f6c21b35120
- corpus: `docs/benchmarks/autoresearch/jev-cases/handoff_select/corpus.toml`
- corpus version: 1
- billing: `Metered`
- route: harness `claude`, model `sonnet`
- cache mode: `cold`
- pressure: `natural`
- stratify: `None`
- evaluator version: 1
- evaluator fingerprint: dcf2518c641517f662bd88c9146c7802f0c9b012ea892889446adc6e1a9d0cd4
- price table as_of: 2026-09-01
- started at: 1790511845 (2026-09-27 12:24:05 UTC)
- finished at: 1790511917 (2026-09-27 12:25:17 UTC)

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
| spend (execution) | (part of total spend, below) | $0.0183 |
| spend (overhead) | (part of total spend, below) | $0.0000 |
| spend (total) | $0.10 | $0.0183 |
| calls | 1000 | 625 |
| trials | 200 | 125 |
| retries (per-trial cap: 1) | max attempt seen: 0 | 0 retry dispatches total |
| wall | 3600s | 72s |

## Spend
- execution: $0.0183 (what the trials' own arms cost -- what a candidate's cost axis is judged on)
- overhead: $0.0000 (judges, proposer -- counted against the campaign budget, never against a candidate's own cost)
- completeness: complete (a crash/timeout charged at its declared ceiling, or any trial with an unknown cost, makes this `partial`)

## Coverage and limitations
- seat_mode = `Single`: single-seat results are not orchestration evidence.
- runtime = `Meta`.
- single project family: `jev-handoff_select` -- this result does not generalize across project families.

## Candidates
| candidate | stage | verdict | rel_cost | rel_wall | d_correctness | d_quality |
|---|---|---|---|---|---|---|
| margin-010 | screen | discard | 0.0000 | -0.0121 | -0.0750 | -0.0125 |
| margin-030 | screen | discard | 0.0000 | 0.0090 | -0.0750 | -0.0750 |
| margin-040 | screen | survive | 0.0000 | 0.0033 | -0.0250 | 0.0125 |
| margin-040 | validate | inconclusive | - | - | 0.0133 | -0.0133 |
| confidence-raised | screen | survive | 0.0000 | 0.0231 | 0.1125 | 0.0750 |
| confidence-raised | validate | inconclusive | 0.0000 | 0.0275 | 0.0933 | 0.0533 |

### `margin-010`
A much lower margin floor (handoff_select) trades away stability for more decisive Jev answers.

**screen**: discard
- reasons:
  - correctness regression vs baseline
- exclusions: none
- retries: 0 (across all stages)

### `margin-030`
A moderately higher margin floor (handoff_select) buys some stability without much lost coverage.

**screen**: discard
- reasons:
  - correctness regression vs baseline
- exclusions: none
- retries: 0 (across all stages)

### `margin-040`
A high margin floor (handoff_select) maximizes stability at the cost of coverage/correctness.

**screen**: survive
- exclusions: none
**validate**: inconclusive
- confidence used: 0.9500
- bootstrap seed: 3991896325322661592
- reasons:
  - meta:sonnet:cold:natural: quality regression CI lower -0.040 below margin 0.020
- exclusions: none
- cohort `meta:sonnet:cold:natural`:
  - baseline: n=15, success_rate=1.000, timeout_rate=0.000, error_rate=0.000, correctness_mean=0.740, quality_mean=0.947, cost_per_success_usd=0.000, wall_median_ms=1438, wall_p90_ms=1506
  - candidate: n=15, success_rate=1.000, timeout_rate=0.000, error_rate=0.000, correctness_mean=0.753, quality_mean=0.933, cost_per_success_usd=0.000, wall_median_ms=1399, wall_p90_ms=1512
  - d_correctness: 0.0133 [0.0000, 0.0400], d_quality: -0.0133 [-0.0400, 0.0000], rel_cost: -, rel_wall: - (point [lo, hi])
- retries: 0 (across all stages)

### `confidence-raised`
Raising the confidence floor (handoff_select) above its compiled default (0.0) improves stability without an unacceptable correctness cost.

**screen**: survive
- exclusions: none
**validate**: inconclusive
- confidence used: 0.9500
- bootstrap seed: 3815352144415053972
- reasons:
  - meta:sonnet:cold:natural: no material quality win
- exclusions: none
- cohort `meta:sonnet:cold:natural`:
  - baseline: n=15, success_rate=1.000, timeout_rate=0.000, error_rate=0.000, correctness_mean=0.740, quality_mean=0.947, cost_per_success_usd=0.000, wall_median_ms=1438, wall_p90_ms=1506
  - candidate: n=15, success_rate=1.000, timeout_rate=0.000, error_rate=0.000, correctness_mean=0.833, quality_mean=1.000, cost_per_success_usd=0.000, wall_median_ms=1459, wall_p90_ms=1604
  - d_correctness: 0.0933 [0.0000, 0.2267], d_quality: 0.0533 [0.0000, 0.1200], rel_cost: 0.0000 [0.0000, 0.0000], rel_wall: 0.0275 [-0.0082, 0.0692] (point [lo, hi])
- retries: 0 (across all stages)

## Reproduction
1. Check out the exact baseline this campaign ran against:
```
git -C "<repo>" checkout 7978bf6bbe853d83ce280759eb859f6c21b35120
```
2. Run the same manifest, resuming this campaign directory if it stopped early (a fresh run without `--resume` starts a new campaign instead):
```
zirv workflow research run "docs\benchmarks\autoresearch\campaigns\jev-determinism-handoff_select.toml" --repo "<repo>" --dir "<campaigns>/handoff_select" --resume
```
