# Specification

## Context

Existing seams (verified 2026-09-17 on `main` @f1f618a2):

- Launch: `chat.rs::resolve_adapter(cfg, requested)` (explicit `--agent` →
  `cfg.agent` → `adapters::resolve_default`), `extra_with_model(cfg, adapter,
  extra)` (prepends `adapter.model_args(cfg.chat.model)`),
  `resolve_initial_prompt` / `orchestrator_initial_prompt` →
  `build_launch(adapter, initial_prompt: Option<&str>, extra)` → `wrap_args_for`.
  `compile.rs::compile()` assembles the injected context; `prompt.rs` adds the
  active workflow step ("zirv workflow step ...") when one is active, so a
  workflow started before spawn is visible to the seat's first turn.
- Deterministic classification: `workflow/classify.rs` (`Intent{Feature,
  Bugfix, Refactor, Spike, Review, Other}`, `Complexity{Trivial, Bounded,
  Substantial, Architectural}`, `RiskBand{Low, Medium, High, Critical}`),
  `workflow/profile.rs::ExecutionProfile{classification, execution:
  ExecutionMode{Direct, Bounded, Orchestrated}, model_tier: ModelTier{Fast,
  Standard, Deep}, validation: ValidationProfile, domains, confidence, reasons}`
  with pure `derive(request_text, classification)`; its module doc names it the
  #537 replacement seam. `team.rs::compile_for_objective` = classify → derive →
  compile → `TeamPlan`. `engine.rs::start_workflow(state_dir, StartArgs{id,
  task, complexity, risk, no_brainstorm, ..})`; `selection::select_definition`
  picks a definition from task text + classification.
- Roster: `catalogue.rs` (`Vendor`, `Rung{alias, id, strength, tier:
  Option<Tier{Cheap, Standard, Deep}>, price}`, `tier_model`,
  `built_in_prices`), `handover.rs::resolve_model(agent, tier_or_model, cfg)`
  (tier → concrete model per harness, operator overrides), `adapters::ADAPTERS`
  × `settings::AgentGate::is_enabled` × `ready()`, headroom from the fallback
  router (`FallbackConfig`, pace/usage).
- Helper model: `helper.rs` `ROLE_*` constants +
  `handoff.rs::helper_answer(role, adapter, model, prompt, timeout)` (native
  first, harness `run_model` fallback); prompt + block-parse precedent in
  `optimize.rs::{judgment_prompt, parse_judgment}`; `tests/fixtures/fake-model.sh`
  driven by `FAKE_MODEL_MODE`.
- HTTP: `ureq = "3"` only. One-shot template `provider/probe.rs::HttpProbe::models`
  (lines 70–141); status → typed failure pattern
  `provider/anthropic.rs::classify_http_error`; fake servers via
  `std::net::TcpListener` in `provider/{anthropic,openai,google}.rs` tests.
- Spend: `log.rs::Delegation` ledger row → `price.rs::price(model, usage,
  table)`; built-in table from `catalogue::built_in_prices()`; no `typesafe`
  vendor exists yet.
- Native: `runtime/mod.rs::native_available() == cfg!(test)` and
  `require_native_available()` guard every native entry; the interactive submit
  loop is `runtime/native.rs::spawn_interactive` (`for text in submit_rx.iter()`);
  `ctx/team.rs::route_for_role` is a flat `NativeConfig.roles` lookup;
  `provider/config.rs::NativeConfig{endpoints, accounts, routes, roles, policy}`
  with `RouteConfig.model`.
- Config: `config.rs::CtxConfig`, `REPO_FORBIDDEN` (each forbidden key paired
  with the operator env var that may still set it), `EndpointTarget{vendor,
  base_url, credential_env, model, wire_api}` as the template for a
  credential-by-env-name target.
