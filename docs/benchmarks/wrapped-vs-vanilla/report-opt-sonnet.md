# zirv-vs-vanilla benchmark report

Runs found: 135 (errored/timed out: 0). Conditions: vanilla, zirv, zirv-proxy.

## Headline

| Metric | vanilla | zirv | zirv-proxy | change: zirv vs vanilla (%) | change: zirv-proxy vs vanilla (%) | higher is better? |
|---|---|---|---|---|---|---|
| Speed: mean wall_s (s) | 119.0 | 92.9 | 96.4 | -22.0% | -19.0% | lower |
| Speed: median wall_s (s) | 59.0 | 66.9 | 73.0 | +13.4% | +23.9% | lower |
| Cost: mean total_cost_usd ($) | 0.501 | 0.380 | 0.273 | -24.0% | -45.6% | lower |
| Cost: mean total tokens | 1122116 | 803332 | 877798 | -28.4% | -21.8% | lower |
| Cost: mean output_tokens | 10350 | 7909 | 7448 | -23.6% | -28.0% | lower |
| Intelligence: mean score | 0.933 | 0.972 | 0.984 | +4.1% | +5.4% | higher |
| Intelligence: solve rate (score==1.0) | 0.933 | 0.933 | 0.911 | +0.0% | -2.4% | higher |
| Intelligence: mean visible_ok | 1.000 | 0.978 | 1.000 | -2.2% | +0.0% | higher |

## Per-task

| Task | vanilla mean score | vanilla mean cost | vanilla mean wall_s | vanilla n | zirv mean score | zirv mean cost | zirv mean wall_s | zirv n | zirv-proxy mean score | zirv-proxy mean cost | zirv-proxy mean wall_s | zirv-proxy n |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| t01_tiebreak | 1.000 | 0.197 | 50.6 | 3 | 1.000 | 0.171 | 39.4 | 3 | 1.000 | 0.053 | 49.5 | 3 |
| t02_pagination | 1.000 | 0.233 | 49.3 | 3 | 1.000 | 0.207 | 37.8 | 3 | 1.000 | 0.206 | 37.7 | 3 |
| t03_money | 1.000 | 0.259 | 52.7 | 3 | 1.000 | 0.223 | 39.8 | 3 | 1.000 | 0.247 | 48.2 | 3 |
| t04_budget | 1.000 | 0.701 | 185.7 | 3 | 1.000 | 0.564 | 152.1 | 3 | 1.000 | 0.318 | 175.5 | 3 |
| t05_dedupe | 1.000 | 0.193 | 38.8 | 3 | 1.000 | 0.228 | 37.8 | 3 | 1.000 | 0.232 | 41.7 | 3 |
| t06_currency | 0.000 | 0.163 | 31.1 | 3 | 1.000 | 0.409 | 88.7 | 3 | 1.000 | 0.525 | 99.9 | 3 |
| t07_redtest | 1.000 | 0.200 | 39.6 | 3 | 1.000 | 0.165 | 23.1 | 3 | 1.000 | 0.152 | 27.0 | 3 |
| t08_usage_doc | 1.000 | 0.213 | 52.0 | 3 | 0.933 | 0.458 | 108.7 | 3 | 1.000 | 0.249 | 51.7 | 3 |
| t09_count | 1.000 | 0.181 | 42.3 | 3 | 1.000 | 0.280 | 67.9 | 3 | 1.000 | 0.077 | 56.1 | 3 |
| t10_shares | 1.000 | 0.365 | 98.4 | 3 | 1.000 | 0.245 | 65.2 | 3 | 1.000 | 0.242 | 72.9 | 3 |
| t11_export | 1.000 | 1.120 | 302.0 | 3 | 1.000 | 0.812 | 214.8 | 3 | 1.000 | 0.275 | 151.4 | 3 |
| t12_deadcode | 1.000 | 0.203 | 46.9 | 3 | 1.000 | 0.239 | 51.3 | 3 | 1.000 | 0.251 | 52.8 | 3 |
| t13_recurring | 1.000 | 2.209 | 477.9 | 3 | 1.000 | 0.833 | 263.7 | 3 | 0.817 | 0.456 | 267.5 | 3 |
| t14_bugsweep | 1.000 | 0.474 | 127.2 | 3 | 0.667 | 0.416 | 89.9 | 3 | 1.000 | 0.467 | 107.9 | 3 |
| t15_reports | 1.000 | 0.800 | 190.4 | 3 | 0.980 | 0.455 | 112.7 | 3 | 0.941 | 0.340 | 206.5 | 3 |

