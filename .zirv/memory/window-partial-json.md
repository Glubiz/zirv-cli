## Memory
- Key: window-partial-json
- Written-by: claude
- Written: 1790668231
- Verified: 1790668231
- Source: explicit

A live transcript read can end mid-JSON row; advancing the consumed offset past it loses that row's usage, so hold back an unparseable newline-less tail (#779).
