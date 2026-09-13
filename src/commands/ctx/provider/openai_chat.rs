//! Direct OpenAI **chat completions** transport (issue #482, roadmap N13).
//!
//! `POST {base}/v1/chat/completions` over raw HTTPS/SSE is what most
//! compatible vendors and every local runtime in the profile registry speak,
//! so one transport serves DeepSeek, xAI, Qwen, Moonshot, Mistral, Zhipu,
//! MiniMax, Meta, Ollama, LM Studio, vLLM and any operator-declared
//! compatible endpoint -- plus Azure OpenAI, which is the same body behind a
//! different address and a different auth header.
//!
//! It mirrors `openai.rs`: request validation happens before a socket is
//! opened, the blocking read runs under `transport::supervise`, and every
//! failure is one of the shared `FailureClass` values. What differs is the
//! contract the *profile* declares. A compatible endpoint is not a feature
//! superset of OpenAI's: reasoning text that arrives as `reasoning_content`
//! is display-only here (there is no signature to replay), caching has no
//! wire representation, and tools exist only where the profile says the
//! vendor implements them. Anything a request asks for that the profile does
//! not declare is refused with a typed failure rather than dropped.

#![allow(dead_code)] // Route selection reaches this adapter through native.rs.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader};

use serde_json::{Map, Value, json};

use super::adapter::{
    CacheMode, Cancellation, Effort, EventSink, FailureClass, FailureScope, FailureScopeKind,
    FinishReason, ProviderAdapter, ProviderContent, ProviderFailure, ProviderMessageRole,
    ProviderRequest, ProviderResponse, ProviderStreamEvent, ProviderTarget, ProviderUsage,
    RetryHint, ThinkingConfig, resolve_target,
};
use super::config::NativeConfig;
use super::credential::{Credential, CredentialStore};
use super::probe::{is_local_http_host, is_plaintext_non_loopback};
use super::profiles::{CredentialClass, RouteProfile, profile_for, validate_extensions};
use super::transport::{
    MAX_ERROR_BODY_BYTES, StreamTimeouts, WORKER_READ_POLL, parse_retry_after_ms, read_sse_line,
    supervise, target_scope,
};
use super::{OpaqueProviderData, Protocol, RouteId, Support};
use crate::commands::ctx::config::EnvLookup;

const PROVIDER: &str = "chat-completions";

/// How the request is addressed. Azure is a separate variant rather than a
/// base-URL substitution: its path is built from a deployment id, its
/// api-version is mandatory, and its key rides a different header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatEndpoint {
    Compatible,
    Azure {
        deployment: String,
        api_version: String,
    },
}

#[derive(Clone)]
pub struct OpenAiChatAdapter {
    target: ProviderTarget,
    /// `None` only when the bound profile's credential class says the
    /// endpoint takes none. Nothing is ever fabricated to fill it in.
    credential: Option<Credential>,
    timeouts: StreamTimeouts,
    profile: &'static RouteProfile,
    endpoint: ChatEndpoint,
    extensions: BTreeMap<String, Value>,
}

impl std::fmt::Debug for OpenAiChatAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiChatAdapter")
            .field("target", &self.target)
            .field("profile", &self.profile.id)
            .field("endpoint", &self.endpoint)
            .field(
                "credential",
                &self.credential.as_ref().map(|_| "[redacted]"),
            )
            .field("timeouts", &self.timeouts)
            .finish()
    }
}

impl OpenAiChatAdapter {
    pub fn from_config(
        config: &NativeConfig,
        route: &RouteId,
        env: EnvLookup<'_>,
        store: &dyn CredentialStore,
        now: u64,
        timeouts: StreamTimeouts,
    ) -> Result<Self, ProviderFailure> {
        let (target, credential) = resolve_target(config, route, env, store, now)?;
        let endpoints = config.effective_endpoints();
        let vendor = endpoints
            .get(&target.endpoint)
            .map(|endpoint| endpoint.vendor.clone())
            .unwrap_or_default();
        let profile = profile_for(target.provider.as_ref(), &vendor).ok_or_else(|| {
            config_error(format!(
                "route `{route}` has no route profile for vendor `{vendor}`"
            ))
        })?;
        let route_config = config
            .routes
            .get(route)
            .ok_or_else(|| config_error(format!("unknown native route `{route}`")))?;
        let extensions = validate_extensions(profile, &route_config.extensions)
            .map_err(|error| config_error(format!("route `{route}` extensions: {error}")))?;
        let endpoint = match target.protocol {
            Protocol::AzureOpenAiChat => {
                let account = config.accounts.get(&target.account).ok_or_else(|| {
                    config_error(format!("unknown account `{}`", target.account))
                })?;
                ChatEndpoint::Azure {
                    deployment: route_config.deployment.clone().ok_or_else(|| {
                        config_error(format!("route `{route}` has no Azure deployment"))
                    })?,
                    api_version: account.api_version.clone().ok_or_else(|| {
                        config_error(format!(
                            "account `{}` has no Azure api_version",
                            target.account
                        ))
                    })?,
                }
            }
            _ => ChatEndpoint::Compatible,
        };
        Self::new(target, credential, timeouts, profile, endpoint, extensions)
    }

