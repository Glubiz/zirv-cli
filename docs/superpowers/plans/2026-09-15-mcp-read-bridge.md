# Read-only meta-harness MCP bridge — issue #658

## Scope

First implementation on `main`: local stdio server, four tools, documented
operator registration. Mutating coordination operations remain on issue #658.

## Decisions

- `zirv ctx mcp serve [--stdio] [--repo PATH]` fixes repository authority at
  startup and captures the launch environment. Operator file policy is
  refreshed per call. `tool_access = ask/deny` and malformed configuration
  refuse calls; the bridge cannot attest an approval.
- Reuse existing retrieval, workflow, artifact records and session record
  types. Read raw session observations without the CLI's cleanup side effects;
  report liveness and host enforcement as unverified.
- Separate current-bucket lookup from the CLI's legacy-state adoption. The
  bridge and its workflow/artifact readers must never rename state while
  resolving a repository key.
- Artifact IDs come from the existing repo registry. Pin a repository directory
  capability (`cap-std`) to enforce payload containment across symlink changes.
- `rmcp` 3.3.0 supplies protocol negotiation and schemas. Tools advertise their
  read-only effects and return structured data plus a text fallback. Do not
  depend on sampling, subscriptions, or experimental tasks.
- Explicit host registration makes the initial bridge usable without adding
  config-writing behavior or changing every adapter's launch path.

## Verification

Inline tests cover scope, trusted-key precedence, changing policy/memory gates,
bounded results, UTF-8 pagination, foreign paths and symlink/FIFO replacements.
Real subprocess protocol tests exercise legacy initialization, current stateless
discovery, calls, tool errors and EOF. A Git repository with legacy buckets
verifies that reads preserve every bucket and its contents.

Before the PR: build, full nextest with `--no-fail-fast`, serial cargo test,
format check, and clippy for all targets with warnings denied. Preserve logs
and report any failures by name; no performance gains are claimed without a
separate comparison against CLI command-schema discovery.
