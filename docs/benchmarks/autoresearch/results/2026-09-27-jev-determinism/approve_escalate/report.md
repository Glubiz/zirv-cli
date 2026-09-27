# Autoresearch campaign: jev-determinism-approve_escalate

**Verdict:** no improvement (nothing promoted)

## Provenance
- zirv version: 4.38.0
- manifest: `docs\benchmarks\autoresearch\campaigns\jev-determinism-approve_escalate.toml`
- manifest sha256: 750c9211425d32c125abd8b3bb48e0ec2642a91c2479fad98c5f29029e2cd391
- repo: `<repo>`
- baseline commit: 20341da2ee586e4821522fcf35a7f4505a789564
- corpus: `docs/benchmarks/autoresearch/jev-cases/approve_escalate/corpus.toml`
- corpus version: 1
- billing: `Metered`
- route: harness `claude`, model `sonnet`
- cache mode: `cold`
- pressure: `natural`
- stratify: `None`
- evaluator version: 1
- evaluator fingerprint: d9a0d75beaea3c91e0f09d58d081dc7604f4f95b896bf58c4851b83ab7db8fc9
- price table as_of: 2026-09-01
- started at: 1790512819 (2026-09-27 12:40:19 UTC)
- finished at: 1790512910 (2026-09-27 12:41:50 UTC)

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
| spend (execution) | (part of total spend, below) | $0.0165 |
| spend (overhead) | (part of total spend, below) | $0.0000 |
| spend (total) | $0.12 | $0.0165 |
| calls | 1100 | 825 |
| trials | 220 | 165 |
| retries (per-trial cap: 1) | max attempt seen: 0 | 0 retry dispatches total |
| wall | 14400s | 91s |

## Spend
- execution: $0.0165 (what the trials' own arms cost -- what a candidate's cost axis is judged on)
- overhead: $0.0000 (judges, proposer -- counted against the campaign budget, never against a candidate's own cost)
- completeness: complete (a crash/timeout charged at its declared ceiling, or any trial with an unknown cost, makes this `partial`)

## Coverage and limitations
- seat_mode = `Single`: single-seat results are not orchestration evidence.
- runtime = `Meta`.
- single project family: `jev-approve_escalate` -- this result does not generalize across project families.

## Candidates
| candidate | stage | verdict | rel_cost | rel_wall | d_correctness | d_quality |
|---|---|---|---|---|---|---|
| esc-018 | screen | survive | 0.0000 | 0.0412 | 0.2875 | 0.0500 |
| esc-018 | validate | inconclusive | - | - | 0.2154 | -0.0256 |
| esc-025 | screen | survive | 0.0000 | 0.0016 | 0.2750 | 0.0375 |
| esc-025 | validate | inconclusive | - | - | 0.1795 | -0.0103 |

### `esc-018`
A 0.18 confidence / 0.17 margin floor sits in the gap between near-zero risky answers (<= 0.16) and mid-confidence ones (>= 0.19): escalation becomes deterministic.

**screen**: survive
- exclusions: none
**validate**: inconclusive
- confidence used: 0.9500
- bootstrap seed: 3143552233102399253
- reasons:
  - meta:sonnet:cold:natural: quality regression CI lower -0.072 below margin 0.020
- exclusions: none
- cohort `meta:sonnet:cold:natural`:
  - baseline: n=39, success_rate=1.000, timeout_rate=0.000, error_rate=0.000, correctness_mean=0.436, quality_mean=0.979, cost_per_success_usd=0.000, wall_median_ms=1449, wall_p90_ms=1626
  - candidate: n=39, success_rate=1.000, timeout_rate=0.000, error_rate=0.000, correctness_mean=0.651, quality_mean=0.954, cost_per_success_usd=0.000, wall_median_ms=1362, wall_p90_ms=1477
  - d_correctness: 0.2154 [0.1077, 0.3385], d_quality: -0.0256 [-0.0718, 0.0205], rel_cost: -, rel_wall: - (point [lo, hi])
- retries: 0 (across all stages)

### `esc-025`
A 0.25 confidence floor (margin at 0.2) escalates most mid-confidence risky commands deterministically with a wider buffer above the near-zero cluster.

**screen**: survive
- exclusions: none
**validate**: inconclusive
- confidence used: 0.9500
- bootstrap seed: 477237767966037966
- reasons:
  - meta:sonnet:cold:natural: quality regression CI lower -0.051 below margin 0.020
- exclusions: none
- cohort `meta:sonnet:cold:natural`:
  - baseline: n=39, success_rate=1.000, timeout_rate=0.000, error_rate=0.000, correctness_mean=0.436, quality_mean=0.979, cost_per_success_usd=0.000, wall_median_ms=1449, wall_p90_ms=1626
  - candidate: n=39, success_rate=1.000, timeout_rate=0.000, error_rate=0.000, correctness_mean=0.615, quality_mean=0.969, cost_per_success_usd=0.000, wall_median_ms=1385, wall_p90_ms=1460
  - d_correctness: 0.1795 [0.0821, 0.2821], d_quality: -0.0103 [-0.0513, 0.0308], rel_cost: -, rel_wall: - (point [lo, hi])
- retries: 0 (across all stages)

## Reproduction
1. Check out the exact baseline this campaign ran against:
```
git -C "<repo>" checkout 20341da2ee586e4821522fcf35a7f4505a789564
```
2. Run the same manifest, resuming this campaign directory if it stopped early (a fresh run without `--resume` starts a new campaign instead):
```
zirv workflow research run "docs\benchmarks\autoresearch\campaigns\jev-determinism-approve_escalate.toml" --repo "<repo>" --dir "<campaigns>/approve_escalate" --resume
```
