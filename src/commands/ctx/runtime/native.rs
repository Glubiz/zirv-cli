//! The native agent loop (issue #478, roadmap N09).
//!
//! This is the module that makes `RuntimeKind::Native` a real backend. It
//! conducts the provider request -> tool execution -> provider continuation
//! cycle itself, from an explicit session/turn/request/tool state machine,
//! over the contracts the earlier steps shipped:
//!
//! - N02/N07/N08 [`ProviderAdapter`] transports ONE already-compiled request.
//!   It never owns the loop, never executes a tool and never persists
//!   anything. No provider agent SDK is involved at any point.
//! - N03 [`Journal`] is the authoritative conversation. Every request this
//!   loop sends is rebuilt from the journal's own replay, so a request is a
//!   projection of durable facts rather than of in-memory bookkeeping that a
//!   crash would take with it.
//! - N04/N05 authorization and tools sit behind [`ToolExecutor`]. The
//!   production implementation is `NativeToolClient`, which brokers every
//!   action at effect time; the loop adds the durable barrier and the
//!   ordering contract on top and never reaches a filesystem itself.
//! - N09's own shared lifecycle services (`ctx::lifecycle`) decide tool
//!   admission and whether a session may stop. Those calls are direct: a
//!   native session never shells out to a hook and never probes for an
//!   installed coding harness.
//!
//! # The barrier
//!
//! A tool cannot execute until (1) its arguments parsed as a complete JSON
//! object, (2) the shared before-tool service admitted it, and (3) the
//! assistant message that requested it, plus the tool-call record itself, are
//! durably committed. A truncated argument stream therefore cannot become an
//! effect, and a crash between "committed" and "executed" leaves a record the
//! next open can reconcile rather than a silent gap.
//!
//! # Ordering
//!
//! Providers require tool results in the order their tool-use blocks were
//! emitted, keyed by call id. The scheduler here is free to run independent
//! (read-only) calls before mutating ones, so results can COMPLETE out of
//! order; [`TurnOutcome::results`] is always rebuilt in the provider's own
//! declared order before it goes back over the wire.
//!
//! # Input, steering and interruption
//!
//! Every accepted input is acknowledged durably (`InputAcknowledged`) the
//! moment it is taken, before anything can be lost. Delivery boundaries are
//! explicit: an acknowledged input joins the conversation at the next request
//! built for this session -- between requests inside a turn, or between turns
//! -- never mid-stream and never mid-tool. An input that has not reached a
//! boundary before the loop stops stays visibly queued in the final status,
//! so "delivered once, or still queued" is always decidable from durable
//! state.
//!
//! An interrupt cancels the in-flight provider stream, every tool that has
//! not started, and every remaining turn. It deliberately does NOT cancel an
//! effect that already started: that execution becomes `OutcomeUnknown` and
//! must be reconciled before any retry. Interrupted work is never reported
//! complete.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;

use super::super::CtxResult;
use super::super::config::{EnvLookup, OrchestratorWrites};
use super::super::lifecycle;
use super::super::provider::adapter::{
    Cancellation, CancellationFlag, EventSink, FailureClass, FinishReason, ProviderAdapter,
    ProviderContent, ProviderFailure, ProviderMessage, ProviderMessageRole, ProviderRequest,
    ProviderStreamEvent, ProviderUsage, journal_blocks, replayed_content,
};
use super::checkpoint::{self, CheckpointContext};
use super::compaction::{
    self, CompactionAction, CompactionDecision, CompactionPolicy, CompactionRecord, DistillBudget,
    NativeBudget, RETAIN_RECENT_MESSAGES,
};
use super::journal::{
    AssistantBlock, CheckpointId, CheckpointKind, ContentRef, ConversationState, EventScope,
    ExecutionId, ExecutionRecord, ExecutionState, Journal, JournalSessionId, MessageId,
    MessageRole, RequestAttemptId, RouteIdentity, SequenceId, ToolCallId, TurnId, UsageId,
    UsageRecord,
};
use super::tools::{
    NativeToolClient, ResourceClaimKind, RetryPolicy, ToolDefinition, ToolExecutionMode,
    ToolReceipt, ToolReceiptState,
};
use super::{
    BackendConversationRef, RuntimeBackend, RuntimeCapabilities, RuntimeError, RuntimeKind,
    SessionHandle, SessionSpec, UiSurface,
};

/// Bumped whenever [`NativeFinalStatus`]'s own shape changes. A consumer of
/// `zirv ctx exec --runtime native --json` branches on this, never on field
/// presence.
pub const FINAL_STATUS_SCHEMA_VERSION: u32 = 2;

/// The policy source label recorded on every tool call this loop prepares.
/// The authoritative fingerprint comes back on the receipt from the broker
/// that actually admitted the effect; this names who asked.
const POLICY_SOURCE: &str = "native-loop";

// -- limits --------------------------------------------------------------

/// Every bound a native session runs under. All of them are hard: the loop
/// stops at a limit and says which one, rather than continuing on a
/// best-effort basis.
///
/// `first_event_ms`/`idle_ms` are handed to the provider transport, which
/// owns the actual timers (`provider::transport::StreamTimeouts`); the loop
/// carries them so one struct describes the whole envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeLimits {
    pub max_turns: u32,
    pub max_requests_per_turn: u32,
    pub max_tool_calls: u32,
    pub max_wall_ms: u64,
    pub max_output_tokens: u64,
    /// How much of ONE tool result may go back to the model inline. Anything
    /// larger is stored whole as a journal artifact and replaced with a
    /// bounded head/tail extract naming its retrieval id, so a single huge
    /// result can neither blow the context window nor be silently lost.
    pub max_tool_result_bytes: usize,
    pub first_event_ms: u64,
    pub idle_ms: u64,
    /// How many times ONE request may be re-sent after a retryable provider
    /// failure. Distinct from `tool_retry_budget`: re-sending a request that
    /// produced no committed effect is free, re-running a tool is not.
    pub response_retry_budget: u32,
    /// How many times a FAILED tool whose retry policy is `Safe` may be
    /// re-run. `Reconcile`/`NeverAfterStart` tools and any outcome-unknown
    /// execution are never retried here at all.
    pub tool_retry_budget: u32,
}

impl Default for NativeLimits {
    fn default() -> Self {
        Self {
            max_turns: 64,
            max_requests_per_turn: 128,
            max_tool_calls: 512,
            max_wall_ms: 60 * 60 * 1000,
            max_output_tokens: 16_384,
            max_tool_result_bytes: 64 * 1024,
            first_event_ms: 60_000,
            idle_ms: 120_000,
            response_retry_budget: 3,
            tool_retry_budget: 1,
        }
    }
}

/// Which bound stopped the loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    Turns,
    RequestsPerTurn,
    ToolCalls,
    WallClock,
}

impl LimitKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LimitKind::Turns => "turns",
            LimitKind::RequestsPerTurn => "requests_per_turn",
            LimitKind::ToolCalls => "tool_calls",
            LimitKind::WallClock => "wall_clock",
        }
    }
}

// -- the state machine ---------------------------------------------------

/// The session-level state. `Idle` accepts a submit; `Running` refuses one
/// (a second concurrent turn is [`RuntimeError::Busy`], never a silent
/// interleave); the three terminal states accept nothing but observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Idle,
    Running,
    Interrupted,
    Completed,
    Failed,
}

/// The turn-level state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    /// Input is acknowledged; no request has been sent yet.
    Pending,
    /// A provider request is in flight.
    Requesting,
    /// The assistant message is committed and its tool calls are executing.
    ExecutingTools,
    /// Tool results are committed; the next continuation request is owed.
    Continuing,
    Completed,
    Interrupted,
    Failed,
}

/// The request-level state, per provider attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestState {
    Streaming,
    Committed,
    Retrying,
    Failed,
    Cancelled,
}

/// The tool-level state this loop tracks, mirroring the journal's own
/// [`ExecutionState`] one-for-one so the in-memory view and the durable
/// record can never disagree about what happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolState {
    /// Admitted and durably recorded, not started.
    Prepared,
    /// The effect began.
    Started,
    Completed,
    Failed,
    /// Never started, and now never will be.
    Cancelled,
    /// Started, outcome unknown. Requires reconciliation before any retry.
    OutcomeUnknown,
}

impl ToolState {
    fn journal_state(self) -> ExecutionState {
        match self {
            ToolState::Prepared => ExecutionState::Prepared,
            ToolState::Started => ExecutionState::Started,
            ToolState::Completed => ExecutionState::Completed,
            ToolState::Failed => ExecutionState::Failed,
            ToolState::Cancelled => ExecutionState::Cancelled,
            ToolState::OutcomeUnknown => ExecutionState::OutcomeUnknown,
        }
    }

    fn is_terminal(self) -> bool {
        matches!(
            self,
            ToolState::Completed | ToolState::Failed | ToolState::Cancelled
        )
    }
}

// -- the tool seam -------------------------------------------------------

/// One complete, admitted, durably committed tool call, as the executor
/// receives it.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeToolCall {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// The seam between the loop and whatever actually performs an effect.
///
/// The production implementation is [`ClientToolExecutor`], a thin wrapper
/// over N05's `NativeToolClient` (which re-reads policy and brokers every
/// action at effect time). A deterministic fixture implementation lets every
/// loop-correctness property below be proven without a filesystem, a
/// subprocess or a paid provider call.
pub trait ToolExecutor: std::fmt::Debug {
    /// The tool definitions the provider is told about.
    fn definitions(&self) -> Vec<ToolDefinition>;
    /// Performs one call. Returning a receipt is not an admission that the
    /// effect succeeded: `ToolReceiptState` carries that, and
    /// `ToolReceiptState::OutcomeUnknown` is how an executor says it cannot
    /// tell.
    fn execute(&mut self, call: &NativeToolCall) -> ToolReceipt;
}

/// The production [`ToolExecutor`]: N05's own client.
///
/// The client's optional journal argument is deliberately not used. This loop
/// owns the durable record for an execution -- it commits the assistant
/// message and the tool-call record BEFORE preflight and transitions the
/// execution itself -- so handing the client a second writer for the same
/// rows would duplicate every event and split ownership of the barrier.
#[derive(Debug)]
pub struct ClientToolExecutor {
    client: NativeToolClient,
}

impl ClientToolExecutor {
    pub fn new(client: NativeToolClient) -> Self {
        Self { client }
    }
}

impl ToolExecutor for ClientToolExecutor {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.client.registry().definitions().cloned().collect()
    }

    fn execute(&mut self, call: &NativeToolCall) -> ToolReceipt {
        self.client
            .execute(&call.name, call.arguments.clone(), None, None)
    }
}

// -- scheduling ----------------------------------------------------------

/// Whether a tool definition's effects are independent of every other call in
/// the same batch: it claims nothing but read roots, the output store and the
/// search index, and it is neither a background process nor process control.
///
/// Everything else -- any write claim, any memory-store claim, any process --
/// is serialized in the provider's declared order, because two such calls in
/// one batch can genuinely depend on each other. An UNKNOWN tool is dependent
/// by default: fail closed, never reorder something this build cannot classify.
fn is_independent(definition: Option<&ToolDefinition>) -> bool {
    let Some(definition) = definition else {
        return false;
    };
    if !matches!(
        definition.execution_mode,
        ToolExecutionMode::Immediate | ToolExecutionMode::Retrieval
    ) {
        return false;
    }
    definition.resource_claims.iter().all(|claim| {
        matches!(
            claim,
            ResourceClaimKind::ReadRoot
                | ResourceClaimKind::OutputStore
                | ResourceClaimKind::SearchIndex
        )
    })
}

/// The execution order for one batch of tool calls: every independent call
/// first, in declared order, then every dependent one, in declared order.
/// Deterministic by construction, and never the order results are reported
/// in -- see [`TurnOutcome::results`].
pub fn execution_order(kinds: &[bool]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..kinds.len()).filter(|i| kinds[*i]).collect();
    order.extend((0..kinds.len()).filter(|i| !kinds[*i]));
    order
}

// -- outcomes ------------------------------------------------------------

/// One tool call's durable outcome, as the next request needs it.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutcome {
    pub call: NativeToolCall,
    pub execution: ExecutionId,
    pub state: ToolState,
    pub content: String,
    pub is_error: bool,
    pub retry: RetryPolicy,
    pub policy_fingerprint: Option<String>,
    pub attempts: u32,
}

/// One finished turn.
#[derive(Clone, Debug, PartialEq)]
pub struct TurnOutcome {
    pub turn: TurnId,
    pub state: TurnState,
    pub requests: u32,
    /// Every tool result this turn produced, batch by batch, each batch in the
    /// provider's own declared order whatever order it actually completed in.
    pub results: Vec<ToolOutcome>,
    pub final_text: Option<String>,
    pub finish_reason: Option<FinishReason>,
    pub usage: ProviderUsage,
    pub failure: Option<String>,
    pub limit: Option<LimitKind>,
}

/// How a native session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeStatus {
    /// The model finished AND every tool execution reached a terminal state
    /// AND nothing else outranked the finish token.
    Completed,
    /// The model said it was finished, but something outranks that: an
    /// execution that never reported, an outcome-unknown effect, an
    /// undelivered input, or a lifecycle gate.
    Incomplete,
    Interrupted,
    LimitReached,
    Failed,
}

impl NativeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            NativeStatus::Completed => "completed",
            NativeStatus::Incomplete => "incomplete",
            NativeStatus::Interrupted => "interrupted",
            NativeStatus::LimitReached => "limit_reached",
            NativeStatus::Failed => "failed",
        }
    }

    /// The process exit code a headless native run reports. Mapped onto the
    /// supervisor's own vocabulary so `zirv ctx exec` consumers do not have
    /// to learn a second one.
    pub fn exit_code(self) -> i32 {
        match self {
            NativeStatus::Completed => 0,
            NativeStatus::Incomplete => super::super::exec::EXIT_CONTRACT_FAILED,
            NativeStatus::Interrupted => 130,
            NativeStatus::LimitReached => super::super::exec::EXIT_BUDGET_EXHAUSTED,
            NativeStatus::Failed => 1,
        }
    }
}

/// One piece of evidence behind the final status: a durable record a reader
/// can go and check, never a claim this struct makes on its own.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NativeEvidence {
    pub kind: &'static str,
    pub id: String,
    pub detail: String,
}

/// The structured final status of a native session.
///
/// A model finish token is ONE input here. `status` is `Completed` only when
/// nothing outranks it: no incomplete or outcome-unknown execution, no
/// undelivered acknowledged input, no interruption, no limit and no
/// lifecycle stop block.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NativeFinalStatus {
    pub schema_version: u32,
    pub runtime: &'static str,
    pub status: NativeStatus,
    pub session: String,
    pub route: String,
    pub provider: String,
    pub endpoint: String,
    pub account: String,
    pub billing_pool: String,
    /// The model the ROUTE resolves to.
    pub configured_model: String,
    /// The model the provider actually answered with, when it said. A route
    /// alias and the served model are not the same fact.
    pub served_model: Option<String>,
    pub turns: u32,
    pub requests: u32,
    pub tool_calls: u32,
    pub usage: ProviderUsage,
    /// Issue #487 (item 7): what this session actually settled, separated
    /// into billable and unpriced tokens and counted once per PROVIDER
    /// REQUEST rather than once per fold. `usage` above is the running
    /// accumulation the loop reports; this is the reconciliation that says
    /// how many distinct requests it came from and what each was billed as,
    /// which is what a spend readout can be checked against.
    pub reconciliation: super::super::route::Reconciliation,
    pub finish_reason: Option<String>,
    pub final_text: Option<String>,
    pub incomplete_tools: Vec<String>,
    pub outcome_unknown_tools: Vec<String>,
    pub queued_input: Vec<String>,
    pub limit: Option<LimitKind>,
    pub failure: Option<String>,
    pub blocked_reason: Option<String>,
    /// Issue #486: the compactions this run committed, oldest first, and the
    /// newest compaction decision -- including one that was only advice.
    /// Durable facts: every entry names a journal sequence a reader can go
    /// and check.
    pub compactions: Vec<CompactionRecord>,
    pub compaction_decision: Option<CompactionDecision>,
    pub evidence: Vec<NativeEvidence>,
    pub exit_code: i32,
}

// -- the loop ------------------------------------------------------------

/// Everything the loop needs that is not a live borrow.
#[derive(Clone, Debug)]
pub struct NativeSessionConfig {
    pub session: JournalSessionId,
    pub generation: u64,
    pub route: RouteIdentity,
    pub role: String,
    pub seat_model: Option<String>,
    pub write_posture: OrchestratorWrites,
    pub limits: NativeLimits,
    pub task: Option<super::journal::TaskId>,
    /// A workflow gate that refuses completion, with its own message. Fed
    /// straight into the shared stop service, where it outranks any model
    /// finish token.
    ///
    /// A fixed override, used by tests and by a caller that has already
    /// decided. Production leaves it `None` and sets [`Self::workflow_repo`]
    /// instead, so the gate is read LIVE at every completion attempt rather
    /// than snapshotted before the session had done anything.
    pub workflow_gate: Option<String>,
    /// Issue #486: how this session compacts itself. Default is a working
    /// configuration -- automatic policy, unknown context window, the shared
    /// scoring config's own thresholds -- so a caller that says nothing still
    /// gets compaction rather than a silently unprotected session.
    pub compaction: CompactionSettings,
    /// Issue #484 (roadmap N15): the repository whose ACTIVE workflow gates
    /// this session's completion. `None` for a session with no workflow in
    /// view -- a helper call, a fixture run -- which is gated by nothing.
    pub workflow_repo: Option<PathBuf>,
    /// Issue #484: the standing instructions this session runs under, compiled
    /// once by the native context compiler (`runtime::context`) -- the
    /// engineering standard, the role methodology, the model profile, the
    /// operator's and repository's own instruction files, and the active
    /// workflow's current step. Sent as the provider's system prompt on every
    /// request.
    ///
    /// Compiled ONCE, at session start, deliberately: it is the cacheable
    /// stable prefix, and rebuilding it each turn would defeat prompt caching
    /// for a refresh the session can ask for explicitly through the
    /// `workflow_context` tool. What must stay live is the workflow GATE, and
    /// that is read at every completion attempt (see `finalize`).
    pub system: Vec<String>,
    /// Issue #484: the untrusted DATA half of the same compilation --
    /// repository instruction files, canonical context, memory. Delivered as
    /// one leading user message rather than as instructions, because that is
    /// what it is.
    pub preamble: Vec<String>,
}

/// Everything one session's compaction needs that is not a live borrow.
#[derive(Clone, Debug)]
pub struct CompactionSettings {
    /// `false` disables compaction entirely for this loop. Observation still
    /// runs and the decision is still reported, so a disabled session says
    /// what it would have done.
    pub enabled: bool,
    pub policy: CompactionPolicy,
    pub budget: NativeBudget,
    /// The shared rot scoring config. Reused rather than duplicated: the
    /// native token gate differs only in the CAPACITY it is given, never in
    /// the thresholds an operator already configured.
    pub score: super::super::config::ScoreConfig,
    pub distill: DistillBudget,
    /// How many of the newest messages a compaction always leaves verbatim.
    pub retain_recent_messages: usize,
    /// Operator-stated hard constraints, carried into every checkpoint from
    /// the same typed source `runtime::context::CompileRequest` reads.
    pub constraints: Vec<String>,
    /// Where the portable checkpoint export is written. `None` keeps the
    /// journal event as the only copy, which is all a resume needs.
    pub state: Option<super::super::state::StateDir>,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            policy: CompactionPolicy::default(),
            budget: NativeBudget::default(),
            score: super::super::config::ScoreConfig::default(),
            distill: DistillBudget::default(),
            retain_recent_messages: RETAIN_RECENT_MESSAGES,
            constraints: Vec::new(),
            state: None,
        }
    }
}

/// The native agent loop itself.
///
/// Borrows rather than owns its collaborators so one journal and one tool
/// client can be shared with the rest of a session's machinery, and so a test
/// can substitute a deterministic provider and executor without any
/// production code knowing.
pub struct NativeLoop<'a> {
    config: NativeSessionConfig,
    provider: &'a dyn ProviderAdapter,
    tools: &'a mut dyn ToolExecutor,
    journal: &'a mut Journal,
    cancel: Arc<CancellationFlag>,
    now_ms: &'a dyn Fn() -> u64,
    env: EnvLookup<'a>,
    counter: u64,
    started_ms: u64,
    /// The last journal sequence already folded into a provider request.
    /// Everything after it is acknowledged-but-undelivered input.
    delivered_through: SequenceId,
    turns: u32,
    requests: u32,
    tool_calls: u32,
    usage: ProviderUsage,
    served_model: Option<String>,
    evidence: Vec<NativeEvidence>,
    /// Provider context-overflow refusals seen in this loop. A refused
    /// request commits nothing, so this is not a journal fact; it is handed
    /// to the scoring projection, which is where a non-event becomes a
    /// scoring signal.
    overflows: usize,
    /// Compactions this loop committed, oldest first.
    compactions: Vec<CompactionRecord>,
    /// The newest decision, whatever it was. Reported even when the policy
    /// or the `enabled` flag stopped it from being acted on.
    last_decision: Option<CompactionDecision>,
    /// Issue #487 (item 3): this session's own once-per-request settlement,
    /// keyed by the provider's own request id. A response-level retry that
    /// actually reached the provider, and a journal a second supervisor
    /// replays, both present the same request twice; folding it twice would
    /// double the pool's usage and the spend readout with it.
    reconciliation: super::super::route::Reconciliation,
}

impl std::fmt::Debug for NativeLoop<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeLoop")
            .field("config", &self.config)
            .field("provider", &self.provider)
            .field("tools", &self.tools)
            .field("turns", &self.turns)
            .field("requests", &self.requests)
            .field("tool_calls", &self.tool_calls)
            .finish_non_exhaustive()
    }
}

