# Chunk K: release evidence and acceptance-level tests (#609, #610, #615)

**Date:** 2026-09-14 · **Issues:** #609, #610, #615 · **Roadmap:** #469 ·
**Worktree:** `native/fix-k`, based on `release/native-harness` @ `dac2014d`

## Context

PR #493's cross-harness review found the release's evidence weaker than its
own parity matrix claimed: an installed-binary install proof that actually
used a stub launcher (#609), acceptance coverage composed of component tests
rather than production-boundary tests (#610), and a real binary-size/build-
cost regression with no recorded budget (#615). This chunk closes what could
be closed without touching `runtime/native.rs` core logic, `route.rs`,
spend, or handover/rollover -- those are owned by a concurrent worker on the
same release -- and says plainly what could not be closed that way.

## Decisions

### #609: harness-free install proof, driven by the real registry

The old proof had two independent gaps: the CI job's "no coding harness"
assertion only named `claude`/`codex` (not the other six adapters
`ctx::adapters::ADAPTERS` registers), and the cited coordinator/helper tests
substituted `RecordingLauncher` for the one seam that actually launches a
worker, so a regression in the launch path itself would never have been
caught.

Both are closed by one new test,
`tools::tests::a_real_native_worker_and_every_helper_role_complete_with_every_registered_harness_canaried_and_uninvoked`
(`src/commands/ctx/runtime/tools/mod.rs`), plus a shared helper,
`testenv::canary_path_for_every_registered_harness`
(`src/commands/ctx/mod.rs`):

- The harness list comes from `ADAPTERS` itself, not a copy of it, so a
  ninth adapter needs no test rewritten.
- Each name gets a CANARY executable (a `.cmd` on Windows, a `chmod +x`
  shell script on Unix) that appends its own name to a shared log and exits
  non-zero if the OS ever actually runs it -- direct evidence of
  INVOCATION, which an empty `PATH` alone does not give: an empty `PATH`
  only proves a lookup would have failed, not that nothing tried to look.
- The test drives the REAL production worker entry point
  (`runtime::native::run_session`, the exact function `zirv ctx exec
  --runtime native --provider fixture:` calls) and the REAL helper entry
  point (`helper::run`) to completion, then asserts the canary log never
  came to exist.

Wired into the `Native Install` job (`.github/workflows/ci.yaml`) as
`Verify The Harness-Free Install Proof`, on all three OSes, with the same
non-vacuous `test result: ok. [1-9]...` guard every sibling step in that job
uses. The bash "Assert No Coding Harness Is Installed" step was also widened
from the `claude`/`codex` pair to the full eight names -- best-effort and
can still drift if a ninth adapter forgets to update it, which is exactly
why the Rust test is the one that does not depend on that list being
current: it reads the registry directly and scrubs `PATH` itself.

Left as a documented gap, not worked around: a delegated native worker
(`native_worker::run`, the code path `zirv agent --runtime native` and the
coordinator's own `delegate` tool both reach) always passes
`HeadlessRequest.provider = None` -- there is no seam to fixture-inject a
REAL dispatched worker's transport, by design (`HeadlessRequest::provider`
is deliberately never threaded onto a model-facing `LaunchRequest`, so a
model can never redirect its own worker's provider). Closing that fully
would need a new operator-only override on `AgentArgs`/`native_worker::
Request` mirroring `ExecArgs::provider`, which touches `agent.rs`/
`native_worker.rs` core logic this chunk does not modify.

### #610: acceptance-level tests, at the real production boundary

Five scenarios named in the issue, one at a time:

1. **Coding TUI read/edit/test path -- MET.** Every existing
   `spawn_interactive`/`run_hosted_turns` test in
   `src/commands/ctx/session/native.rs` drove `TestEnvironment`, a stub that
   only records it was asked to run; production's own `ProviderEnvironment`
   had never executed in any test in that file. The new
   `FixtureProviderEnvironment` is `ProviderEnvironment::run` verbatim with
   one substitution (a fixture transport and tool script instead of `None,
   None`), and the new test
   `a_hosted_native_conversation_reads_edits_and_tests_through_the_real_turn_runner`
   drives a real search/read/edit/test-run sequence through
   `run_hosted_turns` -- the exact call the native pane/session driver makes
   for a live conversation -- and asserts both the transcript
   (`session.history`) and the durable journal events independently.
2. **Frontend run-inspect-capture-review -- UNMET.** Genuinely needs a
   configured browser capability; `docs/design/2026-09-14-native-release-
   evidence.md` §3 already states this is unverified here ("Browser-backed
   frontend rendering is configuration-gated and unverified"). Faking a
   stub browser well enough to be honest evidence, rather than a
   self-fulfilling one, was judged out of scope for this chunk's time
   budget -- reported as UNMET rather than substituted with a stub.
3. **Multi-file workflow through review and verification -- MET.** New test
   `a_real_multi_file_change_advances_only_once_test_review_and_verify_each_genuinely_pass`
   (`src/commands/workflow/engine.rs`) drives a real two-file git diff
   through Test (`--run-checks`, a real passing check), Review (a real
   unresolved finding blocking, then a real fresh review-evidence row
   letting it pass -- `verification::change_fingerprint` computed for real,
   not guessed), and Verify (a real failing check refusing, then a real
   passing check clearing it) -- three real gates, one continuous workflow,
   entirely through `run(&args, ...)`/`advance_with_evidence`, the same
   entry points `zirv workflow advance` itself uses.
4. **Writer crash / torn-state recovery -- MET, pre-existing.**
   `runtime::native::tests::a_resume_with_a_reconcile_notice_writes_exactly_one_json_object_to_stdout`
   already does exactly this: it writes a started-but-never-completed
   execution through one journal handle, closes it (scope exit, not a live
   handle passed to the resume call), and resumes through `run_headless` --
   a genuinely fresh `Journal::open` reading the real half-written sqlite
   file back, not the same in-memory object. It was under-cited in
   `native-parity.md`'s `ctx resume` row; that citation is now added. No new
   test was needed.
5. **Per-OS containment backend (Windows) -- MET here.** The existing
   `unavailable_process_isolation_never_falls_back_to_a_plain_spawn` proves
   the broker refuses a HAND-FED `Unavailable` isolation value; it never
   calls `PlatformIsolation::detect()`, so it says nothing about what this
   actual OS reports. The new
   `a_process_action_is_refused_by_this_machines_own_real_platform_isolation_detection`
   calls `detect()` for real and drives a real broker-mediated process
   action through it. On this Windows machine it also pins the exact
   verdict (`Unavailable{platform:"windows",...}`) and the refusal. The
   Unix branch is conservative and not verified here -- it accepts either a
   genuinely available backend (must authorize) or `Unavailable` (must
   refuse), matching the repo's own convention for `#[cfg(unix)]` code this
   machine cannot compile or run (see CLAUDE.md).

### #615: build cost, measured and bounded

`scripts/bench-build-cost.sh` (POSIX) and `scripts/bench-build-cost.ps1`
(Windows, same contract) measure clean debug build time and stripped
release binary size for two git refs via disposable `git worktree`
checkouts, print one parseable line per ref, and fail past a documented
threshold (+25% build time, +15% release size vs base; both overridable per
run for a deliberate, justified exception). `docs/benchmarks/build-cost.md`
records the thresholds, the rationale (calibrated against #615's own
measured +19.7%/+26.5% regression), and this machine's own actual
measurement (main vs `release/native-harness`).

CI wiring is advisory, the same shape `security-scan` already uses:
`continue-on-error: true`, PR-only, `ZIRV_BENCH_SKIP_RELEASE=1` (debug time
only -- a full release-profile LTO build of both refs on every PR was
judged too slow for a per-PR check; see the doc's own §4 for the exact
reasoning). This was a deliberate choice: a blocking gate would need the
release half timed and budgeted on every PR, and the recording in the doc
shows why that is not currently practical as a hard merge gate.

The recorded measurement (main vs `release/native-harness`, this machine,
2026-09-14) independently confirms the regression #615 itself found and
FAILS against both thresholds: +28.6% clean debug build time (threshold
+25%) and +26.1% stripped release binary size (threshold +15%). That is the
tool working correctly, not a defect in it -- this chunk's scope was to
build and record the measurement, not to reduce the regression, which is
separate work. See `docs/benchmarks/build-cost.md` §5 for the full numbers.

## What is verified, and by what

- `tools::tests::a_real_native_worker_and_every_helper_role_complete_with_every_registered_harness_canaried_and_uninvoked`,
  `CI: Verify The Harness-Free Install Proof` -- #609.
- `session::native::tests::a_hosted_native_conversation_reads_edits_and_tests_through_the_real_turn_runner` -- #610 scenario 1.
- `engine::tests::a_real_multi_file_change_advances_only_once_test_review_and_verify_each_genuinely_pass` -- #610 scenario 3.
- `runtime::native::tests::a_resume_with_a_reconcile_notice_writes_exactly_one_json_object_to_stdout` -- #610 scenario 4 (pre-existing).
- `enforcement::tests::a_process_action_is_refused_by_this_machines_own_real_platform_isolation_detection` -- #610 scenario 5.
- `scripts/bench-build-cost.sh`/`.ps1`, `docs/benchmarks/build-cost.md` -- #615.

## What is NOT verified

- #610 scenario 2 (frontend run-inspect-capture-review): no configured
  browser capability in this environment; reported UNMET rather than faked.
- A REAL delegated native worker's transport still cannot be fixture-driven
  (see #609's "left as a documented gap" above) -- the coordinator's own
  worker-launch seam is still `RecordingLauncher` for that reason, unchanged
  by this chunk.
- The build-cost script's release-size measurement is not run on every PR
  by design (see #615 above); a batch that should re-check it has to run
  the script by hand.
