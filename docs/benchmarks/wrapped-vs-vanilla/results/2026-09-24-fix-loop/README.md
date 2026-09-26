# Benchmark → fix loop, 2026-09-24/25: results and handover

**Target (operator):** zirv with Jev **off** and zirv with Jev **on** must each be at least **20% cheaper**, at least **20% faster**, and at least **1% better in work quality** than vanilla Claude Code with the superpowers plugin (v6.4.1). Tasks should be large or long-running. Beating the target is welcome.

**Status: not met. Round 7 was the full and final benchmark** (4.30.0 at ff4fc885, all 20 Jev gates on in `zirv-jev-full`, same headless levers and intake in both zirv conditions). Cost and work quality clear the bar on the large tasks; time does not, in any condition, on any task group:

| r7, vs vanilla + superpowers | Jev off: cost / time / judge | Jev on: cost / time / judge |
|---|---|---|
| XL t13–t22 (2 reps, 60 runs) | **−20% sig** / +19% worse / **+9% sig** | **−22% sig** / −3% / **+11% sig** |
| t23 9-step chain (3 reps) | **−42%** / −32%* / +0% | −19% / −14%* / +0% |
| t24b 22-step chain (3 reps) | −16% / −11%* / **+9%** (+2 test pts) | −3% / −8%* / **+9%** (+3 test pts) |

\* Chain time is the agent's own time (sum of Claude's per-step `duration_ms`). The measured chain wall included the harness's stagger wait (see Round 7 results), so it is ~equal for every condition and not usable. Claude's `duration_ms` leaves out zirv's wrapper time: about 2 s per launch more than vanilla, plus a one-time intake on step 1.

