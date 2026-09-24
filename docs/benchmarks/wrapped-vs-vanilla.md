# Wrapped vs vanilla: what `zirv ctx` costs and buys

Recorded-measurement protocol and results for the question "what does wrapping
a Claude Code session in zirv do to speed, token cost and correctness?". Same
non-negotiable rule as every other file in this directory: every number below
was observed on a real machine with the harness in
`docs/benchmarks/wrapped-vs-vanilla/`, or the row says so and is absent. No
estimated or "should be roughly" number belongs here.

This is the third generation of this benchmark; §6 covers what changed and
why the earlier grids aren't directly comparable to this one.

**Current headline (2026-09-23, zirv 4.21.0 vs. vanilla `claude -p` with the
obra/superpowers plugin loaded, 15 tasks, both sides given the same
"finish the task, nobody will answer questions" notice):** on Sonnet, zirv is
cheaper and faster overall (-22% to -24%) at no significant change in
correctness; zirv-proxy (Jev-routed model/seat/workflow selection) is
cheaper still (-46%). On Haiku, zirv is essentially the same speed and cost
but significantly more correct (+18%). Split by size, the picture is not
uniform: on the nine large-task runs per condition, zirv trades about 12
points of Sonnet correctness for roughly half the cost and time, because
superpowers' brainstorm/TDD discipline lets vanilla Sonnet score a perfect
1.000 there. Much of zirv's score lead, on both models, comes from vanilla
superpowers still stopping 8 times out of 75 to ask for approval despite the
notice -- a failure mode zirv never exhibited. This remains a headless,
single-session benchmark; see §7.

## 1. Conditions

All conditions run the same Claude Code binary headlessly with
`--output-format json` in a fresh copy of the same target repository.

| Condition | Launch | What it exercises |
|---|---|---|
| `vanilla` | `claude -p --model <m> --setting-sources project,local --permission-mode bypassPermissions --plugin-dir <obra/superpowers>`, prompt on stdin | Claude Code with the obra/superpowers v6.4.1 plugin loaded (brainstorm/plan/TDD workflow) and its own `SessionStart` hook running; the operator's user-settings layer is dropped so zirv's global hooks can't leak in |
| `zirv` | `zirv ctx exec --agent claude --prompt <p> -- --output-format json --model <m>` | The supervised wrapper: compiled system prompt, zirv's PreToolUse/PostToolUse/Stop hooks, safety policy, sandbox posture, registered `zirv:*` skills, the read-only zirv MCP server, rot scoring and restart supervision. Same pinned model as vanilla, so the diff isolates the wrapper |
| `zirv-proxy` | `zirv ctx proxy --json <p>` (Jev) decides complexity/risk/workflow/seat tier; seat tier maps to `haiku`/`sonnet`/`opus`; the decided workflow is started with `zirv workflow start`; the `[zirv proxy]` layer is prepended to the prompt; then launched like `zirv` | Everything in `zirv` plus Jev routing and workflow start. Its model is Jev's choice, not pinned |
| `zirv-jev-full` | Identical launch to `zirv-proxy`, with every `[jev]` advisory gate (memory, supervisor, dispatch, review, gates, context, intake_savings, review_reuse, harvest_screen, admin_dispatch) turned on via `ZIRV_CTX_JEV_*` env vars for the proxy call, workflow start, and exec launch alike | Everything in `zirv-proxy` plus every other Jev-gated advisory site (memory rerank, dispatch tier, context prune, review reuse/dedup, crash/judge supervisor, admin dispatch). Issue #758: not yet measured |
| `zirv-jev-<gate>` | Same as `zirv-jev-full` but with only one `[jev]` gate on (e.g. `zirv-jev-memory`, `zirv-jev-dispatch`) | Per-gate ablation, to decide which gates earn default-on status. Issue #758: not yet measured |

