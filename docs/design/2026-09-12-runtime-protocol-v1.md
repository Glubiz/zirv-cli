# Runtime protocol v1: a stable local API before the daemon (issue #353)

**Date:** 2026-09-12 · **Issue:** #353 (prerequisite of #352, and of #489, step N20 of the native-runtime roadmap #469) · **Status:** decided and implemented

## 1. Context

zirv's control surface is currently several purpose-specific mechanisms that
grew where they were needed: per-session turn-signal sockets
(`ctx::signal`), registry and state files (`ctx::sessions`, `ctx::state`),
dashboard request directories (`dash::spawnreq`), mail files (`ctx::mail`),
and direct in-process calls inside the dashboard. Each is fine on its own.
Together they mean there is no single, versioned answer to "what sessions
exist, what are they doing, and how do I drive one" that anything outside
this binary could depend on.

Issue #352 wants to move PTY ownership out of the dashboard into a
persistent runtime. Issue #489 (N20) wants native sessions reachable through
that same runtime. Both are impossible to do safely while the control
surface is implicit: adding a daemon first would move today's coupling
behind a socket without making it a contract, and the first alternate client
would freeze whatever shape happened to be there.

Step N01 (#470) already established the *in-process* seam:
`runtime::RuntimeBackend`, plus `runtime::protocol`'s versioned
command/event/reply envelopes and `dispatch`. That is the seam between zirv's
own supervision code and a backend. It is not a public API: its envelopes
carry `SessionHandle`s and a `SessionSpec` with the launch prompt in it,
which is exactly the sort of thing a third party must not see or forge.

## 2. Decision

Publish a separate, deliberately narrow protocol — `src/commands/ctx/api/`,
protocol version 1 — and implement it against an **in-process reference
server**. No daemon in this change; issue #353 explicitly allows that, and
issue #352 owns the daemon.

### 2.1 Two wires, on purpose

`runtime::protocol` (internal, backend-shaped) and `api::wire` (public,
redacted) are separate modules and stay separate. The public wire's only
session shape is `SessionFacts`:

```
session_id, short, runtime, generation, surface, state,
role?, agent?, repo_slug?, started_at?, reachable
```

There is no transcript path or body, no prompt, no mail body, no terminal
buffer, no credential and no absolute repository path — only the sanitised
slug `state::repo_slug` already derives. "Do not expose transcript bodies,
secrets, mail bodies, or terminal history in snapshots by default" is
enforced by that type existing rather than by a filter somebody has to
remember to apply, and a test asserts the property over the serialized key
set so adding such a field fails in the change that adds it.

### 2.2 Framing and versioning

NDJSON in both directions. Every frame carries `v`; every server frame is
internally tagged by `type` (`hello`, `response`, `event`) so a non-serde
client can classify a line before parsing it. `#[serde(flatten)]` is used
nowhere on this wire — nesting `outcome` and `payload` one level costs a
line of JSON and buys a shape that is trivial to hand-parse.

Compatibility rules, each covered by a test:

- unknown fields are ignored (nothing sets `deny_unknown_fields`);
- every published vocabulary ends with an `unknown` fallback;
- the server writes a `hello` frame with its advertised capabilities before
  reading anything, and the client intersects that with its own supported
  set **locally** — a capability it does not have is never called, and a
  capability the server lacks is disabled in the client rather than
  discovered through a failed round trip. A committed fixture
  (`client-previous-minor.json`) plays a previous-minor client against the
  current server to prove the direction that actually matters.

Requests carry a caller-chosen `id`; replies echo it and carry the server
`revision`. Mutations accept an optional `idempotency_key`; a retry with the
same key returns the first attempt's result without touching the backend
again, proven by the event log rather than by the reply alone.

### 2.3 Revisions, gaps and pinned waits

One server-wide `revision`, advanced by **exactly one per emitted event**
and by nothing else. That single rule is what makes `revision != last + 1` a
reliable "you missed something" signal; the client's `GapTracker` turns it
into a `session.snapshot` refresh, and the recovery is implemented in the
client because that is where the decision belongs.

`session.wait` resolves the session's generation at call time and compares it
on every poll. A session replaced while a wait is running fails that wait
with `stale_generation` — a replacement never satisfies an old wait, even if
it reaches the requested state. Mutations take an optional `generation` and
are refused in *both* directions when it does not match the current one: an
older pin means the session was replaced, a newer one means the caller is
talking about a session this server has never seen.

### 2.4 Transport and security

`api::transport` mirrors `ctx::signal`'s platform split rather than
inventing a second one: unix domain socket on unix, named pipe on Windows,
same "the state-directory path is the thing callers pass around" discipline,
same up-front length check. Two things differ, both forced by what this
endpoint carries:

- it is **duplex**, so the Windows side creates `PIPE_ACCESS_DUPLEX`
  instances and blocks in `ConnectNamedPipe` on a dedicated accept thread
  instead of running the overlapped dance a supervisor's main loop needs;
- it is **owner-only by construction**. On unix the socket sits in a 0700
  directory, is chmod'ed 0600 after bind, and the server verifies the peer
  uid (`SO_PEERCRED` on Linux, `getpeereid` on macOS/BSD) against its own. On
  Windows the pipe is created with an explicit protected DACL granting
  generic-all to the calling user's SID alone, because the *default*
  named-pipe security descriptor grants read access to Everyone and the
  anonymous account. That needed one added `windows-sys` feature
  (`Win32_Security_Authorization`) and no new crate.

A uid the platform does not expose is **not** treated as a mismatch: Windows
exposes none on a named pipe, and refusing every unknown peer would mean
refusing every Windows connection. The endpoint permissions are the
guarantee there, which is why they are explicit rather than defaulted.

The endpoint path is derived from the operator-owned `StateDir` and from
nothing else. There is no flag, no configuration key and no environment
variable that names one — "never accept repository-controlled endpoint paths
or policy" is enforced by there being no code that could. The server's policy
(which methods exist, which capabilities are advertised) is compiled in.

### 2.5 What the server holds, and what it refuses

Shared runtime facts only: the session set, the event log, backend handles
for sessions this server itself started, and the idempotency cache. Layout,
colour, sidebar selection, mouse state and modals are client presentation
state and no method can read or write them.

A server with no `RuntimeBackend` attached — which is every production
invocation today, because PTY ownership stays with the dashboard until #352 —
serves every read method off the session registry and refuses
`session.start|stop|send_input` with a structured `unsupported` that names
#352 and #489. Refusing loudly is the point: a mutation that silently did
nothing would be worse than no protocol at all.

One nuance the registry forced: `facts_from_record` publishes `generation: 1`
for every registry record, because the session registry has no generation of
its own (the orchestrator seat does, and the persistent runtime will).
Publishing a constant is honest; deriving one from, say, a restart count
would let a client believe a pin meant something it does not.

A second nuance: a lifecycle state a client *reported*
(`session.report_status`), or that this server itself caused, is remembered
and re-applied when the session source is refreshed. The registry projection
is a coarse "is the process alive and is a turn in flight"; the client
driving a session knows better, and a refresh must not silently undo it.

### 2.6 Schema generation

`zirv ctx api schema [--json]` is generated from the binary: the method list,
their parameter and result fields and their gating capability come from the
`METHODS` table (pinned exhaustively against the `Method` enum), and every
vocabulary's values come from `serde_json` serializing the real variants. The
envelope field lists are static data, so a test compares them against a
fully populated example of each real struct in both directions — every key
serde writes must be documented, and every documented key must be one serde
can write.

## 3. What is verified

- 45 tests in `commands::ctx::api::*`, plus the frozen-fixture guard.
- **Wire compatibility.** `tests/fixtures/protocol/v1/` holds one committed
  request and one committed response per published method, the `hello` frame
  and one frame per event kind. The guard replays every request against a
  deterministic server (fixed session source, the in-memory fake backend, a
  fixed call order) and compares byte for byte. `server_version` is
  normalised to `<version>`, because it moves on every release without the
  protocol moving. The fixtures are pinned to LF in `.gitattributes`.
- **Real transport on this platform.** Frame round trip, `probe` liveness,
  and an end-to-end client/server exchange all run over the actual socket or
  named pipe, not a mock.
- **Security.** Endpoint-inside-the-state-directory, the same-user rule in
  both directions, and — under `#[cfg(unix)]` — 0600/0700 on disk and a real
  peer-credential check on a live connection.
- **Behaviour.** Stable ids across client/surface changes, gap detection and
  refresh, idempotent retry that emits no second event and creates no second
  backend session, generation-pinned waits refused by a replacement, and the
  previous-minor negotiation.
- **The CLI and the test client exercise the same methods** through the same
  `api::client::Client` over the same transport: `zirv ctx api call` starts
  the reference server in-process when nothing is listening rather than
  short-circuiting into the server object.

## 4. What is deferred

- **The daemon** (#352). `serve` is bounded and in-process; nothing survives
  the process that started it.
- **Native sessions on the protocol** (#489, N20): native submit/steer/
  interrupt/approval methods, journal cursors for bounded event fanout,
  controller-versus-observer enforcement, and reconciliation after a service
  restart.
- **Dashboard and CLI callers.** Nothing in zirv *consumes* the protocol yet;
  today's callers keep using the registry and the in-process paths. Routing
  them through it is #352/#489 work, and doing it here would have meant
  changing the dashboard in the same change that defined the contract.
- **Peer identity on Windows.** Named pipes expose the client token through
  impersonation rather than a socket option; v1 relies on the owner-only DACL
  and says so rather than pretending to a check it does not make.
- **Everything outside the v1 method set**: mail, memory, work groups,
  workflows, layout and plugins get methods when a concrete client needs
  them, not before.
