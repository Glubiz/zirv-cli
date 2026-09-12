//! Direct OpenAI Responses transport (issue #477, roadmap N08).
//!
//! This is raw HTTPS/SSE against `POST /v1/responses`, not the Codex binary,
//! the Codex SDK, or the Codex App Server. Zirv owns request construction,
//! stream accumulation, tool execution, conversation state, cancellation,
//! typed failures, and the opaque reasoning items required for a safe
//! continuation.
//!
//! The conversation is always locally owned: every request carries
//! `store: false` and the full replayed history. `previous_response_id` is
//! deliberately unused -- it only continues a provider-stored response, so
//! it could never be the durable state, and Zirv keeps no route where it is.

#![allow(dead_code)] // N09 wires direct providers into the persistent runtime loop.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader};

use serde_json::{Map, Value, json};

use super::adapter::{
    CacheMode, Cancellation, Effort, EventSink, FailureClass, FailureScope, FailureScopeKind,
    FinishReason, ProviderAdapter, ProviderContent, ProviderFailure, ProviderMessageRole,
    ProviderRequest, ProviderResponse, ProviderStreamEvent, ProviderTarget, ProviderUsage,
    RetryHint, ThinkingConfig, ThinkingDisplay, resolve_target,
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
/// OpenAI call sites reading as OpenAI ones.
pub type OpenAiTimeouts = StreamTimeouts;

#[derive(Clone)]
pub struct OpenAiResponsesAdapter {
    target: ProviderTarget,
    credential: Credential,
    timeouts: OpenAiTimeouts,
}

impl std::fmt::Debug for OpenAiResponsesAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiResponsesAdapter")
            .field("target", &self.target)
            .field("credential", &"[redacted]")
            .field("timeouts", &self.timeouts)
            .finish()
    }
}

impl OpenAiResponsesAdapter {
    pub fn from_config(
        config: &NativeConfig,
        route: &RouteId,
        env: EnvLookup<'_>,
        store: &dyn CredentialStore,
        now: u64,
        timeouts: OpenAiTimeouts,
    ) -> Result<Self, ProviderFailure> {
        let (target, credential) = resolve_target(config, route, env, store, now)?;
        if target.protocol != Protocol::OpenAiResponses {
            return Err(ProviderFailure::new(
                FailureClass::Configuration,
                FailureScope::request(),
                format!(
                    "route `{route}` uses {:?}, not the OpenAI Responses protocol",
                    target.protocol
                ),
            ));
        }
        let credential = credential.ok_or_else(|| {
            ProviderFailure::new(
                FailureClass::Authentication,
                FailureScope {
                    kind: FailureScopeKind::Account,
                    id: Some(target.account.to_string()),
                },
                format!("OpenAI account `{}` has no API credential", target.account),
            )
        })?;
        Self::new(target, credential, timeouts)
    }

    pub fn new(
        target: ProviderTarget,
        credential: Credential,
        timeouts: OpenAiTimeouts,
    ) -> Result<Self, ProviderFailure> {
        if target.protocol != Protocol::OpenAiResponses {
            return Err(ProviderFailure::new(
                FailureClass::Configuration,
                FailureScope::request(),
                "OpenAI adapter requires an openai-responses target",
            ));
        }
        if is_plaintext_non_loopback(&target.base_url) {
            return Err(ProviderFailure::new(
                FailureClass::Configuration,
                FailureScope {
                    kind: FailureScopeKind::Endpoint,
                    id: Some(target.endpoint.to_string()),
                },
                "OpenAI credentials cannot be sent over plaintext HTTP to a non-loopback host",
            ));
        }
        if timeouts.has_zero() {
            return Err(ProviderFailure::new(
                FailureClass::Configuration,
                FailureScope::request(),
                "OpenAI timeouts must be greater than zero",
            ));
        }
        reject_subscription_credential(credential.secret.expose(), &target)?;
        Ok(Self {
            target,
            credential,
            timeouts,
        })
    }

    fn responses_url(&self) -> String {
        format!(
            "{}/v1/responses",
            self.target.base_url.trim_end_matches('/')
        )
    }

    fn encode_request(&self, request: &ProviderRequest) -> Result<EncodedRequest, ProviderFailure> {
        validate_request(request, &self.target)?;
        let mut body = Map::new();
        body.insert("model".into(), Value::String(request.model.clone()));
        body.insert("stream".into(), Value::Bool(true));
        // Zirv owns the conversation: the provider stores nothing and every
        // turn replays the full local history.
        body.insert("store".into(), Value::Bool(false));
        body.insert("max_output_tokens".into(), json!(request.max_output_tokens));
        if !request.system.is_empty() {
            body.insert(
                "instructions".into(),
                Value::String(request.system.join("\n\n")),
            );
        }
        body.insert("input".into(), encode_input(request)?);
        if !request.tools.is_empty() {
            body.insert(
                "tools".into(),
                Value::Array(
                    request
                        .tools
                        .iter()
                        .map(|tool| {
                            json!({
                                "type": "function",
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.input_schema,
                            })
                        })
                        .collect(),
                ),
            );
            body.insert("parallel_tool_calls".into(), Value::Bool(true));
        }
        if model_profile(&request.model).reasoning {
            // Encrypted reasoning is the only continuation material a
            // `store: false` conversation can replay on the next turn.
            body.insert("include".into(), json!(["reasoning.encrypted_content"]));
            let mut reasoning = Map::new();
            if let Some(effort) = request.effort {
                reasoning.insert("effort".into(), Value::String(effort_name(effort).into()));
            }
            if wants_reasoning_summary(&request.thinking) {
                reasoning.insert("summary".into(), Value::String("auto".into()));
            }
            if !reasoning.is_empty() {
                body.insert("reasoning".into(), Value::Object(reasoning));
            }
        }
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
                format!("failed to encode OpenAI request: {error}"),
            )
        })?;
        let http = agent
            .post(self.responses_url())
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header(
                "authorization",
                format!("Bearer {}", self.credential.secret.expose()),
            )
            .header("user-agent", format!("zirv/{}", env!("CARGO_PKG_VERSION")));
        let mut response = http
            .send(payload)
            .map_err(|error| classify_transport_error(error, false, &self.target))?;
        let status = response.status().as_u16();
        let request_id = response
            .headers()
            .get("x-request-id")
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
                .unwrap_or_else(|_| "OpenAI returned an unreadable error body".into());
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
            "OpenAI",
            "zirv-openai-stream",
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

impl ProviderAdapter for OpenAiResponsesAdapter {
    fn protocol(&self) -> Protocol {
        Protocol::OpenAiResponses
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

/// A Platform API key is the only credential class this transport accepts.
/// A ChatGPT/Codex subscription login is an OAuth JWT or a whole `auth.json`
/// object; billing it as API usage would silently move spend between two
/// different identities, so it is refused by class rather than attempted.
fn reject_subscription_credential(
    secret: &str,
    target: &ProviderTarget,
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
            format!("OpenAI account `{}` has an empty API key", target.account),
        ));
    }
    let subscription_shaped = trimmed.starts_with('{')
        || trimmed.starts_with("eyJ")
        || trimmed.to_ascii_lowercase().starts_with("bearer ");
    if subscription_shaped {
        return Err(ProviderFailure::new(
            FailureClass::Entitlement,
            account_scope(),
            format!(
                "OpenAI account `{}` is configured with a ChatGPT/Codex subscription login \
                 (an OAuth token or an `auth.json` blob), not a Platform API key; point the \
                 account credential at an `OPENAI_API_KEY` from platform.openai.com -- \
                 subscription entitlements are never substituted for API billing",
                target.account
            ),
        ));
    }
    Ok(())
}

// -- Local validation ----------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ModelProfile {
    /// The model takes `reasoning` controls and emits reasoning items.
    reasoning: bool,
    /// The id names a Codex harness build rather than a verified Responses
    /// API model id.
    codex_harness_build: bool,
}

