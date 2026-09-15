# Native route profiles: compatible endpoints, local models and cloud routes (N13)

**Date:** 2026-09-13 · **Issue:** #482 · **Roadmap:** #469

## Context

N07, N08 and N12 gave the native runtime three direct transports (Anthropic
Messages, OpenAI Responses, Google Gemini). The catalogue, however, already
names twelve more vendor families plus three local runtimes, and N02's
inventory could *recognize* those routes without anything being able to
*run* them. The gap this step closes is not "one more adapter": it is that a
route's address, credential class, capability set and permitted request
options were nowhere stated, so every compatible endpoint looked
interchangeable with OpenAI's.

## Decision

### 1. A versioned route-profile registry is the unit of extension

`provider/profiles.rs` holds one `RouteProfile` row per supported route:
protocol, documented base URL (or "the operator's own host"), request path,
credential *class*, capability caveats, and a typed extension allow-list.
`PROFILE_SCHEMA` versions the row shape; each row carries its own `version`
for when a vendor's own facts change.

A `ProviderSpec` still answers "which wire protocol and which auth header".
The profile answers everything a concrete vendor route additionally raises.
Binding is `(provider, endpoint vendor)`, so `openai-compatible` +
`deepseek` selects `deepseek-chat` while an unknown vendor falls back to
`openai-chat-generic` — and a fixed-vendor provider has no fallback at all,
because "anthropic + deepseek" is a configuration mistake, not a new route.

**Adding a model or endpoint** is therefore a row plus, only when the wire
protocol itself is new, a transport module. Nothing in the agent loop, the
tool layer or the UI changes. **Retiring one** removes the row, its fixture
directory and its compatibility-suite coverage in the same commit.

### 2. `Support::LegacyOnly` is an upstream fact, never a zirv gap

`Support` now has three states. `Planned(tracking)` means zirv has not
written that adapter; `LegacyOnly(reason)` means upstream publishes no
documented, separately authorized direct API. Copilot and Factory/Droid are
`LegacyOnly` with their reasons spelled out: they are brokered subscription
identities, and their honest home is the harness backend. Configuring such a
route is refused at load time with that reason, and the inventory reports
the two states separately so an unimplemented adapter can never masquerade
as an entitlement limit.

### 3. One chat-completions transport, many profiles

`provider/openai_chat.rs` implements `chat.completions` SSE with `tool_calls`
deltas, mirroring `openai.rs`'s structure and sharing `provider::transport`'s
supervisor, deadlines, cancellation and failure classes. It serves DeepSeek,
xAI, Qwen, Moonshot, Mistral, Zhipu, MiniMax, Meta, Ollama, LM Studio, vLLM,
any operator-declared compatible endpoint, and Azure OpenAI.

Azure is a `ChatEndpoint::Azure` variant, not a base-URL substitution: the
path is built from a deployment id, `api-version` is mandatory, and the key
rides `api-key` rather than `authorization`. The request carries no `model`
field, because on Azure the deployment names the model.

Two deliberate non-features:

- **Reasoning is display-only.** `reasoning_content` arrives without a
  signature, so it is streamed as `ThinkingDelta` for the operator to watch
  and never becomes a `Thinking` content block. Replaying unsigned reasoning
  as continuation state would be inventing provider state.
- **Nothing is silently dropped.** A request that asks for tools, reasoning
  effort, a thinking configuration or prompt caching the bound profile does
  not declare is refused with a typed `Configuration` failure before a socket
  opens.

### 4. Local runtimes are a credential class, not a URL shape

`CredentialClass::LocalNone` / `LocalOptional` say a route legitimately runs
without a key, so nothing is fabricated for Ollama or LM Studio, and
declaring a credential on such a route is an error rather than a secret sent
to a local server. Ollama is reached through its OpenAI-compatible `/v1`
surface rather than `/api/chat`: that is the documented compatibility layer
and needs no second parser.

Plaintext HTTP is allowed only to a loopback or private address
(`probe::is_local_http_host`, which trusts literal addresses only — a
hostname that resolves privately today says nothing about tomorrow). The
older, stricter rule still stands on top of it: a credential is never sent
in the clear to a non-loopback host.