impl<'a> NativeLoop<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: NativeSessionConfig,
        provider: &'a dyn ProviderAdapter,
        tools: &'a mut dyn ToolExecutor,
        journal: &'a mut Journal,
        cancel: Arc<CancellationFlag>,
        now_ms: &'a dyn Fn() -> u64,
        env: EnvLookup<'a>,
    ) -> Self {
        let started_ms = now_ms();
        Self {
            config,
            provider,
            tools,
            journal,
            cancel,
            now_ms,
            env,
            counter: 0,
            started_ms,
            delivered_through: SequenceId(0),
            turns: 0,
            requests: 0,
            tool_calls: 0,
            usage: ProviderUsage::default(),
            served_model: None,
            evidence: Vec::new(),
            overflows: 0,
            compactions: Vec::new(),
            last_decision: None,
            reconciliation: super::super::route::Reconciliation::default(),
        }
    }

    /// A fresh id for one journal record.
    ///
    /// Namespaced by GENERATION, not just by a per-loop counter: a resumed
    /// session starts a new loop whose counter begins at zero again, and the
    /// journal rejects a duplicate usage/execution id outright. Without the
    /// generation in the name, every resume would collide with the records
    /// the previous generation already wrote -- which is exactly the failure
    /// a real resume surfaced.
    fn mint(&mut self, prefix: &str) -> String {
        self.counter += 1;
        format!("{prefix}-g{}-{}", self.config.generation, self.counter)
    }

    fn secs(&self) -> u64 {
        (self.now_ms)() / 1000
    }

    fn elapsed_ms(&self) -> u64 {
        (self.now_ms)().saturating_sub(self.started_ms)
    }

    fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    fn note(&mut self, kind: &'static str, id: impl Into<String>, detail: impl Into<String>) {
        self.evidence.push(NativeEvidence {
            kind,
            id: id.into(),
            detail: detail.into(),
        });
    }

    /// Durably acknowledges one input. Called before anything else can
    /// happen to it, so an input is never lost between "accepted" and
    /// "recorded" -- if this returns, the input is on disk.
    pub fn acknowledge(&mut self, text: &str, steering: bool) -> CtxResult<MessageId> {
        let message_id = MessageId::new(self.mint("msg"))?;
        let at_ms = (self.now_ms)();
        acknowledge_input(
            self.journal,
            &self.config.session,
            self.config.generation,
            message_id.clone(),
            text,
            steering,
            at_ms,
        )?;
        self.note(
            "input",
            message_id.to_string(),
            if steering { "steering" } else { "submit" },
        );
        Ok(message_id)
    }

    /// Acknowledged inputs that have not yet reached a delivery boundary.
    pub fn queued_input(&self) -> CtxResult<Vec<MessageId>> {
        let state = self.journal.replay(&self.config.session)?;
        Ok(state
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::User && m.sequence > self.delivered_through)
            .map(|m| m.message_id.clone())
            .collect())
    }

    /// Rebuilds the provider conversation from the journal alone, and marks
    /// everything in it delivered. This IS the delivery boundary: an input
    /// acknowledged before this call is in the request; one acknowledged
    /// after stays queued for the next boundary.
    fn build_request(&mut self) -> CtxResult<ProviderRequest> {
        let state = self.journal.replay(&self.config.session)?;
        let latest = latest_executions(&state);
        let mut messages: Vec<ProviderMessage> = Vec::new();
        let mut last = self.delivered_through;

        // Issue #486: the compaction in force, if any. Its summary message
        // stands in for every journal message at or before `covers_through`;
        // everything after it replays verbatim, including any tool call whose
        // effect is still unsettled -- the boundary is chosen so it never
        // crosses one. The stable prefix (`ProviderRequest::system`) is not
        // touched at all, which is what keeps provider prompt caching valid
        // across a compaction.
        let active = compaction::active(self.journal, &self.config.session)?;
        let covered = active.as_ref().map_or(SequenceId(0), |checkpoint| {
            SequenceId(checkpoint.covers_through)
        });
        if let Some(checkpoint) = &active {
            messages.push(compaction::summary_message(checkpoint));
        }

        for stored in &state.messages {
            if stored.sequence > last {
                last = stored.sequence;
            }
            if stored.sequence <= covered {
                continue;
            }
            match stored.role {
                MessageRole::User => {
                    let text = stored.text.clone().unwrap_or_default();
                    messages.push(ProviderMessage {
                        role: ProviderMessageRole::User,
                        content: vec![ProviderContent::Text { text }],
                    });
                }
                MessageRole::Assistant => {
                    let mut content = Vec::new();
                    let mut pending_results = Vec::new();
                    for block in &stored.blocks {
                        match block {
                            AssistantBlock::ToolCall { tool_call } => {
                                let Some(record) = state.tool_calls.get(tool_call) else {
                                    continue;
                                };
                                content.push(ProviderContent::ToolUse {
                                    id: tool_call.to_string(),
                                    name: record.name.clone(),
                                    input: record.arguments.clone(),
                                });
                                // Results follow their assistant message, in
                                // the provider's own declared block order,
                                // carrying each call's LATEST execution --
                                // never the failed attempt a retry replaced.
                                if let Some(execution) = latest.get(tool_call).copied() {
                                    let (text, is_error) =
                                        match (&execution.result, execution.state) {
                                            (Some(result), ExecutionState::Completed) => {
                                                (content_text(result), false)
                                            }
                                            (Some(result), _) => (content_text(result), true),
                                            (None, _) => (
                                                execution
                                                    .detail
                                                    .clone()
                                                    .unwrap_or_else(|| "no result".to_string()),
                                                true,
                                            ),
                                        };
                                    pending_results.push(ProviderContent::ToolResult {
                                        tool_use_id: tool_call.to_string(),
                                        content: text,
                                        is_error,
                                    });
                                }
                            }
                            other => {
                                if let Some(replayed) = replayed_content(other) {
                                    content.push(replayed);
                                }
                            }
                        }
                    }
                    if !content.is_empty() {
                        messages.push(ProviderMessage {
                            role: ProviderMessageRole::Assistant,
                            content,
                        });
                    }
                    if !pending_results.is_empty() {
                        messages.push(ProviderMessage {
                            role: ProviderMessageRole::User,
                            content: pending_results,
                        });
                    }
                }
            }
        }

        self.delivered_through = last;
        // Issue #484: the compiled standing context leads every request. The
        // instruction half is the provider's system prompt; the untrusted data
        // half is a leading user message, ahead of the journal's own replay,
        // so a session never has to be hand-seeded with methodology.
        if !self.config.preamble.is_empty() {
            messages.insert(
                0,
                ProviderMessage {
                    role: ProviderMessageRole::User,
                    content: self
                        .config
                        .preamble
                        .iter()
                        .map(|text| ProviderContent::Text { text: text.clone() })
                        .collect(),
                },
            );
        }
        Ok(ProviderRequest {
            model: self.config.route.model.id.clone(),
            system: self.config.system.clone(),
            messages,
            tools: self.tools.definitions(),
            max_output_tokens: self.config.limits.max_output_tokens,
            stop_sequences: Vec::new(),
            thinking: Default::default(),
            effort: None,
            cache: Default::default(),
        })
    }

    /// Observes this session and decides whether it should compact.
    ///
    /// Observation reads the journal; the decision itself is pure
    /// ([`compaction::evaluate`]). Split deliberately, so a test can assert a
    /// verdict without a database and a status reader can ask what the
    /// decision WOULD be without acting on it.
    pub fn compaction_decision(&self) -> CtxResult<CompactionDecision> {
        let observation = compaction::observe(self.journal, &self.config.session, self.overflows)?;
        Ok(compaction::evaluate(
            &observation,
            &self.config.compaction.score,
            self.config.compaction.budget,
            self.config.compaction.policy,
        ))
    }

    /// Records one provider context-overflow refusal and compacts in response
    /// to it, when the policy allows. Returns whether the request may be
    /// re-sent.
    ///
    /// An advisory policy deliberately returns `false`: "zirv may not compact
    /// this session on its own" has to mean the session stops with the
    /// overflow, or the narrowing would be decorative.
    fn recover_from_overflow(&mut self, scope: &EventScope) -> CtxResult<bool> {
        self.overflows = self.overflows.saturating_add(1);
        let decision = self.compaction_decision()?;
        let act = decision.should_compact() && self.config.compaction.enabled;
        let reason = decision.reason.clone();
        self.last_decision = Some(decision);
        if !act {
            let policy = self.config.compaction.policy.as_str();
            self.note(
                "compaction_skipped",
                "policy",
                format!("context overflow ({reason}), but the compaction policy is {policy}"),
            );
            return Ok(false);
        }
        self.compact_now(scope, &reason)
    }

    /// Evaluates, and compacts when the decision, the policy and the enable
    /// flag all say so. Returns whether a compaction was actually committed.
    ///
    /// An `Advise` decision is recorded as evidence and nothing else: that is
    /// what an advisory policy means, and what a below-threshold session that
    /// is merely repeating itself gets.
    fn maybe_compact(&mut self, scope: &EventScope) -> CtxResult<bool> {
        let decision = self.compaction_decision()?;
        let act = decision.should_compact() && self.config.compaction.enabled;
        let reason = decision.reason.clone();
        if decision.action == CompactionAction::Advise {
            self.note(
                "compaction_advice",
                decision.verdict.as_str(),
                reason.clone(),
            );
        }
        self.last_decision = Some(decision);
        if !act {
            return Ok(false);
        }
        self.compact_now(scope, &reason)
    }

    /// Commits one compaction.
    ///
    /// Returns `false` -- without writing anything -- when compacting would
    /// settle nothing: no boundary exists (everything is either too recent or
    /// behind an unsettled tool call), or the boundary is no further along
    /// than the compaction already in force. That second guard is what stops
    /// a session that is over its budget for some other reason from
    /// compacting on every single request.
    fn compact_now(&mut self, scope: &EventScope, reason: &str) -> CtxResult<bool> {
        let state = self.journal.replay(&self.config.session)?;
        let Some(boundary) =
            checkpoint::boundary(&state, self.config.compaction.retain_recent_messages)
        else {
            self.note(
                "compaction_skipped",
                "no_boundary",
                "nothing before the retained tail has settled",
            );
            return Ok(false);
        };
        let already = compaction::active(self.journal, &self.config.session)?
            .map_or(0, |checkpoint| checkpoint.covers_through);
        if boundary.0 <= already {
            self.note(
                "compaction_skipped",
                "no_progress",
                format!("already compacted through sequence {already}"),
            );
            return Ok(false);
        }

        // Distillation runs through this session's OWN native route, with a
        // bounded output budget and no tool schemas at all. It cannot fail:
        // with no provider capacity, no credential, a refusal or an attempted
        // tool call it returns the deterministic structural summary instead.
        let distilled = compaction::distill(
            Some(self.provider),
            &self.config.route.model.id,
            self.cancel.as_ref(),
            &state,
            boundary,
            self.config.compaction.distill,
        );

        let checkpoint_id = CheckpointId::new(self.mint("checkpoint"))?;
        let now = self.secs();

        // A distillation that really called the route is a real cost. It is
        // recorded in the journal and summed into this session's usage like
        // any other request, so compaction can never be a spend a reader
        // cannot see.
        if distilled.usage != ProviderUsage::default() {
            let usage_id = UsageId::new(self.mint("usage"))?;
            self.journal.record_usage(
                &self.config.session,
                self.config.generation,
                scope,
                UsageRecord {
                    id: usage_id,
                    input_tokens: distilled.usage.input_tokens,
                    cache_creation_input_tokens: distilled.usage.cache_creation_input_tokens,
                    cache_read_input_tokens: distilled.usage.cache_read_input_tokens,
                    output_tokens: distilled.usage.output_tokens,
                    reasoning_tokens: distilled.usage.reasoning_tokens,
                    provider_request_id: None,
                    estimated: false,
                },
                now,
            )?;
            accumulate(&mut self.usage, &distilled.usage);
        }
        let summary = distilled.summary;
        let portable = checkpoint::build(
            &state,
            boundary,
            self.delivered_through,
            &checkpoint_id,
            &CheckpointContext {
                hard_constraints: self.config.compaction.constraints.clone(),
                task: self.config.task.as_ref().map(|task| task.to_string()),
                workflow: None,
                reason: reason.to_string(),
            },
            summary,
            now,
        );
        let sequence = checkpoint::commit(
            self.journal,
            self.config.compaction.state.as_ref(),
            self.config.generation,
            scope,
            CheckpointKind::Compaction,
            &portable,
            now,
        )?;
        // Every overflow seen so far has now been addressed. Leaving the
        // count standing would make the next decision propose a compaction
        // that has already happened.
        self.overflows = 0;
        self.note(
            "compaction",
            checkpoint_id.to_string(),
            format!(
                "{reason}; covered through sequence {} with a {} summary",
                portable.covers_through, portable.summary.source
            ),
        );
        self.compactions.push(CompactionRecord {
            sequence: sequence.0,
            kind: "compaction".to_string(),
            reason: reason.to_string(),
            covers_through: portable.covers_through,
            summary_source: portable.summary.source.clone(),
            created_at: now,
        });
        Ok(true)
    }

    /// Whether a provider failure may be retried by simply re-sending the
    /// request. Only failures that cannot have produced a committed effect
    /// qualify; a refusal, an authentication problem or a context overflow
    /// would produce the same answer forever.
    fn response_retryable(failure: &ProviderFailure) -> bool {
        if failure.class == FailureClass::Cancelled {
            return false;
        }
        failure.retry.retryable
            || matches!(
                failure.class,
                FailureClass::Transport
                    | FailureClass::Overloaded
                    | FailureClass::RateLimited
                    | FailureClass::FirstEventTimeout
                    | FailureClass::IdleTimeout
            )
    }

    /// This loop's route, in the placement model's own vocabulary (#487).
    pub(crate) fn route_identity(&self) -> super::super::route::RouteIdentity {
        super::super::route::RouteIdentity::from_runtime(&self.config.route)
    }

    /// Which scope one provider failure is evidence about. Pure: the whole
    /// decision lives in `route::route_failure`, so it can be replayed from
    /// the failure and the identity alone.
    fn failure_routing(&self, failure: &ProviderFailure) -> super::super::route::FailureRouting {
        super::super::route::route_failure(failure, &self.route_identity())
    }

    /// Folds one completed request's settled usage into this session's
    /// reconciliation, exactly once (#487 item 3).
    ///
    /// The key is the provider's own request id where there is one. A
    /// response with no id falls back to the route plus this session's own
    /// monotonic request counter, which is stable for the REQUEST -- the
    /// retry loop in [`Self::stream_once`] does not mint a new one on a
    /// retry that ultimately returns this same response.
    fn reconcile_request(&mut self, response: &super::super::provider::adapter::ProviderResponse) {
        let key = match &response.request_id {
            Some(id) => id.clone(),
            None => format!("{}#{}", self.config.route.route, self.requests),
        };
        // A pool id is not a posture, and the loop does not carry the
        // account config that states one. Metered API credit is the
        // conservative reading: over-reporting billable usage is visible to
        // an operator, under-reporting is not.
        let billing = super::super::route::BillingPosture::Api;
        let (next, verdict) = super::super::route::reconcile(
            &self.reconciliation,
            &key,
            super::super::route::Settled {
                input_tokens: response.usage.input_tokens,
                output_tokens: response.usage.output_tokens,
            },
            0,
            billing,
        );
        self.reconciliation = next;
        if verdict == super::super::route::Reconciled::AlreadyCounted {
            self.note(
                "usage_already_reconciled",
                key,
                "this provider request was already counted; not folded again".to_string(),
            );
        }
    }

    /// Sends one request, retrying within the response-retry budget. Returns
    /// `Ok(None)` when cancellation won the race.
    fn stream_once(
        &mut self,
        request: &ProviderRequest,
        events: &mut Vec<ProviderStreamEvent>,
    ) -> Result<Option<super::super::provider::adapter::ProviderResponse>, ProviderFailure> {
        let mut attempt = 0u32;
        loop {
            if self.cancelled() {
                return Ok(None);
            }
            self.requests += 1;
            let mut sink = CollectingSink(events);
            match self
                .provider
                .stream(request, self.cancel.as_ref(), &mut sink)
            {
                Ok(response) => {
                    self.served_model = Some(response.model.clone());
                    return Ok(Some(response));
                }
                Err(failure) => {
                    if self.cancelled() {
                        return Ok(None);
                    }
                    if attempt < self.config.limits.response_retry_budget
                        && Self::response_retryable(&failure)
                    {
                        attempt += 1;
                        self.note(
                            "response_retry",
                            format!("attempt-{attempt}"),
                            failure.message.clone(),
                        );
                        continue;
                    }
                    return Err(failure);
                }
            }
        }
    }

    /// Runs one turn: request, commit, tools, continue, until the model stops
    /// asking for tools or a bound stops the loop.
    ///
    /// A TURN is one unit of user intent -- an acknowledged input driven to
    /// the point where the model stops asking for tools -- so a session's
    /// turn count is how many separate things it was told to do. The bound is
    /// enforced HERE rather than in [`Self::run_to_completion`] so it holds
    /// for any driver: a caller stepping turns itself (an interactive surface,
    /// a test) is bounded exactly as the headless loop is.
    pub fn run_turn(&mut self) -> CtxResult<TurnOutcome> {
        if self.turns >= self.config.limits.max_turns {
            let turn = TurnId::new(self.mint("turn"))?;
            return Ok(TurnOutcome {
                turn,
                state: TurnState::Failed,
                requests: 0,
                results: Vec::new(),
                final_text: None,
                finish_reason: None,
                usage: ProviderUsage::default(),
                failure: None,
                limit: Some(LimitKind::Turns),
            });
        }
        self.turns += 1;
        let turn = TurnId::new(self.mint("turn"))?;
        let mut outcome = TurnOutcome {
            turn: turn.clone(),
            state: TurnState::Pending,
            requests: 0,
            results: Vec::new(),
            final_text: None,
            finish_reason: None,
            usage: ProviderUsage::default(),
            failure: None,
            limit: None,
        };

        // One compaction recovery per turn. A second overflow after a
        // compaction that already committed means the remaining tail alone
        // does not fit, and re-compacting would settle nothing -- the turn
        // fails explicitly instead of looping.
        let mut overflow_recoveries = 0u32;

        for request_index in 0..self.config.limits.max_requests_per_turn {
            if self.cancelled() {
                outcome.state = TurnState::Interrupted;
                return Ok(outcome);
            }
            if self.elapsed_ms() > self.config.limits.max_wall_ms {
                outcome.state = TurnState::Failed;
                outcome.limit = Some(LimitKind::WallClock);
                return Ok(outcome);
            }

            let attempt = RequestAttemptId::new(self.mint("attempt"))?;
            let scope = EventScope {
                turn: Some(turn.clone()),
                attempt: Some(attempt.clone()),
                task: self.config.task.clone(),
            };

            // Issue #486: decide BEFORE the request is built, so a compaction
            // takes effect on the very request that needed it. Skipped on the
            // first request of a session, where no usage has been reported
            // and there is nothing yet to measure.
            if self.requests > 0 {
                self.maybe_compact(&scope)?;
            }

            outcome.state = TurnState::Requesting;
            let request = self.build_request()?;
            let mut events: Vec<ProviderStreamEvent> = Vec::new();
            let response = match self.stream_once(&request, &mut events) {
                Ok(Some(response)) => response,
                Ok(None) => {
                    outcome.state = TurnState::Interrupted;
                    return Ok(outcome);
                }
                Err(failure) => {
                    // A context overflow commits nothing, so recovering from
                    // it repeats no effect: compact, then rebuild the (now
                    // smaller) request from the journal and send it again.
                    if failure.class == FailureClass::ContextOverflow
                        && overflow_recoveries == 0
                        && self.recover_from_overflow(&scope)?
                    {
                        overflow_recoveries += 1;
                        continue;
                    }
                    outcome.state = TurnState::Failed;
                    outcome.failure = Some(failure.to_string());
                    // #487 item 4: the failure is recorded with the SCOPE it
                    // is evidence about, so a rejected credential does not
                    // read as an endpoint outage, an over-long prompt does
                    // not read as a failure at all, and a rate limit never
                    // reaches a breaker. `breaker_key` is `None` for exactly
                    // the classes that are not health evidence.
                    let routing = self.failure_routing(&failure);
                    let breaker = match routing.breaker_key() {
                        Some((key, class)) => {
                            format!("health evidence for {} as {class:?}", key.label())
                        }
                        None => "not health evidence".to_string(),
                    };
                    self.note(
                        "provider_failure",
                        attempt.to_string(),
                        format!(
                            "{failure} [route {}; scoped to {}; {breaker}]",
                            self.route_identity().label(),
                            routing.label(),
                        ),
                    );
                    return Ok(outcome);
                }
            };
            if response.finish_reason == FinishReason::ContextWindowExceeded {
                // The provider answered but said the window was exceeded. The
                // reply is already committed below; the count makes the next
                // decision see the overflow.
                self.overflows = self.overflows.saturating_add(1);
            }
            outcome.requests += 1;

            // Usage first: an assistant message may reference it, and the
            // journal enforces that the reference resolves.
            let usage_id = UsageId::new(self.mint("usage"))?;
            let now = self.secs();
            self.journal.record_usage(
                &self.config.session,
                self.config.generation,
                &scope,
                UsageRecord {
                    id: usage_id.clone(),
                    input_tokens: response.usage.input_tokens,
                    cache_creation_input_tokens: response.usage.cache_creation_input_tokens,
                    cache_read_input_tokens: response.usage.cache_read_input_tokens,
                    output_tokens: response.usage.output_tokens,
                    reasoning_tokens: response.usage.reasoning_tokens,
                    provider_request_id: response.request_id.clone(),
                    estimated: false,
                },
                now,
            )?;
            accumulate(&mut self.usage, &response.usage);
            accumulate(&mut outcome.usage, &response.usage);
            self.reconcile_request(&response);

            // THE BARRIER. The assistant message -- with its complete tool
            // calls -- is committed before any preflight below can run.
            let blocks = journal_blocks(&response.content)
                .map_err(|failure| Box::new(failure) as Box<dyn std::error::Error>)?;
            let message_id = MessageId::new(self.mint("msg"))?;
            self.journal.record_assistant_message(
                &self.config.session,
                self.config.generation,
                &scope,
                message_id.clone(),
                blocks.clone(),
                Some(usage_id),
                Some((self.now_ms)()),
                now,
            )?;

            let text = collect_text(&response.content);
            if !text.is_empty() {
                outcome.final_text = Some(text);
            }
            outcome.finish_reason = Some(response.finish_reason.clone());

            let calls = tool_uses(&response.content);
            if calls.is_empty() {
                outcome.state = TurnState::Completed;
                return Ok(outcome);
            }

            if self.tool_calls.saturating_add(calls.len() as u32)
                > self.config.limits.max_tool_calls
            {
                outcome.state = TurnState::Failed;
                outcome.limit = Some(LimitKind::ToolCalls);
                return Ok(outcome);
            }

            outcome.state = TurnState::ExecutingTools;
            outcome.results.extend(self.run_tools(&scope, &calls)?);
            self.tool_calls += calls.len() as u32;

            if self.cancelled() {
                outcome.state = TurnState::Interrupted;
                return Ok(outcome);
            }
            outcome.state = TurnState::Continuing;

            if request_index + 1 == self.config.limits.max_requests_per_turn {
                outcome.state = TurnState::Failed;
                outcome.limit = Some(LimitKind::RequestsPerTurn);
                return Ok(outcome);
            }
        }

        outcome.state = TurnState::Failed;
        outcome.limit = Some(LimitKind::RequestsPerTurn);
        Ok(outcome)
    }

    /// Admits, commits, schedules and executes one batch of tool calls,
    /// returning their outcomes in the PROVIDER's declared order.
    fn run_tools(
        &mut self,
        scope: &EventScope,
        calls: &[NativeToolCall],
    ) -> CtxResult<Vec<ToolOutcome>> {
        let definitions = self.tools.definitions();
        let by_name: BTreeMap<&str, &ToolDefinition> =
            definitions.iter().map(|d| (d.name.as_str(), d)).collect();

        // Preflight: schema, admission, durable record. Nothing executes
        // until every call in the batch has cleared this.
        let mut prepared: Vec<PreparedCall> = Vec::new();
        for call in calls {
            let definition = by_name.get(call.name.as_str()).copied();
            let intent = lifecycle::ToolIntent {
                tool: call.name.clone(),
                write_target: write_target(&call.name, &call.arguments),
                subagent: None,
                delegated: false,
            };
            let admission = lifecycle::before_tool(
                self.config.seat_model.as_deref(),
                Some(self.config.role.as_str()),
                &intent,
                &self.env,
                self.config.write_posture,
            );

            let now = self.secs();
            let at_ms = (self.now_ms)();
            self.journal.prepare_tool_call(
                &self.config.session,
                self.config.generation,
                scope,
                call.id.clone(),
                call.name.clone(),
                call.arguments.clone(),
                super::journal::PolicyProvenance {
                    fingerprint: String::new(),
                    source: POLICY_SOURCE.to_string(),
                    decision: admission.log_label().to_string(),
                    scope: self.config.role.clone(),
                },
                Some(at_ms),
                now,
            )?;
            let execution = ExecutionId::new(self.mint("exec"))?;
            self.journal.prepare_execution(
                &self.config.session,
                self.config.generation,
                scope,
                execution.clone(),
                call.id.clone(),
                Some(at_ms),
                now,
            )?;
            prepared.push(PreparedCall {
                call: call.clone(),
                execution,
                independent: is_independent(definition),
                // Fail closed, the same rule `is_independent` applies to an
                // unknown tool: a call this build cannot classify gets the
                // most restrictive contract there is, never the most
                // permissive one. Assuming `Safe` for something whose effects
                // are unknown is how a mutation gets silently repeated.
                retry: definition
                    .map(|d| d.retry)
                    .unwrap_or(RetryPolicy::NeverAfterStart),
                denied: match &admission {
                    lifecycle::ToolAdmission::Deny(reason) => Some(reason.clone()),
                    _ => None,
                },
                advice: match &admission {
                    lifecycle::ToolAdmission::Advise(note) => Some(note.clone()),
                    _ => None,
                },
            });
        }

        let flags: Vec<bool> = prepared.iter().map(|p| p.independent).collect();
        let order = execution_order(&flags);

        let mut outcomes: BTreeMap<usize, ToolOutcome> = BTreeMap::new();
        for index in order {
            let entry = &prepared[index];
            let outcome = if let Some(reason) = entry.denied.clone() {
                // Denied by policy: never started, so `Cancelled` is the
                // honest terminal state, and the model gets the reason.
                self.transition(
                    scope,
                    &entry.execution,
                    ToolState::Cancelled,
                    None,
                    Some(reason.as_str()),
                )?;
                ToolOutcome {
                    call: entry.call.clone(),
                    execution: entry.execution.clone(),
                    state: ToolState::Cancelled,
                    content: reason,
                    is_error: true,
                    retry: entry.retry,
                    policy_fingerprint: None,
                    attempts: 0,
                }
            } else if self.cancelled() {
                self.transition(
                    scope,
                    &entry.execution,
                    ToolState::Cancelled,
                    None,
                    Some("interrupted before this effect started"),
                )?;
                ToolOutcome {
                    call: entry.call.clone(),
                    execution: entry.execution.clone(),
                    state: ToolState::Cancelled,
                    content: "interrupted before this effect started".to_string(),
                    is_error: true,
                    retry: entry.retry,
                    policy_fingerprint: None,
                    attempts: 0,
                }
            } else {
                self.execute_one(scope, entry)?
            };
            outcomes.insert(index, outcome);
        }

        // Provider-declared order, whatever order execution finished in.
        Ok((0..prepared.len())
            .filter_map(|index| outcomes.remove(&index))
            .collect())
    }

    /// Runs one prepared call, with the tool-retry budget applied only where
    /// the tool's own retry policy allows it.
    fn execute_one(&mut self, scope: &EventScope, entry: &PreparedCall) -> CtxResult<ToolOutcome> {
        let mut attempts = 0u32;
        let mut execution = entry.execution.clone();
        loop {
            attempts += 1;
            self.transition(scope, &execution, ToolState::Started, None, None)?;
            let receipt = self.tools.execute(&entry.call);
            let (state, content, is_error) = classify(&receipt);
            // The shared after-tool service decides whether this result is
            // worth replacing. `Replace` stores the WHOLE result as a journal
            // artifact first, so the bounded extract the model reads can never
            // be the only surviving copy.
            let (content, result) = match self.disposition(&content)? {
                (lifecycle::ResultDisposition::Keep, reference) => (content, reference),
                (lifecycle::ResultDisposition::Replace { retrieval_id }, reference) => (
                    bounded_extract(
                        &content,
                        self.config.limits.max_tool_result_bytes,
                        &retrieval_id,
                    ),
                    reference,
                ),
            };
            let terminal_with_result = matches!(state, ToolState::Completed | ToolState::Failed);
            let result = terminal_with_result.then_some(result);
            self.transition(
                scope,
                &execution,
                state,
                result,
                (!terminal_with_result).then_some(content.as_str()),
            )?;

            // A tool-effect retry is NOT a response retry. An outcome-unknown
            // effect is never replayed blindly, whatever the budget says --
            // only a failure whose own contract declares it safe to repeat.
            let may_retry = state == ToolState::Failed
                && entry.retry == RetryPolicy::Safe
                && attempts <= self.config.limits.tool_retry_budget
                && !self.cancelled();
            if !may_retry {
                if state == ToolState::OutcomeUnknown {
                    self.note(
                        "outcome_unknown",
                        execution.to_string(),
                        "effect began and its outcome is unknown; reconcile before any retry",
                    );
                }
                let mut content = content;
                if let Some(advice) = &entry.advice {
                    content = format!("{content}\n{advice}");
                }
                return Ok(ToolOutcome {
                    call: entry.call.clone(),
                    execution,
                    state,
                    content,
                    is_error,
                    retry: entry.retry,
                    policy_fingerprint: receipt.policy_fingerprint.clone(),
                    attempts,
                });
            }
            // A retry needs its OWN execution record: the journal's execution
            // state machine is terminal at `Failed`, and reusing the id would
            // both be rejected and hide that a second effect happened.
            let retried = ExecutionId::new(self.mint("exec"))?;
            let now = self.secs();
            self.journal.prepare_execution(
                &self.config.session,
                self.config.generation,
                scope,
                retried.clone(),
                entry.call.id.clone(),
                Some((self.now_ms)()),
                now,
            )?;
            self.note(
                "tool_retry",
                entry.call.id.to_string(),
                format!("attempt {attempts} failed under a Safe retry policy"),
            );
            execution = retried;
        }
    }

    /// Applies the shared after-tool service to one tool result: `Keep` with
    /// an inline reference when it is small enough, or `Replace` once the
    /// whole text is durably stored as an artifact this journal can hand back.
    fn disposition(
        &mut self,
        content: &str,
    ) -> CtxResult<(lifecycle::ResultDisposition, ContentRef)> {
        if !lifecycle::should_compact_result(
            content.len(),
            true,
            self.config.limits.max_tool_result_bytes,
        ) {
            return Ok((
                lifecycle::ResultDisposition::Keep,
                ContentRef::Inline {
                    text: content.to_string(),
                },
            ));
        }
        let now = self.secs();
        let reference = self
            .journal
            .put_artifact("text/plain", content.as_bytes(), now)?;
        let retrieval_id = match &reference {
            ContentRef::Artifact { sha256, .. } => sha256.clone(),
            ContentRef::Inline { .. } => String::new(),
        };
        self.note(
            "tool_result_offloaded",
            retrieval_id.clone(),
            format!("{} bytes stored as an artifact", content.len()),
        );
        Ok((
            lifecycle::ResultDisposition::Replace { retrieval_id },
            reference,
        ))
    }

    fn transition(
        &mut self,
        scope: &EventScope,
        execution: &ExecutionId,
        to: ToolState,
        result: Option<ContentRef>,
        detail: Option<&str>,
    ) -> CtxResult<()> {
        let now = self.secs();
        self.journal.transition_execution(
            &self.config.session,
            self.config.generation,
            scope,
            execution,
            to.journal_state(),
            result,
            detail.map(str::to_string),
            Some((self.now_ms)()),
            now,
        )?;
        Ok(())
    }

    /// Runs turns until the model stops asking for tools, a bound is hit, or
    /// an interrupt lands, then builds the final status.
    pub fn run_to_completion(&mut self) -> CtxResult<NativeFinalStatus> {
        let mut last;
        let mut limit: Option<LimitKind> = None;
        let mut failure: Option<String> = None;
        let mut interrupted = false;

        loop {
            let outcome = self.run_turn()?;
            match outcome.state {
                TurnState::Completed => {
                    last = Some(outcome);
                    // A finished turn is not necessarily a finished session:
                    // an input acknowledged while that turn was running
                    // reached no delivery boundary inside it, and the next
                    // boundary is exactly here. Running another turn is what
                    // makes `max_turns` a bound on something real.
                    if self.queued_input()?.is_empty() {
                        break;
                    }
                }
                TurnState::Interrupted => {
                    interrupted = true;
                    last = Some(outcome);
                    break;
                }
                TurnState::Failed => {
                    limit = outcome.limit;
                    failure = outcome.failure.clone();
                    last = Some(outcome);
                    break;
                }
                _ => {
                    last = Some(outcome);
                    break;
                }
            }
        }

        self.finalize(last, limit, failure, interrupted)
    }

    /// Builds the structured final status from durable facts.
    pub fn finalize(
        &mut self,
        last: Option<TurnOutcome>,
        limit: Option<LimitKind>,
        failure: Option<String>,
        interrupted: bool,
    ) -> CtxResult<NativeFinalStatus> {
        let state = self.journal.replay(&self.config.session)?;
        let latest = latest_executions(&state);

        let mut incomplete = BTreeSet::new();
        let mut unknown = BTreeSet::new();
        for (call, execution) in &latest {
            match &execution.state {
                ExecutionState::OutcomeUnknown => {
                    unknown.insert(call.to_string());
                }
                other if !other.is_terminal() => {
                    incomplete.insert(call.to_string());
                }
                _ => {}
            }
        }

        let queued: Vec<String> = self
            .queued_input()?
            .into_iter()
            .map(|id| id.to_string())
            .collect();

        let finish = last.as_ref().and_then(|t| t.finish_reason.clone());
        let model_says_done = matches!(
            finish,
            Some(FinishReason::EndTurn) | Some(FinishReason::StopSequence)
        );

        // A model finish token is one input. The shared stop service is what
        // actually decides, and its `Block` outranks the token outright.
        //
        // Issue #484 (roadmap N15): the gate is read HERE, at the completion
        // attempt, not snapshotted at session start -- a session that reached
        // the Test step after it began is gated on the evidence that exists
        // now. An explicit `workflow_gate` still wins, so a caller that has
        // already decided (and every test) keeps a fixed answer.
        let workflow_gate = self.config.workflow_gate.clone().or_else(|| {
            let repo = self.config.workflow_repo.as_deref()?;
            let state = super::super::state::StateDir::resolve(self.env).ok()?;
            crate::commands::workflow::engine::native_completion_gate(&state, repo)
        });
        let stop = lifecycle::stop(&lifecycle::StopSignals {
            already_blocked: false,
            incomplete_tools: incomplete.iter().cloned().collect(),
            verification: lifecycle::VerificationDecision::NotRequired,
            workflow_gate,
        });
        let blocked_reason = match &stop {
            lifecycle::StopDecision::Block(reason) => Some(reason.clone()),
            _ => None,
        };

        let status = if failure.is_some() {
            NativeStatus::Failed
        } else if interrupted {
            NativeStatus::Interrupted
        } else if limit.is_some() {
            NativeStatus::LimitReached
        } else if blocked_reason.is_some()
            || !incomplete.is_empty()
            || !unknown.is_empty()
            || !queued.is_empty()
            || !model_says_done
        {
            NativeStatus::Incomplete
        } else {
            NativeStatus::Completed
        };

        if status != NativeStatus::Completed {
            self.note(
                "status",
                status.as_str(),
                "model finish token did not decide",
            );
        }

        Ok(NativeFinalStatus {
            schema_version: FINAL_STATUS_SCHEMA_VERSION,
            runtime: RuntimeKind::Native.as_str(),
            status,
            session: self.config.session.to_string(),
            route: self.config.route.route.to_string(),
            provider: self.config.route.provider.to_string(),
            endpoint: self.config.route.endpoint.to_string(),
            account: self.config.route.account.to_string(),
            billing_pool: self.config.route.billing_pool.to_string(),
            configured_model: self.config.route.model.id.clone(),
            served_model: self.served_model.clone(),
            turns: self.turns,
            requests: self.requests,
            tool_calls: self.tool_calls,
            usage: self.usage.clone(),
            reconciliation: self.reconciliation.clone(),
            finish_reason: finish.map(|reason| format!("{reason:?}")),
            final_text: last.as_ref().and_then(|t| t.final_text.clone()),
            incomplete_tools: incomplete.into_iter().collect(),
            outcome_unknown_tools: unknown.into_iter().collect(),
            queued_input: queued,
            limit,
            failure,
            blocked_reason,
            compactions: self.compactions.clone(),
            compaction_decision: self.last_decision.clone(),
            evidence: self.evidence.clone(),
            exit_code: status.exit_code(),
        })
    }
}

