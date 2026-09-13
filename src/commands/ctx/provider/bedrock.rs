//! Direct Amazon Bedrock transport (issue #482, roadmap N13).
//!
//! Bedrock differs from every other route in two ways that make "change the
//! base URL" impossible: it authenticates with a SigV4 signature over the
//! canonical request (`aws_sigv4`), and it answers with AWS **event-stream**
//! binary frames rather than SSE.
//!
//! One body shape is used for every vendor Bedrock hosts: `ConverseStream`.
//! Converse is Bedrock's own vendor-neutral surface -- the same request and
//! the same event set serve Amazon Nova, the Bedrock-hosted Anthropic,
//! Meta, Mistral and Cohere families -- so a second, model-specific
//! `invoke-with-response-stream` parser would be a duplicate of this one
//! with a different JSON dialect. What does differ per family is *what the
//! route may ask for*, and that is exactly what the bound profile's caveats
//! carry: reasoning replay is declared for the Anthropic-on-Bedrock profile
//! (Converse returns a reasoning signature there) and not for the generic
//! one.
//!
//! Frame CRCs are not verified. The stream runs over TLS, each frame's
//! length is checked against its prelude, and any corruption that survived
//! both surfaces immediately as a JSON decode failure -- a typed
//! `InvalidStream`, never a silently accepted turn.

#![allow(dead_code)] // Route selection reaches this adapter through native.rs.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufReader, Read};

use serde_json::{Map, Value, json};

use super::adapter::{
    CacheMode, Cancellation, EventSink, FailureClass, FailureScope, FailureScopeKind, FinishReason,
    ProviderAdapter, ProviderContent, ProviderFailure, ProviderMessageRole, ProviderRequest,
    ProviderResponse, ProviderStreamEvent, ProviderTarget, ProviderUsage, RetryHint,
    ThinkingConfig, resolve_target,
};
use super::aws_sigv4::{AwsCredentials, CanonicalRequest, amz_date, sign, uri_encode};
use super::config::NativeConfig;
use super::credential::{Credential, CredentialStore};
use super::profiles::{RouteProfile, profile_for};
use super::transport::{
    MAX_ERROR_BODY_BYTES, StreamTimeouts, WORKER_READ_POLL, parse_retry_after_ms, supervise,
    target_scope,
};
use super::{OpaqueProviderData, Protocol, RouteId, Support};
use crate::commands::ctx::config::EnvLookup;

const PROVIDER: &str = "Bedrock";
const SERVICE: &str = "bedrock";
/// One event-stream frame, prelude and trailing CRC included.
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// The opaque envelope a replayable Bedrock reasoning block is stored in.
const REASONING_ENVELOPE: &str = "bedrock_reasoning";

#[derive(Clone)]
pub struct BedrockAdapter {
    target: ProviderTarget,
    credentials: AwsCredentials,
    region: String,
    timeouts: StreamTimeouts,
    profile: &'static RouteProfile,
    /// Injected so signing stays reproducible in tests; production passes
    /// the state clock.
    now: u64,
}

impl std::fmt::Debug for BedrockAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BedrockAdapter")
            .field("target", &self.target)
            .field("profile", &self.profile.id)
            .field("region", &self.region)
            .field("credentials", &"[redacted]")
            .field("timeouts", &self.timeouts)
            .finish()
    }
}

