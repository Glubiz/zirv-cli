## Headline (mean per task, every task weighted equally)

Change vs **Vanilla + superpowers** below each value; *better*/*worse* = paired-bootstrap 95% CI over tasks excludes zero, *n.s.* = within noise.

| Metric | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---:|---:|---:|
| Quality score (0-100) | 97 | 97<br>-0 pts (n.s.) | 96<br>-1 pts (n.s.) |
| Fully solved (%) | 60% | 60%<br>+0 pts (n.s.) | 60%<br>+0 pts (n.s.) |
| Time per task (min) | 1.9 | 1.7<br>-12% (n.s.) | 1.8<br>-8% (n.s.) |
| Cost per task (USD) | $0.41 | $0.35<br>-15% (n.s.) | $0.34<br>-16% (n.s.) |
| Agent turns | 23 | 17<br>-25% (better) | 18<br>-20% (better) |
| Output tokens (k) | 12.7 | 10.5<br>-18% (n.s.) | 10.4<br>-18% (better) |
| Work quality (judge, 0-100) | 73 | 74<br>+1% (n.s.) | 74<br>+1% (n.s.) |

- **Quality score (0-100)**: best is Vanilla + superpowers
- **Fully solved (%)**: tie: Vanilla + superpowers; zirv, Jev off; zirv, Jev on
- **Time per task (min)**: best is zirv, Jev off
- **Cost per task (USD)**: best is zirv, Jev on
- **Agent turns**: best is zirv, Jev off
- **Output tokens (k)**: best is zirv, Jev on
- **Work quality (judge, 0-100)**: tie: zirv, Jev off; zirv, Jev on

## Reliability

| | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---:|---:|---:|
| Runs | 10 | 10 | 10 |
| Errors / timeouts | 0/10 | 0/10 | 0/10 |
| Visible tests still green | 10/10 | 10/10 | 10/10 |
| Model actually used | sonnet x10 | sonnet x10 | sonnet x10 |

## By task group

| Task group | Metric | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---|---:|---:|---:|
| XL (t16-t22) | Quality score (0-100) | 97 | 97 (+0 pts) | 96 (-1 pts) |
| XL (t16-t22) | Fully solved (%) | 75% | 75% (+0 pts) | 75% (+0 pts) |
| XL (t16-t22) | Time per task (min) | 1.3 | 1.1 (-17%) | 1.1 (-14%) |
| XL (t16-t22) | Cost per task (USD) | $0.29 | $0.19 (-33%) | $0.20 (-32%) |
| Long-session chain | Quality score (0-100) | 98 | 98 (-1 pts) | 98 (+0 pts) |
| Long-session chain | Fully solved (%) | 0% | 0% | 0% |
| Long-session chain | Time per task (min) | 4.4 | 4.1 (-6%) | 4.3 (-2%) |
| Long-session chain | Cost per task (USD) | $0.90 | $0.97 (+8%) | $0.94 (+5%) |

## Per task

| Task | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| t16_tags | 88 / $0.20 / 0.8 | 88 / $0.17 / 0.9 | 84 / $0.14 / 0.8 |
| t17_schema_migration | 100 / $0.21 / 1.0 | 100 / $0.14 / 0.8 | 100 / $0.14 / 0.8 |
| t18_ledger_layer | 100 / $0.12 / 0.4 | 100 / $0.13 / 0.7 | 100 / $0.13 / 0.7 |
| t22_envelopes | 100 / $0.62 / 3.0 | 100 / $0.32 / 2.0 | 100 / $0.37 / 2.3 |
| t23_afternoon | 98 / $0.90 / 4.4 | 98 / $0.97 / 4.1 | 98 / $0.94 / 4.3 |

## Long-session chain (per step)

### t23_afternoon

| Step | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| 01 (tests) | 100 / $0.17 / 0.7 | 100 / $0.13 / 0.6 | 100 / $0.09 / 0.5 |
| 02 (tests) | 100 / $0.09 / 0.4 | 100 / $0.11 / 0.4 | 100 / $0.12 / 0.5 |
| 03 (tests) | 100 / $0.05 / 0.2 | 100 / $0.03 / 0.2 | 100 / $0.04 / 0.3 |
| 04 (tests) | 100 / $0.07 / 0.4 | 100 / $0.03 / 0.3 | 100 / $0.04 / 0.3 |
| 05 (tests) | 100 / $0.16 / 0.6 | 100 / $0.08 / 0.5 | 100 / $0.13 / 0.7 |
| 06 (tests) | 100 / $0.12 / 0.6 | 100 / $0.17 / 0.6 | 100 / $0.21 / 0.8 |
| 07 (tests) | 100 / $0.04 / 0.2 | 100 / $0.09 / 0.3 | 100 / $0.08 / 0.2 |
| 08 (tests) | 88 / $0.12 / 0.6 | 88 / $0.18 / 0.7 | 88 / $0.11 / 0.6 |
| 09 (judge) | 95 / $0.07 / 0.4 | 90 / $0.15 / 0.4 | 95 / $0.12 / 0.4 |
