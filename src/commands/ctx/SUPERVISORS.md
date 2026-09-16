# Supervisors: `wrap.rs` / `exec.rs` / `run_loop.rs`

All three run one adapter-launched harness process and react to its rot
score. `wrap.rs` and `exec.rs` each carry their own module doc comment with
far more detail than this file repeats; `run_loop.rs` has none, which is the
gap this file exists to cover.

## `wrap.rs` -- interactive supervisor (`zirv ctx wrap`)

- **Scope:** an attended session behind a real pty, bytes passed through
  byte for byte.
- **Owns:** the pty lifecycle, injecting `/compact`/a restart or one
  advisory mail line only at a verified-idle turn boundary, and degrading to
  pure passthrough on any supervision failure.
- **Does not own:** headless execution (`exec.rs`) or repeated-cycle looping
  (`run_loop.rs`); never runs without a real terminal attached.

## `exec.rs` -- headless single-run supervisor (`zirv ctx exec`)

- **Scope:** one unattended run from launch to exit, no pty.
- **Owns:** restarting the run on rot with a distilled handoff, and
  composing the launch's mail/prompt exactly once (a nudge relaunch is the
  one deliberate recompute).
- **Does not own:** interactive passthrough (`wrap.rs`) or the outer cycle
  that relaunches it repeatedly (`run_loop.rs`, which calls `exec::
  action_for_verdict`/`compact_in_place`/`headless_resume_launch` directly
  rather than reimplementing them).

## `run_loop.rs` -- repeated-cycle supervisor (`zirv ctx loop`)

- **Scope:** the same prompt run again and again, re-listing mail every
  cycle.
- **Owns:** the cycle boundary itself and the objective/no-progress exit
  codes (`EXIT_OBJECTIVE_BLOCKED`, `EXIT_OBJECTIVE_NO_PROGRESS`).
- **Does not own:** the compact/restart mechanics it calls into `exec.rs`
  for, or any pty.
