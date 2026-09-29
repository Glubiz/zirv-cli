## Memory
- Key: workflow-git-tree-diff
- Written-by: claude
- Written: 1790668296
- Verified: 1790668296
- Source: explicit

Triple-dot git diff (A...B, merge-base) requires commit-like refs and rejects a bare tree object, while plain git diff A accepts a tree; review delta rounds diff from a reviewed tree, so they must use the plain form (DiffBaseKind::Tree).
