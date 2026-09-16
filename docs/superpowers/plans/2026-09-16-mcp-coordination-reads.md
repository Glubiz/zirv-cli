# MCP coordination reads

Substantial change: extend the read-only bridge with worker status, persisted
report pages, scoped inbox reads, and a real stdio connection diagnostic.

- Keep all mutations on the existing CLI. No sends, acknowledgements, launches,
  workflow transitions, memory writes, or host configuration writes.
- Fix inbox identity at launch with `--session` (or the inherited
  `ZIRV_CTX_SESSION`). Resolve it against the supervisor's registry and require
  the registered canonical repository to match `--repo`. Derive the agent and
  mailbox from that record. Without a binding, expose only undirected `any`
  mail in the selected repository. This is operator-owned local configuration,
  not isolation from hostile processes sharing the OS account.
- Reuse the mail reader's recipient, expiry, and fan-out rules without consuming
  mail. Apply current mail and tool-access policy on every call.
- Read delegation records from the current repository bucket without invoking
  legacy migration. Also expose ordinary harness reports, whose existing writer
  gains additive repository provenance. Old unscoped reports require an existing
  scoped delegation reference; their filename alone never authorizes a read.
- Report persisted phases and contract outcomes distinctly from liveness and
  task correctness. Page full stored result JSON so validation errors and
  structured results remain available alongside report text.
- Bound listings and pages, return cursors and content revisions, and reject
  stale pagination. Confine report access to the operator-owned result directory.
- `zirv ctx mcp doctor` launches the current executable, negotiates MCP, discovers
  tools, and performs a real snapshot read under a deadline. It diagnoses the
  server, not whether a particular host application registered it.

Verify isolation, no side effects, policy revocation, report provenance,
pagination, changed content, and real stdio calls. Run build, full nextest
(`--no-fail-fast`), serial compatibility tests, formatting, and Clippy before PR.