#[derive(Clone, Debug)]
struct PreparedCall {
    call: NativeToolCall,
    execution: ExecutionId,
    independent: bool,
    retry: RetryPolicy,
    denied: Option<String>,
    advice: Option<String>,
}

struct CollectingSink<'a>(&'a mut Vec<ProviderStreamEvent>);

impl EventSink for CollectingSink<'_> {
    fn push(&mut self, event: ProviderStreamEvent) {
        self.0.push(event);
    }
}

fn accumulate(total: &mut ProviderUsage, delta: &ProviderUsage) {
    total.input_tokens = total.input_tokens.saturating_add(delta.input_tokens);
    total.cache_creation_input_tokens = total
        .cache_creation_input_tokens
        .saturating_add(delta.cache_creation_input_tokens);
    total.cache_read_input_tokens = total
        .cache_read_input_tokens
        .saturating_add(delta.cache_read_input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(delta.output_tokens);
    if let Some(reasoning) = delta.reasoning_tokens {
        total.reasoning_tokens = Some(total.reasoning_tokens.unwrap_or(0) + reasoning);
    }
}

/// Wall-clock milliseconds. Only the entry points that have no injected clock
/// of their own use it -- [`NativeLoop`] takes one, so every loop-correctness
/// test stays deterministic.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

/// The ONE durable acknowledgement every entry point uses: [`NativeLoop::
/// acknowledge`] for a loop the caller drives itself, and
/// [`NativeBackend`]'s `submit`/`steer`/`resume` for a caller coming through
/// the runtime trait. It is written before the input can be acted on or even
/// reported as accepted, so nothing between "accepted" and "recorded" can
/// lose it -- a crash immediately after either call returns `Ok` still finds
/// the input in the journal.
pub fn acknowledge_input(
    journal: &mut Journal,
    session: &JournalSessionId,
    generation: u64,
    message_id: MessageId,
    text: &str,
    steering: bool,
    at_ms: u64,
) -> CtxResult<()> {
    journal.acknowledge_input(
        session,
        generation,
        &EventScope::default(),
        message_id,
        text.to_string(),
        steering,
        Some(at_ms),
        at_ms / 1000,
    )?;
    Ok(())
}

/// What [`NativeBackend::accept_input`] recorded: the durable identity the
/// input now has, and whether it was already there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedInput {
    pub message_id: MessageId,
    pub duplicate: bool,
}

/// The journal identity one caller-chosen idempotency key maps to.
///
/// Hashed rather than used verbatim: a key is caller text, and a `MessageId`
/// is bounded, NUL-free and compared for equality. A cryptographic digest
/// keeps distinct keys distinct -- a cheap hash's collision would silently
/// drop a genuinely different input as a duplicate, which is the one failure
/// mode this whole mechanism exists to prevent.
pub fn idempotent_message_id(key: &str) -> CtxResult<MessageId> {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(key.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(MessageId::new(format!("idem-{hex}"))?)
}

/// What a resume owed the journal before the session may run again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResumeOutcome {
    pub identity: super::journal::SessionIdentity,
    pub previous_generation: u64,
    pub generation: u64,
    /// Executions that were durably `Started` when the previous generation
    /// stopped. Each is now `OutcomeUnknown` and must be reconciled against
    /// the real world before anything retries its effect.
    pub reconciled: Vec<ExecutionId>,
}

/// Brings a stored native session back under a live runtime.
///
/// This is the only correct order, and the reason `Journal::
/// reconcile_started_as_unknown` and `Journal::advance_generation` exist:
///
/// 1. Read the stored identity, which fails loudly for a session this
///    journal has never heard of rather than inventing one.
/// 2. Convert every execution whose last durable state is `Started` to
///    `OutcomeUnknown`. An effect that began and never reported cannot be
///    assumed to have failed, so it is never silently retried; it is marked
///    for reconciliation instead.
/// 3. Advance the generation. That fences the previous one: any straggler
///    still holding the old generation -- a half-dead process, a stale
///    handle -- is refused by the journal and by the N04 broker from here on.
///
/// Reconciliation happens BEFORE the fence on purpose: it is written as the
/// old generation, which is the generation those executions actually belong
/// to, so the record reads as one continuous history rather than as the new
/// generation having somehow started effects it never issued.
pub fn resume_journal(
    journal: &mut Journal,
    session: &JournalSessionId,
    at_ms: u64,
) -> CtxResult<ResumeOutcome> {
    let now = at_ms / 1000;
    let identity = journal.session(session)?;
    let previous_generation = identity.generation;
    let reconciled =
        journal.reconcile_started_as_unknown(session, previous_generation, Some(at_ms), now)?;
    let generation = previous_generation.saturating_add(1);
    journal.advance_generation(session, previous_generation, generation, now)?;
    Ok(ResumeOutcome {
        identity,
        previous_generation,
        generation,
        reconciled,
    })
}

/// The authoritative execution for each tool call: the one with the highest
/// journal sequence.
///
/// A retried effect mints a NEW execution id (the journal's execution state
/// machine is terminal at `Failed`, and reusing the id would both be rejected
/// and hide that a second effect happened), so one tool call can own several
/// execution records. `ConversationState::executions` is keyed by
/// `ExecutionId` and therefore iterates in ID order, not in time order --
/// taking the first match would hand a follow-up request the stale failed
/// attempt that a successful retry already superseded. Both the request
/// builder and the final status read this one helper so they can never
/// disagree about what a tool call actually did.
fn latest_executions(state: &ConversationState) -> BTreeMap<ToolCallId, &ExecutionRecord> {
    let mut latest: BTreeMap<ToolCallId, &ExecutionRecord> = BTreeMap::new();
    for execution in state.executions.values() {
        match latest.entry(execution.tool_call.clone()) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(execution);
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                if execution.sequence >= slot.get().sequence {
                    slot.insert(execution);
                }
            }
        }
    }
    latest
}

/// The bounded head/tail extract a model reads in place of an offloaded tool
/// result, naming the artifact the whole text is retrievable from. Split on
/// character boundaries, so this can never hand back invalid UTF-8.
fn bounded_extract(content: &str, limit: usize, retrieval_id: &str) -> String {
    let half = (limit / 2).max(1);
    let head: String = content.chars().take(half).collect();
    let tail: String = {
        let chars: Vec<char> = content.chars().collect();
        chars[chars.len().saturating_sub(half)..].iter().collect()
    };
    format!(
        "{head}\n[... {} bytes elided; the whole result is stored as artifact {retrieval_id} ...]\n{tail}",
        content.len()
    )
}

fn content_text(reference: &ContentRef) -> String {
    match reference {
        ContentRef::Inline { text } => text.clone(),
        ContentRef::Artifact { sha256, .. } => format!("[artifact {sha256}]"),
    }
}