- Jev API (docs.typesafe.ai, read 2026-09-17): `POST
  https://api.typesafe.ai/v1/systemone`, `Authorization: Bearer <key>`, JSON
  body `{state: string|object|array, model: "jev-latest", questions: {<id>:
  {type: "choice"|"score"|"noul", instructions, criteria}}}`. Choice
  `criteria` is a map option → description (≤255 options; `null` allowed);
  Score `criteria` is an ordered array of ≥2 level descriptions; Noul
  `criteria` is optional `{true, false}` boundaries. Response `{model, answers:
  {<id>: {type, choice|score|noul, probabilities, confidence, legend}}, usage:
  {input_tokens, output_tokens}}`. Errors 401, 422, 429, 529. Documented
  confidence baseline 0.5–0.6. Price $0.042 per MTok input, output free.
  Early access; `TYPESAFE_API_KEY` is unset on the development machine.

## Goals

- One inspectable decision (`ProxyDecision`) before any provider turn: intent,
  complexity, risk, execution mode, workflow kind, orchestrator harness+model,
  worker tier, decider, per-field confidence, reasons, fallbacks.
- TypeSafe Jev over HTTP as the primary decider; the existing helper-model
  chokepoint as the second; the deterministic classifier and profile as the
  third and as the floor.
- `zirv chat` applies the decision to a wrapped harness launch and starts the
  chosen workflow with the user's prompt as first prompt; the native runtime
  applies the same decision behind its existing gate.
- Disabled by default; when disabled every launch path is byte-identical to today.

## Non-goals

- Enabling the native harness or changing `native_available()`.
- Replacing the deterministic classifier, the team compiler, workflow
  definitions or gates; a second planner.
