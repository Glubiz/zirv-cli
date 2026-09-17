## Memory
- Key: proxy-intake-baseline-is-text-only
- Written-by: claude
- Written: 1789634710
- Verified: 1789634710
- Source: explicit

Decided 2026-09-17 while shipping the harness proxy (PR from feat/harness-proxy, 4.5.0): the proxy's deterministic BASELINE classifies from the request text only (classify::classify with no paths, changed_lines 0) and never measures the repo diff. Reason: classify::from_args measures the branch diff against its base, so on any feature branch every fresh request inflated to architectural/orchestrated and the monotonic floor then forbade Jev from lowering it (observed: 'fix the typo in README' -> 
[truncated]
