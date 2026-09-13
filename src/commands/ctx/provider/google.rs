//! Direct Google Gemini transport (issue #481, roadmap N12).
//!
//! This is raw HTTPS/SSE against `POST .../models/{model}:streamGenerateContent
//! ?alt=sse`, not the Gemini CLI. Zirv owns request construction, stream
//! accumulation, tool execution, conversation state, cancellation, typed
//! failures, and the opaque `thoughtSignature` continuation metadata.
//!
//! Two explicitly versioned protocol profiles share this one transport,
//! selected by [`Protocol`]:
//!
//! - [`GoogleProfile::Developer`] (`Protocol::GoogleGenerativeAi`): the Gemini
//!   Developer API at `generativelanguage.googleapis.com`, authenticated with
//!   an API key (`x-goog-api-key`), addressed as `v1beta/models/{model}` --
//!   `v1beta` is where thinking and function-calling controls live.
//! - [`GoogleProfile::Vertex`] (`Protocol::GoogleVertex`): Vertex AI,
//!   authenticated with an OAuth bearer access token supplied by the
//!   credential contract, addressed by an explicit project and location
//!   (`v1/projects/{project}/locations/{location}/publishers/google/models/{model}`).
//!   Token *acquisition* (e.g. `gcloud auth print-access-token`) is a
//!   separate credential-class concern; this transport only ever carries an
//!   already-resolved bearer token.
//!
//! The two profiles share request/response shapes but never share a schema:
//! [`GoogleAdapter::request_url`] and the header each profile sends are the
//! only points where they diverge. See
//! `docs/design/2026-09-13-native-google-provider.md` for which shapes are
//! fixture-verified here versus assumed from current primary docs.

#![allow(dead_code)] // N09 wires direct providers into the persistent runtime loop.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader};

use serde_json::{Map, Value, json};

use super::adapter::{
    CacheMode, Cancellation, EventSink, FailureClass, FailureScope, FailureScopeKind, FinishReason,
    ProviderAdapter, ProviderContent, ProviderFailure, ProviderMessageRole, ProviderRequest,
    ProviderResponse, ProviderStreamEvent, ProviderTarget, ProviderUsage, RetryHint,
    ThinkingConfig, ThinkingDisplay, resolve_target,
};
use super::config::NativeConfig;
use super::credential::{Credential, CredentialStore};
use super::probe::is_plaintext_non_loopback;
use super::transport::{
    MAX_ERROR_BODY_BYTES, StreamTimeouts, WORKER_READ_POLL, parse_retry_after_ms, read_sse_line,
    supervise, target_scope,
};
use super::{OpaqueProviderData, Protocol, RouteId};
use crate::commands::ctx::config::EnvLookup;

/// The direct providers share one timeout contract; the alias keeps the
/// Google call sites reading as Google ones.
pub type GoogleTimeouts = StreamTimeouts;

/// Which explicitly versioned protocol profile a [`GoogleAdapter`] speaks.
/// Adding a profile means: a new `Protocol` variant in `provider/mod.rs`, a
/// new arm here for its URL/auth, and a fixture set proving its own request
/// and response shape -- never widening an existing profile's schema to
/// "maybe fit" a second product. Retiring one means reversing that: remove
/// its arm, its `Protocol` variant, and its fixtures together so an orphaned
/// profile can never be silently selected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoogleProfile {
    /// The Gemini Developer API: API-key credential, no project/location.
    Developer,
    /// Vertex AI: OAuth access-token credential, addressed by project and
    /// location.
    Vertex { project: String, location: String },
}

#[derive(Clone)]
pub struct GoogleAdapter {
    target: ProviderTarget,
    credential: Credential,
    timeouts: GoogleTimeouts,
    profile: GoogleProfile,
}

impl std::fmt::Debug for GoogleAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleAdapter")
            .field("target", &self.target)
            .field("credential", &"[redacted]")
            .field("timeouts", &self.timeouts)
            .field("profile", &self.profile)
            .finish()
    }
}

impl GoogleAdapter {
    pub fn from_config(
        config: &NativeConfig,
        route: &RouteId,
        env: EnvLookup<'_>,
        store: &dyn CredentialStore,
        now: u64,
        timeouts: GoogleTimeouts,
    ) -> Result<Self, ProviderFailure> {
        let (target, credential) = resolve_target(config, route, env, store, now)?;
        let profile = match target.protocol {
            Protocol::GoogleGenerativeAi => GoogleProfile::Developer,
            Protocol::GoogleVertex => {
                let route_cfg = config
                    .routes
                    .get(route)
                    .ok_or_else(|| config_error(format!("unknown native route `{route}`")))?;
                let account_cfg = config.accounts.get(&route_cfg.account).ok_or_else(|| {
                    config_error(format!(
                        "route `{route}` references unknown account `{}`",
                        route_cfg.account
                    ))
                })?;
                let project = account_cfg.project.clone().ok_or_else(|| {
                    config_error(format!(
                        "account `{}` has no `project` for the google-vertex profile",
                        route_cfg.account
                    ))
                })?;
                let location = account_cfg.location.clone().ok_or_else(|| {
                    config_error(format!(
                        "account `{}` has no `location` for the google-vertex profile",
                        route_cfg.account
                    ))
                })?;
                GoogleProfile::Vertex { project, location }
            }
            other => {
                return Err(config_error(format!(
                    "route `{route}` uses {other:?}, not a Google protocol"
                )));
            }
        };
        let credential = credential.ok_or_else(|| {
            ProviderFailure::new(
                FailureClass::Authentication,
                FailureScope {
                    kind: FailureScopeKind::Account,
                    id: Some(target.account.to_string()),
                },
                format!("Google account `{}` has no API credential", target.account),
            )
        })?;
        Self::new(target, credential, profile, timeouts)
    }

    pub fn new(
        target: ProviderTarget,
        credential: Credential,
        profile: GoogleProfile,
        timeouts: GoogleTimeouts,
    ) -> Result<Self, ProviderFailure> {
        match (target.protocol, &profile) {
            (Protocol::GoogleGenerativeAi, GoogleProfile::Developer) => {}
            (Protocol::GoogleVertex, GoogleProfile::Vertex { .. }) => {}
            _ => {
                return Err(config_error(
                    "Google adapter target protocol does not match its profile".into(),
                ));
            }
        }
        if is_plaintext_non_loopback(&target.base_url) {
            return Err(ProviderFailure::new(
                FailureClass::Configuration,
                FailureScope {
                    kind: FailureScopeKind::Endpoint,
                    id: Some(target.endpoint.to_string()),
                },
                "Google credentials cannot be sent over plaintext HTTP to a non-loopback host",
            ));
        }
        if timeouts.has_zero() {
            return Err(config_error(
                "Google timeouts must be greater than zero".into(),
            ));
        }
        reject_gemini_cli_credential(credential.secret.expose(), &target, &profile)?;
        Ok(Self {
            target,
            credential,
            timeouts,
            profile,
        })
    }

    fn request_url(&self) -> String {
        let base = self.target.base_url.trim_end_matches('/');
        let model = &self.target.model.id;
        match &self.profile {
            GoogleProfile::Developer => {
                format!("{base}/v1beta/models/{model}:streamGenerateContent?alt=sse")
            }
            GoogleProfile::Vertex { project, location } => format!(
                "{base}/v1/projects/{project}/locations/{location}/publishers/google/models/{model}:streamGenerateContent?alt=sse"
            ),
        }
    }

