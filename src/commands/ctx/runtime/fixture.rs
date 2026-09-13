//! Deterministic fixture provider and tool executor for the native agent
//! loop (issue #478, roadmap N09).
//!
//! Loop correctness must be provable without a paid provider call, a network
//! socket, a real filesystem effect or an installed coding harness. These two
//! types are how: a [`FixtureProvider`] replays a scripted sequence of
//! provider turns (in either primary protocol's shape) and a
//! [`FixtureToolExecutor`] replays scripted tool receipts, both driven from
//! JSON under `tests/fixtures/runtime/native/`.
//!
//! Production-compiled (no `cfg(test)`) for the same reason `fake.rs` is: a
//! real, selectable stand-in that later roadmap steps and built-in checks can
//! run a native session against is not the same thing as test-only scaffolding.
//! Nothing here reads a clock, an environment variable or a network.

#![allow(dead_code)] // Consumed by this module's own tests and by later roadmap steps.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::Deserialize;

use super::super::provider::adapter::{
    Cancellation, EventSink, FailureClass, FailureScope, FinishReason, ProviderAdapter,
    ProviderContent, ProviderFailure, ProviderRequest, ProviderResponse, ProviderStreamEvent,
    ProviderTarget, ProviderUsage, RetryHint,
};
use super::super::provider::{
    AccountId, BillingPoolId, EndpointId, ModelId, Protocol, ProviderId, RouteId,
};
use super::native::{NativeToolCall, ToolExecutor};
use super::tools::{
    RetryPolicy, ToolDefinition, ToolError, ToolErrorCode, ToolReceipt, ToolReceiptState,
    ToolRegistry,
};

/// Every fixture failure is an internal one from the loop's point of view:
/// nothing here reaches a real broker, filesystem or process.
fn fixture_error(message: impl Into<String>) -> ToolError {
    ToolError::new(ToolErrorCode::Internal, message)
}

/// One scripted provider turn.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureTurn {
    /// `anthropic` or `openai`: selects which protocol's streaming shape is
    /// emitted into the sink. The committed response is identical either way
    /// -- that is the point of the provider-neutral contract -- but the event
    /// sequence a consumer observes is not.
    #[serde(default = "default_shape")]
    pub shape: String,
    #[serde(default)]
    pub message_id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub blocks: Vec<FixtureBlock>,
    #[serde(default = "default_finish")]
    pub finish_reason: String,
    #[serde(default)]
    pub usage: FixtureUsage,
    /// When set, this attempt fails instead of answering.
    #[serde(default)]
    pub failure: Option<FixtureFailure>,
}

fn default_shape() -> String {
    "anthropic".to_string()
}

fn default_finish() -> String {
    "end_turn".to_string()
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum FixtureBlock {
    Text {
        text: String,
        /// Split the text into this many deltas before the block completes,
        /// so a consumer sees a genuinely incremental stream.
        #[serde(default)]
        deltas: usize,
    },
    Refusal {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
        /// Emit the arguments as these literal partial-JSON fragments before
        /// the block completes. The COMPLETE `input` is still what the
        /// response carries: a partial fragment is a stream artifact, never
        /// an executable call.
        #[serde(default)]
        partial_json: Vec<String>,
    },
    /// A tool-use block whose argument stream never completes. The response
    /// carries a non-object `input`, which the loop must refuse to execute.
    TruncatedToolUse {
        id: String,
        name: String,
        partial_json: Vec<String>,
    },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureFailure {
    pub class: String,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
}

/// A whole scripted provider script.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureScript {
    #[serde(default = "default_shape")]
    pub shape: String,
    #[serde(default)]
    pub model: String,
    pub turns: Vec<FixtureTurn>,
}

impl FixtureScript {
    pub fn from_json(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|error| error.to_string())
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        Self::from_json(&text)
    }
}

/// The root every native loop fixture lives under.
pub fn fixture_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/runtime/native")
}

/// A deterministic [`ProviderAdapter`] that replays a [`FixtureScript`].
///
/// Each `stream` call consumes the next scripted turn. A script that runs out
/// of turns answers with an explicit `Provider` failure rather than looping,
/// so a loop bug shows up as a failed test instead of an infinite run.
#[derive(Debug)]
pub struct FixtureProvider {
    target: ProviderTarget,
    script: FixtureScript,
    next: AtomicUsize,
    sent: std::sync::Mutex<Vec<ProviderRequest>>,
}