- Mid-session reclassification or re-routing (only intake plus the monotonic
  floor from #537 step 6).
- A rich chat TUI: v1 reads the request from the terminal; the dashboard and
  native composers are the eventual surface.
- Vendor SDKs, streaming, or retries beyond one bounded HTTP attempt.
- Per-worker concrete model selection beyond a tier.

## Design

### Module layout — `src/commands/ctx/proxy/`

- `mod.rs` — `ProxyArgs` (clap for `zirv ctx proxy`), `pub fn decide(cfg,
  state_dir, repo, request: &str) -> ProxyDecision`, `run()` for the verb,
  `read_request(reader) -> Option<String>` (multi-line until an empty line or
  EOF), `persist(state_dir, &decision)` appending to `proxy-decisions.jsonl`,
  `latest_for_repo(state_dir, repo)`, `announce_line(&decision) -> String`.
- `decision.rs` — `ProxyDecision{request_sha256, intent, complexity, risk,
  execution, validation, workflow: Option<String>, orchestrator: Seat{harness,
  model}, worker_tier: Tier, needs_clarification: f32, decider: Decider,
  confidence: BTreeMap<&'static str, f32>, reasons, fallbacks, elapsed_ms,
  usage: Option<Usage>, created_at}`, `Decider{Typesafe, Helper,
  Deterministic}`, neutral `Question`/`Answers` types shared by both model
  deciders, `IntakeState` (the Jev `state`), `questions(&IntakeState) ->
  Vec<Question>`, `baseline(request, classification, registry, roster) ->
  ProxyDecision`, `merge(baseline, answers, min_confidence) -> ProxyDecision`,
  `validate(&mut decision, &Roster)`.
- `typesafe.rs` — `SystemOneRequest`/`SystemOneResponse` serde types mirroring
  the documented shape, `decide(&TypesafeConfig, &IntakeState, &[Question]) ->
  Result<(Answers, Usage), TypesafeError>` over `ureq` with `timeout_secs`;
  credential read from `credential_env` only; `TypesafeError{NoCredential,
  Auth(401), Invalid(422), RateLimited(429), Overloaded(529), Status(u16),
  Timeout, Transport, Malformed}`.
- `llm.rs` — renders the same questions as a JSON contract prompt, calls
  `handoff::helper_answer(helper::ROLE_PROXY, adapter, model, prompt, timeout)`
  (model = the adapter's distiller default), parses `{"answers": {<id>:
  {"probabilities": {...}}}}` (confidence = max probability), one repair prompt
  on parse failure, then `Err`.
- `native.rs` — `route_for_decision(&ProxyDecision, &NativeConfig) ->
  Option<RouteId>`: the route whose `model` equals the decided model id or
  catalogue alias; `None` keeps the configured role route.

### Decision fields

| Field | Jev question | Deterministic baseline | Merge rule |
|---|---|---|---|
| intent | choice over the six `Intent` variants with descriptions | `classification.intent` | model if confident |
| complexity | score, 4 ordered levels | `classification.complexity` | `max(model, baseline)` |
| risk | score, 4 ordered levels; criteria name auth/security, migration, deploy, public API, concurrency as raising | `classification.risk` | `max(model, baseline)` |
| execution | choice Direct / Bounded / Orchestrated | `ExecutionProfile.execution` | `max` (Direct < Bounded < Orchestrated) |
| workflow | choice over registry ids (+ `none`), each with its definition description | `selection::select_definition`, `none` when Trivial | model if confident and id exists |
| orchestrator seat | choice over `harness/alias` from enabled+ready harnesses × catalogue rungs, described with strength, tier, price and headroom | `resolve_default` + `cfg.chat.model` | model if confident and available |
| worker tier | choice cheap / standard / deep | `ExecutionProfile.model_tier` mapped Fast→Cheap, Standard→Standard, Deep→Deep | model if confident |
| needs_clarification | noul: "too ambiguous to start without one question" | 0.0 | advisory only (announced when > 0.7) |

`validation` is recomputed by `ExecutionProfile::derive` on the merged
complexity and risk, so a raise propagates and nothing is ever lowered.
Per-field confidence below `min_confidence` keeps the baseline value and
records `"<field>: confidence 0.41 < 0.50, kept baseline"`.

### `IntakeState` (the Jev `state`, ≤ `request_max_bytes`, no file contents)

```json
{
  "request": "<user prompt, truncated to request_max_bytes>",
  "repository": {"name": "...", "changed_files": 2, "changed_lines": 111,
                 "active_workflow": null, "primary_extensions": ["rs", "md"]},
  "harnesses": [{"name": "claude", "ready": true, "headroom_pct": 92,
                 "models": [{"alias": "fable", "tier": "orchestrator", "strength": 4,
                             "input_usd_per_mtok": 15.0}, ...]}],
  "workflows": [{"id": "feature", "description": "Capture intent, ..."}, ...],
  "policy": {"native_available": false}
}
```

### Decider chain

`decide()` always computes `baseline` first (pure apart from the classifier's
git measurement). Starting at `cfg.proxy.decider`, exactly one model decider
runs: `typesafe` (skipped with fallback "credential env TYPESAFE_API_KEY
unset" when absent; any `TypesafeError` falls through) → `helper` (skipped
when no adapter is ready or `helper_answer` fails) → `deterministic` (baseline
as is). The winner's `Answers` are merged over the baseline, then `validate`
rejects a harness that is not enabled/ready, a model alias absent from that
harness's vendor catalogue, or a workflow id absent from the registry, each
with a recorded reason. The decision records `decider`, `fallbacks`,
`elapsed_ms` and `usage`, is appended to `proxy-decisions.jsonl`, and a
`log::Delegation` row (agent `typesafe`, model `jev-latest`, input tokens from
`usage`, outcome `ok`) is written so `zirv ctx spend` prices the call through
a new `catalogue` vendor `typesafe` (`Rung{alias: "jev", id: "jev-latest",
tier: Cheap, price: input 42_000 µUSD/MTok, output 0}`).

### Apply — wrapped harness (`chat.rs`)

Activation (operator decision 2026-09-17): the proxy takes over a launch —
opening the intake view first — only when `cfg.proxy.enabled` is true AND the
configured decider has a usable model: `typesafe` needs a non-empty
`proxy.typesafe.model` and the env var named by `credential_env` set;
`helper` needs a resolvable default adapter; `deterministic` never takes over.
`proxy::activation(cfg) -> Result<(), String>` encodes this; on `Err` the
launch proceeds exactly as today (the full orchestrator harness) after one
`zirv ▸` advisory line carrying the reason, e.g. `proxy: enabled but
TYPESAFE_API_KEY is unset; starting the orchestrator harness`. The per-launch
`--proxy` / `--no-proxy` flags override `enabled` only; `--resume` (the
handoff owns the first prompt) and `--simple` always skip the proxy. The
runtime fallback chain inside `decide()` is unaffected by this predicate.
Sequence, inserted before `resolve_adapter`:

1. `request` = the `--prompt`-style positional if given, else `read_request`
   from the terminal after one `zirv ▸ proxy: describe the task (empty line to
   send)` line; a non-tty stdin without a request refuses with a clear error.
2. `decision = proxy::decide(...)`; one `zirv ▸` line via `announce_line`,
   e.g. `proxy: orchestrated · claude/fable · workers standard · workflow
   feature (substantial/medium) · typesafe 0.81`.
3. `requested_agent = Some(decision.orchestrator.harness)`; the decided model
   replaces `cfg.chat.model` in `extra_with_model`; `SEAT_MODEL_ENV` follows.
4. If `decision.workflow` is `Some` and the repo has no active workflow,
   `engine::start_workflow(StartArgs{id, task: request, complexity, risk,
   no_brainstorm: false, ..})` runs immediately before spawn; an existing
   active workflow is kept and recorded as a fallback; a spawn failure closes
   the just-started workflow with reason `proxy launch failed`.
5. `initial_prompt = Some(request)` flows through `orchestrator_initial_prompt`
   → `build_launch` unchanged. A bounded `[zirv proxy]` layer (≤ 6 lines:
   execution, seats, workflow, "do this work in this seat; delegate only for a
   distinct need" for Direct/Bounded) joins the compiled context. Operator
   layers still win; the proxy advises, it does not override `~/.zirv`.

Bare `zirv` and `zirv chat` keep today's routing; the proxy is opt-in.

### Apply — native runtime (behind the gate, tests only)

In `spawn_interactive`'s submit loop, when the session journal has no prior
turn and the proxy is active: `decide` → the same workflow start →
`native::route_for_decision` overrides this session's orchestrator route when
it returns `Some`, else the configured role route stays and a fallback is
recorded. The insertion is one guarded call; `require_native_available()`,
`NATIVE_COMING_SOON` and the `zirv chat --runtime native` refusal are untouched.

### CLI and configuration

- `zirv ctx proxy [--json] [REQUEST]` — decide and print, never launch. Reads
  `REQUEST` from stdin when absent and stdin is not a tty. Human output: the
  announce line plus one line per field with source and confidence, then
  reasons and fallbacks. `--json` prints the `ProxyDecision`.
- `zirv ctx chat --proxy | --no-proxy` per-launch override.
- `~/.zirv/ctx.toml`:

  ```toml
  [proxy]
  enabled = false              # ZIRV_CTX_PROXY_ENABLED
  decider = "typesafe"         # typesafe | helper | deterministic; ZIRV_CTX_PROXY_DECIDER
  min_confidence = 0.5         # ZIRV_CTX_PROXY_MIN_CONFIDENCE
  request_max_bytes = 16384    # ZIRV_CTX_PROXY_REQUEST_MAX_BYTES

  [proxy.typesafe]
  base_url = "https://api.typesafe.ai/v1"   # ZIRV_CTX_PROXY_TYPESAFE_BASE_URL
  credential_env = "TYPESAFE_API_KEY"       # ZIRV_CTX_PROXY_TYPESAFE_CREDENTIAL_ENV
  model = "jev-latest"                      # ZIRV_CTX_PROXY_TYPESAFE_MODEL
  timeout_secs = 10                         # ZIRV_CTX_PROXY_TYPESAFE_TIMEOUT_SECS
  ```

  Every `proxy.*` key joins `REPO_FORBIDDEN` with its env pair. `deny_unknown_fields`.
- `helper.rs`: `pub const ROLE_PROXY: &str = "proxy"`.
- `status.rs`: a `proxy:` line — `off`, or the latest decision for this repo
  rendered by `announce_line`.
- README: "### Harness proxy" beside the `zirv chat` section; `[proxy]` in the
  configuration example and the trust-boundary table; `zirv ctx proxy` in the
  verbs table; a `typesafe` row in the model catalogue table.

## Testing strategy

- `typesafe.rs`: serde round-trip of the request against the documented shape
  and of the documented response example stored verbatim as
  `tests/fixtures/proxy/jev-response.json`; a `TcpListener` fake server: 200 →
  `Answers`, 401/422/429/529 → the matching `TypesafeError`, a stalled server →
  `Timeout` within `timeout_secs`; unset credential env → `NoCredential` with
  no connection attempted.
- `decision.rs`: merge table (model lower than baseline → baseline kept; model
  higher → raised; confidence below floor → baseline with reason);
  `validate` rejects a disabled harness, an unknown alias and an unknown
  workflow id, each falling to baseline with a reason; `questions` never
  exceeds 255 options and always includes `none`/`other`.
- `llm.rs`: contract parse, one repair, garbage → `Err`, through new
  `fake-model.sh` modes `proxy` and `proxy_garbage`.
- Battery `tests/fixtures/proxy/battery.json`: at least eight requests (typo
  fix, README wording, bug with stack trace, feature, architecture redesign,
  a "one-line" auth change, a question, a mixed-language prompt) with expected
  baseline execution, complexity floor and workflow; the test runs the
  deterministic decider only.
- `chat.rs`: proxy disabled → argv identical to today (existing tests
  unchanged); enabled with an injected decision → decided `--model` and adapter
  in argv and the request as initial prompt; `--resume` with proxy → skipped
  with reason; workflow started once and closed on spawn failure.
- `config.rs`: `[proxy]` in a repository `ctx.toml` → `REPO_FORBIDDEN` error;
  env overrides parse; defaults match the table above.
- Native: `route_for_decision` match / no-match; `native_available()` test and
  the `--runtime native` refusal test unchanged and green.
- `status.rs`: `proxy:` line for `off` and for a stored decision.
- Gates before the PR: `cargo build`, `cargo nextest run --no-fail-fast`,
  `cargo fmt -- --check`, `cargo clippy --all-targets -- -D warnings`; failure
  names diffed against a same-environment run on `main`.
- Optional operator smoke test when a key exists: `zirv ctx proxy --json "fix
  the typo in README"` and one substantial request, recorded in the PR.

## Risks

- No Jev credential on the development machine: the live path is verified
  only against the documented schema and a fake server. Mitigation: the
  fixture is the docs' example verbatim, error mapping follows the docs, and
  the chain guarantees a launch without Jev.
- Jev's accuracy on this taxonomy is unknown: per-field confidence gating,
  the monotonic floor, the full fallback chain and the committed battery bound
  the damage; every decision is logged for later tuning.
- Cost and latency on every launch: one bounded call (≤16 KiB, 10 s timeout),
  off by default.
- Repository text steering the decision: the state carries names and counts,
  never file contents; the decision is validated against the enabled roster;
  every `proxy.*` key is operator-only.
- Orphaned workflow on a failed spawn: the workflow starts immediately before
  spawn and is closed with a reason if the spawn fails.
- Touching `native.rs`: the change is one guarded call; the gate, its
  constant and the refusal tests are untouched and re-run.
