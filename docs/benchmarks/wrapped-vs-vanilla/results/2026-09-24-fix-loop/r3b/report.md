## Headline (mean per task, every task weighted equally)

Change vs **Vanilla + superpowers** below each value; *better*/*worse* = paired-bootstrap 95% CI over tasks excludes zero, *n.s.* = within noise.

| Metric | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---:|---:|---:|
| Quality score (0-100) | 98 | 97<br>-0 pts (n.s.) | 97<br>-0 pts (n.s.) |
| Fully solved (%) | 60% | 60%<br>+0 pts (n.s.) | 60%<br>+0 pts (n.s.) |
| Time per task (min) | 1.7 | 1.8<br>+6% (n.s.) | 1.6<br>-10% (n.s.) |
| Cost per task (USD) | $0.36 | $0.36<br>+2% (n.s.) | $0.29<br>-19% (n.s.) |
| Agent turns | 18 | 20<br>+11% (n.s.) | 13<br>-30% (n.s.) |
| Output tokens (k) | 11.5 | 11.1<br>-4% (n.s.) | 9.6<br>-17% (better) |
| Work quality (judge, 0-100) | 73 | 78<br>+7% (better) | 77<br>+5% (n.s.) |

- **Quality score (0-100)**: best is Vanilla + superpowers
- **Fully solved (%)**: tie: Vanilla + superpowers; zirv, Jev off; zirv, Jev on
- **Time per task (min)**: best is zirv, Jev on
- **Cost per task (USD)**: best is zirv, Jev on
- **Agent turns**: best is zirv, Jev on
- **Output tokens (k)**: best is zirv, Jev on
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
| XL (t16-t22) | Quality score (0-100) | 98 | 97 (-0 pts) | 97 (-0 pts) |
| XL (t16-t22) | Fully solved (%) | 75% | 75% (+0 pts) | 75% (+0 pts) |
| XL (t16-t22) | Time per task (min) | 1.2 | 1.1 (-5%) | 1.1 (-5%) |
| XL (t16-t22) | Cost per task (USD) | $0.24 | $0.24 (-0%) | $0.23 (-3%) |
| Long-session chain | Quality score (0-100) | 98 | 98 (+1 pts) | 98 (-0 pts) |
| Long-session chain | Fully solved (%) | 0% | 0% | 0% |
| Long-session chain | Time per task (min) | 4.0 | 4.7 (+18%) | 3.4 (-16%) |
| Long-session chain | Cost per task (USD) | $0.82 | $0.86 (+5%) | $0.51 (-37%) |

## Per task

| Task | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| t16_tags | 90 / $0.19 / 0.9 | 88 / $0.18 / 0.8 | 88 / $0.18 / 0.8 |
| t17_schema_migration | 100 / $0.18 / 0.8 | 100 / $0.15 / 0.6 | 100 / $0.14 / 0.6 |
| t18_ledger_layer | 100 / $0.12 / 0.4 | 100 / $0.19 / 0.8 | 100 / $0.16 / 0.7 |
| t22_envelopes | 100 / $0.47 / 2.6 | 100 / $0.44 / 2.2 | 100 / $0.44 / 2.4 |
| t23_afternoon | 98 / $0.82 / 4.0 | 98 / $0.86 / 4.7 | 98 / $0.51 / 3.4 |

## Long-session chain (per step)

### t23_afternoon

| Step | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| 01 (tests) | 100 / $0.20 / 0.8 | 100 / $0.14 / 0.6 | 100 / $0.13 / 0.5 |
| 02 (tests) | 100 / $0.10 / 0.4 | 100 / $0.11 / 0.6 | 100 / $0.06 / 0.4 |
| 03 (tests) | 100 / $0.05 / 0.3 | 100 / $0.04 / 0.3 | 100 / $0.03 / 0.2 |
| 04 (tests) | 100 / $0.05 / 0.3 | 100 / $0.08 / 0.4 | 100 / $0.03 / 0.3 |
| 05 (tests) | 100 / $0.07 / 0.4 | 100 / $0.05 / 0.4 | 100 / $0.03 / 0.3 |
| 06 (tests) | 100 / $0.09 / 0.5 | 100 / $0.21 / 0.9 | 100 / $0.06 / 0.5 |
| 07 (tests) | 100 / $0.05 / 0.2 | 100 / $0.05 / 0.3 | 100 / $0.04 / 0.3 |
| 08 (tests) | 94 / $0.12 / 0.6 | 94 / $0.10 / 0.6 | 88 / $0.07 / 0.4 |
| 09 (judge) | 85 / $0.08 / 0.4 | 90 / $0.07 / 0.4 | 90 / $0.06 / 0.4 |
