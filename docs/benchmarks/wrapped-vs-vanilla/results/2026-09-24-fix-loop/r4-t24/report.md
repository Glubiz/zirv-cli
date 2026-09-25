## Headline (mean per task, every task weighted equally)

Change vs **Vanilla + superpowers** below each value; *better*/*worse* = paired-bootstrap 95% CI over tasks excludes zero, *n.s.* = within noise.

| Metric | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---:|---:|---:|
| Quality score (0-100) | 92 | 73<br>-20 pts (n/a) | 65<br>-27 pts (n/a) |
| Fully solved (%) | 0% | 0%<br>+0 pts (n/a) | 0%<br>+0 pts (n/a) |
| Time per task (min) | 16.4 | 17.1<br>+5% (n/a) | 17.0<br>+4% (n/a) |
| Cost per task (USD) | $3.26 | $2.54<br>-22% (n/a) | $2.29<br>-30% (n/a) |
| Agent turns | 77 | 107<br>+39% (n/a) | 102<br>+32% (n/a) |
| Output tokens (k) | 99.2 | 74.2<br>-25% (n/a) | 74.1<br>-25% (n/a) |
| Work quality (judge, 0-100) | 60 | 30<br>-50% (n/a) | 30<br>-50% (n/a) |

- **Quality score (0-100)**: best is Vanilla + superpowers
- **Fully solved (%)**: tie: Vanilla + superpowers; zirv, Jev off; zirv, Jev on
- **Time per task (min)**: best is Vanilla + superpowers
- **Cost per task (USD)**: best is zirv, Jev on
- **Agent turns**: best is Vanilla + superpowers
- **Output tokens (k)**: best is zirv, Jev on
- **Work quality (judge, 0-100)**: best is Vanilla + superpowers

## Reliability

| | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---:|---:|---:|
| Runs | 1 | 1 | 1 |
| Errors / timeouts | 0/1 | 0/1 | 0/1 |
| Visible tests still green | 1/1 | 1/1 | 1/1 |
| Model actually used | sonnet x1 | sonnet x1 | sonnet x1 |

## By task group

| Task group | Metric | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---|---:|---:|---:|
| Long-session chain | Quality score (0-100) | 92 | 73 (-20 pts) | 65 (-27 pts) |
| Long-session chain | Fully solved (%) | 0% | 0% | 0% |
| Long-session chain | Time per task (min) | 16.4 | 17.1 (+5%) | 17.0 (+4%) |
| Long-session chain | Cost per task (USD) | $3.26 | $2.54 (-22%) | $2.29 (-30%) |

## Per task

| Task | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| t24_long_haul | 92 / $3.26 / 16.4 | 73 / $2.54 / 17.1 | 65 / $2.29 / 17.0 |

## Long-session chain (per step)

### t24_long_haul

| Step | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| 01 (tests) | 100 / $0.12 / 0.5 | 100 / $0.14 / 0.5 | 100 / $0.17 / 0.7 |
| 02 (tests) | 100 / $0.12 / 0.8 | 100 / $0.09 / 0.6 | 100 / $0.06 / 0.5 |
| 03 (tests) | 100 / $0.12 / 0.8 | 100 / $0.19 / 1.2 | 100 / $0.21 / 1.3 |
| 04 (tests) | 100 / $0.05 / 0.4 | 0 / $0.21 / 0.9 | 0 / $0.22 / 0.9 |
| 05 (tests) | 100 / $0.14 / 0.7 | 100 / $0.08 / 0.5 | 100 / $0.17 / 0.9 |
| 06 (tests) | 33 / $0.09 / 0.5 | 100 / $0.15 / 0.7 | 33 / $0.12 / 0.7 |
| 07 (tests) | 100 / $0.07 / 0.5 | 100 / $0.07 / 0.4 | 100 / $0.11 / 0.6 |
| 08 (tests) | 100 / $0.59 / 3.2 | 90 / $0.33 / 1.6 | 90 / $0.25 / 1.3 |
| 09 (tests) | 83 / $0.14 / 0.8 | 83 / $0.14 / 0.8 | 83 / $0.14 / 0.8 |
| 10 (tests) | 100 / $0.11 / 0.4 | 100 / $0.21 / 0.8 | 100 / $0.11 / 0.5 |
| 11 (judge) | 100 / $0.13 / 0.8 | 90 / $0.11 / 0.6 | 90 / $0.11 / 0.6 |
| 12 (tests) | 100 / $0.20 / 1.0 | 100 / $0.18 / 0.8 | 100 / $0.27 / 1.1 |
| 13 (tests) | 100 / $0.09 / 0.4 | 100 / $0.22 / 0.8 | 100 / $0.15 / 0.5 |
| 14 (tests) | 50 / $0.11 / 0.5 | 50 / $0.24 / 0.7 | 50 / $0.19 / 0.7 |
| 15 (tests) | 100 / $0.10 / 0.4 | 100 / $0.17 / 0.4 | 100 / $0.00 / 1.4 |
| 16 (tests) | 100 / $0.11 / 0.4 | 100 / $0.00 / 1.6 | 100 / $0.00 / 1.4 |
| 17 (tests) | 100 / $0.12 / 0.4 | 100 / $0.00 / 1.5 | 0 / $0.00 / 0.4 |
| 18 (tests) | 100 / $0.14 / 0.7 | 50 / $0.00 / 0.4 | 50 / $0.00 / 0.4 |
| 19 (tests) | 100 / $0.17 / 0.5 | 33 / $0.00 / 0.4 | 33 / $0.00 / 0.4 |
| 20 (tests) | 100 / $0.14 / 0.6 | 0 / $0.00 / 0.4 | 0 / $0.00 / 0.4 |
| 21 (tests) | 100 / $0.18 / 0.6 | 0 / $0.00 / 0.4 | 0 / $0.00 / 0.4 |
| 22 (judge) | 60 / $0.23 / 1.0 | 0 / $0.00 / 0.4 | 0 / $0.00 / 0.4 |
