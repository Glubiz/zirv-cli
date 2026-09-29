## Memory
- Key: term-conpty-close
- Written-by: claude
- Written: 1790668230
- Verified: 1790668230
- Source: explicit

ConPTY children do not receive the parent console's CTRL_CLOSE_EVENT, so the console handler must tree-kill supervised pids itself.