impl BedrockAdapter {
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
        let account = config
            .accounts
            .get(&target.account)
            .ok_or_else(|| config_error(format!("unknown account `{}`", target.account)))?;
        let region = account.region.clone().ok_or_else(|| {
            config_error(format!(
                "account `{}` has no AWS region; a SigV4 signature is region-scoped",
                target.account
            ))
        })?;
        let credential = credential.ok_or_else(|| {
            ProviderFailure::new(
                FailureClass::Authentication,
                FailureScope {
                    kind: FailureScopeKind::Account,
                    id: Some(target.account.to_string()),
                },
                format!("account `{}` has no AWS credential", target.account),
            )
        })?;
        Self::new(target, &credential, region, timeouts, profile, now)
    }

    pub fn new(
        target: ProviderTarget,
        credential: &Credential,
        region: String,
        timeouts: StreamTimeouts,
        profile: &'static RouteProfile,
        now: u64,
    ) -> Result<Self, ProviderFailure> {
        if target.protocol != Protocol::AwsBedrock {
            return Err(config_error(format!(
                "the Bedrock adapter requires an aws-bedrock target, not {:?}",
                target.protocol
            )));
        }
        if profile.protocol != Protocol::AwsBedrock {
            return Err(config_error(format!(
                "route profile `{}` is not a Bedrock profile",
                profile.id
            )));
        }
        if let Support::LegacyOnly(reason) = profile.support {
            return Err(ProviderFailure::new(
                FailureClass::Entitlement,
                FailureScope {
                    kind: FailureScopeKind::Provider,
                    id: Some(target.provider.to_string()),
                },
                format!("route profile `{}` has no direct API: {reason}", profile.id),
            ));
        }
        if timeouts.has_zero() {
            return Err(config_error(
                "Bedrock timeouts must be greater than zero".into(),
            ));
        }
        if region.trim().is_empty() {
            return Err(config_error("Bedrock requires an AWS region".into()));
        }
        if target.base_url.starts_with("http://")
            && !super::probe::is_local_http_host(&target.base_url)
        {
            return Err(ProviderFailure::new(
                FailureClass::Configuration,
                FailureScope {
                    kind: FailureScopeKind::Endpoint,
                    id: Some(target.endpoint.to_string()),
                },
                "a SigV4-signed request is never sent to a public host over plaintext HTTP",
            ));
        }
        let credentials = AwsCredentials::parse(credential.secret.expose()).map_err(|error| {
            ProviderFailure::new(
                FailureClass::Authentication,
                FailureScope {
                    kind: FailureScopeKind::Account,
                    id: Some(target.account.to_string()),
                },
                format!("account `{}`: {error}", target.account),
            )
        })?;
        Ok(Self {
            target,
            credentials,
            region,
            timeouts,
            profile,
            now,
        })
    }

    fn path(&self) -> String {
        format!(
            "/model/{}/converse-stream",
            uri_encode(&self.target.model.id, true)
        )
    }

    fn host(&self) -> Result<String, ProviderFailure> {
        let rest = self
            .target
            .base_url
            .strip_prefix("https://")
            .or_else(|| self.target.base_url.strip_prefix("http://"))
            .ok_or_else(|| config_error("a Bedrock endpoint must be an http(s) URL".into()))?;
        let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
        if host.is_empty() {
            return Err(config_error("a Bedrock endpoint must name a host".into()));
        }
        Ok(host.to_string())
    }

    fn encode_request(&self, request: &ProviderRequest) -> Result<Value, ProviderFailure> {
        validate_request(request, &self.target, self.profile)?;
        let mut body = Map::new();
        body.insert("messages".into(), encode_messages(request)?);
        if !request.system.is_empty() {
            body.insert(
                "system".into(),
                Value::Array(
                    request
                        .system
                        .iter()
                        .map(|text| json!({"text": text}))
                        .collect(),
                ),
            );
        }
        let mut inference = Map::new();
        inference.insert("maxTokens".into(), json!(request.max_output_tokens));
        if !request.stop_sequences.is_empty() {
            inference.insert("stopSequences".into(), json!(request.stop_sequences));
        }
        body.insert("inferenceConfig".into(), Value::Object(inference));
        if !request.tools.is_empty() {
            body.insert(
                "toolConfig".into(),
                json!({
                    "tools": request
                        .tools
                        .iter()
                        .map(|tool| json!({
                            "toolSpec": {
                                "name": tool.name,
                                "description": tool.description,
                                "inputSchema": {"json": tool.input_schema},
                            }
                        }))
                        .collect::<Vec<_>>(),
                }),
            );
        }
        // Reasoning is a model-family field, which Converse carries in its
        // passthrough object rather than in its own schema.
        if let ThinkingConfig::Enabled { budget_tokens, .. } = request.thinking {
            body.insert(
                "additionalModelRequestFields".into(),
                json!({"thinking": {"type": "enabled", "budget_tokens": budget_tokens}}),
            );
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
        let payload = serde_json::to_string(body)
            .map_err(|error| config_error(format!("failed to encode Bedrock request: {error}")))?;
        let host = self.host()?;
        let path = self.path();
        let signed = sign(
            &CanonicalRequest {
                method: "POST",
                path: &path,
                query: "",
                host: &host,
                extra_headers: &[("content-type", "application/json".to_string())],
                payload: payload.as_bytes(),
            },
            &self.region,
            SERVICE,
            &amz_date(self.now),
            &self.credentials,
        )
        .map_err(|error| config_error(format!("Bedrock request could not be signed: {error}")))?;

        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_connect(Some(self.timeouts.connect))
            .timeout_recv_response(Some(self.timeouts.first_event))
            .timeout_recv_body(Some(WORKER_READ_POLL))
            .build()
            .into();
        let mut http = agent
            .post(format!(
                "{}{path}",
                self.target.base_url.trim_end_matches('/')
            ))
            .header("content-type", "application/json")
            .header("accept", "application/vnd.amazon.eventstream")
            .header("user-agent", format!("zirv/{}", env!("CARGO_PKG_VERSION")));
        for (name, value) in &signed.headers {
            // `host` is set by the HTTP client itself from the URL.
            if name != "host" {
                http = http.header(name.as_str(), value.as_str());
            }
        }
        let mut response = http
            .send(payload)
            .map_err(|error| classify_transport_error(error, false, &self.target))?;
        let status = response.status().as_u16();
        let request_id = response
            .headers()
            .get("x-amzn-requestid")
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
                .unwrap_or_else(|_| "Bedrock returned an unreadable error body".into());
            return Err(classify_http_error(
                status,
                &body,
                request_id,
                retry_after,
                &self.target,
            ));
        }
        let reader = BufReader::new(response.into_body().into_reader());
        parse_event_stream(reader, request_id, cancellation, sink, &self.target)
    }
}

