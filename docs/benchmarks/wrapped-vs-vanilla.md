# Wrapped vs vanilla: what `zirv ctx` costs and buys on short tasks

Recorded-measurement protocol and results for the question "what does wrapping
a Claude Code session in zirv do to speed, token cost and correctness?". Same
non-negotiable rule as every other file in this directory: every number below
was observed on a real machine with the harness in
`docs/benchmarks/wrapped-vs-vanilla/`, or the row says so and is absent. No
estimated or "should be roughly" number belongs here.

The headline, stated plainly: **on short headless tasks zirv is slower and
more expensive, and no more correct.** Wrapping added +34% to +39% wall-clock
and +25% to +41% list-price cost at the same model. Correctness did not move
at a level this benchmark can distinguish from noise: Sonnet solved every
task in every condition, and Haiku's improvement under zirv has a confidence
interval that includes zero. What zirv is designed to protect against --
context rot over long sessions, restarts with handoff, cross-harness
supervision -- never engages on a task that finishes in under four minutes,
so this benchmark measures the wrapper's **fixed overhead**, not its
long-session value. That value is not yet measured; see §6.

## 1. Conditions

All three run the same Claude Code binary (2.1.278) headlessly with
`--output-format json` in a fresh copy of the same target repository.

| Condition | Launch | What it exercises |
|---|---|---|
| `vanilla` | `claude -p --model <m> --settings '{"disableAllHooks":true}'`, prompt on stdin | Claude Code alone; the operator's global zirv hooks disabled so nothing of zirv leaks in |
| `zirv` | `zirv ctx exec --agent claude --prompt <p> -- --output-format json --model <m>` | The supervised wrapper: compiled system prompt (engineering standard, meta-harness rules, skill index, harness roster), zirv's PreToolUse/PostToolUse/Stop hooks, safety policy, sandbox posture (`--permission-mode dontAsk` plus allow/deny lists), registered `zirv:*` skills, the read-only zirv MCP server, rot scoring and restart supervision. Same pinned model as vanilla, so the diff isolates the wrapper |
| `zirv-proxy` | The runner replays `zirv chat`'s Jev intake, which headless `exec` skips: `zirv ctx proxy --json <p>` (TypeSafe Jev, `jev-1.13.0`) decides complexity/risk/workflow/seat tier; seat tier maps to `haiku`/`sonnet`/`opus`; the decided workflow is started with `zirv workflow start`; the `[zirv proxy]` layer is prepended to the prompt; then launched exactly like `zirv` | Everything in `zirv` plus Jev routing and workflow start. Its model is Jev's choice, not pinned |

`zirv ctx exec` compiles the **Worker** role prompt. The interactive
orchestrator prompt (`zirv chat`) is not what a headless run receives, so
delegation conventions, the dashboard, mail and pane spawning are out of
scope by construction.

## 2. Target project and tasks

A synthetic, stdlib-only Python 3.11 package ("ledgerlite", 444 LOC, 30
visible unit tests of which exactly one is deliberately red) with five
planted defects and twelve tasks. Prompts are written the way a user writes
them -- symptom first, never naming the file or function for bug tasks. The
template is frozen and identical for every run; hidden graders live outside
the repo the agent sees.

| Task | Kind | Grader |
|---|---|---|
| t01 tiebreak | answer | 3 required regexes over the reply (module/function, priority, tie-break order) |
| t02 pagination | tests | 7 hidden unittests (two planted off-by-one defects) |
| t03 money | tests | 6 hidden (parentheses negatives, thousands separators) |
| t04 budget | tests | 6 hidden (new module + CLI subcommand against a stated API) |
| t05 dedupe | judge | blind Sonnet judge, rubric: delegate to existing report functions, byte-identical output |
| t06 currency | tests | 8 hidden (field threaded through model, store, CSV import, report, CLI) |
| t07 redtest | tests | 3 hidden; score forced to 0 if the named test file was edited |
| t08 usage doc | judge | blind Sonnet judge with the real argparse definitions in the rubric |
| t09 count | answer | exact integer computed from the template's actual behaviour |
| t10 shares | tests | 7 hidden (largest-remainder rounding to exactly 100.00, tie-break by name) |
| t11 export | tests | 9 hidden (RFC-4180 quoting, inclusive bounds, ordering, exit code on bad range) |
| t12 dead code | answer | AST-derived truth; −0.25 per live function claimed dead on the final `DEAD:` line |

Score is 0..1 per run (share of hidden tests passed, share of required
facts, or judge/10). "Solve rate" is the share of runs scoring exactly 1.0.
Every tests-kind grader was verified to score 0 or partial on the pristine
template and 1.0 on a hand-applied correct fix before any run.

## 3. Protocol

- Runs interleave conditions rep-major (rep 1: t01 vanilla, t01 zirv, t01
  zirv-proxy, t02 vanilla, ...) so API-side drift lands on all conditions
  alike. 3 to 5 runs were in flight at any time; wall-clock therefore
  includes equal contention for every condition.