fn model_profile(model: &str) -> ModelProfile {
    let id = model.to_ascii_lowercase();
    ModelProfile {
        reasoning: ["gpt-5", "gpt-6", "o1", "o3", "o4"]
            .iter()
            .any(|family| id.starts_with(family)),
        codex_harness_build: id.starts_with("codex-") || id.contains("-codex"),
    }
}

fn effort_name(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        // Rejected by `validate_reasoning_controls` before this is reached.
        Effort::Xhigh => "high",
        Effort::Max => "high",
    }
}

fn wants_reasoning_summary(thinking: &ThinkingConfig) -> bool {
    matches!(
        thinking,
        ThinkingConfig::Adaptive {
            display: Some(ThinkingDisplay::Summarized)
        }
    )
}

fn config_error(message: String) -> ProviderFailure {
    ProviderFailure::new(
        FailureClass::Configuration,
        FailureScope::request(),
        message,
    )
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
    let profile = model_profile(&request.model);
    if profile.codex_harness_build {
        return Err(config_error(format!(
            "`{}` names a Codex harness build, not a model id verified on the Responses API; \
             set the route to an exact platform model id",
            request.model
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
            "OpenAI Responses requests require at least one input message".into(),
        ));
    }
    if !request.stop_sequences.is_empty() {
        return Err(config_error(
            "the Responses API has no stop-sequence parameter".into(),
        ));
    }
    if request.cache != CacheMode::Disabled {
        return Err(config_error(
            "OpenAI prompt caching is automatic; the Responses API has no cache_control".into(),
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
    validate_reasoning_controls(request, profile)?;
    validate_content_relationships(request)?;
    Ok(())
}

fn declared_context_window(vendor: &str, model: &str) -> Option<u64> {
    crate::commands::ctx::catalogue::vendor(vendor)
        .and_then(|vendor| crate::commands::ctx::catalogue::context_window(vendor, Some(model)))
}

fn validate_reasoning_controls(
    request: &ProviderRequest,
    profile: ModelProfile,
) -> Result<(), ProviderFailure> {
    if !profile.reasoning {
        if request.effort.is_some() {
            return Err(config_error(format!(
                "model `{}` is not a reasoning model and takes no reasoning effort",
                request.model
            )));
        }
        return match request.thinking {
            ThinkingConfig::Default | ThinkingConfig::Disabled => Ok(()),
            _ => Err(config_error(format!(
                "model `{}` is not a reasoning model and takes no thinking configuration",
                request.model
            ))),
        };
    }
    if matches!(request.effort, Some(Effort::Xhigh | Effort::Max)) {
        return Err(config_error(format!(
            "the Responses API accepts low, medium, or high reasoning effort; `{:?}` is not a \
             documented OpenAI level",
            request.effort.unwrap_or(Effort::Low)
        )));
    }
    match request.thinking {
        ThinkingConfig::Default => Ok(()),
        ThinkingConfig::Disabled => Err(config_error(format!(
            "reasoning cannot be disabled on `{}`; route a non-reasoning model instead",
            request.model
        ))),
        ThinkingConfig::Enabled { .. } => Err(config_error(
            "the Responses API has no manual thinking budget; use reasoning effort".into(),
        )),
        ThinkingConfig::Adaptive {
            display: Some(ThinkingDisplay::Updates),
        } => Err(config_error(
            "streamed thinking-display updates are an Anthropic beta, not a Responses feature"
                .into(),
        )),
        ThinkingConfig::Adaptive { .. } => Ok(()),
    }
}

/// The opaque half of a reasoning item: the complete provider item, kept
/// verbatim so the next turn can replay it byte for byte.
fn reasoning_envelope(signature: &OpaqueProviderData) -> Option<&Map<String, Value>> {
    let object = signature.expose().as_object()?;
    if object.get("type").and_then(Value::as_str) != Some("reasoning") {
        return None;
    }
    if object
        .get("id")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return None;
    }
    Some(object)
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
                "assistant function_call items must be followed immediately by their \
                 function_call_output items"
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
                 function_call_output items"
                    .into(),
            ));
        }
        for block in &message.content {
            match block {
                ProviderContent::ToolUse { id, input, .. } => {
                    if message.role != ProviderMessageRole::Assistant
                        || id.is_empty()
                        || !input.is_object()
                    {
                        return Err(ProviderFailure::new(
                            FailureClass::InvalidToolArguments,
                            FailureScope::request(),
                            "assistant function_call arguments must be one complete JSON object",
                        ));
                    }
                    if !pending.insert(id.clone()) {
                        return Err(config_error(format!(
                            "duplicate unresolved function call_id `{id}`"
                        )));
                    }
                }
                ProviderContent::ToolResult { tool_use_id, .. } => {
                    if !pending.remove(tool_use_id) {
                        return Err(config_error(format!(
                            "function_call_output references unknown call_id `{tool_use_id}`"
                        )));
                    }
                }
                ProviderContent::Thinking { signature, .. } => {
                    if message.role != ProviderMessageRole::Assistant {
                        return Err(config_error(
                            "reasoning items belong to assistant messages".into(),
                        ));
                    }
                    if reasoning_envelope(signature).is_none() {
                        return Err(config_error(
                            "this thinking block does not carry an OpenAI reasoning item \
                             (`type: reasoning` with an id); opaque continuation state from \
                             another provider is never replayed into a Responses request"
                                .into(),
                        ));
                    }
                }
                ProviderContent::RedactedThinking { .. } => {
                    return Err(config_error(
                        "redacted-thinking blocks are Anthropic continuation state and have no \
                         Responses representation"
                            .into(),
                    ));
                }
                ProviderContent::Refusal { .. } => {
                    if message.role != ProviderMessageRole::Assistant {
                        return Err(config_error(
                            "refusal items belong to assistant messages".into(),
                        ));
                    }
                }
                ProviderContent::Text { .. } => {}
            }
        }
        if has_results && !pending.is_empty() {
            return Err(config_error(
                "function_call_output items must resolve every function_call from the preceding \
                 assistant message"
                    .into(),
            ));
        }
    }
    if !pending.is_empty() {
        return Err(config_error(
            "assistant function_call items must be followed by matching function_call_output items"
                .into(),
        ));
    }
    Ok(())
}

// -- Request encoding ----------------------------------------------------

fn encode_input(request: &ProviderRequest) -> Result<Value, ProviderFailure> {
    let mut items: Vec<Value> = Vec::new();
    for message in &request.messages {
        let assistant = message.role == ProviderMessageRole::Assistant;
        let mut parts: Vec<Value> = Vec::new();
        for block in &message.content {
            match block {
                ProviderContent::Text { text } => parts.push(json!({
                    "type": if assistant { "output_text" } else { "input_text" },
                    "text": text,
                })),
                ProviderContent::Refusal { text } => {
                    parts.push(json!({"type":"refusal", "refusal":text}));
                }
                ProviderContent::Thinking { signature, .. } => {
                    flush_message(assistant, &mut parts, &mut items);
                    let envelope = reasoning_envelope(signature).ok_or_else(|| {
                        config_error("reasoning item lost its provider envelope".into())
                    })?;
                    items.push(Value::Object(envelope.clone()));
                }
                ProviderContent::ToolUse { id, name, input } => {
                    flush_message(assistant, &mut parts, &mut items);
                    let arguments = serde_json::to_string(input).map_err(|error| {
                        ProviderFailure::new(
                            FailureClass::InvalidToolArguments,
                            FailureScope::request(),
                            format!("function call `{name}` arguments cannot be encoded: {error}"),
                        )
                    })?;
                    items.push(json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": arguments,
                    }));
                }
                ProviderContent::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    flush_message(assistant, &mut parts, &mut items);
                    // `function_call_output` has no error flag, so a failed
                    // tool keeps its failure in the output text rather than
                    // being replayed as an ordinary success.
                    let output = if *is_error {
                        format!("tool error: {content}")
                    } else {
                        content.clone()
                    };
                    items.push(json!({
                        "type": "function_call_output",
                        "call_id": tool_use_id,
                        "output": output,
                    }));
                }
                ProviderContent::RedactedThinking { .. } => {
                    return Err(config_error(
                        "redacted-thinking blocks have no Responses representation".into(),
                    ));
                }
            }
        }
        flush_message(assistant, &mut parts, &mut items);
    }
    Ok(Value::Array(items))
}

