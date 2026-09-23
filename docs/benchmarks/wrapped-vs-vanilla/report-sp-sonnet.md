# zirv-vs-vanilla benchmark report

Runs found: 108 (errored/timed out: 0). Conditions: vanilla, zirv, zirv-proxy.

## Headline

| Metric | vanilla | zirv | zirv-proxy | change: zirv vs vanilla (%) | change: zirv-proxy vs vanilla (%) | higher is better? |
|---|---|---|---|---|---|---|
| Speed: mean wall_s (s) | 46.2 | 82.3 | 78.7 | +78.0% | +70.4% | lower |
| Speed: median wall_s (s) | 39.5 | 61.9 | 62.0 | +56.7% | +56.8% | lower |
| Cost: mean total_cost_usd ($) | 0.208 | 0.347 | 0.339 | +66.3% | +62.6% | lower |
| Cost: mean total tokens | 364048 | 736129 | 592126 | +102.2% | +62.7% | lower |
| Cost: mean output_tokens | 3587 | 5730 | 5481 | +59.7% | +52.8% | lower |
| Intelligence: mean score | 0.715 | 0.979 | 0.993 | +36.9% | +38.8% | higher |
| Intelligence: solve rate (score==1.0) | 0.694 | 0.917 | 0.972 | +32.0% | +40.0% | higher |
| Intelligence: mean visible_ok | 1.000 | 1.000 | 1.000 | +0.0% | +0.0% | higher |

## Per-task

| Task | vanilla mean score | vanilla mean cost | vanilla mean wall_s | vanilla n | zirv mean score | zirv mean cost | zirv mean wall_s | zirv n | zirv-proxy mean score | zirv-proxy mean cost | zirv-proxy mean wall_s | zirv-proxy n |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| t01_tiebreak | 1.000 | 0.142 | 29.2 | 3 | 1.000 | 0.222 | 51.6 | 3 | 1.000 | 0.156 | 32.9 | 3 |
| t02_pagination | 1.000 | 0.215 | 51.4 | 3 | 1.000 | 0.224 | 45.0 | 3 | 1.000 | 0.252 | 55.5 | 3 |
| t03_money | 1.000 | 0.287 | 62.3 | 3 | 1.000 | 0.277 | 58.2 | 3 | 1.000 | 0.276 | 58.5 | 3 |
| t04_budget | 0.000 | 0.216 | 52.2 | 3 | 1.000 | 0.515 | 123.7 | 3 | 1.000 | 0.563 | 90.5 | 3 |
| t05_dedupe | 1.000 | 0.176 | 36.1 | 3 | 1.000 | 0.290 | 57.6 | 3 | 1.000 | 0.250 | 58.1 | 3 |
| t06_currency | 0.000 | 0.154 | 25.4 | 3 | 1.000 | 0.574 | 129.9 | 3 | 1.000 | 0.585 | 87.1 | 3 |
| t07_redtest | 1.000 | 0.213 | 31.7 | 3 | 1.000 | 0.209 | 40.9 | 3 | 1.000 | 0.181 | 38.2 | 3 |
| t08_usage_doc | 1.000 | 0.147 | 33.1 | 3 | 1.000 | 0.357 | 92.5 | 3 | 1.000 | 0.519 | 140.8 | 3 |
| t09_count | 1.000 | 0.267 | 72.4 | 3 | 1.000 | 0.205 | 41.6 | 3 | 1.000 | 0.077 | 63.0 | 3 |
| t10_shares | 0.667 | 0.288 | 77.2 | 3 | 1.000 | 0.315 | 90.4 | 3 | 1.000 | 0.264 | 72.8 | 3 |
| t11_export | 0.000 | 0.188 | 42.4 | 3 | 1.000 | 0.736 | 191.9 | 3 | 1.000 | 0.726 | 200.6 | 3 |
| t12_deadcode | 0.917 | 0.209 | 41.1 | 3 | 0.750 | 0.237 | 63.8 | 3 | 0.917 | 0.218 | 46.8 | 3 |

## Features used (zirv / zirv-proxy runs)

| Condition | mean tool_calls | mean zirv workflow | mean zirv skill | mean zirv agent | mean zirv ctx | mean zirv other | mean subagents_spawned | mean permission_denials |
|---|---|---|---|---|---|---|---|---|
| zirv | 14.2 | 0.00 | 0.25 | 0.00 | 0.19 | 0.00 | 0.03 | 0.25 |
| zirv-proxy | 12.9 | 0.00 | 0.17 | 0.00 | 0.25 | 0.00 | 0.00 | 0.03 |

vanilla mean tool_calls (for comparison): 9.1

## zirv-proxy decisions per task

| Task | mode complexity | mode seat_tier | mode model_used | mode workflow | runs that started a workflow | n runs |
|---|---|---|---|---|---|---|
| t01_tiebreak | bounded | standard | sonnet | n/a | 0/3 | 3 |
| t02_pagination | bounded | standard | sonnet | bugfix | 3/3 | 3 |
| t03_money | bounded | standard | sonnet | bugfix | 3/3 | 3 |
| t04_budget | substantial | frontier | opus | feature | 3/3 | 3 |
| t05_dedupe | bounded | standard | sonnet | refactor | 3/3 | 3 |
| t06_currency | substantial | frontier | opus | feature | 3/3 | 3 |
| t07_redtest | bounded | standard | sonnet | bugfix | 3/3 | 3 |
| t08_usage_doc | bounded | standard | sonnet | documentation-runbook-change | 3/3 | 3 |
| t09_count | trivial | cheap | haiku | n/a | 0/3 | 3 |
| t10_shares | bounded | standard | sonnet | n/a | 0/3 | 3 |
| t11_export | bounded | standard | sonnet | feature | 3/3 | 3 |
| t12_deadcode | bounded | standard | sonnet | n/a | 0/3 | 3 |

## Robustness


Paired bootstrap 95% CI, mean(zirv) - mean(vanilla), 10000 resamples seed 0, paired by (task,rep):
  - score: diff=+0.2639, 95% CI [+0.1181, +0.4167] (n=36 pairs)
  - total_cost_usd: diff=+0.1381, 95% CI [+0.0772, +0.2043] (n=36 pairs)
  - wall_s: diff=+36.0419, 95% CI [+19.5017, +53.0893] (n=36 pairs)

Paired bootstrap 95% CI, mean(zirv-proxy) - mean(vanilla), 10000 resamples seed 0, paired by (task,rep):
  - score: diff=+0.2778, 95% CI [+0.1319, +0.4306] (n=36 pairs)
  - total_cost_usd: diff=+0.1304, 95% CI [+0.0584, +0.2046] (n=36 pairs)
  - wall_s: diff=+32.5187, 95% CI [+15.9853, +50.1505] (n=36 pairs)

Errored/timed-out runs per condition:
  - vanilla: 0 / 36
  - zirv: 0 / 36
  - zirv-proxy: 0 / 36