/// The assistant text of one response. A refusal is deliberately included:
/// it is the answer the session got, and hiding it would make an explicitly
/// refused turn look like an empty one.
fn collect_text(content: &[ProviderContent]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ProviderContent::Text { text } => Some(text.as_str()),
            ProviderContent::Refusal { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// The complete tool calls in one response, in the provider's declared order.
/// A call whose id the journal would reject is dropped rather than executed:
/// an unusable id cannot be recorded, and an unrecorded call must not run.
fn tool_uses(content: &[ProviderContent]) -> Vec<NativeToolCall> {
    content
        .iter()
        .filter_map(|block| match block {
            ProviderContent::ToolUse { id, name, input } => {
                let id = ToolCallId::new(id.clone()).ok()?;
                // The journal requires a JSON object; a truncated or
                // non-object argument stream is not a complete tool call.
                if !input.is_object() {
                    return None;
                }
                Some(NativeToolCall {
                    id,
                    name: name.clone(),
                    arguments: input.clone(),
                })
            }
            _ => None,
        })
        .collect()
}

/// The repository path a native write tool targets, for the shared
/// before-tool service. Read-only tools name no write target.
fn write_target(name: &str, arguments: &serde_json::Value) -> Option<std::path::PathBuf> {
    if !matches!(name, super::tools::FILE_WRITE | super::tools::APPLY_PATCH) {
        return None;
    }
    arguments
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(std::path::PathBuf::from)
}

/// Projects a tool receipt onto the loop's own state vocabulary plus the text
/// the model is given back.
fn classify(receipt: &ToolReceipt) -> (ToolState, String, bool) {
    match receipt.state {
        ToolReceiptState::Completed => (
            ToolState::Completed,
            receipt
                .result
                .as_ref()
                .map(|value| value.to_string())
                .unwrap_or_default(),
            false,
        ),
        ToolReceiptState::Failed => (
            ToolState::Failed,
            receipt
                .error
                .as_ref()
                .map(|error| error.message.clone())
                .unwrap_or_else(|| "tool failed".to_string()),
            true,
        ),
        ToolReceiptState::OutcomeUnknown => (
            ToolState::OutcomeUnknown,
            receipt
                .error
                .as_ref()
                .map(|error| error.message.clone())
                .unwrap_or_else(|| "tool outcome unknown".to_string()),
            true,
        ),
    }
}

// -- the RuntimeBackend ---------------------------------------------------

/// The `RuntimeKind::Native` [`RuntimeBackend`].
///
/// The backend owns session identity, the durable acknowledgement of input
/// and the cancellation flag; [`NativeLoop`] owns the conversation.
///
/// A backend with a journal attached ([`NativeBackend::attach_journal`] plus
/// [`NativeBackend::bind_session`]) writes every accepted input through the
/// same [`acknowledge_input`] the loop uses, and resumes through the same
/// [`resume_journal`]. Without one it is a pure in-memory protocol surface,
/// which is all a wire-shape test needs and all `runtime::select` can build
/// without knowing a state directory.
#[derive(Debug)]
pub struct NativeBackend {
    sessions: BTreeMap<String, NativeSessionRecord>,
    journal: Option<Journal>,
    minted: u64,
}

#[derive(Debug)]
struct NativeSessionRecord {
    short: String,
    generation: u64,
    role: String,
    surface: UiSurface,
    state: SessionState,
    cancel: Arc<CancellationFlag>,
    events: Vec<super::protocol::EventEnvelope>,
    /// The journal session this handle's inputs are recorded against, once a
    /// caller has bound one. `None` for an unbacked in-memory session.
    journal_session: Option<JournalSessionId>,
}

impl NativeBackend {
    pub fn new() -> Self {
        Self {
            sessions: BTreeMap::new(),
            journal: None,
            minted: 0,
        }
    }

    /// Gives this backend the journal every accepted input is recorded in.
    pub fn attach_journal(&mut self, journal: Journal) {
        self.journal = Some(journal);
    }

    /// Registers an EXISTING handle under this backend and binds it to the
    /// journal session its conversation lives in.
    ///
    /// Idempotent, and the only way a session this process did not itself
    /// `start` -- one recovered by [`resume_journal`] after a crash, or one a
    /// persistent runtime is re-attaching to -- becomes drivable. Until a
    /// handle is adopted its inputs are accepted but tracked only in memory,
    /// which is why `run_headless` adopts before it accepts anything.
    pub fn adopt(
        &mut self,
        session: &SessionHandle,
        journal_session: JournalSessionId,
    ) -> CtxResult<()> {
        match self.sessions.get_mut(&session.logical_id) {
            Some(entry) => {
                entry.generation = session.generation;
                entry.journal_session = Some(journal_session);
            }
            None => {
                self.sessions.insert(
                    session.logical_id.clone(),
                    NativeSessionRecord {
                        short: session.short.clone(),
                        generation: session.generation,
                        role: session.role.clone(),
                        surface: session.surface,
                        state: SessionState::Idle,
                        cancel: Arc::new(CancellationFlag::default()),
                        events: Vec::new(),
                        journal_session: Some(journal_session),
                    },
                );
            }
        }
        Ok(())
    }

    /// The journal, for a caller that drives the loop itself against the same
    /// database this backend acknowledges input into.
    pub fn journal_mut(&mut self) -> Option<&mut Journal> {
        self.journal.as_mut()
    }

    /// The journal for a read-only caller -- history and cursor pages
    /// (issue #489) need no write access, and asking for `&mut` to run a
    /// `SELECT` would force every reader to take the writer's place in the
    /// queue.
    pub fn journal(&self) -> Option<&Journal> {
        self.journal.as_ref()
    }

    fn mint_message_id(&mut self) -> CtxResult<MessageId> {
        self.minted += 1;
        Ok(MessageId::new(format!(
            "input-{}-{}",
            uuid::Uuid::new_v4().simple(),
            self.minted
        ))?)
    }

    /// Records one accepted input durably, BEFORE the caller is told it was
    /// accepted. A session with no journal bound is a no-op, not an error:
    /// the in-memory protocol surface has no durable store to lose it from.
    fn record_input(
        &mut self,
        session: &SessionHandle,
        input: &str,
        steering: bool,
    ) -> CtxResult<()> {
        let Some(entry) = self.sessions.get(&session.logical_id) else {
            return Err(RuntimeError::UnknownSession(session.logical_id.clone()).into());
        };
        let Some(journal_session) = entry.journal_session.clone() else {
            return Ok(());
        };
        let generation = entry.generation;
        if self.journal.is_none() {
            return Ok(());
        }
        let message_id = self.mint_message_id()?;
        let at_ms = now_ms();
        let journal = self.journal.as_mut().expect("checked just above");
        acknowledge_input(
            journal,
            &journal_session,
            generation,
            message_id,
            input,
            steering,
            at_ms,
        )
    }

    /// Issue #489: the durable acknowledgement a PROTOCOL caller's input goes
    /// through, carrying that caller's own idempotency key.
    ///
    /// The key becomes the journal's own `MessageId`, so the deduplication is
    /// a uniqueness constraint on disk rather than a cache in a process's
    /// memory. A retry after a reconnect -- or after the service itself
    /// restarted, which loses every in-memory idempotency cache there is --
    /// hits that constraint, records nothing a second time, and is reported
    /// back as a duplicate so the caller knows no second turn was queued.
    ///
    /// Without a key the id is minted fresh, exactly as `submit`/`steer` do:
    /// a caller that did not ask for deduplication does not get it silently.
    ///
    /// Unlike [`RuntimeBackend::submit`] this does NOT refuse a session with a
    /// turn in flight. A hosted conversation queues input the way the agent
    /// loop is built to take it -- `run_to_completion` drains everything
    /// unconsumed at the next delivery boundary -- so refusing here would
    /// reject a message the loop was about to deliver anyway. Whether a turn
    /// is running is the HOST's fact, not this table's; `session::native`
    /// owns it, because the host is what spawned the runner.
    pub fn accept_input(
        &mut self,
        session: &SessionHandle,
        input: &str,
        steering: bool,
        key: Option<&str>,
    ) -> CtxResult<AcceptedInput> {
        let logical_id = session.logical_id.clone();
        // Fences on generation before anything is written.
        self.resolve_current_mut(session)?;
        let Some(entry) = self.sessions.get(&logical_id) else {
            return Err(RuntimeError::UnknownSession(logical_id).into());
        };
        let journal_session = entry.journal_session.clone();
        let generation = entry.generation;
        let message_id = match key {
            Some(key) => idempotent_message_id(key)?,
            None => self.mint_message_id()?,
        };
        let (Some(journal_session), Some(journal)) = (journal_session, self.journal.as_mut())
        else {
            // No durable store bound: the in-memory protocol surface has
            // nothing to deduplicate against, and says so by reporting the id
            // it would have used rather than pretending to a guarantee.
            return Ok(AcceptedInput {
                message_id,
                duplicate: false,
            });
        };
        let at_ms = now_ms();
        match journal.acknowledge_input(
            &journal_session,
            generation,
            &EventScope::default(),
            message_id.clone(),
            input.to_string(),
            steering,
            Some(at_ms),
            at_ms / 1000,
        ) {
            Ok(_) => Ok(AcceptedInput {
                message_id,
                duplicate: false,
            }),
            // The one error that is not a failure: this exact input is already
            // on disk under this exact identity, so the first attempt won and
            // nothing else may happen.
            Err(super::journal::JournalError::DuplicateId { .. }) => Ok(AcceptedInput {
                message_id,
                duplicate: true,
            }),
            Err(error) => Err(error.into()),
        }
    }

    fn resolve_current_mut(
        &mut self,
        session: &SessionHandle,
    ) -> CtxResult<&mut NativeSessionRecord> {
        let Some(entry) = self.sessions.get_mut(&session.logical_id) else {
            return Err(RuntimeError::UnknownSession(session.logical_id.clone()).into());
        };
        if session.generation < entry.generation {
            return Err(RuntimeError::StaleGeneration {
                expected: entry.generation,
                got: session.generation,
            }
            .into());
        }
        Ok(entry)
    }

    /// The cancellation flag for a live session, so a caller that drives a
    /// [`NativeLoop`] itself shares the one `interrupt` sets.
    pub fn cancellation(&self, session: &SessionHandle) -> Option<Arc<CancellationFlag>> {
        self.sessions
            .get(&session.logical_id)
            .map(|entry| Arc::clone(&entry.cancel))
    }

    /// The session-level state, for a caller that needs to know whether a
    /// turn is in flight before issuing one.
    pub fn state(&self, session: &SessionHandle) -> Option<SessionState> {
        self.sessions
            .get(&session.logical_id)
            .map(|entry| entry.state)
    }
}

impl Default for NativeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeSessionRecord {
    fn push(&mut self, logical_id: &str, event: super::protocol::RuntimeEvent) {
        let revision = self.events.len() as u64 + 1;
        self.events.push(super::protocol::EventEnvelope {
            version: super::protocol::PROTOCOL_VERSION,
            revision,
            session: logical_id.to_string(),
            generation: self.generation,
            event,
        });
    }
}

impl RuntimeBackend for NativeBackend {
    fn kind(&self) -> RuntimeKind {
        RuntimeKind::Native
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            steer: true,
            interrupt: true,
            resume: true,
            events: true,
            surfaces: vec![
                UiSurface::Headless,
                UiSurface::Terminal,
                UiSurface::DashboardPane,
            ],
        }
    }

    fn start(&mut self, spec: &SessionSpec) -> CtxResult<SessionHandle> {
        if spec.runtime != RuntimeKind::Native {
            return Err(RuntimeError::Unsupported(format!(
                "native backend cannot start a `{}` session",
                spec.runtime
            ))
            .into());
        }
        let logical_id = uuid::Uuid::new_v4().to_string();
        let short: String = logical_id.chars().take(8).collect();
        let handle = SessionHandle {
            runtime: RuntimeKind::Native,
            logical_id: logical_id.clone(),
            short: short.clone(),
            generation: 1,
            role: spec.role.clone(),
            surface: spec.surface,
            conversation: Some(BackendConversationRef {
                agent: RuntimeKind::Native.as_str().to_string(),
                conversation: logical_id.clone(),
            }),
        };
        let mut record = NativeSessionRecord {
            short,
            generation: 1,
            role: spec.role.clone(),
            surface: spec.surface,
            state: SessionState::Idle,
            cancel: Arc::new(CancellationFlag::default()),
            events: Vec::new(),
            journal_session: None,
        };
        record.push(
            &logical_id,
            super::protocol::RuntimeEvent::Started {
                session: handle.clone(),
            },
        );
        self.sessions.insert(logical_id, record);
        Ok(handle)
    }

    /// A submit is only ever accepted by an idle session. Driving the turn is
    /// the caller's own call into [`NativeLoop::run_to_completion`], because
    /// the loop needs a journal and a provider this trait deliberately does
    /// not carry -- see `run_headless`.
    fn submit(&mut self, session: &SessionHandle, input: &str) -> CtxResult<()> {
        let logical_id = session.logical_id.clone();
        {
            let entry = self.resolve_current_mut(session)?;
            if entry.state == SessionState::Running {
                return Err(RuntimeError::Busy(logical_id).into());
            }
        }
        // Durable FIRST, in-memory bookkeeping after: a crash between the two
        // costs a protocol event a subscriber can re-derive, never the input
        // itself. The reverse order would let a caller see `Ok` for an input
        // that exists nowhere but this process's memory.
        self.record_input(session, input, false)?;
        let entry = self.resolve_current_mut(session)?;
        entry.state = SessionState::Running;
        entry.push(&logical_id, super::protocol::RuntimeEvent::TurnStarted);
        entry.push(
            &logical_id,
            super::protocol::RuntimeEvent::AssistantText {
                text: format!("queued: {input}"),
            },
        );
        Ok(())
    }

    /// Steering is accepted at ANY time, including mid-turn: that is the
    /// point. It joins the conversation at the next delivery boundary, and is
    /// durable from the moment it is accepted.
    fn steer(&mut self, session: &SessionHandle, input: &str) -> CtxResult<()> {
        let logical_id = session.logical_id.clone();
        // Fences on generation before anything is written.
        self.resolve_current_mut(session)?;
        self.record_input(session, input, true)?;
        let entry = self.resolve_current_mut(session)?;
        entry.push(
            &logical_id,
            super::protocol::RuntimeEvent::AssistantText {
                text: format!("steering queued: {input}"),
            },
        );
        Ok(())
    }

    fn interrupt(&mut self, session: &SessionHandle) -> CtxResult<()> {
        let logical_id = session.logical_id.clone();
        let entry = self.resolve_current_mut(session)?;
        entry.cancel.cancel();
        entry.state = SessionState::Interrupted;
        entry.push(&logical_id, super::protocol::RuntimeEvent::Interrupted);
        Ok(())
    }

    /// Brings a session back under this runtime.
    ///
    /// With a journal bound this is the real thing: every execution that was
    /// durably `Started` when the previous generation stopped becomes
    /// `OutcomeUnknown` (never silently retried), and the generation advances,
    /// fencing the old one out of the journal and out of the N04 broker. See
    /// [`resume_journal`]. Without a journal it is the in-memory generation
    /// bump a protocol-shape test needs.
    fn resume(&mut self, session: &SessionHandle, input: Option<&str>) -> CtxResult<SessionHandle> {
        let logical_id = session.logical_id.clone();
        let journal_session = self
            .sessions
            .get(&logical_id)
            .ok_or_else(|| RuntimeError::UnknownSession(logical_id.clone()))?
            .journal_session
            .clone();
        let resumed = match (journal_session.as_ref(), self.journal.as_mut()) {
            (Some(journal_session), Some(journal)) => {
                Some(resume_journal(journal, journal_session, now_ms())?)
            }
            _ => None,
        };

        let Some(entry) = self.sessions.get_mut(&logical_id) else {
            return Err(RuntimeError::UnknownSession(logical_id).into());
        };
        entry.generation = match &resumed {
            Some(resumed) => resumed.generation,
            None => entry.generation + 1,
        };
        // A resume is a fresh cancellation scope: the flag an earlier
        // interrupt set must not silently cancel the resumed turn.
        entry.cancel = Arc::new(CancellationFlag::default());
        entry.state = SessionState::Idle;
        let handle = SessionHandle {
            runtime: RuntimeKind::Native,
            logical_id: logical_id.clone(),
            short: entry.short.clone(),
            generation: entry.generation,
            role: entry.role.clone(),
            surface: entry.surface,
            conversation: Some(BackendConversationRef {
                agent: RuntimeKind::Native.as_str().to_string(),
                conversation: logical_id.clone(),
            }),
        };
        entry.push(
            &logical_id,
            super::protocol::RuntimeEvent::Started {
                session: handle.clone(),
            },
        );
        // The input a resume carries is acknowledged against the NEW
        // generation, durably, exactly like any other accepted input.
        if let Some(input) = input {
            self.record_input(&handle, input, false)?;
            let entry = self.resolve_current_mut(&handle)?;
            entry.push(
                &logical_id,
                super::protocol::RuntimeEvent::AssistantText {
                    text: format!("queued: {input}"),
                },
            );
        }
        Ok(handle)
    }

    fn subscribe(
        &mut self,
        session: &SessionHandle,
        after_revision: u64,
    ) -> CtxResult<Vec<super::protocol::EventEnvelope>> {
        let Some(entry) = self.sessions.get(&session.logical_id) else {
            return Err(RuntimeError::UnknownSession(session.logical_id.clone()).into());
        };
        Ok(entry
            .events
            .iter()
            .filter(|event| event.revision > after_revision)
            .cloned()
            .collect())
    }
}

// -- headless execution ---------------------------------------------------

/// The `--provider` prefix that swaps the live transport for a deterministic
/// fixture script. Operator-only by construction: it is a command-line flag,
/// and no configuration layer -- least of all a repository's -- can set it.
pub const FIXTURE_PROVIDER_PREFIX: &str = "fixture:";

/// Everything `zirv ctx exec --runtime native -- <prompt>` needs.
///
/// `route` is a `[route]` name from the operator's own native provider
/// configuration; omitting it uses the `[roles]` entry for `role`. There is no
/// adapter, no agent binary and no PATH probe anywhere on this path.
#[derive(Debug)]
pub struct HeadlessRequest<'a> {
    pub repo: &'a std::path::Path,
    pub prompt: &'a str,
    pub route: Option<&'a str>,
    pub role: &'a str,
    pub limits: NativeLimits,
    /// An existing native journal session to continue instead of starting a
    /// new one. See [`resume_journal`] for what a resume owes first.
    pub resume: Option<&'a str>,
    /// Operator-only transport override. The only accepted shape today is
    /// `fixture:<path>`, which replays the deterministic provider script at
    /// that path instead of calling a provider.
    pub provider: Option<&'a str>,
    /// The fixture tool script a `fixture:` provider executes against. With
    /// none, every tool call reports a fixture failure rather than touching
    /// the machine.
    pub fixture_tools: Option<&'a std::path::Path>,
    /// Issue #479 (roadmap N10): the SHARED task-card id this session is
    /// working (`zirv ctx task`), carried onto the journal's own session
    /// identity, onto the loop's config and onto every tool call's execution
    /// identity, so a native worker's durable record names the same task a
    /// legacy worker's does. `None` for a plain `zirv ctx exec --runtime
    /// native` with no card behind it.
    pub task: Option<String>,
    /// Issue #479: the live writer permit this session's repository writes are
    /// backed by (`permit::acquire_writer`). Moved into the execution broker
    /// ([`brokered_tools`]), which refuses every repository write that is not
    /// covered by a permit for this exact worktree -- so a native worker and a
    /// legacy worker can never both hold one checkout. `None` is a session
    /// nobody granted a tree to: its file writes are refused, which is the
    /// honest answer rather than an unbacked write.
    pub writer: Option<Box<dyn super::enforcement::WriterLease>>,
}

/// The route this request will spend, and the PROVIDER whose reservation
/// ledger it spends against -- resolved from operator configuration alone.
///
/// Issue #479 (roadmap N10): a delegated native worker has to reserve its
/// token ceiling against the same per-provider ledger a legacy delegation
/// reserves against (`ctx::reservation`), and that reservation is taken
/// BEFORE the run, so it cannot wait for the route resolution
/// [`build_transport`] performs. This deliberately touches no credential
/// store and no network: it reads `[route]`/`[account]` and answers, so a
/// missing or expired credential fails where it should -- at the actual
/// request -- and not at accounting time.
pub fn route_provider(
    repo: &std::path::Path,
    route: Option<&str>,
    role: &str,
    env: EnvLookup<'_>,
) -> CtxResult<(super::super::provider::RouteId, String)> {
    use super::super::provider::RouteId;
    use super::super::provider::config::NativeConfig;

    let home = crate::utils::home_dir()?;
    let native = NativeConfig::load(&home, repo)?.ok_or_else(|| {
        format!(
            "native runtime: no provider configuration at {}. Run `zirv ctx provider` to set up \
             an account, endpoint and route first.",
            NativeConfig::operator_path(&home).display()
        )
    })?;
    let _ = env;
    let route_id = match route {
        Some(name) => RouteId::new(name)?,
        None => native.roles.get(role).cloned().ok_or_else(|| {
            format!(
                "native runtime: no route for role `{role}`; pass --route or add a [roles] entry"
            )
        })?,
    };
    let provider = native
        .routes
        .get(&route_id)
        .and_then(|route| native.accounts.get(&route.account))
        .map(|account| account.provider.to_string())
        .ok_or_else(|| format!("native runtime: route `{route_id}` names no configured account"))?;
    Ok((route_id, provider))
}

/// The durable route identity a new native conversation is filed under
/// (issue #489).
///
/// The persistent runtime has to create the journal session when the client
/// asks for it, and a journal session carries the route it will spend. This
/// resolves that route through the SAME `resolve_target` the transport uses,
/// so the identity written at creation is the identity the first request
/// spends -- rather than a second, hopeful derivation that could disagree with
/// it. A route the operator has not configured fails here, loudly, instead of
/// producing a conversation pinned to a route that does not exist.
pub fn journal_route_identity(
    repo: &std::path::Path,
    route: Option<&str>,
    role: &str,
    env: EnvLookup<'_>,
) -> CtxResult<RouteIdentity> {
    use super::super::provider::config::NativeConfig;
    use super::super::provider::credential::OsStore;
    use super::super::provider::{RouteId, adapter::resolve_target};
    use super::super::state::now_secs;

    let home = crate::utils::home_dir()?;
    let native = NativeConfig::load(&home, repo)?.ok_or_else(|| {
        format!(
            "native runtime: no provider configuration at {}. Run `zirv ctx provider` to set up \
             an account, endpoint and route first.",
            NativeConfig::operator_path(&home).display()
        )
    })?;
    let route_id = match route {
        Some(name) => RouteId::new(name)?,
        None => native.roles.get(role).cloned().ok_or_else(|| {
            format!("native runtime: no route for role `{role}`; add a [roles] entry")
        })?,
    };
    let (target, _) = resolve_target(&native, &route_id, env, &OsStore::default(), now_secs())?;
    Ok(RouteIdentity {
        route: target.route.clone(),
        provider: target.provider.clone(),
        endpoint: target.endpoint.clone(),
        account: target.account.clone(),
        billing_pool: target.billing_pool.clone(),
        protocol: target.protocol,
        model: target.model.clone(),
    })
}

