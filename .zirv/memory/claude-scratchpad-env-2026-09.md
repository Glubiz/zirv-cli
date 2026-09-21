## Memory
- Key: claude-scratchpad-env-2026-09
- Written-by: unknown
- Written: 1789990166
- Verified: 1789990166
- Source: explicit

Verified from local Claude Code hook/session evidence on 2026-09-21: CLAUDE_CODE_TMPDIR and TMPDIR may already name /tmp/claude-<uid>, while scratchpad paths in commands use the macOS realpath /private/tmp/claude-<uid>. Preserve an already-suffixed override and recognize both spellings. PermissionPrompt records today predominantly reported auto without an interactive launch pin. PR #712 contains the matching regression coverage.
