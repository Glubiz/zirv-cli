# Autoresearch campaign: jev-determinism-handoff_thin

**Verdict:** no improvement (nothing promoted)

## Provenance
- zirv version: 4.38.0
- manifest: `docs\benchmarks\autoresearch\campaigns\jev-determinism-handoff_thin.toml`
- manifest sha256: e890cb57e33f8cbbfb2eff00e2983aadffc1edf3544f40223890ea9f33447ada
- repo: `<repo>`
- baseline commit: 43999822a3bf6f816bdd80949ceb22466b7a0cb8
- corpus: `docs/benchmarks/autoresearch/jev-cases/handoff_thin/corpus.toml`
- corpus version: 1
- billing: `Metered`
- route: harness `claude`, model `sonnet`
- cache mode: `cold`
- pressure: `natural`
- stratify: `None`
- evaluator version: 1
- evaluator fingerprint: 9a5c744d034a750899655bc977a435576ed6292b8b4ba693e9a170a5ff0564f5
- price table as_of: 2026-09-01
- started at: 1790507158 (2026-09-27 11:05:58 UTC)
- finished at: 1790507282 (2026-09-27 11:08:02 UTC)

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
| spend (execution) | (part of total spend, below) | $0.0142 |
| spend (overhead) | (part of total spend, below) | $0.0000 |
| spend (total) | $0.40 | $0.0142 |
| calls | 2500 | 800 |
| trials | 250 | 80 |
| retries (per-trial cap: 1) | max attempt seen: 0 | 0 retry dispatches total |
| wall | 3600s | 124s |

## Spend
- execution: $0.0142 (what the trials' own arms cost -- what a candidate's cost axis is judged on)
- overhead: $0.0000 (judges, proposer -- counted against the campaign budget, never against a candidate's own cost)
- completeness: complete (a crash/timeout charged at its declared ceiling, or any trial with an unknown cost, makes this `partial`)

## Coverage and limitations
- seat_mode = `Single`: single-seat results are not orchestration evidence.
- runtime = `Meta`.
- single project family: `jev-handoff_thin` -- this result does not generalize across project families.

## Candidates
| candidate | stage | verdict | rel_cost | rel_wall | d_correctness | d_quality |
|---|---|---|---|---|---|---|
| conf-050 | screen | discard | 0.0738 | -0.6925 | 0.0687 | -0.0312 |
| conf-060 | screen | discard | 0.0738 | -0.7063 | 0.0938 | -0.0312 |
| conf-070 | screen | discard | 0.0738 | -0.7038 | -0.0313 | -0.0312 |
| conf-080 | screen | discard | 0.0738 | -0.7041 | -0.0562 | -0.0063 |

### `conf-050`
A confidence floor of 0.5 (handoff_select) starts to filter round 1's tiny-cluster answers (0.82-0.95) without touching the untested intermediate range.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

### `conf-060`
A confidence floor of 0.6 (handoff_select) sits inside round 1's observed intermediate/medium cluster (0.0-0.37 to 0.82-0.95 gap) and may start admitting some of those rows.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

### `conf-070`
A confidence floor of 0.7 (handoff_select) trades more admitted rows for stability, testing the upper half of the 0.4-0.8 gap this corpus was built to probe.

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

### `conf-080`
A confidence floor of 0.8 (handoff_select) stays close to the compiled HANDOFF_THIN_FLOOR (0.9), checking whether a slightly lower floor still captures the tiny-cluster wins at less correctness cost.

**screen**: discard
- reasons:
  - correctness regression vs baseline
- exclusions: none
- retries: 0 (across all stages)

## Reproduction
1. Check out the exact baseline this campaign ran against:
```
git -C "<repo>" checkout 43999822a3bf6f816bdd80949ceb22466b7a0cb8
```
2. Run the same manifest, resuming this campaign directory if it stopped early (a fresh run without `--resume` starts a new campaign instead):
```
zirv workflow research run "docs\benchmarks\autoresearch\campaigns\jev-determinism-handoff_thin.toml" --repo "<repo>" --dir "<campaigns>/handoff_thin" --resume
```