    pub fn new(
        target: ProviderTarget,
        credential: Option<Credential>,
        timeouts: StreamTimeouts,
        profile: &'static RouteProfile,
        endpoint: ChatEndpoint,
        extensions: BTreeMap<String, Value>,
    ) -> Result<Self, ProviderFailure> {
        if !matches!(
            target.protocol,
            Protocol::OpenAiChatCompatible | Protocol::AzureOpenAiChat
        ) {
            return Err(config_error(format!(
                "the chat-completions adapter requires a chat target, not {:?}",
                target.protocol
            )));
        }
        if target.protocol != profile.protocol {
            return Err(config_error(format!(
                "route profile `{}` speaks {:?}, not the target's {:?}",
                profile.id, profile.protocol, target.protocol
            )));
        }
        if matches!(target.protocol, Protocol::AzureOpenAiChat)
            != matches!(endpoint, ChatEndpoint::Azure { .. })
        {
            return Err(config_error(
                "an Azure target must carry an Azure endpoint and vice versa".into(),
            ));
        }
        if let Support::LegacyOnly(reason) = profile.support {
            return Err(ProviderFailure::new(
                FailureClass::Entitlement,
                FailureScope {
                    kind: FailureScopeKind::Provider,
                    id: Some(target.provider.to_string()),
                },
                format!(
                    "route profile `{}` has no direct API: {reason}",
                    profile.id
                ),
            ));
        }
        if timeouts.has_zero() {
            return Err(config_error(
                "chat-completions timeouts must be greater than zero".into(),
            ));
        }
        if target.base_url.starts_with("http://") && !is_local_http_host(&target.base_url) {
            return Err(endpoint_error(
                &target,
                "a public host is never addressed over plaintext HTTP; only a loopback or private \
                 address may be reached without TLS"
                    .into(),
            ));
        }
        if credential.is_some() && is_plaintext_non_loopback(&target.base_url) {
            return Err(endpoint_error(
                &target,
                "a credential is never sent over plaintext HTTP to a non-loopback host".into(),
            ));
        }
        match &credential {
            Some(credential) => reject_broker_credential(credential.secret.expose(), &target)?,
            None if profile.credential.is_optional() => {}
            None => {
                return Err(ProviderFailure::new(
                    FailureClass::Authentication,
                    FailureScope {
                        kind: FailureScopeKind::Account,
                        id: Some(target.account.to_string()),
                    },
                    format!(
                        "account `{}` has no API credential, which route profile `{}` requires",
                        target.account, profile.id
                    ),
                ));
            }
        }
        Ok(Self {
            target,
            credential,
            timeouts,
            profile,
            endpoint,
            extensions,
        })
    }

    pub fn profile(&self) -> &'static RouteProfile {
        self.profile
    }

    fn request_url(&self) -> String {
        let base = self.target.base_url.trim_end_matches('/');
        match &self.endpoint {
            ChatEndpoint::Compatible => format!("{base}{}", self.profile.path),
            ChatEndpoint::Azure {
                deployment,
                api_version,
            } => format!(
                "{base}/openai/deployments/{deployment}/chat/completions?api-version={api_version}"
            ),
        }
    }

    fn encode_request(&self, request: &ProviderRequest) -> Result<Value, ProviderFailure> {
        validate_request(request, &self.target, self.profile)?;
        let mut body = Map::new();
        // Azure addresses the model by deployment; sending a model id there
        // would name something the deployment may not serve.
        if matches!(self.endpoint, ChatEndpoint::Compatible) {
            body.insert("model".into(), Value::String(request.model.clone()));
        }
        body.insert("stream".into(), Value::Bool(true));
        body.insert(
            "stream_options".into(),
            json!({ "include_usage": true }),
        );
        body.insert("max_tokens".into(), json!(request.max_output_tokens));
        body.insert("messages".into(), encode_messages(request)?);
        if !request.stop_sequences.is_empty() {
            body.insert("stop".into(), json!(request.stop_sequences));
        }
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
                                "function": {
                                    "name": tool.name,
                                    "description": tool.description,
                                    "parameters": tool.input_schema,
                                },
                            })
                        })
                        .collect(),
                ),
            );
            // A local server rejects unknown top-level keys more often than
            // it implements this one, so it is sent only where the vendor
            // documents it.
            if self.profile.caveats.parallel_tool_calls && !self.profile.credential.is_local() {
                body.insert("parallel_tool_calls".into(), Value::Bool(true));
            }
        }
        if let Some(effort) = request.effort {
            body.insert(
                "reasoning_effort".into(),
                Value::String(effort_name(effort).into()),
            );
        }
        for (key, value) in &self.extensions {
            if body.contains_key(key) {
                return Err(config_error(format!(
                    "extension `{key}` would overwrite a protocol-owned request field"
                )));
            }
            body.insert(key.clone(), value.clone());
        }
        Ok(Value::Object(body))
    }

    fn perform_blocking(
        &self,
        body: &Value,
        cancellation: &dyn Cancellation,
        sink: &mut dyn EventSink,
    ) -> Result<ProviderResponse, ProviderFailure> {
        if cancellation.is_cancelled() {
            return Err(super::transport::cancelled(PROVIDER));
        }
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(self.timeouts.connect))
            .timeout_recv_response(Some(self.timeouts.first_event))
            .timeout_recv_body(Some(WORKER_READ_POLL))
            .build()
            .into();
        let payload = serde_json::to_string(body).map_err(|error| {
            config_error(format!("failed to encode chat request: {error}"))
        })?;
        let mut http = agent
            .post(self.request_url())
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header("user-agent", format!("zirv/{}", env!("CARGO_PKG_VERSION")));
        if let Some(credential) = &self.credential {
            let secret = credential.secret.expose();
            http = match self.profile.credential {
                CredentialClass::HeaderApiKey { header } => http.header(header, secret),
                _ => http.header("authorization", format!("Bearer {secret}")),
            };
        }
        let mut response = http
            .send(payload)
            .map_err(|error| classify_transport_error(error, false, &self.target))?;
        let status = response.status().as_u16();
        let request_id = response
            .headers()
            .get("x-request-id")
            .or_else(|| response.headers().get("apim-request-id"))
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
                .unwrap_or_else(|_| "the endpoint returned an unreadable error body".into());
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
}

impl ProviderAdapter for OpenAiChatAdapter {
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
        let body = self.encode_request(request)?;
        let adapter = self.clone();
        supervise(
            PROVIDER,
            "zirv-chat-stream",
            self.timeouts,
            &self.target,
            cancellation,
            sink,
            move |worker_cancellation, worker_sink| {
                adapter.perform_blocking(&body, worker_cancellation, worker_sink)
            },
        )
    }
}

// -- credentials ---------------------------------------------------------

