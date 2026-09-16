# `src/commands/ctx/`

`zirv ctx`: supervises Claude Code/Codex (and other adapter) sessions,
rot-scores transcripts, and advises, compacts, or restarts with handoff
before rot ruins them. Most modules carry their own module-level doc comment
that is the actual reference for that file -- start there. Two clusters have
no single place that says who owns what across several files with
confusingly similar names, so they get a dedicated doc instead:

- [`SUPERVISORS.md`](SUPERVISORS.md) -- `wrap.rs` vs `exec.rs` vs
  `run_loop.rs`, the three places a harness process is actually launched and
  supervised.
- [`SESSIONS.md`](SESSIONS.md) -- `sessions.rs` vs `session/` vs `dash/`,
  three different things all named "session".

For which files need extra review scrutiny (PTY/subprocess spawn, argv
construction, permission/safety decisions), see [`SECURITY.md`](../../../SECURITY.md#critical-files).