/// Runs one headless native session end to end and prints its structured
/// final status as JSON, returning the exit code a `zirv ctx exec` consumer
/// expects.
///
/// This is the native equivalent of `exec::run_with_clock_inner`'s harness
/// spawn: same command, same structured outcome, an entirely different
/// mechanism underneath. Everything it needs comes from operator
/// configuration and the state directory; nothing is inherited from a harness
/// process, because there is none.
pub fn run_headless<W: std::io::Write>(
    request: &mut HeadlessRequest<'_>,
    w: &mut W,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    // `run_session`'s own doc comment: `w` can carry one human line before
    // the status exists (a resume's outcome-unknown reconcile notice), which
    // a `--json` caller has to route somewhere other than its own
    // single-object stdout. This is that caller -- it always prints exactly
    // one JSON status object to `w` below, so the notice is captured here
    // and re-emitted on stderr instead, mirroring `native_worker::launch_
    // native`'s identical treatment of the same notice.
    let mut notices: Vec<u8> = Vec::new();
    let status = run_session(request, &mut notices, env)?;
    if !notices.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&notices));
    }
    writeln!(w, "{}", serde_json::to_string_pretty(&status)?)?;
    Ok(status.exit_code)
}

/// The session itself, without the final JSON print. Split out of
/// [`run_headless`] for issue #479 (roadmap N10): a delegated native worker
/// needs the structured status back as a VALUE -- to hold to a `--result-
/// schema` contract, to store as its result, to publish as its terminal
/// outcome and to fold into a delegation receipt -- not written to a stream.
///
/// `w` still carries the one human line a run can owe before its status
/// exists (a resume's outcome-unknown reconcile notice), which a `--json`
/// caller routes somewhere other than its own single-object stdout.
pub fn run_session<W: std::io::Write>(
    request: &mut HeadlessRequest<'_>,
    w: &mut W,
    env: EnvLookup<'_>,
) -> CtxResult<NativeFinalStatus> {
    use super::super::state::{StateDir, now_secs};
    use super::journal::{SeatId, SessionIdentity, TaskId};

    let state = StateDir::resolve(env)?;
    let home = crate::utils::home_dir()?;
    let cfg = super::super::config::CtxConfig::load(request.repo, env)?;
    let now = now_secs();
    // Issue #479: the shared task card, validated once here so a malformed id
    // fails before any seat, journal session or effect exists.
    let task = request.task.clone().map(TaskId::new).transpose()?;

    let (provider, mut tools, route, brokered) =
        build_transport(request, &state, &home, &cfg, env)?;

    let mut journal = Journal::open(&state)?;
    let mut backend = NativeBackend::new();

    // Session identity first: the seat record is what the effect-time
    // generation fence reads, so it has to exist before any tool can run.
    let (handle, session) = match request.resume {
        Some(resume) => {
            let session = JournalSessionId::new(resume)?;
            let resumed = resume_journal(&mut journal, &session, now_ms())?;
            let handle = SessionHandle {
                runtime: RuntimeKind::Native,
                logical_id: session.to_string(),
                short: resumed.identity.seat.to_string(),
                generation: resumed.generation,
                role: request.role.to_string(),
                surface: UiSurface::Headless,
                conversation: Some(BackendConversationRef {
                    agent: RuntimeKind::Native.as_str().to_string(),
                    conversation: session.to_string(),
                }),
            };
            if !resumed.reconciled.is_empty() {
                writeln!(
                    w,
                    "native runtime: {} execution(s) were in flight when this session stopped and \
                     are now outcome-unknown; reconcile before retrying their effects",
                    resumed.reconciled.len()
                )?;
            }
            (handle, session)
        }
        None => {
            let handle = backend.start(&SessionSpec {
                runtime: RuntimeKind::Native,
                role: request.role.to_string(),
                agent: None,
                provider_route: Some(route.route.clone()),
                model: Some(route.model.id.clone()),
                surface: UiSurface::Headless,
                cwd: request.repo.to_path_buf(),
                prompt: request.prompt.to_string(),
                extra_args: Vec::new(),
            })?;
            let session = JournalSessionId::new(handle.logical_id.clone())?;
            journal.create_session(&SessionIdentity {
                session: session.clone(),
                seat: SeatId::new(handle.short.clone())?,
                generation: handle.generation,
                // Issue #479: the SHARED card, so a native worker's journal
                // session and a legacy worker's task card name one task.
                task: task.clone(),
                route: route.clone(),
                created_at: now,
                completed_at: None,
            })?;
            (handle, session)
        }
    };

    super::super::seat::store(
        &state,
        &super::super::seat::Seat {
            short: handle.short.clone(),
            session: handle.logical_id.clone(),
            generation: handle.generation,
            agent: RuntimeKind::Native.as_str().to_string(),
            model: Some(route.model.id.clone()),
            provider: route.provider.to_string(),
            role: request.role.to_string(),
            pinned: false,
            phase: Default::default(),
            visited: Vec::new(),
            last_rollover_at: None,
            pending: None,
            displaced: None,
            created_at: now,
            updated_at: now,
            runtime: RuntimeKind::Native,
        },
    )?;

    if brokered {
        // The broker is built here, after the seat record exists, because its
        // own fence reads that record at every effect.
        let executor = brokered_tools(request, &state, &home, &cfg, &handle)?;
        tools = executor;
    }

    // Issue #484 (roadmap N15): the standing context, compiled ONCE by the
    // native context compiler. This is what makes methodology and workflow
    // adoption automatic -- a native session gets the engineering standard,
    // its role's methodology, the model profile, the operator's and
    // repository's instruction files and the active workflow's current step
    // without anyone hand-seeding a prompt. A compilation that fails degrades
    // to no standing context rather than failing the session: a session that
    // runs with less context is recoverable, one that will not start is not.
    let (system, preamble) = match compile_standing_context(
        &state, &home, &cfg, request, &route, &session, now,
    ) {
        Ok(compiled) => compiled,
        Err(error) => {
            writeln!(
                w,
                "native runtime: standing context could not be compiled ({error}); continuing with the conversation alone"
            )?;
            (Vec::new(), Vec::new())
        }
    };

    // The backend owns the durable acknowledgement, so the input is on disk
    // before anything is told it was accepted -- the same `acknowledge_input`
    // the loop's own `acknowledge` uses.
    backend.attach_journal(journal);
    backend.adopt(&handle, session.clone())?;
    if !request.prompt.is_empty() {
        backend.submit(&handle, request.prompt)?;
    }
    let cancel = backend
        .cancellation(&handle)
        .unwrap_or_else(|| std::sync::Arc::new(CancellationFlag::default()));

    // Issue #486: the compaction envelope for this run. The capacity is the
    // route model's DECLARED context window less the output reservation this
    // run actually asked for -- an unknown window stays `None`, which the rot
    // token gate reads as "use the absolute fallbacks", never as a guess. The
    // policy comes from `~/.zirv/native.toml`, already narrowed by any
    // repository layer.
    let compaction = CompactionSettings {
        enabled: true,
        policy: super::super::provider::config::NativeConfig::load(&home, request.repo)?
            .map(|native| native.compaction_policy())
            .unwrap_or_default(),
        budget: NativeBudget {
            context_window_tokens: super::super::provider::capability::declared(
                route.protocol,
                &route.model,
                None,
            )
            .context_window,
            output_reserve_tokens: request.limits.max_output_tokens,
        },
        score: cfg.score.clone(),
        distill: DistillBudget::default(),
        retain_recent_messages: RETAIN_RECENT_MESSAGES,
        constraints: Vec::new(),
        state: Some(state.clone()),
    };

    let status = {
        let journal = backend
            .journal_mut()
            .ok_or("native runtime: the journal was not attached")?;
        let mut driver = NativeLoop::new(
            NativeSessionConfig {
                session: session.clone(),
                generation: handle.generation,
                route: route.clone(),
                role: request.role.to_string(),
                seat_model: env(super::super::adapters::SEAT_MODEL_ENV),
                write_posture: lifecycle::orchestrator_write_posture(&cfg),
                limits: request.limits,
                // Issue #479: the shared card the loop's own tool-call scopes
                // and task receipts are filed under.
                task: task.clone(),
                workflow_gate: None,
                compaction,
                // Issue #484: the active workflow of the repository this
                // session is actually working, consulted live at every
                // completion attempt. A fixture run brokers nothing and
                // performs no effects, so gating it would only be theatre.
                workflow_repo: brokered.then(|| request.repo.to_path_buf()),
                system,
                preamble,
            },
            provider.as_ref(),
            tools.as_mut(),
            journal,
            cancel,
            &now_ms,
            env,
        );
        driver.run_to_completion()?
    };

    if let Some(journal) = backend.journal_mut() {
        journal.complete_session(
            &session,
            handle.generation,
            status.status.as_str().to_string(),
            now_secs(),
        )?;
    }
    Ok(status)
}

/// The standing instruction and data context one native session runs under
/// (issue #484, roadmap N15), split the way the provider request wants it:
/// instructions become the system prompt, data becomes a leading user message.
///
/// Everything here comes from `runtime::context::compile`, which is the only
/// place that decides what a native session is told and in what order. This
/// function's whole job is handing it the session's own identity and budget.
#[allow(clippy::type_complexity)]
fn compile_standing_context(
    state: &super::super::state::StateDir,
    home: &std::path::Path,
    cfg: &super::super::config::CtxConfig,
    request: &HeadlessRequest<'_>,
    route: &RouteIdentity,
    session: &JournalSessionId,
    now: u64,
) -> CtxResult<(Vec<String>, Vec<String>)> {
    use super::context::{CompileRequest, MessageRole, TokenBudget};

    let capabilities =
        super::super::provider::capability::declared(route.protocol, &route.model, None);
    // The window the route's own model declares, or a conservative floor when
    // the catalogue has nothing for it. Reserving the session's own output
    // ceiling is what keeps the compiled prefix from crowding out the answer.
    let context_window_tokens = capabilities.context_window.unwrap_or(128_000);
    let provider = route.provider.to_string();
    let session_id = session.to_string();
    let compiled = super::context::compile(&CompileRequest {
        home: Some(home),
        repo: request.repo,
        cwd: request.repo,
        state,
        config: cfg,
        role: prompt_role(request.role),
        session_id: &session_id,
        task: request.prompt,
        constraints: &[],
        pending_actions: &[],
        provider: &provider,
        model: &route.model.id,
        capabilities: &capabilities,
        budget: TokenBudget {
            context_window_tokens,
            output_reserve_tokens: request.limits.max_output_tokens,
            max_inline_evidence_bytes: cfg.output.max_summary_bytes,
        },
        evidence: &[],
        token_counter: None,
        now,
    })?;
    let mut system = Vec::new();
    let mut preamble = Vec::new();
    for message in &compiled.messages {
        match message.role {
            MessageRole::Instruction => system.push(message.content.clone()),
            MessageRole::Data => preamble.push(message.content.clone()),
        }
    }
    Ok((system, preamble))
}

/// The prompt role a native session's `--role` names. Unknown values are
/// workers: the least-privileged methodology is the safe default, and an
/// orchestrator layer handed to a worker would tell it to delegate work
/// nobody asked it to delegate.
fn prompt_role(role: &str) -> super::super::prompt::PromptRole {
    use super::super::prompt::PromptRole;

    match role {
        "orchestrator" => PromptRole::Orchestrator,
        "sub-orchestrator" => PromptRole::SubOrchestrator,
        _ => PromptRole::Worker,
    }
}

/// Everything the persistent runtime needs to run the turns already queued on
/// an EXISTING native conversation (issue #489, step N20).
///
/// The difference from [`HeadlessRequest`] is the whole point: a hosted turn
/// neither creates the journal session nor resumes it nor completes it. The
/// service created it when the client asked for the session, the generation is
/// the one the service is holding, and the conversation outlives this turn --
/// so advancing a generation here (what a resume does) would fence the service
/// out of its own session, and completing it here would end a conversation the
/// operator never asked to end.
#[derive(Debug)]
pub struct HostedTurn<'a> {
    pub repo: &'a std::path::Path,
    /// The journal session whose queued input this runs.
    pub session: &'a JournalSessionId,
    /// The seat short id, so the loop's identity matches the registry record
    /// the service already filed for this session.
    pub seat_short: &'a str,
    pub generation: u64,
    pub role: &'a str,
    pub route: Option<&'a str>,
    pub limits: NativeLimits,
    pub provider: Option<&'a str>,
    pub fixture_tools: Option<&'a std::path::Path>,
    pub task: Option<String>,
    /// The writer permit this session's repository writes are backed by, or
    /// `None` for a session nobody granted a tree to -- whose file writes are
    /// then refused, which is the honest answer rather than an unbacked write.
    pub writer: Option<Box<dyn super::enforcement::WriterLease>>,
    /// Shared with the host, so `session.interrupt` cancels the turn this
    /// call is running rather than the next one.
    pub cancel: Arc<CancellationFlag>,
}

/// Drives every turn already queued on a hosted native session to completion.
///
/// Returns when the conversation has no unconsumed input left, the turn was
/// interrupted, or a limit was hit -- i.e. when the session is idle again. The
/// session itself stays open: the caller (`session::native`) keeps its
/// journal, its registry record and its identity, and calls this again the
/// next time input arrives.
pub fn run_hosted_turns<W: std::io::Write>(
    turn: &mut HostedTurn<'_>,
    w: &mut W,
    env: EnvLookup<'_>,
) -> CtxResult<NativeFinalStatus> {
    use super::super::state::StateDir;

    let _ = w;
    let state = StateDir::resolve(env)?;
    let home = crate::utils::home_dir()?;
    let cfg = super::super::config::CtxConfig::load(turn.repo, env)?;
    let task = turn
        .task
        .clone()
        .map(super::journal::TaskId::new)
        .transpose()?;

    // The SAME transport, route resolution and broker assembly a headless run
    // uses. A second way to build either would be a second place for a native
    // launch to drift, which is exactly what issue #489 says not to do.
    let mut request = HeadlessRequest {
        repo: turn.repo,
        prompt: "",
        route: turn.route,
        role: turn.role,
        limits: turn.limits,
        resume: None,
        provider: turn.provider,
        fixture_tools: turn.fixture_tools,
        task: turn.task.clone(),
        writer: turn.writer.take(),
    };
    let (provider, mut tools, route, brokered) =
        build_transport(&request, &state, &home, &cfg, env)?;

    let handle = SessionHandle {
        runtime: RuntimeKind::Native,
        logical_id: turn.session.to_string(),
        short: turn.seat_short.to_string(),
        generation: turn.generation,
        role: turn.role.to_string(),
        surface: UiSurface::Headless,
        conversation: Some(BackendConversationRef {
            agent: RuntimeKind::Native.as_str().to_string(),
            conversation: turn.session.to_string(),
        }),
    };
    if brokered {
        tools = brokered_tools(&mut request, &state, &home, &cfg, &handle)?;
    }

    let compaction = CompactionSettings {
        enabled: true,
        policy: super::super::provider::config::NativeConfig::load(&home, turn.repo)?
            .map(|native| native.compaction_policy())
            .unwrap_or_default(),
        budget: NativeBudget {
            context_window_tokens: super::super::provider::capability::declared(
                route.protocol,
                &route.model,
                None,
            )
            .context_window,
            output_reserve_tokens: turn.limits.max_output_tokens,
        },
        score: cfg.score.clone(),
        distill: DistillBudget::default(),
        retain_recent_messages: RETAIN_RECENT_MESSAGES,
        constraints: Vec::new(),
        state: Some(state.clone()),
    };

    // Issue #484 (N15): the SAME standing context a headless run compiles --
    // the engineering standard, the role methodology, the model profile and
    // the operator's and repository's own instruction files. A hosted turn
    // that skipped it would be a session told less than every other one.
    let (system, preamble) = compile_standing_context(
        &state,
        &home,
        &cfg,
        &request,
        &route,
        turn.session,
        super::super::state::now_secs(),
    )?;
    let mut journal = Journal::open(&state)?;
    let mut driver = NativeLoop::new(
        NativeSessionConfig {
            session: turn.session.clone(),
            generation: turn.generation,
            route,
            role: turn.role.to_string(),
            seat_model: env(super::super::adapters::SEAT_MODEL_ENV),
            write_posture: lifecycle::orchestrator_write_posture(&cfg),
            limits: turn.limits,
            task,
            workflow_gate: None,
            compaction,
            // Issue #484: gated by the repository this session actually works
            // whenever it performs real effects, exactly as a headless run is.
            workflow_repo: brokered.then(|| turn.repo.to_path_buf()),
            system,
            preamble,
        },
        provider.as_ref(),
        tools.as_mut(),
        &mut journal,
        Arc::clone(&turn.cancel),
        &now_ms,
        env,
    );
    driver.run_to_completion()
}

/// Resolves the provider transport, the tool executor and the route identity
/// for one headless run. The fourth value says whether the returned executor
/// is a placeholder that must be replaced by a brokered one once the seat
/// record exists -- a fixture run never brokers, because it performs no
/// effects at all.
#[allow(clippy::type_complexity)]
fn build_transport(
    request: &HeadlessRequest<'_>,
    state: &super::super::state::StateDir,
    home: &std::path::Path,
    cfg: &super::super::config::CtxConfig,
    env: EnvLookup<'_>,
) -> CtxResult<(
    Box<dyn ProviderAdapter>,
    Box<dyn ToolExecutor>,
    RouteIdentity,
    bool,
)> {
    use std::time::Duration;

    use super::super::provider::anthropic::AnthropicMessagesAdapter;
    use super::super::provider::bedrock::BedrockAdapter;
    use super::super::provider::config::NativeConfig;
    use super::super::provider::credential::OsStore;
    use super::super::provider::google::GoogleAdapter;
    use super::super::provider::openai::OpenAiResponsesAdapter;
    use super::super::provider::openai_chat::OpenAiChatAdapter;
    use super::super::provider::transport::StreamTimeouts;
    use super::super::provider::{Protocol, RouteId, adapter::resolve_target};
    use super::super::state::now_secs;
    use super::fixture::{
        FixtureProvider, FixtureScript, FixtureToolExecutor, FixtureToolScript, fixture_target,
    };

    let _ = (state, cfg);

    if let Some(spec) = request.provider {
        let Some(path) = spec.strip_prefix(FIXTURE_PROVIDER_PREFIX) else {
            return Err(format!(
                "--provider '{spec}': the only supported value is \
                 `{FIXTURE_PROVIDER_PREFIX}<path to a provider script>`"
            )
            .into());
        };
        let script = FixtureScript::load(std::path::Path::new(path))?;
        let protocol = if script.shape == "openai" {
            Protocol::OpenAiResponses
        } else {
            Protocol::AnthropicMessages
        };
        let model = if script.model.is_empty() {
            "fixture-model".to_string()
        } else {
            script.model.clone()
        };
        let target = fixture_target(protocol, &model);
        let route = RouteIdentity {
            route: target.route.clone(),
            provider: target.provider.clone(),
            endpoint: target.endpoint.clone(),
            account: target.account.clone(),
            billing_pool: target.billing_pool.clone(),
            protocol: target.protocol,
            model: target.model.clone(),
        };
        let tool_script = match request.fixture_tools {
            Some(path) => FixtureToolScript::load(path)?,
            None => FixtureToolScript::default(),
        };
        return Ok((
            Box::new(FixtureProvider::new(target, script)),
            Box::new(FixtureToolExecutor::new(tool_script)),
            route,
            false,
        ));
    }

    let native = NativeConfig::load(home, request.repo)?.ok_or_else(|| {
        format!(
            "native runtime: no provider configuration at {}. Run `zirv ctx provider` to \
             set up an account, endpoint and route first.",
            NativeConfig::operator_path(home).display()
        )
    })?;
    let route_id = match request.route {
        Some(name) => RouteId::new(name)?,
        None => native.roles.get(request.role).cloned().ok_or_else(|| {
            format!(
                "native runtime: no route for role `{}`; pass --route or add a [roles] entry",
                request.role
            )
        })?,
    };

    let store = OsStore::default();
    let now = now_secs();
    let timeouts = StreamTimeouts {
        connect: Duration::from_secs(10),
        first_event: Duration::from_millis(request.limits.first_event_ms.max(1)),
        idle: Duration::from_millis(request.limits.idle_ms.max(1)),
    };
    let (target, _) = resolve_target(&native, &route_id, env, &store, now)?;
    let provider: Box<dyn ProviderAdapter> = match target.protocol {
        Protocol::AnthropicMessages => Box::new(AnthropicMessagesAdapter::from_config(
            &native, &route_id, env, &store, now, timeouts,
        )?),
        Protocol::OpenAiResponses => Box::new(OpenAiResponsesAdapter::from_config(
            &native, &route_id, env, &store, now, timeouts,
        )?),
        Protocol::GoogleGenerativeAi | Protocol::GoogleVertex => Box::new(
            GoogleAdapter::from_config(&native, &route_id, env, &store, now, timeouts)?,
        ),
        // One transport serves every chat-completions-compatible vendor,
        // every local runtime and Azure; the bound route profile is what
        // decides the address, the auth header and the caveats (N13).
        Protocol::OpenAiChatCompatible | Protocol::AzureOpenAiChat => Box::new(
            OpenAiChatAdapter::from_config(&native, &route_id, env, &store, now, timeouts)?,
        ),
        Protocol::AwsBedrock => Box::new(BedrockAdapter::from_config(
            &native, &route_id, env, &store, now, timeouts,
        )?),
    };
    let route = RouteIdentity {
        route: target.route.clone(),
        provider: target.provider.clone(),
        endpoint: target.endpoint.clone(),
        account: target.account.clone(),
        billing_pool: target.billing_pool.clone(),
        protocol: target.protocol,
        model: target.model.clone(),
    };
    // A placeholder: the real executor needs the seat record that only exists
    // once session identity is settled, so `run_headless` swaps it in there.
    Ok((
        provider,
        Box::new(FixtureToolExecutor::new(FixtureToolScript::default())),
        route,
        true,
    ))
}