/// A compatible endpoint takes a vendor API key. A harness login blob -- a
/// whole `auth.json`, an OAuth JWT, an already-prefixed `Bearer ...` -- is a
/// different identity with different billing, so it is refused by shape
/// rather than forwarded and charged somewhere unexpected.
fn reject_broker_credential(secret: &str, target: &ProviderTarget) -> Result<(), ProviderFailure> {
    let account_scope = || FailureScope {
        kind: FailureScopeKind::Account,
        id: Some(target.account.to_string()),
    };
    let trimmed = secret.trim();
    if trimmed.is_empty() {
        return Err(ProviderFailure::new(
            FailureClass::Authentication,
            account_scope(),
            format!("account `{}` has an empty API key", target.account),
        ));
    }
    if trimmed.starts_with('{')
        || trimmed.starts_with("eyJ")
        || trimmed.to_ascii_lowercase().starts_with("bearer ")
    {
        return Err(ProviderFailure::new(
            FailureClass::Entitlement,
            account_scope(),
            format!(
                "account `{}` is configured with a subscription/harness login blob rather than a \
                 vendor API key; broker entitlements are never spent as API billing",
                target.account
            ),
        ));
    }
    Ok(())
}

// -- validation ----------------------------------------------------------

fn config_error(message: String) -> ProviderFailure {
    ProviderFailure::new(
        FailureClass::Configuration,
        FailureScope::request(),
        message,
    )
}

fn endpoint_error(target: &ProviderTarget, message: String) -> ProviderFailure {
    ProviderFailure::new(
        FailureClass::Configuration,
        FailureScope {
            kind: FailureScopeKind::Endpoint,
            id: Some(target.endpoint.to_string()),
        },
        message,
    )
}

fn effort_name(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High | Effort::Xhigh | Effort::Max => "high",
    }
}

fn validate_request(
    request: &ProviderRequest,
    target: &ProviderTarget,
    profile: &RouteProfile,
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
    if let Some(window) = crate::commands::ctx::catalogue::vendor(&target.model.vendor)
        .and_then(|vendor| {
            crate::commands::ctx::catalogue::context_window(vendor, Some(&request.model))
        })
        && request.max_output_tokens >= window
    {
        return Err(config_error(format!(
            "max_output_tokens {} does not fit the declared {window}-token context window of `{}`",
            request.max_output_tokens, request.model
        )));
    }
    if request.messages.is_empty() {
        return Err(config_error(
            "chat-completions requests require at least one message".into(),
        ));
    }
    if request.cache != CacheMode::Disabled {
        return Err(config_error(
            "the chat-completions body has no cache_control; prompt caching, where a vendor has \
             it, is automatic"
                .into(),
        ));
    }
    if !request.tools.is_empty() && !profile.caveats.tools {
        return Err(config_error(format!(
            "route profile `{}` does not implement function calling, so a task that needs tools \
             cannot run on it",
            profile.id
        )));
    }
    for tool in &request.tools {
        if !tool.input_schema.is_object() {
            return Err(config_error(format!(
                "tool `{}` input_schema must be a JSON object",
                tool.name
            )));
        }
    }
    if request.effort.is_some() && !profile.caveats.reasoning_controls {
        return Err(config_error(format!(
            "route profile `{}` accepts no reasoning-effort control",
            profile.id
        )));
    }
    match request.thinking {
        ThinkingConfig::Default | ThinkingConfig::Disabled => {}
        _ if profile.caveats.reasoning_controls => {}
        _ => {
            return Err(config_error(format!(
                "route profile `{}` has no thinking configuration; any reasoning it emits is \
                 display-only",
                profile.id
            )));
        }
    }
    validate_content_relationships(request, profile)
}

fn validate_content_relationships(
    request: &ProviderRequest,
    profile: &RouteProfile,
) -> Result<(), ProviderFailure> {
    let mut pending: BTreeSet<String> = BTreeSet::new();
    for message in &request.messages {
        let has_results = message
            .content
            .iter()
            .any(|block| matches!(block, ProviderContent::ToolResult { .. }));
        if !pending.is_empty() && !has_results {
            return Err(config_error(
                "assistant tool_calls must be answered by their tool messages before the next \
                 turn"
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
                "tool-result continuation messages carry only tool results".into(),
            ));
        }
        for block in &message.content {
            match block {
                ProviderContent::Text { .. } => {}
                ProviderContent::Refusal { .. } => {
                    if message.role != ProviderMessageRole::Assistant {
                        return Err(config_error(
                            "a refusal is assistant content".into(),
                        ));
                    }
                }
                ProviderContent::ToolUse { id, input, .. } => {
                    if message.role != ProviderMessageRole::Assistant
                        || id.is_empty()
                        || !input.is_object()
                    {
                        return Err(ProviderFailure::new(
                            FailureClass::InvalidToolArguments,
                            FailureScope::request(),
                            "assistant tool_call arguments must be one complete JSON object",
                        ));
                    }
                    if !pending.insert(id.clone()) {
                        return Err(config_error(format!(
                            "duplicate unresolved tool_call id `{id}`"
                        )));
                    }
                }
                ProviderContent::ToolResult { tool_use_id, .. } => {
                    if !pending.remove(tool_use_id) {
                        return Err(config_error(format!(
                            "tool message references unknown tool_call id `{tool_use_id}`"
                        )));
                    }
                }
                ProviderContent::Thinking { .. } => {
                    if !profile.caveats.reasoning_replay {
                        return Err(config_error(format!(
                            "route profile `{}` carries no replayable reasoning: a chat-completions \
                             endpoint emits reasoning text without a signature, so it is shown but \
                             never sent back as continuation state",
                            profile.id
                        )));
                    }
                }
                ProviderContent::RedactedThinking { .. } => {
                    return Err(config_error(
                        "redacted-thinking blocks are Anthropic continuation state and have no \
                         chat-completions representation"
                            .into(),
                    ));
                }
            }
        }
    }
    if !pending.is_empty() {
        return Err(config_error(
            "assistant tool_calls must be followed by matching tool messages".into(),
        ));
    }
    Ok(())
}

// -- request encoding ----------------------------------------------------