fn flush_message(assistant: bool, parts: &mut Vec<Value>, items: &mut Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    items.push(json!({
        "type": "message",
        "role": if assistant { "assistant" } else { "user" },
        "content": std::mem::take(parts),
    }));
}

// -- Stream accumulation -------------------------------------------------

#[derive(Debug)]
enum PartState {
    OutputText(String),
    Refusal(String),
    Other,
}

#[derive(Debug)]
enum ItemState {
    Message {
        parts: BTreeMap<usize, PartState>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        streamed_arguments: String,
        done_arguments: Option<String>,
    },
    Reasoning {
        id: String,
        summary: BTreeMap<usize, String>,
    },
    Ignored,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Terminal {
    status: String,
    incomplete_reason: Option<String>,
}

#[derive(Default)]
struct Accumulator {
    response_id: Option<String>,
    model: Option<String>,
    items: BTreeMap<usize, ItemState>,
    completed: BTreeMap<(usize, usize), ProviderContent>,
    terminal: Option<Terminal>,
    usage: ProviderUsage,
    saw_event: bool,
}

fn parse_sse<R: BufRead>(
    mut reader: R,
    request_id: Option<String>,
    cancellation: &dyn Cancellation,
    sink: &mut dyn EventSink,
    target: &ProviderTarget,
) -> Result<ProviderResponse, ProviderFailure> {
    let mut accumulator = Accumulator::default();
    let mut event_name = String::new();
    let mut data = String::new();
    let mut line = String::new();
    loop {
        let read = read_sse_line(&mut reader, &mut line, "OpenAI", cancellation, target)?;
        if read == 0 {
            if !event_name.is_empty() || !data.is_empty() {
                process_sse_event(&event_name, &data, &mut accumulator, sink, target)?;
            }
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if !event_name.is_empty() || !data.is_empty() {
                process_sse_event(&event_name, &data, &mut accumulator, sink, target)?;
                event_name.clear();
                data.clear();
            }
            line.clear();
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("event:") {
            event_name = value.trim_start().to_string();
        } else if let Some(value) = trimmed.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim_start());
        }
        line.clear();
    }
    finish_response(accumulator, request_id)
}

fn finish_response(
    accumulator: Accumulator,
    request_id: Option<String>,
) -> Result<ProviderResponse, ProviderFailure> {
    // A stream that stopped without a terminal event is a truncated stream,
    // never a completed turn.
    let terminal = accumulator
        .terminal
        .ok_or_else(|| invalid_stream("OpenAI stream ended before a terminal response event"))?;
    let message_id = accumulator
        .response_id
        .ok_or_else(|| invalid_stream("OpenAI stream had no response id"))?;
    let model = accumulator
        .model
        .ok_or_else(|| invalid_stream("OpenAI stream had no serving model"))?;
    let mut content: Vec<ProviderContent> = accumulator.completed.into_values().collect();
    let mut stop_details = None;
    let finish_reason = match terminal.status.as_str() {
        "completed" => {
            if content
                .iter()
                .any(|block| matches!(block, ProviderContent::ToolUse { .. }))
            {
                FinishReason::ToolUse
            } else if content
                .iter()
                .any(|block| matches!(block, ProviderContent::Refusal { .. }))
            {
                FinishReason::Refusal
            } else {
                FinishReason::EndTurn
            }
        }
        "incomplete" => {
            // A truncated turn never hands the runtime an executable tool
            // call, and the omission is recorded rather than silent.
            let omitted: Vec<String> = content
                .iter()
                .filter_map(|block| match block {
                    ProviderContent::ToolUse { id, .. } => Some(id.clone()),
                    _ => None,
                })
                .collect();
            content.retain(|block| !matches!(block, ProviderContent::ToolUse { .. }));
            let reason = terminal.incomplete_reason.clone().unwrap_or_default();
            stop_details = Some(OpaqueProviderData::new(json!({
                "status": "incomplete",
                "reason": reason,
                "omitted_function_calls": omitted,
            })));
            match reason.as_str() {
                "max_output_tokens" => FinishReason::MaxTokens,
                "content_filter" => FinishReason::Refusal,
                other => FinishReason::Unknown(other.to_string()),
            }
        }
        other => {
            return Err(invalid_stream(format!(
                "OpenAI stream ended with unsupported response status `{other}`"
            )));
        }
    };
    Ok(ProviderResponse {
        message_id,
        model,
        content,
        finish_reason,
        stop_sequence: None,
        stop_details,
        usage: accumulator.usage,
        request_id,
    })
}

