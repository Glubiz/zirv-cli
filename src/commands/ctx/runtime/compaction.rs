//! Native token-aware compaction and context-rot recovery (issue #486,
//! roadmap N17).
//!
//! Three separable pieces, in the order a session meets them:
//!
//! 1. **Observation.** Committed journal facts and the provider's own
//!    measured context usage are PROJECTED into the existing
//!    `NormalizedEvent` vocabulary and scored by the existing pure
//!    [`rot`](super::super::rot) engine. `rot.rs` is not touched and not
//!    extended: the projection is an adapter at exactly the same boundary as
//!    every external harness transcript parser, so equivalent projected
//!    events give identical verdicts and every existing scoring regression
//!    keeps passing unchanged.
//!
//!    The one thing the projection does NOT copy is the transcript-scoring
//!    token thresholds. A native session knows two things a transcript never
//!    does: the model's declared context window and the output reservation
//!    this run actually asked for. So the token gate is fed
//!    `window - output_reserve` as the capacity, and the measurement is the
//!    provider's own reported input footprint -- fresh input, cache writes
//!    and cache reads together, because a cached prefix still occupies the
//!    window.
//!
//! 2. **Decision.** [`evaluate`] is pure and returns explicitly TYPED
//!    triggers ([`CompactionTrigger`]) rather than one opaque verdict, so a
//!    status reader and a test can both say *why*. Whether a `Compact`
//!    action is taken automatically or only reported is
//!    [`CompactionPolicy`], which a repository checkout may narrow to
//!    `advisory` and never widen.
//!
//! 3. **Commit.** A compaction is one [`checkpoint::PortableCheckpoint`]
//!    committed through [`checkpoint::commit`]. It REPLACES NOTHING: the
//!    journal keeps every original event and every stored artifact, and the
//!    summary is only what the next provider request sends in place of the
//!    covered prefix. Compaction therefore cannot destroy history, cannot
//!    mark a pending action complete (the boundary never crosses an
//!    unsettled tool call) and cannot drop acknowledged input (every
//!    acknowledged input is carried in the checkpoint, and undelivered input
//!    stays after the boundary).

use serde::{Deserialize, Serialize};

use super::super::CtxResult;
use super::super::config::ScoreConfig;
use super::super::event::{Capabilities, NormalizedEvent, ProviderErrorClass};
use super::super::provider::adapter::{
    Cancellation, ProviderAdapter, ProviderContent, ProviderMessage, ProviderMessageRole,
    ProviderRequest, ProviderUsage,
};
use super::super::rot::{self, Verdict};
use super::checkpoint::{self, DistilledSummary, PortableCheckpoint};
use super::journal::{
    CheckpointKind, ConversationState, Journal, JournalEvent, JournalSessionId, MessageRole,
    SequenceId,
};

/// How many of the newest messages a compaction always leaves verbatim.
pub const RETAIN_RECENT_MESSAGES: usize = 4;

/// How much of the covered conversation is handed to a distiller, and how
/// much it may write back. Both are hard bounds: a distillation that would
/// cost more than the compaction saves is not worth making.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DistillBudget {
    pub max_input_chars: usize,
    pub max_output_tokens: u64,
}

impl Default for DistillBudget {
    fn default() -> Self {
        Self {
            max_input_chars: 24_000,
            max_output_tokens: 1_024,
        }
    }
}

/// Whether zirv may compact a native session on its own, or only say that it
/// should be compacted.
///
/// NARROWING ONLY. `automatic` is the default because compaction is how a
/// native session survives a long task at all; a repository checkout may
/// narrow it to `advisory` (zirv reports and does nothing), never widen it.
/// See `provider::config::NativeConfig` for the fold and README's trust
/// boundary for the statement of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompactionPolicy {
    #[default]
    Automatic,
    Advisory,
}

impl CompactionPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::Advisory => "advisory",
        }
    }

    /// The repo-narrowing fold: a checkout may only make zirv act LESS on its
    /// own, so `advisory` from either layer wins.
    pub fn narrow(operator: Self, repo: Option<Self>) -> Self {
        match (operator, repo) {
            (_, Some(Self::Advisory)) | (Self::Advisory, _) => Self::Advisory,
            _ => Self::Automatic,
        }
    }
}

/// The capacity half of the native token gate: what the model can hold, and
/// what this run reserved for its own output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NativeBudget {
    /// The model's declared context window. `None` means unknown, which
    /// `rot::token_gates` reads as "use the absolute fallbacks" -- never as a
    /// guess.
    pub context_window_tokens: Option<u64>,
    pub output_reserve_tokens: u64,
}

impl NativeBudget {
    /// The input capacity actually available: the window less the output
    /// reservation. `None` when the window is unknown; `None` too when the
    /// reservation swallows the whole window, which is a misconfiguration
    /// this must not paper over with a fabricated number.
    pub fn usable_input_tokens(self) -> Option<u64> {
        self.context_window_tokens
            .and_then(|window| window.checked_sub(self.output_reserve_tokens))
            .filter(|usable| *usable > 0)
    }

    /// The `Capabilities` the pure rot engine sees. Only signals a native
    /// session really feeds are claimed: there is no injected reply marker,
    /// so `marker_signal` stays false and the marker component contributes
    /// nothing rather than a fabricated zero-miss rate.
    pub fn capabilities(self) -> Capabilities {
        Capabilities {
            marker_signal: false,
            token_usage: true,
            turn_signal: true,
            system_prompt: true,
            events: true,
            context_window_tokens: self.usable_input_tokens(),
            ..Capabilities::default()
        }
    }
}