## Features used (zirv / zirv-proxy runs)

| Condition | mean tool_calls | mean zirv workflow | mean zirv skill | mean zirv agent | mean zirv ctx | mean zirv other | mean subagents_spawned | mean permission_denials |
|---|---|---|---|---|---|---|---|---|
| zirv | 16.1 | 0.00 | 0.04 | 0.00 | 0.07 | 0.00 | 0.00 | 0.18 |
| zirv-proxy | 19.4 | 0.00 | 0.02 | 0.00 | 0.00 | 0.00 | 0.00 | 0.02 |

vanilla mean tool_calls (for comparison): 19.1

## zirv-proxy decisions per task

| Task | mode complexity | mode seat_tier | mode model_used | mode workflow | runs that started a workflow | n runs |
|---|---|---|---|---|---|---|
| t01_tiebreak | trivial | cheap | haiku | n/a | 0/3 | 3 |
| t02_pagination | bounded | standard | sonnet | bugfix | 3/3 | 3 |
| t03_money | bounded | standard | sonnet | n/a | 0/3 | 3 |
| t04_budget | trivial | cheap | haiku | n/a | 0/3 | 3 |
| t05_dedupe | bounded | standard | sonnet | refactor | 3/3 | 3 |
| t06_currency | substantial | standard | sonnet | feature | 3/3 | 3 |
| t07_redtest | bounded | standard | sonnet | bugfix | 3/3 | 3 |
| t08_usage_doc | bounded | standard | sonnet | documentation-runbook-change | 3/3 | 3 |
| t09_count | trivial | cheap | haiku | n/a | 0/3 | 3 |
| t10_shares | bounded | standard | sonnet | n/a | 0/3 | 3 |
| t11_export | trivial | cheap | haiku | n/a | 0/3 | 3 |
| t12_deadcode | bounded | standard | sonnet | n/a | 0/3 | 3 |
| t13_recurring | trivial | cheap | haiku | n/a | 0/3 | 3 |
| t14_bugsweep | substantial | standard | sonnet | bugfix | 3/3 | 3 |
| t15_reports | trivial | cheap | haiku | n/a | 0/3 | 3 |

## Robustness


Paired bootstrap 95% CI, mean(zirv) - mean(vanilla), 10000 resamples seed 0, paired by (task,rep):
  - score: diff=+0.0387, 95% CI [-0.0458, +0.1289] (n=45 pairs)
  - total_cost_usd: diff=-0.1204, 95% CI [-0.2895, -0.0026] (n=45 pairs)
  - wall_s: diff=-26.1390, 95% CI [-55.3871, -2.9840] (n=45 pairs)

Paired bootstrap 95% CI, mean(zirv-proxy) - mean(vanilla), 10000 resamples seed 0, paired by (task,rep):
  - score: diff=+0.0505, 95% CI [-0.0173, +0.1333] (n=45 pairs)
  - total_cost_usd: diff=-0.2281, 95% CI [-0.4210, -0.0821] (n=45 pairs)
  - wall_s: diff=-22.5793, 95% CI [-51.4684, +0.2086] (n=45 pairs)

Errored/timed-out runs per condition:
  - vanilla: 0 / 45
  - zirv: 0 / 45
  - zirv-proxy: 0 / 45
