## Headline (mean per task, every task weighted equally)

Change vs **Vanilla + superpowers** below each value; *better*/*worse* = paired-bootstrap 95% CI over tasks excludes zero, *n.s.* = within noise.

| Metric | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---:|---:|---:|
| Quality score (0-100) | 98 | 97<br>-1 pts (n.s.) | 96<br>-1 pts (n.s.) |
| Fully solved (%) | 60% | 50%<br>-10 pts (n.s.) | 60%<br>+0 pts (n.s.) |
| Time per task (min) | 1.9 | 1.8<br>-7% (n.s.) | 1.8<br>-6% (n.s.) |
| Cost per task (USD) | $0.38 | $0.36<br>-6% (n.s.) | $0.37<br>-5% (n.s.) |
| Agent turns | 24 | 18<br>-23% (better) | 17<br>-29% (n.s.) |
| Output tokens (k) | 12.7 | 11.0<br>-13% (n.s.) | 11.2<br>-12% (n.s.) |
| Work quality (judge, 0-100) | 73 | 78<br>+7% (better) | 76<br>+4% (n.s.) |

- **Quality score (0-100)**: best is Vanilla + superpowers
- **Fully solved (%)**: tie: Vanilla + superpowers; zirv, Jev on
- **Time per task (min)**: best is zirv, Jev off
- **Cost per task (USD)**: best is zirv, Jev off
- **Agent turns**: best is zirv, Jev on
- **Output tokens (k)**: best is zirv, Jev off
- **Work quality (judge, 0-100)**: best is zirv, Jev off

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
| XL (t16-t22) | Quality score (0-100) | 98 | 97 (-1 pts) | 96 (-2 pts) |
| XL (t16-t22) | Fully solved (%) | 75% | 62% (-12 pts) | 75% (+0 pts) |
| XL (t16-t22) | Time per task (min) | 1.2 | 1.2 (-1%) | 1.2 (+3%) |
| XL (t16-t22) | Cost per task (USD) | $0.25 | $0.25 (+1%) | $0.26 (+5%) |
| Long-session chain | Quality score (0-100) | 98 | 98 (+0 pts) | 98 (+0 pts) |
| Long-session chain | Fully solved (%) | 0% | 0% | 0% |
| Long-session chain | Time per task (min) | 4.9 | 4.2 (-14%) | 4.2 (-14%) |
| Long-session chain | Cost per task (USD) | $0.92 | $0.80 (-13%) | $0.78 (-15%) |

## Per task

| Task | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| t16_tags | 90 / $0.18 / 0.8 | 88 / $0.21 / 0.9 | 84 / $0.21 / 0.9 |
| t17_schema_migration | 100 / $0.21 / 0.9 | 100 / $0.20 / 0.9 | 100 / $0.16 / 0.6 |
| t18_ledger_layer | 100 / $0.12 / 0.5 | 100 / $0.16 / 0.7 | 100 / $0.16 / 0.6 |
| t22_envelopes | 100 / $0.48 / 2.6 | 99 / $0.44 / 2.4 | 100 / $0.53 / 2.8 |
| t23_afternoon | 98 / $0.92 / 4.9 | 98 / $0.80 / 4.2 | 98 / $0.78 / 4.2 |

## Long-session chain (per step)

### t23_afternoon

| Step | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| 01 (tests) | 100 / $0.20 / 0.9 | 100 / $0.15 / 0.6 | 100 / $0.15 / 0.6 |
| 02 (tests) | 100 / $0.11 / 0.6 | 100 / $0.08 / 0.4 | 100 / $0.09 / 0.5 |
| 03 (tests) | 100 / $0.06 / 0.3 | 100 / $0.04 / 0.3 | 100 / $0.04 / 0.3 |
| 04 (tests) | 100 / $0.09 / 0.4 | 100 / $0.04 / 0.3 | 100 / $0.06 / 0.3 |
| 05 (tests) | 100 / $0.07 / 0.4 | 100 / $0.13 / 0.6 | 100 / $0.13 / 0.6 |
| 06 (tests) | 100 / $0.11 / 0.7 | 100 / $0.12 / 0.7 | 100 / $0.12 / 0.6 |
| 07 (tests) | 100 / $0.06 / 0.3 | 100 / $0.04 / 0.2 | 100 / $0.04 / 0.2 |
| 08 (tests) | 88 / $0.14 / 0.8 | 88 / $0.14 / 0.6 | 94 / $0.10 / 0.5 |
| 09 (judge) | 90 / $0.08 / 0.4 | 90 / $0.06 / 0.4 | 85 / $0.06 / 0.3 |
