## Memory
- Key: macos-accepted-socket-mode
- Written-by: unknown
- Written: 1789541299
- Verified: 1789541299
- Source: explicit

Verified 2026-09-16 with a loopback C probe on macOS: accept() inherits O_NONBLOCK from a nonblocking listener. A framed blocking reader must explicitly clear it on each accepted socket; read/write timeouts alone do not do that. Native MCP Bridge::start dropped fragmented records until this was fixed. Tracked with reproduction and source hash in GitHub issue #664.