fn process_sse_event(
    event_name: &str,
    data: &str,
    accumulator: &mut Accumulator,
    sink: &mut dyn EventSink,
    target: &ProviderTarget,
) -> Result<(), ProviderFailure> {
    let value: Value = serde_json::from_str(data).map_err(|error| {
        invalid_stream(format!(
            "invalid OpenAI SSE JSON for `{event_name}`: {error}"
        ))
    })?;
    accumulator.saw_event = true;
    sink.push(ProviderStreamEvent::ProtocolActivity);
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or(event_name)
        .to_string();
    match kind.as_str() {
        "response.created" => {
            let response = value.get("response").unwrap_or(&Value::Null);
            let id = required_string(response, "id", "response.created")?;
            let model = required_string(response, "model", "response.created")?;
            accumulator.response_id = Some(id.clone());
            accumulator.model = Some(model.clone());
            sink.push(ProviderStreamEvent::MessageStarted { id, model });
        }
        "response.output_item.added" => {
            let index = required_index(&value, "output_index")?;
            if accumulator.items.contains_key(&index)
                || accumulator.completed.keys().any(|(item, _)| *item == index)
            {
                return Err(invalid_stream(format!(
                    "duplicate OpenAI output item index {index}"
                )));
            }
            let item = value.get("item").unwrap_or(&Value::Null);
            let state = match required_string(item, "type", "output_item")?.as_str() {
                "message" => ItemState::Message {
                    parts: BTreeMap::new(),
                },
                "function_call" => ItemState::FunctionCall {
                    call_id: required_string(item, "call_id", "function_call")?,
                    name: required_string(item, "name", "function_call")?,
                    streamed_arguments: item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    done_arguments: None,
                },
                "reasoning" => ItemState::Reasoning {
                    id: required_string(item, "id", "reasoning")?,
                    summary: BTreeMap::new(),
                },
                _ => ItemState::Ignored,
            };
            accumulator.items.insert(index, state);
        }
        "response.content_part.added" => {
            let index = required_index(&value, "output_index")?;
            let content_index = required_index(&value, "content_index")?;
            let ItemState::Message { parts } = open_item(accumulator, index)? else {
                return Err(invalid_stream("content part on a non-message item"));
            };
            let part = value.get("part").unwrap_or(&Value::Null);
            let state = match part.get("type").and_then(Value::as_str).unwrap_or("") {
                "output_text" => PartState::OutputText(
                    part.get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ),
                "refusal" => PartState::Refusal(
                    part.get("refusal")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ),
                _ => PartState::Other,
            };
            parts.insert(content_index, state);
        }
        "response.output_text.delta" | "response.refusal.delta" => {
            let index = required_index(&value, "output_index")?;
            let content_index = required_index(&value, "content_index")?;
            let refusal = kind == "response.refusal.delta";
            let delta = required_string(&value, "delta", &kind)?;
            let ItemState::Message { parts } = open_item(accumulator, index)? else {
                return Err(invalid_stream("text delta on a non-message item"));
            };
            match parts.get_mut(&content_index) {
                Some(PartState::OutputText(buffer)) if !refusal => {
                    check_block_accumulator_cap(buffer.len(), delta.len())?;
                    buffer.push_str(&delta);
                }
                Some(PartState::Refusal(buffer)) if refusal => {
                    check_block_accumulator_cap(buffer.len(), delta.len())?;
                    buffer.push_str(&delta);
                }
                Some(_) => return Err(invalid_stream("delta does not match its content part")),
                None => {
                    return Err(invalid_stream(format!(
                        "delta for unopened OpenAI content part {index}/{content_index}"
                    )));
                }
            }
            if !refusal {
                sink.push(ProviderStreamEvent::TextDelta { index, text: delta });
            }
        }
        "response.function_call_arguments.delta" => {
            let index = required_index(&value, "output_index")?;
            let delta = required_string(&value, "delta", &kind)?;
            let ItemState::FunctionCall {
                streamed_arguments, ..
            } = open_item(accumulator, index)?
            else {
                return Err(invalid_stream(
                    "function-call argument delta on a non-function item",
                ));
            };
            check_block_accumulator_cap(streamed_arguments.len(), delta.len())?;
            streamed_arguments.push_str(&delta);
            sink.push(ProviderStreamEvent::ToolInputDelta {
                index,
                partial_json: delta,
            });
        }
        "response.function_call_arguments.done" => {
            let index = required_index(&value, "output_index")?;
            let arguments = required_string(&value, "arguments", &kind)?;
            let ItemState::FunctionCall { done_arguments, .. } = open_item(accumulator, index)?
            else {
                return Err(invalid_stream(
                    "function-call argument completion on a non-function item",
                ));
            };
            *done_arguments = Some(arguments);
        }
        "response.reasoning_summary_text.delta" => {
            let index = required_index(&value, "output_index")?;
            let summary_index = required_index(&value, "summary_index")?;
            let delta = required_string(&value, "delta", &kind)?;
            let ItemState::Reasoning { summary, .. } = open_item(accumulator, index)? else {
                return Err(invalid_stream(
                    "reasoning summary delta on a non-reasoning item",
                ));
            };
            let buffer = summary.entry(summary_index).or_default();
            check_block_accumulator_cap(buffer.len(), delta.len())?;
            buffer.push_str(&delta);
            sink.push(ProviderStreamEvent::ThinkingDelta { index, text: delta });
        }
        "response.output_item.done" => {
            let index = required_index(&value, "output_index")?;
            let state = accumulator.items.remove(&index).ok_or_else(|| {
                invalid_stream(format!("completion for unopened OpenAI item {index}"))
            })?;
            let item = value.get("item").unwrap_or(&Value::Null);
            for (content_index, block) in finish_item(state, item)? {
                accumulator.completed.insert((index, content_index), block);
            }
            sink.push(ProviderStreamEvent::BlockCompleted { index });
        }
        "response.completed" | "response.incomplete" => {
            let response = value.get("response").unwrap_or(&Value::Null);
            let status = required_string(response, "status", &kind)?;
            if status == "completed" && !accumulator.items.is_empty() {
                return Err(invalid_stream(
                    "OpenAI reported a completed response with an unfinished output item",
                ));
            }
            // Usage is read exactly once, from the terminal event: the
            // per-event payloads are cumulative snapshots, not increments.
            capture_usage(&mut accumulator.usage, response.get("usage"));
            accumulator.terminal = Some(Terminal {
                status,
                incomplete_reason: response
                    .pointer("/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        }
        "response.failed" => {
            let error = value
                .pointer("/response/error")
                .cloned()
                .unwrap_or(Value::Null);
            return Err(classify_error_payload(&error, target));
        }
        "error" => return Err(classify_error_payload(&value, target)),
        _ => {}
    }
    Ok(())
}

fn open_item(
    accumulator: &mut Accumulator,
    index: usize,
) -> Result<&mut ItemState, ProviderFailure> {
    accumulator
        .items
        .get_mut(&index)
        .ok_or_else(|| invalid_stream(format!("event for unopened OpenAI item {index}")))
}

fn finish_item(
    state: ItemState,
    item: &Value,
) -> Result<Vec<(usize, ProviderContent)>, ProviderFailure> {
    match state {
        ItemState::Message { .. } => {
            let mut blocks = Vec::new();
            let parts = item
                .get("content")
                .and_then(Value::as_array)
                .ok_or_else(|| invalid_stream("completed OpenAI message has no content array"))?;
            for (index, part) in parts.iter().enumerate() {
                match part.get("type").and_then(Value::as_str).unwrap_or("") {
                    "output_text" => blocks.push((
                        index,
                        ProviderContent::Text {
                            text: required_string(part, "text", "output_text")?,
                        },
                    )),
                    "refusal" => blocks.push((
                        index,
                        ProviderContent::Refusal {
                            text: required_string(part, "refusal", "refusal")?,
                        },
                    )),
                    _ => {}
                }
            }
            Ok(blocks)
        }
        ItemState::FunctionCall {
            call_id,
            name,
            streamed_arguments,
            done_arguments,
        } => {
            if required_string(item, "call_id", "function_call")? != call_id
                || required_string(item, "name", "function_call")? != name
            {
                return Err(invalid_stream(
                    "completed OpenAI function call changed its identity mid-stream",
                ));
            }
            let arguments = required_string(item, "arguments", "function_call")?;
            if let Some(done) = &done_arguments
                && *done != arguments
            {
                return Err(invalid_stream(
                    "completed OpenAI function call disagrees with its arguments.done event",
                ));
            }
            if !streamed_arguments.is_empty() && streamed_arguments != arguments {
                return Err(invalid_stream(
                    "streamed OpenAI function-call arguments do not match the completed item",
                ));
            }
            let input: Value = serde_json::from_str(&arguments).map_err(|error| {
                ProviderFailure::new(
                    FailureClass::InvalidToolArguments,
                    FailureScope::request(),
                    format!("OpenAI function call `{name}` returned incomplete JSON: {error}"),
                )
            })?;
            if !input.is_object() {
                return Err(ProviderFailure::new(
                    FailureClass::InvalidToolArguments,
                    FailureScope::request(),
                    format!("OpenAI function call `{name}` arguments are not a JSON object"),
                ));
            }
            Ok(vec![(
                0,
                ProviderContent::ToolUse {
                    id: call_id,
                    name,
                    input,
                },
            )])
        }
        ItemState::Reasoning { id, summary } => {
            if required_string(item, "id", "reasoning")? != id {
                return Err(invalid_stream(
                    "completed OpenAI reasoning item changed its id mid-stream",
                ));
            }
            let envelope = item
                .as_object()
                .ok_or_else(|| invalid_stream("completed OpenAI reasoning item is not an object"))?
                .clone();
            let text = item
                .get("summary")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("")
                })
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| summary.into_values().collect::<Vec<_>>().join(""));
            Ok(vec![(
                0,
                ProviderContent::Thinking {
                    thinking: text,
                    signature: OpaqueProviderData::new(Value::Object(envelope)),
                },
            )])
        }
        ItemState::Ignored => Ok(Vec::new()),
    }
}

/// OpenAI reports one cumulative usage object on the terminal event. It has
/// no cache-*write* class, so `cache_creation_input_tokens` stays zero
/// rather than being invented from the cached-read count.
fn capture_usage(usage: &mut ProviderUsage, value: Option<&Value>) {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return;
    };
    usage.input_tokens = value
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    usage.cache_read_input_tokens = value
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    usage.output_tokens = value
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    usage.reasoning_tokens = value
        .pointer("/output_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64);
}

fn required_index(value: &Value, field: &str) -> Result<usize, ProviderFailure> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| invalid_stream(format!("OpenAI event has no valid `{field}`")))
}

