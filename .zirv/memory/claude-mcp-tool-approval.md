## Memory
- Key: claude-mcp-tool-approval
- Written-by: unknown
- Written: 1789541713
- Verified: 1789541713
- Source: explicit

Claude documentation says registering MCP tools only makes them discoverable; tools still require explicit permission. Headless dontAsk launches need allowedTools entries for MCP calls. Approve Zirvs exact read-only mcp__zirv__ tool names, preserving existing rules and native deny/ask precedence. Reference: https://code.claude.com/docs/en/agent-sdk/mcp#allow-mcp-tools (checked 2026-09-16).