/// The production tool executor: N05's client behind N04's broker, fenced on
/// the persisted native seat record this run just wrote.
/// Issue #479: takes `request` by `&mut` so the live writer permit can be
/// MOVED into the broker rather than cloned -- a lease is the right to write
/// one tree, and duplicating it would be exactly the thing the per-tree claim
/// exists to prevent.
fn brokered_tools(
    request: &mut HeadlessRequest<'_>,
    state: &super::super::state::StateDir,
    home: &std::path::Path,
    cfg: &super::super::config::CtxConfig,
    handle: &SessionHandle,
) -> CtxResult<Box<dyn ToolExecutor>> {
    use super::enforcement::ExecutionIdentity;
    use super::tools::ToolLimits;

    let broker = session_broker(
        request.repo,
        state,
        home,
        cfg,
        ExecutionIdentity::from_handle(handle, request.task.clone())?,
        request.writer.take(),
    )?;
    let services = super::capabilities::CapabilityServices::from_config(
        cfg,
        request.repo,
        &super::super::config::env_from_process(),
        super::super::state::now_secs(),
    );
    Ok(Box::new(ClientToolExecutor::new(
        NativeToolClient::new(
            broker,
            state.clone(),
            request.repo.to_path_buf(),
            ToolLimits::from_config(cfg),
        )
        .with_capabilities(services),
    )))
}

/// The execution broker one native session runs behind.
///
/// Extracted from [`brokered_tools`] for issue #484 (roadmap N15) so the
/// read-only helper contract can be asserted against the SAME construction a
/// real session gets, rather than against a test-local copy of it that could
/// drift. `writer` is the whole of that contract: a `None` lease means every
/// repository write, outside write, write-effect process and shared-scope
/// knowledge write is refused here, at effect time, with
/// `BrokerError::WriterPermit` -- and `ApprovalMode::Headless` means the
/// refusal cannot be approved away either.
pub(crate) fn session_broker(
    repo: &std::path::Path,
    state: &super::super::state::StateDir,
    home: &std::path::Path,
    cfg: &super::super::config::CtxConfig,
    identity: super::enforcement::ExecutionIdentity,
    writer: Option<Box<dyn super::enforcement::WriterLease>>,
) -> Result<super::enforcement::ExecutionBroker, super::enforcement::BrokerError> {
    use super::enforcement::{
        ApprovalAuthority, ApprovalMode, ConfigPolicySource, ExecutionBroker, PlatformIsolation,
        ResourceClaims, StoredSeatFence,
    };

    let claims = ResourceClaims::new(
        repo,
        repo,
        state.root(),
        home,
        configured_network_scope(cfg),
    )?;
    // Only a session that actually holds a writer lease claims git metadata
    // roots. `discover_linked_worktree_git` refuses a MAIN checkout outright
    // ("native writers require a linked worktree"), which is the right answer
    // for a worker that was granted a tree -- and the wrong one for a
    // read-only helper or a plain `zirv ctx exec --runtime native`, neither of
    // which can write anything at all: without this, an inspection session in
    // an ordinary checkout could not even construct its broker.
    let claims = match writer {
        Some(_) => claims.discover_linked_worktree_git()?,
        None => claims,
    };

    ExecutionBroker::new(
        identity,
        claims,
        ApprovalMode::Headless,
        std::sync::Arc::new(ConfigPolicySource::new(repo.to_path_buf())),
        std::sync::Arc::new(StoredSeatFence::new(state.clone())),
        std::sync::Arc::new(ApprovalAuthority::new()),
        writer,
        PlatformIsolation::detect(),
        Default::default(),
    )
}

