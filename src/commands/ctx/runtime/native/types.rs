//! Shared types: the session/turn/request/tool state machine, the tool
//! seam, scheduling and the result/status types every backend below uses.

use std::path::PathBuf;

use serde::Serialize;

use super::super::super::CtxResult;
use super::super::super::config::OrchestratorWrites;
use super::super::super::provider::adapter::{CacheMode, FinishReason, ProviderUsage};
use super::super::compaction::{
    CompactionDecision, CompactionPolicy, CompactionRecord, DistillBudget, NativeBudget,
    RETAIN_RECENT_MESSAGES,
};
use super::super::journal::{
    ExecutionId, ExecutionState, JournalSessionId, RouteIdentity, ToolCallId, TurnId,
};
use super::super::tools::{
    NativeToolClient, ResourceClaimKind, RetryPolicy, ToolDefinition, ToolExecutionMode,
    ToolReceipt,
};

/// Bumped whenever [`NativeFinalStatus`]'s own shape changes. A consumer of
/// `zirv ctx exec --runtime native --json` branches on this, never on field
/// presence.
pub const FINAL_STATUS_SCHEMA_VERSION: u32 = 3;

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
    /// Token ceiling for the whole run (issue #637; mirrors `--budget-tokens`
    /// on the harness path, `exec::ExecArgs::budget_tokens`). Checkpoints
    /// once at `agent::BUDGET_SOFT_FRACTION` of the ceiling (an evidence
    /// note, not a stop) and stops at the ceiling itself. `None` is
    /// unbounded, the default -- a plain `zirv ctx exec --runtime native`
    /// with no `--budget-tokens` must run exactly as before.
    pub max_budget_tokens: Option<u64>,
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
            max_budget_tokens: None,
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
    /// Native token budget supplied by `--budget-tokens`. (#637)
    Tokens,
}

impl LimitKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LimitKind::Turns => "turns",
            LimitKind::RequestsPerTurn => "requests_per_turn",
            LimitKind::ToolCalls => "tool_calls",
            LimitKind::WallClock => "wall_clock",
            LimitKind::Tokens => "tokens",
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
    /// Never started and will not start.
    Cancelled,
    /// Started, outcome unknown. Requires reconciliation before any retry.
    OutcomeUnknown,
}

impl ToolState {
    pub(super) fn journal_state(self) -> ExecutionState {
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
///
/// Issue #480 (roadmap N11): `Send` so a `Box<dyn ToolExecutor>` can move
/// into the background thread [`spawn_interactive`] drives a dashboard
/// native pane's turns on -- an unbounded trait object is not `Send`
/// automatically, only a trait declared with the bound is, and every real
/// implementor (`ClientToolExecutor`, `fixture::FixtureToolExecutor`) already
/// was.
pub trait ToolExecutor: std::fmt::Debug + Send {
    /// The tool definitions the provider is told about.
    fn definitions(&self) -> Vec<ToolDefinition>;
    /// Performs one call. Returning a receipt is not an admission that the
    /// effect succeeded: `ToolReceiptState` carries that, and
    /// `ToolReceiptState::OutcomeUnknown` is how an executor says it cannot
    /// tell.
    fn execute(&mut self, call: &NativeToolCall) -> ToolReceipt;

    fn generation_lease(
        &self,
    ) -> CtxResult<Option<Box<dyn super::super::enforcement::GenerationLease>>> {
        Ok(None)
    }

    fn execute_with_generation_lease(&mut self, call: &NativeToolCall) -> ToolReceipt {
        self.execute(call)
    }
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

    fn generation_lease(
        &self,
    ) -> CtxResult<Option<Box<dyn super::super::enforcement::GenerationLease>>> {
        self.client.lock_generation().map(Some).map_err(Into::into)
    }

    fn execute_with_generation_lease(&mut self, call: &NativeToolCall) -> ToolReceipt {
        self.client
            .execute_unfenced(&call.name, call.arguments.clone(), None, None)
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
pub(super) fn is_independent(definition: Option<&ToolDefinition>) -> bool {
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
            NativeStatus::Incomplete => super::super::super::exec::EXIT_CONTRACT_FAILED,
            NativeStatus::Interrupted => 130,
            NativeStatus::LimitReached => super::super::super::exec::EXIT_BUDGET_EXHAUSTED,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<super::super::execution::ExecutionObservation>,
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
    /// Settled spend counts each provider request once, split into billable and unpriced usage. (#487)
    pub reconciliation: super::super::super::route::Reconciliation,
    pub finish_reason: Option<String>,
    pub final_text: Option<String>,
    pub incomplete_tools: Vec<String>,
    pub outcome_unknown_tools: Vec<String>,
    pub queued_input: Vec<String>,
    pub limit: Option<LimitKind>,
    pub failure: Option<String>,
    /// Pure route failure scope used by the durable breaker; the loop writes no health record itself. (#487, #554)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_routing: Option<super::super::super::route::FailureRouting>,
    pub blocked_reason: Option<String>,
    /// Committed compactions are ordered oldest first and name verifiable journal sequences. (#486)
    pub compactions: Vec<CompactionRecord>,
    pub compaction_decision: Option<CompactionDecision>,
    pub evidence: Vec<NativeEvidence>,
    pub exit_code: i32,
}

/// Carries already-billed work through a hard loop failure so every caller
/// can settle it before propagating the error. (#554)
#[derive(Debug)]
pub struct AbortedRun {
    /// What this loop had already billed when it failed. Boxed because a
    /// full final status dwarfs the success value it shares a `Result` with,
    /// and an abort is the rare path.
    pub status: Box<NativeFinalStatus>,
    pub error: String,
}

impl std::fmt::Display for AbortedRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.error)
    }
}

impl std::error::Error for AbortedRun {}

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
    pub task: Option<super::super::journal::TaskId>,
    /// A workflow gate that refuses completion, with its own message. Fed
    /// straight into the shared stop service, where it outranks any model
    /// finish token.
    ///
    /// A fixed override, used by tests and by a caller that has already
    /// decided. Production leaves it `None` and sets [`Self::workflow_repo`]
    /// instead, so the gate is read LIVE at every completion attempt rather
    /// than snapshotted before the session had done anything.
    pub workflow_gate: Option<String>,
    /// Default compaction policy works without caller overrides; unknown capacity stays unknown. (#486)
    pub compaction: CompactionSettings,
    /// Repository whose active workflow gates completion; `None` leaves the session ungated. (#484)
    pub workflow_repo: Option<PathBuf>,
    /// Stable, cacheable system instructions compile once; the live workflow gate is checked at every completion. (#484)
    pub system: Vec<String>,
    /// Repository context is untrusted data delivered as a user message, never as system instructions. (#484)
    pub preamble: Vec<String>,
    /// Prompt-cache mode for the provider request's stable prefix; `Disabled` unless opted in.
    pub prompt_cache: CacheMode,
}

