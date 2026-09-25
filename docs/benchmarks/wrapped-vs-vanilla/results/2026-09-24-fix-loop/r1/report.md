## Headline (mean per task, every task weighted equally)

Change vs **Vanilla + superpowers** below each value; *better*/*worse* = paired-bootstrap 95% CI over tasks excludes zero, *n.s.* = within noise.

| Metric | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---:|---:|---:|
| Quality score (0-100) | 99 | 98<br>-1 pts (n.s.) | 98<br>-1 pts (n.s.) |
| Fully solved (%) | 85% | 85%<br>+0 pts (n.s.) | 80%<br>-5 pts (n.s.) |
| Time per task (min) | 1.0 | 1.0<br>-2% (n.s.) | 1.3<br>+28% (worse) |
| Cost per task (USD) | $0.22 | $0.19<br>-11% (n.s.) | $0.20<br>-7% (n.s.) |
| Agent turns | 13 | 13<br>-2% (n.s.) | 13<br>+2% (n.s.) |
| Output tokens (k) | 7.3 | 7.3<br>-0% (n.s.) | 7.6<br>+4% (n.s.) |
| Work quality (judge, 0-100) | 72 | 66<br>-9% (worse) | 64<br>-10% (worse) |

- **Quality score (0-100)**: best is Vanilla + superpowers
- **Fully solved (%)**: tie: Vanilla + superpowers; zirv, Jev off
- **Time per task (min)**: best is zirv, Jev off
- **Cost per task (USD)**: best is zirv, Jev off
- **Agent turns**: best is zirv, Jev off
- **Output tokens (k)**: best is zirv, Jev off
- **Work quality (judge, 0-100)**: best is Vanilla + superpowers

## Reliability

| | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---:|---:|---:|
| Runs | 20 | 20 | 20 |
| Errors / timeouts | 0/20 | 0/20 | 0/20 |
| Visible tests still green | 20/20 | 19/20 | 18/20 |
| Model actually used | sonnet x20 | sonnet x20 | sonnet x20 |

## By task group

| Task group | Metric | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---|---:|---:|---:|
| Small/large (t01-t15) | Quality score (0-100) | 99 | 100 (+1 pts) | 100 (+1 pts) |
| Small/large (t01-t15) | Fully solved (%) | 83% | 100% (+17 pts) | 100% (+17 pts) |
| Small/large (t01-t15) | Time per task (min) | 0.7 | 0.8 (+4%) | 1.0 (+36%) |
| Small/large (t01-t15) | Cost per task (USD) | $0.18 | $0.16 (-9%) | $0.16 (-12%) |
| XL (t16-t22) | Quality score (0-100) | 99 | 98 (-1 pts) | 97 (-2 pts) |
| XL (t16-t22) | Fully solved (%) | 86% | 79% (-7 pts) | 71% (-14 pts) |
| XL (t16-t22) | Time per task (min) | 1.1 | 1.0 (-4%) | 1.4 (+26%) |
| XL (t16-t22) | Cost per task (USD) | $0.24 | $0.21 (-12%) | $0.22 (-5%) |

## Per task

| Task | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| t13_recurring | 100 / $0.19 / 0.9 | 100 / $0.21 / 1.1 | 100 / $0.22 / 1.4 |
| t14_bugsweep | 100 / $0.14 / 0.5 | 100 / $0.10 / 0.4 | 100 / $0.12 / 0.7 |
| t15_reports | 97 / $0.20 / 0.8 | 100 / $0.17 / 0.8 | 100 / $0.13 / 0.9 |
| t16_tags | 92 / $0.18 / 0.8 | 88 / $0.23 / 1.2 | 88 / $0.26 / 1.6 |
| t17_schema_migration | 100 / $0.21 / 0.9 | 100 / $0.20 / 1.0 | 100 / $0.20 / 1.3 |
| t18_ledger_layer | 100 / $0.12 / 0.5 | 100 / $0.10 / 0.4 | 100 / $0.12 / 0.8 |
| t19_goals_saga | 100 / $0.29 / 1.3 | 100 / $0.25 / 1.3 | 100 / $0.25 / 1.6 |
| t20_audit_log | 100 / $0.16 / 0.7 | 100 / $0.15 / 0.7 | 100 / $0.15 / 1.0 |
| t21_search | 100 / $0.24 / 1.0 | 95 / $0.17 / 0.8 | 90 / $0.22 / 1.4 |
| t22_envelopes | 100 / $0.46 / 2.5 | 100 / $0.36 / 2.0 | 100 / $0.37 / 2.0 |