Every round directory here holds one `<task>__<cond>__rN.json` per run (the harness's `result.json`) and a `report.md` regenerated with the current `compare.py`. To recompute a report, copy a round's JSON files back into `<dir>/<name>/result.json` and run `compare.py --runs <dir>`.

## Rounds

Changes are relative to vanilla + superpowers. "sig" means the paired-bootstrap 95% CI over tasks excludes zero; everything else is within noise.

| Round | zirv build | Tasks (runs) | Jev off: cost / time / judge | Jev on: cost / time / judge | Validity |
|---|---|---|---|---|---|
| r0 | origin/main 4.27.0 | 15 mostly small (85) | +29% / +15% / – | +18% / +115% / – | **Unfair:** zirv also loaded operator plugins vanilla lacked; no judge yet |
| r1 | fc59babb | 10 large t13–t22 (60) | −11% / −2% / −9% sig | −7% / +28% / −10% sig | **Unfair:** zirv's user-level hooks were stripped |
| r2 | f4df7785 | subset t16,t17,t18,t22 (24) | −13% sig / −13% sig / −4% | −13% sig / −9% sig / −2% | **Unfair:** zirv's user-level hooks were stripped |
| r3 | 1e1add75 | subset + t23 chain (30) | −11% / −4% / +3% | −21% / −11% / −1% | **Invalid:** zirv's user-level hooks were stripped |
| r3b | 1e1add75, fair harness | subset + t23 (30) | +2% / +6% / **+7% sig** | −19% / −10% / +5% | Valid |
| r4 | 5ca6b180 + 6414a21c | subset + t23 (30) | **−6% / −7% / +7% sig** | −5% / −6% / +4% | Valid |
| r4-t24 | same as r4 | t24 22-step pilot (3, 1 rep) | see below | see below | **Broken:** rot-handling bugs; costs undercounted |
| r5 | 4.30.0 (2b95a3b7): all 17 Jev gates on, headless levers (5m TTL, lean, medium effort) | subset + t23 (30) | −15% / −12% / +1% | −16% / −8% / +1% | Valid; t23 hit by the effort-flip cache bug (+8% / +5% cost) |
| r5b | 2e943093 + d41e9976: sticky effort, no Jev approve call under dontAsk | t23 chain (6) | **−24% / −25% / +23%** | **−47% / −36% / +23%** | Valid, 1 task × 2 reps |
| r5b-t24 | same as r5b | t24 22-step chain (6) | **−27%** / −15% / +0% | **−29%** / −16% / +0% | Valid; no compaction or restart fired (1M window) |
| r6 | aee6d976: + read-only approve skip, Jev keep-alive relay, scope guard | subset + t23 (30) | **−28% / −12% / +10% sig** | −22% / −6% / +4% | Valid; t17 Jev on r1 was handed to codex at the session limit and re-run (harness fix a1617d4b) |
| r6-t24 | same as r6 | t24 22-step chain (6) | **−33% / −20% / +7%** (+5 score pts) | **−33%** / −13% / +7% (+4 pts) | Valid; Jev off meets the target on t24 |
| r7 | ff4fc885: + shell-edit checkpoint, tests owed at first edit, stated-details list, headless single seat and nojev intake parity, `[jev] missing_tests`/`launch_effort`/`compaction_select` | XL t13–t22 (60) | **−20% sig** / +19% (worse, sig) / **+9% sig** | **−22% sig** / −3% / **+11% sig** | Valid; Jev off time is a 15 s intake timeout; t14 grader zeroes an additive test (see Round 7) |
| r7-t23 | same as r7 | t23 chain (9) | **−42%** / −32%* / +0% | −19% / −14%* / +0% | Valid; *agent time, measured wall is stagger-bound |
| r7-t24b | same as r7 | t24b 22-step chain (9) | −16% / −11%* / +9% (+2 pts) | −3% / −8%* / +9% (+3 pts) | Valid; *agent time, measured wall is stagger-bound |

About r1–r3: from fc59babb on, the harness ran zirv with `--setting-sources project,local`, which drops `~/.claude/settings.json`, where zirv's own hooks live. Fixed in d256124e. From r3b on, zirv keeps the user layer and vanilla receives the same `enabledPlugins` through `--settings`.

On the t23 chain, a 9-step session, r4 has both zirv conditions at −13 to −15% cost and −14% time. This is the only place zirv wins consistently.

### The t24 long-haul pilot (r4-t24)

t24 is a 22-step session with recall dependencies, built to push context past 150k. Vanilla ran all 22 steps in one conversation, peaking at 209k context with no compaction, and scored 0.92 at $3.26.

Both zirv runs broke at steps 15–16:

1. **Rot fired at about 160k tokens.** The claude adapter assumes a window of about 200k (token floor 0.5 and ceiling 0.8 of it), but Claude Code reports `modelUsage.claude-sonnet-5.contextWindow = 1000000`.
2. **Headless compaction was killed mid-flight.** Claude's PreCompact hook fired, and zirv logged "compact command timed out" about 20 s later. It then killed the session and restarted with a distilled handoff, and that step still passed.
3. **The harness kept resuming the old session id.** It did not follow the new conversation, so rot re-fired on every step and the restart breaker tripped: exit 75, and steps 18–22 failed. The harness is fixed in 28993c15.

Before any rot event (steps 1–14) the runs compare as follows:

| Steps 1–14 | Score | Cost | Wall | Turns |
|---|---:|---:|---:|---:|
| vanilla | 0.905 | $2.08 | 679 s | 58 |
| zirv, Jev off | 0.867 | $2.37 (+14%) | 659 s | 86 |
| zirv, Jev on | 0.819 | $2.29 (+10%) | 661 s | 87 |

## Findings to build on

1. **Short "XL" tasks tie.** t16–t22 take about 1 min and roughly 10 turns on Sonnet 5. All three conditions make 8–9 API calls and end near 41k context. zirv's first-turn prompt is now about 1k tokens *smaller* than vanilla + superpowers (26.4k vs 27.4k). A supervisor cannot find 20% on these tasks without doing less work.
2. **Wall time is model time.** About 88% of wall time is API time. zirv's own overhead is now around 280 ms per `ctx exec` (was 2,270 ms) and around 24 ms per hook call (was 39 ms).
3. **zirv's quality lead costs turns.** zirv agents write many more tests (at t24 step 4: 45–52 visible tests vs 30). That is where the judge's +7% comes from, and also the extra turns and cost.
4. **Scope creep.** At t24 step 4, both zirv agents "fixed" a pagination quirk the prompt said to keep, and failed the step. Vanilla left it alone and passed. zirv's own standard forbids drive-by fixes, so worker prompt discipline is a real lever that also matters outside the benchmark.
5. **What zirv adds to the context.** No memory bank or vault is injected. Every zirv run composes only `default + adapter + skill pointer`. The one behavioural difference is the intake hook's `INTAKE_DISCIPLINE_TEXT` (hook.rs ~2324), added to each "substantial" first prompt: "Plan ordered, verifiable steps before editing. Write or extend tests first for behaviour changes. … Run the full test suite before declaring done." This is the likely driver of finding 3's extra tests and turns, and a direct lever for the cost/quality trade-off. In r4 the agents almost never ran the suggested `zirv skill load` (0 of 20 runs).
6. **t16 edge case.** t16's two hidden tests that every condition fails use a stored `"Food"` tag (un-normalized). The prompt says matching is case-insensitive, so this is a fair edge case, not a harness bug.
7. **Jev latency.** Intake takes about 220 ms. Jev never sees permission requests (metadata-only contract).

## What this branch ships (all rounds)

| Commit | Change | Issue |
|---|---|---|
| fc59babb | Code edits never route below the standard seat; workflows start only for substantial work; zirv's skill plugin loads only for orchestrator seats; stop-hook advisory cached | intent a93fc040 |
| f4df7785 | Browser probe no longer launches Chrome (workflow start 16.5 s → 0.9 s); no pre-created artifacts; headless PreToolUse allow for operator-allowed commands | – |
| 1e1add75 | Headless missing-tests Stop gate; Stop-hook block decisions actually block | – |
| 5ca6b180 | One PreToolUse hook with the safety check in-process; 50 ms signal connect; lazy tokio; SubagentStop gate; compact worker standard (3,751 → 2,280 B, orchestrators unchanged); same-error rot weight on; `ctx exec --resume`; exit polled every 20 ms (post-exit wait 2,010 → 35 ms); exec notices on stderr | #769 #770 #771 #774 #772 (worker half) #763 #778 |
| 6414a21c | Review fixes: migration keeps operator hooks; SubagentStop is keyed per agent_id and scans the subagent's own transcript; tick cadence | – |
| 3a30825e | A running dash or wrap honours a changed `fallback.auto_orchestrator_rollover` | #780 |
| 86cffc31 | Incremental transcript usage cache (`ctx usage` 49 s → 0.37 s, byte-identical output); `ctx loop` notices on stderr | #779 |
| 474052e7 | Headless in-place compaction is no longer killed mid-flight; claude-sonnet-5 window is 1M, and zirv learns observed windows | – |
| 9c046123 | Review fixes: rollover switch checked on cadence only; usage cache checks mtime and prunes; compaction waits on the hard bound only; learned window bounded to 8,192–10,000,000 and written atomically | – |
| bench commits | `zirv-nojev` condition, `--stagger-s`, `compare.py`, t16–t24 tasks, chain kind, blind opus quality judge (+ retry), usage-limit pause/retry, chain cost deltas and session-switch following | – |

## Round 5 results (r5, r5b, r5b-t24)

- **Effort flips broke the prompt cache on resumed sessions.** `[headless.effort]` re-classified every `--resume` launch, so effort flipped between turns and each flip re-wrote the whole conversation cache (t23 Jev off: 164k cache writes vs 49k). Fixed in 2e943093: effort is decided at a session's first launch and replayed; in-place compaction launches now get the levers too.
- **Why Jev on was not better than Jev off.** The conditions differ by more than Jev: only `zirv-jev-full` runs the harness intake, which routes every XL task to an orchestrator seat and starts a workflow, and the deterministic intake makes the same call, so that part is not Jev. Jev's own cost was the approve gate: 240 calls in r5 (84 uncached at ~0.7 s), 23 escalations and 0 effects, because under `dontAsk` the hook emits nothing for allow or ask. Fixed in d41e9976 (no call under `dontAsk`) and 59f3e608 (no call for read-only local commands). About 75% of a Jev call is TCP+TLS setup from a fresh hook process (~570 ms of three ~190 ms round trips); Jev's own inference is ~100–150 ms.
- **Scope creep is not zirv-specific.** At t24 step 4, Jev on scored 0.0 twice, vanilla 0.0 and 0.5, Jev off 1.0 and 0.5. Jev off's winning report left the pagination bug alone and asked.
- **t24 steps 9 and 14 look like task defects.** Every condition loses the same hidden test in every run. Step 14's failing test calls `cashflow(..., to_usd=True)`, a keyword the step-14 prompt never names. The loss is equal across conditions, so it compresses scores without biasing them.

## Round 6 results (r6, r6-t24)

- **The scope guard works where it matters.** At t24 step 4 (the scope-creep trap) the Stop backstop fired in all four zirv runs, and all four then scored 1.0; vanilla scored 0 in both reps. One block was a false positive on a hypothetical ("Fixing it would change ... too"), tightened in round 7. The first-edit checkpoint rarely fires, because every condition edits mostly through Bash (`python - <<EOF`, `sed -i`), not Edit/Write; round 7 adds a shell-change trigger.
- **Where the time goes on the XL tasks.** zirv is +7–11% slower on t16–t22. On t18 the missing-tests Stop gate adds a blocked turn (~10 s) that vanilla never spends. On t16 zirv has missed the same spec details since r3b ("sorted", "no spaces after the commas"). The harness intake costs only ~0.5 s now.
- **A usage limit contaminated one run.** At the 16:40 session limit, `zirv ctx exec` handed t17 Jev on r1 to codex, which finished it; run.py then crashed on codex's prose. Since a1617d4b, zirv conditions run with `ZIRV_CTX_FALLBACK=false`, a zirv run that parks on a limit is killed and retried like vanilla's, and `parse_last_json` keeps JSON objects only. No earlier round was affected.
- **The cold cache after a pause is an artifact.** t18 Jev off r1 wrote 28.8k cache tokens on its first request after the one-hour pause, because the 5m-TTL prefix had expired while vanilla's 1h prefix survived. Back-to-back zirv runs read 20.4k of cached prefix.

## Round 7 results (r7, r7-t23, r7-t24b)

- **Jev off's XL time is one intake timeout.** Without a Jev credential, `proxy::decide` falls through to `try_helper` (the handoff distiller model), which runs into its ~15 s timeout and then uses the deterministic baseline anyway (`decider: deterministic`, `elapsed_ms` ≈ 15,200). Per XL run, wall minus Claude's own `duration_ms` is 18.3 s for Jev off, 4.0 s for Jev on (Jev intake 216 ms) and 2.2 s for vanilla. Jev off's Claude time is −7% vs vanilla; without the helper attempt its wall would be about −3%. Any operator running headless without a Jev credential pays this on every launch. Fix candidate: skip the helper model for headless intake (headless already forces the single seat, so the helper cannot change the routing that matters), or bound it far below 15 s.
- **The XL hidden-test gap is a grading artifact.** All of it (99 vs 88/89) is t14: both zirv conditions scored 0 in both reps because `grade.py` zeroes any change to `tests/test_rules.py`. The rule exists to stop an agent editing the known-failing `test_regex_rule_case_insensitive` into passing. zirv's agents left that test untouched and appended a new regression test class (6 added lines, 0 removed). Re-graded with an intent-preserving check (fail only if existing lines in the file changed), all four zirv runs pass 20/20, and the XL test score is a tie (98.7 vanilla, 98.1 Jev off, 98.6 Jev on). The appended test duplicates the existing one. That is a real side effect of the round-7 "tests owed at the first edit" line, which does not ask whether a test already covers the change.
- **Chain wall time measured the stagger, not the agent.** `run.py` started each chain step's clock before `wait_for_launch_slot()`, so with 3 parallel chains and `--stagger-s 30` every step waited up to ~60 s inside its own wall. Every condition lands at ~13 min (t23) and ~33 min (t24b). Fixed after the run: the slot wait is kept out of the step wall, and a chain's wall is the sum of its step walls. The table above uses Claude's per-step `duration_ms` instead (read from each run's `stdout_step_*.json`, not archived).
- **t24b vs r6's t24.** Vanilla got cheaper once steps 9 and 14 stated what their hidden tests check ($3.87 vs $4.77), so zirv's cost lead on the long haul shrank from −33% to −16% (Jev off). The +9% judge lead held.
- **Sizing.** The full grid (78 runs, $51.6 of agent spend plus opus judging) used about 73% of one 5h window (2% → 75% over 3.1 h, ~24%/h at `--parallel 2–3`), inside the 55–84% predicted from the 2026-09-25 dollar calibration.