fn required_string(value: &Value, field: &str, context: &str) -> Result<String, ProviderFailure> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| invalid_stream(format!("OpenAI {context} has no string `{field}`")))
}

// -- Failure normalization -----------------------------------------------

fn invalid_stream(message: impl Into<String>) -> ProviderFailure {
    super::transport::invalid_stream(message.into())
}

fn check_block_accumulator_cap(
    current_len: usize,
    delta_len: usize,
) -> Result<(), ProviderFailure> {
    super::transport::check_block_accumulator_cap("OpenAI", current_len, delta_len)
}

fn cancelled() -> ProviderFailure {
    super::transport::cancelled("OpenAI")
}

fn timeout_failure(saw_event: bool, target: &ProviderTarget) -> ProviderFailure {
    super::transport::timeout_failure("OpenAI", saw_event, target)
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
    transport_failure(format!("OpenAI transport failed: {error}"), target)
}

fn error_code(value: &Value) -> String {
    value
        .get("code")
        .and_then(Value::as_str)
        .or_else(|| value.get("type").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string()
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
    let code = error_code(&error);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("OpenAI request failed")
        .to_string();
    let lower = message.to_ascii_lowercase();
    let (class, scope_kind, retryable) = match status {
        401 => (
            FailureClass::Authentication,
            FailureScopeKind::Account,
            false,
        ),
        403 => (FailureClass::Permission, FailureScopeKind::Account, false),
        404 => (FailureClass::ModelAccess, FailureScopeKind::Model, false),
        413 => (
            FailureClass::ContextOverflow,
            FailureScopeKind::Request,
            false,
        ),
        // A spent prepaid balance shares 429 with ordinary rate limiting but
        // is an entitlement problem no backoff can clear.
        429 if code == "insufficient_quota" => {
            (FailureClass::Entitlement, FailureScopeKind::Account, false)
        }
        429 => (
            FailureClass::RateLimited,
            FailureScopeKind::BillingPool,
            true,
        ),
        408 => (FailureClass::Transport, FailureScopeKind::Endpoint, true),
        503 => (FailureClass::Overloaded, FailureScopeKind::Provider, true),
        500..=599 => (FailureClass::Provider, FailureScopeKind::Endpoint, true),
        _ if code == "context_length_exceeded"
            || (lower.contains("context") && lower.contains("length")) =>
        {
            (
                FailureClass::ContextOverflow,
                FailureScopeKind::Request,
                false,
            )
        }
        _ if code == "model_not_found" => {
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
    failure.provider_request_id = parsed
        .pointer("/error/request_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or(header_request_id);
    failure.retry = RetryHint {
        retryable,
        after_ms: retry_after_ms,
    };
    failure
}

fn classify_error_payload(value: &Value, target: &ProviderTarget) -> ProviderFailure {
    let code = error_code(value);
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("OpenAI stream failed")
        .to_string();
    let (class, scope_kind, retryable) = match code.as_str() {
        "rate_limit_exceeded" => (
            FailureClass::RateLimited,
            FailureScopeKind::BillingPool,
            true,
        ),
        "insufficient_quota" => (FailureClass::Entitlement, FailureScopeKind::Account, false),
        "invalid_api_key" | "authentication_error" => (
            FailureClass::Authentication,
            FailureScopeKind::Account,
            false,
        ),
        "model_not_found" => (FailureClass::ModelAccess, FailureScopeKind::Model, false),
        "context_length_exceeded" => (
            FailureClass::ContextOverflow,
            FailureScopeKind::Request,
            false,
        ),
        "server_error" => (FailureClass::Provider, FailureScopeKind::Endpoint, true),
        _ => (FailureClass::Provider, FailureScopeKind::Request, false),
    };
    let mut failure = ProviderFailure::new(class, target_scope(target, scope_kind), message);
    failure.provider_request_id = value
        .get("request_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    failure.retry.retryable = retryable;
    failure
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::commands::ctx::provider::adapter::{
        CancellationFlag, NeverCancelled, ProviderMessage, journal_blocks, replayed_content,
    };
    use crate::commands::ctx::provider::anthropic::{AnthropicMessagesAdapter, AnthropicTimeouts};
    use crate::commands::ctx::provider::credential::Secret;
    use crate::commands::ctx::provider::transport::MAX_BLOCK_ACCUMULATOR_BYTES;
    use crate::commands::ctx::provider::{
        AccountId, BillingPoolId, EndpointId, ModelId, ProviderId,
    };
    use crate::commands::ctx::runtime::journal::AssistantBlock;
    use crate::commands::ctx::runtime::tools::ToolRegistry;

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/provider/openai/v1/",
                $name
            ))
        };
    }

    const MULTI_TOOL: &str = fixture!("stream-multi-tool-reasoning.sse");
    const MALFORMED_TOOL: &str = fixture!("stream-malformed-tool.sse");
    const TRUNCATED: &str = fixture!("stream-truncated-tool.sse");
    const INCOMPLETE: &str = fixture!("stream-incomplete.sse");
    const REFUSAL: &str = fixture!("stream-refusal.sse");
    const STREAM_ERROR: &str = fixture!("stream-error.sse");
    const STREAM_FAILED: &str = fixture!("stream-failed.sse");
    const FINAL_TEXT: &str = fixture!("stream-final-text.sse");

    fn target(base_url: String) -> ProviderTarget {
        ProviderTarget {
            route: RouteId::new("work-sol").unwrap(),
            provider: ProviderId::new("openai").unwrap(),
            endpoint: EndpointId::new("openai").unwrap(),
            account: AccountId::new("work").unwrap(),
            billing_pool: BillingPoolId::new("work").unwrap(),
            protocol: Protocol::OpenAiResponses,
            base_url,
            model: ModelId {
                vendor: "openai".into(),
                id: "gpt-5.6-sol".into(),
            },
        }
    }

    fn credential() -> Credential {
        Credential {
            secret: Secret::new(concat!("sk-", "test-secret-never-log").into()),
            expires_at: None,
        }
    }

    fn adapter(base_url: String) -> OpenAiResponsesAdapter {
        OpenAiResponsesAdapter::new(target(base_url), credential(), OpenAiTimeouts::default())
            .unwrap()
    }

    fn request() -> ProviderRequest {
        ProviderRequest {
            model: "gpt-5.6-sol".into(),
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
            effort: Some(Effort::Medium),
            cache: CacheMode::Disabled,
        }
    }

    fn parse(stream: &str) -> Result<ProviderResponse, ProviderFailure> {
        parse_sse(
            BufReader::new(stream.as_bytes()),
            Some("req_header".into()),
            &NeverCancelled,
            &mut Vec::new(),
            &target("https://api.openai.com".into()),
        )
    }

    #[test]
    fn stream_reassembles_reasoning_text_and_multiple_function_calls() {
        let mut events = Vec::new();
        let response = parse_sse(
            BufReader::new(MULTI_TOOL.as_bytes()),
            Some("req_header".into()),
            &NeverCancelled,
            &mut events,
            &target("https://api.openai.com".into()),
        )
        .unwrap();
        assert_eq!(response.message_id, "resp_fixture");
        assert_eq!(response.model, "gpt-5.6-sol");
        assert_eq!(response.finish_reason, FinishReason::ToolUse);
        assert_eq!(response.request_id.as_deref(), Some("req_header"));
        assert_eq!(response.usage.input_tokens, 120);
        assert_eq!(response.usage.cache_read_input_tokens, 60);
        assert_eq!(response.usage.cache_creation_input_tokens, 0);
        assert_eq!(response.usage.output_tokens, 45);
        assert_eq!(response.usage.reasoning_tokens, Some(30));
        assert_eq!(response.content.len(), 4);
        assert!(matches!(
            &response.content[0],
            ProviderContent::Thinking { thinking, signature }
                if thinking == "check both files"
                    && signature.expose()["encrypted_content"] == "opaque-reasoning-blob"
                    && signature.expose()["id"] == "rs_fixture"
        ));
        assert!(matches!(
            &response.content[1],
            ProviderContent::Text { text } if text == "Reading both files."
        ));
        assert!(matches!(
            &response.content[2],
            ProviderContent::ToolUse { id, input, .. }
                if id == "call_1" && input["path"] == "src/lib.rs"
        ));
        assert!(matches!(
            &response.content[3],
            ProviderContent::ToolUse { id, input, .. }
                if id == "call_2" && input["path"] == "Cargo.toml"
        ));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderStreamEvent::ToolInputDelta { index: 2, .. }))
        );
        assert!(!format!("{response:?}").contains("opaque-reasoning-blob"));
    }

    #[test]
    fn malformed_streamed_function_arguments_never_become_a_tool_call() {
        let mut events = Vec::new();
        let error = parse_sse(
            BufReader::new(MALFORMED_TOOL.as_bytes()),
            None,
            &NeverCancelled,
            &mut events,
            &target("https://api.openai.com".into()),
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
    fn a_truncated_stream_is_never_a_completion() {
        let error = parse(TRUNCATED).unwrap_err();
        assert_eq!(error.class, FailureClass::InvalidStream);
        assert!(error.message.contains("terminal response event"));
    }

    #[test]
    fn incomplete_responses_drop_every_function_call_and_record_the_reason() {
        let response = parse(INCOMPLETE).unwrap();
        assert_eq!(response.finish_reason, FinishReason::MaxTokens);
        assert!(
            !response
                .content
                .iter()
                .any(|block| matches!(block, ProviderContent::ToolUse { .. }))
        );
        let details = response
            .stop_details
            .expect("incomplete records its reason");
        assert_eq!(details.expose()["reason"], "max_output_tokens");
        assert_eq!(
            details.expose()["omitted_function_calls"][0],
            "call_partial"
        );
        assert_eq!(response.usage.output_tokens, 4096);
    }

    #[test]
    fn refusal_items_stay_typed_and_never_become_assistant_prose() {
        let response = parse(REFUSAL).unwrap();
        assert_eq!(response.finish_reason, FinishReason::Refusal);
        assert!(matches!(
            &response.content[0],
            ProviderContent::Refusal { text } if text == "I can't help with that."
        ));
    }

    #[test]
    fn stream_error_events_are_typed_with_a_retry_hint() {
        let error = parse(STREAM_ERROR).unwrap_err();
        assert_eq!(error.class, FailureClass::RateLimited);
        assert!(error.retry.retryable);
    }

    #[test]
    fn oversized_content_block_settles_to_invalid_stream() {
        // Each individual SSE line stays well under MAX_SSE_LINE_BYTES; only
        // the cumulative output_text delta payload across many lines crosses
        // MAX_BLOCK_ACCUMULATOR_BYTES, exercising the per-block cap rather
        // than the pre-existing per-line cap.
        let chunk = "a".repeat(900_000);
        let overflow_deltas = MAX_BLOCK_ACCUMULATOR_BYTES / chunk.len() + 2;
        let mut stream = String::from(concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_big\",\"model\":\"gpt-5.6-sol\",\"status\":\"in_progress\"}}\n\n",
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_big\",\"role\":\"assistant\",\"content\":[]}}\n\n",
            "event: response.content_part.added\n",
            "data: {\"type\":\"response.content_part.added\",\"output_index\":0,\"content_index\":0,\"part\":{\"type\":\"output_text\",\"text\":\"\"}}\n\n",
        ));
        for _ in 0..overflow_deltas {
            stream.push_str(&format!(
                "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"{chunk}\"}}\n\n"
            ));
        }
        let error = parse(&stream).unwrap_err();
        assert_eq!(error.class, FailureClass::InvalidStream);
        assert!(
            error.message.contains("content block exceeds"),
            "expected the per-block cap, got: {}",
            error.message
        );
    }

    #[test]
    fn failed_response_events_are_typed_from_their_nested_error() {
        // `response.failed` carries its error under `/response/error`, unlike
        // the top-level `error` event, and it arrives after partial output:
        // the turn must settle to the typed failure, never to a completion
        // holding the half-streamed text.
        let error = parse(STREAM_FAILED).unwrap_err();
        assert_eq!(error.class, FailureClass::Provider);
        assert_eq!(error.scope.kind, FailureScopeKind::Endpoint);
        assert_eq!(error.scope.id.as_deref(), Some("openai"));
        assert!(error.retry.retryable);
        assert!(error.message.contains("The model failed to generate"));
    }

    #[test]
    fn usage_is_captured_once_from_the_terminal_event() {
        // A cumulative snapshot on an in-flight event must not be added to
        // the terminal one.
        let stream = concat!(
            "event: response.created\n",
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_u\",\"model\":\"gpt-5.6-sol\",\"status\":\"in_progress\",\"usage\":null}}\n\n",
            "event: response.in_progress\n",
            "data: {\"type\":\"response.in_progress\",\"response\":{\"id\":\"resp_u\",\"model\":\"gpt-5.6-sol\",\"status\":\"in_progress\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_u\",\"model\":\"gpt-5.6-sol\",\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"input_tokens_details\":{\"cached_tokens\":2},\"output_tokens\":7,\"output_tokens_details\":{\"reasoning_tokens\":3}}}}\n\n",
        );
        let response = parse(stream).unwrap();
        assert_eq!(response.usage.input_tokens, 10);
        assert_eq!(response.usage.output_tokens, 7);
        assert_eq!(response.usage.cache_read_input_tokens, 2);
        assert_eq!(response.usage.reasoning_tokens, Some(3));
        assert_eq!(response.finish_reason, FinishReason::EndTurn);
    }

    #[test]
    fn subscription_credential_classes_are_refused_with_an_actionable_error() {
        for secret in [
            "eyJhbGciOiJIUzI1NiJ9.payload.signature",
            "{\"tokens\":{\"access_token\":\"x\"}}",
            "Bearer chatgpt-session",
        ] {
            let error = OpenAiResponsesAdapter::new(
                target("https://api.openai.com".into()),
                Credential {
                    secret: Secret::new(secret.into()),
                    expires_at: None,
                },
                OpenAiTimeouts::default(),
            )
            .unwrap_err();
            assert_eq!(error.class, FailureClass::Entitlement, "for {secret}");
            assert_eq!(error.scope.kind, FailureScopeKind::Account);
            assert!(error.message.contains("Platform API key"));
            assert!(!error.message.contains(secret));
        }
    }

    #[test]
    fn codex_aliases_and_unsupported_controls_fail_before_transport() {
        let base = target("https://api.openai.com".into());
        let mut codex_target = base.clone();
        codex_target.model.id = "gpt-5-codex".into();
        let mut codex = request();
        codex.model = "gpt-5-codex".into();
        let error = validate_request(&codex, &codex_target).unwrap_err();
        assert_eq!(error.class, FailureClass::Configuration);
        assert!(error.message.contains("Codex harness build"));

        for mutate in [
            (|request: &mut ProviderRequest| request.effort = Some(Effort::Max))
                as fn(&mut ProviderRequest),
            |request| {
                request.thinking = ThinkingConfig::Enabled {
                    budget_tokens: 2048,
                    display: None,
                    interleaved: false,
                }
            },
            |request| {
                request.thinking = ThinkingConfig::Adaptive {
                    display: Some(ThinkingDisplay::Updates),
                }
            },
            |request| request.thinking = ThinkingConfig::Disabled,
            |request| request.cache = CacheMode::Ephemeral5m,
            |request| request.stop_sequences = vec!["STOP".into()],
            |request| request.max_output_tokens = 0,
        ] {
            let mut invalid = request();
            mutate(&mut invalid);
            assert_eq!(
                validate_request(&invalid, &base).unwrap_err().class,
                FailureClass::Configuration
            );
        }
    }

    #[test]
    fn continuation_requires_openai_reasoning_items_and_exact_call_relationships() {
        let base = target("https://api.openai.com".into());
        let mut continued = request();
        continued.messages.push(ProviderMessage {
            role: ProviderMessageRole::Assistant,
            content: vec![
                ProviderContent::Thinking {
                    thinking: "plan".into(),
                    signature: OpaqueProviderData::new(
                        json!({"type":"reasoning","id":"rs_1","encrypted_content":"blob"}),
                    ),
                },
                ProviderContent::ToolUse {
                    id: "call_1".into(),
                    name: "file_read".into(),
                    input: json!({"path":"README.md"}),
                },
            ],
        });
        continued.messages.push(ProviderMessage {
            role: ProviderMessageRole::User,
            content: vec![ProviderContent::ToolResult {
                tool_use_id: "call_1".into(),
                content: "result".into(),
                is_error: false,
            }],
        });
        validate_request(&continued, &base).unwrap();

        let mut foreign = continued.clone();
        foreign.messages[1].content[0] = ProviderContent::Thinking {
            thinking: "plan".into(),
            signature: OpaqueProviderData::new(json!("anthropic-signature")),
        };
        assert_eq!(
            validate_request(&foreign, &base).unwrap_err().class,
            FailureClass::Configuration
        );

        let mut redacted = continued.clone();
        redacted.messages[1].content[0] = ProviderContent::RedactedThinking {
            data: OpaqueProviderData::new(json!("anthropic-redacted")),
        };
        assert_eq!(
            validate_request(&redacted, &base).unwrap_err().class,
            FailureClass::Configuration
        );

        let mut unmatched = continued;
        let ProviderContent::ToolResult { tool_use_id, .. } = &mut unmatched.messages[2].content[0]
        else {
            unreachable!();
        };
        *tool_use_id = "call_other".into();
        assert_eq!(
            validate_request(&unmatched, &base).unwrap_err().class,
            FailureClass::Configuration
        );
    }

    #[test]
    fn same_route_replay_preserves_reasoning_items_and_cannot_leak_across_providers() {
        let response = parse(MULTI_TOOL).unwrap();
        let blocks = journal_blocks(&response.content).unwrap();
        // Survives the journal's own serialization round trip.
        let stored = serde_json::to_string(&blocks).unwrap();
        assert!(!format!("{blocks:?}").contains("opaque-reasoning-blob"));
        let restored: Vec<AssistantBlock> = serde_json::from_str(&stored).unwrap();
        let replayed: Vec<ProviderContent> = restored.iter().filter_map(replayed_content).collect();
        assert_eq!(replayed[0], response.content[0]);

        let mut continued = request();
        continued.messages.push(ProviderMessage {
            role: ProviderMessageRole::Assistant,
            content: vec![replayed[0].clone()],
        });
        let body = adapter("https://api.openai.com".into())
            .encode_request(&continued)
            .unwrap()
            .body;
        assert_eq!(body["input"][1]["type"], "reasoning");
        assert_eq!(
            body["input"][1]["encrypted_content"],
            "opaque-reasoning-blob"
        );
        assert_eq!(body["store"], json!(false));

        // The same replayed state can never reach an Anthropic request.
        let anthropic = AnthropicMessagesAdapter::new(
            anthropic_target(),
            credential(),
            AnthropicTimeouts::default(),
        )
        .unwrap();
        let mut crossed = continued;
        crossed.model = "claude-sonnet-5".into();
        crossed.thinking = ThinkingConfig::Default;
        crossed.effort = None;
        let error = anthropic
            .stream(&crossed, &NeverCancelled, &mut Vec::new())
            .unwrap_err();
        assert_eq!(error.class, FailureClass::Configuration);
    }

    fn anthropic_target() -> ProviderTarget {
        let mut target = target("https://api.anthropic.com".into());
        target.protocol = Protocol::AnthropicMessages;
        target.provider = ProviderId::new("anthropic").unwrap();
        target.model = ModelId {
            vendor: "anthropic".into(),
            id: "claude-sonnet-5".into(),
        };
        target
    }

    #[test]
    fn direct_http_request_uses_the_responses_api_and_owns_conversation_state() {
        let (url, captured) = one_shot_server(200, MULTI_TOOL, &[]);
        let adapter = adapter(url);
        let response = adapter
            .stream(&request(), &NeverCancelled, &mut Vec::new())
            .unwrap();
        assert_eq!(response.finish_reason, FinishReason::ToolUse);
        let sent = captured.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(sent.starts_with("POST /v1/responses HTTP/1.1"));
        assert!(sent.to_ascii_lowercase().contains(concat!(
            "authorization: bearer sk-",
            "test-secret-never-log"
        )));
        assert!(sent.contains("\"stream\":true"));
        assert!(sent.contains("\"store\":false"));
        assert!(sent.contains("\"include\":[\"reasoning.encrypted_content\"]"));
        assert!(sent.contains("\"reasoning\":{\"effort\":\"medium\",\"summary\":\"auto\"}"));
        assert!(sent.contains("\"type\":\"function\""));
        assert!(!sent.contains("previous_response_id"));
        assert!(!sent.contains("resource_claims"));
        assert!(!sent.contains("execution_mode"));
        assert!(!sent.contains("codex"));
    }

    #[test]
    fn a_multi_turn_tool_interaction_completes_with_no_codex_binary() {
        let (first_url, first_capture) = one_shot_server(200, MULTI_TOOL, &[]);
        let first = adapter(first_url)
            .stream(&request(), &NeverCancelled, &mut Vec::new())
            .unwrap();
        assert_eq!(first.finish_reason, FinishReason::ToolUse);
        let calls: Vec<(String, String)> = first
            .content
            .iter()
            .filter_map(|block| match block {
                ProviderContent::ToolUse { id, name, .. } => Some((id.clone(), name.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2);

        // Zirv executes the tools; the provider never does.
        let mut second = request();
        second.messages.push(ProviderMessage {
            role: ProviderMessageRole::Assistant,
            content: first.content.clone(),
        });
        second.messages.push(ProviderMessage {
            role: ProviderMessageRole::User,
            content: calls
                .iter()
                .map(|(id, _)| ProviderContent::ToolResult {
                    tool_use_id: id.clone(),
                    content: "file body".into(),
                    is_error: false,
                })
                .collect(),
        });
        let (second_url, second_capture) = one_shot_server(200, FINAL_TEXT, &[]);
        let final_turn = adapter(second_url)
            .stream(&second, &NeverCancelled, &mut Vec::new())
            .unwrap();
        assert_eq!(final_turn.finish_reason, FinishReason::EndTurn);
        assert!(matches!(
            &final_turn.content[0],
            ProviderContent::Text { text } if text == "Both files read."
        ));

        let first_body = first_capture.recv_timeout(Duration::from_secs(2)).unwrap();
        let second_body = second_capture.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!first_body.contains("previous_response_id"));
        assert!(second_body.contains("\"type\":\"function_call_output\""));
        assert!(second_body.contains("\"call_id\":\"call_2\""));
        assert!(second_body.contains("\"type\":\"reasoning\""));
    }

    #[test]
    fn status_failures_have_stable_class_scope_request_id_and_retry_hint() {
        let target = target("https://api.openai.com".into());
        for (status, body, class, scope, retryable) in [
            (
                401,
                r#"{"error":{"message":"bad key","code":"invalid_api_key","request_id":"req_auth"}}"#,
                FailureClass::Authentication,
                FailureScopeKind::Account,
                false,
            ),
            (
                403,
                r#"{"error":{"message":"no access","code":"forbidden","request_id":"req_access"}}"#,
                FailureClass::Permission,
                FailureScopeKind::Account,
                false,
            ),
            (
                429,
                r#"{"error":{"message":"slow down","code":"rate_limit_exceeded","request_id":"req_rate"}}"#,
                FailureClass::RateLimited,
                FailureScopeKind::BillingPool,
                true,
            ),
            (
                429,
                r#"{"error":{"message":"quota","code":"insufficient_quota","request_id":"req_quota"}}"#,
                FailureClass::Entitlement,
                FailureScopeKind::Account,
                false,
            ),
            (
                503,
                r#"{"error":{"message":"overloaded","code":"server_error","request_id":"req_busy"}}"#,
                FailureClass::Overloaded,
                FailureScopeKind::Provider,
                true,
            ),
            (
                500,
                r#"{"error":{"message":"internal","code":"server_error","request_id":"req_500"}}"#,
                FailureClass::Provider,
                FailureScopeKind::Endpoint,
                true,
            ),
            (
                400,
                r#"{"error":{"message":"too long","code":"context_length_exceeded","request_id":"req_ctx"}}"#,
                FailureClass::ContextOverflow,
                FailureScopeKind::Request,
                false,
            ),
            (
                400,
                r#"{"error":{"message":"bad param","code":"invalid_request_error","request_id":"req_bad"}}"#,
                FailureClass::Configuration,
                FailureScopeKind::Request,
                false,
            ),
        ] {
            let failure =
                classify_http_error(status, body, Some("header".into()), Some(2_000), &target);
            assert_eq!(failure.class, class, "status {status}");
            assert_eq!(failure.scope.kind, scope, "status {status}");
            assert_eq!(failure.retry.retryable, retryable, "status {status}");
            assert!(failure.provider_request_id.unwrap().starts_with("req_"));
        }
    }

    #[test]
    fn http_retry_after_is_carried_on_rate_limits() {
        let (url, _) = one_shot_server(
            429,
            r#"{"error":{"message":"slow","code":"rate_limit_exceeded","request_id":"req_429"}}"#,
            &[("Retry-After", "3")],
        );
        let error = adapter(url)
            .stream(&request(), &NeverCancelled, &mut Vec::new())
            .unwrap_err();
        assert_eq!(error.class, FailureClass::RateLimited);
        assert_eq!(error.retry.after_ms, Some(3_000));
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
    fn first_event_idle_and_in_flight_cancellation_are_enforced() {
        let (url, _) = delayed_stream_server("", MULTI_TOOL, Duration::from_millis(100));
        let error = OpenAiResponsesAdapter::new(
            target(url),
            credential(),
            OpenAiTimeouts {
                connect: Duration::from_secs(1),
                first_event: Duration::from_millis(20),
                idle: Duration::from_secs(1),
            },
        )
        .unwrap()
        .stream(&request(), &NeverCancelled, &mut Vec::new())
        .unwrap_err();
        assert_eq!(error.class, FailureClass::FirstEventTimeout);

        let split = MULTI_TOOL
            .find("event: response.output_item.added")
            .unwrap();
        let (url, _) = delayed_stream_server(
            &MULTI_TOOL[..split],
            &MULTI_TOOL[split..],
            Duration::from_millis(100),
        );
        let error = OpenAiResponsesAdapter::new(
            target(url),
            credential(),
            OpenAiTimeouts {
                connect: Duration::from_secs(1),
                first_event: Duration::from_secs(1),
                idle: Duration::from_millis(20),
            },
        )
        .unwrap()
        .stream(&request(), &NeverCancelled, &mut Vec::new())
        .unwrap_err();
        assert_eq!(error.class, FailureClass::IdleTimeout);

        let (url, _) = delayed_stream_server("", MULTI_TOOL, Duration::from_millis(100));
        let flag = Arc::new(CancellationFlag::default());
        let canceller = Arc::clone(&flag);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            canceller.cancel();
        });
        let error = adapter(url)
            .stream(&request(), flag.as_ref(), &mut Vec::new())
            .unwrap_err();
        assert_eq!(error.class, FailureClass::Cancelled);
    }

    #[test]
    fn cancellation_mid_stream_tears_down_the_worker_promptly() {
        let split = FINAL_TEXT
            .find("event: response.output_item.added")
            .unwrap();
        let (url, server_observed_close) = slow_body_server(&FINAL_TEXT[..split]);
        let adapter = OpenAiResponsesAdapter::new(
            target(url),
            credential(),
            OpenAiTimeouts {
                connect: Duration::from_secs(1),
                first_event: Duration::from_secs(5),
                idle: Duration::from_secs(5),
            },
        )
        .unwrap();
        let flag = Arc::new(CancellationFlag::default());
        let canceller = Arc::clone(&flag);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            canceller.cancel();
        });
        let error = adapter
            .stream(&request(), flag.as_ref(), &mut Vec::new())
            .unwrap_err();
        assert_eq!(error.class, FailureClass::Cancelled);

        // perform() already returned; the worker's TCP read must not linger
        // for anywhere near ureq's own (multi-second) recv-body budget.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !server_observed_close.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "worker did not close its TCP connection within 2s of cancellation"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
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

    /// Sends `prefix` but declares a much larger `Content-Length` and then
    /// never sends the rest, simulating a connection stalled mid-stream.
    /// Reports (via the returned flag) whether it observed the client side of
    /// the connection close, which is how a test can prove a cancelled worker
    /// actually tore down its TCP read instead of leaking it.
    fn slow_body_server(prefix: &'static str) -> (String, Arc<AtomicBool>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let closed = Arc::new(AtomicBool::new(false));
        let server_closed = Arc::clone(&closed);
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_http_request(&mut stream);
            let declared_length = prefix.len() + 4096;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {declared_length}\r\nConnection: close\r\n\r\n{prefix}"
            )
            .unwrap();
            stream.flush().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut buffer = [0u8; 64];
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => server_closed.store(true, Ordering::Release),
                Ok(_) => {}
            }
        });
        (format!("http://{address}"), closed)
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
            if expected.is_none()
                && let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
            {
                let headers = String::from_utf8_lossy(&bytes[..end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                expected = Some(end + 4 + length);
            }
            if expected.is_some_and(|expected| bytes.len() >= expected) {
                break;
            }
        }
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    #[ignore = "live OpenAI contract; set OPENAI_API_KEY and ZIRV_OPENAI_LIVE_MODEL"]
    fn live_openai_responses_contract() {
        let key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY");
        let model = std::env::var("ZIRV_OPENAI_LIVE_MODEL")
            .expect("ZIRV_OPENAI_LIVE_MODEL must name an entitled exact API model id");
        let mut target = target("https://api.openai.com".into());
        target.model.id = model.clone();
        let adapter = OpenAiResponsesAdapter::new(
            target,
            Credential {
                secret: Secret::new(key),
                expires_at: None,
            },
            OpenAiTimeouts::default(),
        )
        .unwrap();
        let mut request = request();
        request.model = model;
        request.messages = vec![ProviderMessage {
            role: ProviderMessageRole::User,
            content: vec![ProviderContent::Text {
                text: "Reply with exactly: zirv-live-ok".into(),
            }],
        }];
        request.tools.clear();
        request.thinking = ThinkingConfig::Default;
        request.effort = None;
        let response = adapter
            .stream(&request, &NeverCancelled, &mut Vec::new())
            .unwrap();
        assert!(!response.message_id.is_empty());
        assert!(response.usage.output_tokens > 0);
    }
}
