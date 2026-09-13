# Native Google Gemini provider (N12)

**Date:** 2026-09-13 · **Issue:** #481 · **Roadmap:** #469

## Boundary

`GoogleAdapter` implements the same `provider::adapter::ProviderAdapter`
contract the Anthropic and OpenAI transports do: it transports one
already-compiled request and nothing else. It does not own the agent loop,
execute tools, choose fallback routes, or persist messages, and no Gemini CLI
process participates -- the adapter opens `POST .../models/{model}:
streamGenerateContent?alt=sse` itself over raw HTTPS/SSE, sharing the same
`provider::transport` supervisor, first-event/idle deadlines, cancellation and
`ProviderFailure` classes the other two adapters use.

Route, account, endpoint, model and credential identity come from N02.
Subscription-billed accounts are refused before any credential is read.
Non-loopback plaintext endpoints are refused. The request carries only the
compiled system/data messages and each Zirv tool's name, description, and
JSON schema; permission claims, executor metadata and credentials never enter
tool schemas or diagnostics.

## Two explicitly versioned protocol profiles

Google exposes the same content model through two different products, so
this adapter picks one profile per route and never mixes their schemas:

- **Developer** (`Protocol::GoogleGenerativeAi`): the Gemini Developer API at
  `generativelanguage.googleapis.com`, an API-key credential
  (`x-goog-api-key`), addressed at `v1beta/models/{model}` -- `v1beta` is
  where thinking controls and function calling live in the current docs.
- **Vertex** (`Protocol::GoogleVertex`): Vertex AI, an OAuth bearer
  access-token credential (`authorization: Bearer ...`), addressed at
  `v1/projects/{project}/locations/{location}/publishers/google/models/
  {model}`. `project` and `location` are new, required `AccountConfig`
  fields, validated only for `google-vertex` accounts and forbidden for every
  other provider (`config.rs`). Token *acquisition* (e.g.
  `gcloud auth print-access-token`, a service-account exchange, workload
  identity federation) is a separate credential-class concern the credential
  contract already owns; this transport only ever carries an
  already-resolved bearer token and never negotiates one itself.

`GoogleAdapter::new` refuses to construct unless the target's `Protocol` and
the supplied `GoogleProfile` agree, so a mismatched pairing is a construction
error, not a runtime surprise.

### How to add or retire a protocol profile

Adding one: (1) a new `Protocol` variant in `provider/mod.rs`'s enum plus a
`ProviderSpec` row (protocol, default base URL if any, credential class,
auth scheme, `models_list_path`, `Support`); (2) a new `GoogleProfile` arm
here with its own URL-building and header logic in `request_url` and
`perform_blocking` -- never widen an existing arm's URL shape to "maybe fit"
the new one; (3) its own credential-shape refusal case in
`reject_gemini_cli_credential` if the new profile's legitimate credential
shape could otherwise collide with a CLI-login shape; (4) its own fixture set
under `tests/fixtures/provider/google/v1/` proving its request and response
shape independently -- fixtures are never shared between profiles even when
the JSON looks similar; (5) a capability-matrix row addition if the new
profile changes what N02 should declare.

Retiring one: reverse all five in the same commit -- remove the `Protocol`
variant, the `ProviderSpec` row, the `GoogleProfile` arm, its fixtures, and
its capability row together, so an orphaned profile can never be silently
selected by a leftover route.

## Credential classes

The Developer profile takes an API key; the Vertex profile takes an access
token with project and location. A Gemini CLI login -- the OAuth desktop flow
that populates `~/.gemini/oauth_creds.json` -- is never accepted for either
profile, because it establishes no API product, project, quota or permission
a direct client can rely on. Two independent layers refuse it:

1. `credential::is_harness_login_path` refuses the path itself
   (`~/.gemini/oauth_creds.json`), alongside the existing `~/.claude/
   .credentials.json` and `~/.codex/auth.json` guards, with `Authentication`
   class.
