# Native OpenAI Responses provider (N08)

**Date:** 2026-09-12 · **Issue:** #477 · **Roadmap:** #469

## Boundary

`OpenAiResponsesAdapter` implements the same `provider::adapter::
ProviderAdapter` contract the Anthropic transport does: it transports one
already-compiled request and nothing else. It does not own the agent loop,
execute tools, choose fallback routes, or persist messages, and no Codex
binary, Codex SDK, or Codex App Server participates -- the adapter opens
`POST /v1/responses` itself over raw HTTPS/SSE.

Route, account, endpoint, model and credential identity come from N02.
Subscription-billed accounts are refused before any credential is read.
Non-loopback plaintext endpoints are refused. The request carries only the
compiled system/data messages and each Zirv tool's name, description, and
JSON schema; permission claims, executor metadata and credentials never
enter tool schemas or diagnostics.

## Credential class

Only a Platform API key is accepted. A ChatGPT/Codex subscription login is an
OAuth JWT or a whole `auth.json` object, and billing it as API usage would
silently move spend between two different identities, so a secret shaped like
one (`{...}`, a `eyJ`-prefixed JWT, or an already-formed `Bearer ...` value)
is refused with an `Entitlement` failure naming the fix. No credential class
is ever substituted for another, and N02 already refuses `~/.codex/auth.json`
as a credential *reference*; this is the value-shaped half of the same rule.

## Conversation state

Every request sends `store: false` and replays the full locally owned
history. `previous_response_id` is deliberately unused: it can only continue
a provider-stored response, so it could never be the durable state, and a
route that depended on it would lose the conversation when the provider
expired it.

Reasoning items are preserved as one typed `Thinking` block whose summary
text is readable and whose signature is the **entire** provider item --
`id`, `summary`, and `encrypted_content` -- kept verbatim as
`OpaqueProviderData`. Replay re-emits that item byte for byte, before the
function calls it belongs to, and `include: ["reasoning.encrypted_content"]`
asks for it on every reasoning-model request. `Debug` output is always
redacted.

`call_id` is the durable function-call identity (it is what a
`function_call_output` references). The per-item `fc_...` id is not replayed:
it is provider bookkeeping for a stored response, not continuation state.

## Cross-provider containment

Opaque state cannot cross providers. A `Thinking` block reaching this adapter
must carry an OpenAI reasoning envelope (`type: reasoning` with a non-empty
id) or the request is rejected before transport; `redacted_thinking` blocks
are rejected as Anthropic-only. In the other direction the Anthropic
transport already requires a string signature, so an OpenAI reasoning
envelope is rejected there too, and its `redacted_thinking` rejects a refusal
block the Messages API has no representation for. The N03 journal binds
stored continuation envelopes to their route/protocol/model and answers a
mismatched identity with `ContinuationMismatch`.

## Streams, failures and usage

The SSE accumulator is driven by each event's own `type`, independent of HTTP
chunk boundaries. Content is committed only from `response.output_item.done`:
streamed deltas drive the UI and the watchdog but never the committed turn.
A function call is committed only when the completed item's `arguments` parse
to a JSON object *and* match both the streamed deltas and any
`function_call_arguments.done` value; anything else is
`InvalidToolArguments`.

- `status: incomplete` drops **every** function call from the turn, records
  the reason and the omitted `call_id`s in `stop_details`, and maps
  `max_output_tokens`/`content_filter` onto `MaxTokens`/`Refusal`.
- A stream that ends without a terminal event is `InvalidStream`, never a
  completion. So is `response.completed` with an unfinished output item.
- `response.failed` and top-level `error` events, and HTTP statuses, map onto
  stable classes: authentication, permission, model access, entitlement
  (including `insufficient_quota`, which shares 429 with rate limiting but no
  backoff can clear), rate limits with `Retry-After`, overload, provider,
  transport, context overflow, cancellation, and first-event/idle timeouts.
- Refusal items stay a typed `Refusal` block and never become assistant
  prose.

Usage is read exactly once, from the terminal event: OpenAI reports a
cumulative object, so per-event snapshots are ignored rather than summed.
`cache_creation_input_tokens` stays zero because the Responses API publishes
no cache-write class; `cached_tokens` and `reasoning_tokens` map onto the
cache-read and reasoning classes.

## Model controls

Controls are validated against the exact API model id, never a Codex alias: a
model id naming a Codex harness build (`codex-*`, `*-codex`) is refused with
an actionable message, because Codex availability and Responses API
availability are not interchangeable. Reasoning models take `low`/`medium`/
`high` effort and an optional `summary`; `xhigh`/`max` (Anthropic levels), a
manual thinking budget, disabled reasoning, and thinking-display updates are
all refused before the request. Non-reasoning models refuse reasoning
controls entirely. Stop sequences and explicit cache modes are refused
because the Responses API has neither. Output size is checked against the
declared context window when the catalogue has a verified one; no capacity is
invented where the catalogue records none.

## What is verified, and what is not

Fixture-verified shapes (frozen under `tests/fixtures/provider/openai/v1/`):
`response.created`, `response.in_progress`, `response.output_item.added`,
`response.content_part.added`, `response.content_part.done`,
`response.output_text.delta`, `response.refusal.delta`,
`response.refusal.done`, `response.reasoning_summary_part.added`,
`response.reasoning_summary_text.delta`,
`response.reasoning_summary_text.done`,
`response.function_call_arguments.delta`,
`response.function_call_arguments.done`, `response.output_item.done` for
message/function_call/reasoning items, `response.completed`,
`response.incomplete` with `incomplete_details.reason`, and the top-level
`error` event.

Assumed from documentation and **not** yet observed live here: the exact
reasoning-item field set beyond `id`/`summary`/`encrypted_content`, HTTP
error `code` values other than the ones normalized above, and per-model
effort/limit availability for any specific id.

Live evidence: none. The opt-in `live_openai_responses_contract` test is
`#[ignore]`d and needs `OPENAI_API_KEY` plus an entitled exact model id in
`ZIRV_OPENAI_LIVE_MODEL`; ordinary CI never requires credentials and the test
records nothing without them. Until that runs against a configured primary
role model, OpenAI routes are **fixture-verified, live pending** -- not
supported. N02's capability table likewise still reports `declared`, never
`verified`.

## Deferred

Azure and other cloud transports stay out of this adapter: they are a
distinct endpoint profile, not an arbitrary `base_url` substitution, and N02's
endpoint contract still governs which base URLs a route may use. Structured
output arrives as ordinary `output_text` (the adapter contract carries no
response-schema field yet), image and audio modalities have no contract
representation to validate, and wiring this adapter into the durable agent
loop belongs to N09.
