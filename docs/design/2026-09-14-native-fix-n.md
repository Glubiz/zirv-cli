# Chunk N: release-test defects (#637, #638, #639) and D1

**Date:** 2026-09-14 · **Issues:** #637, #638, #639, D1 (not filed) ·
**Worktree:** `native/fix-n`, based on `release/native-harness` @ `59e9b77a`

## Context

The 4.0.0 release-build feature test found three native-runtime defects
(#637, #638, #639) and one tester observation not yet filed as an issue
(D1). This chunk fixes the three filed defects with the minimum diff and one
focused test per behaviour, and investigates D1 without fixing it (its
verdict names a broader gap than this chunk's scope).

## Decisions

### #637: `--budget-tokens` was accepted by clap and silently ignored

`NativeLimits` had no field for a token ceiling at all -- `--max-tool-calls`
on the same request already stopped the loop (`limits.max_tool_calls`), but
`ExecArgs::budget_tokens` was parsed and then never read on the native path.
Added `NativeLimits::max_budget_tokens: Option<u64>` (default `None`,
unbounded, so every existing call site is unaffected), wired from
`ExecArgs::budget_tokens` in `exec.rs`'s `run_native`, and enforced in
`NativeLoop::run_turn` with a new `LimitKind::Tokens`: checked AFTER the
assistant message and its usage are committed (mirroring where
`LimitKind::ToolCalls` is checked), so a budget verdict never erases a real
turn. Mirrors the harness path's `agent::budget_state` semantics: a
one-time `budget_soft_checkpoint` evidence note at
`agent::BUDGET_SOFT_FRACTION` (0.8) of the ceiling, and a hard stop
(`status:"limit_reached"`, `limit:"tokens"`, `exit_code:
EXIT_BUDGET_EXHAUSTED` -- already the mapping for every `LimitReached`
status) at the ceiling itself.

### #638: overflow recovery skipped compaction with `no_boundary`

Root cause: `RETAIN_RECENT_MESSAGES` (`runtime/compaction.rs`) was `4`, but
no in-tree test had ever exercised that untouched default end to end --
both compaction acceptance tests (`a_long_session_compacts_on_token_
pressure_...`, `a_context_overflow_recovers_through_a_compaction_...`)
override it to `2`. At `4`, a short, realistic session (as few as 3
tool-call turns, exactly the `compaction-overflow-recovery.json` fixture's
shape) that overflows the context window on its very first compaction
attempt keeps its ENTIRE history inside the retained tail, so
`checkpoint::boundary` finds nothing before it to compact and
`recover_from_overflow` reports `compaction_skipped: no_boundary` and fails
with the same `context_overflow` a second time instead of distilling --
exactly the CLI repro in #638.

Lowered the constant to `2`, the value both existing acceptance tests
already validate as correct (not an invented number). Added a new
CLI-config-level test,
`headless_native_exec_recovers_a_first_turn_overflow_with_the_cli_defaults`,
which drives `run_session` -- the real `zirv ctx exec --runtime native`
entry point, not the `config_for` test helper -- with no `retain_recent_
messages` override at all, so what is proven is the CLI's actual default
config, closing the gap the issue named ("the CLI defaults differ from the
test's").

### #639: `--resume` had no repository affinity at all

`journal::SessionIdentity` carried no repository, so nothing could refuse
or even explain a cross-repository resume; `<short>.seat.json` (which IS
written for every headless native run) also carried none. Chose the
journal's `native_sessions` row as the authoritative record over
`seat.json`: it is written exactly once, at true session start, and never
rewritten, whereas `seat.json` is unconditionally overwritten on every
run (fresh or resumed) with the CURRENT request's data -- recording there
would need careful read-before-overwrite ordering the journal row does not.

Added a `repo` column (`repo_root TEXT NOT NULL DEFAULT ''`) to the
`native_sessions` genesis table -- schema is still version 1 and unreleased
(`JOURNAL_SCHEMA_VERSION`), so this is a genesis-table change, not an
`ALTER TABLE` migration; no shipped journal predates it. `run_session`'s
resume branch reads the recorded `repo` **before** calling `resume_journal`
(which mutates: it reconciles outcome-unknown executions and advances the
generation), so a refused resume is a pure refusal, not a resume that
partly happened and then got refused. `--resume` from a different
repository is refused with an error naming the recorded origin; resume from
the same repository, and every path that only ever starts a fresh session
(`spawn_interactive`, the persistent-runtime service in
`session/native.rs`), are unaffected. Documented in README's native
`--resume` paragraph.

### D1: finished native sessions invisible to `explain-status`/`ask`/`session.list`

Two separate findings, of different severity:

1. **The FINISHED-session half matches harness -- not a defect.**
   `sessions::resolve_prefix` (used by both `explain-status` and `ask`) and
   the API server's `RegistrySource::sessions()` (used by `session.list`)
   both read exclusively from `sessions::list`/`list_with_retention`, which
   globs `<state>/sessions/*.json` (`sessions::Record`, a DIFFERENT file
   from `<short>.seat.json` or the journal). `list_with_retention` deletes
   a record's file the moment its process is no longer alive AND it has no
   `in_flight` marker (the only grace window is for a mid-effect crash) --
   this is generic over `Record`/`Liveness` and has nothing to do with
   `runtime`/`agent`. A harness session that registered a `Record` (every
   `zirv ctx exec --runtime harness` does, `exec.rs:1721`) disappears from
   these tools the same way, on the very next `list()` call after it exits
   cleanly.
2. **A plain headless `zirv ctx exec --runtime native` run was invisible
   even while LIVE -- a real, separate gap.** `runtime::native::run_session`
   (the entry point `zirv ctx exec --runtime native` calls) never called
   `sessions::SessionGuard::register`/`Record::new` at all -- unlike the
   harness path above, and unlike dashboard-hosted native panes
   (`dash/pane.rs:1499,1531`) and the persistent-runtime service
   (`session/native.rs:618-628`), which both DO register. So no `<state>/
   sessions/<short>.json` ever existed for a plain native exec, live or
   finished, and `explain-status`/`ask`/`session.list` saw nothing
   regardless of liveness -- not a sweep-timing issue, a registration gap
   specific to the plain headless CLI entry point.

Confirmed empirically (scratch state dir, `--provider fixture:...
helper-answer.json`): after the run, `<short>.seat.json` and `<short>.
conversation` exist, no `<short>.json`, and `zirv ctx explain-status
<short>` answers "no sessions are registered".

**Follow-up (same day, same worktree): filed as #645 and fixed.** Finding
2 above was reported but explicitly left unfixed in this chunk's original
scope; the coordinator filed it as issue #645 and asked for it to be
closed in this same worktree/branch. `run_session` now registers a
`sessions::SessionGuard` right after the seat/journal identity is
established (fresh or resumed), reusing the exact record shape
`session::native::NativeSessions::register` already builds (`Verb::Exec`,
`.unreachable()`), scoped to `Accounting::Seat` (excludes a delegated
`native_worker` run, which already owns its own visibility/settlement),
with `stamp_in_flight`/`clear_in_flight` around `run_to_completion`
mirroring `exec.rs`'s own crash-witness/retention semantics. See
`runtime::native::tests::a_live_headless_native_run_appears_in_the_registry_and_disappears_after`.

## What is verified, and by what

- `runtime::native::tests::the_token_budget_ceiling_stops_the_loop_and_names_itself`,
  `runtime::native::tests::the_token_budget_soft_checkpoint_notes_once_before_the_hard_stop` -- #637.
- `runtime::native::tests::headless_native_exec_recovers_a_first_turn_overflow_with_the_cli_defaults`
  (CLI-config level), `runtime::native::tests::a_context_overflow_recovers_through_a_compaction_without_repeating_an_effect`
  (fixture level, pre-existing, still passes) -- #638.
- `runtime::native::tests::resume_from_a_different_repository_is_refused_and_names_the_recorded_origin`,
  `runtime::native::tests::resume_from_the_same_repository_is_unaffected` -- #639.
- D1 (finding 1, finished-session parity): code inspection
  (`sessions::resolve_prefix`, `list_with_retention`,
  `api::server::RegistrySource`/`facts_from_record`) plus one empirical
  scratch-state repro with the debug binary.
- D1 (finding 2, live-invisibility) / #645:
  `runtime::native::tests::a_live_headless_native_run_appears_in_the_registry_and_disappears_after`.

## What is NOT verified / NOT done

- D1's live-native-invisibility gap was reported, not fixed, in this
  chunk's original scope -- explicitly out of scope at the time; closed by
  the same-day follow-up as #645 (see above).
- The two other headless-native `SessionIdentity` call sites this chunk
  touched only to keep the build green (`session/native.rs`'s persistent-
  runtime service, `spawn_interactive`) record `repo` but were not given
  their own cross-repository resume check: neither path resumes an
  EXISTING native journal session from a caller-supplied id the way `zirv
  ctx exec --runtime native --resume` does (the persistent-runtime service
  reattaches from its own registry-recorded `cwd`; `spawn_interactive`
  always mints a fresh journal session), so #639's acceptance criterion
  ("`--resume` … refuses") is fully met by the one path that has a
  `--resume` flag at all.
