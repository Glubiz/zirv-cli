## Memory
- Key: codex-approve-for-me-cli-conflicts
- Written-by: unknown
- Written: 1789986602
- Verified: 1789986602
- Source: explicit

Verified 2026-09-21 on codex-cli 0.155.1: --approve-for-me is a standalone workspace-write/automatic-review preset. Clap rejects it alongside --sandbox or --ask-for-approval (exit 2). Adding --help hides those conflicts: help exits before validation. With only --approve-for-me and null stdin, parsing succeeds and exits 1: stdin is not a terminal. Issue #710 failures appear in local decision logs from September 14; repeated weekly-headroom triggers amplify them on September 21.