impl FixtureProvider {
    pub fn new(target: ProviderTarget, script: FixtureScript) -> Self {
        Self {
            target,
            script,
            next: AtomicUsize::new(0),
            sent: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// How many scripted turns have been consumed so far.
    pub fn consumed(&self) -> usize {
        self.next.load(Ordering::Acquire)
    }

    /// Every request the loop actually sent, in order. This is the only way
    /// to assert what a CONTINUATION request carried -- which tool results it
    /// replayed, and which attempt's result each one was.
    pub fn sent(&self) -> Vec<ProviderRequest> {
        self.sent
            .lock()
            .map(|sent| sent.clone())
            .unwrap_or_default()
    }
}

/// A target that names a route without reaching any configuration, so a
/// fixture run needs no credentials and no `[route]` table.
pub fn fixture_target(protocol: Protocol, model: &str) -> ProviderTarget {
    ProviderTarget {
        route: RouteId::new("fixture").expect("static slug"),
        provider: ProviderId::new(match protocol {
            Protocol::AnthropicMessages => "anthropic",
            _ => "openai",
        })
        .expect("static slug"),
        endpoint: EndpointId::new("fixture").expect("static slug"),
        account: AccountId::new("fixture").expect("static slug"),
        billing_pool: BillingPoolId::new("fixture").expect("static slug"),
        protocol,
        base_url: "https://fixture.invalid".to_string(),
        model: ModelId {
            vendor: "fixture".to_string(),
            id: model.to_string(),
        },
    }
}

impl ProviderAdapter for FixtureProvider {
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
        if let Ok(mut sent) = self.sent.lock() {
            sent.push(request.clone());
        }
        if cancellation.is_cancelled() {
            return Err(ProviderFailure::new(
                FailureClass::Cancelled,
                FailureScope::request(),
                "cancelled before the fixture stream started",
            ));
        }
        let index = self.next.fetch_add(1, Ordering::AcqRel);
        let Some(turn) = self.script.turns.get(index) else {
            return Err(ProviderFailure::new(
                FailureClass::Provider,
                FailureScope::request(),
                format!("fixture script has no turn {index}"),
            ));
        };

        if let Some(failure) = &turn.failure {
            let mut error = ProviderFailure::new(
                parse_class(&failure.class),
                FailureScope::request(),
                failure.message.clone(),
            );
            error.retry = RetryHint {
                retryable: failure.retryable,
                after_ms: None,
            };
            return Err(error);
        }

        let shape = if turn.shape.is_empty() {
            self.script.shape.as_str()
        } else {
            turn.shape.as_str()
        };
        let model = if turn.model.is_empty() {
            self.script.model.clone()
        } else {
            turn.model.clone()
        };
        let message_id = if turn.message_id.is_empty() {
            format!("fixture-msg-{index}")
        } else {
            turn.message_id.clone()
        };

        sink.push(ProviderStreamEvent::MessageStarted {
            id: message_id.clone(),
            model: model.clone(),
        });

        let mut content = Vec::new();
        for (block_index, block) in turn.blocks.iter().enumerate() {
            if cancellation.is_cancelled() {
                return Err(ProviderFailure::new(
                    FailureClass::Cancelled,
                    FailureScope::request(),
                    "cancelled mid-stream",
                ));
            }
            match block {
                FixtureBlock::Text { text, deltas } => {
                    for chunk in split_into(text, *deltas) {
                        sink.push(ProviderStreamEvent::TextDelta {
                            index: block_index,
                            text: chunk,
                        });
                    }
                    content.push(ProviderContent::Text { text: text.clone() });
                }
                FixtureBlock::Refusal { text } => {
                    content.push(ProviderContent::Refusal { text: text.clone() });
                }
                FixtureBlock::ToolUse {
                    id,
                    name,
                    input,
                    partial_json,
                } => {
                    for fragment in partial_json {
                        sink.push(ProviderStreamEvent::ToolInputDelta {
                            index: block_index,
                            partial_json: fragment.clone(),
                        });
                    }
                    content.push(ProviderContent::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    });
                }
                FixtureBlock::TruncatedToolUse {
                    id,
                    name,
                    partial_json,
                } => {
                    for fragment in partial_json {
                        sink.push(ProviderStreamEvent::ToolInputDelta {
                            index: block_index,
                            partial_json: fragment.clone(),
                        });
                    }
                    // The joined fragments are NOT a JSON object. The response
                    // carries them verbatim as a string so the loop's own
                    // completeness check is what refuses the call.
                    content.push(ProviderContent::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: serde_json::Value::String(partial_json.concat()),
                    });
                }
            }
            sink.push(ProviderStreamEvent::BlockCompleted { index: block_index });
        }

        // Anthropic's own stream carries a ping; the Responses stream does
        // not. The committed response is identical either way.
        if shape == "anthropic" {
            sink.push(ProviderStreamEvent::Ping);
        }