    fn encode_request(&self, request: &ProviderRequest) -> Result<EncodedRequest, ProviderFailure> {
        validate_request(request, &self.target)?;
        let mut body = Map::new();
        body.insert("contents".into(), Value::Array(encode_contents(request)?));
        if !request.system.is_empty() {
            body.insert(
                "systemInstruction".into(),
                json!({"parts": [{"text": request.system.join("\n\n")}]}),
            );
        }
        if !request.tools.is_empty() {
            let declarations: Vec<Value> = request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    })
                })
                .collect();
            body.insert(
                "tools".into(),
                json!([{"functionDeclarations": declarations}]),
            );
        }
        let mut generation_config = Map::new();
        generation_config.insert("maxOutputTokens".into(), json!(request.max_output_tokens));
        if !request.stop_sequences.is_empty() {
            generation_config.insert("stopSequences".into(), json!(request.stop_sequences));
        }
        if let Some(thinking_config) = encode_thinking_config(&request.thinking) {
            generation_config.insert("thinkingConfig".into(), thinking_config);
        }
        body.insert("generationConfig".into(), Value::Object(generation_config));
        Ok(EncodedRequest {
            body: Value::Object(body),
        })
    }

    fn perform_blocking(
        &self,
        encoded: &EncodedRequest,
        cancellation: &dyn Cancellation,
        sink: &mut dyn EventSink,
    ) -> Result<ProviderResponse, ProviderFailure> {
        if cancellation.is_cancelled() {
            return Err(cancelled());
        }
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(self.timeouts.connect))
            .timeout_recv_response(Some(self.timeouts.first_event))
            .timeout_recv_body(Some(WORKER_READ_POLL))
            .build()
            .into();
        let payload = serde_json::to_string(&encoded.body).map_err(|error| {
            ProviderFailure::new(
                FailureClass::Configuration,
                FailureScope::request(),
                format!("failed to encode Google request: {error}"),
            )
        })?;
        let mut http = agent
            .post(self.request_url())
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header("user-agent", format!("zirv/{}", env!("CARGO_PKG_VERSION")));
        http = match &self.profile {
            GoogleProfile::Developer => {
                http.header("x-goog-api-key", self.credential.secret.expose())
            }
            GoogleProfile::Vertex { .. } => http.header(
                "authorization",
                format!("Bearer {}", self.credential.secret.expose()),
            ),
        };
        let mut response = http
            .send(payload)
            .map_err(|error| classify_transport_error(error, false, &self.target))?;
        let status = response.status().as_u16();
        // The Gemini API documents no per-request correlation header; when
        // one is absent the response's own `responseId` is used instead
        // (assumed -- see the design note).
        let request_id = response
            .headers()
            .get("x-goog-request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(parse_retry_after_ms);
        if status >= 400 {
            let body = response
                .body_mut()
                .with_config()
                .limit(MAX_ERROR_BODY_BYTES)
                .read_to_string()
                .unwrap_or_else(|_| "Google returned an unreadable error body".into());
            return Err(classify_http_error(
                status,
                &body,
                request_id,
                retry_after,
                &self.target,
            ));
        }
        let reader = BufReader::new(response.into_body().into_reader());
        parse_sse(reader, request_id, cancellation, sink, &self.target)
    }

    fn perform(
        &self,
        encoded: &EncodedRequest,
        cancellation: &dyn Cancellation,
        sink: &mut dyn EventSink,
    ) -> Result<ProviderResponse, ProviderFailure> {
        let adapter = self.clone();
        let encoded = encoded.clone();
        supervise(
            "Google",
            "zirv-google-stream",
            self.timeouts,
            &self.target,
            cancellation,
            sink,
            move |worker_cancellation, worker_sink| {
                adapter.perform_blocking(&encoded, worker_cancellation, worker_sink)
            },
        )
    }
}

impl ProviderAdapter for GoogleAdapter {
    fn protocol(&self) -> Protocol {
        self.target.protocol
    }

    fn target(&self) -> &ProviderTarget {
        &self.target
    }

    fn stream(
        &self,
        request: &ProviderRequest,
        cancellation: &dyn Cancellation,
        sink: &mut dyn EventSink,
    ) -> Result<ProviderResponse, ProviderFailure> {
        let encoded = self.encode_request(request)?;
        self.perform(&encoded, cancellation, sink)
    }
}

#[derive(Clone)]
struct EncodedRequest {
    body: Value,
}

// -- Credentials ---------------------------------------------------------

/// A Gemini CLI login (the OAuth desktop flow that populates `~/.gemini`) is
/// never accepted for either profile: it establishes no API product,
/// project, quota or permission a direct client can rely on. The credential
/// contract already refuses the `~/.gemini/oauth_creds.json` path outright
/// (see `credential::is_harness_login_path`); this shape check is the second,
/// independent layer -- the same posture as OpenAI's subscription refusal --
/// so a copy-pasted token or a re-pointed file still cannot slip through.
fn reject_gemini_cli_credential(
    secret: &str,
    target: &ProviderTarget,
    profile: &GoogleProfile,
) -> Result<(), ProviderFailure> {
    let account_scope = || FailureScope {
        kind: FailureScopeKind::Account,
        id: Some(target.account.to_string()),
    };
    let trimmed = secret.trim();
    if trimmed.is_empty() {
        return Err(ProviderFailure::new(
            FailureClass::Authentication,
            account_scope(),
            format!(
                "Google account `{}` has an empty credential",
                target.account
            ),
        ));
    }
    let cli_shaped =
        trimmed.starts_with('{') || trimmed.to_ascii_lowercase().starts_with("bearer ");
    if cli_shaped {
        let expected = match profile {
            GoogleProfile::Developer => "a `GEMINI_API_KEY` from ai.google.dev",
            GoogleProfile::Vertex { .. } => {
                "a Vertex AI access token (e.g. `gcloud auth print-access-token`)"
            }
        };
        return Err(ProviderFailure::new(
            FailureClass::Entitlement,
            account_scope(),
            format!(
                "Google account `{}` is configured with a Gemini CLI OAuth login (a \
                 `~/.gemini` desktop-flow token or an `oauth_creds.json`-shaped blob), not an \
                 API credential; point the account credential at {expected} -- CLI entitlements \
                 are never substituted for API billing",
                target.account
            ),
        ));
    }
    Ok(())
}

// -- Local validation ----------------------------------------------------

/// Gemini 2.5 and later expose `thinkingConfig` (budget, `includeThoughts`)
/// and `thoughtSignature` continuation; 1.5/2.0 models do not.
fn thinking_supported(model: &str) -> bool {
    let id = model.to_ascii_lowercase();
    id.starts_with("gemini-2.5") || id.starts_with("gemini-3")
}

fn config_error(message: String) -> ProviderFailure {
    ProviderFailure::new(
        FailureClass::Configuration,
        FailureScope::request(),
        message,
    )
}

fn declared_context_window(vendor: &str, model: &str) -> Option<u64> {
    crate::commands::ctx::catalogue::vendor(vendor)
        .and_then(|vendor| crate::commands::ctx::catalogue::context_window(vendor, Some(model)))
}

fn validate_request(
    request: &ProviderRequest,
    target: &ProviderTarget,
) -> Result<(), ProviderFailure> {
    if request.model != target.model.id {
        return Err(config_error(format!(
            "request model `{}` does not exactly match route model `{}`",
            request.model, target.model.id
        )));
    }
    if request.max_output_tokens == 0 {
        return Err(config_error(
            "max_output_tokens must be greater than zero".into(),
        ));
    }
    if let Some(window) = declared_context_window(&target.model.vendor, &request.model)
        && request.max_output_tokens >= window
    {
        return Err(config_error(format!(
            "max_output_tokens {} does not fit the declared {window}-token context window of `{}`",
            request.max_output_tokens, request.model
        )));
    }
    if request.messages.is_empty() {
        return Err(config_error(
            "Gemini generateContent requests require at least one content turn".into(),
        ));
    }
    if request.stop_sequences.len() > 5 {
        return Err(config_error(
            "Gemini accepts at most 5 stop sequences".into(),
        ));
    }
    if request.effort.is_some() {
        return Err(config_error(
            "Gemini has no reasoning-effort parameter; use thinkingConfig.thinkingBudget instead"
                .into(),
        ));
    }
    if request.cache != CacheMode::Disabled {
        return Err(config_error(
            "Gemini context caching is an explicit CachedContent resource, not a per-request \
             cache_control flag"
                .into(),
        ));
    }
    for tool in &request.tools {
        if !tool.input_schema.is_object() {
            return Err(config_error(format!(
                "tool `{}` input_schema must be a JSON object",
                tool.name
            )));
        }
    }
    validate_thinking_controls(request)?;
    validate_content_relationships(request)?;
    Ok(())
}

