# Live provider-contract evidence (issue #592)

`provider-live-contract-manifest.json` is the redacted record of whether
each production provider route has actually been exercised against a real
vendor endpoint, as opposed to the fixture- and local-loopback-server
evidence `docs/design/native-parity.md` already covers for every route.

No key in this repository or CI ever touches a real vendor endpoint. Only an
operator, running the ignored `live_*` tests below with their own
credentials, can move a row from `not_collected` to `collected`:

| Route | Test | Required environment |
|---|---|---|
| `anthropic-messages` | `commands::ctx::provider::anthropic::tests::live_anthropic_messages_contract` | `ANTHROPIC_API_KEY`, `ZIRV_ANTHROPIC_LIVE_MODEL` |
| `openai-responses` | `commands::ctx::provider::openai::tests::live_openai_responses_contract` | `OPENAI_API_KEY`, `ZIRV_OPENAI_LIVE_MODEL` |
| `google-developer` | `commands::ctx::provider::google::tests::live_google_generative_ai_contract` | `GEMINI_API_KEY`, `ZIRV_GOOGLE_LIVE_MODEL` |
| `openai-chat-generic` | `commands::ctx::provider::openai_chat::tests::live_openai_compatible_chat_contract` | `ZIRV_OPENAI_COMPATIBLE_BASE_URL`, `ZIRV_OPENAI_COMPATIBLE_API_KEY`, `ZIRV_OPENAI_COMPATIBLE_LIVE_MODEL` |

Each row in the manifest also carries its own exact `operator_command`, so
this table and the file can never drift silently.

## Collecting a row

Run the row's test directly, with its required environment variables set,
for example:

```sh
ANTHROPIC_API_KEY=*** ZIRV_ANTHROPIC_LIVE_MODEL=<exact-model-id> \
  cargo test --bin zirv --release -- --ignored live_anthropic_messages_contract --nocapture
```

The test drives one real single-turn completion through the production
adapter, then `commands::ctx::provider::evidence::record_stream_result`
rewrites that route's row in place with the date, the exact model id, the
behaviors asserted (a message id and non-zero usage), and the outcome
(`pass` or `fail`). A live failure still updates the row -- `fail` is
recorded, not hidden -- and the test itself still fails loudly so the
operator knows the run did not produce passing evidence.

Nothing routed through `record_stream_result` ever receives the credential
itself. A failure is first passed through the calling adapter's own
`redact_failure`, which knows the exact live secret, and any free-text detail
is then run through the generic `commands::ctx::pace::redact_for_log`
heuristic before it is written. That catches a provider or proxy echoing the
key back in its error body in every form the adapter can recognise, but it is
best-effort against an unknown vendor's echo format: before committing a
filled manifest, grep it for your key and for any token-shaped string you do
not recognise.

## Reading the manifest

`collection_status` is the only field that matters for "is this proven":
`not_collected` means exactly what it says, regardless of what else is in
the row. A `collected` row with `outcome: "fail"` is evidence too -- it
means the last real attempt did not succeed, which is different from never
having been attempted.

Repeat vendors within the same protocol family (DeepSeek, xAI, Moonshot,
Mistral, a self-hosted Ollama/vLLM/LM Studio instance, ...) all speak the
`openai-chat-generic` wire shape; the manifest carries one representative
row for that family rather than one per vendor, matching how
`docs/design/native-parity.md` scores the family as a whole.

Bedrock, Azure and the broker-only vendors (Copilot, Factory/Droid) are
intentionally not rows here: they need cloud credentials or a brokered
subscription this tooling does not model, and `docs/design/2026-09-14-
native-release-evidence.md` §3.1 already states that gap plainly rather than
inventing a manifest row nothing here can collect.
