# zirv-vs-vanilla benchmark report

Runs found: 48 (errored/timed out: 0). Conditions: vanilla, zirv.

## Headline

| Metric | vanilla | zirv | change: zirv vs vanilla (%) | higher is better? |
|---|---|---|---|---|
| Speed: mean wall_s (s) | 72.0 | 95.7 | +33.0% | lower |
| Speed: median wall_s (s) | 61.5 | 80.9 | +31.5% | lower |
| Cost: mean total_cost_usd ($) | 0.127 | 0.159 | +24.9% | lower |
| Cost: mean total tokens | 526822 | 709649 | +34.7% | lower |
| Cost: mean output_tokens | 6578 | 7044 | +7.1% | lower |
| Intelligence: mean score | 0.959 | 0.990 | +3.2% | higher |
| Intelligence: solve rate (score==1.0) | 0.833 | 0.958 | +15.0% | higher |
| Intelligence: mean visible_ok | 1.000 | 1.000 | +0.0% | higher |

## Per-task

| Task | vanilla mean score | vanilla mean cost | vanilla mean wall_s | vanilla n | zirv mean score | zirv mean cost | zirv mean wall_s | zirv n |
|---|---|---|---|---|---|---|---|---|
| t01_tiebreak | 1.000 | 0.051 | 28.4 | 2 | 1.000 | 0.052 | 28.8 | 2 |
| t02_pagination | 1.000 | 0.113 | 70.8 | 2 | 1.000 | 0.122 | 80.9 | 2 |
| t03_money | 1.000 | 0.104 | 52.3 | 2 | 1.000 | 0.129 | 76.8 | 2 |
| t04_budget | 0.750 | 0.298 | 139.6 | 2 | 1.000 | 0.276 | 164.1 | 2 |
| t05_dedupe | 1.000 | 0.074 | 39.2 | 2 | 1.000 | 0.094 | 56.7 | 2 |
| t06_currency | 1.000 | 0.159 | 92.0 | 2 | 1.000 | 0.235 | 135.9 | 2 |
| t07_redtest | 1.000 | 0.072 | 34.6 | 2 | 1.000 | 0.075 | 40.7 | 2 |
| t08_usage_doc | 0.900 | 0.063 | 34.9 | 2 | 1.000 | 0.090 | 45.8 | 2 |
| t09_count | 1.000 | 0.089 | 67.3 | 2 | 1.000 | 0.100 | 61.7 | 2 |
| t10_shares | 0.857 | 0.096 | 68.6 | 2 | 1.000 | 0.149 | 118.9 | 2 |
| t11_export | 1.000 | 0.251 | 157.1 | 2 | 1.000 | 0.400 | 219.1 | 2 |
| t12_deadcode | 1.000 | 0.160 | 78.8 | 2 | 0.875 | 0.186 | 119.1 | 2 |

## Features used (zirv / zirv-proxy runs)

| Condition | mean tool_calls | mean zirv workflow | mean zirv skill | mean zirv agent | mean zirv ctx | mean zirv other | mean subagents_spawned | mean permission_denials |
|---|---|---|---|---|---|---|---|---|
| zirv | 18.0 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 |

vanilla mean tool_calls (for comparison): 15.5

## Robustness


Paired bootstrap 95% CI, mean(zirv) - mean(vanilla), 10000 resamples seed 0, paired by (task,rep):
  - score: diff=+0.0307, 95% CI [-0.0149, +0.0848] (n=24 pairs)
  - total_cost_usd: diff=+0.0317, 95% CI [+0.0117, +0.0541] (n=24 pairs)
  - wall_s: diff=+23.7303, 95% CI [+13.6670, +35.2224] (n=24 pairs)

Errored/timed-out runs per condition:
  - vanilla: 0 / 24
  - zirv: 0 / 24
