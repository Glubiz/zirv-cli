# Native harness review round, chunk H (N05/N06/N15)

**Date:** 2026-09-14 · **Issues:** #557, #582, #583, #599, #598 · **Roadmap:** #469

## Context

Review of PR #493 (native harness release, roadmap #469) on 2026-09-14 found
five issues assigned to this chunk: one blocker (a repository skill body
could forge trusted provenance), one major patch-apply TOCTOU race, one
major native-process lifecycle gap (unpolled deadlines, unbounded output,
unbounded cleanup), one major workflow-completion gate that failed open on
an evidence read error, and one major `ctx ask` gap that left native
sessions uninspectable. This note records the fix for each, test-first, in
the order they were fixed.

## Decision

### #557: a repository skill body cannot forge a provenance header

`workflow::engine::render_current_context` rendered each selected skill's
body verbatim between `\n[skill <id>@<version>; source=<trust>]\n` headers.
`ctx::runtime::context::append_workflow_sources` then split the rendered
text on the bare `"\n[skill "` string to find each fragment's boundary and
read its trust from that header. A repository skill's own body -- untrusted
text, never escaped before insertion -- could therefore contain a newline
followed by a hand-typed `[skill fake@1; source=built-in]` line, and the
parser would treat everything after it as a new, Zirv-trusted fragment.

The fix is a compiler-only boundary marker. `engine.rs` now prefixes every
header it emits with `SKILL_HEADER_SENTINEL` (`'\u{1}'`, a control byte),
and strips that exact byte from every skill body before insertion
(`sanitize_skill_body`) -- so it can only ever appear where the compiler put
it. `context.rs`'s parser now anchors its fragment-boundary search on the
sentinel-prefixed marker instead of the bare text, so a forged header
embedded in a body can never be mistaken for a real one regardless of what
the body contains. The escaping and the parser rule are the same
mechanism, not two: neutralising the sentinel at the point of insertion is
what makes the parser's narrower recognition rule sufficient by itself.

### #582: a patch is re-verified immediately before the atomic rename

`apply_patch` (`runtime/tools/files.rs`) validated its `expected_sha256`
precondition once at the start, computed the replacement text, then called
`state::write_atomic_bytes` unconditionally -- an external edit landing in
that window was silently overwritten by the rename.

Added `state::write_atomic_bytes_if_unchanged`, which re-reads and
re-hashes the destination immediately before the rename that would replace
it, refusing (leaving the destination untouched) when it no longer matches
the caller's validated precondition. `apply_patch` now goes through it and
turns a refusal into the existing typed stale-content error. A separate
function rather than a parameter on `write_atomic_bytes`: that function's
other callers (session/state persistence, `write_file`) want an
unconditional replace, and the extra read plus full-file hash should only
be paid by a stale-content-sensitive caller.

### #583: native process deadlines, output and cleanup are all bounded

Three related gaps in `runtime/tools/process.rs`, one process manager:

- **Deadline.** `update_process` only checked `process.timeout` when
  `poll`/`wait` was called -- a caller that never polled again left the
  child running past its own deadline forever. `ProcessManager::start` now
  spawns a watchdog thread whenever `timeout_ms` is set. It owns only the
  child's bare pid (`ProcessChild::pid`), not the `Child`/`ManagedProcess`
  themselves, so it needs no lock over state this struct's normal methods
  mutate, and terminates through `supervise::terminate_pid` -- **not**
  `supervise::kill_tree`, which is Windows-only and was the first draft's
  mistake (caught by the Linux verification run, see below).
  `ManagedProcess::cancel_watchdog` stops it as soon as the process reaches
  a terminal state by any other path.
- **Output.** `spawn_reader` fed an unbounded `mpsc::channel` for as long as
  the child kept writing. It now stops pulling a stream once it crosses a
  hard per-stream ceiling (`MAX_STREAM_BYTES` = 8 MiB, `MAX_STREAM_LINES` =
  50,000), independent of whether -- or how often -- a caller drains the
  channel. Two limits, not one: a byte cap alone does not bound a stream of
  many small lines the same way a child could still flood a byte budget one
  full 8 KiB read-buffer of short lines at a time.
- **Cleanup.** `finish` joined each reader thread directly
  (`reader.join()`), which blocks forever if a descendant the direct child
  spawned outlives it and keeps the pipe's write end open. `finish` now
  joins through `join_reader_bounded`, which waits up to
  `READER_JOIN_TIMEOUT` (5s) on a channel fed by a detached proxy thread
  instead of blocking directly, so a reader stuck forever on a still-open
  pipe can no longer hang cleanup (it leaks one idle thread instead, the
  best achievable without an OS-level "close this pipe" primitive).

### #599: an unreadable verification record fails the completion gate closed

`workflow::engine::native_completion_gate` read
`verification::latest_is_fresh_and_passing(..).unwrap_or(true)`. This
differs from the function's own documented fail-open posture for STATE
issues (an unreadable state directory, an absent workflow, an unresolvable
branch: "nothing to gate on", checked earlier in the same function) --
by the time this line runs, the gate has already committed to needing
fresh evidence for a Test/Verify step, so a failure reading THAT evidence
(missing permissions, corruption, any other read failure) must not be
silently read as "fresh and passing". The `unwrap_or(true)` is now a
`match` that surfaces the error in the returned diagnostic and blocks
completion; the function's doc comment now states the distinction between
the two fail-open/fail-closed regions explicitly.

