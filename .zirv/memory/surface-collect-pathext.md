## Memory
- Key: surface-collect-pathext
- Written-by: claude
- Written: 1790668230
- Verified: 1790668230
- Source: explicit

Windows executable lookup needs PATHEXT to recognize .cmd and .bat shims; use adapters::program_is_present, not a bare-name PATH walk (#108).
