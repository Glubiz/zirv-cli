## Memory
- Key: workflow-claude-sidechain
- Written-by: claude
- Written: 1790668296
- Verified: 1790668296
- Source: explicit

Current Claude Code writes subagent turns under sibling subagents/ files rather than isSidechain rows in the main transcript; sidechain usage reads the in-file rows first for older harnesses, then the subagents/ files (#155).