fn encode_messages(request: &ProviderRequest) -> Result<Value, ProviderFailure> {
    let mut items: Vec<Value> = Vec::new();
    if !request.system.is_empty() {
        items.push(json!({"role":"system", "content": request.system.join("\n\n")}));
    }
    for message in &request.messages {
        let assistant = message.role == ProviderMessageRole::Assistant;
        let mut text = String::new();
        let mut refusal: Option<String> = None;
        let mut tool_calls: Vec<Value> = Vec::new();
        let mut tool_messages: Vec<Value> = Vec::new();
        for block in &message.content {
            match block {
                ProviderContent::Text { text: chunk } => {
                    if !text.is_empty() {
                        text.push_str("\n\n");
                    }
                    text.push_str(chunk);
                }
                ProviderContent::Refusal { text } => refusal = Some(text.clone()),
                ProviderContent::ToolUse { id, name, input } => {
                    let arguments = serde_json::to_string(input).map_err(|error| {
                        ProviderFailure::new(
                            FailureClass::InvalidToolArguments,
                            FailureScope::request(),
                            format!("tool call `{name}` arguments cannot be encoded: {error}"),
                        )
                    })?;
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": arguments},
                    }));
                }
                ProviderContent::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    // A `tool` message has no error flag, so a failed tool
                    // keeps its failure in the text rather than being
                    // replayed as an ordinary success.
                    let body = if *is_error {
                        format!("tool error: {content}")
                    } else {
                        content.clone()
                    };
                    tool_messages.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_use_id,
                        "content": body,
                    }));
                }
                ProviderContent::Thinking { thinking, .. } => {
                    // Only reachable for a profile that declares replayable
                    // reasoning; the text is sent as ordinary assistant text
                    // because there is no separate field for it.
                    if !text.is_empty() {
                        text.push_str("\n\n");
                    }
                    text.push_str(thinking);
                }
                ProviderContent::RedactedThinking { .. } => {
                    return Err(config_error(
                        "redacted-thinking blocks have no chat-completions representation".into(),
                    ));
                }
            }
        }
        if !text.is_empty() || refusal.is_some() || !tool_calls.is_empty() {
            let mut item = Map::new();
            item.insert(
                "role".into(),
                Value::String(if assistant { "assistant" } else { "user" }.into()),
            );
            if let Some(refusal) = refusal {
                item.insert("refusal".into(), Value::String(refusal));
                item.insert("content".into(), Value::Null);
            } else {
                item.insert("content".into(), Value::String(text));
            }
            if !tool_calls.is_empty() {
                item.insert("tool_calls".into(), Value::Array(tool_calls));
            }
            items.push(Value::Object(item));
        }
        items.extend(tool_messages);
    }
    Ok(Value::Array(items))
}

// -- stream accumulation -------------------------------------------------

#[derive(Debug, Default)]
struct ToolCallState {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct Accumulator {
    id: Option<String>,
    model: Option<String>,
    text: String,
    reasoning: String,
    tool_calls: BTreeMap<usize, ToolCallState>,
    finish_reason: Option<String>,
    usage: Option<ProviderUsage>,
    started: bool,
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
    let mut done = false;
    loop {
        let read = read_sse_line(&mut reader, &mut line, PROVIDER, cancellation, target)?;
        if read == 0 {
            if !data.is_empty() && !done {
                process_chunk(&data, &mut accumulator, sink, target)?;
            }
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if !data.is_empty() {
                if data.trim() == "[DONE]" {
                    done = true;
                } else {
                    process_chunk(&data, &mut accumulator, sink, target)?;
                }
                data.clear();
            }
            line.clear();
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim_start());
        }
        line.clear();
    }
    finish_response(accumulator, request_id, sink)
}

fn process_chunk(
    data: &str,
    accumulator: &mut Accumulator,
    sink: &mut dyn EventSink,
    target: &ProviderTarget,
) -> Result<(), ProviderFailure> {
    let value: Value = serde_json::from_str(data)
        .map_err(|error| invalid_stream(format!("invalid chat-completions SSE JSON: {error}")))?;
    sink.push(ProviderStreamEvent::ProtocolActivity);
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        return Err(classify_error_payload(error, target));
    }
    if let Some(id) = value.get("id").and_then(Value::as_str)
        && accumulator.id.is_none()
    {
        accumulator.id = Some(id.to_string());
    }
    if let Some(model) = value.get("model").and_then(Value::as_str)
        && accumulator.model.is_none()
    {
        accumulator.model = Some(model.to_string());
    }
    if !accumulator.started
        && let (Some(id), Some(model)) = (&accumulator.id, &accumulator.model)
    {
        accumulator.started = true;
        sink.push(ProviderStreamEvent::MessageStarted {
            id: id.clone(),
            model: model.clone(),
        });
    }
    if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
        accumulator.usage = Some(capture_usage(usage));
    }
    let Some(choices) = value.get("choices").and_then(Value::as_array) else {
        return Ok(());
    };
    for choice in choices {
        let index = choice.get("index").and_then(Value::as_u64).unwrap_or(0);
        if index != 0 {
            return Err(invalid_stream(
                "zirv requests one completion; a second choice index is not part of the contract"
                    .into(),
            ));
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            accumulator.finish_reason = Some(reason.to_string());
        }
        let Some(delta) = choice.get("delta") else {
            continue;
        };
        if let Some(text) = delta.get("content").and_then(Value::as_str)
            && !text.is_empty()
        {
            check_cap(accumulator.text.len(), text.len())?;
            accumulator.text.push_str(text);
            sink.push(ProviderStreamEvent::TextDelta {
                index: 0,
                text: text.to_string(),
            });
        }
        if let Some(text) = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str)
            && !text.is_empty()
        {
            check_cap(accumulator.reasoning.len(), text.len())?;
            accumulator.reasoning.push_str(text);
            sink.push(ProviderStreamEvent::ThinkingDelta {
                index: 0,
                text: text.to_string(),
            });
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                accumulate_tool_call(call, accumulator, sink)?;
            }
        }
    }
    Ok(())
}

