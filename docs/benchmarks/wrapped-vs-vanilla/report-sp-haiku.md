# zirv-vs-vanilla benchmark report

Runs found: 48 (errored/timed out: 0). Conditions: vanilla, zirv.

## Headline

| Metric | vanilla | zirv | change: zirv vs vanilla (%) | higher is better? |
|---|---|---|---|---|
| Speed: mean wall_s (s) | 95.7 | 145.0 | +51.5% | lower |
| Speed: median wall_s (s) | 86.1 | 135.1 | +56.9% | lower |
| Cost: mean total_cost_usd ($) | 0.151 | 0.216 | +42.8% | lower |
| Cost: mean total tokens | 529405 | 1102198 | +108.2% | lower |
| Cost: mean output_tokens | 6912 | 9532 | +37.9% | lower |
| Intelligence: mean score | 0.484 | 0.733 | +51.6% | higher |
| Intelligence: solve rate (score==1.0) | 0.417 | 0.667 | +60.0% | higher |
| Intelligence: mean visible_ok | 0.958 | 0.958 | +0.0% | higher |

## Per-task

| Task | vanilla mean score | vanilla mean cost | vanilla mean wall_s | vanilla n | zirv mean score | zirv mean cost | zirv mean wall_s | zirv n |
|---|---|---|---|---|---|---|---|---|
| t01_tiebreak | 1.000 | 0.097 | 57.5 | 2 | 1.000 | 0.033 | 22.9 | 2 |
| t02_pagination | 1.000 | 0.154 | 84.0 | 2 | 0.500 | 0.193 | 123.1 | 2 |
| t03_money | 1.000 | 0.154 | 86.3 | 2 | 1.000 | 0.226 | 141.4 | 2 |
| t04_budget | 0.500 | 0.271 | 140.1 | 2 | 1.000 | 0.460 | 283.6 | 2 |
| t05_dedupe | 0.500 | 0.072 | 43.1 | 2 | 0.800 | 0.143 | 75.1 | 2 |
| t06_currency | 0.000 | 0.188 | 105.0 | 2 | 0.500 | 0.363 | 223.4 | 2 |
| t07_redtest | 0.500 | 0.273 | 195.0 | 2 | 1.000 | 0.066 | 43.9 | 2 |
| t08_usage_doc | 0.000 | 0.091 | 58.6 | 2 | 0.500 | 0.180 | 108.1 | 2 |
| t09_count | 0.500 | 0.053 | 36.1 | 2 | 1.000 | 0.185 | 208.3 | 2 |
| t10_shares | 0.429 | 0.144 | 118.1 | 2 | 0.000 | 0.149 | 101.1 | 2 |
| t11_export | 0.000 | 0.174 | 133.0 | 2 | 0.500 | 0.371 | 263.8 | 2 |
| t12_deadcode | 0.375 | 0.142 | 91.8 | 2 | 1.000 | 0.221 | 145.3 | 2 |

## Features used (zirv / zirv-proxy runs)

| Condition | mean tool_calls | mean zirv workflow | mean zirv skill | mean zirv agent | mean zirv ctx | mean zirv other | mean subagents_spawned | mean permission_denials |
|---|---|---|---|---|---|---|---|---|
| zirv | 27.1 | 0.00 | 0.17 | 0.00 | 0.00 | 0.00 | 0.00 | 0.04 |

vanilla mean tool_calls (for comparison): 16.1

## Robustness


Paired bootstrap 95% CI, mean(zirv) - mean(vanilla), 10000 resamples seed 0, paired by (task,rep):
  - score: diff=+0.2497, 95% CI [+0.0119, +0.4810] (n=24 pairs)
  - total_cost_usd: diff=+0.0647, 95% CI [+0.0070, +0.1190] (n=24 pairs)
  - wall_s: diff=+49.2759, 95% CI [+4.6225, +91.1997] (n=24 pairs)

Errored/timed-out runs per condition:
  - vanilla: 0 / 24
  - zirv: 0 / 24