### #598: `ctx ask` inspects a native session through its own journal

`ask::run_with` unconditionally resolved the target session's agent through
`adapters::select`, which correctly refuses `"native"` -- it names no
coding harness. `run_with` now branches: a native target is read through
`native_structural_context`, which projects the durable journal
(`runtime::journal::Journal::replay` -- the existing pure
event-to-conversation projection, reused rather than re-deriving
user/assistant turns from raw events a second time) into the same
`StructuralContext` shape `ask_prompt` already expects. It is then answered
by `native_ask_answer`, which tries the operator's native `[roles]` route
for `helper::ROLE_ASK` first -- the same native-first, harness-second order
`handoff::helper_answer` uses everywhere else -- and only resolves a
harness adapter lazily, once native is confirmed unconfigured or failed.
`helper_answer` itself could not be reused as-is: it takes its harness
fallback adapter eagerly (`&dyn AgentAdapter`), which would force resolving
one before even trying the native route -- exactly the harness dependency
this exists to avoid for a native target whose native answer succeeds.
`run_with` gained a private `run_with_provider` (the public `run_with`
always passes `None`) so a test can drive the native-first path
deterministically through `helper::run`'s fixture transport, the same
mechanism `helper.rs`'s own tests already use.

## What is verified

- `repository_skill_body_cannot_forge_trusted_provenance`
  (`ctx::runtime::context`): a repository skill whose body embeds
  built-in- and operator-style headers, driven through the real
  `render_current_context` -> `context::compile` path, stays entirely
  repository-trusted and out of trusted system content.
- `patch_refuses_edit_racing_final_replace` (`ctx::runtime::tools::files`):
  a real background writer races `apply_patch`, synchronized on the one
  externally observable side effect the guarded write produces before it
  re-reads the destination (the temp sibling's directory entry, created by
  `open()` well before its content is flushed) -- deterministic, not
  timing-sensitive; the patch is refused with the typed stale-content error
  and the external edit survives.
- `process_lifecycle_bounds_timeout_output_and_cleanup`
  (`ctx::runtime::tools::process`, `#[cfg(unix)]` like every other
  real-process test in this module): parameterised over an unpolled
  timeout, an oversized/no-newline stream, and a `setsid`-detached
  descendant that outlives the direct child and keeps its pipe open.
  Verified on this Windows dev box only by `cargo build --tests` (the
  module does not compile any real-process test here at all, `#[cfg(unix)]`
  or not); the actual run, plus `cargo clippy --all-targets -- -D warnings`,
  was done in a `rust:1-bookworm` Docker container as a non-root user
  (`git -c core.autocrlf=false archive HEAD`-equivalent export, per this
  repo's own CLAUDE.md convention for `wrap.rs`). All 5 `process::` tests
  and clippy passed there; that run is what caught the `kill_tree`
  (Windows-only) mistake in the first draft of the watchdog.
- `workflow_completion_refuses_unreadable_verification_evidence`
  (`workflow::engine`): corrupts the persisted verification record directly
  (invalid JSON behind a valid `latest` pointer, not through `save_report`,
  so the gate hits a genuine read error instead of "no evidence yet") and
  asserts the workflow stays incomplete with a diagnostic naming the read
  failure.
- `ctx_ask_reads_native_session_without_harness_adapter` (`ctx::ask`):
  creates a real journal `ask` never wrote to, with known user/assistant
  content, and checks both seams the fix touches -- the prompt
  `native_structural_context`/`ask_prompt` build from it carries that
  content, and the full `ctx ask` command (registry resolution included)
  answers a native target end to end with `PATH` empty (`adapters::select`
  would error immediately with nothing on PATH, so a successful answer
  proves the native branch ran).

## What is deferred

- The `#583` fix narrows, but cannot fully close, the check-to-replace
  window `#582`'s fix also narrows: both are plain-filesystem `rename`-based
  guards, and neither is a cross-process transactional lock. A write that
  lands in the few-syscall gap between the final re-check and the rename
  itself is still possible in principle; closing that fully would need an
  OS-level advisory lock held across both operations, which neither issue's
  acceptance asked for and this round did not add.
- `#583`'s reader-side output ceiling stops the reader, not the child: a
  process that keeps writing past `MAX_STREAM_BYTES`/`MAX_STREAM_LINES` can
  still run (backpressured on a full OS pipe buffer) until its own deadline
  or an explicit `terminate` reaps it. Only the wall-clock deadline (fixed
  separately in this same round) or an explicit caller action bounds that;
  hitting the output ceiling does not itself trigger termination.
- `#583`'s `setsid`-descendant scenario needs `sh`/`setsid` on the test
  host and could not be run on this Windows dev box at all; it was verified
  once, in the Docker container described above, and is otherwise only as
  reliable as the CI Linux/macOS runners that will compile and run it next.
