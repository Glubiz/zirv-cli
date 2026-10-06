# FAFO round 2 record (2026-10-06)

Goal: >=4% better cost, speed or quality of zirv-supervised Claude work, with no
more than marginal losses elsewhere. Every accepted finding is proven by a
benchmark or test (operator, 2026-10-06). Baseline: zirv 4.54.0 (origin/main
851527bb: headless effort low, 5m TTL, lean).

Outcome: two levers cleared the bar and are implemented in 4.55.0. Headless
lean launches also deny ScheduleWakeup, ShareOnboardingGuide, ListAgents,
ReportFindings and (for non-spawning roles) Agent: cost -8.2%* pooled over two
rounds on 4.54.0, time and quality unchanged. A lean `zirv:worker` subagent
type (no Artifact/Agent): subagent cost -11.7%*, run total -7.1%* pooled over
two rounds, quality unchanged. On this machine's real spend mix the two
together are about -1.2%, because they touch only headless workers and
general-purpose subagent dispatches.

Where the money goes (30 days, 1289 non-bench sessions, zirv price table
catalogue.rs:88-123; scripts /tmp/claude-501/fafo-r2/{an,q,q2,q3}.py):
subagent 55.7%, main-interactive 43.9%, headless 0.4%. Cache reads 45.7%.
TTL is already right everywhere: main 1h (5m would cost +40.9%), subagents 5m
(1h would cost +14%), headless 5m (shipped). Independent validator
(validate/v.py, own extraction): shares 55.7/43.9/0.4% of $5116, write TTL
split, repricing (+39.5% / -12.5%), Artifact entry ~55.0k chars in 44 of 45
recent subagent snapshots (median first prompt 54.5k tok), and subagent call
counts (Agent 281, Artifact 6, Skill 78) all confirmed.