fn accumulate_tool_call(
    call: &Value,
    accumulator: &mut Accumulator,
    sink: &mut dyn EventSink,
) -> Result<(), ProviderFailure> {
    let index = call
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| invalid_stream("tool_call delta has no valid `index`".into()))?;
    let state = accumulator.tool_calls.entry(index).or_default();
    if let Some(id) = call.get("id").and_then(Value::as_str)
        && !id.is_empty()
    {
        if !state.id.is_empty() && state.id != id {
            return Err(invalid_stream(
                "a streamed tool call changed its id mid-stream".into(),
            ));
        }
        state.id = id.to_string();
    }
    let function = call.get("function").unwrap_or(&Value::Null);
    if let Some(name) = function.get("name").and_then(Value::as_str)
        && !name.is_empty()
    {
        if !state.name.is_empty() && state.name != name {
            return Err(invalid_stream(
                "a streamed tool call changed its name mid-stream".into(),
            ));
        }
        state.name = name.to_string();
    }
    if let Some(fragment) = function.get("arguments").and_then(Value::as_str)
        && !fragment.is_empty()
    {
        check_cap(state.arguments.len(), fragment.len())?;
        state.arguments.push_str(fragment);
        sink.push(ProviderStreamEvent::ToolInputDelta {
            index,
            partial_json: fragment.to_string(),
        });
    }
    Ok(())
}

fn finish_response(
    accumulator: Accumulator,
    request_id: Option<String>,
    sink: &mut dyn EventSink,
) -> Result<ProviderResponse, ProviderFailure> {
    // A stream that stopped without a finish reason is a truncated stream,
    // never a completed turn.
    let reason = accumulator.finish_reason.ok_or_else(|| {
        invalid_stream("the chat-completions stream ended before a finish reason".into())
    })?;
    let message_id = accumulator
        .id
        .ok_or_else(|| invalid_stream("the chat-completions stream had no completion id".into()))?;
    let model = accumulator.model.ok_or_else(|| {
        invalid_stream("the chat-completions stream had no serving model".into())
    })?;

    let mut content = Vec::new();
    if !accumulator.text.is_empty() {
        content.push(ProviderContent::Text {
            text: accumulator.text,
        });
    }
    let truncated = reason == "length";
    let mut omitted: Vec<String> = Vec::new();
    for (index, state) in accumulator.tool_calls {
        if state.id.is_empty() || state.name.is_empty() {
            return Err(invalid_stream(format!(
                "streamed tool call {index} never named its id and function"
            )));
        }
        if truncated {
            // A truncated turn never hands the runtime an executable call.
            omitted.push(state.id);
            continue;
        }
        let input: Value = serde_json::from_str(&state.arguments).map_err(|error| {
            ProviderFailure::new(
                FailureClass::InvalidToolArguments,
                FailureScope::request(),
                format!(
                    "tool call `{}` returned incomplete JSON arguments: {error}",
                    state.name
                ),
            )
        })?;
        if !input.is_object() {
            return Err(ProviderFailure::new(
                FailureClass::InvalidToolArguments,
                FailureScope::request(),
                format!("tool call `{}` arguments are not a JSON object", state.name),
            ));
        }
        content.push(ProviderContent::ToolUse {
            id: state.id,
            name: state.name,
            input,
        });
        sink.push(ProviderStreamEvent::BlockCompleted { index });
    }

    let mut stop_details = None;
    if !omitted.is_empty() || !accumulator.reasoning.is_empty() {
        stop_details = Some(OpaqueProviderData::new(json!({
            "omitted_tool_calls": omitted,
            // Display-only: reasoning text without a signature is never
            // replayed into the next request, so it is kept out of `content`.
            "reasoning_text_len": accumulator.reasoning.len(),
        })));
    }
    let finish_reason = match reason.as_str() {
        "stop" => FinishReason::EndTurn,
        "length" => FinishReason::MaxTokens,
        "tool_calls" | "function_call" => FinishReason::ToolUse,
        "content_filter" => FinishReason::Refusal,
        other => FinishReason::Unknown(other.to_string()),
    };
    Ok(ProviderResponse {
        message_id,
        model,
        content,
        finish_reason,
        stop_sequence: None,
        stop_details,
        usage: accumulator.usage.unwrap_or_default(),
        request_id,
    })
}

fn capture_usage(value: &Value) -> ProviderUsage {
    ProviderUsage {
        input_tokens: value
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: value
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        output_tokens: value
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        reasoning_tokens: value
            .pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64),
    }
}

// -- failure normalization -----------------------------------------------

fn invalid_stream(message: String) -> ProviderFailure {
    super::transport::invalid_stream(message)
}

fn check_cap(current: usize, delta: usize) -> Result<(), ProviderFailure> {
    super::transport::check_block_accumulator_cap(PROVIDER, current, delta)
}