impl ProviderAdapter for BedrockAdapter {
    fn protocol(&self) -> Protocol {
        Protocol::AwsBedrock
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
            "zirv-bedrock-stream",
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

// -- validation ----------------------------------------------------------

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
    if request.messages.is_empty() {
        return Err(config_error(
            "Converse requests require at least one message".into(),
        ));
    }
    if request.cache != CacheMode::Disabled {
        return Err(config_error(
            "Bedrock prompt caching is a per-block cachePoint, which this transport does not \
             emit; leave the cache mode disabled"
                .into(),
        ));
    }
    if !request.tools.is_empty() && !profile.caveats.tools {
        return Err(config_error(format!(
            "route profile `{}` does not implement tool use",
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
    if request.effort.is_some() {
        return Err(config_error(
            "Converse has no reasoning-effort control; a thinking budget is the Bedrock spelling"
                .into(),
        ));
    }
    match request.thinking {
        ThinkingConfig::Default | ThinkingConfig::Disabled => {}
        ThinkingConfig::Enabled { .. } if profile.caveats.reasoning_controls => {}
        ThinkingConfig::Enabled { .. } => {
            return Err(config_error(format!(
                "route profile `{}` declares no reasoning controls",
                profile.id
            )));
        }
        ThinkingConfig::Adaptive { .. } => {
            return Err(config_error(
                "adaptive thinking is an Anthropic-API control with no Converse representation"
                    .into(),
            ));
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
                "assistant toolUse blocks must be answered by their toolResult blocks".into(),
            ));
        }
        for block in &message.content {
            match block {
                ProviderContent::Text { .. } => {}
                ProviderContent::Refusal { .. } => {
                    return Err(config_error(
                        "Converse has no refusal content block; a filtered turn arrives as a \
                         stopReason instead"
                            .into(),
                    ));
                }
                ProviderContent::ToolUse { id, input, .. } => {
                    if message.role != ProviderMessageRole::Assistant
                        || id.is_empty()
                        || !input.is_object()
                    {
                        return Err(ProviderFailure::new(
                            FailureClass::InvalidToolArguments,
                            FailureScope::request(),
                            "assistant toolUse input must be one complete JSON object",
                        ));
                    }
                    if !pending.insert(id.clone()) {
                        return Err(config_error(format!(
                            "duplicate unresolved toolUseId `{id}`"
                        )));
                    }
                }
                ProviderContent::ToolResult { tool_use_id, .. } => {
                    if !pending.remove(tool_use_id) {
                        return Err(config_error(format!(
                            "toolResult references unknown toolUseId `{tool_use_id}`"
                        )));
                    }
                }
                ProviderContent::Thinking { signature, .. } => {
                    if !profile.caveats.reasoning_replay {
                        return Err(config_error(format!(
                            "route profile `{}` carries no replayable reasoning",
                            profile.id
                        )));
                    }
                    if reasoning_signature(signature).is_none() {
                        return Err(config_error(
                            "this thinking block carries no Bedrock reasoning signature; opaque \
                             continuation state from another provider is never replayed into a \
                             Converse request"
                                .into(),
                        ));
                    }
                }
                ProviderContent::RedactedThinking { .. } => {
                    if !profile.caveats.reasoning_replay {
                        return Err(config_error(format!(
                            "route profile `{}` carries no replayable reasoning",
                            profile.id
                        )));
                    }
                }
            }
        }
    }
    if !pending.is_empty() {
        return Err(config_error(
            "assistant toolUse blocks must be followed by matching toolResult blocks".into(),
        ));
    }
    Ok(())
}

fn reasoning_signature(signature: &OpaqueProviderData) -> Option<&str> {
    let object = signature.expose().as_object()?;
    if object.get("type").and_then(Value::as_str) != Some(REASONING_ENVELOPE) {
        return None;
    }
    object
        .get("signature")
        .and_then(Value::as_str)
        .filter(|signature| !signature.is_empty())
}

// -- request encoding ----------------------------------------------------

fn encode_messages(request: &ProviderRequest) -> Result<Value, ProviderFailure> {
    let mut items: Vec<Value> = Vec::new();
    for message in &request.messages {
        let mut content: Vec<Value> = Vec::new();
        for block in &message.content {
            match block {
                ProviderContent::Text { text } => content.push(json!({"text": text})),
                ProviderContent::ToolUse { id, name, input } => content.push(json!({
                    "toolUse": {"toolUseId": id, "name": name, "input": input},
                })),
                ProviderContent::ToolResult {
                    tool_use_id,
                    content: text,
                    is_error,
                } => content.push(json!({
                    "toolResult": {
                        "toolUseId": tool_use_id,
                        "content": [{"text": text}],
                        "status": if *is_error { "error" } else { "success" },
                    },
                })),
                ProviderContent::Thinking {
                    thinking,
                    signature,
                } => {
                    let signature = reasoning_signature(signature).ok_or_else(|| {
                        config_error("reasoning block lost its Bedrock signature".into())
                    })?;
                    content.push(json!({
                        "reasoningContent": {
                            "reasoningText": {"text": thinking, "signature": signature},
                        },
                    }));
                }
                ProviderContent::RedactedThinking { data } => content.push(json!({
                    "reasoningContent": {"redactedContent": data.expose()},
                })),
                ProviderContent::Refusal { .. } => {
                    return Err(config_error("Converse has no refusal content block".into()));
                }
            }
        }
        if content.is_empty() {
            continue;
        }
        items.push(json!({
            "role": match message.role {
                ProviderMessageRole::User => "user",
                ProviderMessageRole::Assistant => "assistant",
            },
            "content": content,
        }));
    }
    Ok(Value::Array(items))
}

// -- AWS event-stream framing --------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    pub(crate) event_type: Option<String>,
    pub(crate) message_type: Option<String>,
    pub(crate) exception_type: Option<String>,
    pub(crate) payload: Vec<u8>,
}

/// Reads one frame, or `None` at a clean end of stream.
pub(crate) fn read_frame<R: Read>(
    reader: &mut R,
    cancellation: &dyn Cancellation,
    target: &ProviderTarget,
) -> Result<Option<Frame>, ProviderFailure> {
    let mut prelude = [0u8; 12];
    if !read_exact(reader, &mut prelude, cancellation, target)? {
        return Ok(None);
    }
    let total = u32::from_be_bytes([prelude[0], prelude[1], prelude[2], prelude[3]]) as usize;
    let headers_len = u32::from_be_bytes([prelude[4], prelude[5], prelude[6], prelude[7]]) as usize;
    if !(16..=MAX_FRAME_BYTES).contains(&total) || headers_len > total.saturating_sub(16) {
        return Err(invalid_stream(format!(
            "Bedrock event-stream frame declares an impossible length ({total}/{headers_len})"
        )));
    }
    let mut rest = vec![0u8; total - 12];
    if !read_exact(reader, &mut rest, cancellation, target)? {
        return Err(invalid_stream(
            "Bedrock event-stream frame ended mid-message".into(),
        ));
    }
    let headers = &rest[..headers_len];
    // The last four bytes of the message are its CRC; see the module note.
    let payload = rest[headers_len..rest.len() - 4].to_vec();
    let decoded = parse_headers(headers)?;
    Ok(Some(Frame {
        event_type: decoded.get(":event-type").cloned(),
        message_type: decoded.get(":message-type").cloned(),
        exception_type: decoded.get(":exception-type").cloned(),
        payload,
    }))
}

fn parse_headers(mut bytes: &[u8]) -> Result<BTreeMap<String, String>, ProviderFailure> {
    let mut headers = BTreeMap::new();
    while !bytes.is_empty() {
        let name_len = *bytes
            .first()
            .ok_or_else(|| invalid_stream("truncated event-stream header".into()))?
            as usize;
        if bytes.len() < 1 + name_len + 1 {
            return Err(invalid_stream("truncated event-stream header".into()));
        }
        let name = String::from_utf8_lossy(&bytes[1..1 + name_len]).into_owned();
        let value_type = bytes[1 + name_len];
        bytes = &bytes[2 + name_len..];
        // 7 is the string type, which is the only one these events use for
        // the headers zirv reads; anything else is skipped by its own width.
        let width = match value_type {
            0 | 1 => 0,
            2 => 1,
            3 => 2,
            4 | 9 => 4,
            5 | 8 => 8,
            6 | 7 => {
                if bytes.len() < 2 {
                    return Err(invalid_stream("truncated event-stream header".into()));
                }
                let length = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
                bytes = &bytes[2..];
                length
            }
            other => {
                return Err(invalid_stream(format!(
                    "unknown event-stream header value type {other}"
                )));
            }
        };
        if bytes.len() < width {
            return Err(invalid_stream("truncated event-stream header".into()));
        }
        if value_type == 7 {
            headers.insert(name, String::from_utf8_lossy(&bytes[..width]).into_owned());
        }
        bytes = &bytes[width..];
    }
    Ok(headers)
}

/// Fills `buffer`, tolerating the short poll timeout the worker's body read
/// uses so cancellation stays observable. Returns `false` at a clean EOF
/// before any byte of the buffer was read.
fn read_exact<R: Read>(
    reader: &mut R,
    buffer: &mut [u8],
    cancellation: &dyn Cancellation,
    target: &ProviderTarget,
) -> Result<bool, ProviderFailure> {
    let mut filled = 0;
    while filled < buffer.len() {
        if cancellation.is_cancelled() {
            return Err(super::transport::cancelled(PROVIDER));
        }
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => {
                return if filled == 0 {
                    Ok(false)
                } else {
                    Err(invalid_stream(
                        "Bedrock event stream ended mid-frame".into(),
                    ))
                };
            }
            Ok(read) => filled += read,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(error) => {
                return Err(super::transport::transport_failure(
                    format!("Bedrock stream read failed: {error}"),
                    target,
                ));
            }
        }
    }
    Ok(true)
}

// -- stream accumulation -------------------------------------------------

#[derive(Debug)]
enum BlockState {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input: String,
    },
    Reasoning {
        text: String,
        signature: Option<String>,
        redacted: Option<Value>,
    },
}

