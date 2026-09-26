## Headline (mean per task, every task weighted equally)

Change vs **Vanilla + superpowers** below each value; *better*/*worse* = paired-bootstrap 95% CI over tasks excludes zero, *n.s.* = within noise.

| Metric | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---:|---:|---:|
| Quality score (0-100) | 99 | 88<br>-11 pts (n.s.) | 89<br>-10 pts (n.s.) |
| Fully solved (%) | 85% | 75%<br>-10 pts (n.s.) | 80%<br>-5 pts (n.s.) |
| Time per task (min) | 1.0 | 1.2<br>+19% (worse) | 1.0<br>-3% (n.s.) |
| Cost per task (USD) | $0.23 | $0.19<br>-20% (better) | $0.18<br>-22% (better) |
| Agent turns | 13 | 10<br>-27% (better) | 9<br>-33% (better) |
| Output tokens (k) | 7.6 | 7.0<br>-8% (n.s.) | 7.1<br>-7% (n.s.) |
| Work quality (judge, 0-100) | 70 | 76<br>+9% (better) | 78<br>+11% (better) |

- **Quality score (0-100)**: best is Vanilla + superpowers
- **Fully solved (%)**: best is Vanilla + superpowers
- **Time per task (min)**: best is zirv, Jev on
- **Cost per task (USD)**: best is zirv, Jev on
- **Agent turns**: best is zirv, Jev on
- **Output tokens (k)**: best is zirv, Jev off
- **Work quality (judge, 0-100)**: best is zirv, Jev on

## Reliability

| | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---:|---:|---:|
| Runs | 20 | 20 | 20 |
| Errors / timeouts | 0/20 | 0/20 | 0/20 |
| Visible tests still green | 20/20 | 18/20 | 18/20 |
| Model actually used | sonnet x20 | sonnet x20 | sonnet x20 |

## By task group

| Task group | Metric | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---|---:|---:|---:|
| Small/large (t01-t15) | Quality score (0-100) | 99 | 67 (-32 pts) | 67 (-32 pts) |
| Small/large (t01-t15) | Fully solved (%) | 83% | 67% (-17 pts) | 67% (-17 pts) |
| Small/large (t01-t15) | Time per task (min) | 0.9 | 1.0 (+15%) | 0.8 (-5%) |
| Small/large (t01-t15) | Cost per task (USD) | $0.21 | $0.16 (-24%) | $0.16 (-25%) |
| XL (t16-t22) | Quality score (0-100) | 99 | 97 (-1 pts) | 98 (-1 pts) |
| XL (t16-t22) | Fully solved (%) | 86% | 79% (-7 pts) | 86% (+0 pts) |
| XL (t16-t22) | Time per task (min) | 1.1 | 1.3 (+21%) | 1.1 (-3%) |
| XL (t16-t22) | Cost per task (USD) | $0.24 | $0.20 (-18%) | $0.19 (-21%) |

## Per task

| Task | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| t13_recurring | 100 / $0.31 / 1.5 | 100 / $0.25 / 1.5 | 100 / $0.22 / 1.2 |
| t14_bugsweep | 100 / $0.15 / 0.5 | 0 / $0.13 / 0.7 | 0 / $0.13 / 0.6 |
| t15_reports | 97 / $0.19 / 0.7 | 100 / $0.11 / 0.8 | 100 / $0.14 / 0.7 |
| t16_tags | 90 / $0.18 / 0.8 | 86 / $0.17 / 1.1 | 86 / $0.17 / 0.9 |
| t17_schema_migration | 100 / $0.18 / 0.7 | 100 / $0.11 / 0.9 | 100 / $0.13 / 0.7 |
| t18_ledger_layer | 100 / $0.12 / 0.5 | 100 / $0.12 / 0.9 | 100 / $0.11 / 0.6 |
| t19_goals_saga | 100 / $0.28 / 1.4 | 100 / $0.27 / 1.8 | 100 / $0.27 / 1.6 |
| t20_audit_log | 100 / $0.17 / 0.7 | 100 / $0.20 / 1.2 | 100 / $0.11 / 0.6 |
| t21_search | 100 / $0.27 / 1.2 | 95 / $0.12 / 0.9 | 100 / $0.12 / 0.7 |
| t22_envelopes | 100 / $0.48 / 2.5 | 100 / $0.38 / 2.5 | 100 / $0.40 / 2.5 |
