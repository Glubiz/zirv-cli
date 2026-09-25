## Headline (mean per task, every task weighted equally)

Change vs **Vanilla + superpowers** below each value; *better*/*worse* = paired-bootstrap 95% CI over tasks excludes zero, *n.s.* = within noise.

| Metric | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---:|---:|---:|
| Quality score (0-100) | 98 | 96<br>-1 pts (n.s.) | 97<br>-0 pts (n.s.) |
| Fully solved (%) | 75% | 75%<br>+0 pts (n.s.) | 75%<br>+0 pts (n.s.) |
| Time per task (min) | 1.1 | 1.0<br>-13% (better) | 1.0<br>-9% (better) |
| Cost per task (USD) | $0.24 | $0.21<br>-13% (better) | $0.21<br>-13% (better) |
| Agent turns | 13 | 10<br>-25% (n.s.) | 9<br>-28% (n.s.) |
| Output tokens (k) | 8.4 | 7.0<br>-16% (better) | 7.5<br>-11% (better) |
| Work quality (judge, 0-100) | 71 | 69<br>-4% (n.s.) | 70<br>-2% (n.s.) |

- **Quality score (0-100)**: best is Vanilla + superpowers
- **Fully solved (%)**: tie: Vanilla + superpowers; zirv, Jev off; zirv, Jev on
- **Time per task (min)**: best is zirv, Jev off
- **Cost per task (USD)**: best is zirv, Jev on
- **Agent turns**: best is zirv, Jev on
- **Output tokens (k)**: best is zirv, Jev off
- **Work quality (judge, 0-100)**: best is Vanilla + superpowers

## Reliability

| | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---:|---:|---:|
| Runs | 8 | 8 | 8 |
| Errors / timeouts | 0/8 | 0/8 | 0/8 |
| Visible tests still green | 8/8 | 8/8 | 8/8 |
| Model actually used | sonnet x8 | sonnet x8 | sonnet x8 |

## By task group

| Task group | Metric | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---|---:|---:|---:|
| XL (t16-t22) | Quality score (0-100) | 98 | 96 (-1 pts) | 97 (-0 pts) |
| XL (t16-t22) | Fully solved (%) | 75% | 75% (+0 pts) | 75% (+0 pts) |
| XL (t16-t22) | Time per task (min) | 1.1 | 1.0 (-13%) | 1.0 (-9%) |
| XL (t16-t22) | Cost per task (USD) | $0.24 | $0.21 (-13%) | $0.21 (-13%) |

## Per task

| Task | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| t16_tags | 90 / $0.17 / 0.7 | 86 / $0.13 / 0.6 | 88 / $0.13 / 0.6 |
| t17_schema_migration | 100 / $0.21 / 0.9 | 100 / $0.14 / 0.6 | 100 / $0.15 / 0.7 |
| t18_ledger_layer | 100 / $0.13 / 0.5 | 100 / $0.10 / 0.4 | 100 / $0.12 / 0.5 |
| t22_envelopes | 100 / $0.44 / 2.4 | 100 / $0.46 / 2.4 | 100 / $0.43 / 2.4 |