2. `reject_gemini_cli_credential` (this module) refuses a credential
   *value* shaped like a CLI OAuth blob -- a JSON object (`{...}`) or an
   already-formed `Bearer ...` string -- with an `Entitlement` failure naming
   the fix (`GEMINI_API_KEY` for Developer, an access token for Vertex). This
   is the same posture as OpenAI's subscription-credential refusal in
   `openai::reject_subscription_credential`: no credential class is ever
   substituted for another. A real Vertex access token (`ya29...`) is not
   rejected by shape -- only the JSON-blob/`Bearer` shapes are, since those
   are what a copy-pasted `oauth_creds.json` or a mis-set header value looks
   like.

## Conversation state

Each `ProviderMessage` maps to exactly one Gemini `Content` turn (`role` +
`parts`): unlike the OpenAI Responses API, Gemini needs no item-flushing --
a turn never mixes roles, so `encode_contents` builds one `contents[]` entry
per message directly.

Google's `functionCall`/`functionResponse` parts have no call-id concept the
way Anthropic's `tool_use`/`tool_result` or OpenAI's `function_call`/
`function_call_output` do -- a function result is addressed by `name`, not by
an id. Zirv still needs a durable per-call identity for its own journal and
tool-execution bookkeeping, so the adapter synthesizes one (`call_0`,
`call_1`, ... per response, monotonically increasing) when parsing a stream,
and reconstructs the `name` a `functionResponse` needs at encode time by
scanning the same request's message history for the matching `ToolUse`
block -- the two are always adjacent by the same tool/result invariant the
other two adapters enforce (`validate_content_relationships`).

### Thought signatures

`thoughtSignature` is Gemini's continuation metadata for a chain-of-thought
turn, analogous to Anthropic's `signature` and OpenAI's encrypted reasoning
item. It is preserved as one typed `Thinking` block whose opaque envelope is
`{"type":"gemini_thought_signature","attached_to":"thought"|
"function_call","thought_signature":"..."}`:

- `attached_to: "thought"` -- a signature on a visible thought-text part. The
  block's `thinking` field carries the readable summary text.
- `attached_to: "function_call"` -- a signature-only thought riding on the
  function-call part that follows it, with no visible text of its own
  (Gemini can attach a signature straight to a `functionCall` part with no
  preceding thought-text part at all). The block's `thinking` field is empty,
  and `validate_content_relationships` requires it to be immediately
  followed by its `ToolUse` block, mirroring how the other two adapters keep
  a tool call and its reasoning adjacent.

Replay is exact: `encode_contents` re-emits a `"thought"`-attached signature
as its own `{"text":..., "thought":true, "thoughtSignature":...}` part, and a
`"function_call"`-attached signature onto the `thoughtSignature` field of the
immediately following `functionCall` part -- never as a separate part. This
is a documented design decision, not an observed wire shape: Google's current
primary docs describe `thoughtSignature` and multi-step function-calling
continuation, but the exact placement of a signature-only-on-function-call
part is inferred rather than independently confirmed against a live response
in this pass (see "What is verified" below). `Debug` output is always
redacted, and cross-provider replay is rejected: a `Thinking` block reaching
this adapter must carry the `gemini_thought_signature` envelope or the
request is rejected before transport (`thought_signature_envelope`), exactly
as Anthropic and OpenAI already reject each other's envelope shapes.

## Streams, failures and usage

Gemini's SSE stream carries no named `event:` line, only `data: <json>`
chunks, each a (assumed) incremental, non-overlapping slice of the growing
response -- not a by-index delta protocol like Anthropic's
`content_block_delta` or OpenAI's `response.*.delta` events. A `functionCall`
part is assumed to always arrive complete in one chunk (the API is not
documented to fragment function-call arguments across events the way a text
part can be fragmented across events), so there is no partial-JSON
accumulation for tool arguments the way the other two adapters need -- only
for plain and thought text.

