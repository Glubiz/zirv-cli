# Native MCP, web, browser, diagnostics and artifact capabilities (N14)

**Date:** 2026-09-13 · **Issue:** #483 · **Roadmap:** #469

## Context

N09 made a native session drive a model and its own tools. What it could not
do was anything a host harness used to supply on the side: MCP servers, a
browser, web search, language diagnostics, artifact presentation. Calling a
model API inherits none of that, and the existing `CapabilityReport` had no
vocabulary for "configured but not proven", so a session could only either
claim a capability it did not have or deny one it did.

That gap is sharpest for frontend work, which is the one workflow domain that
genuinely cannot proceed without rendering and looking at a page.

## Decision

Three new modules, thirteen new tools, one new verb, and one new state in the
capability report.

### `runtime::mcp` -- the protocol

zirv speaks MCP itself. `McpTransport` is framing only; every protocol rule
lives in `McpClient`: `initialize` against the 2025-11-25 revision with an
explicit refusal for an unknown one, `notifications/initialized`, paginated
`tools/list`, `resources/list`, `tools/call`, `notifications/cancelled`,
shutdown, and reconnect with re-discovery. Two transports implement the same
trait -- a child process over newline-delimited JSON-RPC, and Streamable HTTP
with bearer credentials from the existing provider credential store, accepting
both reply shapes a POST may return.

**Two contracts are enforced rather than documented.**

*A stale call can never execute a different tool.* Every catalogue entry
carries a digest over its name and input schema. A reconnect that changed or
removed a tool invalidates it, and a call naming an invalidated tool fails with
`StaleTool` before the request is sent. Only an explicit re-describe clears the
hold, so the caller has demonstrably seen the current shape.

*A large catalogue does not enter every request.* At or below
`capabilities.max_inline_mcp_tools` (24), discovered tools are promoted into
the registry as real tool definitions. Above it, the catalogue is reachable
only through `mcp_list` (a compact index: name, title, one bounded summary
line, no schema), `mcp_describe` (one full schema on demand) and `mcp_call`.

Server descriptions and results are untrusted data throughout: bounded,
redacted through the existing `pace::redact_for_log` path, never executed, and
size-capped into the existing output store when they exceed the inline limit.

### `runtime::capabilities` -- the configured backends

Web search/fetch and browser automation are *configured* capabilities. A raw
model API supplies neither, and nothing here pretends otherwise: each backend
exists only where an operator configured one, and a call against an absent
backend returns a typed `Unavailable` naming the missing binary, credential or
config key. **No path in this module returns an empty success**, which is what
makes "fabricated capability success is impossible" structural.

Provenance is likewise structural. A search row whose source URL does not parse
is dropped rather than reported; a browser capture that wrote no readable file
is an error rather than a success with a path nobody checked. The browser
backend is the same headless Chromium invocation `frontend render` already
drives, so a machine that can capture a render can inspect a page.

`EgressGuard` is the single interception point for every outbound request this
module can make. #466's on-device obfuscation replaces the pass-through
implementation; it does not need a second interception subsystem.

Diagnostics report the tooling actually installed, reusing `ctx::diagnostics`'s
own checker discovery -- never an imaginary IDE feature.

### The three states

`CapabilityReport` gains `integrations`, each `available`, `unavailable` or
`unverified`, and `admit`, which refuses a workflow step and quotes the
diagnosis. `unverified` is what keeps the other two honest: discovery contacts
nothing (config, PATH and the tree only, so it is cheap enough for every
workflow admission check), so a configured MCP server nobody spoke to this run
is not evidence that it answers. `zirv ctx capabilities --probe` is the surface
that converts an unverified row into a verified one by connecting; `--require`
gates a script on the same rule the engine applies.

`workflow::engine` refuses a step whose required integration is unavailable at
`workflow start`, before the step runs -- today a frontend implement/review/
verify step needs `frontend.render`, and a present step needs
`artifact.render`.

### The tools

All thirteen reach the registry through the SAME path the file and process
tools use: closed schema, complete typed request, N04 action, broker
authorization, bounded receipt, existing output store. Nothing forks that
machinery.

