## Memory
- Key: bench-xl-noise-floor
- Written-by: claude
- Written: 1791270198
- Verified: 1791270198
- Source: explicit

XL benchmark (t13-t22 x2, zirv-nojev, sonnet): the same zirv binary measured 9.9 vs 8.85 API rounds and 62 vs 57 s agent time in two concurrent rounds (2026-10-06). A single 20-run round paired over 10 tasks gave a significant -1.35 round cut for a prompt variant that vanished on replication. Require an independent replicate round before claiming an XL prompt-variant improvement. Fewer rounds did not cut wall time: it tracks how much text the model generates. Record: .zirv/work/68ea4af3-3098-4af1-8cfa-e3b60a691a53/fafo-record.md