/// Why a compaction is being proposed. Typed, ordered by severity, and
/// reported verbatim to status so a reader never has to infer the cause from
/// a score.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    /// The provider itself refused the request for exceeding the window.
    ContextOverflow,
    /// Measured input tokens reached the ceiling derived from the route's
    /// window and this run's output reservation.
    TokenPressure,
    /// The same normalized tool-result error text repeated: different fixes,
    /// one unchanged error.
    RepeatedIdenticalErrors,
    /// The same tool call with the same input repeated: the session is going
    /// round rather than forward.
    LossOfProgress,
}

impl CompactionTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContextOverflow => "context_overflow",
            Self::TokenPressure => "token_pressure",
            Self::RepeatedIdenticalErrors => "repeated_identical_errors",
            Self::LossOfProgress => "loss_of_progress",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionAction {
    None,
    Advise,
    Compact,
}

impl CompactionAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Advise => "advise",
            Self::Compact => "compact",
        }
    }
}

/// What the projection saw. Separated from [`evaluate`] so observation (I/O)
/// and action (pure) never mix.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NativeObservation {
    pub events: Vec<NormalizedEvent>,
    /// The provider's own reported input footprint of the newest request:
    /// fresh input plus cache writes plus cache reads. `0` means no usage was
    /// ever reported, never "the context is empty".
    pub measured_input_tokens: u64,
    /// Whether that figure came from the provider or from an estimate.
    pub estimated: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CompactionDecision {
    pub action: CompactionAction,
    pub policy: CompactionPolicy,
    pub triggers: Vec<CompactionTrigger>,
    pub verdict: Verdict,
    pub score: u32,
    pub measured_input_tokens: u64,
    pub usable_input_tokens: Option<u64>,
    /// The measured-token level at which pressure alone forces a compaction.
    pub compact_at_tokens: u64,
    pub estimated: bool,
    pub reason: String,
}

impl CompactionDecision {
    pub fn should_compact(&self) -> bool {
        self.action == CompactionAction::Compact
    }
}

/// Projects committed journal facts plus measured usage into the scoring
/// vocabulary.
///
/// `overflows` is the number of provider context-overflow refusals this
/// session has seen. Provider failures are not journal facts (a refused
/// request commits nothing), so the loop counts them and hands the count in
/// here rather than inventing a journal event for something that never
/// happened to the conversation.
pub fn observe(
    journal: &Journal,
    session: &JournalSessionId,
    overflows: usize,
) -> CtxResult<NativeObservation> {
    let mut events = journal.normalized_events(session)?;
    for _ in 0..overflows {
        events.push(NormalizedEvent::ProviderError {
            class: ProviderErrorClass::Overflow,
            at: None,
            id: None,
        });
    }
    let (measured_input_tokens, estimated) = measured_context(journal, session)?;
    Ok(NativeObservation {
        events,
        measured_input_tokens,
        estimated,
    })
}

/// The input footprint of the newest CONVERSATION request.
///
/// Two deliberate choices:
///
/// * It is not `UsageRecord::input_tokens` alone. On every protocol that
///   reports caching, the cached prefix is billed separately but still
///   occupies the window, so scoring on fresh input only would under-read a
///   long cached session by exactly the part most likely to overflow.
/// * It is keyed off the newest committed ASSISTANT MESSAGE's usage, not off
///   the newest usage row. A distillation records its own usage in the same
///   journal (compaction must not be an invisible spend) and that request's
///   input is deliberately small; reading it as "the context" would make a
///   session look like it had just shrunk when it had not.
fn measured_context(journal: &Journal, session: &JournalSessionId) -> CtxResult<(u64, bool)> {
    let mut totals: std::collections::BTreeMap<super::journal::UsageId, (u64, bool)> =
        std::collections::BTreeMap::new();
    let mut measured = (0u64, false);
    for stored in journal.events(session)? {
        match stored.event {
            JournalEvent::UsageRecorded { usage } => {
                let total = usage
                    .input_tokens
                    .saturating_add(usage.cache_creation_input_tokens)
                    .saturating_add(usage.cache_read_input_tokens);
                totals.insert(usage.id, (total, usage.estimated));
            }
            JournalEvent::AssistantMessageCommitted {
                usage: Some(usage), ..
            } => {
                if let Some((total, estimated)) = totals.get(&usage)
                    && *total > 0
                {
                    measured = (*total, *estimated);
                }
            }
            _ => {}
        }
    }
    Ok(measured)
}

