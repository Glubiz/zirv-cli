# FAFO record: faster / cheaper / better zirv-wrapped agents (2026-10-06)

Goal: cut wall time and/or cost of zirv-wrapped headless sessions, or raise
quality, without lowering hidden-test pass rate or judge score.

Outcome: one lever cleared the operator's bar (>=4% on time, cost or quality,
same direction in two independent rounds) and was implemented: headless
effort `low` by default (cost -10.1%, agent time -8.9%, hidden tests +1.1 pts,
judge -1.75 pts). The worker-prompt rules (part 1) and chain compaction
(part 2) did not clear it.

Part 1 (worker-prompt rules on the XL grid) is below; part 2 (chains and
effort) follows it.

## Setup

- Bench: XL t13-t22 x 2 reps per arm, `zirv-nojev`, sonnet, `--noninteractive`,
  `--stagger-s 30`, `--parallel 1`, all arms of a round launched together.
  Claude Code 2.1.291, vanilla = superpowers 6.4.1.
- Arms: origin/main f0d28ad5 (4.52.1) built locally; each variant is that
  source plus one edit to `DEFAULT_PROMPT_WORKER` (`src/commands/ctx/prompt.rs`).
  Benchmark seats get the worker prompt (`v13 layers: default+adapter+skill pointer`).
- Analysis: `stats.py` (paired bootstrap over the 10 tasks, 95% CI) and
  `rounds_by_kind.py` (each API round classified by its dominant tool call)
  in this folder. Run data stayed in the session scratchpad (not committed).
- Spend: 140 runs, about $30 of agent spend, about 72% of one 5-hour window.

## Baseline

4.52.1 vs vanilla (pooled base arms, n=40 vs 20): rounds +2.0 [0.7, 3.2],
agent time +10.7 s [1.7, 19.3] (+22%), cost flat, judge +12 [8, 16], tests tie.
Round decomposition: test-edit rounds +1.2, source-edit rounds +0.85, reads
+1.0, test-run rounds -0.7. Vanilla mostly adds no tests; the judge rubric
scores the agent's own tests as one of five dimensions.

## Experiments

| ID | hypothesis | probe / baseline | evidence | outcome |
|---|---|---|---|---|
| E0 | the round gap is wrapper overhead or tool-error loops | transcript mining of the 4.49 arm vs r9 vanilla (no spend) | hook attachments small; tool errors 0.5/run (edit guard); gap is test-edit rounds; intake note's "tests first" ignored (source first in 20/20 runs) | rejected; target test-writing rounds |
| A1 | writing tests in the same message as the source Edits cuts rounds and keeps the judge score | QA bullet + "same message, one Edit per test file, in parallel" vs A0 | round 1: rounds -1.35 [-2.55,-0.15], time -6.3 s n.s.; replicate B1 vs B0: rounds +0.35 n.s., time +5.1 s n.s.; pooled n=40: rounds -0.5 [-1.25,+0.4], time -0.6 s; compliance 5/20 runs | rejected: round-1 result was noise |
| A2 | a `git diff` self-check before the report raises the judge score (top deductions: tracebacks on bad input, duplicated helpers, diff churn) | extra worker bullet vs A0 | followed in 18/20 runs; judge +1.5 [-1,+4], time +3.1 s n.s. | rejected: no measurable quality gain |
| B3 | a new test file written with one Write in the same message removes the test-file read and separate test rounds | QA bullet variant vs B0 | followed in 19/20 runs (Write to a test file 1.7 vs 0.4/run); rounds -1.35 [-2.05,-0.70], reads -0.95, cost -$0.01 [-0.03,-0.00] (-5%), time +0.6 s n.s., judge -1.5 n.s. | not integrated: no wall-time gain, conflicts with repository test layout outside the benchmark; single round, not replicated |

## What was learned

- Noise floor: the same base binary measured 9.9 vs 8.85 rounds and 62 vs 57 s
  in two concurrent rounds. A single round of 20 runs per arm, paired over 10
  tasks, produced a "significant" round cut (A1) that vanished on replication.
  Claim an XL improvement only after an independent replicate round.
- Fewer rounds do not mean less wall time: B3 cut 1.35 rounds and no time,
  because the same test code is generated in fewer, larger turns. Wall time
  tracks output tokens.
