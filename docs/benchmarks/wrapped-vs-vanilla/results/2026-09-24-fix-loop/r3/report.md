## Headline (mean per task, every task weighted equally)

Change vs **Vanilla + superpowers** below each value; *better*/*worse* = paired-bootstrap 95% CI over tasks excludes zero, *n.s.* = within noise.

| Metric | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---:|---:|---:|
| Quality score (0-100) | 98 | 97<br>-1 pts (n.s.) | 97<br>-0 pts (n.s.) |
| Fully solved (%) | 60% | 60%<br>+0 pts (n.s.) | 60%<br>+0 pts (n.s.) |
| Time per task (min) | 1.7 | 1.6<br>-4% (n.s.) | 1.5<br>-11% (better) |
| Cost per task (USD) | $0.34 | $0.31<br>-11% (better) | $0.27<br>-21% (better) |
| Agent turns | 18 | 16<br>-10% (n.s.) | 15<br>-17% (n.s.) |
| Output tokens (k) | 11.2 | 10.1<br>-10% (better) | 9.3<br>-17% (better) |
| Work quality (judge, 0-100) | 71 | 73<br>+3% (n.s.) | 70<br>-1% (n.s.) |

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
| XL (t16-t22) | Quality score (0-100) | 98 | 96 (-1 pts) | 97 (-0 pts) |
| XL (t16-t22) | Fully solved (%) | 75% | 75% (+0 pts) | 75% (+0 pts) |
| XL (t16-t22) | Time per task (min) | 1.1 | 1.0 (-10%) | 1.0 (-16%) |
| XL (t16-t22) | Cost per task (USD) | $0.24 | $0.20 (-15%) | $0.19 (-18%) |
| Long-session chain | Quality score (0-100) | 98 | 98 (+0 pts) | 98 (+0 pts) |
| Long-session chain | Fully solved (%) | 0% | 0% | 0% |
| Long-session chain | Time per task (min) | 3.9 | 4.1 (+4%) | 3.7 (-6%) |
| Long-session chain | Cost per task (USD) | $0.77 | $0.72 (-6%) | $0.57 (-25%) |

## Per task

| Task | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| t16_tags | 90 / $0.16 / 0.6 | 86 / $0.12 / 0.6 | 88 / $0.13 / 0.6 |
| t17_schema_migration | 100 / $0.22 / 1.0 | 100 / $0.16 / 0.8 | 100 / $0.17 / 0.8 |
| t18_ledger_layer | 100 / $0.11 / 0.4 | 100 / $0.10 / 0.4 | 100 / $0.11 / 0.5 |
| t22_envelopes | 100 / $0.46 / 2.5 | 100 / $0.41 / 2.3 | 100 / $0.36 / 2.0 |
| t23_afternoon | 98 / $0.77 / 3.9 | 98 / $0.72 / 4.1 | 98 / $0.57 / 3.7 |

## Long-session chain (per step)

### t23_afternoon

| Step | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| 01 (tests) | 100 / $0.12 / 0.5 | 100 / $0.20 / 0.9 | 100 / $0.12 / 0.5 |
| 02 (tests) | 100 / $0.07 / 0.4 | 100 / $0.10 / 0.5 | 100 / $0.06 / 0.4 |
| 03 (tests) | 100 / $0.05 / 0.2 | 100 / $0.05 / 0.3 | 100 / $0.03 / 0.2 |
| 04 (tests) | 100 / $0.05 / 0.3 | 100 / $0.05 / 0.3 | 100 / $0.04 / 0.3 |
| 05 (tests) | 100 / $0.17 / 0.7 | 100 / $0.07 / 0.4 | 100 / $0.04 / 0.3 |
| 06 (tests) | 100 / $0.11 / 0.5 | 100 / $0.07 / 0.4 | 100 / $0.10 / 0.6 |
| 07 (tests) | 100 / $0.04 / 0.2 | 100 / $0.03 / 0.2 | 100 / $0.05 / 0.3 |
| 08 (tests) | 88 / $0.10 / 0.5 | 88 / $0.09 / 0.5 | 88 / $0.07 / 0.4 |
| 09 (judge) | 90 / $0.07 / 0.3 | 90 / $0.06 / 0.4 | 90 / $0.07 / 0.4 |