- A `functionCall` part whose `args` is present but not a JSON object is
  `InvalidToolArguments` immediately -- it never becomes a completed
  `BlockCompleted` tool call.
- A stream that ends without a `finishReason` ever appearing is
  `InvalidStream`, never a completion (covers a truncated function-call
  argument stream the same way).
- `finishReason` values `SAFETY`, `RECITATION`, `PROHIBITED_CONTENT`,
  `BLOCKLIST`, `SPII`, `IMAGE_SAFETY` commit as a typed `Refusal` block and
  `FinishReason::Refusal` -- never prose -- with any in-flight tool call
  dropped. `MAX_TOKENS` maps to `FinishReason::MaxTokens`.
  `MALFORMED_FUNCTION_CALL` is a hard `InvalidToolArguments` error, not a
  finish reason the runtime could treat as a completed turn.
  `promptFeedback.blockReason` (the whole prompt blocked, no candidates ever
  emitted) commits as a `Refusal` response using the still-present
  `responseId`/`modelVersion`.
- HTTP-level and mid-stream `{"error": {...}}` failures share one
  classification table keyed by HTTP status and Google's `error.status`
  string (`UNAUTHENTICATED`, `PERMISSION_DENIED`, `NOT_FOUND`,
  `RESOURCE_EXHAUSTED`, ...): authentication, permission, model access,
  invalid project/location (400 mentioning `project`/`location`, scoped to
  the account), context overflow, rate limiting with `google.rpc.RetryInfo.
  retryDelay` parsed as a retry hint, overload (503), provider (5xx),
  transport, cancellation, and first-event/idle timeouts. The mid-stream
  `{"error": {...}}` shape is defensive: Gemini's documented failure path for
  `generateContent` is an HTTP-level error before the stream starts, but a
  gateway or proxy in front of the API can still fail this way.

Usage is read from `usageMetadata` (a cumulative snapshot, so later chunks
simply overwrite earlier ones): `promptTokenCount` → `input_tokens`,
`candidatesTokenCount` → `output_tokens`, `thoughtsTokenCount` →
`reasoning_tokens`, `cachedContentTokenCount` → `cache_read_input_tokens`.
`cache_creation_input_tokens` stays zero: Gemini's caching is an explicit,
separately created `CachedContent` resource, not a per-request write class,
so `CacheMode::Ephemeral5m`/`Ephemeral1h` are refused before transport with a
`Configuration` error rather than silently ignored.

## Model controls

Controls are validated against the exact model id before any request:

- `thinkingConfig` (budget, `includeThoughts`) is declared supported only
  for `gemini-2.5*`/`gemini-3*` model ids (`thinking_supported`); older ids
  refuse any non-default `ThinkingConfig`. `ThinkingConfig::Adaptive` maps to
  `thinkingBudget: -1` (dynamic); `ThinkingConfig::Enabled{budget_tokens}`
  maps directly; `ThinkingConfig::Disabled` maps to `thinkingBudget: 0` and
  is refused on a `*-pro` id (Gemini Pro models are declared not to support
  fully disabling thinking). Interleaved thinking and the `updates` display
  mode have no Gemini representation and are refused.
- Gemini has no reasoning-effort parameter analogous to Anthropic's/OpenAI's
  `effort`; any `Some(Effort)` is refused before transport.
- Stop sequences map to `generationConfig.stopSequences`, capped at 5 (the
  documented Gemini limit).
- Output size is checked against the declared context window when the
  catalogue has a verified one, same as the other two adapters.

Structured output (`responseSchema`/`responseMimeType`) and inline image
input are **not** wired in this pass: the shared `ProviderRequest`/
`ProviderContent` contract that all three direct adapters compile against has
no structured-output field and no image content variant yet -- neither the
Anthropic nor the OpenAI adapter has one either. Extending that contract for
all three providers together is future roadmap work, not something this
issue does unilaterally for Google alone; see "Deferred" below. Nothing here
silently drops a structured-output or image request: there is simply no path
by which a caller could construct one through the current contract, so the
"never silently downgrade" requirement is satisfied by the contract's own
shape rather than by adapter-level rejection code.

