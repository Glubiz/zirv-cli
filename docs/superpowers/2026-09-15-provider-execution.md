# Provider-owned execution (#650)

## Decision

Add a versioned execution adapter beside direct provider transports. A route
selects its provider, execution adapter, authentication owner and billing class
separately. Only the official local Claude Code adapter is enabled initially;
future providers must implement the same execution contract and establish their
own authentication entitlement. This corrects #644 for Claude: no token import,
OAuth client cloning, subscription HTTP transport or credential migration.

Claude Code owns its model connection, internal agent loop and conversation.
Zirv owns the seat, native journal/presentation, task board and effect broker.
Expose Zirv tools through an authenticated, local MCP bridge. Disable Claude
built-in tools for this route using its documented tool-selection interface;
this preserves native path policy, sandboxing, approvals and generation fencing
at the actual effect. Streamed tool events are observations, never instructions
to execute again. Internal subagents are unavailable; independently scheduled
Zirv workers use the existing team tools.

## Authentication, policy and billing evidence (2026-09-15)

- https://code.claude.com/docs/en/legal-and-compliance permits running the
  unmodified binary under its conditions and applicable Commercial Terms.
  Users authenticate themselves through the official binary. No authentication
  method is removed, no credentials are collected, and no endorsement is claimed.
- https://support.claude.com/en/articles/15036540-use-the-claude-agent-sdk-with-your-claude-plan
  has a June 15 pause: print-mode usage currently draws subscription limits.
  Its older credit announcement is superseded. This is not permanent entitlement.
- https://code.claude.com/docs/en/agent-sdk/overview retains restrictive language
  about third-party subscription login. This integration relies on the explicit
  unmodified-binary allowance, not a new SDK authentication integration. Changes
  beyond that allowance require clarification from Anthropic.
- https://code.claude.com/docs/en/headless documents streaming, explicit resume,
  incomplete SIGTERM turns and startup hooks without a trust prompt. Bare mode
  excludes subscription login and is not used.
- https://code.claude.com/docs/en/cli-reference documents auth status/login,
  settings-source selection, restricted mode, tool selection and strict MCP.

Subscription selection rejects conflicting effective account selectors before
launch. No automatic API fallback. CLI cost estimates are API-equivalent
estimates, not invoices; billed spend and allowance remain unknown. Users control
paid usage credits in Claude Settings > Usage; Zirv cannot verify that switch.

## Boundaries

Fail closed on unsupported capabilities, unmanaged startup execution or ambiguous
billing. Never consume allowance in discovery/status checks. Persist an exact
Claude session reference with the route, workspace and seat. An in-flight
checkpoint survives a crash: require reconciliation before resuming uncertain
effects rather than automatically replaying a task. Continuation tokens never
cross into API conversation state. Handoff uses portable Zirv checkpoints.

Managed settings sources were rechecked against
https://code.claude.com/docs/en/managed-settings. They include file/drop-in
policy, macOS managed preference domains, and Windows registry/WSL inheritance.
The route rejects detected managed policy and initially excludes Windows/WSL
until effective registry-policy verification is available. No broader
authentication/permission mode is substituted.


## Validation evidence

Deterministic inline tests exercise fragmented/bounded NDJSON, initialization
restrictions, typed upstream errors, exact continuation, shell-safe arguments,
MCP authorization and duplicate-request rejection, process exit/timeout/cancel,
managed policy appearing between turns, and native receipts/recovery after an
effect. Existing mixed-runtime task and generation fences remain the integration
boundary; live authenticated team runs are not claimed.

The installed official binary reported 2.1.272. Its public auth-status command
reported signed out; no live model smoke was run. The opt-in procedure is in
README.md. A local smoke test also passed through the built binary's private
MCP relay: two authenticated JSON-RPC requests were forwarded and the relay
exited cleanly, without a model request.

Native worker token ceilings are forwarded into the same native-loop budget
field as headless requests, so official execution refuses an unenforceable
ceiling instead of silently dropping it.

Full-suite validation found #651 on unchanged release commit b4a1e2b4: ureq's
body timeout is an `ErrorKind::Other` wrapping a typed timeout. The shared SSE
reader now recognizes that wrapper, preserves partial lines, and lets the
existing first-event/idle supervisor classify the deadline. Both provider
regressions and a new deterministic partial-line case pass with this correction.

Serial verification also exposed #652: two Codex nudge fixtures waited for
session registration but could interrupt their fake process before it consumed
the initial `hang` mode. They now use the existing sibling fixture's mode-file
handshake; their original exit and mail-delivery assertions are retained.