## Round 5 (implementation notes)

Operator decision: compaction should fire when the context actually rots, not at a token count far below the model's window. Let the session gather context until it starts to rot.

- **Real window (474052e7).** The catalogue now gives `sonnet`/`claude-sonnet-5` a 1,000,000-token window, as Claude Code itself reports. `model_window.rs` records `modelUsage.<model>.contextWindow` from each `-p` JSON result in `~/.zirv/model-windows.json`, and the claude adapter prefers that over the catalogue. The operator's `score.model_context_tokens` pin still wins over both. With a 1M window the token gates sit at 500k and 800k, so in t24 (peak ~209k) only real degradation signals can trigger zirv.
- **What drove t24's compactions.** Both zirv transcripts were rescored under the old 200k assumption. Each verdict was "compact" with a weighted score of only 18–21 (`compact_at` is 60): the token ceiling alone, 160k = 0.8 × 200k, triggered it. Failing test runs are **not** a false rot signal. All 51 failing-test tool results carried `is_error: false`, because Claude's flag tracks whether the tool ran, not whether the tests passed. No weights were changed.
- **Compaction timeout (474052e7).** `exec::compact_in_place` used to wait on `wrap.inject_timeout_ms` (20 s), a value sized for PTY nudges. It now waits on a `supervise.compact_timeout_ms` hard bound alone (default 10 min, REPO_FORBIDDEN). Transcript growth is not a liveness signal here, because a single compaction turn appends nothing until it completes. That was a review finding, fixed in 9c046123.
- **Checked, no change.** Session identity was a harness bug (the runner resumed the old id; fixed in 28993c15). The restart-chain breaker only counts real restart boots, so one genuine rot event followed by healthy steps can never trip it.
- **Also in round 5:**
  - #779: the usage cache;
  - `ctx loop` notices on stderr;
  - #780: live rollover switch;
  - harness: session-switch following and cost, judge retry.