#[derive(Default)]
struct Accumulator {
    blocks: BTreeMap<usize, BlockState>,
    completed: BTreeMap<usize, ProviderContent>,
    display_only_reasoning: usize,
    stop_reason: Option<String>,
    usage: ProviderUsage,
}

pub(crate) fn parse_event_stream<R: Read>(
    mut reader: R,
    request_id: Option<String>,
    cancellation: &dyn Cancellation,
    sink: &mut dyn EventSink,
    target: &ProviderTarget,
) -> Result<ProviderResponse, ProviderFailure> {
    let mut accumulator = Accumulator::default();
    while let Some(frame) = read_frame(&mut reader, cancellation, target)? {
        sink.push(ProviderStreamEvent::ProtocolActivity);
        let value: Value = if frame.payload.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&frame.payload)
                .map_err(|error| invalid_stream(format!("invalid Bedrock event JSON: {error}")))?
        };
        if frame.message_type.as_deref() == Some("exception")
            || frame.exception_type.is_some()
            || frame
                .event_type
                .as_deref()
                .is_some_and(|name| name.ends_with("Exception"))
        {
            let name = frame
                .exception_type
                .or(frame.event_type)
                .unwrap_or_else(|| "UnknownException".into());
            return Err(classify_exception(&name, &value, target));
        }
        let Some(event) = frame.event_type.as_deref() else {
            continue;
        };
        process_event(event, &value, &mut accumulator, sink, target)?;
    }
    finish_response(accumulator, request_id)
}