/// The decision. PURE: no fs, clock, env or net, and no hidden state -- the
/// same observation, config, budget and policy always give the same decision.
pub fn evaluate(
    observation: &NativeObservation,
    cfg: &ScoreConfig,
    budget: NativeBudget,
    policy: CompactionPolicy,
) -> CompactionDecision {
    let caps = budget.capabilities();
    let signals = rot::signals(&observation.events, caps, cfg);
    let score = rot::score_from(
        signals.clone(),
        observation.measured_input_tokens,
        cfg,
        caps,
    );
    let (_, ceiling) = rot::token_gates(cfg, caps);

    let mut triggers = Vec::new();
    if signals.provider_overflows > 0 {
        triggers.push(CompactionTrigger::ContextOverflow);
    }
    if observation.measured_input_tokens >= ceiling {
        triggers.push(CompactionTrigger::TokenPressure);
    }
    if cfg.same_error_threshold > 0 && signals.same_error_repeats >= cfg.same_error_threshold {
        triggers.push(CompactionTrigger::RepeatedIdenticalErrors);
    }
    if cfg.repetition_threshold > 0 && signals.max_repeat >= cfg.repetition_threshold {
        triggers.push(CompactionTrigger::LossOfProgress);
    }
    triggers.sort_unstable();
    triggers.dedup();

    // Overflow and token pressure are capacity facts: they do not get better
    // by waiting. The two behavioural triggers, and the weighted score on its
    // own, are advice until the rot engine's own gate says otherwise.
    let forced = triggers.iter().any(|trigger| {
        matches!(
            trigger,
            CompactionTrigger::ContextOverflow | CompactionTrigger::TokenPressure
        )
    });
    let mut action = if forced || matches!(score.verdict, Verdict::Compact | Verdict::Restart) {
        CompactionAction::Compact
    } else if !triggers.is_empty() || score.verdict == Verdict::Advise {
        CompactionAction::Advise
    } else {
        CompactionAction::None
    };
    if policy == CompactionPolicy::Advisory && action == CompactionAction::Compact {
        action = CompactionAction::Advise;
    }

    let reason = if triggers.is_empty() {
        format!("verdict {}", score.verdict.as_str())
    } else {
        triggers
            .iter()
            .map(|trigger| trigger.as_str())
            .collect::<Vec<_>>()
            .join(",")
    };

    CompactionDecision {
        action,
        policy,
        triggers,
        verdict: score.verdict,
        score: score.score,
        measured_input_tokens: observation.measured_input_tokens,
        usable_input_tokens: budget.usable_input_tokens(),
        compact_at_tokens: ceiling,
        estimated: observation.estimated,
        reason,
    }
}

// -- distillation --------------------------------------------------------

/// The instruction a route-backed distiller runs under. Read-only is not a
/// request here: the distillation request carries NO tool schemas at all, and
/// a reply that still contains a tool-use block is refused outright (see
/// [`distill`]).
const DISTILL_SYSTEM: &str = "\
You are compacting one agent session's earlier conversation into a briefing \
for the same session to continue from. Write plain prose, at most 20 lines. \
State only what the transcript shows: what was asked, what was decided, what \
was done and verified, and what is still open. Never invent a result, never \
claim a tool call succeeded unless its outcome says so, and never restate an \
outcome recorded as unknown as anything but unknown. You have no tools and \
must not request any.";

/// The deterministic rendering of the covered conversation. One function, so
/// the structural fallback summarises exactly what a model would have been
/// shown.
pub fn render_covered(
    state: &ConversationState,
    covers_through: SequenceId,
    limit: usize,
) -> String {
    let mut out = String::new();
    for message in &state.messages {
        if message.sequence > covers_through {
            break;
        }
        match message.role {
            MessageRole::User => {
                let label = if message.steering { "steering" } else { "user" };
                out.push_str(&format!(
                    "[{label}] {}\n",
                    message.text.as_deref().unwrap_or_default().trim()
                ));
            }
            MessageRole::Assistant => {
                let mut text = String::new();
                let mut calls = Vec::new();
                for block in &message.blocks {
                    match block {
                        super::journal::AssistantBlock::Text { text: body }
                        | super::journal::AssistantBlock::Refusal { text: body } => {
                            text.push_str(body)
                        }
                        super::journal::AssistantBlock::ToolCall { tool_call } => {
                            let name = state
                                .tool_calls
                                .get(tool_call)
                                .map(|record| record.name.as_str())
                                .unwrap_or("unknown");
                            calls.push(format!("{name}={}", latest_state(state, tool_call)));
                        }
                        _ => {}
                    }
                }
                if !text.trim().is_empty() {
                    out.push_str(&format!("[assistant] {}\n", text.trim()));
                }
                if !calls.is_empty() {
                    out.push_str(&format!("[tools] {}\n", calls.join(", ")));
                }
            }
        }
        if out.len() >= limit {
            break;
        }
    }
    truncate_chars(&out, limit)
}

/// The deterministic, no-model summary. Always available: it needs no
/// credential, no capacity and no network, which is what makes every
/// compaction helper work with the external harness binaries absent.
pub fn structural_summary(
    state: &ConversationState,
    covers_through: SequenceId,
) -> DistilledSummary {
    let mut turns = 0usize;
    let mut assistant = 0usize;
    let mut tools = 0usize;
    for message in &state.messages {
        if message.sequence > covers_through {
            break;
        }
        match message.role {
            MessageRole::User => turns += 1,
            MessageRole::Assistant => {
                assistant += 1;
                tools += message
                    .blocks
                    .iter()
                    .filter(|block| {
                        matches!(block, super::journal::AssistantBlock::ToolCall { .. })
                    })
                    .count();
            }
        }
    }
    let text = format!(
        "Structural summary of the first {covered} journal events: {turns} acknowledged \
         input(s), {assistant} assistant message(s), {tools} tool call(s). The verbatim record \
         is retained in the native journal and is not replaced by this summary.\n\n{transcript}",
        covered = covers_through.0,
        transcript = render_covered(state, covers_through, 4_000),
    );
    DistilledSummary {
        source: DistilledSummary::STRUCTURAL.to_string(),
        model: None,
        text,
        decisions: Vec::new(),
    }
}

/// A summary plus what producing it actually cost. The usage is zero for the
/// structural fallback, which is the point of having one.
#[derive(Clone, Debug, PartialEq)]
pub struct DistillOutcome {
    pub summary: DistilledSummary,
    pub usage: ProviderUsage,
}

impl DistillOutcome {
    fn structural(state: &ConversationState, covers_through: SequenceId) -> Self {
        Self {
            summary: structural_summary(state, covers_through),
            usage: ProviderUsage::default(),
        }
    }
}