## Next steps for the next agent

1. **Time is the unmet target.** No condition is 20% faster on any task group; ~88% of wall is model time, so the lever is fewer or shorter turns, not wrapper overhead. First remove the 15 s helper intake timeout for headless Jev-off launches (Round 7 results).
2. **Make the tests-owed line ask whether a test already covers the change** before requesting a new one (the t14 duplicate test).
3. **Decide the t14 grader rule.** The operator should choose whether to switch `t14_bugsweep/grade.py` to the intent-preserving check. It changes the published XL test score, so it should not be switched silently.
4. **Before merge:**
   - the full four checks (failure-name diff against main);
   - CI on Linux (covers the cfg(unix) wrap tests);
   - the Docker AI-feature matrix for harness-facing changes;
   - README reference rows are updated per commit.

## How to run

```sh
# Bench root: a scratch dir with tasks/ (copy of docs/.../tasks, keep it in
# sync: t24 was missing once), template/ (git init'ed copy of template/) and
# the superpowers plugin dir. --zirv-dir puts the zirv under test first on PATH,
# so hooks run that binary too.
python -u run.py --bench-root <root> --runs-subdir runs-r5 \
  --tasks t24_long_haul,t23_afternoon --conds vanilla,zirv-nojev,zirv-jev-full \
  --reps 2 --parallel 3 --timeout-min 300 --model sonnet --noninteractive \
  --resume --vanilla-plugin-dir <superpowers> --zirv-dir <dir with zirv.exe>
python compare.py --runs <root>/runs-r5 --out report-r5.md
```

Gotchas:

- **Shared subscription.** Benchmark runs share the operator's subscription with the orchestrator session. A 30-run round can exhaust the 5-hour window. The runner pauses and retries on "hit your … limit".
- **Launch through WMI.** Use `Invoke-CimMethod Win32_Process Create`. A `Start-Process` child lives in the dash pane's process tree and dies on a rollover.
- **No `--setting-sources project,local` for zirv conditions,** because it strips zirv's hooks.
- **Chain costs are deltas.** `claude --resume` reports a cumulative `total_cost_usd`, so the runner uses deltas, and after a supervisor session switch it uses the new total plus a transcript-priced estimate.
- **Small tasks can't show the target.** Iterate on the subset (t16, t17, t18, t22, plus t23). The XL tasks cannot show a 20% gap; long sessions can.