- `web_search`, `web_fetch`, `browser_capture`, `browser_inspect` become
  `ExecutionAction::Network`, so the host crosses the operator's own allowlist
  -- which is also what a native session's `NetworkScope` is now built from.
- `browser_capture` takes a *label*, not a path: zirv slugifies it and writes
  under a state-dir evidence root, the same way the output store hands back
  opaque ids. Provider output cannot steer where a screenshot lands.
- `diagnostics_report`, `capability_report`, `artifact_present` are knowledge
  reads; `artifact_register` is a file read (the record itself is zirv state).
- `frontend_render` and `frontend_review` are knowledge actions the broker
  prices with `shell_exec` and `network`, because they start a development
  server and a browser. The pricing lives in the broker, not in the tool.
- `mcp_list`/`mcp_describe` carry no declared effects; `mcp_call` and every
  promoted tool carry the effects the *operator* declared for that server.
  `NativeToolClient` substitutes them before the broker sees the action, and an
  unknown server gets the conservative all-effects declaration N04 requires.
  An MCP call is never replayed (`NeverAfterStart`), and a cancelled one
  reports an unknown outcome.

Promoted tools are namespaced `mcp__<server>__<tool>`, so a server calling its
tool `file_write` cannot shadow the built-in one, and a collision is refused
rather than overwritten.

### Configuration

The whole `[capabilities]` table is `REPO_FORBIDDEN`, as one prefix entry.
Unlike tables where only some keys are operator-only, there is no narrowing
half here: every key names an MCP server command zirv spawns, a remote endpoint
it authenticates to, a credential reference, or a browser binary it launches.
Everything is off by default.

## What is verified

Deterministic, no network, no paid call, no installed MCP server or browser:

- A local server negotiating, discovering and calling a tool, and a remote one
  doing the same over bearer-authenticated HTTP in both legal POST reply
  shapes, with the negotiated session id pinned on later requests.
- A changed schema and a removed tool after reconnect each refusing the stale
  call; the changed one admitted again only after a re-describe.
- A 64-tool catalogue indexing compactly and serving a schema on demand.
- Secret-shaped server prose redacted and bounded before it can reach a
  request; malformed and oversized tool rows dropped rather than registered.
- A cancelled call sending `notifications/cancelled` and reporting an unknown
  outcome; a JSON-RPC error staying a typed error; an unsupported protocol
  revision refused rather than guessed.
- End to end through `NativeToolClient::execute` with the real broker and only
  the transport replaced: a completed MCP receipt with a policy fingerprint, a
  policy denial that never reaches the server, a 40-tool catalogue left out of
  the registry but callable through `mcp_call`, an unconfigured web tool
  failing with a diagnosis, and a capability report naming only the three
  states.
- Web results carrying their source URL (and rows without one dropped); the
  host allowlist as a closed door with exact-or-dotted-suffix matching; the
  egress seam refusing a request; a capture linking to evidence that exists and
  a capture without evidence being an error.
- Workflow admission refusing an unavailable integration and quoting the
  diagnosis, with `unverified` admitting.

## What is deferred

- **Live backends.** No real MCP server, search endpoint or browser was
  contacted from this step: every backend is fixture-verified, and the
  integration is reported `unverified` until a real one is configured. That is
  the honest reading of "record actual provider/platform coverage".
- **#438 and #439.** Both are open and unimplemented. Evidence already flows
  through the existing output store, which is the seam #438 would build on; no
  duplicate evidence service was created here.
- **#466.** `EgressGuard` is the named seam and the default is pass-through.
  No obfuscation or placeholder machinery is implemented.
- **TUI surfacing.** Every output is inspectable from headless results and from
  `zirv ctx capabilities`; wiring evidence paths into dashboard panes is N11's
  surface, not this step's.
- **Concurrent MCP servers per call.** Clients connect lazily and are used one
  at a time, mirroring N09's own single-effect scheduling.
- **Credential rotation mid-session.** Bearer tokens resolve once at session
  start; a rotation is picked up by the next session, not by a reconnect.