fn classify_transport_error(
    error: ureq::Error,
    saw_event: bool,
    target: &ProviderTarget,
) -> ProviderFailure {
    if matches!(error, ureq::Error::Timeout(_)) {
        return super::transport::timeout_failure(PROVIDER, saw_event, target);
    }
    super::transport::transport_failure(format!("chat-completions transport failed: {error}"), target)
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
        .unwrap_or("the endpoint rejected the request")
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
        429 if code == "insufficient_quota" || lower.contains("quota") => {
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
    failure.provider_request_id = header_request_id;
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
        .unwrap_or("the chat-completions stream failed")
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
    failure.retry.retryable = retryable;
    failure
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::commands::ctx::provider::adapter::{NeverCancelled, ProviderMessage};
    use crate::commands::ctx::provider::credential::Secret;
    use crate::commands::ctx::provider::profiles::profile;
    use crate::commands::ctx::provider::testhttp::one_shot_server;
    use crate::commands::ctx::provider::{
        AccountId, BillingPoolId, EndpointId, ModelId, ProviderId,
    };
    use crate::commands::ctx::runtime::tools::ToolRegistry;

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/provider/openai-chat/v1/",
                $name
            ))
        };
    }

    const MULTI_TOOL: &str = fixture!("stream-multi-tool-reasoning.sse");
    const FINAL_TEXT: &str = fixture!("stream-final-text.sse");
    const MALFORMED_TOOL: &str = fixture!("stream-malformed-tool.sse");
    const TRUNCATED: &str = fixture!("stream-truncated.sse");
    const LENGTH: &str = fixture!("stream-length.sse");
    const STREAM_ERROR: &str = fixture!("stream-error.sse");

    fn target(base_url: String, vendor: &str, model: &str) -> ProviderTarget {
        ProviderTarget {
            route: RouteId::new("work").unwrap(),
            provider: ProviderId::new("openai-compatible").unwrap(),
            endpoint: EndpointId::new("vendor").unwrap(),
            account: AccountId::new("work").unwrap(),
            billing_pool: BillingPoolId::new("work").unwrap(),
            protocol: Protocol::OpenAiChatCompatible,
            base_url,
            model: ModelId {
                vendor: vendor.into(),
                id: model.into(),
            },
        }
    }

    fn credential() -> Credential {
        Credential {
            secret: Secret::new(concat!("sk-", "test-secret-never-log").into()),
            expires_at: None,
        }
    }

    fn deepseek(base_url: String) -> OpenAiChatAdapter {
        OpenAiChatAdapter::new(
            target(base_url, "deepseek", "deepseek-v4-pro"),
            Some(credential()),
            StreamTimeouts::default(),
            profile("deepseek-chat").unwrap(),
            ChatEndpoint::Compatible,
            BTreeMap::new(),
        )
        .unwrap()
    }

    fn ollama(base_url: String) -> OpenAiChatAdapter {
        OpenAiChatAdapter::new(
            target(base_url, "ollama", "qwen3-coder"),
            None,
            StreamTimeouts::default(),
            profile("ollama-openai").unwrap(),
            ChatEndpoint::Compatible,
            BTreeMap::new(),
        )
        .unwrap()
    }

    fn request(model: &str) -> ProviderRequest {
        ProviderRequest {
            model: model.into(),
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
            thinking: ThinkingConfig::Default,
            effort: None,
            cache: CacheMode::Disabled,
        }
    }

    fn parse(
        stream: &str,
        events: &mut Vec<ProviderStreamEvent>,
    ) -> Result<ProviderResponse, ProviderFailure> {
        parse_sse(
            BufReader::new(stream.as_bytes()),
            Some("req_header".into()),
            &NeverCancelled,
            events,
            &target(
                "https://api.deepseek.com".into(),
                "deepseek",
                "deepseek-v4-pro",
            ),
        )
    }

    #[test]
    fn the_stream_reassembles_text_and_every_streamed_tool_call() {
        let mut events = Vec::new();
        let response = parse(MULTI_TOOL, &mut events).unwrap();
        assert_eq!(response.message_id, "chatcmpl_fixture");
        assert_eq!(response.model, "deepseek-v4-pro");
        assert_eq!(response.finish_reason, FinishReason::ToolUse);
        assert_eq!(response.request_id.as_deref(), Some("req_header"));
        assert!(matches!(
            &response.content[0],
            ProviderContent::Text { text } if text == "Reading both files."
        ));
        let calls: Vec<(&str, &str, &Value)> = response
            .content
            .iter()
            .filter_map(|block| match block {
                ProviderContent::ToolUse { id, name, input } => {
                    Some((id.as_str(), name.as_str(), input))
                }
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "call_1");
        assert_eq!(calls[0].2["path"], json!("src/lib.rs"));
        assert_eq!(calls[1].1, "text_search");
        assert_eq!(response.usage.input_tokens, 120);
        assert_eq!(response.usage.output_tokens, 42);
        assert_eq!(response.usage.cache_read_input_tokens, 64);
        assert_eq!(response.usage.reasoning_tokens, Some(11));
        assert!(events.iter().any(|event| matches!(
            event,
            ProviderStreamEvent::ToolInputDelta { partial_json, .. }
                if partial_json.contains("path")
        )));
    }

    #[test]
    fn reasoning_text_is_shown_while_streaming_but_is_never_replayable_content() {
        let mut events = Vec::new();
        let response = parse(MULTI_TOOL, &mut events).unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            ProviderStreamEvent::ThinkingDelta { text, .. } if text == "Both files first."
        )));
        assert!(
            !response
                .content
                .iter()
                .any(|block| matches!(block, ProviderContent::Thinking { .. })),
            "chat-completions reasoning carries no signature and must not be replayable"
        );
        let details = response.stop_details.expect("reasoning is recorded");
        assert_eq!(details.expose()["reasoning_text_len"], json!(17));
    }

    #[test]
    fn malformed_streamed_tool_arguments_never_become_a_tool_call() {
        let error = parse(MALFORMED_TOOL, &mut Vec::new()).unwrap_err();
        assert_eq!(error.class, FailureClass::InvalidToolArguments);
        assert!(error.message.contains("incomplete JSON"));
    }

    #[test]
    fn a_truncated_stream_is_never_a_completion() {
        let error = parse(TRUNCATED, &mut Vec::new()).unwrap_err();
        assert_eq!(error.class, FailureClass::InvalidStream);
        assert!(error.message.contains("before a finish reason"));
    }

    #[test]
    fn a_length_stop_drops_every_tool_call_and_records_the_omission() {
        let response = parse(LENGTH, &mut Vec::new()).unwrap();
        assert_eq!(response.finish_reason, FinishReason::MaxTokens);
        assert!(
            !response
                .content
                .iter()
                .any(|block| matches!(block, ProviderContent::ToolUse { .. }))
        );
        let details = response.stop_details.expect("omission is recorded");
        assert_eq!(details.expose()["omitted_tool_calls"], json!(["call_cut"]));
    }

    #[test]
    fn stream_error_payloads_are_typed_with_a_retry_hint() {
        let error = parse(STREAM_ERROR, &mut Vec::new()).unwrap_err();
        assert_eq!(error.class, FailureClass::RateLimited);
        assert_eq!(error.scope.kind, FailureScopeKind::BillingPool);
        assert!(error.retry.retryable);
    }

    #[test]
    fn the_request_uses_the_vendor_path_and_a_bearer_key() {
        let (url, captured) = one_shot_server(200, MULTI_TOOL, "text/event-stream");
        let response = deepseek(url)
            .stream(
                &request("deepseek-v4-pro"),
                &NeverCancelled,
                &mut Vec::new(),
            )
            .unwrap();
        assert_eq!(response.finish_reason, FinishReason::ToolUse);
        let sent = captured.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(sent.starts_with("POST /v1/chat/completions HTTP/1.1"));
        assert!(sent.to_ascii_lowercase().contains(concat!(
            "authorization: bearer sk-",
            "test-secret-never-log"
        )));
        assert!(sent.contains("\"stream\":true"));
        assert!(sent.contains("\"include_usage\":true"));
        assert!(sent.contains("\"type\":\"function\""));
        assert!(sent.contains("\"role\":\"system\""));
        assert!(sent.contains("\"parallel_tool_calls\":true"));
    }

    #[test]
    fn a_non_primary_vendor_completes_a_multi_turn_tool_task_with_no_harness() {
        let (first_url, first_capture) = one_shot_server(200, MULTI_TOOL, "text/event-stream");
        let first = deepseek(first_url)
            .stream(
                &request("deepseek-v4-pro"),
                &NeverCancelled,
                &mut Vec::new(),
            )
            .unwrap();
        let calls: Vec<String> = first
            .content
            .iter()
            .filter_map(|block| match block {
                ProviderContent::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2);

        // Zirv executes the tools; the endpoint never does.
        let mut second = request("deepseek-v4-pro");
        second.messages.push(ProviderMessage {
            role: ProviderMessageRole::Assistant,
            content: first.content.clone(),
        });
        second.messages.push(ProviderMessage {
            role: ProviderMessageRole::User,
            content: calls
                .iter()
                .map(|id| ProviderContent::ToolResult {
                    tool_use_id: id.clone(),
                    content: "file body".into(),
                    is_error: false,
                })
                .collect(),
        });
        let (second_url, second_capture) = one_shot_server(200, FINAL_TEXT, "text/event-stream");
        let final_turn = deepseek(second_url)
            .stream(&second, &NeverCancelled, &mut Vec::new())
            .unwrap();
        assert_eq!(final_turn.finish_reason, FinishReason::EndTurn);
        assert!(matches!(
            &final_turn.content[0],
            ProviderContent::Text { text } if text == "Both files read."
        ));
        assert!(
            !first_capture
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .contains("\"role\":\"tool\"")
        );
        let replay = second_capture.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(replay.contains("\"role\":\"tool\""));
        assert!(replay.contains("\"tool_call_id\":\"call_2\""));
        assert!(replay.contains("\"tool_calls\""));
    }

    #[test]
    fn a_local_endpoint_completes_a_turn_with_no_key_and_no_authorization_header() {
        let (url, captured) = one_shot_server(200, MULTI_TOOL, "text/event-stream");
        let adapter = ollama(url);
        assert!(adapter.credential.is_none());
        let response = adapter
            .stream(&request("qwen3-coder"), &NeverCancelled, &mut Vec::new())
            .unwrap();
        assert_eq!(response.finish_reason, FinishReason::ToolUse);
        let sent = captured.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!sent.to_ascii_lowercase().contains("authorization:"));
        assert!(!sent.contains("parallel_tool_calls"));
    }

    #[test]
    fn an_azure_route_is_addressed_by_deployment_and_carries_its_own_key_header() {
        let (url, captured) = one_shot_server(200, FINAL_TEXT, "text/event-stream");
        let mut azure_target = target(url, "openai", "gpt-5.6-sol");
        azure_target.protocol = Protocol::AzureOpenAiChat;
        azure_target.provider = ProviderId::new("azure-openai").unwrap();
        let adapter = OpenAiChatAdapter::new(
            azure_target,
            Some(credential()),
            StreamTimeouts::default(),
            profile("azure-openai-chat").unwrap(),
            ChatEndpoint::Azure {
                deployment: "sol-prod".into(),
                api_version: "2026-05-01".into(),
            },
            BTreeMap::new(),
        )
        .unwrap();
        adapter
            .stream(&request("gpt-5.6-sol"), &NeverCancelled, &mut Vec::new())
            .unwrap();
        let sent = captured.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            sent.starts_with(
                "POST /openai/deployments/sol-prod/chat/completions?api-version=2026-05-01 HTTP/1.1"
            ),
            "got {}",
            sent.lines().next().unwrap_or_default()
        );
        assert!(
            sent.to_ascii_lowercase()
                .contains(concat!("api-key: sk-", "test-secret-never-log"))
        );
        assert!(!sent.to_ascii_lowercase().contains("authorization:"));
        // The deployment names the model; sending a model id would name
        // something the deployment may not serve.
        assert!(!sent.contains("\"model\":"));
    }

    #[test]
    fn an_azure_target_and_a_compatible_endpoint_cannot_be_mixed() {
        let mut azure_target = target(
            "https://contoso.openai.azure.com".into(),
            "openai",
            "gpt-5.6-sol",
        );
        azure_target.protocol = Protocol::AzureOpenAiChat;
        let error = OpenAiChatAdapter::new(
            azure_target,
            Some(credential()),
            StreamTimeouts::default(),
            profile("azure-openai-chat").unwrap(),
            ChatEndpoint::Compatible,
            BTreeMap::new(),
        )
        .unwrap_err();
        assert_eq!(error.class, FailureClass::Configuration);
        assert!(error.message.contains("Azure endpoint"));
    }

    #[test]
    fn incompatible_task_requirements_are_refused_before_any_socket_opens() {
        let adapter = deepseek("https://api.deepseek.com".into());
        let base = request("deepseek-v4-pro");

        let mut effort = base.clone();
        effort.effort = Some(Effort::High);
        assert!(
            adapter
                .encode_request(&effort)
                .unwrap_err()
                .message
                .contains("no reasoning-effort control")
        );

        let mut cache = base.clone();
        cache.cache = CacheMode::Ephemeral5m;
        assert!(
            adapter
                .encode_request(&cache)
                .unwrap_err()
                .message
                .contains("no cache_control")
        );

        let mut replay = base.clone();
        replay.messages.push(ProviderMessage {
            role: ProviderMessageRole::Assistant,
            content: vec![ProviderContent::Thinking {
                thinking: "kept".into(),
                signature: OpaqueProviderData::new(json!({"type":"reasoning","id":"rs_1"})),
            }],
        });
        assert!(
            adapter
                .encode_request(&replay)
                .unwrap_err()
                .message
                .contains("no replayable reasoning")
        );

        let mut wrong_model = base.clone();
        wrong_model.model = "deepseek-v4-flash".into();
        assert!(
            adapter
                .encode_request(&wrong_model)
                .unwrap_err()
                .message
                .contains("does not exactly match route model")
        );

        let mut dangling = base;
        dangling.messages.push(ProviderMessage {
            role: ProviderMessageRole::Assistant,
            content: vec![ProviderContent::ToolUse {
                id: "call_1".into(),
                name: "file_read".into(),
                input: json!({}),
            }],
        });
        assert!(
            adapter
                .encode_request(&dangling)
                .unwrap_err()
                .message
                .contains("matching tool messages")
        );
    }

    #[test]
    fn a_broker_profile_never_constructs_an_adapter() {
        let error = OpenAiChatAdapter::new(
            target(
                "https://api.example.invalid".into(),
                "copilot",
                "gpt-5.6-sol",
            ),
            Some(credential()),
            StreamTimeouts::default(),
            profile("copilot-broker").unwrap(),
            ChatEndpoint::Compatible,
            BTreeMap::new(),
        )
        .unwrap_err();
        assert_eq!(error.class, FailureClass::Entitlement);
        assert!(error.message.contains("no direct API"));
    }

    #[test]
    fn plaintext_and_credential_class_rules_hold_at_construction() {
        let remote_plaintext = OpenAiChatAdapter::new(
            target(
                "http://api.deepseek.com".into(),
                "deepseek",
                "deepseek-v4-pro",
            ),
            Some(credential()),
            StreamTimeouts::default(),
            profile("deepseek-chat").unwrap(),
            ChatEndpoint::Compatible,
            BTreeMap::new(),
        )
        .unwrap_err();
        assert!(remote_plaintext.message.contains("plaintext HTTP"));
        assert_eq!(remote_plaintext.scope.kind, FailureScopeKind::Endpoint);

        let public_local = OpenAiChatAdapter::new(
            target("http://models.example.com:11434".into(), "ollama", "qwen3"),
            None,
            StreamTimeouts::default(),
            profile("ollama-openai").unwrap(),
            ChatEndpoint::Compatible,
            BTreeMap::new(),
        )
        .unwrap_err();
        assert!(public_local.message.contains("loopback or private"));

        let missing_key = OpenAiChatAdapter::new(
            target(
                "https://api.deepseek.com".into(),
                "deepseek",
                "deepseek-v4-pro",
            ),
            None,
            StreamTimeouts::default(),
            profile("deepseek-chat").unwrap(),
            ChatEndpoint::Compatible,
            BTreeMap::new(),
        )
        .unwrap_err();
        assert_eq!(missing_key.class, FailureClass::Authentication);

        let harness_login = OpenAiChatAdapter::new(
            target(
                "https://api.deepseek.com".into(),
                "deepseek",
                "deepseek-v4-pro",
            ),
            Some(Credential {
                secret: Secret::new("{\"tokens\":{\"access_token\":\"x\"}}".into()),
                expires_at: None,
            }),
            StreamTimeouts::default(),
            profile("deepseek-chat").unwrap(),
            ChatEndpoint::Compatible,
            BTreeMap::new(),
        )
        .unwrap_err();
        assert_eq!(harness_login.class, FailureClass::Entitlement);
    }

    #[test]
    fn declared_extensions_reach_the_wire_and_never_overwrite_protocol_fields() {
        let (url, captured) = one_shot_server(200, FINAL_TEXT, "text/event-stream");
        let adapter = OpenAiChatAdapter::new(
            target(url, "deepseek", "deepseek-v4-pro"),
            Some(credential()),
            StreamTimeouts::default(),
            profile("deepseek-chat").unwrap(),
            ChatEndpoint::Compatible,
            BTreeMap::from([("temperature".to_string(), json!(0.2))]),
        )
        .unwrap();
        adapter
            .stream(
                &request("deepseek-v4-pro"),
                &NeverCancelled,
                &mut Vec::new(),
            )
            .unwrap();
        assert!(
            captured
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .contains("\"temperature\":0.2")
        );

        let colliding = OpenAiChatAdapter::new(
            target(
                "https://api.deepseek.com".into(),
                "deepseek",
                "deepseek-v4-pro",
            ),
            Some(credential()),
            StreamTimeouts::default(),
            profile("deepseek-chat").unwrap(),
            ChatEndpoint::Compatible,
            BTreeMap::from([("messages".to_string(), json!([]))]),
        )
        .unwrap();
        assert!(
            colliding
                .encode_request(&request("deepseek-v4-pro"))
                .unwrap_err()
                .message
                .contains("protocol-owned request field")
        );
    }

    #[test]
    fn http_status_failures_are_typed_with_scope_and_retry_hint() {
        for (status, body, class, scope, retryable) in [
            (
                401,
                r#"{"error":{"message":"bad key","code":"invalid_api_key"}}"#,
                FailureClass::Authentication,
                FailureScopeKind::Account,
                false,
            ),
            (
                429,
                r#"{"error":{"message":"slow down","code":"rate_limit_exceeded"}}"#,
                FailureClass::RateLimited,
                FailureScopeKind::BillingPool,
                true,
            ),
            (
                404,
                r#"{"error":{"message":"no such model","code":"model_not_found"}}"#,
                FailureClass::ModelAccess,
                FailureScopeKind::Model,
                false,
            ),
        ] {
            let (url, _captured) = one_shot_server(status, body, "application/json");
            let error = deepseek(url)
                .stream(
                    &request("deepseek-v4-pro"),
                    &NeverCancelled,
                    &mut Vec::new(),
                )
                .unwrap_err();
            assert_eq!(error.class, class, "status {status}");
            assert_eq!(error.scope.kind, scope, "status {status}");
            assert_eq!(error.retry.retryable, retryable, "status {status}");
            assert_eq!(error.http_status, Some(status));
        }
    }
}
