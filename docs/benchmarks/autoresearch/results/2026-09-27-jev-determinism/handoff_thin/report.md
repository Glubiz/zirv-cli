# Autoresearch campaign: jev-determinism-handoff_thin

**Verdict:** no improvement (nothing promoted)

## Provenance
- zirv version: 4.38.0
- manifest: `docs\benchmarks\autoresearch\campaigns\jev-determinism-handoff_thin.toml`
- manifest sha256: e7b5ae75d3afc5e8bdfeb4cf34ff86d26bf4521cdd6ae0744c5e1d2b9532dbe3
- repo: `<repo>`
- baseline commit: 590ecafbe511f254d64df24a4a703f3ddb185649
- corpus: `docs/benchmarks/autoresearch/jev-cases/handoff_thin/corpus.toml`
- corpus version: 1
- billing: `Metered`
- route: harness `claude`, model `sonnet`
- cache mode: `cold`
- pressure: `natural`
- stratify: `None`
- evaluator version: 1
- evaluator fingerprint: e25be7cb1fb8a3c09789db25dd824eb2b3f3ad4c75638a0eb5a75257a71c84e5
- price table as_of: 2026-09-01
- started at: 1790513736 (2026-09-27 12:55:36 UTC)
- finished at: 1790513771 (2026-09-27 12:56:11 UTC)

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
| spend (execution) | (part of total spend, below) | $0.0058 |
| spend (overhead) | (part of total spend, below) | $0.0000 |
| spend (total) | $0.45 | $0.0058 |
| calls | 4200 | 320 |
| trials | 420 | 32 |
| retries (per-trial cap: 1) | max attempt seen: 0 | 0 retry dispatches total |
| wall | 14400s | 35s |

## Spend
- execution: $0.0058 (what the trials' own arms cost -- what a candidate's cost axis is judged on)
- overhead: $0.0000 (judges, proposer -- counted against the campaign budget, never against a candidate's own cost)
- completeness: complete (a crash/timeout charged at its declared ceiling, or any trial with an unknown cost, makes this `partial`)

## Coverage and limitations
- seat_mode = `Single`: single-seat results are not orchestration evidence.
- runtime = `Meta`.
- single project family: `jev-handoff_thin` -- this result does not generalize across project families.

## Candidates
| candidate | stage | verdict | rel_cost | rel_wall | d_correctness | d_quality |
|---|---|---|---|---|---|---|
| conf-085 | screen | discard | 0.0000 | 0.0038 | -0.0062 | -0.0062 |

### `conf-085`
A 0.85 floor probes the lower edge of the tiny-handoff band (0.84-0.95); expected to flip more than 0.82 (control for the gap's position).

**screen**: discard
- reasons:
  - no material quality point-estimate improvement
- exclusions: none
- retries: 0 (across all stages)

## Reproduction
1. Check out the exact baseline this campaign ran against:
```
git -C "<repo>" checkout 590ecafbe511f254d64df24a4a703f3ddb185649
```
2. Run the same manifest, resuming this campaign directory if it stopped early (a fresh run without `--resume` starts a new campaign instead):
```
zirv workflow research run "docs\benchmarks\autoresearch\campaigns\jev-determinism-handoff_thin.toml" --repo "<repo>" --dir "<campaigns>/handoff_thin" --resume
```