### 5. Bedrock signs; it does not just point elsewhere

`provider/aws_sigv4.rs` implements SigV4 over `sha2` (already a dependency).
An AWS SDK would add a large async surface for one header. HMAC-SHA256 is
pinned to RFC 4231's published vectors and the canonical-URI double-encoding
rule for non-S3 services has its own test, because getting it wrong is
invisible locally and fatal against the real endpoint.

`provider/bedrock.rs` speaks **Converse** only. Converse is Bedrock's own
vendor-neutral body, so Amazon Nova and the Bedrock-hosted Anthropic, Meta
and Mistral families share one parser; a second `invoke-with-response-stream`
dialect would duplicate it. What differs per family is what the route may
ask for, and that is what the profile's caveats carry: the
Anthropic-on-Bedrock profile declares replayable reasoning (Converse returns
a reasoning signature there) and the generic profile does not. Responses
arrive as AWS event-stream binary frames, decoded into the same
provider-neutral `ProviderResponse` every other adapter produces.

A Bedrock credential is a small JSON object (`access_key_id`,
`secret_access_key`, optional `session_token`): SigV4 needs a key pair, which
one opaque secret string cannot carry. The region is an account field,
because a signature is region-scoped.

### 6. Provider-native options are a typed allow-list

`[route.<id>.extensions]` carries provider-native request options, validated
at config load against the bound profile's `ExtensionSpec` list: unknown key,
wrong type or out-of-range value is a load error naming the accepted keys.
There is no free-form passthrough, and an extension may not overwrite a
protocol-owned body field.

## What is verified

- `profiles::tests` — every catalogue vendor binds to a profile or states an
  upstream limitation; brokers are `LegacyOnly` with no vendor credential;
  local profiles never require a key and default to loopback; extensions are
  an allow-list, per profile, typed and range-checked.
- `config::tests` — Bedrock needs a vendor and a region, Azure needs
  `api_version` and a per-route `deployment`, a broker vendor is refused with
  its upstream reason, a documented base URL need not be retyped, plaintext
  is refused to a public host, and a local route may not carry a credential.
- `capability::tests` — a compatible route declares its *vendor profile*, not
  its protocol.
- `openai_chat::tests` — fixture-driven stream reassembly, display-only
  reasoning, malformed/truncated/length-stopped streams, typed HTTP and
  stream errors, and three live-loopback-server proofs: the vendor path and
  bearer key, a DeepSeek multi-turn tool task with no harness process, and a
  local endpoint completing a turn with no `authorization` header at all.
  Azure's deployment address and `api-key` header have their own test.
- `bedrock::tests` — real event-stream frames built from reviewable fixture
  rows; signed reasoning replays, unsigned reasoning does not; SigV4 headers,
  the double-encoded model id, and a multi-turn task that replays its
  signature and tool results.
- `aws_sigv4::tests` — RFC 4231 HMAC vectors, the AWS test-suite scope and
  authorization shape, determinism, region scoping, and credential parsing.
- `runtime::native::tests::every_route_profile_shape_completes_the_
  investigate_edit_test_script` — the compatibility suite: the same
  investigate/edit/test script drives the loop to the same effects through
  every native profile's adapter shape, and the table is checked for
  completeness against the registry itself.

## What is deferred, and why

- **No live cloud validation.** There is no AWS, Azure or vendor account in
  this environment, so every route's evidence is fixture- and
  local-HTTP-server-based. SigV4's primitives are pinned to published
  vectors, but a real Bedrock 200 is unproven; the inventory's ladder still
  stops short of `validated` for exactly this reason.
- **Bedrock `invoke-with-response-stream`** is not implemented. Converse
  covers the same models; a model-specific dialect would be added only if a
  capability turns out to be reachable through it and not through Converse.
- **Bedrock `cachePoint` blocks and Azure `max_completion_tokens`** are not
  emitted. Both are per-family refinements of a body that already works; the
  request validator refuses what it cannot express rather than guessing.
- **Images.** Several profiles declare vision, but the provider-neutral
  request model carries no image content block yet, so nothing can ask for
  it. The declaration is a vendor fact waiting on the neutral model, not a
  silently dropped capability.