- Speed = wall-clock around the whole child process (for `zirv-proxy`,
  including the Jev call and workflow start). `duration_api_ms` from
  Claude's own result object is kept separately.
- Cost = `total_cost_usd` from Claude Code's result object (list price,
  `costBasis: "list"`), plus the Jev call priced at the catalogue's
  $0.042/MTok input (≈$0.0002 per call; cache hits cost 0). Tokens are
  the four raw classes summed (`input`, `cache_creation`, `cache_read`,
  `output`), i.e. the `TranscriptUsage` basis in `token-cost.md` §1.
- Intelligence = the per-run score above. Judge runs are re-scorable from
  the kept repos (`regrade.py`); one grader defect was found and fixed this
  way (an agent that *committed* its work made the judge's `git diff HEAD`
  empty; the judge now diffs against the template's root commit).
- Per-run timeout 20 min; 0 of 156 runs errored or timed out. No run hit a
  usage limit (checked: every run has a normal turn count and cost, and no
  output contains limit text).
- Machine: Windows 11, i9-13900K, zirv 4.20.0 (Chocolatey), Claude Code
  2.1.278, Python 3.11.0, 2026-09-22. Jev gates all on, credential present.

## 4. Results

### 4.1 Sonnet grid -- 12 tasks × 3 reps, n = 36 runs per condition

| Metric | vanilla | zirv | zirv-proxy | zirv vs vanilla | zirv-proxy vs vanilla | better |
|---|---|---|---|---|---|---|
| Speed: mean wall (s) | 55.3 | 76.9 | 68.5 | **+39.1%** | **+23.8%** | lower |
| Speed: median wall (s) | 40.9 | 54.8 | 54.8 | +33.8% | +33.8% | lower |
| Cost: mean $/task (list) | 0.236 | 0.332 | 0.348 | **+40.6%** | **+47.2%** | lower |
| Cost: mean tokens/task (4 classes) | 456,353 | 706,855 | 518,549 | +54.9% | +13.6% | lower |
| Cost: mean output tokens | 4,640 | 5,510 | 4,901 | +18.8% | +5.6% | lower |
| Intelligence: mean score | 0.986 | 1.000 | 1.000 | **+1.4%** | **+1.4%** | higher |
| Intelligence: solve rate | 0.944 | 1.000 | 1.000 | +5.9% | +5.9% | higher |
| Visible suite intact | 1.000 | 1.000 | 1.000 | 0 | 0 | higher |

Paired bootstrap (10,000 resamples, paired by task×rep, seed 0), zirv minus
vanilla: score +0.014 [0.000, +0.035]; cost +$0.096 [+0.056, +0.140]; wall
+21.6 s [+9.6, +34.1]. zirv-proxy minus vanilla: score +0.014 [0.000,
+0.035]; cost +$0.112 [+0.056, +0.169]; wall +13.2 s [+1.0, +26.3].

The whole score difference is two vanilla runs of t12 that listed one live
function as dead (−0.25 each). Every other one of the 108 runs scored 1.0.
At this ceiling the intelligence column says "no measurable difference", not
"zirv is 1.4% smarter".

### 4.2 Haiku grid -- 12 tasks × 2 reps, n = 24 runs per condition

Run because Sonnet saturated the suite; a weaker model gives correctness
room to move. `zirv-proxy` is omitted (it chooses its own model).

| Metric | vanilla | zirv | zirv vs vanilla | better |
|---|---|---|---|---|
| Speed: mean wall (s) | 72.0 | 95.7 | **+33.0%** | lower |
| Speed: median wall (s) | 61.5 | 80.9 | +31.5% | lower |
| Cost: mean $/task (list) | 0.127 | 0.159 | **+24.9%** | lower |
| Cost: mean tokens/task | 526,822 | 709,649 | +34.7% | lower |
| Cost: mean output tokens | 6,578 | 7,044 | +7.1% | lower |
| Intelligence: mean score | 0.959 | 0.990 | **+3.2%** | higher |
| Intelligence: solve rate | 0.833 | 0.958 | +15.0% | higher |
| Visible suite intact | 1.000 | 1.000 | 0 | higher |

Paired bootstrap, zirv minus vanilla: score +0.031 [−0.015, +0.085]; cost
+$0.032 [+0.012, +0.054]; wall +23.7 s [+13.7, +35.2]. The score interval
includes zero: Haiku under zirv solved t04 and t10 where vanilla Haiku
partially failed, and lost points on t12 where vanilla did not, over 24
pairs. Suggestive, not established.

### 4.3 Where the overhead comes from

From the Sonnet grid, means per run:

| | vanilla | zirv | zirv-proxy |
|---|---|---|---|
| context tokens per model turn (input + both cache classes ÷ turns) | 34.6k | 45.9k | -- |
| model turns | 12.7 | 15.1 | 11.8 |
| tool calls | 11.8 | 13.9 | 10.7 |
| API time (`duration_api_ms`) | 50 s | 59 s | 52 s |
| wall minus API time | 5 s | 18 s | 16 s |

Two mechanisms, roughly equal: (1) about 11k extra tokens of system prefix
per turn (`zirv ctx compile --measure` reports 17 KB for the composed layers
alone; the registered `zirv:*` skills and the MCP tool schemas add the rest),
paid as cache reads on every turn and as a cache write on the first; (2)
about 13 s of non-API wall time per run -- process supervision, prompt
compilation, and a hook subprocess on every tool call -- versus 5 s for bare
Claude. Wrapped runs also took two to three more turns on average.

### 4.4 What the zirv features actually did in a headless run

Counted from the transcripts of the 72 wrapped Sonnet runs:

| Per run | zirv | zirv-proxy |
|---|---|---|
| `zirv workflow …` calls by the agent | 0.00 | 0.00 |
| `zirv skill …` calls | 0.17 | 0.06 |
| `zirv ctx …` calls | 0.06 | 0.22 |
| subagents spawned | 0 | 0 |
| tool calls denied by the sandbox posture | 0.17 | 0.08 |

Jev routed every task deterministically across reps: t09 (count rows) to
`haiku` ($0.073 vs $0.231 vanilla Sonnet, still 100% correct); t04 and t06
to `opus` ($0.833 and $0.703 vs $0.485 and $0.322, no correctness gain
available at the ceiling); the other nine stayed on `sonnet`. Jev started a
workflow in 27 of 36 proxy runs (bugfix/feature/refactor/documentation
packs) and **the agent never advanced or consulted any of them**. The Worker
role prompt does not tell it to, and nothing gates a headless `-p` run on
workflow state. Skill loads were rare (8 of 72 runs) and none changed an
outcome that the grader could see.

Two operator-facing observations from the runs, not benchmark results:
`zirv ctx proxy` resolved the frontier seat to `gpt-5.6-sol` even for harness
`claude` on this machine (the operator's `chat.model` profile), which the
runner had to override with a tier→model map; and the sandbox posture denied
a tool call in 8 of 72 wrapped runs (the agents recovered every time).

## 5. Reading the numbers honestly

- The cost delta is a real, tight-CI **+25% to +41%** at equal model. The
  wall delta is **+33% to +39%**. Anyone wrapping short, one-shot tasks
  pays this for nothing this benchmark can see.
- The correctness delta is **not distinguishable from zero** on either
  grid. Twelve tasks and 2-3 reps give roughly 24-36 paired samples;
  a 3-point mean-score shift with a CI straddling zero is exactly what
  no-effect looks like at that n. Reporting the +15% Haiku solve rate
  without its interval would be the base-rate mistake `statistical-sanity`
  warns about: it is 20/24 versus 23/24.
- Multiple comparisons: eight metrics × two grids × two contrasts were
  computed; the two significant ones (cost, wall) are the ones with an
  obvious mechanism (§4.3), the near-significant one (score) has none.
- Selection: the tasks are short (16 s to 4 min), self-contained, in a
  repo the model reads whole. That is the population this generalises to.
  It says nothing about a 3-hour session, a 2M-token transcript, a crashed
  harness, or a task that needs a second harness.

## 6. Not yet measured

- **Long-session value.** The thing zirv exists for -- rot scoring, compaction
  advice, restart with handoff, cross-harness rollover -- needs sessions
  long enough to rot. A recorded protocol for that is the natural next
  benchmark: the same task set chained into one multi-hour session per
  condition, scoring the last tasks in the chain.
- **Interactive `zirv chat`.** The orchestrator prompt, dashboard panes,
  delegation and mail are not exercised by `-p`. `zirv chat` has no
  headless mode today, so this needs a PTY driver.
- **Release-build zirv.** The Chocolatey binary was used as installed; a
  debug-vs-release delta on the 13 s of non-API overhead is unmeasured.

## 7. Reproduce

```sh
cd docs/benchmarks/wrapped-vs-vanilla
python run.py --tasks all --conds vanilla,zirv,zirv-proxy --reps 3 --model sonnet --parallel 3
python aggregate.py --runs runs --out report-sonnet.md
python run.py --tasks all --conds vanilla,zirv --reps 2 --model haiku --runs-subdir runs-haiku --parallel 3
python aggregate.py --runs runs-haiku --out report-haiku.md
```

`results-sonnet.csv` and `results-haiku.csv` are the per-run rows behind
every number above; `report-*.md` are the aggregator's own output. The
harness needs `claude`, `zirv` and `git` on PATH, a Jev credential for the
`zirv-proxy` condition, and roughly $30 of list-price usage for the two
grids as recorded.
