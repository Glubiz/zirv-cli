## Headline (mean per task, every task weighted equally)

Change vs **Vanilla + superpowers** below each value; *better*/*worse* = paired-bootstrap 95% CI over tasks excludes zero, *n.s.* = within noise.

| Metric | Vanilla + superpowers | zirv, Jev off | zirv, Jev on |
|---|---:|---:|---:|
| Quality score (0-100) | 97 | 98<br>+1 pts (n/a) | 97<br>+0 pts (n/a) |
| Fully solved (%) | 0% | 0%<br>+0 pts (n/a) | 0%<br>+0 pts (n/a) |
| Time per task (min) | 5.2 | 3.9<br>-25% (n/a) | 3.3<br>-36% (n/a) |
| Cost per task (USD) | $0.94 | $0.71<br>-24% (n/a) | $0.50<br>-47% (n/a) |
| Agent turns | 80 | 45<br>-44% (n/a) | 30<br>-62% (n/a) |
| Output tokens (k) | 30.6 | 19.0<br>-38% (n/a) | 15.8<br>-48% (n/a) |
| Work quality (judge, 0-100) | 65 | 80<br>+23% (n/a) | 80<br>+23% (n/a) |

- **Quality score (0-100)**: best is zirv, Jev off
- **Fully solved (%)**: tie: Vanilla + superpowers; zirv, Jev off; zirv, Jev on
- **Time per task (min)**: best is zirv, Jev on
- **Cost per task (USD)**: best is zirv, Jev on
- **Agent turns**: best is zirv, Jev on
- **Output tokens (k)**: best is zirv, Jev on
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
| Long-session chain | Quality score (0-100) | 97 | 98 (+1 pts) | 97 (+0 pts) |
| Long-session chain | Fully solved (%) | 0% | 0% | 0% |
| Long-session chain | Time per task (min) | 5.2 | 3.9 (-25%) | 3.3 (-36%) |
| Long-session chain | Cost per task (USD) | $0.94 | $0.71 (-24%) | $0.50 (-47%) |

## Per task

| Task | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| t23_afternoon | 97 / $0.94 / 5.2 | 98 / $0.71 / 3.9 | 97 / $0.50 / 3.3 |

## Long-session chain (per step)

### t23_afternoon

| Step | Vanilla + superpowers<br>score / $ / min | zirv, Jev off<br>score / $ / min | zirv, Jev on<br>score / $ / min |
|---|---:|---:|---:|
| 01 (tests) | 100 / $0.17 / 0.7 | 100 / $0.13 / 0.6 | 100 / $0.09 / 0.5 |
| 02 (tests) | 100 / $0.11 / 0.5 | 100 / $0.09 / 0.5 | 100 / $0.07 / 0.4 |
| 03 (tests) | 100 / $0.05 / 0.3 | 100 / $0.04 / 0.2 | 100 / $0.03 / 0.2 |
| 04 (tests) | 100 / $0.08 / 0.5 | 100 / $0.04 / 0.3 | 100 / $0.03 / 0.2 |
| 05 (tests) | 100 / $0.07 / 0.4 | 100 / $0.11 / 0.5 | 100 / $0.03 / 0.3 |
| 06 (tests) | 100 / $0.17 / 1.0 | 100 / $0.11 / 0.6 | 100 / $0.06 / 0.5 |
| 07 (tests) | 100 / $0.08 / 0.4 | 100 / $0.05 / 0.2 | 100 / $0.03 / 0.2 |
| 08 (tests) | 88 / $0.13 / 0.8 | 88 / $0.09 / 0.5 | 88 / $0.09 / 0.5 |
| 09 (judge) | 85 / $0.08 / 0.4 | 90 / $0.06 / 0.3 | 85 / $0.06 / 0.3 |