        Ok(ProviderResponse {
            message_id,
            model,
            content,
            finish_reason: parse_finish(&turn.finish_reason),
            stop_sequence: None,
            stop_details: None,
            usage: ProviderUsage {
                input_tokens: turn.usage.input_tokens,
                cache_creation_input_tokens: turn.usage.cache_creation_input_tokens,
                cache_read_input_tokens: turn.usage.cache_read_input_tokens,
                output_tokens: turn.usage.output_tokens,
                reasoning_tokens: turn.usage.reasoning_tokens,
            },
            request_id: Some(format!("fixture-req-{index}")),
        })
    }
}

fn split_into(text: &str, deltas: usize) -> Vec<String> {
    if deltas <= 1 || text.is_empty() {
        return vec![text.to_string()];
    }
    let chars: Vec<char> = text.chars().collect();
    let size = chars.len().div_ceil(deltas).max(1);
    chars
        .chunks(size)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

fn parse_class(value: &str) -> FailureClass {
    match value {
        "cancelled" => FailureClass::Cancelled,
        "first_event_timeout" => FailureClass::FirstEventTimeout,
        "idle_timeout" => FailureClass::IdleTimeout,
        "authentication" => FailureClass::Authentication,
        "permission" => FailureClass::Permission,
        "entitlement" => FailureClass::Entitlement,
        "model_access" => FailureClass::ModelAccess,
        "configuration" => FailureClass::Configuration,
        "context_overflow" => FailureClass::ContextOverflow,
        "rate_limited" => FailureClass::RateLimited,
        "overloaded" => FailureClass::Overloaded,
        "transport" => FailureClass::Transport,
        "invalid_stream" => FailureClass::InvalidStream,
        "invalid_tool_arguments" => FailureClass::InvalidToolArguments,
        _ => FailureClass::Provider,
    }
}

fn parse_finish(value: &str) -> FinishReason {
    match value {
        "end_turn" => FinishReason::EndTurn,
        "max_tokens" => FinishReason::MaxTokens,
        "stop_sequence" => FinishReason::StopSequence,
        "tool_use" => FinishReason::ToolUse,
        "pause_turn" => FinishReason::PauseTurn,
        "refusal" => FinishReason::Refusal,
        "context_window_exceeded" => FinishReason::ContextWindowExceeded,
        other => FinishReason::Unknown(other.to_string()),
    }
}

// -- the fixture tool executor -------------------------------------------

/// One scripted tool outcome.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureToolOutcome {
    /// `completed`, `failed` or `outcome_unknown`.
    pub state: String,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub message: String,
}

/// A whole scripted tool script: per tool name, the outcomes to hand back in
/// order. A name with a single outcome repeats it; a name with several walks
/// them, which is how a retry's second attempt can differ from its first.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureToolScript {
    #[serde(default)]
    pub tools: BTreeMap<String, Vec<FixtureToolOutcome>>,
}

impl FixtureToolScript {
    pub fn from_json(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|error| error.to_string())
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        Self::from_json(&text)
    }
}

/// A deterministic [`ToolExecutor`] that performs no effect at all and hands
/// back scripted receipts, recording the order it was actually called in.
#[derive(Debug)]
pub struct FixtureToolExecutor {
    definitions: Vec<ToolDefinition>,
    script: FixtureToolScript,
    attempts: BTreeMap<String, usize>,
    /// The tool-call ids in the order they were executed. The loop is free to
    /// run independent calls first, so this is deliberately NOT the order the
    /// provider declared them in -- and asserting the difference is how the
    /// ordering contract is proven.
    pub calls: Vec<String>,
    clock_ms: u64,
}

impl FixtureToolExecutor {
    pub fn new(script: FixtureToolScript) -> Self {
        Self {
            definitions: ToolRegistry::native().definitions().cloned().collect(),
            script,
            attempts: BTreeMap::new(),
            calls: Vec::new(),
            clock_ms: 0,
        }
    }

    /// Narrows the advertised definitions to `names`, so a fixture can put a
    /// small, readable tool surface in front of the model.
    pub fn with_only(mut self, names: &[&str]) -> Self {
        self.definitions
            .retain(|definition| names.contains(&definition.name.as_str()));
        self
    }
}

