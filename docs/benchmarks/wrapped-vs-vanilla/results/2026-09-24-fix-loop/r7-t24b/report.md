## Headline (mean per task, every task weighted equally)

Change vs **Vanilla + superpowers** below each value; *better*/*worse* = paired-bootstrap 95% CI over tasks excludes zero, *n.s.* = within noise.

| Metric | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---:|---:|---:|
| Quality score (0-100) | 89 | 92<br>+2 pts (n/a) | 93<br>+3 pts (n/a) |
| Fully solved (%) | 0% | 0%<br>+0 pts (n/a) | 0%<br>+0 pts (n/a) |
| Time per task (min) | 33.2 | 32.7<br>-2% (n/a) | 33.0<br>-1% (n/a) |
| Cost per task (USD) | $3.87 | $3.25<br>-16% (n/a) | $3.74<br>-3% (n/a) |
| Agent turns | 110 | 104<br>-6% (n/a) | 125<br>+13% (n/a) |
| Output tokens (k) | 103.1 | 91.1<br>-12% (n/a) | 93.3<br>-9% (n/a) |
| Work quality (judge, 0-100) | 73 | 80<br>+9% (n/a) | 80<br>+9% (n/a) |

- **Quality score (0-100)**: best is zirv, Jev on
- **Fully solved (%)**: tie: Vanilla + superpowers; zirv, Jev off; zirv, Jev on
- **Time per task (min)**: best is zirv, Jev off
- **Cost per task (USD)**: best is zirv, Jev off
- **Agent turns**: best is zirv, Jev off
- **Output tokens (k)**: best is zirv, Jev off
- **Work quality (judge, 0-100)**: tie: zirv, Jev off; zirv, Jev on

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
| Long-session chain | Quality score (0-100) | 89 | 92 (+2 pts) | 93 (+3 pts) |
| Long-session chain | Fully solved (%) | 0% | 0% | 0% |
| Long-session chain | Time per task (min) | 33.2 | 32.7 (-2%) | 33.0 (-1%) |
| Long-session chain | Cost per task (USD) | $3.87 | $3.25 (-16%) | $3.74 (-3%) |

## Per task

| Task | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| t24b_long_haul | 89 / $3.87 / 33.2 | 92 / $3.25 / 32.7 | 93 / $3.74 / 33.0 |

## Long-session chain (per step)

### t24b_long_haul

| Step | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| 01 (tests) | 100 / $0.15 / 0.7 | 100 / $0.13 / 1.4 | 100 / $0.16 / 1.4 |
| 02 (tests) | 100 / $0.08 / 0.9 | 100 / $0.05 / 1.2 | 100 / $0.05 / 1.1 |
| 03 (tests) | 100 / $0.14 / 1.8 | 100 / $0.18 / 2.1 | 100 / $0.21 / 2.3 |
| 04 (tests) | 17 / $0.12 / 1.3 | 67 / $0.11 / 1.2 | 100 / $0.23 / 1.5 |
| 05 (tests) | 100 / $0.08 / 1.1 | 100 / $0.08 / 1.3 | 100 / $0.08 / 1.3 |
| 06 (tests) | 33 / $0.09 / 1.2 | 33 / $0.09 / 1.5 | 33 / $0.09 / 1.4 |
| 07 (tests) | 100 / $0.09 / 1.5 | 100 / $0.08 / 1.4 | 100 / $0.08 / 1.3 |
| 08 (tests) | 90 / $0.37 / 3.1 | 97 / $0.28 / 2.3 | 100 / $0.33 / 2.1 |
| 09 (tests) | 100 / $0.19 / 1.6 | 100 / $0.13 / 1.0 | 100 / $0.14 / 1.1 |
| 10 (tests) | 100 / $0.20 / 0.9 | 100 / $0.21 / 1.2 | 100 / $0.19 / 1.3 |
| 11 (judge) | 90 / $0.12 / 1.5 | 90 / $0.10 / 1.4 | 87 / $0.10 / 1.5 |
| 12 (tests) | 100 / $0.22 / 1.9 | 100 / $0.22 / 1.7 | 100 / $0.15 / 1.5 |
| 13 (tests) | 100 / $0.16 / 1.2 | 100 / $0.14 / 1.0 | 100 / $0.18 / 1.3 |
| 14 (tests) | 50 / $0.13 / 1.4 | 50 / $0.13 / 1.4 | 50 / $0.18 / 1.5 |
| 15 (tests) | 100 / $0.13 / 1.4 | 100 / $0.11 / 1.4 | 100 / $0.15 / 1.4 |
| 16 (tests) | 100 / $0.14 / 1.6 | 100 / $0.14 / 1.5 | 100 / $0.19 / 1.6 |
| 17 (tests) | 100 / $0.17 / 1.6 | 100 / $0.13 / 1.4 | 100 / $0.14 / 1.4 |
| 18 (tests) | 100 / $0.25 / 1.8 | 100 / $0.16 / 1.7 | 100 / $0.24 / 1.8 |
| 19 (tests) | 100 / $0.27 / 1.4 | 100 / $0.18 / 1.3 | 100 / $0.24 / 1.2 |
| 20 (tests) | 100 / $0.29 / 1.6 | 100 / $0.22 / 1.5 | 100 / $0.17 / 1.5 |
| 21 (tests) | 100 / $0.18 / 1.1 | 100 / $0.14 / 1.1 | 100 / $0.21 / 1.4 |
| 22 (judge) | 87 / $0.31 / 2.0 | 80 / $0.25 / 1.9 | 67 / $0.26 / 1.7 |
