## Memory
- Key: workflow-nextest-docs
- Written-by: claude
- Written: 1790668295
- Verified: 1790668295
- Source: explicit

cargo nextest run skips doctests, while cargo test --doc exits 101 (no library targets found) for a binary-only crate, so the discovered Rust test command chains --doc only when src/lib.rs exists (#495).