| ID | hypothesis | probe / baseline | evidence | outcome |
|---|---|---|---|---|
| Y1 | zirv's per-tool-call hooks cost >= 4% of agent time | 15 timed `zirv ctx hook pretool/posttool` calls, isolated state dir | 13.6 ms each, 2 per tool call, ~20 calls/XL run = ~0.6 s of 55 s (~1%) | rejected |
| Y2 | a terse final report cuts headless output/time | 38 recorded XL runs (G/H arm A) | final message = 6.5% of output tokens; halving it = ~3% of output, ~1% cost | rejected (arithmetic) |
| Y3 | zirv's own skill listing is worth trimming | skill_listing attachment in main + subagent transcripts | zirv:* entries 11.9k chars (~3k tok) of ~54-67k prefix; full removal ~1% of total spend | rejected (arithmetic) |
| Y4 | headless workers carry dead tool definitions | snapshot tools of a 4.54.0 headless XL run; `/context` with `ZIRV_CTX_HEADLESS_DISALLOWED_TOOLS` (free) | ScheduleWakeup 8.6k, Agent 5.7k, ReportFindings 2.9k, ListAgents 2.1k, ShareOnboardingGuide 1.9k chars; denying the first four: system tools 7.1k -> 3.2k tok; predicted ~-7% headless cost | R1 (A vs T, XL t13-t22 x2, sonnet; NOTE binary built 13:35, before the 5m-TTL and lean defaults, so baseline = effort low, 1h writes, no lean): cost -10.4% [-18%, -3.7%]*, at 5m pricing -11.5%*, read tok -24%*, write tok -5.4%*, agent time -4.4% n.s., output -5.2% n.s., turns -1.45 n.s., tests -0.5 n.s., judge -1.5 n.s. -> replicate R2 on the installed 4.54.0 (lean, 5m). R2 (shipped 4.54.0, lean + 5m verified in launch line): cost -5.3% [-12.5%, +0.8%], read tok -16.7%*, write tok -2.3% n.s. (the tool block is cached across back-to-back runs), turns -0.05, output +2.2%, agent time +2.7%, tests -0.5, judge +1.0 (all n.s.) -> R3 for power. R3 (20/20, resumed after a disk-full stop): cost -11.0% [-14.6%, -7.1%]*, read tok -24.7%*, write tok -5.9%*, agent time -2.8% n.s., turns -0.95 n.s., tests -0.95 n.s., judge -2.0 n.s. Pooled R2+R3 (shipped baseline): cost **-8.2% [-12.5%, -4.1%]***, read tok -20.8%*, write tok -4.1%*, agent time -0.1%, wall +0.3%, turns -2.6% n.s., tests -0.7 [-1.8, 0], judge -0.5 n.s. The test dip is one flaky task: t21_search scored 90 in 3 of 6 T runs vs 0 of 6 A runs, and in 3 of 10 earlier G/H runs without the diet; no arm ever called Agent. -> **success, implemented** (lean also denies these tools) |
| Y5 | native subagents carry dead tool definitions | prompt_snapshot of recent subagents | Artifact 55k chars (~13.7k tok, ~25% of the 54k-tok subagent prefix), Agent 5.6k; but 30-day subagent calls: Agent 281, Artifact 6, Skill 78 | candidate: lean `worker` type; haiku probe ($0.08) confirmed custom-agent `disallowedTools` strips definitions and `--agents` can override general-purpose; Artifact is interactive-only, so bench needs interactive mode. S1 (sbench.py: interactive claude in a PTY, parent sonnet delegates the XL task verbatim to one subagent; G = general-purpose, W = `worker` with the same body + disallowedTools Artifact,Agent,ScheduleWakeup,ShareOnboardingGuide,ListAgents,ReportFindings,Workflow; XL t13-t22 x2, 40/40 valid): sub cost -16.8% [-22%, -12%]*, total -10.5%*, sub first prompt -12.4k tok (-42.7%), wall -20.6%* and sub time -25.7%* (driven by two G outliers, 168 s and 215 s; not claimed), sub output -5.4% n.s., tests +0.75 n.s., judge -0.5 n.s.; t14 W fixed bugs via Bash only (G used Edit/Read), judge 0.6 vs 0.7 both reps -> watch in S2. Supervisor ruling c04ac722: ship as plugin agent `zirv:worker`; route only when the plugin attaches; keep the omitted-model guard. Haiku probe: plugin-agent frontmatter `disallowedTools` strips the definitions. -> replicate S2. S2: runs from t19 rep 2 on were first contaminated (a second driver pair was started while the first was still alive; a sandboxed `ps` read as "no process"); those 7 runs per arm were moved aside and re-run with one driver per arm. S2 (complete, 19/20 valid; one W run dispatched two subagents): sub cost -6.3% [-14.3%, +1.7%], total -3.6% n.s., sub output +12.5%*, wall +3.9% n.s., tests +0.2, judge +2.0 n.s. Pooled S1 + S2 (20 task-round pairs): sub cost **-11.7% [-16.9%, -6.2%]***, total **-7.1% [-10.4%, -3.6%]***, wall -9.3% n.s. (not claimed), sub output +3.1% n.s., tests +0.5, judge +0.75 (n.s.). Edit style: both arms mostly write files through Bash heredocs in this bench (no zirv hooks); `worker` used Edit/Write in 2 of 32 runs vs 10 of 33; python/sed splices 10 vs 7. -> **success, implemented** as plugin agent `zirv:worker`, routed only when the plugin attaches; artifact-publishing workers stay `general-purpose` |

Real-spend projection (composed, not measured): `general-purpose` subagents were $1054 of $5116 (30 days); the measured 12.4k-token cut repriced over their recorded requests saves $62 (5.8% of them, 1.2% of all spend). Headless is 0.4% of spend, so its -8.2% is ~0.03%. Custom/team subagents ($1739, 34% of spend) also carry Artifact in 86% of cases: the next probe.

Side findings:
- The old bench binary in /tmp/claude-501/fafo881/bin predates the 5m-TTL and lean defaults; R1 ran on it. Check `--version` and the launch line (`-lean` settings path) before a round.
- Worktree target/ dirs filled the disk mid-round (ENOSPC stops every Claude Code command, including `!`); R3/S2 resumed after cleanup.
- A sandboxed `ps -p` reports live processes as gone; check detached runners unsandboxed.
- The review finding "sub-orchestrators get no zirv:worker routing" was rejected: SUB_ORCHESTRATOR_PROMPT dispatches through `zirv agent`, not the native Agent tool.

Tools: launch.sh (headless rounds), reprice.py (headless analysis), sbench.py (interactive subagent rounds, PTY-driven, imports the harness), sstats.py (subagent analysis). Spend: ~140 XL headless runs, ~95 interactive parent+subagent runs, a few haiku probes; the 5-hour window went from 2% to 90%.
