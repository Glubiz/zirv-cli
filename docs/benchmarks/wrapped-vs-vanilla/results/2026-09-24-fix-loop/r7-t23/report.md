## Headline (mean per task, every task weighted equally)

Change vs **Vanilla + superpowers** below each value; *better*/*worse* = paired-bootstrap 95% CI over tasks excludes zero, *n.s.* = within noise.

| Metric | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---:|---:|---:|
| Quality score (0-100) | 98 | 98<br>+0 pts (n/a) | 98<br>+0 pts (n/a) |
| Fully solved (%) | 0% | 0%<br>+0 pts (n/a) | 0%<br>+0 pts (n/a) |
| Time per task (min) | 13.0 | 13.2<br>+1% (n/a) | 13.3<br>+2% (n/a) |
| Cost per task (USD) | $0.98 | $0.57<br>-42% (n/a) | $0.79<br>-19% (n/a) |
| Agent turns | 62 | 34<br>-45% (n/a) | 47<br>-25% (n/a) |
| Output tokens (k) | 27.6 | 17.1<br>-38% (n/a) | 22.3<br>-19% (n/a) |
| Work quality (judge, 0-100) | 80 | 80<br>+0% (n/a) | 80<br>+0% (n/a) |

- **Quality score (0-100)**: best is zirv, Jev on
- **Fully solved (%)**: tie: Vanilla + superpowers; zirv, Jev off; zirv, Jev on
- **Time per task (min)**: best is Vanilla + superpowers
- **Cost per task (USD)**: best is zirv, Jev off
- **Agent turns**: best is zirv, Jev off
- **Output tokens (k)**: best is zirv, Jev off
- **Work quality (judge, 0-100)**: tie: Vanilla + superpowers; zirv, Jev off; zirv, Jev on

## Reliability

| | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---:|---:|---:|
| Runs | 3 | 3 | 3 |
| Errors / timeouts | 0/3 | 0/3 | 0/3 |
| Visible tests still green | 3/3 | 3/3 | 3/3 |
| Model actually used | sonnet x3 | sonnet x3 | sonnet x3 |

## By task group

| Task group | Metric | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---|---:|---:|---:|
| Long-session chain | Quality score (0-100) | 98 | 98 (+0 pts) | 98 (+0 pts) |
| Long-session chain | Fully solved (%) | 0% | 0% | 0% |
| Long-session chain | Time per task (min) | 13.0 | 13.2 (+1%) | 13.3 (+2%) |
| Long-session chain | Cost per task (USD) | $0.98 | $0.57 (-42%) | $0.79 (-19%) |

## Per task

| Task | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| t23_afternoon | 98 / $0.98 / 13.0 | 98 / $0.57 / 13.2 | 98 / $0.79 / 13.3 |

## Long-session chain (per step)

### t23_afternoon

| Step | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| 01 (tests) | 100 / $0.17 / 1.2 | 100 / $0.10 / 1.4 | 100 / $0.14 / 1.4 |
| 02 (tests) | 100 / $0.11 / 1.3 | 100 / $0.07 / 1.2 | 100 / $0.09 / 1.4 |
| 03 (tests) | 100 / $0.05 / 1.2 | 100 / $0.04 / 1.3 | 100 / $0.04 / 1.2 |
| 04 (tests) | 100 / $0.08 / 1.6 | 100 / $0.04 / 1.5 | 100 / $0.04 / 1.5 |
| 05 (tests) | 100 / $0.09 / 1.5 | 100 / $0.08 / 1.6 | 100 / $0.12 / 1.7 |
| 06 (tests) | 100 / $0.22 / 2.0 | 100 / $0.09 / 1.6 | 100 / $0.13 / 1.6 |
| 07 (tests) | 100 / $0.07 / 0.8 | 100 / $0.03 / 1.2 | 100 / $0.06 / 1.1 |
| 08 (tests) | 88 / $0.13 / 1.8 | 88 / $0.08 / 1.7 | 92 / $0.10 / 1.7 |
| 09 (judge) | 90 / $0.08 / 1.3 | 90 / $0.06 / 1.4 | 90 / $0.06 / 1.3 |
