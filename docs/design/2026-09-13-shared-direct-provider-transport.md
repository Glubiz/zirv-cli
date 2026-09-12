# Shared SSE transport hardening for both direct providers

**Date:** 2026-09-13 · **Issues:** #476 (N07), #477 (N08)

## Context

Three Anthropic follow-up fixes landed on `release/native-harness` (PR #510)
while the direct OpenAI Responses provider (#477) was being written on a
branch that had meanwhile lifted the Anthropic transport's worker supervision
into `provider::transport`. Two of the three were not Anthropic facts at all:

- the worker's blocking body read was pinned to `StreamTimeouts::idle`, so a
  cancelled or timed-out worker kept its TCP read open for up to minutes after
  `supervise()` had already returned to the caller; and
- text / thinking / partial-tool-JSON buffers were rebuilt from an unbounded
  number of deltas, with only a single SSE *line* capped.

Both describe the SSE framing every direct adapter runs, so keeping them in
`anthropic.rs` would have meant the brand-new OpenAI adapter shipped with the
exact two defects that had just been fixed next door.

## Decision

The two transport-shaped fixes live in `provider::transport` and both adapters
inherit them; the third (the `ThinkingDisplay::Updates` family gate) stays in
`anthropic.rs`, because the `thinking-display-updates-2026-08-18` beta and the
model families that own it are Anthropic vocabulary with no OpenAI analogue.

- `WORKER_READ_POLL` (250 ms) is the socket-read cadence both adapters pass to
  `timeout_recv_body`. It is not a semantic deadline: `supervise()`'s
  wall-clock loop enforces the real first-event and idle budgets independently,
  so this only bounds how promptly a worker notices cancellation.
- `read_sse_line` is the one line reader for both `parse_sse` implementations.
  Because a bare poll timeout is now expected rather than exceptional, it
  retries on `TimedOut`/`WouldBlock` (rechecking cancellation each time) and
  keeps the partially-read line buffered across the retry so no SSE line is
  ever split in half. It also owns the per-line `MAX_SSE_LINE_BYTES` cap and
  the `InvalidData`/other-`io::Error` classification, which were duplicated
  verbatim in both providers.
- `check_block_accumulator_cap` + `MAX_BLOCK_ACCUMULATOR_BYTES` (16 MiB per
  block) settle an over-long block to the same `InvalidStream` class as an
  over-long line. Each provider keeps a one-line wrapper so its own call sites
  read in its own voice. The OpenAI call sites are the `output_text`/`refusal`
  part buffers, `function_call_arguments` deltas, and each reasoning-summary
  buffer; `function_call_arguments.done` is not capped because it arrives whole
  on one already-capped SSE line.

## What is verified

- `anthropic::tests::cancellation_mid_stream_tears_down_the_worker_promptly`
  and `openai::tests::cancellation_mid_stream_tears_down_the_worker_promptly`:
  a server that stalls mid-body observes the client close its connection within
  2 s of cancellation. Confirmed load-bearing -- restoring
  `timeout_recv_body(idle)` in `openai.rs` fails the OpenAI one in 2.08 s.
- `anthropic::tests::oversized_content_block_settles_to_invalid_stream` and
  `openai::tests::oversized_content_block_settles_to_invalid_stream`: many
  individually-legal SSE lines whose cumulative block crosses the cap settle to
  `InvalidStream` naming the block cap, not the line cap.
- `anthropic::tests::thinking_display_updates_is_gated_to_the_owning_model_family`:
  unchanged by the move, still Anthropic-local.
- `openai::tests::failed_response_events_are_typed_from_their_nested_error`
  against the new `tests/fixtures/provider/openai/v1/stream-failed.sse`: the
  `response.failed` arm reads `/response/error` (unlike the top-level `error`
  event, the only error shape previously fixture-covered) and settles to
  `Provider`/endpoint-scoped/retryable while discarding partial output.

## What is deferred

`parse_sse` itself is still one function per provider. Only the line reader is
shared, because the event-name dispatch, the accumulator shape and the
terminal-event contract genuinely differ between the Messages and Responses
protocols; folding those together would be a speculative abstraction over two
formats rather than a shared fact. Live-network validation of either provider
remains pending per the two N07/N08 notes.
