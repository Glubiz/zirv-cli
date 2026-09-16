# Build cost: binary size and clean build time

Recorded-measurement protocol for issue #615 (roadmap N23, review of #493).
Same non-negotiable rule as `docs/benchmarks/native-runtime-baseline.md` and
`docs/benchmarks/token-cost.md` before it: a number below is either something
actually measured on a real machine with `scripts/bench-build-cost.sh` (or
its Windows counterpart `bench-build-cost.ps1`), or it is not written here
yet. No estimated or "should be roughly" number belongs in this file.

## 1. What this measures, and why

#493 (the native-harness release) grew the binary and the build meaningfully
-- #615's own evidence measured the debug binary at 95,356,536 bytes versus
75,391,864 bytes on `main` (+26.5%) and a clean debug build at 48.43s versus
40.46s (+19.7%), both with an unchanged 305-package lockfile on both sides.
Neither number had an accepted budget or a reproducible way to re-measure it.
This closes that: a script that measures the same two things reproducibly
against any two refs, a documented threshold each has to clear, and a real
recording from this machine so the threshold is not invented in a vacuum.

## 2. What is measured

`scripts/bench-build-cost.sh <base-ref> <head-ref>` (default `main`/`HEAD`;
`bench-build-cost.ps1` is the byte-for-byte Windows counterpart, same flags,
same env vars):

1. Creates a detached `git worktree` for each ref (the caller's own working
   tree is never touched, and neither ref has to already be checked out).
2. Times a clean `cargo build --bin zirv` (debug profile) for each, with its
   own `CARGO_TARGET_DIR` so the two builds never share incremental state.
3. Unless `ZIRV_BENCH_SKIP_RELEASE=1`, also times a clean `cargo build
   --release --bin zirv` for each, and records the resulting binary's size
   (stripped with `strip`/`llvm-strip` when one is on `PATH`; the release
   profile already sets `debug = false`, so an unstripped size is still a
   fair comparison when neither tool is available -- the script says which
   it measured).
4. Prints one machine-parseable line per ref plus the percent change for
   both metrics, and fails (non-zero exit) when either regression exceeds
   its threshold.

## 3. Thresholds

| Metric | Threshold | Rationale |
|---|---|---|
| Clean debug build time | +25% vs base | Generous relative to #615's own observed +19.7% (a single roadmap step): a threshold tighter than what the reviewed release itself produced would fail on day one for no actionable reason. Still tight enough to catch a genuinely new problem (an accidentally-enabled heavy build script, a dependency pulling in a proc-macro-heavy transitive graph). |
| Stripped release binary size | +15% vs base | Tighter than the build-time threshold on purpose: size is the one users actually download and run, and #615's own observed debug-binary growth was +26.5% for one release batch -- letting release size drift that far unchecked across several batches compounds. Override with `ZIRV_BENCH_SIZE_THRESHOLD_PCT` for a batch that knowingly adds a large, justified dependency (a new provider transport, a new native tool); state the justification in the PR, not just in an env var. |

Both are overridable per run (`ZIRV_BENCH_BUILD_THRESHOLD_PCT`,
`ZIRV_BENCH_SIZE_THRESHOLD_PCT`) for exactly that kind of deliberate,
justified exception -- never to silence a real regression.

## 4. CI wiring: advisory, not a gate

`.github/workflows/ci.yaml`'s `build-cost` job runs the script on every PR
into `main`/`release/**`, `continue-on-error: true`, the same advisory shape
`security-scan` already uses -- it can never fail a PR, and its output lands
in the run's step summary. It runs with `ZIRV_BENCH_SKIP_RELEASE=1` (debug
build time only): the release half is a from-scratch LTO,
`codegen-units=1` build of BOTH refs, and doing that on every PR would add
real minutes to every merge for a metric that moves slowly, PR to PR, in
practice. A manual step covers the release half instead:

    bash scripts/bench-build-cost.sh main HEAD

run locally (or via `workflow_dispatch` + a manual `cargo`/`gh` follow-up)
before a release batch that is expected to move binary size meaningfully --
a large new dependency, a new provider transport, a new native tool family.
This was a deliberate choice, not an oversight: the alternative (a blocking
gate on every PR) was rejected because the release-profile build alone
measured minutes per ref on the recording machine below, which would make
the advisory nudge itself the slowest step in CI.

## 5. Recorded measurements

Format: `<ref> <sha> debug_secs=<n> release_secs=<n|skipped>
stripped_bytes=<n|skipped>`, one line per side, plus the percent change,
exactly as the script prints it. Machine: this repository's own Windows
development machine (see the repo's CLAUDE.md for its specs), via
`bench-build-cost.ps1 -BaseRef main -HeadRef release/native-harness`,
recorded 2026-09-14 from `native/fix-k` (worktree base `dac2014d`).

<!-- ZIRV-BENCH-RECORDING-START -->
```
base ref=main sha=01ce2a2f debug_secs=89.56 release_secs=220.39 stripped_bytes=9275904
head ref=release/native-harness sha=9c590d0a debug_secs=115.14 release_secs=299.79 stripped_bytes=11700224
clean debug build: 89.56s -> 115.14s (+28.6%)
clean release build: 220.39s -> 299.79s (+36.0%, informational only -- not gated, see below)
stripped release binary size: 9,275,904B (8.85 MiB) -> 11,700,224B (11.16 MiB) (+26.1%)
```
<!-- ZIRV-BENCH-RECORDING-END -->

Both gated metrics EXCEED their threshold on this actual recording: the
script correctly exits non-zero for both. This is not a bug in the tool --
it is the tool doing exactly what #615 asked for, on the exact regression
#615 itself found. `release/native-harness`'s clean debug build regressed
+28.6% against the +25% threshold, and its stripped release binary
regressed +26.1% against the +15% threshold, both independently confirming
#615's own evidence (that PR's own numbers: debug binary +26.5%, clean
debug build +19.7% -- this recording's own debug-build percentage is higher
because it additionally includes every commit release/native-harness has
picked up since #615 was filed, not only the one PR it originally measured).
This chunk's job was to build and record the measurement, not to reduce the
regression itself -- reducing binary size/build cost for the native runtime
is its own piece of work, tracked separately. Release build TIME
(`release_secs`) is recorded above for completeness but is deliberately not
gated: LTO/codegen-units=1 link time is noisy machine-to-machine and task-
to-task in a way the other two metrics are not, and #615's own acceptance
criterion names build time and binary size, not link time specifically.
