## Headline (mean per task, every task weighted equally)

Change vs **Vanilla + superpowers** below each value; *better*/*worse* = paired-bootstrap 95% CI over tasks excludes zero, *n.s.* = within noise.

| Metric | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---:|---:|---:|
| Quality score (0-100) | 99 | 92<br>-8 pts (n.s.) | 99<br>-0 pts (n.s.) |
| Fully solved (%) | 97% | 87%<br>-10 pts (n.s.) | 90%<br>-7 pts (n.s.) |
| Time per task (min) | 0.5 | 0.5<br>+15% (worse) | 1.0<br>+115% (worse) |
| Cost per task (USD) | $0.13 | $0.16<br>+29% (worse) | $0.15<br>+18% (worse) |
| Agent turns | 9 | 8<br>-6% (n.s.) | 12<br>+32% (n.s.) |
| Output tokens (k) | 2.9 | 3.0<br>+4% (n.s.) | 4.7<br>+61% (worse) |
| Work quality (judge, 0-100) | - | - | - |

- **Quality score (0-100)**: best is Vanilla + superpowers
- **Fully solved (%)**: best is Vanilla + superpowers
- **Time per task (min)**: best is Vanilla + superpowers
- **Cost per task (USD)**: best is Vanilla + superpowers
- **Agent turns**: best is zirv, Jev off
- **Output tokens (k)**: best is Vanilla + superpowers

## Reliability

| | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---:|---:|---:|
| Runs | 29 | 28 | 28 |
| Errors / timeouts | 0/29 | 0/28 | 0/28 |
| Visible tests still green | 29/29 | 27/28 | 28/28 |
| Model actually used | sonnet x29 | sonnet x28 | haiku x16, sonnet x12 |

## By task group

| Task group | Metric | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---|---:|---:|---:|
| Small/large (t01-t15) | Quality score (0-100) | 99 | 92 (-8 pts) | 99 (-0 pts) |
| Small/large (t01-t15) | Fully solved (%) | 97% | 87% (-10 pts) | 90% (-7 pts) |
| Small/large (t01-t15) | Time per task (min) | 0.5 | 0.5 (+15%) | 1.0 (+115%) |
| Small/large (t01-t15) | Cost per task (USD) | $0.13 | $0.16 (+29%) | $0.15 (+18%) |

## Per task

| Task | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| t01_tiebreak | 100 / $0.08 / 0.3 | 100 / $0.12 / 0.3 | 100 / $0.05 / 0.4 |
| t02_pagination | 100 / $0.08 / 0.3 | 100 / $0.13 / 0.4 | 100 / $0.15 / 1.5 |
| t03_money | 100 / $0.12 / 0.5 | 100 / $0.15 / 0.4 | 100 / $0.10 / 0.9 |
| t04_budget | 100 / $0.19 / 0.8 | 100 / $0.23 / 0.9 | 100 / $0.24 / 1.2 |
| t05_dedupe | 100 / $0.10 / 0.3 | 100 / $0.12 / 0.3 | 100 / $0.11 / 1.0 |
| t06_currency | 100 / $0.17 / 0.7 | 100 / $0.18 / 0.5 | 100 / $0.15 / 0.7 |
| t07_redtest | 100 / $0.10 / 0.2 | 100 / $0.11 / 0.3 | 100 / $0.08 / 0.7 |
| t08_usage_doc | 100 / $0.13 / 0.4 | 100 / $0.12 / 0.3 | 90 / $0.08 / 0.7 |
| t09_count | 100 / $0.09 / 0.2 | 100 / $0.14 / 0.3 | 100 / $0.11 / 1.5 |
| t10_shares | 100 / $0.11 / 0.5 | 100 / $0.13 / 0.4 | 100 / $0.14 / 0.7 |
| t11_export | 100 / $0.16 / 0.6 | 100 / $0.20 / 0.7 | 100 / $0.21 / 1.0 |
| t12_deadcode | 88 / $0.09 / 0.2 | 75 / $0.12 / 0.3 | 100 / $0.15 / 1.4 |
| t13_recurring | 100 / $0.22 / 1.1 | 100 / $0.36 / 1.6 | 95 / $0.35 / 1.9 |
| t14_bugsweep | 100 / $0.14 / 0.5 | 0 / $0.16 / 0.5 | 100 / $0.16 / 0.7 |
| t15_reports | 100 / $0.12 / 0.5 | 100 / $0.19 / 0.7 | 100 / $0.17 / 0.9 |