- Prompt rules that change *where* tests are written are followed (B3 19/20)
  more than rules that ask for parallel tool calls (A1 5/20), consistent with
  the 4.49 finding that concrete tool-naming rules work.

## Side findings (not fixed here)

- Harness fidelity: the proxy arms (`zirv-nojev`, `zirv-jev-*`) receive the
  UserPromptSubmit intake note. A real proxy-routed session sets
  `ZIRV_CTX_PROXY_DECIDED=1` and skips it (`hook/prompt.rs::intake_skipped_for_launch`),
  and `zirv ctx exec` scrubs the variable from its child environment, so the
  harness cannot set it from outside.
- `zirv workflow start` fails on t22 ("task summary exceeds 8192 bytes"), so
  long prompts never get a workflow.

## Part 2: chains and effort (operator bar: >=4% better on time, cost or quality)

Free mining of r10 chains (prices fitted from totals: input $2/M, cache read
$0.2/M, output $10/M, 5m write $2.5/M):
- t24b cost is ~60% cache reads; contexts grow to ~240k in both arms and
  never compact. `zirv ctx score` on a 236k transcript: score 0, "healthy".
- Cause: `rot::token_gates` scales the gate by model capacity (0.5 / 0.8);
  sonnet 5.5 reports a 1M window, so floor 500k / ceiling 800k. `exec`
  already compacts in place on a Compact verdict (`exec/compact.rs`); it
  just never fires.
- zirv's extra chain output vs vanilla is thinking (signature chars +17% on
  t24b, +33% on XL), not test code (tool input +4%).

| ID | hypothesis | probe / baseline | evidence | outcome |
|---|---|---|---|---|
| P1 | headless compaction works on a resumed `-p` session | haiku probe: `/compact` via `--resume`, then recall | same session id, compact boundary written, recall correct, context 118k -> 19k | mechanism confirmed |
| P2 | mid-step vs step-boundary compaction (gate 40k, t23 x1 each) | env gate 30k/40k vs harness pre-step `/compact` | mid-step 5x: $1.19, 739 s, judge .6; boundary 4x: $1.25, 399 s, judge .9 (r10 ref ~$0.89, ~270 s) | boundary far cheaper in time; both cost more at a low gate |
| C1a | pinning the gate to 100k/160k (real `exec` mid-step compaction) cuts t24b cost | t24b x2 vs base x2 | 1 compaction/run; reads -9.7%; cost +1.5%, wall +1.3%, step score -0.2%, judge -6.7% | rejected |
| C1b | compacting at step boundaries once context >= 160k cuts t24b cost | harness pre-step `/compact`, t24b x2 | 1 compaction/run; reads -9.7%; cost +0.7%, wall +11.1%, step score -3.4%, judge 0 | rejected: summary turn + re-reads + cache re-write eat the read saving (simulation predicted -34% reads) |
| E1 | effort `low` (vs Claude Code's default `medium`) cuts output, time and cost without losing quality | `ZIRV_CTX_HEADLESS_EFFORT_BOUNDED/SUBSTANTIAL=low`, XL x2, two independent rounds (E, F) | round E: cost -9.5%*, time -9.2%*, judge -1.5*; round F: cost -9%*, time -8.5% (CI touches 0), judge -2.0 n.s.; pooled n=40: cost -10.1% [-0.04,-0.01]$*, agent time -8.9%*, API time -9.0%*, output -8.6%*, rounds -1.2*, tests +1.07, judge -1.75 [-3.5,-0.25]* | **success, implemented** (default headless effort `low`); trade-off: judge -2.1% |
| E1-chain | same on t23 | t23 x3 per arm | cost -2.7%, API time +4.5%, step score -1.1%, judge tie (one run had a 422 s wall outlier) | no gain on short chains, no clear loss; n=3 |

Implementation: `HeadlessEffortConfig` defaults every class to `low`; an
operator value, `ZIRV_CTX_HEADLESS_EFFORT_*`, `CLAUDE_CODE_EFFORT_LEVEL` or
`--effort` still wins. The harness mirrors the shipped default.

## Next probe

- Validate the built default against the base binary on XL once more, then
  re-run t23/t24b with more reps to settle the chain effect of `low`.
- Compaction only pays when contexts are far above the point where the
  summary turn and re-reads are amortised; t24b at 160k is not that point.