## Capability matrix (N02)

`Protocol::GoogleGenerativeAi` and `Protocol::GoogleVertex` share one
declared row keyed off `gemini-` model ids: `tools`, `streaming`, `vision`,
`structured_output` declared `true` (vendor docs describe all four); `
prompt_caching` declared `false` (the explicit `CachedContent` resource has
no per-request representation this transport uses); `reasoning_controls` and
`continuation` declared `true` only for the `gemini-2.5*`/`gemini-3*` family
that actually emits `thinkingConfig`/`thoughtSignature`, `false` for older
ids. As with N07/N08, N02 never reports `Verified` -- these are documentation
declarations, upgraded to verified only by an actual live validation, which
this pass does not run.

`google-vertex` moves from `Support::Planned("N12 (#481)")` to
`Support::Native` in this pass (`provider/mod.rs`); `aws-bedrock` remains the
representative `Planned` provider exercised by
`config::tests::planned_provider_route_names_its_roadmap_step`.

## What is verified, and what is not

Fixture-verified (frozen under `tests/fixtures/provider/google/v1/`, all
authored for this issue -- there is no existing Google fixture corpus to
inherit): a thought-text part with its own `thoughtSignature`, two parallel
`functionCall` parts (one carrying its own signature-only `thoughtSignature`,
proving both `attached_to` cases), cumulative `usageMetadata` including
`thoughtsTokenCount`/`cachedContentTokenCount`, a non-object `functionCall.
args` (malformed), a stream that ends before any `finishReason` (truncated),
a `SAFETY` `finishReason` with partial text and `safetyRatings`, a mid-stream
`RESOURCE_EXHAUSTED` error with `google.rpc.RetryInfo.retryDelay`, a generic
mid-stream `INTERNAL` error, and a plain final-text turn.

Assumed from current primary docs and **not** independently observed live in
this pass: the exact shape and placement of `thoughtSignature` on a
signature-only function-call part versus a visible thought part; that
`streamGenerateContent?alt=sse` emits incremental (not cumulative) parts per
event; that a `functionCall` part always arrives whole in one event; the
presence of `responseId`/`modelVersion` on every chunk (used here as the
request correlator when no response header carries one, since Gemini
documents none); the exact `error.details[].{"@type":...,"retryDelay":...}`
`RetryInfo` shape used for the quota retry hint; and per-model
`thinkingConfig`/effort availability for any specific id beyond the
2.5/3 family split coded here. None of these are fixture-verified against a
real Gemini response -- only against the fixtures this module itself
authored to codify the assumption.

Live evidence: none. The opt-in `live_google_generative_ai_contract` test is
`#[ignore]`d and needs `GEMINI_API_KEY` plus an entitled exact model id in
`ZIRV_GOOGLE_LIVE_MODEL`; ordinary CI never requires credentials and the test
records nothing without them. There is no equivalent live test for the
Vertex profile in this pass (it would additionally need a project, location,
and a way to mint an access token in CI, which is out of scope here). Until
a live run exists for a configured primary role model, **both** Google
routes are **fixture-verified, live pending** -- not supported. N02's
capability table likewise still reports `declared`, never `verified`.

## Deferred

Structured output and inline image input, as described above, need a shared
`ProviderRequest`/`ProviderContent` contract extension that all three direct
adapters would compile against -- not something to bolt onto Google alone.
The Vertex profile's own live contract test, and CI-runnable Vertex token
minting, are left to whenever an operator configures a real Vertex account.
Wiring this adapter into the durable agent loop's route selection was
already done by N09's dispatch in `runtime::native::run_headless`; nothing
further from N09 remains outstanding for Google specifically.