fn process_event(
    event: &str,
    value: &Value,
    accumulator: &mut Accumulator,
    sink: &mut dyn EventSink,
    target: &ProviderTarget,
) -> Result<(), ProviderFailure> {
    match event {
        "messageStart" => {
            sink.push(ProviderStreamEvent::MessageStarted {
                id: format!("bedrock-{}", target.model.id),
                model: target.model.id.clone(),
            });
        }
        "contentBlockStart" => {
            let index = block_index(value)?;
            if let Some(tool) = value.pointer("/start/toolUse") {
                accumulator.blocks.insert(
                    index,
                    BlockState::ToolUse {
                        id: required_string(tool, "toolUseId")?,
                        name: required_string(tool, "name")?,
                        input: String::new(),
                    },
                );
            }
        }
        "contentBlockDelta" => {
            let index = block_index(value)?;
            let delta = value
                .get("delta")
                .ok_or_else(|| invalid_stream("contentBlockDelta has no delta".into()))?;
            if let Some(text) = delta.get("text").and_then(Value::as_str) {
                let state = accumulator
                    .blocks
                    .entry(index)
                    .or_insert_with(|| BlockState::Text(String::new()));
                let BlockState::Text(buffer) = state else {
                    return Err(invalid_stream(
                        "a text delta landed on a non-text block".into(),
                    ));
                };
                check_cap(buffer.len(), text.len())?;
                buffer.push_str(text);
                sink.push(ProviderStreamEvent::TextDelta {
                    index,
                    text: text.to_string(),
                });
            }
            if let Some(fragment) = delta.pointer("/toolUse/input").and_then(Value::as_str) {
                let Some(BlockState::ToolUse { input, .. }) = accumulator.blocks.get_mut(&index)
                else {
                    return Err(invalid_stream(
                        "a toolUse delta landed on a block that never started".into(),
                    ));
                };
                check_cap(input.len(), fragment.len())?;
                input.push_str(fragment);
                sink.push(ProviderStreamEvent::ToolInputDelta {
                    index,
                    partial_json: fragment.to_string(),
                });
            }
            if let Some(reasoning) = delta.get("reasoningContent") {
                let state =
                    accumulator
                        .blocks
                        .entry(index)
                        .or_insert_with(|| BlockState::Reasoning {
                            text: String::new(),
                            signature: None,
                            redacted: None,
                        });
                let BlockState::Reasoning {
                    text,
                    signature,
                    redacted,
                } = state
                else {
                    return Err(invalid_stream(
                        "a reasoning delta landed on a non-reasoning block".into(),
                    ));
                };
                if let Some(chunk) = reasoning.get("text").and_then(Value::as_str) {
                    check_cap(text.len(), chunk.len())?;
                    text.push_str(chunk);
                    sink.push(ProviderStreamEvent::ThinkingDelta {
                        index,
                        text: chunk.to_string(),
                    });
                }
                if let Some(value) = reasoning.get("signature").and_then(Value::as_str) {
                    *signature = Some(value.to_string());
                }
                if let Some(value) = reasoning.get("redactedContent") {
                    *redacted = Some(value.clone());
                }
            }
        }
        "contentBlockStop" => {
            let index = block_index(value)?;
            let state = accumulator.blocks.remove(&index).ok_or_else(|| {
                invalid_stream(format!("contentBlockStop for an unopened block {index}"))
            })?;
            if let Some(block) = finish_block(state, &mut accumulator.display_only_reasoning)? {
                accumulator.completed.insert(index, block);
            }
            sink.push(ProviderStreamEvent::BlockCompleted { index });
        }
        "messageStop" => {
            accumulator.stop_reason = Some(required_string(value, "stopReason")?);
        }
        "metadata" => {
            if let Some(usage) = value.get("usage") {
                accumulator.usage = ProviderUsage {
                    input_tokens: usage
                        .get("inputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or_default(),
                    cache_creation_input_tokens: usage
                        .get("cacheWriteInputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or_default(),
                    cache_read_input_tokens: usage
                        .get("cacheReadInputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or_default(),
                    output_tokens: usage
                        .get("outputTokens")
                        .and_then(Value::as_u64)
                        .unwrap_or_default(),
                    reasoning_tokens: None,
                };
            }
        }
        _ => {}
    }
    Ok(())
}

fn finish_block(
    state: BlockState,
    display_only_reasoning: &mut usize,
) -> Result<Option<ProviderContent>, ProviderFailure> {
    match state {
        BlockState::Text(text) => Ok(Some(ProviderContent::Text { text })),
        BlockState::ToolUse { id, name, input } => {
            let parsed: Value = serde_json::from_str(if input.is_empty() { "{}" } else { &input })
                .map_err(|error| {
                    ProviderFailure::new(
                        FailureClass::InvalidToolArguments,
                        FailureScope::request(),
                        format!("Bedrock toolUse `{name}` returned incomplete JSON: {error}"),
                    )
                })?;
            if !parsed.is_object() {
                return Err(ProviderFailure::new(
                    FailureClass::InvalidToolArguments,
                    FailureScope::request(),
                    format!("Bedrock toolUse `{name}` input is not a JSON object"),
                ));
            }
            Ok(Some(ProviderContent::ToolUse {
                id,
                name,
                input: parsed,
            }))
        }
        BlockState::Reasoning {
            text,
            signature,
            redacted,
        } => {
            if let Some(data) = redacted {
                return Ok(Some(ProviderContent::RedactedThinking {
                    data: OpaqueProviderData::new(data),
                }));
            }
            match signature {
                // Only a signed reasoning block is continuation material.
                Some(signature) => Ok(Some(ProviderContent::Thinking {
                    thinking: text,
                    signature: OpaqueProviderData::new(
                        json!({"type": REASONING_ENVELOPE, "signature": signature}),
                    ),
                })),
                None => {
                    *display_only_reasoning += text.len();
                    Ok(None)
                }
            }
        }
    }
}

fn finish_response(
    accumulator: Accumulator,
    request_id: Option<String>,
) -> Result<ProviderResponse, ProviderFailure> {
    let stop_reason = accumulator
        .stop_reason
        .ok_or_else(|| invalid_stream("the Bedrock stream ended before messageStop".into()))?;
    if !accumulator.blocks.is_empty() {
        return Err(invalid_stream(
            "the Bedrock stream stopped with an unfinished content block".into(),
        ));
    }
    let mut content: Vec<ProviderContent> = accumulator.completed.into_values().collect();
    let truncated = stop_reason == "max_tokens";
    let mut omitted = Vec::new();
    if truncated {
        // A truncated turn never hands the runtime an executable call.
        content.retain(|block| match block {
            ProviderContent::ToolUse { id, .. } => {
                omitted.push(id.clone());
                false
            }
            _ => true,
        });
    }
    let finish_reason = match stop_reason.as_str() {
        "end_turn" => FinishReason::EndTurn,
        "tool_use" => FinishReason::ToolUse,
        "max_tokens" => FinishReason::MaxTokens,
        "stop_sequence" => FinishReason::StopSequence,
        "content_filtered" | "guardrail_intervened" => FinishReason::Refusal,
        other => FinishReason::Unknown(other.to_string()),
    };
    let stop_details = (!omitted.is_empty() || accumulator.display_only_reasoning > 0).then(|| {
        OpaqueProviderData::new(json!({
            "omitted_tool_calls": omitted,
            "display_only_reasoning_len": accumulator.display_only_reasoning,
        }))
    });
    Ok(ProviderResponse {
        message_id: request_id
            .clone()
            .unwrap_or_else(|| "bedrock-stream".to_string()),
        model: String::new(),
        content,
        finish_reason,
        stop_sequence: None,
        stop_details,
        usage: accumulator.usage,
        request_id,
    })
}

fn block_index(value: &Value) -> Result<usize, ProviderFailure> {
    value
        .get("contentBlockIndex")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| invalid_stream("Bedrock event has no contentBlockIndex".into()))
}

fn required_string(value: &Value, field: &str) -> Result<String, ProviderFailure> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| invalid_stream(format!("Bedrock event has no string `{field}`")))
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
    super::transport::transport_failure(format!("Bedrock transport failed: {error}"), target)
}