fn validate_thinking_controls(request: &ProviderRequest) -> Result<(), ProviderFailure> {
    let supported = thinking_supported(&request.model);
    let display = match request.thinking {
        ThinkingConfig::Adaptive { display } => display,
        ThinkingConfig::Enabled { display, .. } => display,
        ThinkingConfig::Default | ThinkingConfig::Disabled => None,
    };
    if display == Some(ThinkingDisplay::Updates) {
        return Err(config_error(
            "Gemini has no streamed thinking-display-updates mode; use `summarized` or `omitted`"
                .into(),
        ));
    }
    if !supported {
        return match request.thinking {
            ThinkingConfig::Default => Ok(()),
            _ => Err(config_error(format!(
                "model `{}` does not declare thinkingConfig support",
                request.model
            ))),
        };
    }
    let model = request.model.to_ascii_lowercase();
    match request.thinking {
        ThinkingConfig::Disabled if model.contains("pro") => {
            return Err(config_error(format!(
                "model `{}` cannot disable thinking; route a flash/flash-lite model instead",
                request.model
            )));
        }
        ThinkingConfig::Enabled {
            budget_tokens,
            interleaved,
            ..
        } => {
            if interleaved {
                return Err(config_error(
                    "Gemini has no interleaved-thinking mode".into(),
                ));
            }
            if budget_tokens == 0 {
                return Err(config_error(
                    "manual thinkingBudget must be greater than zero; use `disabled` to turn \
                     thinking off"
                        .into(),
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

/// The opaque half of a thought: `{"type":"gemini_thought_signature",
/// "attached_to":"thought"|"function_call","thought_signature":"..."}`, kept
/// verbatim so the next turn can replay it. `attached_to` records whether the
/// signature belongs to a visible thought-text part (its own wire part) or a
/// signature-only thought riding on the function-call part that follows it.
fn thought_signature_envelope(data: &OpaqueProviderData) -> Option<(String, String)> {
    let object = data.expose().as_object()?;
    if object.get("type").and_then(Value::as_str) != Some("gemini_thought_signature") {
        return None;
    }
    let attached_to = object
        .get("attached_to")
        .and_then(Value::as_str)?
        .to_string();
    let signature = object
        .get("thought_signature")
        .and_then(Value::as_str)?
        .to_string();
    Some((attached_to, signature))
}

fn validate_content_relationships(request: &ProviderRequest) -> Result<(), ProviderFailure> {
    let mut pending = BTreeSet::new();
    for message in &request.messages {
        let has_results = message
            .content
            .iter()
            .any(|block| matches!(block, ProviderContent::ToolResult { .. }));
        if !pending.is_empty() && !has_results {
            return Err(config_error(
                "assistant functionCall parts must be followed immediately by their \
                 functionResponse parts"
                    .into(),
            ));
        }
        if has_results
            && (message.role != ProviderMessageRole::User
                || message
                    .content
                    .iter()
                    .any(|block| !matches!(block, ProviderContent::ToolResult { .. })))
        {
            return Err(config_error(
                "tool-result continuation messages must be user messages containing only \
                 functionResponse parts"
                    .into(),
            ));
        }
        for (index, block) in message.content.iter().enumerate() {
            match block {
                ProviderContent::ToolUse { id, input, .. } => {
                    if message.role != ProviderMessageRole::Assistant
                        || id.is_empty()
                        || !input.is_object()
                    {
                        return Err(ProviderFailure::new(
                            FailureClass::InvalidToolArguments,
                            FailureScope::request(),
                            "assistant functionCall args must be one complete JSON object",
                        ));
                    }
                    if !pending.insert(id.clone()) {
                        return Err(config_error(format!(
                            "duplicate unresolved functionCall id `{id}`"
                        )));
                    }
                }
                ProviderContent::ToolResult { tool_use_id, .. } => {
                    if !pending.remove(tool_use_id) {
                        return Err(config_error(format!(
                            "functionResponse references unknown call id `{tool_use_id}`"
                        )));
                    }
                }
                ProviderContent::Thinking { signature, .. } => {
                    if message.role != ProviderMessageRole::Assistant {
                        return Err(config_error(
                            "thought parts belong to assistant messages".into(),
                        ));
                    }
                    let Some((attached_to, _)) = thought_signature_envelope(signature) else {
                        return Err(config_error(
                            "this thinking block does not carry a Google thought signature; \
                             opaque continuation state from another provider is never replayed \
                             into a Gemini request"
                                .into(),
                        ));
                    };
                    if attached_to == "function_call" {
                        let next_is_call = message
                            .content
                            .get(index + 1)
                            .is_some_and(|next| matches!(next, ProviderContent::ToolUse { .. }));
                        if !next_is_call {
                            return Err(config_error(
                                "a signature-only thought must immediately precede its \
                                 function call"
                                    .into(),
                            ));
                        }
                    }
                }
                ProviderContent::RedactedThinking { .. } => {
                    return Err(config_error(
                        "redacted-thinking blocks are Anthropic continuation state and have no \
                         Gemini representation"
                            .into(),
                    ));
                }
                ProviderContent::Refusal { .. } => {
                    return Err(config_error(
                        "the Gemini API has no refusal content part".into(),
                    ));
                }
                ProviderContent::Text { .. } => {}
            }
        }
        if has_results && !pending.is_empty() {
            return Err(config_error(
                "functionResponse parts must resolve every functionCall from the preceding \
                 assistant message"
                    .into(),
            ));
        }
    }
    if !pending.is_empty() {
        return Err(config_error(
            "assistant functionCall parts must be followed by matching functionResponse parts"
                .into(),
        ));
    }
    Ok(())
}

fn encode_thinking_config(thinking: &ThinkingConfig) -> Option<Value> {
    let include_thoughts = |display: Option<ThinkingDisplay>| {
        matches!(
            display,
            Some(ThinkingDisplay::Summarized) | Some(ThinkingDisplay::Updates)
        )
    };
    match *thinking {
        ThinkingConfig::Default => None,
        ThinkingConfig::Disabled => Some(json!({"thinkingBudget": 0})),
        ThinkingConfig::Adaptive { display } => Some(json!({
            "thinkingBudget": -1,
            "includeThoughts": include_thoughts(display),
        })),
        ThinkingConfig::Enabled {
            budget_tokens,
            display,
            ..
        } => Some(json!({
            "thinkingBudget": budget_tokens,
            "includeThoughts": include_thoughts(display),
        })),
    }
}

// -- Request encoding ----------------------------------------------------

/// Builds the `contents` array. Each `ProviderMessage` maps to exactly one
/// Gemini `Content` turn (`role` + `parts`) -- unlike the Responses API,
/// Gemini needs no item-flushing: a turn never mixes roles.
fn encode_contents(request: &ProviderRequest) -> Result<Vec<Value>, ProviderFailure> {
    let mut contents = Vec::with_capacity(request.messages.len());
    let mut call_names: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    for message in &request.messages {
        let mut parts: Vec<Value> = Vec::with_capacity(message.content.len());
        let mut pending_signature: Option<String> = None;
        for block in &message.content {
            match block {
                ProviderContent::Text { text } => parts.push(json!({"text": text})),
                ProviderContent::Thinking {
                    thinking,
                    signature,
                } => {
                    let (attached_to, sig) =
                        thought_signature_envelope(signature).ok_or_else(|| {
                            config_error("thought block lost its Google signature envelope".into())
                        })?;
                    if attached_to == "function_call" {
                        pending_signature = Some(sig);
                    } else {
                        let mut part = json!({"text": thinking, "thought": true});
                        if !sig.is_empty() {
                            part["thoughtSignature"] = Value::String(sig);
                        }
                        parts.push(part);
                    }
                }
                ProviderContent::ToolUse { id, name, input } => {
                    call_names.insert(id.clone(), name.clone());
                    let mut part = json!({"functionCall": {"name": name, "args": input}});
                    if let Some(sig) = pending_signature.take() {
                        part["thoughtSignature"] = Value::String(sig);
                    }
                    parts.push(part);
                }
                ProviderContent::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let name = call_names.get(tool_use_id).cloned().ok_or_else(|| {
                        config_error(format!(
                            "functionResponse references unknown call id `{tool_use_id}`"
                        ))
                    })?;
                    let response = if *is_error {
                        json!({"error": content})
                    } else {
                        json!({"content": content})
                    };
                    parts.push(json!({"functionResponse": {"name": name, "response": response}}));
                }
                ProviderContent::RedactedThinking { .. } => {
                    return Err(config_error(
                        "redacted-thinking blocks have no Gemini representation".into(),
                    ));
                }
                ProviderContent::Refusal { .. } => {
                    return Err(config_error(
                        "the Gemini API has no refusal content part".into(),
                    ));
                }
            }
        }
        let role = match message.role {
            ProviderMessageRole::User => "user",
            ProviderMessageRole::Assistant => "model",
        };
        contents.push(json!({"role": role, "parts": parts}));
    }
    Ok(contents)
}

// -- Stream accumulation -------------------------------------------------

/// Gemini streams whole, non-overlapping incremental parts per SSE event
/// (assumed -- not a by-index delta protocol like Anthropic/OpenAI), and a
/// `functionCall` part always arrives complete in one event (assumed: the
/// Gemini API is not documented to fragment function-call args across
/// events). Both assumptions are recorded, and exercised only against the
/// fixtures this module owns, in the design note.
#[derive(Debug)]
enum OpenBlock {
    Text(String),
    Thought {
        text: String,
        signature: Option<String>,
    },
}

#[derive(Default)]
struct Accumulator {
    response_id: Option<String>,
    model: Option<String>,
    started: bool,
    completed: Vec<ProviderContent>,
    open: Option<OpenBlock>,
    tool_call_counter: usize,
    finish_reason_raw: Option<String>,
    stop_details: Option<OpaqueProviderData>,
    prompt_block_reason: Option<String>,
    usage: ProviderUsage,
    saw_event: bool,
    saw_candidate: bool,
}

fn parse_sse<R: BufRead>(
    mut reader: R,
    request_id: Option<String>,
    cancellation: &dyn Cancellation,
    sink: &mut dyn EventSink,
    target: &ProviderTarget,
) -> Result<ProviderResponse, ProviderFailure> {
    let mut accumulator = Accumulator::default();
    let mut data = String::new();
    let mut line = String::new();
    loop {
        let read = read_sse_line(&mut reader, &mut line, "Google", cancellation, target)?;
        if read == 0 {
            flush_data(&mut data, &mut accumulator, sink, target)?;
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            flush_data(&mut data, &mut accumulator, sink, target)?;
            line.clear();
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim_start());
        }
        // Gemini's stream never sends a named `event:` line; anything else
        // (a `:` keepalive comment, for instance) is ignored.
        line.clear();
    }
    finish_stream(accumulator, request_id)
}

fn flush_data(
    data: &mut String,
    accumulator: &mut Accumulator,
    sink: &mut dyn EventSink,
    target: &ProviderTarget,
) -> Result<(), ProviderFailure> {
    if data.is_empty() {
        return Ok(());
    }
    let value: Value = serde_json::from_str(data)
        .map_err(|error| invalid_stream(format!("invalid Google SSE JSON: {error}")))?;
    data.clear();
    process_chunk(&value, accumulator, sink, target)
}

fn process_chunk(
    value: &Value,
    accumulator: &mut Accumulator,
    sink: &mut dyn EventSink,
    target: &ProviderTarget,
) -> Result<(), ProviderFailure> {
    accumulator.saw_event = true;
    sink.push(ProviderStreamEvent::ProtocolActivity);
    if let Some(error) = value.get("error") {
        return Err(classify_error_payload(error, target));
    }
    if let Some(id) = value.get("responseId").and_then(Value::as_str) {
        accumulator.response_id = Some(id.to_string());
    }
    if let Some(model) = value.get("modelVersion").and_then(Value::as_str) {
        accumulator.model = Some(model.to_string());
    }
    if !accumulator.started
        && let (Some(id), Some(model)) = (&accumulator.response_id, &accumulator.model)
    {
        accumulator.started = true;
        sink.push(ProviderStreamEvent::MessageStarted {
            id: id.clone(),
            model: model.clone(),
        });
    }
    if let Some(usage) = value.get("usageMetadata") {
        capture_usage(&mut accumulator.usage, usage);
    }
    match value.get("candidates").and_then(Value::as_array) {
        None => {
            if let Some(reason) = value
                .pointer("/promptFeedback/blockReason")
                .and_then(Value::as_str)
            {
                accumulator.prompt_block_reason = Some(reason.to_string());
            }
        }
        Some(candidates) if candidates.is_empty() => {}
        Some(candidates) => {
            accumulator.saw_candidate = true;
            let candidate = &candidates[0];
            if let Some(parts) = candidate
                .pointer("/content/parts")
                .and_then(Value::as_array)
            {
                for part in parts {
                    process_part(part, accumulator, sink)?;
                }
            }
            if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                flush_open(accumulator)?;
                accumulator.finish_reason_raw = Some(reason.to_string());
                let mut details = json!({"finishReason": reason});
                if let Some(ratings) = candidate.get("safetyRatings") {
                    details["safetyRatings"] = ratings.clone();
                }
                accumulator.stop_details = Some(OpaqueProviderData::new(details));
            }
        }
    }
    Ok(())
}

fn process_part(
    part: &Value,
    accumulator: &mut Accumulator,
    sink: &mut dyn EventSink,
) -> Result<(), ProviderFailure> {
    if let Some(function_call) = part.get("functionCall") {
        flush_open(accumulator)?;
        if let Some(sig) = part.get("thoughtSignature").and_then(Value::as_str) {
            accumulator.completed.push(ProviderContent::Thinking {
                thinking: String::new(),
                signature: OpaqueProviderData::new(json!({
                    "type": "gemini_thought_signature",
                    "attached_to": "function_call",
                    "thought_signature": sig,
                })),
            });
        }
        let name = required_string(function_call, "name", "functionCall")?;
        let args = function_call
            .get("args")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if !args.is_object() {
            return Err(ProviderFailure::new(
                FailureClass::InvalidToolArguments,
                FailureScope::request(),
                format!("Gemini function call `{name}` args are not a JSON object"),
            ));
        }
        let id = format!("call_{}", accumulator.tool_call_counter);
        accumulator.tool_call_counter += 1;
        let index = accumulator.completed.len();
        sink.push(ProviderStreamEvent::ToolInputDelta {
            index,
            partial_json: args.to_string(),
        });
        accumulator.completed.push(ProviderContent::ToolUse {
            id,
            name,
            input: args,
        });
        sink.push(ProviderStreamEvent::BlockCompleted { index });
        return Ok(());
    }
    if let Some(text) = part.get("text").and_then(Value::as_str) {
        let thought = part
            .get("thought")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let signature = part
            .get("thoughtSignature")
            .and_then(Value::as_str)
            .map(str::to_string);
        let continues = matches!(
            (&accumulator.open, thought),
            (Some(OpenBlock::Thought { .. }), true) | (Some(OpenBlock::Text(_)), false)
        );
        if !continues {
            flush_open(accumulator)?;
            accumulator.open = Some(if thought {
                OpenBlock::Thought {
                    text: String::new(),
                    signature: None,
                }
            } else {
                OpenBlock::Text(String::new())
            });
        }
        let index = accumulator.completed.len();
        match accumulator.open.as_mut().expect("just opened above") {
            OpenBlock::Thought {
                text: buffer,
                signature: stored,
            } => {
                check_block_accumulator_cap(buffer.len(), text.len())?;
                buffer.push_str(text);
                if let Some(sig) = signature {
                    *stored = Some(sig);
                }
                sink.push(ProviderStreamEvent::ThinkingDelta {
                    index,
                    text: text.to_string(),
                });
            }
            OpenBlock::Text(buffer) => {
                check_block_accumulator_cap(buffer.len(), text.len())?;
                buffer.push_str(text);
                sink.push(ProviderStreamEvent::TextDelta {
                    index,
                    text: text.to_string(),
                });
            }
        }
        return Ok(());
    }
    // An unrecognized part kind (inline media, executable code, ...) is
    // ignored rather than guessed at; none of these are represented in the
    // shared `ProviderContent` contract yet.
    Ok(())
}

fn flush_open(accumulator: &mut Accumulator) -> Result<(), ProviderFailure> {
    let Some(open) = accumulator.open.take() else {
        return Ok(());
    };
    match open {
        OpenBlock::Text(text) => accumulator.completed.push(ProviderContent::Text { text }),
        OpenBlock::Thought { text, signature } => {
            let signature = signature.ok_or_else(|| {
                invalid_stream("Google thought part ended without a thoughtSignature")
            })?;
            accumulator.completed.push(ProviderContent::Thinking {
                thinking: text,
                signature: OpaqueProviderData::new(json!({
                    "type": "gemini_thought_signature",
                    "attached_to": "thought",
                    "thought_signature": signature,
                })),
            });
        }
    }
    Ok(())
}

fn finish_stream(
    mut accumulator: Accumulator,
    header_request_id: Option<String>,
) -> Result<ProviderResponse, ProviderFailure> {
    if let Some(block_reason) = accumulator.prompt_block_reason.take() {
        let message_id = accumulator
            .response_id
            .clone()
            .ok_or_else(|| invalid_stream("Google stream had no responseId"))?;
        let model = accumulator
            .model
            .clone()
            .ok_or_else(|| invalid_stream("Google stream had no modelVersion"))?;
        return Ok(ProviderResponse {
            message_id: message_id.clone(),
            model,
            content: vec![ProviderContent::Refusal {
                text: format!("prompt blocked: {block_reason}"),
            }],
            finish_reason: FinishReason::Refusal,
            stop_sequence: None,
            stop_details: Some(OpaqueProviderData::new(
                json!({"blockReason": block_reason}),
            )),
            usage: accumulator.usage,
            request_id: header_request_id.or(Some(message_id)),
        });
    }
    let Some(reason_raw) = accumulator.finish_reason_raw.clone() else {
        return Err(invalid_stream("Google stream ended before a finishReason"));
    };
    if !accumulator.saw_candidate {
        return Err(invalid_stream("Google stream produced no candidates"));
    }
    flush_open(&mut accumulator)?;
    let message_id = accumulator
        .response_id
        .clone()
        .ok_or_else(|| invalid_stream("Google stream had no responseId"))?;
    let model = accumulator
        .model
        .clone()
        .ok_or_else(|| invalid_stream("Google stream had no modelVersion"))?;
    let mut content = accumulator.completed;
    let finish_reason = match reason_raw.as_str() {
        "STOP" => {
            if content
                .iter()
                .any(|block| matches!(block, ProviderContent::ToolUse { .. }))
            {
                FinishReason::ToolUse
            } else {
                FinishReason::EndTurn
            }
        }
        "MAX_TOKENS" => FinishReason::MaxTokens,
        "SAFETY" | "RECITATION" | "PROHIBITED_CONTENT" | "BLOCKLIST" | "SPII" | "IMAGE_SAFETY" => {
            // A safety block is a typed refusal outcome, never prose: any
            // in-flight tool call is dropped and a `Refusal` block records
            // which finish reason triggered it.
            content.retain(|block| !matches!(block, ProviderContent::ToolUse { .. }));
            content.push(ProviderContent::Refusal {
                text: format!("blocked: {reason_raw}"),
            });
            FinishReason::Refusal
        }
        "MALFORMED_FUNCTION_CALL" => {
            return Err(ProviderFailure::new(
                FailureClass::InvalidToolArguments,
                FailureScope::request(),
                "Gemini reported a malformed function call",
            ));
        }
        other => FinishReason::Unknown(other.to_string()),
    };
    Ok(ProviderResponse {
        message_id: message_id.clone(),
        model,
        content,
        finish_reason,
        stop_sequence: None,
        stop_details: accumulator.stop_details,
        usage: accumulator.usage,
        request_id: header_request_id.or(Some(message_id)),
    })
}

/// Usage is read from whichever chunk carries it (usually the terminal one);
/// Gemini's `usageMetadata` is a cumulative snapshot, so later values simply
/// overwrite earlier ones rather than accumulating.
fn capture_usage(usage: &mut ProviderUsage, value: &Value) {
    if let Some(tokens) = value.get("promptTokenCount").and_then(Value::as_u64) {
        usage.input_tokens = tokens;
    }
    if let Some(tokens) = value.get("candidatesTokenCount").and_then(Value::as_u64) {
        usage.output_tokens = tokens;
    }
    if let Some(tokens) = value.get("cachedContentTokenCount").and_then(Value::as_u64) {
        usage.cache_read_input_tokens = tokens;
    }
    if let Some(tokens) = value.get("thoughtsTokenCount").and_then(Value::as_u64) {
        usage.reasoning_tokens = Some(tokens);
    }
}

fn required_string(value: &Value, field: &str, context: &str) -> Result<String, ProviderFailure> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| invalid_stream(format!("Google {context} has no string `{field}`")))
}

// -- Failure normalization -----------------------------------------------

fn invalid_stream(message: impl Into<String>) -> ProviderFailure {
    super::transport::invalid_stream(message.into())
}

fn check_block_accumulator_cap(
    current_len: usize,
    delta_len: usize,
) -> Result<(), ProviderFailure> {
    super::transport::check_block_accumulator_cap("Google", current_len, delta_len)
}

fn cancelled() -> ProviderFailure {
    super::transport::cancelled("Google")
}

fn timeout_failure(saw_event: bool, target: &ProviderTarget) -> ProviderFailure {
    super::transport::timeout_failure("Google", saw_event, target)
}

fn transport_failure(message: String, target: &ProviderTarget) -> ProviderFailure {
    super::transport::transport_failure(message, target)
}

fn classify_transport_error(
    error: ureq::Error,
    saw_event: bool,
    target: &ProviderTarget,
) -> ProviderFailure {
    if matches!(error, ureq::Error::Timeout(_)) {
        return timeout_failure(saw_event, target);
    }
    transport_failure(format!("Google transport failed: {error}"), target)
}

/// `google.rpc.RetryInfo.retryDelay` is a Go-duration-style string like
/// `"13s"`; that is the only shape Google's own docs show, so anything else
/// is left unparsed rather than guessed at.
fn parse_google_retry_delay(value: &str) -> Option<u64> {
    let seconds = value.strip_suffix('s')?;
    let seconds: f64 = seconds.parse().ok()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some((seconds * 1000.0).round() as u64)
}

fn google_retry_delay_ms(error: &Value) -> Option<u64> {
    error
        .get("details")
        .and_then(Value::as_array)?
        .iter()
        .find_map(|detail| {
            let kind = detail.get("@type").and_then(Value::as_str)?;
            if !kind.contains("RetryInfo") {
                return None;
            }
            parse_google_retry_delay(detail.get("retryDelay").and_then(Value::as_str)?)
        })
}

fn classify_http_error(
    status: u16,
    body: &str,
    header_request_id: Option<String>,
    retry_after_ms: Option<u64>,
    target: &ProviderTarget,
) -> ProviderFailure {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let error = parsed.get("error").cloned().unwrap_or(Value::Null);
    let status_code = error
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Google request failed")
        .to_string();
    let lower = message.to_ascii_lowercase();
    let retry_after_ms = retry_after_ms.or_else(|| google_retry_delay_ms(&error));
    let (class, scope_kind, retryable) = match status {
        401 => (
            FailureClass::Authentication,
            FailureScopeKind::Account,
            false,
        ),
        403 => (FailureClass::Permission, FailureScopeKind::Account, false),
        404 => (FailureClass::ModelAccess, FailureScopeKind::Model, false),
        429 => (
            FailureClass::RateLimited,
            FailureScopeKind::BillingPool,
            true,
        ),
        400 if lower.contains("project") || lower.contains("location") => (
            FailureClass::Configuration,
            FailureScopeKind::Account,
            false,
        ),
        400 if lower.contains("context") || lower.contains("token") => (
            FailureClass::ContextOverflow,
            FailureScopeKind::Request,
            false,
        ),
        400 => (
            FailureClass::Configuration,
            FailureScopeKind::Request,
            false,
        ),
        503 => (FailureClass::Overloaded, FailureScopeKind::Provider, true),
        500 | 502 | 504 => (FailureClass::Provider, FailureScopeKind::Endpoint, true),
        _ if status_code == "RESOURCE_EXHAUSTED" => (
            FailureClass::RateLimited,
            FailureScopeKind::BillingPool,
            true,
        ),
        _ if status_code == "UNAUTHENTICATED" => (
            FailureClass::Authentication,
            FailureScopeKind::Account,
            false,
        ),
        _ if status_code == "PERMISSION_DENIED" => {
            (FailureClass::Permission, FailureScopeKind::Account, false)
        }
        _ if status_code == "NOT_FOUND" => {
            (FailureClass::ModelAccess, FailureScopeKind::Model, false)
        }
        _ => (
            FailureClass::Configuration,
            FailureScopeKind::Request,
            false,
        ),
    };
    let mut failure = ProviderFailure::new(class, target_scope(target, scope_kind), message);
    failure.http_status = Some(status);
    failure.provider_request_id = header_request_id;
    failure.retry = RetryHint {
        retryable,
        after_ms: retry_after_ms,
    };
    failure
}

/// A mid-stream `{"error": {...}}` SSE chunk. Google's documented failure
/// path for `generateContent` is an HTTP-level error before the stream
/// starts; this in-stream shape is defensive (a proxy or gateway in front of
/// the API can still fail this way) and reuses the same classification table
/// keyed by `error.code`.
fn classify_error_payload(value: &Value, target: &ProviderTarget) -> ProviderFailure {
    let status = value
        .get("code")
        .and_then(Value::as_u64)
        .and_then(|code| u16::try_from(code).ok())
        .unwrap_or(0);
    let body = json!({"error": value}).to_string();
    let mut failure = classify_http_error(status, &body, None, None, target);
    if status == 0 {
        failure.http_status = None;
    }
    failure
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::commands::ctx::provider::adapter::{
        CancellationFlag, NeverCancelled, ProviderMessage, journal_blocks, replayed_content,
    };
    use crate::commands::ctx::provider::anthropic::{AnthropicMessagesAdapter, AnthropicTimeouts};
    use crate::commands::ctx::provider::credential::Secret;
    use crate::commands::ctx::provider::{
        AccountId, BillingPoolId, EndpointId, ModelId, ProviderId,
    };
    use crate::commands::ctx::runtime::journal::AssistantBlock;
    use crate::commands::ctx::runtime::tools::ToolRegistry;

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/provider/google/v1/",
                $name
            ))
        };
    }

    const MULTI_CALL: &str = fixture!("stream-multi-function-thought-signature.sse");
    const MALFORMED: &str = fixture!("stream-malformed-tool.sse");
    const TRUNCATED: &str = fixture!("stream-truncated-tool.sse");
    const SAFETY: &str = fixture!("stream-safety-block.sse");
    const QUOTA: &str = fixture!("stream-quota.sse");
    const ERROR_EVENT: &str = fixture!("stream-error.sse");
    const FINAL_TEXT: &str = fixture!("stream-final-text.sse");

    fn target(base_url: String) -> ProviderTarget {
        ProviderTarget {
            route: RouteId::new("work-gemini").unwrap(),
            provider: ProviderId::new("google").unwrap(),
            endpoint: EndpointId::new("google").unwrap(),
            account: AccountId::new("work").unwrap(),
            billing_pool: BillingPoolId::new("work").unwrap(),
            protocol: Protocol::GoogleGenerativeAi,
            base_url,
            model: ModelId {
                vendor: "google".into(),
                id: "gemini-2.5-pro".into(),
            },
        }
    }

    fn vertex_target(base_url: String) -> ProviderTarget {
        let mut target = target(base_url);
        target.protocol = Protocol::GoogleVertex;
        target.provider = ProviderId::new("google-vertex").unwrap();
        target
    }

    fn credential(value: &str) -> Credential {
        Credential {
            secret: Secret::new(value.into()),
            expires_at: None,
        }
    }

    fn adapter(base_url: String) -> GoogleAdapter {
        GoogleAdapter::new(
            target(base_url),
            credential("AIza-test-secret-never-log"),
            GoogleProfile::Developer,
            GoogleTimeouts::default(),
        )
        .unwrap()
    }

    fn vertex_adapter(base_url: String) -> GoogleAdapter {
        GoogleAdapter::new(
            vertex_target(base_url),
            credential("ya29.test-access-token-never-log"),
            GoogleProfile::Vertex {
                project: "proj-1".into(),
                location: "us-central1".into(),
            },
            GoogleTimeouts::default(),
        )
        .unwrap()
    }

    fn request() -> ProviderRequest {
        ProviderRequest {
            model: "gemini-2.5-pro".into(),
            system: vec!["system method".into()],
            messages: vec![ProviderMessage {
                role: ProviderMessageRole::User,
                content: vec![ProviderContent::Text {
                    text: "do work".into(),
                }],
            }],
            tools: ToolRegistry::native()
                .definitions()
                .take(2)
                .cloned()
                .collect(),
            max_output_tokens: 4096,
            stop_sequences: Vec::new(),
            thinking: ThinkingConfig::Adaptive {
                display: Some(ThinkingDisplay::Summarized),
            },
            effort: None,
            cache: CacheMode::Disabled,
        }
    }

    fn parse(stream: &str) -> Result<ProviderResponse, ProviderFailure> {
        parse_sse(
            BufReader::new(stream.as_bytes()),
            Some("req_header".into()),
            &NeverCancelled,
            &mut Vec::new(),
            &target("https://generativelanguage.googleapis.com".into()),
        )
    }

    #[test]
    fn stream_reassembles_thought_signatures_usage_and_parallel_function_calls() {
        let mut events = Vec::new();
        let response = parse_sse(
            BufReader::new(MULTI_CALL.as_bytes()),
            Some("req_header".into()),
            &NeverCancelled,
            &mut events,
            &target("https://generativelanguage.googleapis.com".into()),
        )
        .unwrap();
        assert_eq!(response.message_id, "resp_fixture");
        assert_eq!(response.model, "gemini-2.5-pro");
        assert_eq!(response.finish_reason, FinishReason::ToolUse);
        assert_eq!(response.request_id.as_deref(), Some("req_header"));
        assert_eq!(response.usage.input_tokens, 120);
        assert_eq!(response.usage.output_tokens, 45);
        assert_eq!(response.usage.reasoning_tokens, Some(11));
        assert_eq!(response.usage.cache_read_input_tokens, 60);
        assert_eq!(response.content.len(), 4);
        assert!(matches!(
            &response.content[0],
            ProviderContent::Thinking { thinking, signature }
                if thinking == "check both files"
                    && signature.expose()["thought_signature"] == "sig-thought-1"
                    && signature.expose()["attached_to"] == "thought"
        ));
        assert!(matches!(
            &response.content[1],
            ProviderContent::ToolUse { id, input, name }
                if id == "call_0" && name == "file_read" && input["path"] == "src/lib.rs"
        ));
        assert!(matches!(
            &response.content[2],
            ProviderContent::Thinking { thinking, signature }
                if thinking.is_empty()
                    && signature.expose()["thought_signature"] == "sig-call-2"
                    && signature.expose()["attached_to"] == "function_call"
        ));
        assert!(matches!(
            &response.content[3],
            ProviderContent::ToolUse { id, input, name }
                if id == "call_1" && name == "file_read" && input["path"] == "Cargo.toml"
        ));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderStreamEvent::ToolInputDelta { .. }))
        );
        assert!(!format!("{response:?}").contains("sig-thought-1"));
        assert!(!format!("{response:?}").contains("sig-call-2"));
    }

    #[test]
    fn malformed_function_args_never_become_a_tool_call() {
        let mut events = Vec::new();
        let error = parse_sse(
            BufReader::new(MALFORMED.as_bytes()),
            None,
            &NeverCancelled,
            &mut events,
            &target("https://generativelanguage.googleapis.com".into()),
        )
        .unwrap_err();
        assert_eq!(error.class, FailureClass::InvalidToolArguments);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ProviderStreamEvent::BlockCompleted { .. }))
        );
    }

    #[test]
    fn a_stream_that_ends_before_a_finish_reason_is_truncated_not_completed() {
        let error = parse(TRUNCATED).unwrap_err();
        assert_eq!(error.class, FailureClass::InvalidStream);
    }

    #[test]
    fn safety_finish_reason_is_a_typed_refusal_not_prose() {
        let response = parse(SAFETY).unwrap();
        assert_eq!(response.finish_reason, FinishReason::Refusal);
        assert!(response.content.iter().any(
            |block| matches!(block, ProviderContent::Refusal { text } if text.contains("SAFETY"))
        ));
        assert!(
            !response
                .content
                .iter()
                .any(|block| matches!(block, ProviderContent::ToolUse { .. }))
        );
    }

    #[test]
    fn quota_and_generic_errors_classify_with_retry_hints() {
        let target = target("https://generativelanguage.googleapis.com".into());
        let quota = parse_sse(
            BufReader::new(QUOTA.as_bytes()),
            None,
            &NeverCancelled,
            &mut Vec::new(),
            &target,
        )
        .unwrap_err();
        assert_eq!(quota.class, FailureClass::RateLimited);
        assert_eq!(quota.scope.kind, FailureScopeKind::BillingPool);
        assert!(quota.retry.retryable);
        assert_eq!(quota.retry.after_ms, Some(13_000));

        let generic = parse_sse(
            BufReader::new(ERROR_EVENT.as_bytes()),
            None,
            &NeverCancelled,
            &mut Vec::new(),
            &target,
        )
        .unwrap_err();
        assert_eq!(generic.class, FailureClass::Provider);
        assert!(generic.retry.retryable);
    }

    #[test]
    fn final_text_turn_has_no_tool_use_and_ends_the_turn() {
        let response = parse(FINAL_TEXT).unwrap();
        assert_eq!(response.finish_reason, FinishReason::EndTurn);
        assert_eq!(
            response.content,
            vec![ProviderContent::Text {
                text: "All checks passed.".into()
            }]
        );
        assert_eq!(response.usage.input_tokens, 8);
        assert_eq!(response.usage.output_tokens, 4);
    }

    #[test]
    fn status_failures_have_stable_class_scope_and_retry_hint() {
        let target = target("https://generativelanguage.googleapis.com".into());
        for (status, body, class, scope) in [
            (
                401,
                r#"{"error":{"code":401,"message":"bad key","status":"UNAUTHENTICATED"}}"#,
                FailureClass::Authentication,
                FailureScopeKind::Account,
            ),
            (
                403,
                r#"{"error":{"code":403,"message":"denied","status":"PERMISSION_DENIED"}}"#,
                FailureClass::Permission,
                FailureScopeKind::Account,
            ),
            (
                404,
                r#"{"error":{"code":404,"message":"no such model","status":"NOT_FOUND"}}"#,
                FailureClass::ModelAccess,
                FailureScopeKind::Model,
            ),
            (
                400,
                r#"{"error":{"code":400,"message":"invalid project id","status":"INVALID_ARGUMENT"}}"#,
                FailureClass::Configuration,
                FailureScopeKind::Account,
            ),
            (
                503,
                r#"{"error":{"code":503,"message":"overloaded","status":"UNAVAILABLE"}}"#,
                FailureClass::Overloaded,
                FailureScopeKind::Provider,
            ),
        ] {
            let failure = classify_http_error(status, body, Some("hdr".into()), None, &target);
            assert_eq!(failure.class, class, "status {status}");
            assert_eq!(failure.scope.kind, scope, "status {status}");
            assert_eq!(failure.http_status, Some(status));
        }
    }

    #[test]
    fn developer_and_vertex_credentials_use_their_own_header_and_url() {
        let (url, captured) = one_shot_server(200, FINAL_TEXT, &[]);
        let response = adapter(url)
            .stream(&request(), &NeverCancelled, &mut Vec::new())
            .unwrap();
        assert_eq!(response.finish_reason, FinishReason::EndTurn);
        let sent = captured.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(sent.starts_with(
            "POST /v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse HTTP/1.1"
        ));
        assert!(sent.contains("x-goog-api-key: AIza-test-secret-never-log"));
        assert!(!sent.to_ascii_lowercase().contains("authorization:"));

        let (url, captured) = one_shot_server(200, FINAL_TEXT, &[]);
        let mut vertex_request = request();
        let response = vertex_adapter(url)
            .stream(&vertex_request, &NeverCancelled, &mut Vec::new())
            .unwrap();
        assert_eq!(response.finish_reason, FinishReason::EndTurn);
        let sent = captured.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(sent.starts_with(
            "POST /v1/projects/proj-1/locations/us-central1/publishers/google/models/gemini-2.5-pro:streamGenerateContent?alt=sse HTTP/1.1"
        ));
        assert!(
            sent.to_ascii_lowercase()
                .contains("authorization: bearer ya29.test-access-token-never-log")
        );
        assert!(!sent.contains("x-goog-api-key"));
        vertex_request.model = "gemini-2.5-pro".into();
        let _ = vertex_request;
    }

    #[test]
    fn gemini_cli_oauth_credentials_are_refused_as_an_entitlement_not_authentication() {
        let developer = GoogleAdapter::new(
            target("https://generativelanguage.googleapis.com".into()),
            credential("{\"access_token\":\"ya29...\",\"refresh_token\":\"1//...\"}"),
            GoogleProfile::Developer,
            GoogleTimeouts::default(),
        )
        .unwrap_err();
        assert_eq!(developer.class, FailureClass::Entitlement);
        assert!(developer.message.contains("Gemini CLI"));

        let vertex = GoogleAdapter::new(
            vertex_target("https://us-central1-aiplatform.googleapis.com".into()),
            credential("Bearer some-token"),
            GoogleProfile::Vertex {
                project: "proj-1".into(),
                location: "us-central1".into(),
            },
            GoogleTimeouts::default(),
        )
        .unwrap_err();
        assert_eq!(vertex.class, FailureClass::Entitlement);

        // A real Vertex access token (`ya29...`, no braces) is not rejected
        // by shape -- only the harness-login-blob shape is.
        assert!(
            GoogleAdapter::new(
                vertex_target("https://us-central1-aiplatform.googleapis.com".into()),
                credential("ya29.a0Aa-real-looking-token"),
                GoogleProfile::Vertex {
                    project: "proj-1".into(),
                    location: "us-central1".into(),
                },
                GoogleTimeouts::default(),
            )
            .is_ok()
        );
    }

    #[test]
    fn model_family_and_thinking_control_validation_fails_locally() {
        let target = target("https://generativelanguage.googleapis.com".into());
        let mut older = request();
        older.model = "gemini-1.5-pro".into();
        let mut older_target = target.clone();
        older_target.model.id = "gemini-1.5-pro".into();
        assert_eq!(
            validate_request(&older, &older_target).unwrap_err().class,
            FailureClass::Configuration
        );

        let mut disabled_on_pro = request();
        disabled_on_pro.thinking = ThinkingConfig::Disabled;
        assert_eq!(
            validate_request(&disabled_on_pro, &target)
                .unwrap_err()
                .class,
            FailureClass::Configuration
        );

        let mut zero_budget = request();
        zero_budget.thinking = ThinkingConfig::Enabled {
            budget_tokens: 0,
            display: None,
            interleaved: false,
        };
        assert_eq!(
            validate_request(&zero_budget, &target).unwrap_err().class,
            FailureClass::Configuration
        );

        let mut effort = request();
        effort.effort = Some(super::super::adapter::Effort::Medium);
        assert_eq!(
            validate_request(&effort, &target).unwrap_err().class,
            FailureClass::Configuration
        );

        let mut cached = request();
        cached.cache = CacheMode::Ephemeral5m;
        assert_eq!(
            validate_request(&cached, &target).unwrap_err().class,
            FailureClass::Configuration
        );
    }

    #[test]
    fn same_route_replay_preserves_thought_signatures_and_cannot_leak_across_providers() {
        let response = parse(MULTI_CALL).unwrap();
        let blocks = journal_blocks(&response.content).unwrap();
        // Survives the journal's own serialization round trip, and never
        // leaks the raw signature through `Debug`.
        let stored = serde_json::to_string(&blocks).unwrap();
        assert!(!format!("{blocks:?}").contains("sig-thought-1"));
        let restored: Vec<AssistantBlock> = serde_json::from_str(&stored).unwrap();
        // `ToolCall` blocks replay as `None` -- their arguments live in the
        // journal's own tool-call record, not in the block -- so only the
        // thinking blocks survive `filter_map`.
        let replayed: Vec<ProviderContent> = restored.iter().filter_map(replayed_content).collect();
        assert_eq!(replayed.len(), 2);
        assert_eq!(replayed[0], response.content[0]);

        let mut continued = request();
        continued.messages.push(ProviderMessage {
            role: ProviderMessageRole::Assistant,
            content: vec![replayed[0].clone()],
        });
        let body = adapter("https://generativelanguage.googleapis.com".into())
            .encode_request(&continued)
            .unwrap()
            .body;
        assert_eq!(body["contents"][1]["role"], "model");
        assert_eq!(body["contents"][1]["parts"][0]["thought"], true);
        assert_eq!(
            body["contents"][1]["parts"][0]["thoughtSignature"],
            "sig-thought-1"
        );

        // The same replayed thought signature can never reach an Anthropic
        // request: it carries no Anthropic-shaped signature at all.
        let anthropic = AnthropicMessagesAdapter::new(
            {
                let mut t = target("https://api.anthropic.com".into());
                t.protocol = Protocol::AnthropicMessages;
                t.provider = ProviderId::new("anthropic").unwrap();
                t.model = ModelId {
                    vendor: "anthropic".into(),
                    id: "claude-sonnet-5".into(),
                };
                t
            },
            Credential {
                secret: Secret::new("sk-ant-test".into()),
                expires_at: None,
            },
            AnthropicTimeouts::default(),
        )
        .unwrap();
        let mut crossed = continued;
        crossed.model = "claude-sonnet-5".into();
        crossed.thinking = ThinkingConfig::Adaptive {
            display: Some(ThinkingDisplay::Omitted),
        };
        let error = anthropic
            .stream(&crossed, &NeverCancelled, &mut Vec::new())
            .unwrap_err();
        assert_eq!(error.class, FailureClass::Configuration);
    }

    #[test]
    fn function_response_parts_are_encoded_by_name_looked_up_from_the_matching_call() {
        let mut continued = request();
        continued.messages.push(ProviderMessage {
            role: ProviderMessageRole::Assistant,
            content: vec![ProviderContent::ToolUse {
                id: "call_0".into(),
                name: "file_read".into(),
                input: json!({"path": "src/lib.rs"}),
            }],
        });
        continued.messages.push(ProviderMessage {
            role: ProviderMessageRole::User,
            content: vec![ProviderContent::ToolResult {
                tool_use_id: "call_0".into(),
                content: "ok".into(),
                is_error: false,
            }],
        });
        let body = adapter("https://generativelanguage.googleapis.com".into())
            .encode_request(&continued)
            .unwrap()
            .body;
        assert_eq!(
            body["contents"][1]["parts"][0]["functionCall"]["name"],
            "file_read"
        );
        assert_eq!(
            body["contents"][2]["parts"][0]["functionResponse"]["name"],
            "file_read"
        );
        assert_eq!(
            body["contents"][2]["parts"][0]["functionResponse"]["response"]["content"],
            "ok"
        );

        let mut unknown = request();
        unknown.messages.push(ProviderMessage {
            role: ProviderMessageRole::User,
            content: vec![ProviderContent::ToolResult {
                tool_use_id: "call_missing".into(),
                content: "ok".into(),
                is_error: false,
            }],
        });
        assert_eq!(
            validate_content_relationships(&unknown).unwrap_err().class,
            FailureClass::Configuration
        );
    }

    #[test]
    fn cancelled_before_transport_is_typed() {
        let flag = CancellationFlag::default();
        flag.cancel();
        let error = adapter("http://127.0.0.1:9".into())
            .stream(&request(), &flag, &mut Vec::new())
            .unwrap_err();
        assert_eq!(error.class, FailureClass::Cancelled);
    }

    #[test]
    fn first_event_timeout_is_enforced() {
        let (url, _captured) = delayed_stream_server("", FINAL_TEXT, Duration::from_millis(150));
        let slow = GoogleAdapter::new(
            target(url),
            credential("AIza-test-secret-never-log"),
            GoogleProfile::Developer,
            GoogleTimeouts {
                connect: Duration::from_secs(1),
                first_event: Duration::from_millis(20),
                idle: Duration::from_secs(1),
            },
        )
        .unwrap();
        let error = slow
            .stream(&request(), &NeverCancelled, &mut Vec::new())
            .unwrap_err();
        assert_eq!(error.class, FailureClass::FirstEventTimeout);
    }

    fn one_shot_server(
        status: u16,
        body: &'static str,
        headers: &'static [(&'static str, &'static str)],
    ) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            let _ = sender.send(request);
            let reason = if status == 200 { "OK" } else { "Error" };
            write!(
                stream,
                "HTTP/1.1 {status} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                if status == 200 {
                    "text/event-stream"
                } else {
                    "application/json"
                },
                body.len()
            )
            .unwrap();
            for (name, value) in headers {
                write!(stream, "{name}: {value}\r\n").unwrap();
            }
            write!(stream, "\r\n{body}").unwrap();
        });
        (format!("http://{address}"), receiver)
    }

    fn delayed_stream_server(
        prefix: &'static str,
        suffix: &'static str,
        delay: Duration,
    ) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            let _ = sender.send(request);
            let length = prefix.len() + suffix.len();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n{prefix}"
            )
            .unwrap();
            stream.flush().unwrap();
            std::thread::sleep(delay);
            let _ = stream.write_all(suffix.as_bytes());
        });
        (format!("http://{address}"), receiver)
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 4096];
        let mut expected = None;
        loop {
            let count = stream.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            if expected.is_none() {
                let text = String::from_utf8_lossy(&bytes);
                if let Some(header_end) = text.find("\r\n\r\n") {
                    let headers = &text[..header_end];
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse::<usize>())
                        })
                        .transpose()
                        .unwrap_or(None)
                        .unwrap_or(0);
                    expected = Some(header_end + 4 + content_length);
                }
            }
            if let Some(expected) = expected
                && bytes.len() >= expected
            {
                break;
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[test]
    #[ignore = "live Gemini Developer API contract; set GEMINI_API_KEY and ZIRV_GOOGLE_LIVE_MODEL"]
    fn live_google_generative_ai_contract() {
        let key = std::env::var("GEMINI_API_KEY").expect("GEMINI_API_KEY");
        let model = std::env::var("ZIRV_GOOGLE_LIVE_MODEL")
            .expect("ZIRV_GOOGLE_LIVE_MODEL must name an entitled exact model id");
        let mut live_target = target("https://generativelanguage.googleapis.com".into());
        live_target.model.id = model.clone();
        let adapter = GoogleAdapter::new(
            live_target,
            Credential {
                secret: Secret::new(key),
                expires_at: None,
            },
            GoogleProfile::Developer,
            GoogleTimeouts::default(),
        )
        .unwrap();
        let mut live_request = request();
        live_request.model = model;
        live_request.messages = vec![ProviderMessage {
            role: ProviderMessageRole::User,
            content: vec![ProviderContent::Text {
                text: "Reply with exactly: zirv-live-ok".into(),
            }],
        }];
        live_request.tools.clear();
        live_request.thinking = ThinkingConfig::Default;
        live_request.effort = None;
        let response = adapter
            .stream(&live_request, &NeverCancelled, &mut Vec::new())
            .unwrap();
        assert!(!response.message_id.is_empty());
        assert!(response.usage.output_tokens > 0);
    }
}
