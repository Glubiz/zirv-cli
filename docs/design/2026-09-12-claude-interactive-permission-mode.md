# Interactive Claude permission-mode override

**Date:** 2026-09-12 · **Issue:** #504

## Context

`ClaudeAdapter::default_sandbox_args` hardcoded `--permission-mode default`
for every interactive launch. That CLI flag outranks `permissions.defaultMode`
in the operator's own `~/.claude/settings.json`, so an operator running
several native subagents delegating into worktrees had no config knob to
quiet the resulting prompt volume (every worker Edit/Write outside `./**`,
every unlisted compound command) short of typing `/permissions
bypassPermissions` into the live session each time.

## Decision

- `[chat] claude_permission_mode` (`"default"` | `"acceptEdits"` |
  `"bypassPermissions"`, operator-only, `REPO_FORBIDDEN`, env override
  `ZIRV_CTX_CHAT_CLAUDE_PERMISSION_MODE`) picks the INTERACTIVE launch's
  `--permission-mode`. `None` (unset) reproduces `"default"` exactly, so
  behavior is unchanged unless an operator opts in. Headless stays hardcoded
  `dontAsk` regardless.
- Delivered as an adapter-instance field (`ClaudeAdapter::
  claude_permission_mode`), attached post-construction by a new
  `AgentAdapter::apply_chat_config` trait method (default no-op), mirroring
  the existing `apply_endpoint`/`endpoint` pattern (issue #395) rather than
  adding a `ChatConfig` parameter to `default_sandbox_args` itself -- the
  same outcome with a far smaller diff across the ~27 existing call sites of
  that method.
- The interactive `Edit(./**)`/`Read(./**)` allow-list scope is separately
  widened (interactive only) to cover Claude Code's own agent-worktree
  convention (`.claude/worktrees/**`, a marker `safety::is_agent_worktree_
  root` already recognizes) and every `--add-dir` grant this same launch
  passes, derived from the one bounded, already-computed worktree-discovery
  set (`current_worktree_grant_paths`) rather than a second, independently
  computed one. `bypassPermissions` changes only the mode flag: the
  `--allowedTools`/`--disallowedTools` lists are built identically either
  way, never suppressed or widened further for it.

## Verified

- `cargo build`, targeted `cargo nextest run adapters::claude:: config::`
  (420 tests) and a full `cargo nextest run --no-fail-fast` pass; `cargo fmt
  -- --check` and `cargo clippy --all-targets -- -D warnings` are clean.
- New tests: config parse/validate over the fixed set, env override,
  REPO_FORBIDDEN rejection (`config.rs`); an adapter test pinning the
  invariant that the configured mode reaches the interactive argv while
  headless keeps `dontAsk` regardless; a `bypassPermissions`
  allow/deny-list-untouched test; an interactive worktree/`--add-dir`
  widening test; a pure unit test for the `--add-dir` -> `Edit`/`Read`
  derivation.

## Deferred

- Honoring `permissions.defaultMode` directly from Claude Code's own
  `~/.claude/settings.json` was considered and rejected in favor of an
  explicit zirv-owned key: reading a second config format at launch time
  adds a parse surface for no real gain over one operator-config key.
