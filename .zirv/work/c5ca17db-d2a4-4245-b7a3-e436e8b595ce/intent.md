# Intent

## Problem

Issue #716 added declarative harness workspaces, but setup commands repeat on
every launch and manifest skill defaults are deliberately outside the launch
path. Issue #717 needs durable success-only progress per setup command. Issue
#719 needs an explicit, bounded bootstrap pass for a worker goal before its
main task, without allowing a bootstrap to recurse into delegation or to
silently accept an incomplete worker result.

## Desired outcome

Workspace setup is idempotent across a worker's restarts and retries by a
stable checkout identity, while changed commands or incomplete steps rerun.
Harness delegation applies an `AgentManifest`'s default skills unless an
explicit workspace supplies its own list. `zirv ctx agent --goal` performs the
workspace gate, then one bounded Fast-tier bootstrap in the same checkout, and
launches the requested worker only after a valid Done report. Existing
no-workspace/no-goal paths keep their behavior.

## Constraints

- Work only in the harness runtime; leave native runtime gating and `rot.rs` unchanged.
- Setup records are append-only JSONL under the state worktree area; malformed
  historic lines are ignored and no unsuccessful command is recorded.
- The bootstrap uses existing supervised exec machinery, has max one restart,
  uses a resolved Fast tier only when configured, and cannot call `agent`.
- Preserve envelopes, cancellation, writer permits, scrubbed setup environment,
  transcript/result semantics, and separate bootstrap accounting.
- Complete the #716 acceptance audit and fix only confirmed gaps; document
  trust/config/CLI changes and bump the crate version above 4.19.0.

## Open questions

None. A stable setup identity will derive from the canonical launch-root path
plus its git common-dir/repository identity so distinct checkouts cannot share
records while the same linked worktree can resume.

## Acceptance criteria

- [ ] Setup skips only matching successful `(index, sha256(command))` records,
      stops at a failed/timed-out step, and safely resumes partial work.
- [ ] `--goal` is harness-only, runs bootstrap before main work, prohibits
      recursive delegation, and refuses a main launch without explicit Done.
- [ ] Fast model routing, timeout/restart limits, cancellation, envelope and
      accounting behavior are covered with fake-adapter tests.
- [ ] #716's config, clone, MCP, skill-default/override and trust-boundary
      behavior has acceptance evidence.
