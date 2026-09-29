## Memory
- Key: workflow-macos-launchd-env
- Written-by: claude
- Written: 1790668296
- Verified: 1790668296
- Source: explicit

On macOS a harness shell child may lack SSH_AUTH_SOCK even when the login session has it; launchctl getenv <name> recovers it, so check children consult it only when the name is absent from zirv's own env (#233).
