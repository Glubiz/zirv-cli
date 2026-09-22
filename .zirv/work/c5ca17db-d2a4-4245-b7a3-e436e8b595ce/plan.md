# Plan

1. Map workspace setup, worktree state helpers, agent launch/report completion,
   tier resolution, delegation ledger seams, and the `--manifest`/AgentRegistry
   integration; finish #716 audit.
2. Add durable per-step setup progress and focused inline tests for resume,
   command changes, corruption, failures, and timeouts.
3. Complete #716 by applying AgentManifest skill defaults through `--manifest`
   and proving explicit workspace skills override them; update README's
   currently incorrect limitation.
4. Add harness-only `--goal` bootstrap at the shared pre-dispatch seam, with
   fake-adapter tests for ordering, routing, failure, timeout, restart,
   non-recursion, and accounting.
5. Update configuration/CLI/trust documentation and version; run focused tests,
   then the repository-required full checks after the coordinator confirms the
   baseline is complete.
