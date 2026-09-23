## Memory
- Key: rel424-handover-1-gates
- Written-by: claude
- Written: 1790164839
- Verified: 1790164839
- Source: explicit

PR #748 (release/4.24.0, draft) ships #685 #678 #676 #669 #713 #450 #703; committed before verification finished. Delete rel424-handover-* entries once it merges. Step 1: run the four gates (build, nextest --no-fail-fast, fmt --check, clippy -D warnings) in a worktree with its OWN CARGO_TARGET_DIR -- the workers shared one and ran stale test binaries. Run unsandboxed (sandbox denies socket binds) and name-diff failures against an origin/main run.
