# FAFO record: faster / cheaper / better zirv-wrapped agents (2026-10-06)

Goal: cut wall time and/or cost of zirv-wrapped headless sessions, or raise
quality, without lowering hidden-test pass rate or judge score.

Outcome: no prompt change cleared replication. Nothing was integrated. The
measured gap to vanilla is test writing that also earns the judge lead, and
three worker-prompt rules aimed at it either did not replicate, did not move
wall time, or did not move quality.

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

## Next probe

Wall time is output generation, mostly test code that earns the judge lead,
so the XL tasks have little left to cut with prompt wording. The larger
measured gaps are on the long chains (r9/r10: t23 time +14%, t24b time +15%),
where context management is what zirv adds. Run a t23 A/B with a replicate
round before changing chain-time behaviour.
