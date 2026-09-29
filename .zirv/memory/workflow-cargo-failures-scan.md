## Memory
- Key: workflow-cargo-failures-scan
- Written-by: claude
- Written: 1790668296
- Verified: 1790668296
- Source: explicit

Cargo prints a failures: section twice per test binary (first with per-test stdout dumps, then a bare indented name list before test result: FAILED); names are read from the nearest failures: line, from the streamed full output because the capped display tail can lose the summary (#215).