/// Opt-in `[runtime].prompt_cache_ttl` for native routes whose API supports prompt-cache TTLs
/// (Anthropic only); every other provider and the unset default stay `Disabled` (#766).
pub fn prompt_cache_for(cfg: &super::super::super::config::CtxConfig, provider: &str) -> CacheMode {
    if provider != "anthropic" {
        return CacheMode::Disabled;
    }
    match cfg.runtime.prompt_cache_ttl.as_deref() {
        Some("1h") => CacheMode::Ephemeral1h,
        Some("5m") => CacheMode::Ephemeral5m,
        _ => CacheMode::Disabled,
    }
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
    pub score: super::super::super::config::ScoreConfig,
    pub distill: DistillBudget,
    /// How many of the newest messages a compaction always leaves verbatim.
    pub retain_recent_messages: usize,
    /// Operator-stated hard constraints, carried into every checkpoint from
    /// the same typed source `runtime::context::CompileRequest` reads.
    pub constraints: Vec<String>,
    /// Where the portable checkpoint export is written. `None` keeps the
    /// journal event as the only copy, which is all a resume needs.
    pub state: Option<super::super::super::state::StateDir>,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            policy: CompactionPolicy::default(),
            budget: NativeBudget::default(),
            score: super::super::super::config::ScoreConfig::default(),
            distill: DistillBudget::default(),
            retain_recent_messages: RETAIN_RECENT_MESSAGES,
            constraints: Vec::new(),
            state: None,
        }
    }
}

/// Everything [`NativeLoop::recompile_if_scope_changed`] needs that is not
/// already on `config`/`journal` (issue #538, chunk C). Owned, not borrowed:
/// `NativeLoop` already carries enough lifetimes, and none of these are hot
/// enough to justify adding another.
#[derive(Clone, Debug)]
pub struct RecompileContext {
    pub state: super::super::super::state::StateDir,
    pub home: PathBuf,
    pub cfg: super::super::super::config::CtxConfig,
    pub repo: PathBuf,
}

#[cfg(test)]
mod tests {
    #[test]
    fn prompt_cache_is_opt_in_and_anthropic_only() {
        let mut cfg = super::super::super::super::config::CtxConfig::default();
        assert_eq!(
            super::prompt_cache_for(&cfg, "anthropic"),
            super::CacheMode::Disabled
        );
        cfg.runtime.prompt_cache_ttl = Some("1h".to_string());
        assert_eq!(
            super::prompt_cache_for(&cfg, "anthropic"),
            super::CacheMode::Ephemeral1h
        );
        assert_eq!(
            super::prompt_cache_for(&cfg, "bedrock"),
            super::CacheMode::Disabled
        );
        cfg.runtime.prompt_cache_ttl = Some("5m".to_string());
        assert_eq!(
            super::prompt_cache_for(&cfg, "anthropic"),
            super::CacheMode::Ephemeral5m
        );
    }

    use super::super::super::super::provider::Protocol;
    use super::super::turn::run_fixture;
    use super::*;

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
}