/// Distils the covered conversation, through the session's own native route
/// when one is available and through the deterministic structural fallback
/// otherwise.
///
/// NEVER FAILS. No credential, no capacity, a refusal, a transport error, an
/// empty reply, or a reply that tried to call a tool all fall back to
/// [`structural_summary`] -- a compaction whose summariser is unavailable
/// still compacts, just with less prose.
pub fn distill(
    provider: Option<&dyn ProviderAdapter>,
    model: &str,
    cancel: &dyn Cancellation,
    state: &ConversationState,
    covers_through: SequenceId,
    budget: DistillBudget,
) -> DistillOutcome {
    let Some(provider) = provider else {
        return DistillOutcome::structural(state, covers_through);
    };
    let transcript = render_covered(state, covers_through, budget.max_input_chars);
    if transcript.trim().is_empty() {
        return DistillOutcome::structural(state, covers_through);
    }
    let request = ProviderRequest {
        model: model.to_string(),
        system: vec![DISTILL_SYSTEM.to_string()],
        messages: vec![ProviderMessage {
            role: ProviderMessageRole::User,
            content: vec![ProviderContent::Text { text: transcript }],
        }],
        // The enforced read-only tool set: none at all. A model cannot call
        // what it was never given, and a reply that tries anyway is refused
        // below rather than executed.
        tools: Vec::new(),
        max_output_tokens: budget.max_output_tokens,
        stop_sequences: Vec::new(),
        thinking: Default::default(),
        effort: None,
        cache: Default::default(),
    };
    let mut sink: Vec<super::super::provider::adapter::ProviderStreamEvent> = Vec::new();
    let Ok(response) = provider.stream(&request, cancel, &mut sink) else {
        return DistillOutcome::structural(state, covers_through);
    };
    // A reply that tried to call a tool is refused outright rather than
    // partially trusted: the request carried no tool schemas, so a tool-use
    // block means the model is not following the read-only contract this
    // summary is produced under. The cost is still reported.
    if response
        .content
        .iter()
        .any(|block| matches!(block, ProviderContent::ToolUse { .. }))
    {
        return DistillOutcome {
            summary: structural_summary(state, covers_through),
            usage: response.usage,
        };
    }
    let text: String = response
        .content
        .iter()
        .filter_map(|block| match block {
            ProviderContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if text.trim().is_empty() {
        return DistillOutcome {
            summary: structural_summary(state, covers_through),
            usage: response.usage,
        };
    }
    DistillOutcome {
        summary: DistilledSummary {
            source: DistilledSummary::ROUTE.to_string(),
            model: Some(response.model),
            text: truncate_chars(&text, budget.max_input_chars),
            decisions: Vec::new(),
        },
        usage: response.usage,
    }
}

// -- the compacted request ----------------------------------------------

/// The single message that stands in for everything a checkpoint covers.
///
/// Explicitly sectioned and explicitly labelled as a zirv-produced summary,
/// so the model can tell a compaction briefing from something a user said.
/// Outstanding tool calls are named with their real state -- including
/// `outcome_unknown` -- and pending input is repeated verbatim.
pub fn summary_message(checkpoint: &PortableCheckpoint) -> ProviderMessage {
    let mut text = String::new();
    text.push_str(
        "[zirv compaction] The earlier part of this conversation has been compacted. The \
         verbatim record is retained in the native journal; nothing below replaces it.\n",
    );
    text.push_str(&format!("Reason: {}\n", checkpoint.reason));
    if let Some(objective) = &checkpoint.objective {
        text.push_str(&format!("\n## Objective\n{objective}\n"));
    }
    if !checkpoint.hard_constraints.is_empty() {
        text.push_str("\n## Hard constraints\n");
        for constraint in &checkpoint.hard_constraints {
            text.push_str(&format!("- {constraint}\n"));
        }
    }
    if let Some(task) = &checkpoint.task {
        text.push_str(&format!("\n## Task\n{task}\n"));
    }
    if let Some(workflow) = &checkpoint.workflow {
        text.push_str(&format!("\n## Workflow\n{workflow}\n"));
    }
    if !checkpoint.summary.text.trim().is_empty() {
        text.push_str(&format!(
            "\n## Summary ({})\n{}\n",
            checkpoint.summary.source,
            checkpoint.summary.text.trim()
        ));
    }
    if !checkpoint.receipts.is_empty() {
        text.push_str("\n## Completed actions\n");
        for receipt in &checkpoint.receipts {
            let evidence = receipt
                .artifact_sha256
                .as_deref()
                .map(|hash| format!(" evidence={hash}"))
                .unwrap_or_default();
            text.push_str(&format!(
                "- {} {} ({}){evidence}\n",
                receipt.tool, receipt.tool_call, receipt.state
            ));
        }
    }
    if !checkpoint.outstanding_tools.is_empty() {
        text.push_str("\n## Outstanding tool calls (NOT complete)\n");
        for tool in &checkpoint.outstanding_tools {
            text.push_str(&format!(
                "- {} {} ({})\n",
                tool.tool, tool.tool_call, tool.state
            ));
        }
    }
    let pending = checkpoint.pending_input();
    if !pending.is_empty() {
        text.push_str("\n## Acknowledged input not yet answered\n");
        for input in pending {
            text.push_str(&format!("- {}\n", input.text));
        }
    }
    ProviderMessage {
        role: ProviderMessageRole::User,
        content: vec![ProviderContent::Text { text }],
    }
}

/// The compaction in force for `session`, or `None` for an uncompacted one.
/// Always the newest VALID checkpoint: a checkpoint this build cannot read is
/// skipped in favour of an older one it can.
pub fn active(
    journal: &Journal,
    session: &JournalSessionId,
) -> CtxResult<Option<PortableCheckpoint>> {
    checkpoint::latest_valid(journal, session, CheckpointKind::Compaction)
}

// -- history for status ---------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CompactionRecord {
    pub sequence: u64,
    pub kind: String,
    pub reason: String,
    pub covers_through: u64,
    pub summary_source: String,
    pub created_at: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ResumeRecord {
    pub sequence: u64,
    pub previous_generation: u64,
    pub generation: u64,
}

/// Every compaction and every resume this session recorded, oldest first.
/// Read straight off the journal, so it survives a restart and needs no
/// second bookkeeping store.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RecoveryHistory {
    pub compactions: Vec<CompactionRecord>,
    pub resumes: Vec<ResumeRecord>,
}

impl RecoveryHistory {
    pub fn is_empty(&self) -> bool {
        self.compactions.is_empty() && self.resumes.is_empty()
    }
}

pub fn history(journal: &Journal, session: &JournalSessionId) -> CtxResult<RecoveryHistory> {
    let mut out = RecoveryHistory::default();
    for stored in journal.events(session)? {
        match stored.event {
            JournalEvent::Checkpoint {
                kind,
                portable_state,
                ..
            } => {
                let parsed =
                    serde_json::from_value::<PortableCheckpoint>(portable_state.clone()).ok();
                out.compactions.push(CompactionRecord {
                    sequence: stored.sequence.0,
                    kind: format!("{kind:?}").to_lowercase(),
                    reason: parsed
                        .as_ref()
                        .map(|checkpoint| checkpoint.reason.clone())
                        .unwrap_or_else(|| "unreadable checkpoint".to_string()),
                    covers_through: parsed
                        .as_ref()
                        .map(|checkpoint| checkpoint.covers_through)
                        .unwrap_or(0),
                    summary_source: parsed
                        .as_ref()
                        .map(|checkpoint| checkpoint.summary.source.clone())
                        .unwrap_or_else(|| "unknown".to_string()),
                    created_at: stored.committed_at,
                });
            }
            JournalEvent::GenerationAdvanced { previous, current } => {
                out.resumes.push(ResumeRecord {
                    sequence: stored.sequence.0,
                    previous_generation: previous,
                    generation: current,
                })
            }
            _ => {}
        }
    }
    Ok(out)
}

// -- continuation planning ------------------------------------------------

/// How a session may continue after a stop, a crash or a route change.
#[derive(Clone, Debug, PartialEq)]
pub enum ContinuationPlan {
    /// The target route is byte-for-byte the route the session ran on, so the
    /// provider's own opaque envelope is still valid and is kept.
    SameRoute {
        checkpoint: Option<PortableCheckpoint>,
    },
    /// The route, model, endpoint, account or protocol changed. The opaque
    /// envelope is DISCARDED -- it belongs to a conversation the new provider
    /// never had -- and a legal semantic history is rebuilt from the
    /// checkpoint and the journal alone.
    Rebuilt {
        checkpoint: Option<PortableCheckpoint>,
        messages: Vec<ProviderMessage>,
    },
}

/// Decides how `session` continues onto `target`.
///
/// The rule the issue states and this enforces: a same-route resume keeps the
/// provider's opaque continuation state; a provider/model change produces a
/// portable, LEGAL history. Legal means: no hidden reasoning, no provider
/// signature, no redacted block, and no synthesized tool outcome. A tool call
/// whose outcome is unknown is carried as unknown, in words, never as a
/// fabricated result and never silently dropped.
pub fn plan_continuation(
    journal: &Journal,
    session: &JournalSessionId,
    target: &super::journal::RouteIdentity,
) -> CtxResult<ContinuationPlan> {
    let state = journal.replay(session)?;
    let checkpoint = active(journal, session)?;
    if state.identity.route == *target {
        return Ok(ContinuationPlan::SameRoute { checkpoint });
    }
    let covered = checkpoint
        .as_ref()
        .map(|checkpoint| SequenceId(checkpoint.covers_through))
        .unwrap_or(SequenceId(0));
    let mut messages = Vec::new();
    if let Some(checkpoint) = &checkpoint {
        messages.push(summary_message(checkpoint));
    }
    messages.extend(semantic_history(&state, covered));
    Ok(ContinuationPlan::Rebuilt {
        checkpoint,
        messages,
    })
}

/// Rebuilds portable conversation history after `covered`.
///
/// Text and refusals only. `Thinking`, `RedactedThinking` and every provider
/// signature are deliberately absent: they are one vendor's internal state,
/// they are not transferable, and fabricating them for another vendor would
/// be exactly the synthesis this step forbids. Tool calls become plain text
/// statements of what was requested and what actually happened, so the new
/// provider is told the truth without being handed a tool-use block it never
/// issued.
///
/// The rebuilt tail always OPENS ON A USER TURN. `covered` can legally land
/// right before an assistant message that still holds an unsettled tool call
/// -- `checkpoint::boundary` places it exactly there so that message stays
/// verbatim -- and handing a provider a history that starts on an assistant
/// turn is not a legal request. Any assistant activity ahead of the first
/// retained user turn is dropped here; it is already named in the
/// checkpoint's own outstanding-tool section (see `summary_message`).
pub fn semantic_history(state: &ConversationState, covered: SequenceId) -> Vec<ProviderMessage> {
    let mut messages = Vec::new();
    let mut seen_user = false;
    for message in &state.messages {
        if message.sequence <= covered {
            continue;
        }
        if !seen_user && message.role != MessageRole::User {
            continue;
        }
        match message.role {
            MessageRole::User => {
                seen_user = true;
                messages.push(ProviderMessage {
                    role: ProviderMessageRole::User,
                    content: vec![ProviderContent::Text {
                        text: message.text.clone().unwrap_or_default(),
                    }],
                });
            }
            MessageRole::Assistant => {
                let mut text = String::new();
                for block in &message.blocks {
                    match block {
                        super::journal::AssistantBlock::Text { text: body }
                        | super::journal::AssistantBlock::Refusal { text: body } => {
                            text.push_str(body)
                        }
                        super::journal::AssistantBlock::ToolCall { tool_call } => {
                            let name = state
                                .tool_calls
                                .get(tool_call)
                                .map(|record| record.name.as_str())
                                .unwrap_or("unknown");
                            let outcome = latest_state(state, tool_call);
                            text.push_str(&format!(
                                "\n[previous tool call] {name} ({tool_call}): {outcome}"
                            ));
                        }
                        // Hidden reasoning and provider signatures are not
                        // portable and are never reconstructed.
                        super::journal::AssistantBlock::Thinking { .. }
                        | super::journal::AssistantBlock::RedactedThinking { .. } => {}
                    }
                }
                if !text.trim().is_empty() {
                    messages.push(ProviderMessage {
                        role: ProviderMessageRole::Assistant,
                        content: vec![ProviderContent::Text { text }],
                    });
                }
            }
        }
    }
    messages
}

/// The latest execution state of one tool call, in the shared wire spelling.
/// A call with no execution record at all is `prepared`: admitted, recorded,
/// never reported on -- never "failed", which would be a claim.
fn latest_state(state: &ConversationState, tool_call: &super::journal::ToolCallId) -> String {
    state
        .executions
        .values()
        .filter(|execution| execution.tool_call == *tool_call)
        .max_by_key(|execution| execution.sequence)
        .map(|execution| checkpoint::state_label(execution.state))
        .unwrap_or_else(|| "prepared".to_string())
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::super::super::provider::Protocol;
    use super::super::super::provider::adapter::NeverCancelled;
    use super::super::fixture::{FixtureProvider, FixtureScript, fixture_target};
    use super::*;
    use crate::commands::ctx::event::NormalizedEvent;
    use crate::commands::ctx::runtime::journal::{
        AssistantBlock, ExecutionRecord, ExecutionState, MessageId, StoredMessage, ToolCallId,
        ToolCallRecord,
    };
    use crate::commands::ctx::runtime::testsupport::{route_identity, session_identity};
    use std::collections::BTreeMap;

    fn cfg() -> ScoreConfig {
        ScoreConfig::default()
    }

    fn budget() -> NativeBudget {
        NativeBudget {
            context_window_tokens: Some(100_000),
            output_reserve_tokens: 20_000,
        }
    }

    fn observation(tokens: u64, events: Vec<NormalizedEvent>) -> NativeObservation {
        NativeObservation {
            events,
            measured_input_tokens: tokens,
            estimated: false,
        }
    }

    fn state_with(messages: Vec<StoredMessage>) -> ConversationState {
        ConversationState {
            identity: session_identity("s1", route_identity()),
            last_sequence: messages
                .last()
                .map(|message| message.sequence)
                .unwrap_or(SequenceId(0)),
            messages,
            usage: BTreeMap::new(),
            tool_calls: BTreeMap::new(),
            executions: BTreeMap::new(),
            task_receipts: BTreeMap::new(),
            checkpoints: BTreeMap::new(),
            ended_reason: None,
        }
    }

    fn user(sequence: u64, text: &str) -> StoredMessage {
        StoredMessage {
            sequence: SequenceId(sequence),
            message_id: MessageId::new(format!("m{sequence}")).expect("id"),
            role: MessageRole::User,
            blocks: Vec::new(),
            text: Some(text.to_string()),
            steering: false,
            usage: None,
        }
    }

    #[test]
    fn the_usable_window_is_the_model_window_less_the_output_reservation() {
        assert_eq!(budget().usable_input_tokens(), Some(80_000));
        assert_eq!(
            NativeBudget {
                context_window_tokens: Some(1_000),
                output_reserve_tokens: 1_000,
            }
            .usable_input_tokens(),
            None
        );
        assert_eq!(
            NativeBudget {
                context_window_tokens: None,
                output_reserve_tokens: 1_000,
            }
            .usable_input_tokens(),
            None
        );
    }

    #[test]
    fn a_quiet_session_under_the_floor_needs_nothing() {
        let decision = evaluate(
            &observation(1_000, Vec::new()),
            &cfg(),
            budget(),
            CompactionPolicy::Automatic,
        );
        assert_eq!(decision.action, CompactionAction::None);
        assert!(decision.triggers.is_empty());
    }

    #[test]
    fn token_pressure_against_the_usable_window_forces_a_compaction() {
        // 80% of the 80_000 usable tokens, not 80% of the 100_000 window:
        // native thresholds come from measured input against the route's
        // window WITH the output reserve removed.
        let decision = evaluate(
            &observation(64_000, Vec::new()),
            &cfg(),
            budget(),
            CompactionPolicy::Automatic,
        );
        assert_eq!(decision.compact_at_tokens, 64_000);
        assert_eq!(decision.triggers, vec![CompactionTrigger::TokenPressure]);
        assert_eq!(decision.action, CompactionAction::Compact);
    }

    #[test]
    fn a_provider_overflow_forces_a_compaction_at_any_token_count() {
        let decision = evaluate(
            &observation(
                10,
                vec![NormalizedEvent::ProviderError {
                    class: ProviderErrorClass::Overflow,
                    at: None,
                    id: None,
                }],
            ),
            &cfg(),
            budget(),
            CompactionPolicy::Automatic,
        );
        assert!(
            decision
                .triggers
                .contains(&CompactionTrigger::ContextOverflow)
        );
        assert_eq!(decision.action, CompactionAction::Compact);
    }

    #[test]
    fn repeated_identical_tool_calls_are_reported_as_loss_of_progress() {
        let mut events = vec![NormalizedEvent::TurnStart { at_ms: Some(1) }];
        for _ in 0..3 {
            events.push(NormalizedEvent::ToolCall {
                name: "bash".to_string(),
                input_hash: 42,
                at_ms: Some(1),
            });
            events.push(NormalizedEvent::ToolResult { is_error: true });
        }
        events.push(NormalizedEvent::AssistantFinal {
            text: "still stuck".to_string(),
            input_tokens: 10,
            at_ms: Some(2),
        });
        let decision = evaluate(
            &observation(10, events),
            &cfg(),
            budget(),
            CompactionPolicy::Automatic,
        );
        assert!(
            decision
                .triggers
                .contains(&CompactionTrigger::LossOfProgress)
        );
    }

    #[test]
    fn repeated_identical_errors_are_their_own_trigger() {
        let mut events = vec![NormalizedEvent::TurnStart { at_ms: Some(1) }];
        for index in 0..3 {
            events.push(NormalizedEvent::ToolCall {
                name: "bash".to_string(),
                input_hash: index,
                at_ms: Some(1),
            });
            events.push(NormalizedEvent::ToolResult { is_error: true });
            events.push(NormalizedEvent::ToolErrorText { hash: 7 });
        }
        let decision = evaluate(
            &observation(10, events),
            &cfg(),
            budget(),
            CompactionPolicy::Automatic,
        );
        assert!(
            decision
                .triggers
                .contains(&CompactionTrigger::RepeatedIdenticalErrors)
        );
    }

    #[test]
    fn an_advisory_policy_downgrades_a_forced_compaction_to_advice() {
        let decision = evaluate(
            &observation(70_000, Vec::new()),
            &cfg(),
            budget(),
            CompactionPolicy::Advisory,
        );
        assert_eq!(decision.triggers, vec![CompactionTrigger::TokenPressure]);
        assert_eq!(decision.action, CompactionAction::Advise);
    }

    #[test]
    fn the_policy_fold_only_ever_narrows() {
        use CompactionPolicy::*;
        assert_eq!(CompactionPolicy::narrow(Automatic, None), Automatic);
        assert_eq!(
            CompactionPolicy::narrow(Automatic, Some(Advisory)),
            Advisory
        );
        // A repository asking for `automatic` cannot widen an operator's
        // `advisory`.
        assert_eq!(
            CompactionPolicy::narrow(Advisory, Some(Automatic)),
            Advisory
        );
    }

    #[test]
    fn equivalent_projected_events_give_identical_decisions() {
        let events = vec![
            NormalizedEvent::ModelId {
                id: "fixture-model".to_string(),
            },
            NormalizedEvent::TurnStart { at_ms: Some(1) },
            NormalizedEvent::AssistantFinal {
                text: "done".to_string(),
                input_tokens: 70_000,
                at_ms: Some(2),
            },
        ];
        let first = evaluate(
            &observation(70_000, events.clone()),
            &cfg(),
            budget(),
            CompactionPolicy::Automatic,
        );
        let second = evaluate(
            &observation(70_000, events),
            &cfg(),
            budget(),
            CompactionPolicy::Automatic,
        );
        assert_eq!(first, second);
    }

    #[test]
    fn the_structural_summary_needs_no_model_and_names_itself() {
        let state = state_with(vec![user(1, "build the thing")]);
        let summary = structural_summary(&state, SequenceId(1));
        assert_eq!(summary.source, DistilledSummary::STRUCTURAL);
        assert!(summary.model.is_none());
        assert!(summary.text.contains("build the thing"));
    }

    #[test]
    fn distillation_without_a_provider_falls_back_structurally() {
        let state = state_with(vec![user(1, "go")]);
        let outcome = distill(
            None,
            "fixture-model",
            &super::super::super::provider::adapter::NeverCancelled,
            &state,
            SequenceId(1),
            DistillBudget::default(),
        );
        assert_eq!(outcome.summary.source, DistilledSummary::STRUCTURAL);
        assert_eq!(outcome.usage, ProviderUsage::default());
    }

    #[test]
    fn distill_sends_the_configured_output_budget_and_no_tool_schema() {
        let state = state_with(vec![user(1, "investigate the timeout")]);
        let script = FixtureScript::from_json(
            r#"{"turns":[{"blocks":[{"type":"text","text":"Investigated the timeout."}],
               "finish_reason":"end_turn"}]}"#,
        )
        .expect("script");
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, "fixture-model"),
            script,
        );
        let outcome = distill(
            Some(&provider),
            "fixture-model",
            &NeverCancelled,
            &state,
            SequenceId(1),
            DistillBudget::default(),
        );
        assert_eq!(outcome.summary.source, DistilledSummary::ROUTE);
        let sent = provider.sent();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].tools.is_empty());
        assert_eq!(
            sent[0].max_output_tokens,
            DistillBudget::default().max_output_tokens
        );
        assert_eq!(sent[0].max_output_tokens, 1_024);
    }

    #[test]
    fn distill_discards_a_reply_that_tries_to_call_a_tool_and_falls_back_structurally() {
        let state = state_with(vec![user(1, "investigate the timeout")]);
        let script = FixtureScript::from_json(
            r#"{"turns":[{"blocks":[{"type":"tool_use","id":"call_1","name":"bash",
               "input":{"cmd":"ls"}}],"finish_reason":"tool_use"}]}"#,
        )
        .expect("script");
        let provider = FixtureProvider::new(
            fixture_target(Protocol::AnthropicMessages, "fixture-model"),
            script,
        );
        let outcome = distill(
            Some(&provider),
            "fixture-model",
            &NeverCancelled,
            &state,
            SequenceId(1),
            DistillBudget::default(),
        );
        // The reply is discarded: no route-sourced summary is produced even
        // though the provider answered, because it tried to call a tool the
        // request never offered it.
        assert_eq!(outcome.summary.source, DistilledSummary::STRUCTURAL);
        let sent = provider.sent();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].tools.is_empty());
    }

    #[test]
    fn a_route_change_rebuilds_history_without_hidden_reasoning() {
        let mut state = state_with(vec![
            user(1, "go"),
            StoredMessage {
                sequence: SequenceId(2),
                message_id: MessageId::new("m2").expect("id"),
                role: MessageRole::Assistant,
                blocks: vec![
                    AssistantBlock::Thinking {
                        text: "secret chain of thought".to_string(),
                        signature: None,
                    },
                    AssistantBlock::Text {
                        text: "reading the file".to_string(),
                    },
                    AssistantBlock::ToolCall {
                        tool_call: ToolCallId::new("call-a").expect("id"),
                    },
                ],
                text: None,
                steering: false,
                usage: None,
            },
        ]);
        state.tool_calls.insert(
            ToolCallId::new("call-a").expect("id"),
            ToolCallRecord {
                sequence: SequenceId(3),
                name: "read".to_string(),
                arguments: serde_json::json!({}),
                policy: super::super::journal::PolicyProvenance {
                    fingerprint: String::new(),
                    source: "test".to_string(),
                    decision: "allow".to_string(),
                    scope: "worker".to_string(),
                },
            },
        );
        state.executions.insert(
            super::super::journal::ExecutionId::new("exec-a").expect("id"),
            ExecutionRecord {
                sequence: SequenceId(4),
                tool_call: ToolCallId::new("call-a").expect("id"),
                state: ExecutionState::OutcomeUnknown,
                result: None,
                detail: None,
            },
        );

        let messages = semantic_history(&state, SequenceId(0));
        let rendered = format!("{messages:?}");
        assert!(!rendered.contains("secret chain of thought"));
        assert!(rendered.contains("outcome_unknown"));
        assert!(!rendered.contains("ToolUse"));
    }

    #[test]
    fn semantic_history_never_leads_with_an_assistant_turn_and_carries_no_tool_blocks() {
        let mut state = state_with(vec![
            user(1, "objective"),
            StoredMessage {
                sequence: SequenceId(2),
                message_id: MessageId::new("m2").expect("id"),
                role: MessageRole::Assistant,
                blocks: vec![
                    AssistantBlock::Thinking {
                        text: "weighing options".to_string(),
                        signature: None,
                    },
                    AssistantBlock::ToolCall {
                        tool_call: ToolCallId::new("call-a").expect("id"),
                    },
                ],
                text: None,
                steering: false,
                usage: None,
            },
            user(5, "still waiting"),
        ]);
        state.tool_calls.insert(
            ToolCallId::new("call-a").expect("id"),
            ToolCallRecord {
                sequence: SequenceId(3),
                name: "bash".to_string(),
                arguments: serde_json::json!({}),
                policy: super::super::journal::PolicyProvenance {
                    fingerprint: String::new(),
                    source: "test".to_string(),
                    decision: "allow".to_string(),
                    scope: "worker".to_string(),
                },
            },
        );
        state.executions.insert(
            super::super::journal::ExecutionId::new("exec-a").expect("id"),
            ExecutionRecord {
                sequence: SequenceId(3),
                tool_call: ToolCallId::new("call-a").expect("id"),
                state: ExecutionState::OutcomeUnknown,
                result: None,
                detail: None,
            },
        );

        // `covered` stops right before the assistant turn that still holds
        // an open tool call -- exactly what `boundary()` produces for a
        // pending call -- so that turn is the first one the tail retains.
        let messages = semantic_history(&state, SequenceId(1));

        assert!(!messages.is_empty());
        assert_eq!(messages[0].role, ProviderMessageRole::User);
        for message in &messages {
            for block in &message.content {
                assert!(!matches!(block, ProviderContent::ToolUse { .. }));
                assert!(!matches!(block, ProviderContent::ToolResult { .. }));
            }
        }
    }

    #[test]
    fn the_summary_message_names_outstanding_work_and_repeats_pending_input() {
        let state = state_with(vec![user(1, "objective"), user(5, "and also this")]);
        let checkpoint = checkpoint::build(
            &state,
            SequenceId(3),
            SequenceId(3),
            &super::super::journal::CheckpointId::new("cp-1").expect("id"),
            &checkpoint::CheckpointContext {
                hard_constraints: vec!["never force-push".to_string()],
                reason: "token_pressure".to_string(),
                ..Default::default()
            },
            structural_summary(&state, SequenceId(3)),
            10,
        );
        let message = summary_message(&checkpoint);
        let ProviderContent::Text { text } = &message.content[0] else {
            panic!("summary is one text block");
        };
        assert!(text.contains("never force-push"));
        assert!(text.contains("and also this"));
        assert!(text.contains("token_pressure"));
    }
}