Every condition's prompt is prefixed with the same fixed notice: "You are
running non-interactively: nobody will answer questions or approve plans.
Make reasonable decisions yourself and complete the task end to end." This is
new since the previous grid (§6.2) and exists so neither side can look cheap
or fast by stopping to ask instead of finishing.

`zirv ctx exec` compiles the **Worker** role prompt; the interactive
orchestrator prompt (`zirv chat`) is not what a headless run receives, so
delegation conventions, the dashboard, mail and pane spawning are out of
scope by construction.

## 2. Target project and tasks

A synthetic, stdlib-only Python 3.11 package ("ledgerlite", frozen template)
with planted defects and fifteen tasks: t01-t12 are small (16 s to 5 min,
answer/tests/judge graders), t13-t15 are large (10-25 min, multi-module
features or a multi-bug sweep, 30-80 tool calls, hidden tests). Prompts are
written the way a user writes them -- symptom first, never naming the file
or function for bug tasks. Hidden graders live outside the repo the agent
sees; every tests-kind grader was verified to score 0 or partial on the
pristine template and 1.0 on a hand-applied correct fix before any run.

| Task | Kind | Grader |
|---|---|---|
| t01 tiebreak | answer | 3 required regexes over the reply |
| t02 pagination | tests | 7 hidden (two planted off-by-one defects) |
| t03 money | tests | 6 hidden (parentheses negatives, thousands separators) |
| t04 budget | tests | 6 hidden (new module + CLI subcommand) |
| t05 dedupe | judge | blind Sonnet judge, refactor quality |
| t06 currency | tests | 8 hidden (field threaded through model/store/CSV/report/CLI) |
| t07 redtest | tests | 3 hidden; score forced to 0 if the named test file was edited |
| t08 usage doc | judge | blind Sonnet judge against the real argparse definitions |
| t09 count | answer | exact integer computed from the template's actual behaviour |
| t10 shares | tests | 7 hidden (largest-remainder rounding, tie-break by name) |
| t11 export | tests | 9 hidden (RFC-4180 quoting, bounds, ordering, exit code) |
| t12 dead code | answer | AST-derived truth; penalty per live function claimed dead |
| t13 recurring (large) | tests | 20 hidden -- new `recurring.py` module, persistence, two CLI surfaces, weekly/monthly/yearly expansion with clamping |
| t14 bugsweep (large) | tests | 20 hidden -- one prompt reporting four bugs at once (pagination, money parsing, case-sensitive rules, a category-erasure regression); score forced to 0 if the protected test file is touched |
| t15 reports (large) | tests | 17 hidden -- new monthly-breakdown and trend reporting functions plus a CLI command, byte-exact output in two formats |

Score is 0..1 per run. "Solve rate" is the share of runs scoring exactly 1.0.

## 3. Protocol

- Runs interleave conditions rep-major so API-side drift lands on all
  conditions alike; several runs were in flight at any time, so wall-clock
  includes equal contention for every condition.
- Speed = wall-clock around the whole child process (for `zirv-proxy`,
  including the Jev call and workflow start).
- Cost = `total_cost_usd` from Claude Code's result object (list price),
  plus the Jev call for `zirv-proxy`.
- Intelligence = the per-run score above.
- Per-run timeout 20 min; 0 of 195 runs (135 Sonnet + 60 Haiku) errored or
  timed out.
- Machine: Windows 11, i9-13900K, a zirv 4.21.0 build under test (PR #736:
  seat-tier recalibration, a skill pointer instead of the full skill index
  for headless Worker/Single seats, adapter-readiness-probe skip cutting
  hook start-up from ~290 ms to ~16 ms, and wider recursive temp-scratch
  deletion), Claude Code, obra/superpowers v6.4.1, 2026-09-23.

## 4. Results

### 4.1 Sonnet -- 15 tasks x 3 reps, n = 45 runs per condition

