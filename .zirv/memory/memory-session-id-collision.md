## Memory
- Key: memory-session-id-collision
- Written-by: claude
- Written: 1790668315
- Verified: 1790668315
- Source: explicit

Lossy path sanitization can map distinct session ids to one memory directory, so retiring one session could delete another's entries; suffix names with a hash of the raw id (legacy unsuffixed dirs stay readable).
