## Memory
- Key: pathutil-separate-git-dir
- Written-by: claude
- Written: 1790668231
- Verified: 1790668231
- Source: explicit

git init --separate-git-dir breaks the common-dir-parent assumption used to identify a main checkout in worktree_identity; accepted limitation (#467).