impl ToolExecutor for FixtureToolExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.definitions.clone()
    }

    fn execute(&mut self, call: &NativeToolCall) -> ToolReceipt {
        self.calls.push(call.id.to_string());
        self.clock_ms += 1;
        let attempt = self.attempts.entry(call.name.clone()).or_insert(0);
        let outcomes = self.script.tools.get(&call.name);
        let outcome = outcomes.and_then(|list| {
            if list.is_empty() {
                None
            } else {
                Some(&list[(*attempt).min(list.len() - 1)])
            }
        });
        *attempt += 1;

        let retry = self
            .definitions
            .iter()
            .find(|definition| definition.name == call.name)
            .map(|definition| definition.retry)
            .unwrap_or(RetryPolicy::Safe);

        let Some(outcome) = outcome else {
            return ToolReceipt {
                receipt_id: format!("fixture-receipt-{}", self.clock_ms),
                tool: call.name.clone(),
                state: ToolReceiptState::Failed,
                retry,
                result: None,
                error: Some(fixture_error(format!(
                    "fixture tool script has no outcome for `{}`",
                    call.name
                ))),
                policy_fingerprint: Some("fixture".to_string()),
                approved_by: None,
                started_at_ms: self.clock_ms,
                completed_at_ms: self.clock_ms,
            };
        };

        let state = match outcome.state.as_str() {
            "completed" => ToolReceiptState::Completed,
            "outcome_unknown" => ToolReceiptState::OutcomeUnknown,
            _ => ToolReceiptState::Failed,
        };
        ToolReceipt {
            receipt_id: format!("fixture-receipt-{}", self.clock_ms),
            tool: call.name.clone(),
            state,
            retry,
            result: match state {
                ToolReceiptState::Completed => {
                    Some(outcome.result.clone().unwrap_or(serde_json::Value::Null))
                }
                _ => None,
            },
            error: match state {
                ToolReceiptState::Completed => None,
                _ => Some(fixture_error(outcome.message.clone())),
            },
            policy_fingerprint: Some("fixture".to_string()),
            approved_by: None,
            started_at_ms: self.clock_ms,
            completed_at_ms: self.clock_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::provider::adapter::NeverCancelled;

    fn empty_request() -> ProviderRequest {
        ProviderRequest {
            model: "fixture".into(),
            system: Vec::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            max_output_tokens: 16,
            stop_sequences: Vec::new(),
            thinking: Default::default(),
            effort: None,
            cache: Default::default(),
        }
    }

    #[test]
    fn a_truncated_tool_argument_stream_never_becomes_an_object() {
        let script = FixtureScript::from_json(
            r#"{"turns":[{"blocks":[{"type":"truncated_tool_use","id":"call_1",
               "name":"file_read","partial_json":["{\"pa"]}],"finish_reason":"tool_use"}]}"#,
        )
        .expect("script");
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, "fixture-model"),
            script,
        );
        let mut events: Vec<ProviderStreamEvent> = Vec::new();
        let response = provider
            .stream(&empty_request(), &NeverCancelled, &mut events)
            .expect("response");
        let ProviderContent::ToolUse { input, .. } = &response.content[0] else {
            panic!("expected a tool use");
        };
        assert!(!input.is_object());
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProviderStreamEvent::ToolInputDelta { .. }))
        );
    }

    #[test]
    fn the_two_protocol_shapes_stream_differently_but_commit_the_same_response() {
        let body =
            r#"{"blocks":[{"type":"text","text":"hello","deltas":2}],"finish_reason":"end_turn"}"#;
        let anthropic = FixtureScript::from_json(&format!(
            r#"{{"shape":"anthropic","turns":[{{"shape":"anthropic",{}]}}"#,
            &body[1..]
        ))
        .expect("anthropic script");
        let openai = FixtureScript::from_json(&format!(
            r#"{{"shape":"openai","turns":[{{"shape":"openai",{}]}}"#,
            &body[1..]
        ))
        .expect("openai script");

        let mut anthropic_events = Vec::new();
        let anthropic_response =
            FixtureProvider::new(fixture_target(Protocol::AnthropicMessages, "m"), anthropic)
                .stream(&empty_request(), &NeverCancelled, &mut anthropic_events)
                .expect("anthropic response");

        let mut openai_events = Vec::new();
        let openai_response =
            FixtureProvider::new(fixture_target(Protocol::OpenAiResponses, "m"), openai)
                .stream(&empty_request(), &NeverCancelled, &mut openai_events)
                .expect("openai response");

        assert_eq!(anthropic_response.content, openai_response.content);
        assert!(anthropic_events.contains(&ProviderStreamEvent::Ping));
        assert!(!openai_events.contains(&ProviderStreamEvent::Ping));
    }

    #[test]
    fn a_script_that_runs_out_of_turns_fails_loudly() {
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, "m"),
            FixtureScript::from_json(r#"{"turns":[]}"#).expect("script"),
        );
        let error = provider
            .stream(&empty_request(), &NeverCancelled, &mut Vec::new())
            .expect_err("no turns");
        assert_eq!(error.class, FailureClass::Provider);
    }
}
