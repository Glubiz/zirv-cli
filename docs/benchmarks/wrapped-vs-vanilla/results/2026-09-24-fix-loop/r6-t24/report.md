## Headline (mean per task, every task weighted equally)

Change vs **Vanilla + superpowers** below each value; *better*/*worse* = paired-bootstrap 95% CI over tasks excludes zero, *n.s.* = within noise.

| Metric | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---:|---:|---:|
| Quality score (0-100) | 88 | 93<br>+5 pts (n/a) | 92<br>+4 pts (n/a) |
| Fully solved (%) | 0% | 0%<br>+0 pts (n/a) | 0%<br>+0 pts (n/a) |
| Time per task (min) | 19.3 | 15.5<br>-20% (n/a) | 16.8<br>-13% (n/a) |
| Cost per task (USD) | $4.77 | $3.19<br>-33% (n/a) | $3.18<br>-33% (n/a) |
| Agent turns | 138 | 110<br>-21% (n/a) | 120<br>-14% (n/a) |
| Output tokens (k) | 112.9 | 87.1<br>-23% (n/a) | 89.1<br>-21% (n/a) |
| Work quality (judge, 0-100) | 75 | 80<br>+7% (n/a) | 80<br>+7% (n/a) |

- **Quality score (0-100)**: best is zirv, Jev off
- **Fully solved (%)**: tie: Vanilla + superpowers; zirv, Jev off; zirv, Jev on
- **Time per task (min)**: best is zirv, Jev off
- **Cost per task (USD)**: best is zirv, Jev on
- **Agent turns**: best is zirv, Jev off
- **Output tokens (k)**: best is zirv, Jev off
- **Work quality (judge, 0-100)**: tie: zirv, Jev off; zirv, Jev on

## Reliability

| | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---:|---:|---:|
| Runs | 2 | 2 | 2 |
| Errors / timeouts | 0/2 | 0/2 | 0/2 |
| Visible tests still green | 2/2 | 2/2 | 2/2 |
| Model actually used | sonnet x2 | sonnet x2 | sonnet x2 |

## By task group

| Task group | Metric | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---|---:|---:|---:|
| Long-session chain | Quality score (0-100) | 88 | 93 (+5 pts) | 92 (+4 pts) |
| Long-session chain | Fully solved (%) | 0% | 0% | 0% |
| Long-session chain | Time per task (min) | 19.3 | 15.5 (-20%) | 16.8 (-13%) |
| Long-session chain | Cost per task (USD) | $4.77 | $3.19 (-33%) | $3.18 (-33%) |

## Per task

| Task | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| t24_long_haul | 88 / $4.77 / 19.3 | 93 / $3.19 / 15.5 | 92 / $3.18 / 16.8 |

## Long-session chain (per step)

### t24_long_haul

| Step | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| 01 (tests) | 100 / $0.15 / 0.6 | 100 / $0.10 / 0.6 | 100 / $0.17 / 0.8 |
| 02 (tests) | 100 / $0.14 / 0.7 | 100 / $0.05 / 0.4 | 100 / $0.08 / 0.5 |
| 03 (tests) | 100 / $0.19 / 1.1 | 100 / $0.15 / 1.1 | 100 / $0.18 / 1.2 |
| 04 (tests) | 0 / $0.11 / 0.6 | 100 / $0.13 / 0.7 | 100 / $0.16 / 0.9 |
| 05 (tests) | 100 / $0.10 / 0.6 | 100 / $0.10 / 0.6 | 100 / $0.18 / 0.8 |
| 06 (tests) | 33 / $0.10 / 0.6 | 33 / $0.11 / 0.6 | 33 / $0.12 / 0.6 |
| 07 (tests) | 100 / $0.09 / 0.6 | 100 / $0.08 / 0.5 | 100 / $0.08 / 0.5 |
| 08 (tests) | 90 / $0.33 / 1.8 | 100 / $0.26 / 1.5 | 100 / $0.24 / 1.8 |
| 09 (tests) | 92 / $0.17 / 0.9 | 83 / $0.12 / 0.7 | 83 / $0.12 / 0.7 |
| 10 (tests) | 100 / $0.30 / 0.9 | 100 / $0.12 / 0.5 | 100 / $0.12 / 0.5 |
| 11 (judge) | 90 / $0.14 / 0.8 | 90 / $0.09 / 0.6 | 90 / $0.09 / 0.6 |
| 12 (tests) | 100 / $0.39 / 1.7 | 100 / $0.16 / 1.0 | 100 / $0.16 / 1.0 |
| 13 (tests) | 100 / $0.27 / 0.8 | 100 / $0.10 / 0.5 | 100 / $0.10 / 0.5 |
| 14 (tests) | 50 / $0.20 / 0.7 | 50 / $0.18 / 0.6 | 50 / $0.13 / 0.5 |
| 15 (tests) | 100 / $0.19 / 0.6 | 100 / $0.17 / 0.6 | 100 / $0.10 / 0.4 |
| 16 (tests) | 100 / $0.22 / 0.6 | 100 / $0.28 / 0.7 | 100 / $0.12 / 0.5 |
| 17 (tests) | 100 / $0.21 / 0.6 | 100 / $0.12 / 0.5 | 100 / $0.10 / 0.4 |
| 18 (tests) | 100 / $0.32 / 1.2 | 100 / $0.20 / 0.7 | 100 / $0.20 / 0.8 |
| 19 (tests) | 100 / $0.35 / 0.8 | 100 / $0.15 / 0.5 | 100 / $0.11 / 0.5 |
| 20 (tests) | 100 / $0.23 / 0.8 | 100 / $0.17 / 0.6 | 100 / $0.15 / 0.6 |
| 21 (tests) | 100 / $0.26 / 0.6 | 100 / $0.16 / 0.5 | 100 / $0.19 / 0.6 |
| 22 (judge) | 90 / $0.32 / 1.1 | 90 / $0.20 / 0.9 | 75 / $0.27 / 1.0 |
