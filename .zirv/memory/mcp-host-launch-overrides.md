## Memory
- Key: mcp-host-launch-overrides
- Written-by: unknown
- Written: 1789538925
- Verified: 1789538925
- Source: explicit

Verified 2026-09-16: codex-cli 0.154.0 accepts a launch-only -c mcp_servers.zirv={...} inline TOML table; mcp get zirv --json confirms its stdio command/args and env_vars without editing config.toml. Official Codex MCP docs say env_vars forwards named variables to filtered stdio children. Claude Code CLI reference documents --mcp-config JSON strings/files; --strict-mcp-config excludes other registrations. Keep generated JSON off Windows cmd.exe argv (see cmd-shim-argv-reparse).