| Metric | vanilla | zirv | zirv-proxy | zirv vs vanilla | zirv-proxy vs vanilla | better |
|---|---|---|---|---|---|---|
| Speed: mean wall (s) | 119.0 | 92.9 | 96.4 | **-22%** [-55, -3 s] | **-19%** [-51, +0.2 s] | lower |
| Cost: mean $/task (list) | 0.501 | 0.380 | 0.273 | **-24%** [-0.29, -0.003] | **-46%** [-0.42, -0.08] | lower |
| Intelligence: mean score | 0.933 | 0.972 | 0.984 | +4.1% [-0.046, +0.129] (n.s.) | +5.4% [-0.017, +0.133] (n.s.) | higher |

Intervals are paired bootstrap 95% CIs (10,000 resamples, paired by
task x rep, seed 0); "n.s." = the interval straddles zero.

### 4.2 Haiku -- 15 tasks x 2 reps, n = 30 runs per condition

| Metric | vanilla | zirv | zirv vs vanilla | better |
|---|---|---|---|---|
| Speed: mean wall (s) | 110.3 | 113.1 | +2.5% [-38, +39 s] (n.s.) | lower |
| Cost: mean $/task (list) | 0.208 | 0.197 | -5.5% [-0.099, +0.063] (n.s.) | lower |
| Intelligence: mean score | 0.822 | 0.972 | **+18.3%** [+0.031, +0.287] | higher |

### 4.3 Small (t01-t12) vs large (t13-t15) tasks

Large-task cells are n=9 per condition (Sonnet) or n=6 (Haiku) -- treat as
indicative, not conclusive.

| Grid | wall_s | cost ($) | score |
|---|---|---|---|
| Sonnet small -- vanilla | 82.5 | 0.336 | 0.917 |
| Sonnet small -- zirv | 77.2 (-6%) | 0.334 (-1%) | 0.994 (+8%) |
| Sonnet small -- zirv-proxy | 72.0 (-13%) | 0.236 (-30%) | 1.000 (+9%) |
| Sonnet large -- vanilla | 265.2 | 1.161 | 1.000 |
| Sonnet large -- zirv | 155.4 (-41%) | 0.568 (-51%) | 0.882 (-12%) |
| Sonnet large -- zirv-proxy | 194.0 (-27%) | 0.421 (-64%) | 0.919 (-8%) |
| Haiku small -- vanilla | 81.6 | 0.145 | 0.865 |
| Haiku small -- zirv | 88.9 (+9%) | 0.155 (+7%) | 0.981 (+13%) |
| Haiku large -- vanilla | 225.1 | 0.461 | 0.649 |
| Haiku large -- zirv | 209.7 (-7%) | 0.364 (-21%) | 0.933 (+44%) |

Per-task tables, features-used counts and zirv-proxy's per-task
seat/model/workflow decisions come from `aggregate.py` (see §8); the raw
results are not committed.

## 5. Reading the results honestly

- **Superpowers makes vanilla do more work.** Its brainstorm/plan/TDD flow
  is slower and roughly 2x costlier than zirv on large tasks, but it scored
  a perfect 1.000 there on Sonnet, where zirv lost points -- one t14 run
  modified the protected test file (forced to 0), and t13/t15 runs passed
  only part of the hidden suite. On large Sonnet tasks, zirv trades about
  12 points of correctness for roughly half the cost and time; zirv-proxy
  recovers some of that correctness (-8% vs vanilla, not -12%) while still
  cutting cost by 64%.
- **The notice didn't fully stop superpowers from asking.** Despite the
  "nobody will answer, finish it yourself" prefix, vanilla+superpowers still
  ended 8 of 75 runs by asking for approval instead of implementing -- all 3
  Sonnet t06 runs, and Haiku t04, t06, t11, t13, t15 -- scoring 0 or near it.
  zirv never did. Much of zirv's score lead in §4.1/4.2 comes directly from
  this, not from zirv writing better code.