/// The task's network claim, built from the operator's own capability
/// allowlist (issue #483). Nothing is reachable by default: a session gets a
/// host-scoped claim only for the hosts an operator wrote down, and
/// `NetworkScope::Only` is deliberately never `Any` -- an arbitrary process
/// still cannot take network under it, which is exactly the asymmetry N04
/// documents between brokered HTTP tools and shells.
fn configured_network_scope(
    cfg: &super::super::config::CtxConfig,
) -> super::enforcement::NetworkScope {
    use super::enforcement::{NetworkScope, NetworkTarget};

    if !cfg.capabilities.enabled {
        return NetworkScope::Denied;
    }
    let mut targets = std::collections::BTreeSet::new();
    for host in &cfg.capabilities.web.allow_hosts {
        let host = host.trim().trim_start_matches('.');
        for scheme in ["https", "http"] {
            if let Ok(target) = NetworkTarget::new(scheme, host, None) {
                targets.insert(target);
            }
        }
    }
    if targets.is_empty() {
        NetworkScope::Denied
    } else {
        NetworkScope::Only { targets }
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::provider::{
        AccountId, BillingPoolId, EndpointId, ModelId, Protocol, ProviderId, RouteId,
    };
    use super::super::fixture::{
        FixtureProvider, FixtureScript, FixtureToolExecutor, FixtureToolScript, fixture_root,
        fixture_target,
    };
    use super::super::journal::{JournalEvent, SeatId, SessionIdentity};
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn route_for(protocol: Protocol, model: &str) -> RouteIdentity {
        RouteIdentity {
            route: RouteId::new("fixture").unwrap(),
            provider: ProviderId::new(match protocol {
                Protocol::AnthropicMessages => "anthropic",
                Protocol::GoogleGenerativeAi | Protocol::GoogleVertex => "google",
                _ => "openai",
            })
            .unwrap(),
            endpoint: EndpointId::new("fixture").unwrap(),
            account: AccountId::new("fixture").unwrap(),
            billing_pool: BillingPoolId::new("fixture").unwrap(),
            protocol,
            model: ModelId {
                vendor: "fixture".into(),
                id: model.into(),
            },
        }
    }

    fn journal_for(route: &RouteIdentity) -> (tempfile::TempDir, Journal, JournalSessionId) {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open_path(dir.path().join("journal.sqlite")).unwrap();
        let session = JournalSessionId::new("native-session-1").unwrap();
        journal
            .create_session(&SessionIdentity {
                session: session.clone(),
                seat: SeatId::new("seat-1").unwrap(),
                generation: 1,
                task: None,
                route: route.clone(),
                created_at: 1,
                completed_at: None,
            })
            .unwrap();
        (dir, journal, session)
    }

    fn config_for(session: JournalSessionId, route: RouteIdentity) -> NativeSessionConfig {
        NativeSessionConfig {
            session,
            generation: 1,
            route,
            role: "worker".to_string(),
            seat_model: None,
            write_posture: OrchestratorWrites::Allow,
            limits: NativeLimits::default(),
            task: None,
            workflow_gate: None,
            compaction: CompactionSettings::default(),
            workflow_repo: None,
            system: Vec::new(),
            preamble: Vec::new(),
        }
    }

    /// Issue #484 (roadmap N15) item 3: workflow and methodology adoption is
    /// AUTOMATIC. Nobody hand-seeds a native session with a methodology
    /// prompt; the context compiler puts the engineering standard and the
    /// active workflow's current step into every request the session makes,
    /// on the strength of the workflow store alone.
    #[test]
    fn a_native_session_adopts_the_active_workflow_without_being_seeded() {
        use crate::commands::workflow::engine;

        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("home");
        let state = crate::commands::ctx::state::StateDir::from_root(
            tempfile::tempdir().expect("state").keep(),
        );
        let workflow = engine::WorkflowState::start(
            repo.path().to_path_buf(),
            "wire the native workflow tools".into(),
            engine::WorkflowKind::Feature,
            None,
            true,
            crate::commands::workflow::classify::Classification {
                intent: crate::commands::workflow::classify::Intent::Feature,
                complexity: crate::commands::workflow::classify::Complexity::Trivial,
                risk: crate::commands::workflow::classify::RiskBand::Low,
                risk_score: 0,
                changed_files: 1,
                changed_lines: 5,
                declared_scope: false,
                work_domain: Default::default(),
                risk_measurement: crate::commands::workflow::classify::RiskMeasurement::Measured,
                reasons: vec!["small".into()],
            },
        );
        engine::save(&state, &workflow, true).expect("save");

        let route = route_for(Protocol::AnthropicMessages, "claude-fixture");
        let session = JournalSessionId::new("native-adoption-1").expect("session id");
        let request = HeadlessRequest {
            repo: repo.path(),
            prompt: "continue the workflow",
            route: None,
            role: "orchestrator",
            limits: NativeLimits::default(),
            resume: None,
            provider: None,
            fixture_tools: None,
            task: None,
            writer: None,
        };
        let (system, preamble) = compile_standing_context(
            &state,
            home.path(),
            &Default::default(),
            &request,
            &route,
            &session,
            1,
        )
        .expect("the standing context compiles");

        let system_text = system.join(
            "
",
        );
        assert!(
            system_text.contains("zirv native model profile"),
            "the model profile is part of every native session's instructions"
        );
        assert!(
            !system.is_empty() && system_text.len() > 200,
            "the engineering standard and role methodology must be present: {system_text:?}"
        );
        let all = format!(
            "{system_text}
{}",
            preamble.join(
                "
"
            )
        );
        assert!(
            all.contains("wire the native workflow tools"),
            "the active workflow's own task must reach the session unseeded: {all}"
        );
    }

    fn script(name: &str) -> FixtureScript {
        FixtureScript::load(&fixture_root().join(name)).expect("provider fixture")
    }

    fn tool_script(name: &str) -> FixtureToolScript {
        FixtureToolScript::load(&fixture_root().join(name)).expect("tool fixture")
    }

    fn spec(runtime: RuntimeKind) -> SessionSpec {
        SessionSpec {
            runtime,
            role: "worker".into(),
            agent: None,
            provider_route: None,
            model: None,
            surface: UiSurface::Headless,
            cwd: std::path::PathBuf::from("."),
            prompt: "go".into(),
            extra_args: Vec::new(),
        }
    }

    /// Drives one whole fixture session and hands back its final status plus
    /// the tool-call ids in the order effects actually ran.
    fn run_fixture(
        protocol: Protocol,
        model: &str,
        provider_fixture: &str,
        tool_fixture: &str,
        prompt: &str,
        mutate: impl FnOnce(&mut NativeSessionConfig),
    ) -> (NativeFinalStatus, Vec<String>) {
        let route = route_for(protocol, model);
        let (_dir, mut journal, session) = journal_for(&route);
        let provider =
            FixtureProvider::new(fixture_target(protocol, model), script(provider_fixture));
        let mut tools = FixtureToolExecutor::new(tool_script(tool_fixture));
        let mut cfg = config_for(session, route);
        mutate(&mut cfg);
        let clock = || 1_000u64;
        let status = {
            let mut driver = NativeLoop::new(
                cfg,
                &provider,
                &mut tools,
                &mut journal,
                Arc::new(CancellationFlag::default()),
                &clock,
                &no_env,
            );
            driver.acknowledge(prompt, false).expect("acknowledged");
            driver.run_to_completion().expect("ran")
        };
        (status, tools.calls)
    }

    // -- (a) multi-turn investigate/edit/test, once per primary provider ---

    #[test]
    fn an_anthropic_shaped_session_investigates_edits_and_tests_over_multiple_turns() {
        let (status, calls) = run_fixture(
            Protocol::AnthropicMessages,
            "fixture-anthropic-model",
            "anthropic-investigate-edit-test.json",
            "tools-investigate-edit-test.json",
            "fix the failing test",
            |_| {},
        );
        assert_eq!(status.status, NativeStatus::Completed);
        assert_eq!(status.requests, 4);
        assert_eq!(status.tool_calls, 4);
        assert_eq!(
            calls,
            vec![
                "call_read_src",
                "call_read_test",
                "call_patch",
                "call_tests"
            ]
        );
        assert_eq!(
            status.served_model.as_deref(),
            Some("fixture-anthropic-model")
        );
        assert_eq!(status.usage.input_tokens, 120 + 200 + 260 + 300);
        assert_eq!(status.exit_code, 0);
    }

    /// The N13 compatibility suite: the same investigate/edit/test script
    /// has to drive the loop to the same four effects through *every* route
    /// profile's adapter shape, not just the three primary ones. The table
    /// is checked for completeness against the registry itself, so adding a
    /// profile without proving its shape fails here rather than shipping an
    /// untested route.
    #[test]
    fn every_route_profile_shape_completes_the_investigate_edit_test_script() {
        use crate::commands::ctx::provider::Support;
        use crate::commands::ctx::provider::profiles::profiles;

        let table: &[(&str, Protocol, &str, &str)] = &[
            (
                "anthropic-messages",
                Protocol::AnthropicMessages,
                "fixture-anthropic-model",
                "anthropic-investigate-edit-test.json",
            ),
            (
                "openai-responses",
                Protocol::OpenAiResponses,
                "fixture-openai-model",
                "openai-investigate-edit-test.json",
            ),
            (
                "google-developer",
                Protocol::GoogleGenerativeAi,
                "fixture-google-model",
                "google-investigate-edit-test.json",
            ),
            (
                "google-vertex",
                Protocol::GoogleVertex,
                "fixture-google-model",
                "google-investigate-edit-test.json",
            ),
            (
                "azure-openai-chat",
                Protocol::AzureOpenAiChat,
                "fixture-chat-model",
                "chat-investigate-edit-test.json",
            ),
            (
                "aws-bedrock-anthropic",
                Protocol::AwsBedrock,
                "fixture-bedrock-model",
                "bedrock-investigate-edit-test.json",
            ),
            (
                "aws-bedrock-converse",
                Protocol::AwsBedrock,
                "fixture-bedrock-model",
                "bedrock-investigate-edit-test.json",
            ),
        ];
        let chat_profiles: Vec<&'static str> = profiles()
            .iter()
            .filter(|profile| {
                profile.protocol == Protocol::OpenAiChatCompatible
                    && profile.support == Support::Native
            })
            .map(|profile| profile.id)
            .collect();
        assert!(
            chat_profiles.len() >= 10,
            "the compatible-vendor family should be the largest one: {chat_profiles:?}"
        );

        let covered: Vec<&str> = table
            .iter()
            .map(|(id, ..)| *id)
            .chain(chat_profiles.iter().copied())
            .collect();
        for profile in profiles() {
            if profile.support != Support::Native {
                continue;
            }
            assert!(
                covered.contains(&profile.id),
                "route profile `{}` has no compatibility-suite row",
                profile.id
            );
        }

        // The expected effects come from each script itself, so a shape's
        // own call ids stay its own while the *order* stays the contract.
        let expected_calls = |fixture: &str| -> Vec<String> {
            script(fixture)
                .turns
                .iter()
                .flat_map(|turn| {
                    turn.blocks.iter().filter_map(|block| match block {
                        super::super::fixture::FixtureBlock::ToolUse { id, .. } => Some(id.clone()),
                        _ => None,
                    })
                })
                .collect()
        };
        let rows = table.iter().copied().chain(
            chat_profiles
                .iter()
                .map(|id| {
                    (
                        *id,
                        Protocol::OpenAiChatCompatible,
                        "fixture-chat-model",
                        "chat-investigate-edit-test.json",
                    )
                })
                .collect::<Vec<_>>(),
        );
        for (profile_id, protocol, model, fixture) in rows {
            let (status, calls) = run_fixture(
                protocol,
                model,
                fixture,
                "tools-investigate-edit-test.json",
                "fix the failing test",
                |_| {},
            );
            assert_eq!(
                status.status,
                NativeStatus::Completed,
                "profile `{profile_id}`"
            );
            assert_eq!(status.requests, 4, "profile `{profile_id}`");
            assert_eq!(status.tool_calls, 4, "profile `{profile_id}`");
            assert_eq!(calls, expected_calls(fixture), "profile `{profile_id}`");
            assert_eq!(status.served_model.as_deref(), Some(model));
            assert_eq!(status.exit_code, 0, "profile `{profile_id}`");
        }
    }

    #[test]
    fn an_openai_shaped_session_investigates_edits_and_tests_over_multiple_turns() {
        let (status, calls) = run_fixture(
            Protocol::OpenAiResponses,
            "fixture-openai-model",
            "openai-investigate-edit-test.json",
            "tools-investigate-edit-test.json",
            "fix the failing test",
            |_| {},
        );
        assert_eq!(status.status, NativeStatus::Completed);
        assert_eq!(status.tool_calls, 4);
        assert_eq!(calls, vec!["fc_search", "fc_read", "fc_write", "fc_tests"]);
        assert_eq!(status.served_model.as_deref(), Some("fixture-openai-model"));
    }

    // -- (b) partial JSON, interleaving, refusals, empty turns, disconnects -

    #[test]
    fn a_truncated_tool_argument_stream_never_executes() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, mut journal, session) = journal_for(&route);
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, "fixture-anthropic-model"),
            script("edge-cases.json"),
        );
        let mut tools = FixtureToolExecutor::new(tool_script("tools-investigate-edit-test.json"));
        let clock = || 1_000u64;
        let outcome = {
            let mut driver = NativeLoop::new(
                config_for(session, route),
                &provider,
                &mut tools,
                &mut journal,
                Arc::new(CancellationFlag::default()),
                &clock,
                &no_env,
            );
            driver.acknowledge("go", false).unwrap();
            driver.run_turn().expect("turn")
        };
        // No COMPLETE tool call, so the turn ends on the model's own token and
        // nothing at all was executed.
        assert_eq!(outcome.state, TurnState::Completed);
        assert!(tools.calls.is_empty());
    }

    #[test]
    fn an_empty_response_and_a_refusal_are_both_explicit_terminal_states() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, mut journal, session) = journal_for(&route);
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, "fixture-anthropic-model"),
            script("edge-cases.json"),
        );
        let mut tools = FixtureToolExecutor::new(tool_script("tools-investigate-edit-test.json"));
        let clock = || 1_000u64;
        let mut driver = NativeLoop::new(
            config_for(session, route),
            &provider,
            &mut tools,
            &mut journal,
            Arc::new(CancellationFlag::default()),
            &clock,
            &no_env,
        );
        driver.acknowledge("go", false).unwrap();
        let truncated = driver.run_turn().unwrap();
        assert_eq!(truncated.finish_reason, Some(FinishReason::ToolUse));

        let empty = driver.run_turn().unwrap();
        assert_eq!(empty.state, TurnState::Completed);
        assert_eq!(empty.final_text, None);

        let refusal = driver.run_turn().unwrap();
        assert_eq!(refusal.state, TurnState::Completed);
        assert_eq!(refusal.finish_reason, Some(FinishReason::Refusal));
        assert_eq!(refusal.final_text.as_deref(), Some("I will not do that."));
    }

    #[test]
    fn interleaved_text_and_tool_blocks_keep_the_declared_result_order() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, mut journal, session) = journal_for(&route);
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, "fixture-anthropic-model"),
            script("edge-cases.json"),
        );
        let mut tools = FixtureToolExecutor::new(tool_script("tools-investigate-edit-test.json"));
        let clock = || 1_000u64;
        let outcome = {
            let mut driver = NativeLoop::new(
                config_for(session, route),
                &provider,
                &mut tools,
                &mut journal,
                Arc::new(CancellationFlag::default()),
                &clock,
                &no_env,
            );
            driver.acknowledge("go", false).unwrap();
            // Three turns end on the model's own token (a truncated call that
            // never became executable, an empty response, a refusal); the
            // fourth is the interleaved one.
            for _ in 0..3 {
                driver.run_turn().unwrap();
            }
            driver.run_turn().expect("interleaved turn")
        };
        let ids: Vec<String> = outcome
            .results
            .iter()
            .map(|result| result.call.id.to_string())
            .collect();
        assert_eq!(ids, vec!["call_a", "call_b"]);
        assert_eq!(outcome.final_text.as_deref(), Some("onetwothree"));
    }

    #[test]
    fn two_disconnects_are_retried_within_the_response_budget() {
        let (status, _) = run_fixture(
            Protocol::AnthropicMessages,
            "fixture-anthropic-model",
            "disconnect-then-recover.json",
            "tools-investigate-edit-test.json",
            "go",
            |cfg| cfg.limits.response_retry_budget = 2,
        );
        assert_eq!(status.status, NativeStatus::Completed);
        assert_eq!(status.requests, 3);
        assert_eq!(
            status
                .evidence
                .iter()
                .filter(|note| note.kind == "response_retry")
                .count(),
            2
        );
    }

    #[test]
    fn a_disconnect_past_the_retry_budget_fails_explicitly() {
        let (status, _) = run_fixture(
            Protocol::AnthropicMessages,
            "fixture-anthropic-model",
            "disconnect-then-recover.json",
            "tools-investigate-edit-test.json",
            "go",
            |cfg| cfg.limits.response_retry_budget = 0,
        );
        assert_eq!(status.status, NativeStatus::Failed);
        assert!(status.failure.unwrap().contains("connection reset"));
    }

    // -- (c) input delivery, steering and interruption ---------------------

    #[test]
    fn steering_accepted_after_the_last_request_stays_visibly_queued() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, mut journal, session) = journal_for(&route);
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, "fixture-anthropic-model"),
            script("anthropic-investigate-edit-test.json"),
        );
        let mut tools = FixtureToolExecutor::new(tool_script("tools-investigate-edit-test.json"));
        let clock = || 1_000u64;
        let mut driver = NativeLoop::new(
            config_for(session, route),
            &provider,
            &mut tools,
            &mut journal,
            Arc::new(CancellationFlag::default()),
            &clock,
            &no_env,
        );
        driver.acknowledge("start", false).unwrap();
        let status = driver.run_to_completion().unwrap();
        assert!(status.queued_input.is_empty());

        let steered = driver.acknowledge("also update the docs", true).unwrap();
        assert_eq!(driver.queued_input().unwrap(), vec![steered]);
    }

    #[test]
    fn an_acknowledged_input_reaches_the_conversation_exactly_once() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, mut journal, session) = journal_for(&route);
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, "fixture-anthropic-model"),
            script("anthropic-investigate-edit-test.json"),
        );
        let mut tools = FixtureToolExecutor::new(tool_script("tools-investigate-edit-test.json"));
        let clock = || 1_000u64;
        let occurrences = {
            let mut driver = NativeLoop::new(
                config_for(session.clone(), route),
                &provider,
                &mut tools,
                &mut journal,
                Arc::new(CancellationFlag::default()),
                &clock,
                &no_env,
            );
            driver.acknowledge("only once", false).unwrap();
            driver.run_to_completion().unwrap();
            driver
                .journal
                .replay(&session)
                .unwrap()
                .messages
                .iter()
                .filter(|message| message.text.as_deref() == Some("only once"))
                .count()
        };
        assert_eq!(occurrences, 1);
    }

    #[test]
    fn an_interrupt_never_marks_the_turn_complete() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, mut journal, session) = journal_for(&route);
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, "fixture-anthropic-model"),
            script("anthropic-investigate-edit-test.json"),
        );
        let mut tools = FixtureToolExecutor::new(tool_script("tools-investigate-edit-test.json"));
        let cancel = Arc::new(CancellationFlag::default());
        cancel.cancel();
        let clock = || 1_000u64;
        let status = {
            let mut driver = NativeLoop::new(
                config_for(session, route),
                &provider,
                &mut tools,
                &mut journal,
                Arc::clone(&cancel),
                &clock,
                &no_env,
            );
            driver.acknowledge("go", false).unwrap();
            driver.run_to_completion().unwrap()
        };
        assert_eq!(status.status, NativeStatus::Interrupted);
        assert_ne!(status.exit_code, 0);
        assert!(tools.calls.is_empty());
    }

    // -- (d) the durable barrier ------------------------------------------

    #[test]
    fn the_assistant_message_and_tool_call_are_durable_before_any_effect() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, mut journal, session) = journal_for(&route);
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, "fixture-anthropic-model"),
            script("anthropic-investigate-edit-test.json"),
        );
        let mut tools = FixtureToolExecutor::new(tool_script("tools-investigate-edit-test.json"));
        let clock = || 1_000u64;
        let kinds: Vec<&str> = {
            let mut driver = NativeLoop::new(
                config_for(session.clone(), route),
                &provider,
                &mut tools,
                &mut journal,
                Arc::new(CancellationFlag::default()),
                &clock,
                &no_env,
            );
            driver.acknowledge("go", false).unwrap();
            driver.run_turn().unwrap();
            driver
                .journal
                .events(&session)
                .unwrap()
                .iter()
                .map(|event| match &event.event {
                    JournalEvent::InputAcknowledged { .. } => "input",
                    JournalEvent::UsageRecorded { .. } => "usage",
                    JournalEvent::AssistantMessageCommitted { .. } => "assistant",
                    JournalEvent::ToolCallPrepared { .. } => "tool_call",
                    JournalEvent::ToolExecution { state, .. } => match state {
                        ExecutionState::Prepared => "prepared",
                        ExecutionState::Started => "started",
                        _ => "finished",
                    },
                    _ => "other",
                })
                .collect()
        };
        let assistant = kinds.iter().position(|kind| *kind == "assistant").unwrap();
        let first_tool_call = kinds.iter().position(|kind| *kind == "tool_call").unwrap();
        let first_started = kinds.iter().position(|kind| *kind == "started").unwrap();
        assert!(assistant < first_tool_call, "{kinds:?}");
        assert!(first_tool_call < first_started, "{kinds:?}");
    }

    #[test]
    fn a_denied_tool_is_never_executed_and_carries_its_reason_back() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, mut journal, session) = journal_for(&route);
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, "fixture-anthropic-model"),
            script("anthropic-investigate-edit-test.json"),
        );
        let mut tools = FixtureToolExecutor::new(tool_script("tools-investigate-edit-test.json"));
        let clock = || 1_000u64;
        let outcome = {
            let mut cfg = config_for(session, route);
            // An orchestrator seat under a deny posture may not edit the repo.
            cfg.role = "orchestrator".to_string();
            cfg.write_posture = OrchestratorWrites::Deny;
            let mut driver = NativeLoop::new(
                cfg,
                &provider,
                &mut tools,
                &mut journal,
                Arc::new(CancellationFlag::default()),
                &clock,
                &no_env,
            );
            driver.acknowledge("go", false).unwrap();
            driver.run_turn().expect("turn")
        };
        let patch = outcome
            .results
            .iter()
            .find(|result| result.call.name == "apply_patch")
            .expect("apply_patch result");
        assert_eq!(patch.state, ToolState::Cancelled);
        assert!(patch.content.contains("dispatch a worker"));
        assert!(!tools.calls.contains(&"call_patch".to_string()));
    }

    // -- (e) retries, cancellation and unknown outcomes --------------------

    #[test]
    fn a_safe_tool_failure_is_retried_and_a_reconcile_tool_never_is() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, mut journal, session) = journal_for(&route);
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, "fixture-anthropic-model"),
            script("anthropic-investigate-edit-test.json"),
        );
        let mut tools = FixtureToolExecutor::new(tool_script("tools-mixed-outcomes.json"));
        let clock = || 1_000u64;
        let turn = {
            let mut driver = NativeLoop::new(
                config_for(session, route),
                &provider,
                &mut tools,
                &mut journal,
                Arc::new(CancellationFlag::default()),
                &clock,
                &no_env,
            );
            driver.acknowledge("go", false).unwrap();
            driver.run_turn().unwrap()
        };
        let read = &turn.results[0];
        // `file_read` is Safe: the first attempt failed, the second succeeded,
        // and the completed result is the one that survives.
        assert_eq!(read.call.id.to_string(), "call_read_src");
        assert_eq!(read.state, ToolState::Completed);
        assert_eq!(read.attempts, 2);
        assert_eq!(
            tools
                .calls
                .iter()
                .filter(|id| *id == "call_read_src")
                .count(),
            2
        );
        // `apply_patch` is Reconcile and reported an unknown outcome, so it is
        // never replayed -- exactly one effect, whatever the retry budget says.
        let patch = turn
            .results
            .iter()
            .find(|result| result.call.name == "apply_patch")
            .expect("apply_patch result");
        assert_eq!(patch.state, ToolState::OutcomeUnknown);
        assert_eq!(patch.attempts, 1);
        assert_eq!(
            tools.calls.iter().filter(|id| *id == "call_patch").count(),
            1
        );
    }

    #[test]
    fn an_outcome_unknown_effect_forces_an_incomplete_final_status() {
        let (status, _) = run_fixture(
            Protocol::AnthropicMessages,
            "fixture-anthropic-model",
            "anthropic-investigate-edit-test.json",
            "tools-mixed-outcomes.json",
            "go",
            |_| {},
        );
        // The model's last turn says `end_turn`; the unknown patch outcome
        // outranks it.
        assert_eq!(status.status, NativeStatus::Incomplete);
        assert_eq!(status.outcome_unknown_tools, vec!["call_patch".to_string()]);
        assert_ne!(status.exit_code, 0);
    }

    #[test]
    fn a_completed_result_is_never_lost_when_a_sibling_call_fails() {
        let route = route_for(Protocol::OpenAiResponses, "fixture-openai-model");
        let (_dir, mut journal, session) = journal_for(&route);
        let provider = FixtureProvider::new(
            fixture_target(Protocol::OpenAiResponses, "fixture-openai-model"),
            script("openai-investigate-edit-test.json"),
        );
        let mut tools = FixtureToolExecutor::new(tool_script("tools-mixed-outcomes.json"));
        let clock = || 1_000u64;
        let outcome = {
            let mut driver = NativeLoop::new(
                config_for(session, route),
                &provider,
                &mut tools,
                &mut journal,
                Arc::new(CancellationFlag::default()),
                &clock,
                &no_env,
            );
            driver.acknowledge("go", false).unwrap();
            driver.run_turn().expect("turn")
        };
        // `process_start` fails outright in this script; the two reads that
        // ran before it, and the write after it, all keep their own results.
        assert_eq!(
            outcome
                .results
                .iter()
                .map(|result| result.call.id.to_string())
                .collect::<Vec<_>>(),
            vec!["fc_search", "fc_read", "fc_write", "fc_tests"]
        );
        assert_eq!(outcome.results[0].state, ToolState::Completed);
        assert_eq!(outcome.results[1].state, ToolState::Completed);
        assert_eq!(outcome.results[2].state, ToolState::Completed);
        assert_eq!(outcome.results[3].state, ToolState::Failed);
    }

    #[test]
    fn a_native_google_agent_completes_a_coding_task_with_no_gemini_cli_binary() {
        // Same per-provider loop pattern as the Anthropic/OpenAI fixtures
        // (issue #481, N12): a search, a read, an edit and a test run, then a
        // final answer -- driven entirely through `ProviderAdapter`, with no
        // Gemini CLI process anywhere in the path.
        let (status, _) = run_fixture(
            Protocol::GoogleGenerativeAi,
            "fixture-google-model",
            "google-investigate-edit-test.json",
            "tools-investigate-edit-test.json",
            "go",
            |_| {},
        );
        assert_eq!(status.status, NativeStatus::Completed);
        assert_eq!(status.exit_code, 0);
    }

    // -- (f) no installed coding harness, lifecycle decisions included -----

    #[test]
    fn a_whole_native_session_runs_with_an_empty_path_and_no_harness_binary() {
        // Every environment lookup a lifecycle decision could make -- PATH, a
        // harness home, a seat variable -- is answered as absent or empty, so
        // any binary probe would fail. The session still runs, and still makes
        // its own admission decision.
        fn hostile_env(name: &str) -> Option<String> {
            match name {
                "PATH" => Some(String::new()),
                _ => None,
            }
        }
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, mut journal, session) = journal_for(&route);
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, "fixture-anthropic-model"),
            script("anthropic-investigate-edit-test.json"),
        );
        let mut tools = FixtureToolExecutor::new(tool_script("tools-investigate-edit-test.json"));
        let clock = || 1_000u64;
        let status = {
            let mut cfg = config_for(session, route);
            cfg.seat_model = Some("claude-mythos-5".to_string());
            cfg.role = "orchestrator".to_string();
            let mut driver = NativeLoop::new(
                cfg,
                &provider,
                &mut tools,
                &mut journal,
                Arc::new(CancellationFlag::default()),
                &clock,
                &hostile_env,
            );
            driver.acknowledge("go", false).unwrap();
            driver.run_to_completion().unwrap()
        };
        assert_eq!(status.status, NativeStatus::Completed);
        assert_eq!(status.runtime, "native");
    }

    // -- limits ------------------------------------------------------------

    #[test]
    fn the_tool_call_ceiling_stops_the_loop_and_names_itself() {
        let (status, _) = run_fixture(
            Protocol::AnthropicMessages,
            "fixture-anthropic-model",
            "anthropic-investigate-edit-test.json",
            "tools-investigate-edit-test.json",
            "go",
            |cfg| cfg.limits.max_tool_calls = 1,
        );
        assert_eq!(status.status, NativeStatus::LimitReached);
        assert_eq!(status.limit, Some(LimitKind::ToolCalls));
    }

    #[test]
    fn the_wall_clock_ceiling_stops_the_loop() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, mut journal, session) = journal_for(&route);
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, "fixture-anthropic-model"),
            script("anthropic-investigate-edit-test.json"),
        );
        let mut tools = FixtureToolExecutor::new(tool_script("tools-investigate-edit-test.json"));
        let ticks = std::sync::atomic::AtomicU64::new(0);
        let clock = || ticks.fetch_add(1_000, std::sync::atomic::Ordering::AcqRel);
        let status = {
            let mut cfg = config_for(session, route);
            cfg.limits.max_wall_ms = 1;
            let mut driver = NativeLoop::new(
                cfg,
                &provider,
                &mut tools,
                &mut journal,
                Arc::new(CancellationFlag::default()),
                &clock,
                &no_env,
            );
            driver.acknowledge("go", false).unwrap();
            driver.run_to_completion().unwrap()
        };
        assert_eq!(status.status, NativeStatus::LimitReached);
        assert_eq!(status.limit, Some(LimitKind::WallClock));
    }

    // -- pure scheduling ---------------------------------------------------

    #[test]
    fn independent_calls_run_first_and_declared_order_is_kept_inside_each_group() {
        assert_eq!(
            execution_order(&[false, true, false, true]),
            vec![1, 3, 0, 2]
        );
        assert_eq!(execution_order(&[true, true]), vec![0, 1]);
        assert_eq!(execution_order(&[false, false]), vec![0, 1]);
    }

    #[test]
    fn an_unknown_tool_is_never_reordered() {
        assert!(!is_independent(None));
    }

    // -- the backend seam --------------------------------------------------

    #[test]
    fn a_second_submit_while_a_turn_is_running_is_busy_not_a_silent_interleave() {
        let mut backend = NativeBackend::new();
        let handle = backend.start(&spec(RuntimeKind::Native)).expect("started");
        backend.submit(&handle, "first").expect("first submit");
        let error = backend.submit(&handle, "second").expect_err("busy");
        assert!(error.to_string().contains("busy"));
        // Steering is always accepted, including mid-turn.
        backend.steer(&handle, "also do this").expect("steered");
    }

    #[test]
    fn a_resume_clears_the_interrupt_flag_and_bumps_the_generation() {
        let mut backend = NativeBackend::new();
        let handle = backend.start(&spec(RuntimeKind::Native)).expect("started");
        backend.interrupt(&handle).expect("interrupted");
        assert!(backend.cancellation(&handle).unwrap().is_cancelled());
        let resumed = backend.resume(&handle, Some("carry on")).expect("resumed");
        assert_eq!(resumed.generation, handle.generation + 1);
        assert!(!backend.cancellation(&resumed).unwrap().is_cancelled());
        assert_eq!(backend.state(&resumed), Some(SessionState::Idle));
        let stale = backend.submit(&handle, "stale").expect_err("stale");
        assert!(stale.to_string().contains("stale generation"));
    }

    #[test]
    fn the_backend_refuses_to_start_a_harness_session() {
        let mut backend = NativeBackend::new();
        let error = backend
            .start(&spec(RuntimeKind::Harness))
            .expect_err("wrong runtime");
        assert!(error.to_string().contains("harness"));
    }

    // -- review round 2 ----------------------------------------------------

    /// Finding 1: `ConversationState::executions` is keyed by `ExecutionId`
    /// and therefore iterates in ID order. Taking the first execution that
    /// mentions a call handed the CONTINUATION request the failed attempt a
    /// successful retry had already superseded.
    #[test]
    fn a_continuation_request_carries_the_retrys_result_not_the_failed_attempt() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, mut journal, session) = journal_for(&route);
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, "fixture-anthropic-model"),
            script("anthropic-investigate-edit-test.json"),
        );
        let mut tools = FixtureToolExecutor::new(tool_script("tools-mixed-outcomes.json"));
        let clock = || 1_000u64;
        {
            let mut driver = NativeLoop::new(
                config_for(session, route),
                &provider,
                &mut tools,
                &mut journal,
                Arc::new(CancellationFlag::default()),
                &clock,
                &no_env,
            );
            driver.acknowledge("go", false).unwrap();
            driver.run_turn().unwrap();
        }
        // `file_read` failed on its first attempt and succeeded on its retry.
        // The SECOND request is the continuation that replays that result.
        let sent = provider.sent();
        assert!(sent.len() >= 2, "expected a continuation request");
        let result = sent[1]
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .find_map(|block| match block {
                ProviderContent::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } if tool_use_id == "call_read_src" => Some((content.clone(), *is_error)),
                _ => None,
            })
            .expect("call_read_src result in the continuation request");
        assert!(!result.1, "replayed the failed attempt: {result:?}");
        assert!(
            result.0.contains("second attempt worked"),
            "replayed the wrong attempt: {result:?}"
        );
    }

    /// Finding 4: a turn is one unit of user intent, and `max_turns` bounds
    /// how many a session may run -- enforced in `run_turn` itself, so it
    /// holds for any driver, not only `run_to_completion`.
    #[test]
    fn the_turn_ceiling_bounds_a_session_across_separately_driven_turns() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, mut journal, session) = journal_for(&route);
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, "fixture-anthropic-model"),
            script("edge-cases.json"),
        );
        let mut tools = FixtureToolExecutor::new(tool_script("tools-investigate-edit-test.json"));
        let clock = || 1_000u64;
        let mut cfg = config_for(session, route);
        cfg.limits.max_turns = 2;
        let mut driver = NativeLoop::new(
            cfg,
            &provider,
            &mut tools,
            &mut journal,
            Arc::new(CancellationFlag::default()),
            &clock,
            &no_env,
        );
        driver.acknowledge("go", false).unwrap();
        assert_eq!(driver.run_turn().unwrap().state, TurnState::Completed);
        assert_eq!(driver.run_turn().unwrap().state, TurnState::Completed);
        let third = driver.run_turn().unwrap();
        assert_eq!(third.state, TurnState::Failed);
        assert_eq!(third.limit, Some(LimitKind::Turns));
        // The refused turn sent nothing at all.
        assert_eq!(third.requests, 0);
    }

    /// Finding 4, the other half: a turn that finished while an input was
    /// still queued is not the end of the session -- the next delivery
    /// boundary is another turn, and that is what `max_turns` bounds.
    #[test]
    fn run_to_completion_runs_another_turn_for_input_queued_during_the_last_one() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, mut journal, session) = journal_for(&route);
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, "fixture-anthropic-model"),
            script("edge-cases.json"),
        );
        let mut tools = FixtureToolExecutor::new(tool_script("tools-investigate-edit-test.json"));
        let clock = || 1_000u64;
        let status = {
            let mut cfg = config_for(session.clone(), route);
            cfg.limits.max_turns = 1;
            let mut driver = NativeLoop::new(
                cfg,
                &provider,
                &mut tools,
                &mut journal,
                Arc::new(CancellationFlag::default()),
                &clock,
                &no_env,
            );
            driver.acknowledge("go", false).unwrap();
            // The first turn ends on the truncated-call response, having
            // delivered "go". A second input is acknowledged before the loop
            // asks whether anything is still queued.
            driver.run_turn().unwrap();
            driver.acknowledge("and this too", true).unwrap();
            driver.run_to_completion().unwrap()
        };
        // Turn 1 is spent, so the queued input cannot be delivered inside the
        // ceiling -- and the status says so rather than reporting completion.
        assert_eq!(status.status, NativeStatus::LimitReached);
        assert_eq!(status.limit, Some(LimitKind::Turns));
        assert_eq!(status.queued_input.len(), 1);
    }

    /// Issue #487 (item 3, criterion 4): every provider request reconciles
    /// exactly once, into this route's own pool, and the settlement is
    /// reported beside the running usage so the two can be checked against
    /// each other.
    ///
    /// A two-request turn (a tool call, then the answer) must report two
    /// reconciled requests and a billable total equal to the usage the loop
    /// accumulated -- not double it, which is what folding the same response
    /// twice would produce.
    #[test]
    fn every_provider_request_reconciles_once_into_this_routes_pool() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, mut journal, session) = journal_for(&route);
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, "fixture-anthropic-model"),
            FixtureScript::from_json(
                r#"{"turns":[{"blocks":[{"type":"tool_use","id":"call_1",
                   "name":"Read","input":{"file_path":"a.txt"}}],"finish_reason":"tool_use"},
                   {"blocks":[{"type":"text","text":"done"}],"finish_reason":"end_turn"}]}"#,
            )
            .expect("script"),
        );
        let mut tools = FixtureToolExecutor::new(
            FixtureToolScript::from_json(
                r#"{"tools":{"Read":[{"state":"completed","result":{"ok":true}}]}}"#,
            )
            .expect("tool script"),
        );
        let clock = || 1_000u64;
        let status = {
            let mut driver = NativeLoop::new(
                config_for(session, route),
                &provider,
                &mut tools,
                &mut journal,
                Arc::new(CancellationFlag::default()),
                &clock,
                &no_env,
            );
            driver.acknowledge("go", false).unwrap();
            driver.run_to_completion().unwrap()
        };

        let reconciled = &status.reconciliation;
        assert_eq!(
            reconciled.requests, status.requests as u64,
            "one settlement per provider request, no more and no fewer: {reconciled:?}"
        );
        assert_eq!(
            reconciled.billable_tokens,
            status.usage.input_tokens + status.usage.output_tokens,
            "the settled total matches the usage the loop accumulated"
        );
        assert_eq!(reconciled.unpriced_tokens, 0);
        assert_eq!(
            reconciled.seen.len(),
            reconciled.requests as usize,
            "every request is remembered by its own key, so a replay is a no-op"
        );
    }

    /// Finding 6: a tool this build cannot classify gets the most restrictive
    /// retry contract, never the most permissive one.
    #[test]
    fn an_unclassifiable_tool_is_never_retried() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, mut journal, session) = journal_for(&route);
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, "fixture-anthropic-model"),
            FixtureScript::from_json(
                r#"{"turns":[{"blocks":[{"type":"tool_use","id":"call_unknown",
                   "name":"not_a_zirv_tool","input":{}}],"finish_reason":"tool_use"},
                   {"blocks":[{"type":"text","text":"done"}],"finish_reason":"end_turn"}]}"#,
            )
            .expect("script"),
        );
        // The script would hand back a success on a second attempt; the loop
        // must never ask for one.
        let mut tools = FixtureToolExecutor::new(
            FixtureToolScript::from_json(
                r#"{"tools":{"not_a_zirv_tool":[{"state":"failed","message":"boom"},
                   {"state":"completed","result":{"ok":true}}]}}"#,
            )
            .expect("tool script"),
        );
        let clock = || 1_000u64;
        let outcome = {
            let mut driver = NativeLoop::new(
                config_for(session, route),
                &provider,
                &mut tools,
                &mut journal,
                Arc::new(CancellationFlag::default()),
                &clock,
                &no_env,
            );
            driver.acknowledge("go", false).unwrap();
            driver.run_turn().unwrap()
        };
        assert_eq!(outcome.results[0].retry, RetryPolicy::NeverAfterStart);
        assert_eq!(outcome.results[0].state, ToolState::Failed);
        assert_eq!(outcome.results[0].attempts, 1);
        assert_eq!(tools.calls, vec!["call_unknown"]);
    }

    /// Finding 7: a blocking stop decision outranks a model finish token, so
    /// N15's workflow-gate wiring cannot land without taking effect.
    #[test]
    fn a_blocking_workflow_gate_outranks_the_models_finish_token() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, mut journal, session) = journal_for(&route);
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, "fixture-anthropic-model"),
            script("anthropic-investigate-edit-test.json"),
        );
        let mut tools = FixtureToolExecutor::new(tool_script("tools-investigate-edit-test.json"));
        let clock = || 1_000u64;
        let status = {
            let mut cfg = config_for(session, route);
            cfg.workflow_gate = Some("zirv workflow: the Test step has no fresh evidence".into());
            let mut driver = NativeLoop::new(
                cfg,
                &provider,
                &mut tools,
                &mut journal,
                Arc::new(CancellationFlag::default()),
                &clock,
                &no_env,
            );
            driver.acknowledge("go", false).unwrap();
            driver.run_to_completion().unwrap()
        };
        // The model said `end_turn` and every tool completed; the gate still
        // decides.
        assert_eq!(status.finish_reason.as_deref(), Some("EndTurn"));
        assert!(status.incomplete_tools.is_empty());
        assert_eq!(status.status, NativeStatus::Incomplete);
        assert_eq!(
            status.blocked_reason.as_deref(),
            Some("zirv workflow: the Test step has no fresh evidence")
        );
        assert_ne!(status.exit_code, 0);
    }

    /// Finding 2: a crash leaves an execution durably `Started`. A resume
    /// must reconcile it as outcome-unknown, advance the generation so the
    /// old one is fenced out, and never re-run the effect.
    #[test]
    fn a_resume_reconciles_a_started_execution_and_fences_the_old_generation() {
        use super::super::journal::{PolicyProvenance, ToolCallId};

        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, mut journal, session) = journal_for(&route);
        let scope = EventScope::default();
        let call = ToolCallId::new("call_crashed").unwrap();
        let execution = ExecutionId::new("exec_crashed").unwrap();
        journal
            .prepare_tool_call(
                &session,
                1,
                &scope,
                call.clone(),
                "apply_patch".into(),
                serde_json::json!({"path": "src/lib.rs", "patch": "x"}),
                PolicyProvenance {
                    fingerprint: String::new(),
                    source: "native-loop".into(),
                    decision: "allowed".into(),
                    scope: "worker".into(),
                },
                Some(1),
                1,
            )
            .unwrap();
        journal
            .prepare_execution(
                &session,
                1,
                &scope,
                execution.clone(),
                call.clone(),
                Some(1),
                1,
            )
            .unwrap();
        journal
            .transition_execution(
                &session,
                1,
                &scope,
                &execution,
                ExecutionState::Started,
                None,
                None,
                Some(1),
                1,
            )
            .unwrap();
        // ... and the process dies here, mid-effect.

        let resumed = resume_journal(&mut journal, &session, 2_000).expect("resumed");
        assert_eq!(resumed.previous_generation, 1);
        assert_eq!(resumed.generation, 2);
        assert_eq!(resumed.reconciled, vec![execution.clone()]);

        let state = journal.replay(&session).unwrap();
        assert_eq!(
            state.executions.get(&execution).map(|e| e.state),
            Some(ExecutionState::OutcomeUnknown),
            "an effect that began and never reported is never assumed to have failed"
        );
        assert_eq!(journal.session(&session).unwrap().generation, 2);

        // The old generation is fenced: anything still holding it is refused
        // rather than allowed to keep writing.
        let stale = journal.acknowledge_input(
            &session,
            1,
            &scope,
            MessageId::new("msg_stale").unwrap(),
            "from the dead generation".into(),
            false,
            Some(2_000),
            2,
        );
        assert!(stale.is_err(), "the superseded generation must be fenced");

        // A continued session reports it as outcome-unknown and never re-runs
        // it: the provider is the only thing that can ask for a tool, and it
        // was never asked again.
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, "fixture-anthropic-model"),
            FixtureScript::from_json(
                r#"{"turns":[{"blocks":[{"type":"text","text":"carrying on"}],
                   "finish_reason":"end_turn"}]}"#,
            )
            .unwrap(),
        );
        let mut tools = FixtureToolExecutor::new(tool_script("tools-investigate-edit-test.json"));
        let clock = || 3_000u64;
        let status = {
            let mut cfg = config_for(session, route);
            cfg.generation = resumed.generation;
            let mut driver = NativeLoop::new(
                cfg,
                &provider,
                &mut tools,
                &mut journal,
                Arc::new(CancellationFlag::default()),
                &clock,
                &no_env,
            );
            driver.run_to_completion().unwrap()
        };
        assert_eq!(
            status.outcome_unknown_tools,
            vec!["call_crashed".to_string()]
        );
        assert_eq!(status.status, NativeStatus::Incomplete);
        assert!(tools.calls.is_empty(), "a crashed effect is never replayed");
    }

    /// Review finding on `run_headless` (issue #479 follow-up): `run_
    /// session`'s own doc comment promises its one human line -- a resume's
    /// outcome-unknown reconcile notice -- is routed "somewhere other than
    /// its own single-object stdout" for a `--json` caller. `run_headless`
    /// broke that promise by writing the notice to the SAME writer as the
    /// final status JSON. Seeds a journal session with an execution stuck
    /// `Started` (mid-effect, as if the process had crashed there, exactly
    /// like `a_resume_reconciles_a_started_execution_and_fences_the_old_
    /// generation` above), resumes it end to end through `run_headless`
    /// itself, and asserts the writer it was given holds exactly one
    /// parseable JSON object -- which a leaked notice line ahead of it would
    /// break entirely, since `serde_json::from_slice` accepts no other
    /// content before or after the one value it parses.
    #[test]
    fn a_resume_with_a_reconcile_notice_writes_exactly_one_json_object_to_stdout() {
        use super::super::super::state::StateDir;
        use super::super::journal::PolicyProvenance;

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state_dir = tmp.path().join("state");
        let state = StateDir::from_root(state_dir.clone());
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let session = JournalSessionId::new("native-session-crashed").unwrap();
        {
            let mut journal = Journal::open(&state).expect("open journal");
            journal
                .create_session(&SessionIdentity {
                    session: session.clone(),
                    seat: SeatId::new("seat-crashed").unwrap(),
                    generation: 1,
                    task: None,
                    route: route.clone(),
                    created_at: 1,
                    completed_at: None,
                })
                .unwrap();
            let scope = EventScope::default();
            let call = ToolCallId::new("call_crashed").unwrap();
            let execution = ExecutionId::new("exec_crashed").unwrap();
            journal
                .prepare_tool_call(
                    &session,
                    1,
                    &scope,
                    call.clone(),
                    "apply_patch".into(),
                    serde_json::json!({"path": "src/lib.rs", "patch": "x"}),
                    PolicyProvenance {
                        fingerprint: String::new(),
                        source: "native-loop".into(),
                        decision: "allowed".into(),
                        scope: "worker".into(),
                    },
                    Some(1),
                    1,
                )
                .unwrap();
            journal
                .prepare_execution(
                    &session,
                    1,
                    &scope,
                    execution.clone(),
                    call.clone(),
                    Some(1),
                    1,
                )
                .unwrap();
            journal
                .transition_execution(
                    &session,
                    1,
                    &scope,
                    &execution,
                    ExecutionState::Started,
                    None,
                    None,
                    Some(1),
                    1,
                )
                .unwrap();
            // ... and the process dies here, mid-effect. The journal is
            // closed (end of this block) with the execution still `Started`.
        }

        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state_dir.to_str().expect("utf8").to_string(),
        )]
        .into();
        let lookup = |k: &str| env.get(k).cloned();

        let fixtures = fixture_root();
        let provider = format!(
            "fixture:{}",
            fixtures.join("resume-continue.json").display()
        );
        let mut request = HeadlessRequest {
            repo: repo.path(),
            prompt: "",
            route: None,
            role: "worker",
            limits: NativeLimits::default(),
            resume: Some("native-session-crashed"),
            provider: Some(&provider),
            fixture_tools: None,
            task: None,
            writer: None,
        };
        let mut out: Vec<u8> = Vec::new();
        run_headless(&mut request, &mut out, &lookup).expect("resumed run");

        let value: serde_json::Value = serde_json::from_slice(&out).unwrap_or_else(|error| {
            panic!(
                "stdout must be exactly one JSON object, never mixed with the reconcile \
                 notice: {error}: {}",
                String::from_utf8_lossy(&out)
            )
        });
        assert!(value.get("status").is_some(), "{value}");

        // The reconcile really happened -- this is not a vacuous pass.
        let journal = Journal::open(&state).expect("reopen journal");
        let replayed = journal.replay(&session).expect("replay");
        let execution = ExecutionId::new("exec_crashed").unwrap();
        assert_eq!(
            replayed
                .executions
                .get(&execution)
                .map(|record| record.state),
            Some(ExecutionState::OutcomeUnknown),
            "the resume must have reconciled the started execution"
        );
    }

    /// Finding 2, the other half: a resume through the backend seam does the
    /// same durable work, not just an in-memory counter bump.
    #[test]
    fn a_backend_resume_with_a_journal_advances_the_stored_generation() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, journal, session) = journal_for(&route);
        let mut backend = NativeBackend::new();
        let handle = backend.start(&spec(RuntimeKind::Native)).expect("started");
        backend.attach_journal(journal);
        backend.adopt(&handle, session.clone()).expect("adopted");

        let resumed = backend.resume(&handle, None).expect("resumed");
        assert_eq!(resumed.generation, 2);
        assert_eq!(
            backend
                .journal_mut()
                .unwrap()
                .session(&session)
                .unwrap()
                .generation,
            2,
            "the backend's generation must be the journal's, not a private counter"
        );
    }

    /// Finding 3: an input the backend accepted is durable before the caller
    /// is told it was accepted, so a crash straight after `Ok` cannot lose it.
    #[test]
    fn backend_input_is_journalled_before_it_is_reported_as_accepted() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, journal, session) = journal_for(&route);
        let mut backend = NativeBackend::new();
        let handle = backend.start(&spec(RuntimeKind::Native)).expect("started");
        backend.attach_journal(journal);
        backend.adopt(&handle, session.clone()).expect("adopted");

        backend.submit(&handle, "do the thing").expect("submitted");
        backend.steer(&handle, "and this too").expect("steered");
        let resumed = backend
            .resume(&handle, Some("carry on"))
            .expect("resumed with input");

        let state = backend
            .journal_mut()
            .unwrap()
            .replay(&session)
            .expect("replay");
        let inputs: Vec<(String, bool)> = state
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .map(|message| (message.text.clone().unwrap_or_default(), message.steering))
            .collect();
        assert_eq!(
            inputs,
            vec![
                ("do the thing".to_string(), false),
                ("and this too".to_string(), true),
                ("carry on".to_string(), false),
            ]
        );
        // The resume's own input is acknowledged against the NEW generation.
        assert_eq!(resumed.generation, 2);
    }

    /// A backend with no journal is still a usable in-memory protocol surface
    /// -- accepting input is a no-op there, never an error.
    #[test]
    fn a_backend_without_a_journal_still_accepts_input() {
        let mut backend = NativeBackend::new();
        let handle = backend.start(&spec(RuntimeKind::Native)).expect("started");
        backend
            .submit(&handle, "in memory only")
            .expect("submitted");
        backend.steer(&handle, "also in memory").expect("steered");
        assert_eq!(backend.state(&handle), Some(SessionState::Running));
    }

    #[test]
    fn the_final_status_serializes_with_its_schema_version_and_actual_route() {
        let (status, _) = run_fixture(
            Protocol::OpenAiResponses,
            "fixture-openai-model",
            "openai-investigate-edit-test.json",
            "tools-investigate-edit-test.json",
            "go",
            |_| {},
        );
        let json = serde_json::to_value(&status).expect("serializable");
        assert_eq!(json["schema_version"], FINAL_STATUS_SCHEMA_VERSION);
        assert_eq!(json["runtime"], "native");
        assert_eq!(json["status"], "completed");
        assert_eq!(json["provider"], "openai");
        assert_eq!(json["served_model"], "fixture-openai-model");
        assert!(json["usage"]["output_tokens"].as_u64().unwrap() > 0);
    }

    // -- (n) issue #486: compaction, rot recovery and checkpoints ---------

    /// A compaction run, with everything an assertion needs: the journal is
    /// returned rather than dropped so a test can prove the ORIGINAL history
    /// is still there, and the provider's sent requests so a test can prove
    /// what the model actually saw after the compaction.
    struct CompactionRun {
        status: NativeFinalStatus,
        calls: Vec<String>,
        sent: Vec<ProviderRequest>,
        journal: Journal,
        session: JournalSessionId,
        _dir: tempfile::TempDir,
    }

    fn run_compaction_fixture(
        provider_fixture: &str,
        prompt: &str,
        mutate: impl FnOnce(&mut NativeSessionConfig),
    ) -> CompactionRun {
        let model = "fixture-anthropic-model";
        let route = route_for(Protocol::AnthropicMessages, model);
        let (dir, mut journal, session) = journal_for(&route);
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, model),
            script(provider_fixture),
        );
        let mut tools = FixtureToolExecutor::new(tool_script("tools-investigate-edit-test.json"));
        let mut cfg = config_for(session.clone(), route);
        mutate(&mut cfg);
        let clock = || 1_000u64;
        let status = {
            let mut driver = NativeLoop::new(
                cfg,
                &provider,
                &mut tools,
                &mut journal,
                Arc::new(CancellationFlag::default()),
                &clock,
                &no_env,
            );
            driver.acknowledge(prompt, false).expect("acknowledged");
            driver.run_to_completion().expect("ran")
        };
        CompactionRun {
            status,
            calls: tools.calls,
            sent: provider.sent(),
            journal,
            session,
            _dir: dir,
        }
    }

    fn first_text(request: &ProviderRequest) -> String {
        request
            .messages
            .first()
            .map(|message| {
                message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ProviderContent::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>()
            })
            .unwrap_or_default()
    }

    /// Acceptance criterion: a long session crosses a context window while
    /// preserving the hard user constraints, the objective, the verification
    /// state and the exact evidence references -- and without destroying the
    /// original history.
    #[test]
    fn a_long_session_compacts_on_token_pressure_and_keeps_its_objective_constraints_and_history() {
        let run = run_compaction_fixture(
            "compaction-long-session.json",
            "fix the failing test",
            |cfg| {
                cfg.compaction.budget = NativeBudget {
                    context_window_tokens: Some(1_000),
                    output_reserve_tokens: 200,
                };
                cfg.compaction.retain_recent_messages = 2;
                cfg.compaction.constraints = vec!["never force-push".to_string()];
            },
        );

        assert_eq!(run.status.status, NativeStatus::Completed);
        assert_eq!(run.status.compactions.len(), 1);
        assert!(
            run.status.compactions[0].reason.contains("token_pressure"),
            "reason was {:?}",
            run.status.compactions[0].reason
        );
        // The distillation really went through the native route.
        assert_eq!(run.status.compactions[0].summary_source, "route");

        // The last request the model saw opens with the compaction briefing,
        // carrying the objective and the operator's hard constraint.
        let summary = first_text(run.sent.last().expect("a request was sent"));
        assert!(summary.contains("[zirv compaction]"), "{summary}");
        assert!(summary.contains("never force-push"), "{summary}");
        assert!(summary.contains("fix the failing test"), "{summary}");
        // Completed actions keep their exact tool-call identities.
        assert!(summary.contains("call_1"), "{summary}");

        // The distillation request carried NO tool schemas: read-only is
        // enforced by giving the model nothing to call.
        let distill = run
            .sent
            .iter()
            .find(|request| {
                request
                    .system
                    .iter()
                    .any(|text| text.contains("compacting"))
            })
            .expect("a distillation request was sent");
        assert!(distill.tools.is_empty());

        // Nothing was destroyed: the original acknowledged input and every
        // original event are still in the journal.
        let events = run.journal.events(&run.session).expect("events");
        assert!(events.iter().any(|stored| matches!(
            &stored.event,
            JournalEvent::InputAcknowledged { text, .. } if text == "fix the failing test"
        )));
        assert!(events.iter().any(|stored| matches!(
            &stored.event,
            JournalEvent::Checkpoint {
                kind: CheckpointKind::Compaction,
                ..
            }
        )));
    }

    /// Acceptance criterion: an overflow recovers from a valid checkpoint
    /// without repeating any effect.
    #[test]
    fn a_context_overflow_recovers_through_a_compaction_without_repeating_an_effect() {
        let run = run_compaction_fixture(
            "compaction-overflow-recovery.json",
            "fix the failing test",
            |cfg| cfg.compaction.retain_recent_messages = 2,
        );
        assert_eq!(run.status.status, NativeStatus::Completed);
        assert_eq!(run.status.compactions.len(), 1);
        assert!(
            run.status.compactions[0]
                .reason
                .contains("context_overflow"),
            "reason was {:?}",
            run.status.compactions[0].reason
        );
        // Every effect ran exactly once: the overflow committed nothing, so
        // the rebuilt request replays results rather than re-running tools.
        assert_eq!(run.calls, vec!["call_1", "call_2", "call_3"]);
    }

    /// An advisory policy reports and does nothing. The narrowing has to be
    /// load-bearing or it is decorative.
    #[test]
    fn an_advisory_policy_reports_the_pressure_and_never_compacts() {
        let run = run_compaction_fixture(
            "compaction-long-session.json",
            "fix the failing test",
            |cfg| {
                cfg.compaction.budget = NativeBudget {
                    context_window_tokens: Some(1_000),
                    output_reserve_tokens: 200,
                };
                cfg.compaction.retain_recent_messages = 2;
                cfg.compaction.policy = CompactionPolicy::Advisory;
            },
        );
        assert!(run.status.compactions.is_empty());
        let decision = run
            .status
            .compaction_decision
            .as_ref()
            .expect("a decision was recorded");
        assert_eq!(decision.policy, CompactionPolicy::Advisory);
        assert!(
            run.status
                .evidence
                .iter()
                .any(|note| note.kind == "compaction_advice")
        );
        // Nothing was written: no checkpoint event exists at all.
        let events = run.journal.events(&run.session).expect("events");
        assert!(
            !events
                .iter()
                .any(|stored| matches!(&stored.event, JournalEvent::Checkpoint { .. }))
        );
    }

    /// Acceptance criterion: a crash mid-compaction resumes from the last
    /// VALID checkpoint. A checkpoint this build cannot read is skipped in
    /// favour of an older one it can, never repaired and never fatal.
    #[test]
    fn an_unreadable_newer_checkpoint_falls_back_to_the_last_valid_one() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, mut journal, session) = journal_for(&route);
        acknowledge_input(
            &mut journal,
            &session,
            1,
            MessageId::new("m1").expect("id"),
            "go",
            false,
            1_000,
        )
        .expect("input");
        let state = journal.replay(&session).expect("replay");
        let good = checkpoint::build(
            &state,
            SequenceId(1),
            SequenceId(1),
            &CheckpointId::new("cp-good").expect("id"),
            &checkpoint::CheckpointContext {
                reason: "token_pressure".to_string(),
                ..Default::default()
            },
            compaction::structural_summary(&state, SequenceId(1)),
            1,
        );
        checkpoint::commit(
            &mut journal,
            None,
            1,
            &EventScope::default(),
            CheckpointKind::Compaction,
            &good,
            1,
        )
        .expect("committed");
        // A newer checkpoint written by a schema this build does not know.
        journal
            .record_checkpoint(
                &session,
                1,
                &EventScope::default(),
                CheckpointId::new("cp-future").expect("id"),
                CheckpointKind::Compaction,
                serde_json::json!({ "schema_version": 9999, "unknown": true }),
                2,
            )
            .expect("recorded");

        let active = compaction::active(&journal, &session)
            .expect("active")
            .expect("a valid checkpoint remains");
        assert_eq!(active.checkpoint_id, "cp-good");
        assert_eq!(active.reason, "token_pressure");
    }

    /// A crash between the portable export and the journal event leaves an
    /// orphan file. The journal is the commit point, so the session is simply
    /// uncompacted -- never half-compacted.
    #[test]
    fn a_portable_export_without_its_journal_event_is_not_a_compaction() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (dir, mut journal, session) = journal_for(&route);
        acknowledge_input(
            &mut journal,
            &session,
            1,
            MessageId::new("m1").expect("id"),
            "go",
            false,
            1_000,
        )
        .expect("input");
        let state = journal.replay(&session).expect("replay");
        let orphan = checkpoint::build(
            &state,
            SequenceId(1),
            SequenceId(1),
            &CheckpointId::new("cp-orphan").expect("id"),
            &checkpoint::CheckpointContext::default(),
            compaction::structural_summary(&state, SequenceId(1)),
            1,
        );
        let state_dir = crate::commands::ctx::state::StateDir::from_root(dir.path().join("state"));
        checkpoint::export(&state_dir, &orphan).expect("exported");
        assert!(
            compaction::active(&journal, &session)
                .expect("active")
                .is_none()
        );
    }

    /// Acceptance criterion: a same-route resume keeps the provider's opaque
    /// continuation state; a route change rebuilds a legal semantic history
    /// instead, with no hidden reasoning and no synthesized outcome.
    #[test]
    fn a_route_change_discards_the_opaque_envelope_and_rebuilds_the_history() {
        let route = route_for(Protocol::AnthropicMessages, "fixture-anthropic-model");
        let (_dir, mut journal, session) = journal_for(&route);
        acknowledge_input(
            &mut journal,
            &session,
            1,
            MessageId::new("m1").expect("id"),
            "keep this",
            false,
            1_000,
        )
        .expect("input");

        match compaction::plan_continuation(&journal, &session, &route).expect("planned") {
            compaction::ContinuationPlan::SameRoute { .. } => {}
            other => panic!("same route must keep the envelope, got {other:?}"),
        }

        let other = route_for(Protocol::OpenAiResponses, "fixture-openai-model");
        match compaction::plan_continuation(&journal, &session, &other).expect("planned") {
            compaction::ContinuationPlan::Rebuilt { messages, .. } => {
                let rendered = format!("{messages:?}");
                assert!(rendered.contains("keep this"), "{rendered}");
            }
            other => panic!("a route change must rebuild, got {other:?}"),
        }
    }

    /// Acceptance criterion: existing rot scoring regressions keep passing,
    /// and equivalent projected events give the same verdict. The projection
    /// is checked here at the seam it actually runs at -- a real journal.
    #[test]
    fn the_journal_projection_scores_deterministically_through_the_pure_engine() {
        let run = run_compaction_fixture(
            "compaction-long-session.json",
            "fix the failing test",
            |cfg| {
                cfg.compaction.budget = NativeBudget {
                    context_window_tokens: Some(1_000),
                    output_reserve_tokens: 200,
                };
                cfg.compaction.retain_recent_messages = 2;
            },
        );
        let first = compaction::observe(&run.journal, &run.session, 0).expect("observed");
        let second = compaction::observe(&run.journal, &run.session, 0).expect("observed");
        assert_eq!(first, second);
        let cfg = crate::commands::ctx::config::ScoreConfig::default();
        let budget = NativeBudget {
            context_window_tokens: Some(1_000),
            output_reserve_tokens: 200,
        };
        assert_eq!(
            compaction::evaluate(&first, &cfg, budget, CompactionPolicy::Automatic),
            compaction::evaluate(&second, &cfg, budget, CompactionPolicy::Automatic)
        );
    }
}
