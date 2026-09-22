# zirv-vs-vanilla benchmark report

Runs found: 108 (errored/timed out: 0). Conditions: vanilla, zirv, zirv-proxy.

## Headline

| Metric | vanilla | zirv | zirv-proxy | change: zirv vs vanilla (%) | change: zirv-proxy vs vanilla (%) | higher is better? |
|---|---|---|---|---|---|---|
| Speed: mean wall_s (s) | 55.3 | 76.9 | 68.5 | +39.1% | +23.8% | lower |
| Speed: median wall_s (s) | 40.9 | 54.8 | 54.8 | +33.8% | +33.8% | lower |
| Cost: mean total_cost_usd ($) | 0.236 | 0.332 | 0.348 | +40.6% | +47.2% | lower |
| Cost: mean total tokens | 456353 | 706855 | 518549 | +54.9% | +13.6% | lower |
| Cost: mean output_tokens | 4640 | 5510 | 4901 | +18.8% | +5.6% | lower |
| Intelligence: mean score | 0.986 | 1.000 | 1.000 | +1.4% | +1.4% | higher |
| Intelligence: solve rate (score==1.0) | 0.944 | 1.000 | 1.000 | +5.9% | +5.9% | higher |
| Intelligence: mean visible_ok | 1.000 | 1.000 | 1.000 | +0.0% | +0.0% | higher |

## Per-task

| Task | vanilla mean score | vanilla mean cost | vanilla mean wall_s | vanilla n | zirv mean score | zirv mean cost | zirv mean wall_s | zirv n | zirv-proxy mean score | zirv-proxy mean cost | zirv-proxy mean wall_s | zirv-proxy n |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| t01_tiebreak | 1.000 | 0.144 | 35.5 | 3 | 1.000 | 0.164 | 32.2 | 3 | 1.000 | 0.160 | 31.3 | 3 |
| t02_pagination | 1.000 | 0.129 | 29.7 | 3 | 1.000 | 0.235 | 48.1 | 3 | 1.000 | 0.210 | 39.5 | 3 |
| t03_money | 1.000 | 0.186 | 37.4 | 3 | 1.000 | 0.262 | 52.1 | 3 | 1.000 | 0.282 | 60.9 | 3 |
| t04_budget | 1.000 | 0.485 | 115.2 | 3 | 1.000 | 0.495 | 139.6 | 3 | 1.000 | 0.833 | 93.6 | 3 |
| t05_dedupe | 1.000 | 0.182 | 34.0 | 3 | 1.000 | 0.248 | 48.0 | 3 | 1.000 | 0.219 | 44.4 | 3 |
| t06_currency | 1.000 | 0.322 | 64.5 | 3 | 1.000 | 0.474 | 111.7 | 3 | 1.000 | 0.703 | 66.4 | 3 |
| t07_redtest | 1.000 | 0.118 | 16.3 | 3 | 1.000 | 0.186 | 32.0 | 3 | 1.000 | 0.169 | 27.1 | 3 |
| t08_usage_doc | 1.000 | 0.135 | 23.6 | 3 | 1.000 | 0.574 | 140.3 | 3 | 1.000 | 0.350 | 84.6 | 3 |
| t09_count | 1.000 | 0.231 | 68.7 | 3 | 1.000 | 0.243 | 61.4 | 3 | 1.000 | 0.073 | 55.4 | 3 |
| t10_shares | 1.000 | 0.177 | 44.7 | 3 | 1.000 | 0.248 | 62.8 | 3 | 1.000 | 0.233 | 63.6 | 3 |
| t11_export | 1.000 | 0.458 | 115.2 | 3 | 1.000 | 0.564 | 129.8 | 3 | 1.000 | 0.676 | 189.7 | 3 |
| t12_deadcode | 0.833 | 0.271 | 78.8 | 3 | 1.000 | 0.295 | 64.8 | 3 | 1.000 | 0.271 | 65.0 | 3 |

## Features used (zirv / zirv-proxy runs)

| Condition | mean tool_calls | mean zirv workflow | mean zirv skill | mean zirv agent | mean zirv ctx | mean zirv other | mean subagents_spawned | mean permission_denials |
|---|---|---|---|---|---|---|---|---|
| zirv | 13.9 | 0.00 | 0.17 | 0.00 | 0.06 | 0.00 | 0.00 | 0.17 |
| zirv-proxy | 10.7 | 0.00 | 0.06 | 0.00 | 0.22 | 0.00 | 0.00 | 0.08 |

vanilla mean tool_calls (for comparison): 11.8

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
  - score: diff=+0.0139, 95% CI [+0.0000, +0.0347] (n=36 pairs)
  - total_cost_usd: diff=+0.0959, 95% CI [+0.0560, +0.1402] (n=36 pairs)
  - wall_s: diff=+21.6044, 95% CI [+9.6469, +34.0915] (n=36 pairs)

Paired bootstrap 95% CI, mean(zirv-proxy) - mean(vanilla), 10000 resamples seed 0, paired by (task,rep):
  - score: diff=+0.0139, 95% CI [+0.0000, +0.0347] (n=36 pairs)
  - total_cost_usd: diff=+0.1117, 95% CI [+0.0563, +0.1686] (n=36 pairs)
  - wall_s: diff=+13.1765, 95% CI [+1.0409, +26.2528] (n=36 pairs)

Errored/timed-out runs per condition:
  - vanilla: 0 / 36
  - zirv: 0 / 36
  - zirv-proxy: 0 / 36