- **Without the notice, vanilla looked artificially cheap.** The earlier
  superpowers-baseline grid (zirv 4.20.0, no notice; §6.2) had vanilla stop
  to ask in 10 of 36 Sonnet runs, producing a headline of 46 s / $0.21 mean
  -- fast and cheap only because a third of its runs did nothing. That grid
  is not comparable to this one; it's why the notice was added.
- **The zirv-itself optimization effect is modest and partly confounded.**
  Comparing zirv 4.20.0 to 4.21.0 on the small-task grid only (Sonnet): mean
  wall 82.3 s -> 77.2 s (-6%), mean cost $0.347 -> $0.334 (-4%). The 4.20.0
  grid lacked the notice, but zirv itself never asked questions in either
  grid, so the confound from that difference is small. The Haiku 4.20.0 grid
  ran during a usage-limit slowdown, so its before/after is not reported.
- **Sample sizes.** Large-task cells are n=9 (Sonnet) or n=6 (Haiku) per
  condition; treat those numbers as indicative, not conclusive. The small-task
  cells (n=36 Sonnet, n=24 Haiku) are the same size as the historical grids.
- **Selection.** This is still a headless benchmark: no long interactive
  session, no context rot, no restart-with-handoff. It measures the
  wrapper's overhead and its effect on one-shot task correctness, not the
  long-session value zirv is built for (§7).

## 6. History: earlier grids

Two earlier grids used this same template and task set (minus t13-t15,
which didn't exist yet) and are kept here for the record. Neither is
directly comparable to §4 -- read why before citing either one.

### 6.1 Original grid (2026-09-22, zirv 4.20.0, disableAllHooks vanilla)

The first version of this benchmark compared zirv to a completely bare
`claude -p --settings '{"disableAllHooks":true}'` -- no plugin at all, no
non-interactive notice, 12 tasks. On that comparison, wrapping was pure
overhead: Sonnet wall +39% (55.3 s -> 76.9 s), cost +41% ($0.236 ->
$0.332); Haiku wall +33% (72.0 s -> 95.7 s), cost +25% ($0.127 -> $0.159).
Correctness didn't move outside noise on either grid (Sonnet was already at
a 0.986-1.000 ceiling). The raw results are not committed.

This grid answers a narrower question than §4 -- "what does zirv cost over
nothing at all" -- rather than "what does zirv cost over a comparably
capable competing harness setup", which is why superpowers was added next.

### 6.2 Superpowers-baseline grid (2026-09-22, zirv 4.20.0, no notice)

Same 12 tasks, vanilla now loads obra/superpowers, but neither side yet had
the anti-stalling notice. Sonnet: vanilla mean wall 46.2 s, cost $0.208,
score 0.715 (108 runs); zirv 82.3 s / $0.347 / 0.979; zirv-proxy 78.7 s /
$0.339 / 0.993. Haiku: vanilla 95.7 s / $0.151 / 0.484 (48 runs); zirv
145.0 s / $0.216 / 0.733. Vanilla's low wall-clock and cost here are not a
sign of efficiency: it stopped to ask for approval in 10 of 36 Sonnet runs
and a comparable share of Haiku runs, scoring nothing on those and exiting
fast. That artifact is exactly why §1's notice exists in the current grid.

## 7. Not yet measured

- **Long-session value.** The thing zirv exists for -- rot scoring,
  compaction advice, restart with handoff, cross-harness rollover -- needs
  sessions long enough to rot. A chained multi-hour version of this task set
  is the natural next benchmark.
- **Interactive `zirv chat`.** The orchestrator prompt, dashboard panes,
  delegation and mail are not exercised by `-p`. `zirv chat` has no headless
  mode today, so this needs a PTY driver.
- **Release-build zirv.** This grid ran a locally-built zirv under test
  (`--zirv-dir`), not the Chocolatey-installed release; that's the right
  binary for isolating the wrapper's own changes but leaves any
  packaging-specific overhead unmeasured.