fn classify_exception(name: &str, value: &Value, target: &ProviderTarget) -> ProviderFailure {
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Bedrock stream failed")
        .to_string();
    let (class, scope_kind, retryable) = match name {
        "throttlingException" | "ThrottlingException" => (
            FailureClass::RateLimited,
            FailureScopeKind::BillingPool,
            true,
        ),
        "modelStreamErrorException" | "ModelStreamErrorException" => {
            (FailureClass::Provider, FailureScopeKind::Endpoint, true)
        }
        "serviceUnavailableException" | "ServiceUnavailableException" => {
            (FailureClass::Overloaded, FailureScopeKind::Provider, true)
        }
        "validationException" | "ValidationException" => (
            FailureClass::Configuration,
            FailureScopeKind::Request,
            false,
        ),
        "accessDeniedException" | "AccessDeniedException" => {
            (FailureClass::Permission, FailureScopeKind::Account, false)
        }
        "modelNotReadyException" | "ModelNotReadyException" => {
            (FailureClass::ModelAccess, FailureScopeKind::Model, true)
        }
        _ => (FailureClass::Provider, FailureScopeKind::Endpoint, true),
    };
    let mut failure = ProviderFailure::new(
        class,
        target_scope(target, scope_kind),
        format!("{name}: {message}"),
    );
    failure.retry.retryable = retryable;
    failure
}

