## Memory
- Key: rel424-handover-4-gaps
- Written-by: claude
- Written: 1790164839
- Verified: 1790164839
- Source: explicit

PR #748 open gaps, not fixed, decide or file issues: capture() and ChromiumRunner::run() pass no --user-data-dir, so a real headless render inside a sandbox likely fails like the discovery probe did; the native path never adds the proxy prompt layer to its context pipeline (#703 follow-up); the #610 ignored frontend acceptance test lives only on fix/592-610-native-evidence and should stay ignored.
