## Headline (mean per task, every task weighted equally)

Change vs **Vanilla + superpowers** below each value; *better*/*worse* = paired-bootstrap 95% CI over tasks excludes zero, *n.s.* = within noise.

| Metric | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---:|---:|---:|
| Quality score (0-100) | 91 | 93<br>+3 pts (n/a) | 88<br>-2 pts (n/a) |
| Fully solved (%) | 0% | 0%<br>+0 pts (n/a) | 0%<br>+0 pts (n/a) |
| Time per task (min) | 17.6 | 15.0<br>-15% (n/a) | 14.7<br>-16% (n/a) |
| Cost per task (USD) | $4.27 | $3.12<br>-27% (n/a) | $3.05<br>-29% (n/a) |
| Agent turns | 120 | 97<br>-19% (n/a) | 97<br>-19% (n/a) |
| Output tokens (k) | 103.5 | 85.2<br>-18% (n/a) | 83.8<br>-19% (n/a) |
| Work quality (judge, 0-100) | 80 | 80<br>+0% (n/a) | 80<br>+0% (n/a) |

- **Quality score (0-100)**: best is zirv, Jev off
- **Fully solved (%)**: tie: Vanilla + superpowers; zirv, Jev off; zirv, Jev on
- **Time per task (min)**: best is zirv, Jev on
- **Cost per task (USD)**: best is zirv, Jev on
- **Agent turns**: tie: zirv, Jev off; zirv, Jev on
- **Output tokens (k)**: best is zirv, Jev on
- **Work quality (judge, 0-100)**: tie: Vanilla + superpowers; zirv, Jev off; zirv, Jev on

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
| Long-session chain | Quality score (0-100) | 91 | 93 (+3 pts) | 88 (-2 pts) |
| Long-session chain | Fully solved (%) | 0% | 0% | 0% |
| Long-session chain | Time per task (min) | 17.6 | 15.0 (-15%) | 14.7 (-16%) |
| Long-session chain | Cost per task (USD) | $4.27 | $3.12 (-27%) | $3.05 (-29%) |

## Per task

| Task | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| t24_long_haul | 91 / $4.27 / 17.6 | 93 / $3.12 / 15.0 | 88 / $3.05 / 14.7 |

## Long-session chain (per step)

### t24_long_haul

| Step | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| 01 (tests) | 100 / $0.14 / 0.6 | 100 / $0.11 / 0.6 | 100 / $0.12 / 0.7 |
| 02 (tests) | 100 / $0.06 / 0.4 | 100 / $0.05 / 0.4 | 100 / $0.06 / 0.4 |
| 03 (tests) | 100 / $0.15 / 0.9 | 100 / $0.16 / 1.1 | 100 / $0.18 / 1.2 |
| 04 (tests) | 25 / $0.14 / 0.6 | 75 / $0.09 / 0.5 | 0 / $0.09 / 0.5 |
| 05 (tests) | 100 / $0.09 / 0.5 | 100 / $0.08 / 0.5 | 100 / $0.08 / 0.5 |
| 06 (tests) | 67 / $0.11 / 0.6 | 67 / $0.08 / 0.6 | 33 / $0.09 / 0.5 |
| 07 (tests) | 100 / $0.07 / 0.5 | 100 / $0.07 / 0.5 | 100 / $0.07 / 0.5 |
| 08 (tests) | 90 / $0.40 / 2.1 | 95 / $0.32 / 2.0 | 90 / $0.27 / 1.5 |
| 09 (tests) | 83 / $0.16 / 0.8 | 83 / $0.15 / 0.7 | 83 / $0.11 / 0.7 |
| 10 (tests) | 100 / $0.16 / 0.5 | 100 / $0.14 / 0.5 | 100 / $0.13 / 0.5 |
| 11 (judge) | 90 / $0.12 / 0.7 | 90 / $0.09 / 0.5 | 95 / $0.08 / 0.5 |
| 12 (tests) | 100 / $0.23 / 1.2 | 100 / $0.20 / 0.9 | 100 / $0.16 / 0.8 |
| 13 (tests) | 100 / $0.21 / 0.7 | 100 / $0.15 / 0.5 | 100 / $0.08 / 0.4 |
| 14 (tests) | 50 / $0.17 / 0.6 | 50 / $0.15 / 0.6 | 50 / $0.09 / 0.5 |
| 15 (tests) | 100 / $0.14 / 0.4 | 100 / $0.11 / 0.4 | 100 / $0.25 / 0.7 |
| 16 (tests) | 100 / $0.33 / 0.8 | 100 / $0.12 / 0.5 | 100 / $0.13 / 0.5 |
| 17 (tests) | 100 / $0.24 / 0.7 | 100 / $0.13 / 0.4 | 100 / $0.14 / 0.5 |
| 18 (tests) | 100 / $0.28 / 1.0 | 100 / $0.23 / 0.8 | 100 / $0.14 / 0.6 |
| 19 (tests) | 100 / $0.35 / 0.8 | 100 / $0.15 / 0.4 | 100 / $0.13 / 0.5 |
| 20 (tests) | 100 / $0.19 / 0.7 | 100 / $0.15 / 0.5 | 100 / $0.22 / 0.7 |
| 21 (tests) | 100 / $0.19 / 0.5 | 100 / $0.15 / 0.4 | 100 / $0.18 / 0.5 |
| 22 (judge) | 90 / $0.33 / 1.1 | 90 / $0.23 / 0.8 | 90 / $0.26 / 0.9 |
