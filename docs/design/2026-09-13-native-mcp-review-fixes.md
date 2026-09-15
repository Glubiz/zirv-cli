# Native MCP client review-fix round (N14)

**Date:** 2026-09-13 · **Issue:** #483 · **PR:** #524

## Context

PR #524 shipped `runtime::mcp`, `runtime::capabilities` and the
`capabilities` command (see `2026-09-13-native-capabilities.md`). Review of
that diff found one major and three minor gaps: a cancellation window a
blocked call could sit inside past its own deadline, a stdio server's stderr
discarded instead of explaining a transport failure, an IPv6 authority
mis-parsed by `host_of`, and a mixed MCP probe collapsing to an
undifferentiated "unavailable" line. This note records the fix for each,
test-first.

## Decision

### `call_raw_cancellable` polls `cancel` while blocked, not only around it

Before this round, `McpClient::call_raw_cancellable` checked
`cancel.is_cancelled()` immediately before calling `transport.request` and
again after it returned an `Err` -- so a cancellation that fired while
`request` was itself blocked (inside the stdio reader's `recv_timeout` loop,
or inside the HTTP poster's blocking POST) was invisible until the request's
own deadline. `McpTransport::request` now takes `cancel: &dyn Cancellation`
and threads it into the wait:

- **Stdio.** `StdioTransport::await_response`'s existing `READ_POLL` (50ms)
  loop now checks `cancel.is_cancelled()` on every tick before blocking on
  the next frame, returning `McpError::Cancelled` directly. The outer
  `call_raw_cancellable` still owns sending `notifications/cancelled` -- it
  reacts to any `Err` while `cancel.is_cancelled()` is true, so this needed
  no change there.
- **HTTP.** `HttpPoster::post` is one blocking call with no cancellation seam
  of its own, so `HttpTransport::post_cancellable` runs it on a background
  thread and polls a channel every `READ_POLL` alongside `cancel`. A
  cancellation returns promptly; the background thread is left to finish (or
  hit the poster's own deadline) on its own, and its answer is simply
  dropped -- exactly what "the outcome is unknown" already meant.

`notify()`'s own send (the handshake's `notifications/initialized`, and the
cancellation notification itself) uses `NeverCancelled`: a notification must
not be cancellable by the same flag that is telling it to fire.

### Stdio stderr is drained into a bounded, redacted tail

The child's stderr was `Stdio::null()`. It is now `Stdio::piped()`, drained
by a second background reader thread into a `MAX_STDERR_TAIL_BYTES` (4 KiB)
ring buffer. A transport error arising while a server is still connected
(closed output stream, a malformed frame) now appends
`(stderr: <redacted tail>)` via the same `redact_for_log` path every other
piece of untrusted server text already goes through. The disconnect branch
sleeps 50ms before reading the tail: the stdout pipe can observably close a
beat before the stderr reader thread has drained already-buffered bytes, and
this is the one place that ordering is load-bearing for a test to observe.

### `host_of` parses bracketed IPv6 authorities

`authority.split(':').next()` treated `[::1]:8080` as host `[`, because the
address itself is full of colons. `host_of` now special-cases a `[`-prefixed
authority: the host is everything up to the matching `]`, and a missing `]`
is malformed input (`None`, fail-closed) rather than a guess. The userinfo
split (`authority.rsplit('@').next()`, taking the *last* `@`-delimited
segment as host) was already correct against a userinfo-based bypass
(`https://trusted.example@evil.example/` resolves to host `evil.example`);
it is now covered by a regression test alongside the IPv6 fix so the two
don't drift apart.

### A mixed MCP probe names both halves instead of one count

`capabilities_cmd::apply_probe` folded a probe into `Available` only if every
configured server answered, `Unavailable` otherwise -- and the `Unavailable`
diagnosis was just "N of M configured server(s) did not answer", with no way
to tell which N. The admission rule is unchanged; the diagnosis (and the
`Available` detail line) now names the reached and unreached servers
explicitly.

## What is verified

- A real stdio child process (`tests/fixtures/mcp-hang-server.{sh,cmd}`) that
  never answers anything: a cancellation fired 200ms after the call starts is
  observed and returns `Cancelled` in well under the 30s request deadline,
  and the follow-up `notifications/cancelled` still reaches the (still-alive)
  server.
- A real stdio child that writes a secret-shaped diagnostic to stderr and
  exits without ever answering: the resulting transport error contains the
  diagnostic and not the secret.
- `host_of("https://[::1]:8080/...")` resolves to `::1` and matches an
  allowlist entry for it; `https://allowed.example@evil.example/` resolves to
  `evil.example`, not `allowed.example`, so userinfo cannot launder a denied
  host past the allowlist.
- A mixed probe (`apply_probe` with one `available` and one `unavailable`
  server) still reports `Unavailable` -- the admission rule did not change --
  with a diagnosis naming both servers.

## What is deferred

- The HTTP cancellation path is verified indirectly (unit tests on the stdio
  poll loop and on `apply_probe`/`host_of`); no fixture HTTP server that
  blocks mid-request was added in this round, since `FixtureHttpPoster`
  answers synchronously in-process by construction. The background-thread
  polling in `post_cancellable` is the same mechanism the stdio fix uses and
  shares its `READ_POLL` granularity, but it has no dedicated timing test the
  way the stdio path now does.
- No change to `McpTransport::shutdown`'s own grace period or to how a
  cancelled-but-still-running background HTTP request is eventually reaped;
  it is left to finish against the poster's own deadline, same as before this
  round for a plain timeout.