- **Every `[jev]` gate beyond intake routing (issue #758).** `zirv-proxy`
  above only exercises Jev-routed model/seat/workflow selection at intake;
  memory rerank, dispatch tier, context prune, review reuse/dedup, the
  crash/judge supervisor, and admin dispatch have no before/after
  measurement of their own, only the `jev-effects.jsonl` byte-count proxy
  `zirv ctx jev status` now rolls up. The `zirv-jev-full` and
  `zirv-jev-<gate>` conditions in §1 and the commands in §8 exist to close
  this gap; not yet run.

## 8. Reproduce

```sh
cd docs/benchmarks/wrapped-vs-vanilla
python run.py --tasks all --conds vanilla,zirv,zirv-proxy --reps 3 --model sonnet --parallel 3 \
  --noninteractive --vanilla-plugin-dir <path to obra/superpowers plugin> --zirv-dir <path to zirv.exe under test>
python aggregate.py --runs runs --out report-opt-sonnet.md

python run.py --tasks all --conds vanilla,zirv --reps 2 --model haiku --runs-subdir runs-haiku --parallel 3 \
  --noninteractive --vanilla-plugin-dir <path to obra/superpowers plugin> --zirv-dir <path to zirv.exe under test>
python aggregate.py --runs runs-haiku --out report-opt-haiku.md
```

`aggregate.py` writes the per-task report and a per-run CSV. Passing
`--tasks t01,t02,...,t12` and dropping `--vanilla-plugin-dir`/
`--noninteractive` reproduces the §6.1 grid; keeping
`--vanilla-plugin-dir` but dropping `--noninteractive` reproduces §6.2. The
harness needs `claude`, `zirv` and `git` on PATH, a Jev credential for the
`zirv-proxy` condition, and roughly $65 of list-price usage for the two
current grids as recorded (45 x (0.501+0.380+0.273) + 30 x (0.208+0.197)).

**Every `[jev]` gate (issue #758, not yet measured).** Add `zirv-jev-full`
against the same baselines to measure every advisory gate at once:

```sh
cd docs/benchmarks/wrapped-vs-vanilla
python run.py --tasks all --conds vanilla,zirv,zirv-proxy,zirv-jev-full --reps 3 --model sonnet --parallel 3 \
  --noninteractive --vanilla-plugin-dir <path to obra/superpowers plugin> --zirv-dir <path to zirv.exe under test>
python aggregate.py --runs runs --out report-jev-full-sonnet.md
```

For per-gate ablations, list the `zirv-jev-<gate>` conditions to run
alongside `zirv-proxy` (any subset of `memory`, `supervisor`, `dispatch`,
`review`, `gates`, `context`, `intake_savings`, `review_reuse`,
`harvest_screen`, `admin_dispatch`):

```sh
python run.py --tasks all --conds vanilla,zirv-proxy,zirv-jev-memory,zirv-jev-dispatch,zirv-jev-review \
  --reps 3 --model sonnet --parallel 3 \
  --noninteractive --vanilla-plugin-dir <path to obra/superpowers plugin> --zirv-dir <path to zirv.exe under test>
python aggregate.py --runs runs --out report-jev-ablation-sonnet.md
```

Every `zirv-jev-*` condition needs the same Jev credential as `zirv-proxy`
(`TYPESAFE_API_KEY` by default) exported in the shell `run.py` runs in --
the gate env vars alone never activate an advisory site without it. `zirv
ctx proxy --json` still runs once per `zirv-jev-*` run, same as
`zirv-proxy`, so its per-run cost/latency is on top of whatever the newly
enabled gates themselves spend; `report-jev-full-sonnet.md`'s "Robustness"
section pairs each `zirv-jev-*` condition against both `vanilla` and
`zirv-proxy` so the gates' own marginal effect is visible separately from
intake routing's.
