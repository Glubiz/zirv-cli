# zirv-vs-vanilla benchmark report

Runs found: 60 (errored/timed out: 0). Conditions: vanilla, zirv.

## Headline

| Metric | vanilla | zirv | change: zirv vs vanilla (%) | higher is better? |
|---|---|---|---|---|
| Speed: mean wall_s (s) | 110.3 | 113.1 | +2.5% | lower |
| Speed: median wall_s (s) | 60.4 | 112.7 | +86.7% | lower |
| Cost: mean total_cost_usd ($) | 0.208 | 0.197 | -5.5% | lower |
| Cost: mean total tokens | 1009910 | 907822 | -10.1% | lower |
| Cost: mean output_tokens | 9848 | 9813 | -0.4% | lower |
| Intelligence: mean score | 0.822 | 0.972 | +18.3% | higher |
| Intelligence: solve rate (score==1.0) | 0.700 | 0.833 | +19.0% | higher |
| Intelligence: mean visible_ok | 1.000 | 1.000 | +0.0% | higher |

## Per-task

| Task | vanilla mean score | vanilla mean cost | vanilla mean wall_s | vanilla n | zirv mean score | zirv mean cost | zirv mean wall_s | zirv n |
|---|---|---|---|---|---|---|---|---|
| t01_tiebreak | 1.000 | 0.071 | 42.5 | 2 | 1.000 | 0.056 | 28.6 | 2 |
| t02_pagination | 1.000 | 0.182 | 92.9 | 2 | 1.000 | 0.166 | 99.6 | 2 |
| t03_money | 1.000 | 0.158 | 81.7 | 2 | 1.000 | 0.110 | 56.6 | 2 |
| t04_budget | 0.500 | 0.165 | 86.6 | 2 | 1.000 | 0.353 | 174.9 | 2 |
| t05_dedupe | 0.950 | 0.087 | 51.9 | 2 | 1.000 | 0.099 | 49.5 | 2 |
| t06_currency | 0.500 | 0.171 | 103.4 | 2 | 1.000 | 0.225 | 129.7 | 2 |
| t07_redtest | 1.000 | 0.089 | 42.0 | 2 | 1.000 | 0.071 | 36.4 | 2 |
| t08_usage_doc | 1.000 | 0.073 | 48.4 | 2 | 0.900 | 0.084 | 51.5 | 2 |
| t09_count | 1.000 | 0.062 | 42.7 | 2 | 1.000 | 0.089 | 67.5 | 2 |
| t10_shares | 0.929 | 0.194 | 126.6 | 2 | 1.000 | 0.204 | 152.7 | 2 |
| t11_export | 0.500 | 0.361 | 186.9 | 2 | 1.000 | 0.230 | 116.7 | 2 |
| t12_deadcode | 1.000 | 0.126 | 73.3 | 2 | 0.875 | 0.167 | 103.6 | 2 |
| t13_recurring | 0.500 | 0.690 | 303.9 | 2 | 0.975 | 0.482 | 256.5 | 2 |
| t14_bugsweep | 0.975 | 0.252 | 137.3 | 2 | 1.000 | 0.260 | 153.8 | 2 |
| t15_reports | 0.471 | 0.440 | 234.3 | 2 | 0.824 | 0.352 | 218.9 | 2 |

## Features used (zirv / zirv-proxy runs)

| Condition | mean tool_calls | mean zirv workflow | mean zirv skill | mean zirv agent | mean zirv ctx | mean zirv other | mean subagents_spawned | mean permission_denials |
|---|---|---|---|---|---|---|---|---|
| zirv | 22.3 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.03 |

vanilla mean tool_calls (for comparison): 19.4

## Robustness


Paired bootstrap 95% CI, mean(zirv) - mean(vanilla), 10000 resamples seed 0, paired by (task,rep):
  - score: diff=+0.1500, 95% CI [+0.0314, +0.2866] (n=30 pairs)
  - total_cost_usd: diff=-0.0114, 95% CI [-0.0990, +0.0628] (n=30 pairs)
  - wall_s: diff=+2.8120, 95% CI [-37.8991, +39.2458] (n=30 pairs)

Errored/timed-out runs per condition:
  - vanilla: 0 / 30
  - zirv: 0 / 30