fn classify_http_error(
    status: u16,
    body: &str,
    request_id: Option<String>,
    retry_after_ms: Option<u64>,
    target: &ProviderTarget,
) -> ProviderFailure {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let message = parsed
        .get("message")
        .or_else(|| parsed.get("Message"))
        .and_then(Value::as_str)
        .unwrap_or("Bedrock rejected the request")
        .to_string();
    let (class, scope_kind, retryable) = match status {
        // SigV4 rejects a bad signature, a skewed clock and an unknown key
        // alike with 403, so the class is authentication rather than a
        // permission decision about the model.
        400 => (
            FailureClass::Configuration,
            FailureScopeKind::Request,
            false,
        ),
        403 => (
            FailureClass::Authentication,
            FailureScopeKind::Account,
            false,
        ),
        404 => (FailureClass::ModelAccess, FailureScopeKind::Model, false),
        424 => (FailureClass::Provider, FailureScopeKind::Endpoint, true),
        429 => (
            FailureClass::RateLimited,
            FailureScopeKind::BillingPool,
            true,
        ),
        503 => (FailureClass::Overloaded, FailureScopeKind::Provider, true),
        500..=599 => (FailureClass::Provider, FailureScopeKind::Endpoint, true),
        _ => (
            FailureClass::Configuration,
            FailureScopeKind::Request,
            false,
        ),
    };
    let mut failure = ProviderFailure::new(class, target_scope(target, scope_kind), message);
    failure.http_status = Some(status);
    failure.provider_request_id = request_id;
    failure.retry = RetryHint {
        retryable,
        after_ms: retry_after_ms,
    };
    failure
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::commands::ctx::provider::adapter::{Effort, NeverCancelled, ProviderMessage};
    use crate::commands::ctx::provider::credential::Secret;
    use crate::commands::ctx::provider::profiles::profile;
    use crate::commands::ctx::provider::testhttp::one_shot_bytes_server;
    use crate::commands::ctx::provider::{
        AccountId, BillingPoolId, EndpointId, ModelId, ProviderId,
    };
    use crate::commands::ctx::runtime::tools::ToolRegistry;

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/provider/bedrock/v1/",
                $name
            ))
        };
    }

    const MULTI_TOOL: &str = fixture!("converse-multi-tool-reasoning.events");
    const FINAL_TEXT: &str = fixture!("converse-final-text.events");
    const MALFORMED_TOOL: &str = fixture!("converse-malformed-tool.events");
    const TRUNCATED: &str = fixture!("converse-truncated.events");
    const MAX_TOKENS: &str = fixture!("converse-max-tokens.events");
    const THROTTLING: &str = fixture!("converse-throttling.events");
    const UNSIGNED_REASONING: &str = fixture!("converse-unsigned-reasoning.events");

    const MODEL: &str = "anthropic.claude-sonnet-5-v1:0";

    /// Builds one real AWS event-stream frame around `payload`, with the
    /// `:event-type` and `:message-type` string headers a live stream
    /// carries. The CRC fields are present but zero: this transport does not
    /// verify them (see the module note), and a fixture that pretended to
    /// would only be testing the fixture generator.
    fn frame(event_type: &str, payload: &[u8]) -> Vec<u8> {
        let mut headers = Vec::new();
        for (name, value) in [(":event-type", event_type), (":message-type", "event")] {
            headers.push(u8::try_from(name.len()).unwrap());
            headers.extend_from_slice(name.as_bytes());
            headers.push(7);
            headers.extend_from_slice(&u16::try_from(value.len()).unwrap().to_be_bytes());
            headers.extend_from_slice(value.as_bytes());
        }
        let total = 12 + headers.len() + payload.len() + 4;
        let mut out = Vec::new();
        out.extend_from_slice(&u32::try_from(total).unwrap().to_be_bytes());
        out.extend_from_slice(&u32::try_from(headers.len()).unwrap().to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&headers);
        out.extend_from_slice(payload);
        out.extend_from_slice(&0u32.to_be_bytes());
        out
    }

    fn frames(fixture: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for line in fixture.lines() {
            let line = line.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (event, payload) = line.split_once('\t').expect("fixture row is event\\tjson");
            out.extend_from_slice(&frame(event, payload.as_bytes()));
        }
        out
    }

    fn target(base_url: String) -> ProviderTarget {
        ProviderTarget {
            route: RouteId::new("work").unwrap(),
            provider: ProviderId::new("aws-bedrock").unwrap(),
            endpoint: EndpointId::new("bedrock").unwrap(),
            account: AccountId::new("work").unwrap(),
            billing_pool: BillingPoolId::new("work").unwrap(),
            protocol: Protocol::AwsBedrock,
            base_url,
            model: ModelId {
                vendor: "anthropic".into(),
                id: MODEL.into(),
            },
        }
    }

    fn credential() -> Credential {
        Credential {
            secret: Secret::new(
                r#"{"access_key_id":"AKIDEXAMPLE","secret_access_key":"never-log-this"}"#.into(),
            ),
            expires_at: None,
        }
    }

    fn adapter(base_url: String) -> BedrockAdapter {
        BedrockAdapter::new(
            target(base_url),
            &credential(),
            "us-east-1".into(),
            StreamTimeouts::default(),
            profile("aws-bedrock-anthropic").unwrap(),
            1_757_721_600,
        )
        .unwrap()
    }

    fn request() -> ProviderRequest {
        ProviderRequest {
            model: MODEL.into(),
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
        fixture: &str,
        events: &mut Vec<ProviderStreamEvent>,
    ) -> Result<ProviderResponse, ProviderFailure> {
        let bytes = frames(fixture);
        parse_event_stream(
            bytes.as_slice(),
            Some("req-1".into()),
            &NeverCancelled,
            events,
            &target("https://bedrock-runtime.us-east-1.amazonaws.com".into()),
        )
    }

    #[test]
    fn the_event_stream_reassembles_text_tool_use_and_signed_reasoning() {
        let mut events = Vec::new();
        let response = parse(MULTI_TOOL, &mut events).unwrap();
        assert_eq!(response.finish_reason, FinishReason::ToolUse);
        assert!(matches!(
            &response.content[0],
            ProviderContent::Thinking { thinking, signature }
                if thinking == "Read both files first."
                    && reasoning_signature(signature) == Some("sig-abc")
        ));
        assert!(matches!(
            &response.content[1],
            ProviderContent::Text { text } if text == "Reading both files."
        ));
        assert!(matches!(
            &response.content[2],
            ProviderContent::ToolUse { id, name, input }
                if id == "tool_1" && name == "file_read" && input["path"] == json!("src/lib.rs")
        ));
        assert_eq!(response.usage.input_tokens, 130);
        assert_eq!(response.usage.output_tokens, 44);
        assert_eq!(response.usage.cache_read_input_tokens, 20);
        assert_eq!(response.usage.cache_creation_input_tokens, 5);
        assert!(events.iter().any(|event| matches!(
            event,
            ProviderStreamEvent::ToolInputDelta { partial_json, .. }
                if partial_json.contains("path")
        )));
    }

    #[test]
    fn unsigned_reasoning_is_display_only_and_never_becomes_replayable_content() {
        let mut events = Vec::new();
        let response = parse(UNSIGNED_REASONING, &mut events).unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            ProviderStreamEvent::ThinkingDelta { text, .. } if text == "unsigned thinking"
        )));
        assert!(
            !response
                .content
                .iter()
                .any(|block| matches!(block, ProviderContent::Thinking { .. })),
            "reasoning without a signature cannot be replayed and is not content"
        );
        let details = response.stop_details.expect("the omission is recorded");
        assert_eq!(details.expose()["display_only_reasoning_len"], json!(17));
    }

    #[test]
    fn malformed_tool_input_never_becomes_a_tool_call() {
        let error = parse(MALFORMED_TOOL, &mut Vec::new()).unwrap_err();
        assert_eq!(error.class, FailureClass::InvalidToolArguments);
        assert!(error.message.contains("incomplete JSON"));
    }

    #[test]
    fn a_stream_without_message_stop_is_never_a_completion() {
        let error = parse(TRUNCATED, &mut Vec::new()).unwrap_err();
        assert_eq!(error.class, FailureClass::InvalidStream);
        assert!(error.message.contains("before messageStop"));
    }

    #[test]
    fn a_max_tokens_stop_drops_every_tool_call_and_records_it() {
        let response = parse(MAX_TOKENS, &mut Vec::new()).unwrap();
        assert_eq!(response.finish_reason, FinishReason::MaxTokens);
        assert!(
            !response
                .content
                .iter()
                .any(|block| matches!(block, ProviderContent::ToolUse { .. }))
        );
        assert_eq!(
            response.stop_details.unwrap().expose()["omitted_tool_calls"],
            json!(["tool_cut"])
        );
    }

    #[test]
    fn stream_exceptions_are_typed_with_a_retry_hint() {
        let error = parse(THROTTLING, &mut Vec::new()).unwrap_err();
        assert_eq!(error.class, FailureClass::RateLimited);
        assert_eq!(error.scope.kind, FailureScopeKind::BillingPool);
        assert!(error.retry.retryable);
        assert!(error.message.contains("Too many requests"));
    }

    #[test]
    fn a_truncated_frame_is_a_typed_stream_failure_not_a_short_turn() {
        let mut bytes = frames(FINAL_TEXT);
        bytes.truncate(bytes.len() - 6);
        let error = parse_event_stream(
            bytes.as_slice(),
            None,
            &NeverCancelled,
            &mut Vec::new(),
            &target("https://bedrock-runtime.us-east-1.amazonaws.com".into()),
        )
        .unwrap_err();
        assert_eq!(error.class, FailureClass::InvalidStream);
        assert!(error.message.contains("mid-frame"));
    }

    #[test]
    fn the_request_is_sigv4_signed_and_addressed_by_an_encoded_model_id() {
        let (url, captured) = one_shot_bytes_server(
            200,
            frames(MULTI_TOOL),
            "application/vnd.amazon.eventstream",
        );
        let response = adapter(url)
            .stream(&request(), &NeverCancelled, &mut Vec::new())
            .unwrap();
        assert_eq!(response.finish_reason, FinishReason::ToolUse);
        let sent = captured.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            sent.starts_with(
                "POST /model/anthropic.claude-sonnet-5-v1%3A0/converse-stream HTTP/1.1"
            ),
            "got {}",
            sent.lines().next().unwrap_or_default()
        );
        let lower = sent.to_ascii_lowercase();
        assert!(lower.contains("authorization: aws4-hmac-sha256 credential=akidexample/"));
        assert!(lower.contains("/us-east-1/bedrock/aws4_request"));
        assert!(lower.contains("x-amz-date: "));
        assert!(lower.contains("x-amz-content-sha256: "));
        // The secret itself is never a header value.
        assert!(!lower.contains("never-log-this"));
        assert!(sent.contains("\"toolConfig\""));
        assert!(sent.contains("\"inferenceConfig\""));
        assert!(sent.contains("\"maxTokens\":4096"));
    }

    #[test]
    fn a_multi_turn_tool_task_replays_its_signed_reasoning_and_tool_results() {
        let (first_url, _first) = one_shot_bytes_server(
            200,
            frames(MULTI_TOOL),
            "application/vnd.amazon.eventstream",
        );
        let first = adapter(first_url)
            .stream(&request(), &NeverCancelled, &mut Vec::new())
            .unwrap();
        let calls: Vec<String> = first
            .content
            .iter()
            .filter_map(|block| match block {
                ProviderContent::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1);

        let mut second = request();
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
        let (second_url, second_capture) = one_shot_bytes_server(
            200,
            frames(FINAL_TEXT),
            "application/vnd.amazon.eventstream",
        );
        let final_turn = adapter(second_url)
            .stream(&second, &NeverCancelled, &mut Vec::new())
            .unwrap();
        assert_eq!(final_turn.finish_reason, FinishReason::EndTurn);
        let replay = second_capture.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(replay.contains("\"toolResult\""));
        assert!(replay.contains("\"toolUseId\":\"tool_1\""));
        assert!(replay.contains("\"signature\":\"sig-abc\""));
        assert!(replay.contains("\"status\":\"success\""));
    }

    #[test]
    fn converse_refuses_what_it_has_no_representation_for() {
        let adapter = adapter("https://bedrock-runtime.us-east-1.amazonaws.com".into());
        let base = request();

        let mut effort = base.clone();
        effort.effort = Some(Effort::High);
        assert!(
            adapter
                .encode_request(&effort)
                .unwrap_err()
                .message
                .contains("no reasoning-effort control")
        );

        let mut adaptive = base.clone();
        adaptive.thinking = ThinkingConfig::Adaptive { display: None };
        assert!(
            adapter
                .encode_request(&adaptive)
                .unwrap_err()
                .message
                .contains("no Converse representation")
        );

        let mut cache = base.clone();
        cache.cache = CacheMode::Ephemeral5m;
        assert!(
            adapter
                .encode_request(&cache)
                .unwrap_err()
                .message
                .contains("cachePoint")
        );

        let mut refusal = base.clone();
        refusal.messages.push(ProviderMessage {
            role: ProviderMessageRole::Assistant,
            content: vec![ProviderContent::Refusal { text: "no".into() }],
        });
        assert!(
            adapter
                .encode_request(&refusal)
                .unwrap_err()
                .message
                .contains("no refusal content block")
        );

        let mut foreign_reasoning = base;
        foreign_reasoning.messages.push(ProviderMessage {
            role: ProviderMessageRole::Assistant,
            content: vec![ProviderContent::Thinking {
                thinking: "kept".into(),
                signature: OpaqueProviderData::new(json!({"type":"reasoning","id":"rs_1"})),
            }],
        });
        assert!(
            adapter
                .encode_request(&foreign_reasoning)
                .unwrap_err()
                .message
                .contains("never replayed into a")
        );
    }

    #[test]
    fn a_generic_bedrock_profile_declares_no_replayable_reasoning() {
        let mut nova_target = target("https://bedrock-runtime.us-east-1.amazonaws.com".into());
        nova_target.model = ModelId {
            vendor: "amazon".into(),
            id: "amazon.nova-pro-v1:0".into(),
        };
        let adapter = BedrockAdapter::new(
            nova_target,
            &credential(),
            "us-east-1".into(),
            StreamTimeouts::default(),
            profile("aws-bedrock-converse").unwrap(),
            1_757_721_600,
        )
        .unwrap();
        let mut request = request();
        request.model = "amazon.nova-pro-v1:0".into();
        request.messages.push(ProviderMessage {
            role: ProviderMessageRole::Assistant,
            content: vec![ProviderContent::Thinking {
                thinking: "kept".into(),
                signature: OpaqueProviderData::new(
                    json!({"type":"bedrock_reasoning","signature":"s"}),
                ),
            }],
        });
        assert!(
            adapter
                .encode_request(&request)
                .unwrap_err()
                .message
                .contains("no replayable reasoning")
        );
    }

    #[test]
    fn construction_refuses_a_bare_key_plaintext_and_a_missing_region() {
        let bare_key = BedrockAdapter::new(
            target("https://bedrock-runtime.us-east-1.amazonaws.com".into()),
            &Credential {
                secret: Secret::new("AKIAEXAMPLE".into()),
                expires_at: None,
            },
            "us-east-1".into(),
            StreamTimeouts::default(),
            profile("aws-bedrock-anthropic").unwrap(),
            0,
        )
        .unwrap_err();
        assert_eq!(bare_key.class, FailureClass::Authentication);
        assert!(bare_key.message.contains("access_key_id"));

        let plaintext = BedrockAdapter::new(
            target("http://bedrock-runtime.us-east-1.amazonaws.com".into()),
            &credential(),
            "us-east-1".into(),
            StreamTimeouts::default(),
            profile("aws-bedrock-anthropic").unwrap(),
            0,
        )
        .unwrap_err();
        assert!(plaintext.message.contains("plaintext HTTP"));

        let no_region = BedrockAdapter::new(
            target("https://bedrock-runtime.us-east-1.amazonaws.com".into()),
            &credential(),
            "  ".into(),
            StreamTimeouts::default(),
            profile("aws-bedrock-anthropic").unwrap(),
            0,
        )
        .unwrap_err();
        assert!(no_region.message.contains("region"));
    }

    #[test]
    fn http_failures_separate_a_rejected_signature_from_a_missing_model() {
        for (status, body, class, scope) in [
            (
                403,
                r#"{"message":"signature does not match"}"#,
                FailureClass::Authentication,
                FailureScopeKind::Account,
            ),
            (
                404,
                r#"{"message":"model not found"}"#,
                FailureClass::ModelAccess,
                FailureScopeKind::Model,
            ),
            (
                429,
                r#"{"message":"too many requests"}"#,
                FailureClass::RateLimited,
                FailureScopeKind::BillingPool,
            ),
        ] {
            let (url, _captured) =
                one_shot_bytes_server(status, body.as_bytes().to_vec(), "application/json");
            let error = adapter(url)
                .stream(&request(), &NeverCancelled, &mut Vec::new())
                .unwrap_err();
            assert_eq!(error.class, class, "status {status}");
            assert_eq!(error.scope.kind, scope, "status {status}");
            assert_eq!(error.http_status, Some(status));
        }
    }
}
