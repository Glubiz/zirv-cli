//! Safe native and cross-runtime orchestrator rollover (issue #488, roadmap
//! step N19).
//!
//! `seat.rs` owns the transaction, `rollover.rs` drives it from capacity
//! evidence, and `handover.rs` is the live-swap seam. All three were built
//! when a seat could only ever move between supervised harness PROCESSES, so
//! "prepare, launch, wait for a turn signal, commit" was a complete story: the
//! successor's own harness owned the continuation, and everything zirv had to
//! carry across was a handoff packet.
//!
//! A native session has no such owner. Its state is a journal, its
//! continuation is either a provider envelope (same route) or a semantic
//! rebuild (any other route), its in-flight tool effects are durable facts
//! rather than screen output, and its subagents are zirv's own delegations.
//! This module is the part that is genuinely different, and only that part:
//!
//! 1. [`Direction`] -- the four runtime combinations, so nothing downstream
//!    has to re-derive "is this a cross-runtime move" from two enums.
//! 2. [`reach_boundary`] -- item 2. Drain or explicitly cancel in-flight work,
//!    reconcile outcome-unknown tools, and persist ONE portable checkpoint
//!    (acknowledged input, pending messages, task/workflow state, claims,
//!    receipts, evidence) BEFORE the successor is prepared.
//! 3. [`validate`] -- item 3. Authentication, capabilities, budget, policy and
//!    a legal continuation payload, decided before the successor may write
//!    anything. The fence that actually stops it writing is `seat::authority`,
//!    which refuses the prepared-but-uncommitted generation outright.
//! 4. [`Record`] -- items 7 and 8. Trigger, tried routes, decision and outcome,
//!    durable next to the seat so "why did my seat move, and where did it
//!    try first" survives the session that answered it.
//! 5. [`settle_subagents`] -- item 6. A native subagent hidden inside a wrapped
//!    harness is finished, stopped or RETAINED under recorded ownership. There
//!    is no fourth answer: zirv has no mechanism that migrates a running
//!    worker to another seat, so it never claims one.
//! 6. [`plan_return`] -- item 8. The return to the preferred route, with N18's
//!    hysteresis and an explicit billing authority check that refuses rather
//!    than silently spends.
//!
//! Pure wherever it can be: [`direction`], [`validate`], [`disposition`] and
//! [`plan_return`] take their inputs and an explicit `now` and touch no fs,
//! clock, env or net, the same discipline `rot.rs` and `route.rs` keep. The
//! I/O is [`reach_boundary`], [`settle_subagents`] and this module's own
//! record store.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::CtxResult;
use super::delegation;
use super::route::{self, BillingPosture, Demand, Ineligible, RouteOffer};
use super::runtime::RuntimeKind;
use super::runtime::checkpoint::{self, CheckpointContext, PortableCheckpoint};
use super::runtime::compaction::{self, ContinuationPlan};
use super::runtime::journal::{
    CheckpointId, CheckpointKind, ExecutionState, Journal, JournalSessionId,
};
use super::seat;
use super::state::StateDir;

/// Bumped on any shape change to the persisted [`Record`]. A record from a
/// schema this build does not know is skipped, never repaired -- the same rule
/// `checkpoint::PortableCheckpoint` states for itself.
pub const RECORD_SCHEMA_VERSION: u32 = 1;

/// How many attempts one rollover record keeps. A rollover that has tried
/// more routes than this has a bigger problem than its own history.
const MAX_ATTEMPTS: usize = 32;

// -- direction --------------------------------------------------------------

/// Which of the four runtime transitions a rollover is.
///
/// Named rather than inferred because the ANSWERS differ: only
/// `NativeToNative` can ever keep a provider's opaque continuation envelope,
/// only `HarnessToHarness` is the behaviour that existed before this issue,
/// and the two mixed directions each have a half that has no counterpart on
/// the other side (a harness has no journal; a native session has no pty).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    HarnessToHarness,
    HarnessToNative,
    NativeToHarness,
    NativeToNative,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HarnessToHarness => "harness->harness",
            Self::HarnessToNative => "harness->native",
            Self::NativeToHarness => "native->harness",
            Self::NativeToNative => "native->native",
        }
    }

    /// Whether the seat changes BACKEND, as opposed to only agent or model.
    pub fn crosses_runtime(self) -> bool {
        matches!(self, Self::HarnessToNative | Self::NativeToHarness)
    }

    pub fn target(self) -> RuntimeKind {
        match self {
            Self::HarnessToHarness | Self::NativeToHarness => RuntimeKind::Harness,
            Self::HarnessToNative | Self::NativeToNative => RuntimeKind::Native,
        }
    }
}

/// The direction from two runtimes. `RuntimeKind::Unknown` -- a value written
/// by a build this one has never heard of -- is an error rather than a guess:
/// treating it as `Harness` would try to spawn a harness process for a session
/// no harness ever ran, which is exactly the failure `RuntimeKind::Unknown`
/// exists to prevent.
pub fn direction(from: RuntimeKind, to: RuntimeKind) -> CtxResult<Direction> {
    Ok(match (from, to) {
        (RuntimeKind::Harness, RuntimeKind::Harness) => Direction::HarnessToHarness,
        (RuntimeKind::Harness, RuntimeKind::Native) => Direction::HarnessToNative,
        (RuntimeKind::Native, RuntimeKind::Harness) => Direction::NativeToHarness,
        (RuntimeKind::Native, RuntimeKind::Native) => Direction::NativeToNative,
        (from, to) => {
            return Err(format!(
                "zirv ctx rollover: cannot roll a seat from runtime `{from}` to `{to}`: an \
                 unrecognised runtime was written by a newer build"
            )
            .into());
        }
    })
}

/// What set this rollover off, in the vocabulary the acceptance criteria use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Trigger {
    /// The source account is out of window capacity.
    UsageExhaustion,
    /// The source route cannot be reached at all.
    EndpointFailure,
    /// An operator asked for the swap.
    ManualHandover,
    /// The preferred route recovered and the seat is going home.
    CapacityReturn,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UsageExhaustion => "usage-exhaustion",
            Self::EndpointFailure => "endpoint-failure",
            Self::ManualHandover => "manual-handover",
            Self::CapacityReturn => "capacity-return",
        }
    }

    /// The trigger a seat cause represents. `unreachable` is
    /// `rollover::evaluate`'s own route-health answer, which is the only thing
    /// that distinguishes "this account is out of capacity" from "this
    /// endpoint is down" -- both arrive as `Cause::Reactive`.
    pub fn from_cause(cause: &seat::Cause, unreachable: bool) -> Self {
        match cause {
            seat::Cause::Manual => Self::ManualHandover,
            seat::Cause::Reclaim { .. } => Self::CapacityReturn,
            seat::Cause::Proactive { .. } => Self::UsageExhaustion,
            seat::Cause::Reactive { .. } if unreachable => Self::EndpointFailure,
            seat::Cause::Reactive { .. } => Self::UsageExhaustion,
        }
    }
}

// -- item 2: the safe boundary ---------------------------------------------

/// Whether the boundary was reached by letting work finish or by cutting it
/// off. Decided by the caller (`rollover::evaluate`'s own verified-idle
/// answer), never guessed here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drain {
    /// The supervisor observed an idle turn boundary. Nothing is cut off.
    Quiesced,
    /// A forced rollover: the source cannot wait for a boundary that may never
    /// come, so work that has not begun is explicitly cancelled.
    Forced,
}

/// What reaching the boundary actually settled. Everything here is a fact read
/// back off the journal AFTER the writes, never an intention.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Boundary {
    pub drained: bool,
    /// Tool calls that were admitted and durably recorded but whose effect had
    /// NOT begun, explicitly cancelled at this boundary. Cancelling one is
    /// honest: nothing happened.
    pub cancelled: Vec<String>,
    /// Executions whose effect began and never reported. These are carried as
    /// unknown, never as failed and never replayed -- and while any remain the
    /// successor HALTS for reconciliation (see [`Boundary::halts_successor`]).
    pub outcome_unknown: Vec<String>,
    /// Acknowledged input the source never folded into a provider request.
    /// The successor still owes it.
    pub pending_input: Vec<String>,
    /// Task claims this session still holds.
    pub claims: Vec<String>,
    /// Completion receipts carried across.
    pub receipts: usize,
    /// Evidence references carried across.
    pub evidence: usize,
    /// The portable checkpoint id, when one was committed.
    pub checkpoint: Option<String>,
}

impl Boundary {
    /// Whether the successor must stop and ask before doing anything else.
    ///
    /// Criterion 4: an ambiguous external effect is never replayed. It is not
    /// enough to carry it -- the successor has to be BLOCKED on it, or the
    /// first thing a fresh model does is call the tool again.
    pub fn halts_successor(&self) -> bool {
        !self.outcome_unknown.is_empty()
    }

    /// The operator-facing sentence naming what is owed. Empty when nothing
    /// is.
    pub fn reconciliation_note(&self) -> Option<String> {
        if self.outcome_unknown.is_empty() {
            return None;
        }
        Some(format!(
            "{} tool effect(s) began and never reported ({}). They are carried as \
             outcome-unknown and must be reconciled before any retry; the successor is halted \
             until they are.",
            self.outcome_unknown.len(),
            self.outcome_unknown.join(", ")
        ))
    }
}

/// Reaches a safe boundary on a NATIVE source session and persists everything
/// the successor will need, in this order:
///
/// 1. every execution whose effect BEGAN and never reported becomes
///    `OutcomeUnknown` (`Journal::reconcile_started_as_unknown`);
/// 2. on a [`Drain::Forced`] boundary, every execution that was merely
///    PREPARED -- admitted, recorded, never begun -- is explicitly
///    `Cancelled`. Nothing that started is ever called cancelled, because
///    that would be a claim about an effect nobody observed;
/// 3. one [`PortableCheckpoint`] is built from the journal and committed as a
///    `CheckpointKind::Handoff`, carrying acknowledged input (with what is
///    still pending), claims, receipts, outstanding tools and evidence.
///
/// The checkpoint is committed BEFORE the caller prepares a successor -- that
/// is item 2's whole point -- and it is written under the SOURCE generation,
/// which is the generation those facts actually belong to.
///
/// A harness source has no journal, so it does not come here at all: its
/// boundary is the existing verified-idle turn boundary plus `handoff.rs`'s
/// structural packet, which this issue does not change.
pub fn reach_boundary(
    journal: &mut Journal,
    session: &JournalSessionId,
    generation: u64,
    drain: Drain,
    context: &CheckpointContext,
    now: u64,
) -> CtxResult<Boundary> {
    let scope = Default::default();
    journal.reconcile_started_as_unknown(session, generation, None, now)?;

    let mut cancelled = Vec::new();
    if drain == Drain::Forced {
        let state = journal.replay(session)?;
        let prepared: Vec<_> = state
            .executions
            .iter()
            .filter(|(_, record)| record.state == ExecutionState::Prepared)
            .map(|(id, record)| (id.clone(), record.tool_call.to_string()))
            .collect();
        for (execution, tool_call) in prepared {
            journal.transition_execution(
                session,
                generation,
                &scope,
                &execution,
                ExecutionState::Cancelled,
                None,
                Some(
                    "cancelled at a rollover boundary before its effect began; nothing ran"
                        .to_string(),
                ),
                None,
                now,
            )?;
            cancelled.push(tool_call);
        }
    }

    let state = journal.replay(session)?;
    let covers_through = checkpoint::boundary(&state, compaction::RETAIN_RECENT_MESSAGES)
        .unwrap_or(state.last_sequence);
    let summary = compaction::structural_summary(&state, covers_through);
    // What counts as DELIVERED, for a rollover: the last assistant message.
    // The journal does not record which provider request an input was folded
    // into, and the one thing it does record honestly is whether the model
    // ever answered after it. Acknowledged input with no assistant turn after
    // it is still owed, and criterion 2 forbids losing it; over-reporting at
    // worst costs the successor a repeated instruction.
    let delivered_through = state
        .messages
        .iter()
        .rev()
        .find(|message| message.role == super::runtime::journal::MessageRole::Assistant)
        .map(|message| message.sequence)
        .unwrap_or(super::runtime::journal::SequenceId(0));
    let id = CheckpointId::new(format!("rollover-{}-{}", generation, state.last_sequence.0))?;
    let portable = checkpoint::build(
        &state,
        covers_through,
        delivered_through,
        &id,
        context,
        summary,
        now,
    );
    checkpoint::commit(
        journal,
        None,
        generation,
        &scope,
        CheckpointKind::Handoff,
        &portable,
        now,
    )?;

    Ok(summarize(&portable, drain, cancelled))
}

/// The [`Boundary`] a committed checkpoint describes. Split out so the summary
/// is derived from what was actually persisted rather than from what the
/// writer intended to persist.
fn summarize(portable: &PortableCheckpoint, drain: Drain, cancelled: Vec<String>) -> Boundary {
    // Named by TOOL CALL id throughout, never by execution id: the tool call
    // is what the successor would otherwise re-issue, and it is the identity
    // the checkpoint's own outstanding-tool section already uses.
    let outcome_unknown: BTreeSet<String> = portable
        .outstanding_tools
        .iter()
        .filter(|tool| tool.state == "outcome_unknown")
        .map(|tool| tool.tool_call.clone())
        .collect();
    Boundary {
        drained: drain == Drain::Quiesced,
        cancelled,
        outcome_unknown: outcome_unknown.into_iter().collect(),
        pending_input: portable
            .pending_input()
            .into_iter()
            .map(|input| input.message_id.clone())
            .collect(),
        claims: portable
            .claims
            .iter()
            .map(|claim| claim.task.clone())
            .collect(),
        receipts: portable.receipts.len(),
        evidence: portable.evidence.len(),
        checkpoint: Some(portable.checkpoint_id.clone()),
    }
}

// -- item 3: successor validation ------------------------------------------

/// Everything about a candidate successor that is not already a
/// [`RouteOffer`]. Supplied by the caller, because every one of them is an
/// I/O answer (a credential resolved, a ledger read, a process started) and
/// this decision must stay pure.
#[derive(Debug, Clone, Copy)]
pub struct SuccessorFacts<'a> {
    pub offer: &'a RouteOffer,
    /// Whether a usable credential for this route actually resolved. A route
    /// that is configured but unauthenticated is refused BEFORE the source is
    /// given up, never after.
    pub authenticated: bool,
    /// Tokens the operator's remaining budget will still cover on this route,
    /// or `None` when no ceiling applies. Compared against the demand's own
    /// context requirement, which is the only number known before the
    /// successor has run.
    pub budget_tokens: Option<u64>,
    /// Whether the successor process/session actually came up. `false` is the
    /// "successor startup failure" case the acceptance criteria name.
    pub started: bool,
}

/// What the successor will actually be handed to continue from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Continuation {
    /// Native->native on the identical route: the provider's own opaque
    /// envelope is still valid and is kept.
    ProviderEnvelope,
    /// Any route change: the envelope is discarded and a legal semantic
    /// history is rebuilt from the checkpoint and the journal.
    SemanticRebuild { messages: usize },
    /// A harness successor. A coding harness cannot be handed another
    /// vendor's continuation envelope at all, so what crosses is the portable
    /// checkpoint rendered as the existing structural handoff packet.
    StructuralPacket,
}

/// Why a successor was refused. Every variant is a fact the SOURCE can act on
/// -- that is what makes item 7's "restore the original" possible: nothing
/// here is discovered after the source has been given up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "refusal", rename_all = "kebab-case")]
pub enum Refusal {
    NotAuthenticated { route: String },
    /// Capability, context room, policy or billing authority -- N18's own
    /// pre-ranking eligibility answer, reused rather than re-derived.
    Ineligible { route: String, reason: String },
    OverBudget { route: String, need: u64, have: u64 },
    /// No legal continuation payload could be produced for this direction.
    NoContinuation { route: String, detail: String },
    StartupFailed { route: String },
}

impl Refusal {
    pub fn route(&self) -> &str {
        match self {
            Self::NotAuthenticated { route }
            | Self::Ineligible { route, .. }
            | Self::OverBudget { route, .. }
            | Self::NoContinuation { route, .. }
            | Self::StartupFailed { route } => route,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::NotAuthenticated { route } => {
                format!("{route}: no usable credential resolved for this route")
            }
            Self::Ineligible { route, reason } => format!("{route}: {reason}"),
            Self::OverBudget { route, need, have } => format!(
                "{route}: the remaining budget covers {have} tokens and this continuation needs \
                 {need}"
            ),
            Self::NoContinuation { route, detail } => format!("{route}: {detail}"),
            Self::StartupFailed { route } => {
                format!("{route}: the successor session did not start")
            }
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label())
    }
}

impl std::error::Error for Refusal {}

/// A successor that cleared every gate and may now be committed onto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admission {
    pub direction: Direction,
    pub continuation: Continuation,
}

/// Item 3: can this successor legally take the seat?
///
/// PURE, and deliberately ordered so the cheapest and most operator-relevant
/// refusals come first: policy/capability/context/billing (N18's `eligible`),
/// then authentication, then budget, then whether it actually started, then
/// whether a legal continuation payload exists for this direction.
///
/// Nothing here writes, and the caller must not let the successor write
/// either: `seat::authority` refuses the prepared-but-uncommitted generation
/// at the journal, the delegation service, the coordinator graph and the
/// writer permit, so "validated but not yet committed" is enforced by the
/// fence rather than by the caller remembering.
pub fn validate(
    facts: &SuccessorFacts<'_>,
    demand: &Demand,
    plan: &ContinuationPlan,
    direction: Direction,
) -> Result<Admission, Refusal> {
    let label = facts.offer.identity.label();
    if let Err(ineligible) = route::eligible(facts.offer, demand) {
        return Err(match ineligible {
            // Named separately because it is the one refusal that is about
            // MONEY rather than about fit: item 8's rule is that moving work
            // onto a differently-billed route is a decision an operator has
            // to have authorized, never a silent consequence of capacity.
            Ineligible::UnauthorizedBilling { .. } => Refusal::Ineligible {
                route: label,
                reason: ineligible.label(),
            },
            other => Refusal::Ineligible {
                route: label,
                reason: other.label(),
            },
        });
    }
    if !facts.authenticated {
        return Err(Refusal::NotAuthenticated { route: label });
    }
    if let Some(have) = facts.budget_tokens
        && demand.context_tokens > have
    {
        return Err(Refusal::OverBudget {
            route: label,
            need: demand.context_tokens,
            have,
        });
    }
    if !facts.started {
        return Err(Refusal::StartupFailed { route: label });
    }
    let continuation = continuation_for(direction, plan).ok_or_else(|| Refusal::NoContinuation {
        route: label.clone(),
        detail: "a same-route provider envelope cannot be handed to a different runtime, and no \
                 semantic history could be rebuilt"
            .to_string(),
    })?;
    Ok(Admission {
        direction,
        continuation,
    })
}

/// The legal continuation payload for one direction (item 5).
///
/// The rule, stated once: a provider's opaque envelope belongs to one
/// conversation on one route with one vendor. Only a native successor on the
/// IDENTICAL route may keep it. A harness successor never sees it -- it gets
/// the structural packet, which is what a harness has always taken. Every
/// other native case is a semantic rebuild, which N17 already produces.
fn continuation_for(direction: Direction, plan: &ContinuationPlan) -> Option<Continuation> {
    match (direction.target(), plan) {
        (RuntimeKind::Harness, _) => Some(Continuation::StructuralPacket),
        (RuntimeKind::Native, ContinuationPlan::SameRoute { .. })
            if direction == Direction::NativeToNative =>
        {
            Some(Continuation::ProviderEnvelope)
        }
        // A harness->native move can never be `SameRoute`: the source ran no
        // native route at all, so there is no envelope to keep and the plan
        // saying otherwise is about a different session. Fall through to the
        // rebuild, which is always legal.
        (RuntimeKind::Native, ContinuationPlan::SameRoute { .. }) => {
            Some(Continuation::SemanticRebuild { messages: 0 })
        }
        (RuntimeKind::Native, ContinuationPlan::Rebuilt { messages, .. }) => {
            Some(Continuation::SemanticRebuild {
                messages: messages.len(),
            })
        }
        (RuntimeKind::Unknown, _) => None,
    }
}

// -- item 6: native subagents under a wrapped harness ----------------------

/// What happened to one delegation a rolling seat owned.
///
/// There is deliberately no `Migrated`. zirv has no observed mechanism that
/// moves a running worker from one seat generation to another, and claiming
/// one would be exactly the "transparent control" this issue forbids. A live
/// worker is either stopped or RETAINED -- still running, still owned, and the
/// ownership record says by whom.
///
/// `#[allow(dead_code)]` on this type and its two functions for the reason
/// `runtime/mod.rs` and `route::offers_from_config` already document for
/// themselves: settling subagents needs the REPOSITORY root, which the
/// seat-level rollover driver (`rollover.rs`, which works from a seat short id
/// and a state directory) does not carry. The supervisors that do are
/// `wrap.rs` and `dash/mod.rs`, which this task deliberately does not touch
/// -- see the design note. The decision and its durable effects are real and
/// tested here so that wiring is a call, not a redesign.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "disposition", rename_all = "kebab-case")]
pub enum Disposition {
    /// Already terminal when the boundary was reached.
    Finished { phase: String },
    /// Interrupted at a forced boundary.
    Stopped { reason: String },
    /// Left running, under the seat address that keeps answering for it. A
    /// seat's short id is a stable address that outlives the session id
    /// rotating underneath it, which is precisely why retention works across
    /// a runtime change and a fresh conversation does not.
    Retained { owner: String },
}

impl Disposition {
    /// See [`Disposition`] for why this has no in-tree caller yet.
    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Finished { .. } => "finished",
            Self::Stopped { .. } => "stopped",
            Self::Retained { .. } => "retained",
        }
    }
}

/// The pure half of item 6: what SHOULD happen to `record` at this boundary.
/// See [`Disposition`] for why this has no in-tree caller yet.
#[allow(dead_code)]
pub fn disposition(record: &delegation::Record, drain: Drain, owner: &str) -> Disposition {
    if record.phase.is_terminal() {
        return Disposition::Finished {
            phase: record.phase.as_str().to_string(),
        };
    }
    match drain {
        Drain::Quiesced => Disposition::Retained {
            owner: owner.to_string(),
        },
        Drain::Forced => Disposition::Stopped {
            reason: "the seat rolled over at a forced boundary and could not wait for this \
                     worker to finish"
                .to_string(),
        },
    }
}

/// Applies [`disposition`] to every delegation this seat launched, and records
/// what it did. Returns `(delegation handle, disposition)` oldest first.
///
/// A `Stopped` worker goes through `delegation::interrupt`, which owns
/// cancellation and -- importantly -- preserves `unknown_tool_outcomes`: an
/// unresolved effect is exactly what a cancel must not erase. A `Retained`
/// worker's record is re-saved unchanged apart from its updated timestamp,
/// which is what makes the retention itself durable rather than an assumption.
/// See [`Disposition`] for why this has no in-tree caller yet.
#[allow(dead_code)]
pub fn settle_subagents(
    state: &StateDir,
    repo: &Path,
    seat_short: &str,
    parent_session: Option<&str>,
    drain: Drain,
    now: u64,
) -> Vec<(String, Disposition)> {
    let mut out = Vec::new();
    let mut records = delegation::list(state, repo);
    records.sort_by_key(|record| record.launched_at);
    for record in records {
        let ours = record.handle.short == seat_short
            || parent_session
                .is_some_and(|session| record.parent_session.as_deref() == Some(session));
        if !ours {
            continue;
        }
        let verdict = disposition(&record, drain, seat_short);
        if matches!(verdict, Disposition::Stopped { .. }) {
            let _ = delegation::interrupt(state, repo, &record.handle.delegation, now);
        } else if matches!(verdict, Disposition::Retained { .. }) {
            // Ownership is re-stated rather than moved: the reservation and
            // the write claim this worker holds are still its own, and the
            // address that answers for it is still this seat.
            let _ = delegation::record_ownership(
                state,
                repo,
                &record.handle.delegation,
                record.reservation.clone(),
                record.write_claim.clone(),
                now,
            );
        }
        out.push((record.handle.delegation.clone(), verdict));
    }
    out
}

// -- items 7 and 8: the durable rollover record ----------------------------

/// One route this rollover actually tried, and what came of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attempt {
    pub route: String,
    pub runtime: String,
    /// `admitted` or `refused`.
    pub outcome: String,
    pub detail: String,
    pub at: u64,
}

/// How the rollover ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum Settlement {
    /// The successor took the seat.
    Committed { route: String, generation: u64 },
    /// Preparation failed and the ORIGINAL session was kept or restored. The
    /// seat never moved.
    Restored { reason: String },
    /// Nothing could take the seat, so it waits honestly rather than being
    /// described as having moved.
    Parked { until: u64, reason: String },
}

/// The durable answer to "why did my seat move, what did it try, and what
/// happened" -- item 7's recording requirement, stored next to the seat
/// record it is about (`<state>/sessions/<short>.rollover.json`) rather than
/// on it, for exactly the reason `seat.rs` keeps the seat out of
/// `sessions::Record`: a seat is rewritten on every ordinary tick and this is
/// not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub short: String,
    pub trigger: Trigger,
    pub direction: Option<Direction>,
    pub source_agent: String,
    pub source_runtime: String,
    pub generation: u64,
    /// The one-line decision, in the same words the decision log carries.
    pub decision: String,
    #[serde(default)]
    pub attempts: Vec<Attempt>,
    #[serde(default)]
    pub boundary: Option<Boundary>,
    #[serde(default)]
    pub subagents: Vec<(String, Disposition)>,
    #[serde(default)]
    pub settlement: Option<Settlement>,
    pub started_at: u64,
    pub updated_at: u64,
}

fn default_schema_version() -> u32 {
    RECORD_SCHEMA_VERSION
}

impl Record {
    pub fn open(seat: &seat::Seat, trigger: Trigger, decision: &str, now: u64) -> Self {
        Self {
            schema_version: RECORD_SCHEMA_VERSION,
            short: seat.short.clone(),
            trigger,
            direction: None,
            source_agent: seat.agent.clone(),
            source_runtime: seat.runtime.as_str().to_string(),
            generation: seat.generation,
            decision: decision.to_string(),
            attempts: Vec::new(),
            boundary: None,
            subagents: Vec::new(),
            settlement: None,
            started_at: now,
            updated_at: now,
        }
    }

    pub fn admitted(&mut self, route: &str, runtime: RuntimeKind, detail: &str, now: u64) {
        self.push(Attempt {
            route: route.to_string(),
            runtime: runtime.as_str().to_string(),
            outcome: "admitted".to_string(),
            detail: detail.to_string(),
            at: now,
        });
    }

    pub fn refused(&mut self, refusal: &Refusal, runtime: RuntimeKind, now: u64) {
        self.push(Attempt {
            route: refusal.route().to_string(),
            runtime: runtime.as_str().to_string(),
            outcome: "refused".to_string(),
            detail: refusal.label(),
            at: now,
        });
    }

    fn push(&mut self, attempt: Attempt) {
        self.updated_at = attempt.at;
        self.attempts.push(attempt);
        if self.attempts.len() > MAX_ATTEMPTS {
            self.attempts.remove(0);
        }
    }

    pub fn settle(&mut self, settlement: Settlement, now: u64) {
        self.settlement = Some(settlement);
        self.updated_at = now;
    }

    /// The status line an operator reads: the same logical seat, the new
    /// backend and model, and why. Criterion 6's text half -- the identity
    /// never changes, only what is answering at it.
    pub fn status_line(&self) -> String {
        let mut line = format!(
            "seat {} (generation {}): {} from {} [{}]",
            self.short,
            self.generation,
            self.trigger.as_str(),
            self.source_agent,
            self.source_runtime,
        );
        if let Some(direction) = self.direction {
            line.push_str(&format!(" via {}", direction.as_str()));
        }
        match &self.settlement {
            Some(Settlement::Committed { route, generation }) => line.push_str(&format!(
                " -> {route} (generation {generation}): {}",
                self.decision
            )),
            Some(Settlement::Restored { reason }) => {
                line.push_str(&format!(" -> kept the original session: {reason}"))
            }
            Some(Settlement::Parked { until, reason }) => {
                line.push_str(&format!(" -> parked until unix {until}: {reason}"))
            }
            None => line.push_str(" -> in flight"),
        }
        if let Some(note) = self.boundary.as_ref().and_then(Boundary::reconciliation_note) {
            line.push_str(&format!(" | {note}"));
        }
        line
    }
}

fn record_path(state: &StateDir, short: &str) -> PathBuf {
    state.sessions().join(format!("{short}.rollover.json"))
}

/// Tolerant-read like every other registry record in this codebase: a missing
/// file, a malformed one and one from a schema this build does not know are
/// all `None`, never an error.
pub fn load(state: &StateDir, short: &str) -> Option<Record> {
    let raw = std::fs::read_to_string(record_path(state, short)).ok()?;
    serde_json::from_str::<Record>(&raw)
        .ok()
        .filter(|record| record.schema_version == RECORD_SCHEMA_VERSION)
}

pub fn store(state: &StateDir, record: &Record) -> CtxResult<()> {
    super::state::create_private_dir_all(&state.sessions())?;
    let json = serde_json::to_string_pretty(record)?;
    super::state::write_private(&record_path(state, &record.short), &json)?;
    Ok(())
}

/// Drops the record for `short`. Best-effort, like `seat::remove`.
pub fn forget(state: &StateDir, short: &str) {
    let _ = std::fs::remove_file(record_path(state, short));
}

// -- item 8: the return ----------------------------------------------------

/// What [`plan_return`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReturnVerdict {
    /// Go home, onto this route.
    Return { route: String, runtime: RuntimeKind },
    /// Not yet, for a reason that may resolve on its own.
    Hold(String),
    /// Not at all, for a reason that will not.
    Refuse(Refusal),
}

/// Everything the return decision needs, gathered by the caller.
#[derive(Debug, Clone, Copy)]
pub struct ReturnInputs<'a> {
    pub seat: &'a seat::Seat,
    pub now: u64,
    /// The preferred route's own offer, and the facts about it.
    pub preferred: &'a SuccessorFacts<'a>,
    /// The preferred route's measured projected headroom, or `None` when it is
    /// unknown or stale. An unknown reading never triggers a return: the same
    /// "never migrate on missing data" rule `seat::decide` documents.
    pub preferred_headroom_pct: Option<f64>,
    /// The seat's own measured headroom, the number the preferred route has
    /// to beat by `min_candidate_headroom_pct`.
    pub seat_headroom_pct: Option<f64>,
    pub min_candidate_headroom_pct: f64,
    pub cooldown_secs: u64,
    /// The supervisor's verified turn boundary. A return is a convenience and
    /// always waits for one.
    pub idle: bool,
}

/// Item 8: should the seat go back to the route it was displaced from?
///
/// PURE. Four gates, in the order an operator would ask them:
///
/// 1. There has to be somewhere to return TO -- a `seat::Displaced` entry.
///    A seat that was never displaced has no home to reclaim.
/// 2. Hysteresis: the preferred route must read MEASURED headroom at least
///    `min_candidate_headroom_pct` above the seat's own, an idle boundary must
///    have been observed, and `cooldown_secs` must have elapsed since the last
///    rollover. This is the same anti-flap discipline `seat::decide` applies
///    in the other direction, and it is why a route that keeps oscillating
///    around the threshold cannot drag the seat back and forth.
/// 3. Authority: the return is validated like any other successor, so an
///    unauthenticated, ineligible or differently-billed home route is REFUSED
///    with the reason rather than silently taken. Moving work onto metered API
///    credit because a subscription recovered is still a billing decision.
/// 4. Only then, return.
pub fn plan_return(inputs: &ReturnInputs<'_>, demand: &Demand) -> ReturnVerdict {
    let Some(displaced) = inputs.seat.displaced.as_ref() else {
        return ReturnVerdict::Hold(
            "this seat was never displaced; it has no preferred route to return to".to_string(),
        );
    };
    if !matches!(inputs.seat.phase, seat::Phase::Idle) {
        return ReturnVerdict::Hold(format!(
            "the seat is not idle ({:?}); a return always waits",
            inputs.seat.phase
        ));
    }
    if !inputs.idle {
        return ReturnVerdict::Hold("idle boundary".to_string());
    }
    if let Some(last) = inputs.seat.last_rollover_at
        && inputs.now.saturating_sub(last) < inputs.cooldown_secs
    {
        return ReturnVerdict::Hold("cooldown".to_string());
    }
    let Some(preferred_pct) = inputs.preferred_headroom_pct else {
        return ReturnVerdict::Hold(
            "the preferred route has no measured headroom reading; a return never moves a seat \
             on missing data"
                .to_string(),
        );
    };
    let seat_pct = inputs.seat_headroom_pct.unwrap_or(0.0);
    if preferred_pct < seat_pct + inputs.min_candidate_headroom_pct {
        return ReturnVerdict::Hold(format!(
            "hysteresis: the preferred route reads {preferred_pct:.1}% against the seat's \
             {seat_pct:.1}%, which does not clear the {:.1}% margin",
            inputs.min_candidate_headroom_pct
        ));
    }
    // The billing/authority gate runs through the same `validate` every other
    // successor does, so a return cannot take a shortcut a forward rollover
    // would have been refused for. The continuation is trivially legal here
    // (a return resumes the parked conversation), so a rebuilt plan stands in.
    let direction = match direction(inputs.seat.runtime, displaced.runtime) {
        Ok(direction) => direction,
        Err(error) => return ReturnVerdict::Hold(error.to_string()),
    };
    let plan = ContinuationPlan::Rebuilt {
        checkpoint: None,
        messages: Vec::new(),
    };
    match validate(inputs.preferred, demand, &plan, direction) {
        Ok(_) => ReturnVerdict::Return {
            route: displaced.agent.clone(),
            runtime: displaced.runtime,
        },
        Err(refusal) => ReturnVerdict::Refuse(refusal),
    }
}

/// The billing postures a rollover is authorized to move work ONTO by
/// default: whatever the seat is already billed as, plus `Local` (a local
/// runtime has no credential, no invoice and no ceiling, so moving onto one
/// spends nothing).
///
/// Stated as a function rather than left implicit because the default is the
/// whole authority rule: `Demand::authorizes` treats an EMPTY set as "no
/// billing constraint was stated", which is the right pre-N18 behaviour for
/// callers that never thought about billing and exactly the wrong default for
/// a rollover, whose entire job is to move work somewhere else. An operator
/// who wants a subscription seat to fail over onto metered API credit widens
/// this at the call site; nothing widens it on their behalf.
pub fn default_authorized_billing(current: BillingPosture) -> BTreeSet<BillingPosture> {
    [current, BillingPosture::Local].into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::route::{Capability, PolicyVerdict, RouteIdentity, RouteOffer};
    use crate::commands::ctx::runtime::journal::{
        ExecutionId, PolicyProvenance, SessionIdentity, ToolCallId,
    };
    use crate::commands::ctx::runtime::testsupport;

    fn offer(
        provider: &str,
        model: &str,
        billing: BillingPosture,
        runtime: route::RuntimeKind,
    ) -> RouteOffer {
        RouteOffer {
            identity: RouteIdentity {
                runtime,
                provider: provider.to_string(),
                endpoint: provider.to_string(),
                credential: format!("{provider}-account"),
                model: Some(model.to_string()),
                pool: provider.to_string(),
            },
            capabilities: [Capability::ToolCalls, Capability::Streaming]
                .into_iter()
                .collect(),
            billing,
            context_window_tokens: Some(200_000),
            readings: Vec::new(),
            policy: PolicyVerdict::Allowed,
        }
    }

    fn facts(offer: &RouteOffer) -> SuccessorFacts<'_> {
        SuccessorFacts {
            offer,
            authenticated: true,
            budget_tokens: None,
            started: true,
        }
    }

    fn demand() -> Demand {
        Demand {
            capabilities: [Capability::ToolCalls].into_iter().collect(),
            context_tokens: 10_000,
            authorized_billing: BTreeSet::new(),
            preferred: None,
        }
    }

    fn seat_at(short: &str, runtime: RuntimeKind) -> seat::Seat {
        seat::Seat {
            short: short.to_string(),
            session: "session".to_string(),
            generation: 3,
            agent: "native".to_string(),
            model: Some("claude-sonnet-4-5".to_string()),
            provider: "anthropic".to_string(),
            role: "orchestrator".to_string(),
            pinned: false,
            phase: seat::Phase::Idle,
            visited: Vec::new(),
            last_rollover_at: None,
            pending: None,
            displaced: None,
            created_at: 1,
            updated_at: 1,
            runtime,
        }
    }

    #[test]
    fn every_runtime_pair_has_a_named_direction_and_unknown_is_an_error() {
        assert_eq!(
            direction(RuntimeKind::Harness, RuntimeKind::Harness).expect("h->h"),
            Direction::HarnessToHarness
        );
        assert_eq!(
            direction(RuntimeKind::Harness, RuntimeKind::Native).expect("h->n"),
            Direction::HarnessToNative
        );
        assert_eq!(
            direction(RuntimeKind::Native, RuntimeKind::Harness).expect("n->h"),
            Direction::NativeToHarness
        );
        assert_eq!(
            direction(RuntimeKind::Native, RuntimeKind::Native).expect("n->n"),
            Direction::NativeToNative
        );
        assert!(direction(RuntimeKind::Unknown, RuntimeKind::Native).is_err());
        assert!(Direction::HarnessToNative.crosses_runtime());
        assert!(!Direction::NativeToNative.crosses_runtime());
    }

    /// Item 5: a provider's opaque envelope is only ever kept for a
    /// native->native move on the identical route. Every other direction
    /// rebuilds, and a harness successor never sees an envelope at all.
    #[test]
    fn only_a_same_route_native_successor_keeps_the_provider_envelope() {
        let same = ContinuationPlan::SameRoute { checkpoint: None };
        let rebuilt = ContinuationPlan::Rebuilt {
            checkpoint: None,
            messages: Vec::new(),
        };
        assert_eq!(
            continuation_for(Direction::NativeToNative, &same),
            Some(Continuation::ProviderEnvelope)
        );
        assert_eq!(
            continuation_for(Direction::NativeToNative, &rebuilt),
            Some(Continuation::SemanticRebuild { messages: 0 })
        );
        assert_eq!(
            continuation_for(Direction::HarnessToNative, &same),
            Some(Continuation::SemanticRebuild { messages: 0 }),
            "a harness source has no native envelope to keep, whatever the plan says"
        );
        for direction in [Direction::NativeToHarness, Direction::HarnessToHarness] {
            assert_eq!(
                continuation_for(direction, &same),
                Some(Continuation::StructuralPacket),
                "{direction:?} must never hand a harness a provider envelope"
            );
        }
    }

    /// Item 3: every gate refuses BEFORE the source is given up, and each
    /// refusal names itself.
    #[test]
    fn a_successor_is_refused_for_auth_capability_budget_startup_or_billing() {
        let route = offer(
            "anthropic",
            "claude-sonnet-4-5",
            BillingPosture::Api,
            route::RuntimeKind::Native,
        );
        let plan = ContinuationPlan::Rebuilt {
            checkpoint: None,
            messages: Vec::new(),
        };

        let unauthenticated = SuccessorFacts {
            authenticated: false,
            ..facts(&route)
        };
        assert!(matches!(
            validate(
                &unauthenticated,
                &demand(),
                &plan,
                Direction::NativeToNative
            ),
            Err(Refusal::NotAuthenticated { .. })
        ));

        let mut needs_vision = demand();
        needs_vision.capabilities.insert(Capability::Vision);
        let refusal = validate(
            &facts(&route),
            &needs_vision,
            &plan,
            Direction::NativeToNative,
        )
        .expect_err("no vision");
        assert!(refusal.label().contains("vision"), "{refusal}");

        let poor = SuccessorFacts {
            budget_tokens: Some(100),
            ..facts(&route)
        };
        assert!(matches!(
            validate(&poor, &demand(), &plan, Direction::NativeToNative),
            Err(Refusal::OverBudget { .. })
        ));

        let dead = SuccessorFacts {
            started: false,
            ..facts(&route)
        };
        assert!(matches!(
            validate(&dead, &demand(), &plan, Direction::NativeToNative),
            Err(Refusal::StartupFailed { .. })
        ));

        // Item 8's money rule: a subscription seat does not slide onto metered
        // API credit just because the API route is healthy.
        let mut subscription_only = demand();
        subscription_only.authorized_billing =
            default_authorized_billing(BillingPosture::Subscription);
        let refusal = validate(
            &facts(&route),
            &subscription_only,
            &plan,
            Direction::HarnessToNative,
        )
        .expect_err("unauthorized billing");
        assert!(
            refusal.label().contains("not authorized"),
            "the refusal must name the billing authority: {refusal}"
        );

        // And the whole thing passes when every gate is clear.
        let admitted = validate(&facts(&route), &demand(), &plan, Direction::HarnessToNative)
            .expect("admitted");
        assert_eq!(admitted.direction, Direction::HarnessToNative);
        assert_eq!(
            admitted.continuation,
            Continuation::SemanticRebuild { messages: 0 }
        );
    }

    /// Item 2 and criterion 4, on a real journal: a tool whose effect BEGAN
    /// becomes outcome-unknown and halts the successor, a tool that was only
    /// prepared is explicitly cancelled at a forced boundary, and the
    /// checkpoint carries the acknowledged input and the claim.
    #[test]
    fn the_boundary_reconciles_started_effects_and_cancels_ones_that_never_began() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut journal = Journal::open(&state).expect("journal");
        let route = testsupport::route_identity();
        let identity: SessionIdentity = testsupport::session_identity("rollover-source", route);
        let session = identity.session.clone();
        journal.create_session(&identity).expect("create");
        let scope = Default::default();

        journal
            .acknowledge_input(
                &session,
                1,
                &scope,
                super::super::runtime::journal::MessageId::new("m1").expect("id"),
                "ship the release".to_string(),
                false,
                None,
                10,
            )
            .expect("input");
        for (call, execution, start) in [("call-a", "exec-a", true), ("call-b", "exec-b", false)] {
            journal
                .prepare_tool_call(
                    &session,
                    1,
                    &scope,
                    ToolCallId::new(call).expect("id"),
                    "write_file".to_string(),
                    serde_json::json!({ "path": "x.rs" }),
                    PolicyProvenance {
                        fingerprint: "fp".to_string(),
                        source: "test".to_string(),
                        decision: "allow".to_string(),
                        scope: "repo".to_string(),
                    },
                    None,
                    11,
                )
                .expect("tool call");
            journal
                .prepare_execution(
                    &session,
                    1,
                    &scope,
                    ExecutionId::new(execution).expect("id"),
                    ToolCallId::new(call).expect("id"),
                    None,
                    12,
                )
                .expect("execution");
            if start {
                journal
                    .transition_execution(
                        &session,
                        1,
                        &scope,
                        &ExecutionId::new(execution).expect("id"),
                        ExecutionState::Started,
                        None,
                        None,
                        None,
                        13,
                    )
                    .expect("started");
            }
        }

        let context = CheckpointContext {
            hard_constraints: vec!["never force-push".to_string()],
            task: Some("task-1".to_string()),
            workflow: None,
            reason: "rollover".to_string(),
        };
        let boundary = reach_boundary(&mut journal, &session, 1, Drain::Forced, &context, 20)
            .expect("boundary");

        assert!(!boundary.drained);
        assert_eq!(
            boundary.outcome_unknown,
            vec!["call-a".to_string()],
            "an effect that began is carried as unknown, never replayed"
        );
        assert!(boundary.halts_successor());
        assert!(
            boundary
                .reconciliation_note()
                .expect("a note")
                .contains("reconciled")
        );
        assert_eq!(
            boundary.cancelled,
            vec!["call-b".to_string()],
            "a tool that never began is explicitly cancelled, which is honest"
        );
        assert_eq!(
            boundary.pending_input,
            vec!["m1".to_string()],
            "acknowledged input the source never delivered is still owed"
        );
        assert!(boundary.checkpoint.is_some());

        // The checkpoint is a durable HANDOFF checkpoint and it survives a
        // reopen -- criterion 2's "nothing acknowledged is lost".
        let stored = checkpoint::latest_valid(&journal, &session, CheckpointKind::Handoff)
            .expect("read back")
            .expect("a handoff checkpoint was committed");
        assert_eq!(stored.objective.as_deref(), Some("ship the release"));
        assert_eq!(stored.hard_constraints, vec!["never force-push".to_string()]);
        assert_eq!(stored.task.as_deref(), Some("task-1"));
        assert!(
            stored
                .outstanding_tools
                .iter()
                .any(|tool| tool.tool_call == "call-a" && tool.state == "outcome_unknown")
        );
    }

    /// A quiesced boundary cancels nothing: work that finished is finished,
    /// and there is nothing in flight to cut off.
    #[test]
    fn a_quiesced_boundary_cancels_nothing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut journal = Journal::open(&state).expect("journal");
        let identity = testsupport::session_identity("quiet-source", testsupport::route_identity());
        let session = identity.session.clone();
        journal.create_session(&identity).expect("create");
        journal
            .acknowledge_input(
                &session,
                1,
                &Default::default(),
                super::super::runtime::journal::MessageId::new("m1").expect("id"),
                "do the thing".to_string(),
                false,
                None,
                10,
            )
            .expect("input");
        let boundary = reach_boundary(
            &mut journal,
            &session,
            1,
            Drain::Quiesced,
            &CheckpointContext::default(),
            20,
        )
        .expect("boundary");
        assert!(boundary.drained);
        assert!(boundary.cancelled.is_empty());
        assert!(!boundary.halts_successor());
        assert!(boundary.reconciliation_note().is_none());
    }

    /// Item 6: a live subagent is retained under recorded ownership at a
    /// quiesced boundary and stopped at a forced one -- never described as
    /// migrated.
    #[test]
    fn native_subagents_are_finished_stopped_or_retained_but_never_migrated() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo");
        let cfg = super::super::config::CtxConfig::default();

        let handle = |id: &str| delegation::WorkerHandle {
            delegation: id.to_string(),
            attempt: 1,
            runtime: RuntimeKind::Native,
            worker_session: format!("{id}-worker"),
            short: "seatshrt".to_string(),
            role: super::super::team::IMPLEMENTER.to_string(),
            task: Some(format!("task-{id}")),
            group: None,
            objective: None,
            workdir: repo.clone(),
        };
        delegation::record_launch(&state, &repo, handle("live"), None, 10).expect("launch");
        delegation::record_launch(&state, &repo, handle("done"), None, 11).expect("launch");
        delegation::publish_terminal(
            &state,
            &repo,
            &cfg,
            "done",
            delegation::Phase::Completed,
            Some(0),
            None,
            None,
            12,
        )
        .expect("terminal");

        let quiesced = settle_subagents(&state, &repo, "seatshrt", None, Drain::Quiesced, 20);
        assert_eq!(
            quiesced
                .iter()
                .map(|(id, verdict)| (id.as_str(), verdict.as_str()))
                .collect::<Vec<_>>(),
            vec![("live", "retained"), ("done", "finished")]
        );
        assert!(matches!(
            quiesced[0].1,
            Disposition::Retained { ref owner } if owner == "seatshrt"
        ));

        let forced = settle_subagents(&state, &repo, "seatshrt", None, Drain::Forced, 21);
        assert_eq!(forced[0].1.as_str(), "stopped");
        let record = delegation::load(&state, &repo, "live").expect("record");
        assert!(
            record.cancel_requested,
            "a stopped worker is interrupted through the service that owns cancellation"
        );
    }

    /// Item 7 and criterion 6: the record names the trigger, every route
    /// tried, the decision and the outcome, and the status line keeps the
    /// logical seat identity while reporting the new backend and reason.
    #[test]
    fn the_rollover_record_carries_the_trigger_tried_routes_and_outcome() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let seat = seat_at("seatshrt", RuntimeKind::Harness);
        let mut record = Record::open(
            &seat,
            Trigger::EndpointFailure,
            "the source route's endpoint is unreachable",
            100,
        );
        record.direction = Some(Direction::HarnessToNative);
        record.refused(
            &Refusal::NotAuthenticated {
                route: "openai/gpt5@openai".to_string(),
            },
            RuntimeKind::Native,
            101,
        );
        record.admitted(
            "anthropic/claude-sonnet-4-5@anthropic",
            RuntimeKind::Native,
            "admitted with a rebuilt semantic history",
            102,
        );
        record.settle(
            Settlement::Committed {
                route: "anthropic/claude-sonnet-4-5@anthropic".to_string(),
                generation: 4,
            },
            103,
        );
        store(&state, &record).expect("store");

        let reread = load(&state, "seatshrt").expect("durable");
        assert_eq!(reread, record);
        assert_eq!(reread.attempts.len(), 2);
        assert_eq!(reread.attempts[0].outcome, "refused");
        assert_eq!(reread.attempts[1].outcome, "admitted");

        let line = reread.status_line();
        assert!(line.contains("seat seatshrt"), "{line}");
        assert!(line.contains("generation 3"), "{line}");
        assert!(line.contains("endpoint-failure"), "{line}");
        assert!(line.contains("harness->native"), "{line}");
        assert!(line.contains("generation 4"), "{line}");

        forget(&state, "seatshrt");
        assert!(load(&state, "seatshrt").is_none());
    }

    /// Item 7's failure half: a rollover whose preparation failed says the
    /// original session was kept, not that the seat moved.
    #[test]
    fn a_failed_preparation_is_recorded_as_the_original_session_retained() {
        let seat = seat_at("seatshrt", RuntimeKind::Native);
        let mut record = Record::open(&seat, Trigger::UsageExhaustion, "source is exhausted", 100);
        record.refused(
            &Refusal::StartupFailed {
                route: "anthropic/claude@anthropic".to_string(),
            },
            RuntimeKind::Harness,
            101,
        );
        record.settle(
            Settlement::Restored {
                reason: "the successor never started; the source session still holds the seat"
                    .to_string(),
            },
            102,
        );
        assert!(matches!(
            record.settlement,
            Some(Settlement::Restored { .. })
        ));
        assert!(
            record.status_line().contains("kept the original session"),
            "{}",
            record.status_line()
        );
    }

    /// Item 8: the return waits for hysteresis and refuses an unauthorized
    /// billing change rather than taking it.
    #[test]
    fn a_return_respects_hysteresis_and_refuses_an_unauthorized_billing_change() {
        let mut seat = seat_at("seatshrt", RuntimeKind::Native);
        seat.displaced = Some(seat::Displaced {
            agent: "claude".to_string(),
            model: Some("opus".to_string()),
            session: "old-session".to_string(),
            conversation: Some("conv-1".to_string()),
            runtime: RuntimeKind::Harness,
            since: 50,
        });
        seat.last_rollover_at = Some(100);
        let home = offer(
            "anthropic",
            "opus",
            BillingPosture::Api,
            route::RuntimeKind::Harness,
        );
        let home_facts = facts(&home);

        let base = ReturnInputs {
            seat: &seat,
            now: 1_000,
            preferred: &home_facts,
            preferred_headroom_pct: Some(80.0),
            seat_headroom_pct: Some(60.0),
            min_candidate_headroom_pct: 10.0,
            cooldown_secs: 600,
            idle: true,
        };

        // Mid-turn: always wait.
        assert!(matches!(
            plan_return(&ReturnInputs { idle: false, ..base }, &demand()),
            ReturnVerdict::Hold(ref reason) if reason == "idle boundary"
        ));
        // Inside the cooldown: wait.
        assert!(matches!(
            plan_return(&ReturnInputs { now: 200, ..base }, &demand()),
            ReturnVerdict::Hold(ref reason) if reason == "cooldown"
        ));
        // Only marginally better: hysteresis holds the seat where it is.
        assert!(matches!(
            plan_return(
                &ReturnInputs {
                    preferred_headroom_pct: Some(65.0),
                    ..base
                },
                &demand()
            ),
            ReturnVerdict::Hold(ref reason) if reason.contains("hysteresis")
        ));
        // No measured reading at all: never move on missing data.
        assert!(matches!(
            plan_return(
                &ReturnInputs {
                    preferred_headroom_pct: None,
                    ..base
                },
                &demand()
            ),
            ReturnVerdict::Hold(_)
        ));

        // Clear on capacity, but the home route is billed in a way this work
        // is not authorized for: a typed refusal, not a silent spend.
        let mut subscription_only = demand();
        subscription_only.authorized_billing =
            default_authorized_billing(BillingPosture::Subscription);
        assert!(matches!(
            plan_return(&base, &subscription_only),
            ReturnVerdict::Refuse(Refusal::Ineligible { .. })
        ));

        // Authorized and clear: go home, onto the runtime it was displaced
        // from.
        assert_eq!(
            plan_return(&base, &demand()),
            ReturnVerdict::Return {
                route: "claude".to_string(),
                runtime: RuntimeKind::Harness,
            }
        );

        // A seat that was never displaced has no home to reclaim.
        let mut never = seat.clone();
        never.displaced = None;
        assert!(matches!(
            plan_return(
                &ReturnInputs {
                    seat: &never,
                    ..base
                },
                &demand()
            ),
            ReturnVerdict::Hold(_)
        ));
    }

    #[test]
    fn the_default_billing_authority_is_what_the_seat_already_spends_plus_local() {
        let authorized = default_authorized_billing(BillingPosture::Subscription);
        assert!(authorized.contains(&BillingPosture::Subscription));
        assert!(authorized.contains(&BillingPosture::Local));
        assert!(
            !authorized.contains(&BillingPosture::Api),
            "metered API credit is never authorized on the seat's behalf"
        );
    }

    #[test]
    fn a_trigger_is_derived_from_the_cause_and_the_route_health_answer() {
        assert_eq!(
            Trigger::from_cause(&seat::Cause::Manual, false),
            Trigger::ManualHandover
        );
        assert_eq!(
            Trigger::from_cause(
                &seat::Cause::Reclaim {
                    headroom_pct: 80.0,
                    observed_at: 1
                },
                false
            ),
            Trigger::CapacityReturn
        );
        let reactive = seat::Cause::Reactive {
            detail: "blocked".to_string(),
            observed_at: 1,
        };
        assert_eq!(
            Trigger::from_cause(&reactive, false),
            Trigger::UsageExhaustion
        );
        assert_eq!(
            Trigger::from_cause(&reactive, true),
            Trigger::EndpointFailure,
            "only the route-health answer separates an outage from an exhausted account"
        );
    }

    // -- the four directions x four triggers -------------------------------

    /// The four causes the acceptance criteria name, with the seat inputs
    /// that produce each.
    fn triggers() -> Vec<(&'static str, seat::Cause, bool)> {
        vec![
            (
                "usage exhaustion",
                seat::Cause::Proactive {
                    headroom_pct: 3.0,
                    observed_at: 500,
                },
                false,
            ),
            (
                "endpoint failure",
                seat::Cause::Reactive {
                    detail: "the route's endpoint refused every connection".to_string(),
                    observed_at: 500,
                },
                true,
            ),
            ("manual handover", seat::Cause::Manual, false),
            (
                // The startup-failure case shares the reactive cause; what
                // makes it different is that the successor never comes up,
                // which is `SuccessorFacts::started`.
                "successor startup failure",
                seat::Cause::Reactive {
                    detail: "the account is out of capacity".to_string(),
                    observed_at: 500,
                },
                false,
            ),
        ]
    }

    fn runtimes() -> [(RuntimeKind, RuntimeKind); 4] {
        [
            (RuntimeKind::Harness, RuntimeKind::Harness),
            (RuntimeKind::Harness, RuntimeKind::Native),
            (RuntimeKind::Native, RuntimeKind::Harness),
            (RuntimeKind::Native, RuntimeKind::Native),
        ]
    }

    /// Registers a seat at `source`, and -- for a native source -- a real
    /// journal session holding one acknowledged input, one completed action
    /// with a receipt, one held task claim and one effect that began and never
    /// reported.
    fn source_session(
        state: &StateDir,
        session: &str,
        source: RuntimeKind,
    ) -> (String, Option<JournalSessionId>) {
        use super::super::runtime::journal::{
            ContentRef, ExecutionId, MessageId, PolicyProvenance, TaskId, TaskReceiptState,
            ToolCallId,
        };
        let short = super::super::sessions::short_id(session);
        seat::register(
            state,
            &short,
            session,
            if source == RuntimeKind::Native {
                "native"
            } else {
                "claude"
            },
            Some("standard"),
            "anthropic",
            "orchestrator",
            false,
            400,
        )
        .expect("register");
        let mut record = seat::load(state, &short).expect("seat");
        record.runtime = source;
        seat::store(state, &record).expect("store");
        super::super::sessions::record_conversation_on(
            state,
            &short,
            &record.agent,
            session,
            "source-conversation",
            source,
        );
        if source != RuntimeKind::Native {
            return (short, None);
        }

        let mut journal = Journal::open(state).expect("journal");
        let identity = testsupport::session_identity(session, testsupport::route_identity());
        let journal_session = identity.session.clone();
        journal.create_session(&identity).expect("create");
        let scope = Default::default();
        let policy = PolicyProvenance {
            fingerprint: "fp".to_string(),
            source: "test".to_string(),
            decision: "allow".to_string(),
            scope: "repo".to_string(),
        };
        journal
            .acknowledge_input(
                &journal_session,
                1,
                &scope,
                MessageId::new("m1").expect("id"),
                "cut the release".to_string(),
                false,
                None,
                401,
            )
            .expect("input");
        journal
            .record_task_receipt(
                &journal_session,
                1,
                &scope,
                TaskId::new("task-1").expect("id"),
                TaskReceiptState::Started,
                serde_json::json!({ "note": "claimed" }),
                402,
            )
            .expect("claim");
        for (call, execution, finished) in [("done", "exec-done", true), ("mid", "exec-mid", false)]
        {
            journal
                .prepare_tool_call(
                    &journal_session,
                    1,
                    &scope,
                    ToolCallId::new(call).expect("id"),
                    "write_file".to_string(),
                    serde_json::json!({ "path": "x.rs" }),
                    policy.clone(),
                    None,
                    403,
                )
                .expect("call");
            journal
                .prepare_execution(
                    &journal_session,
                    1,
                    &scope,
                    ExecutionId::new(execution).expect("id"),
                    ToolCallId::new(call).expect("id"),
                    None,
                    404,
                )
                .expect("execution");
            journal
                .transition_execution(
                    &journal_session,
                    1,
                    &scope,
                    &ExecutionId::new(execution).expect("id"),
                    ExecutionState::Started,
                    None,
                    None,
                    None,
                    405,
                )
                .expect("started");
            if finished {
                journal
                    .transition_execution(
                        &journal_session,
                        1,
                        &scope,
                        &ExecutionId::new(execution).expect("id"),
                        ExecutionState::Completed,
                        Some(ContentRef::Inline {
                            text: "ok".to_string(),
                        }),
                        None,
                        None,
                        406,
                    )
                    .expect("completed");
            }
        }
        (short, Some(journal_session))
    }

    /// Criteria 1 and 2, for every direction and every trigger that moves the
    /// seat: the transaction commits onto the successor's runtime, the seat
    /// keeps its logical identity, the source is parked with its own
    /// conversation, and nothing acknowledged, claimed or receipted is lost.
    #[test]
    fn every_direction_and_trigger_commits_without_losing_acknowledged_state() {
        let route = offer(
            "anthropic",
            "claude-sonnet-4-5",
            BillingPosture::Api,
            route::RuntimeKind::Native,
        );
        let plan = ContinuationPlan::Rebuilt {
            checkpoint: None,
            messages: Vec::new(),
        };
        for (index, (source, target)) in runtimes().into_iter().enumerate() {
            for (slot, (label, cause, unreachable)) in triggers().into_iter().enumerate() {
                if label == "successor startup failure" {
                    continue; // its own test, below
                }
                let tmp = tempfile::tempdir().expect("tempdir");
                let state = StateDir::from_root(tmp.path().join("state"));
                let session = format!("{index:04x}{slot:04x}-1111-4000-8000-000000000000");
                let (short, journal_session) = source_session(&state, &session, source);

                let drain = if matches!(cause, seat::Cause::Reactive { .. }) {
                    Drain::Forced
                } else {
                    Drain::Quiesced
                };
                let boundary = journal_session.as_ref().map(|journal_session| {
                    let mut journal = Journal::open(&state).expect("journal");
                    reach_boundary(
                        &mut journal,
                        journal_session,
                        1,
                        drain,
                        &CheckpointContext {
                            hard_constraints: vec!["never force-push".to_string()],
                            task: Some("task-1".to_string()),
                            workflow: None,
                            reason: "rollover".to_string(),
                        },
                        500,
                    )
                    .expect("boundary")
                });

                let seat_before = seat::load(&state, &short).expect("seat");
                let moving = direction(source, target).expect("direction");
                let mut ledger = Record::open(
                    &seat_before,
                    Trigger::from_cause(&cause, unreachable),
                    label,
                    500,
                );
                ledger.direction = Some(moving);
                ledger.boundary = boundary.clone();

                let admitted = validate(&facts(&route), &demand(), &plan, moving)
                    .expect("the successor clears every gate");
                assert_eq!(admitted.direction, moving);
                let generation = seat::prepare_onto(
                    &state,
                    &short,
                    "successor",
                    Some("standard"),
                    target,
                    cause.clone(),
                    500,
                )
                .expect("prepare");
                // Item 3: the successor may not write before the commit, and
                // the fence is what says so.
                assert!(seat::guard(&state, &short, generation).is_err());
                let committed = seat::commit(&state, &short, generation, "successor-session", 501)
                    .expect("commit");
                ledger.admitted("successor", target, label, 501);
                ledger.settle(
                    Settlement::Committed {
                        route: "successor".to_string(),
                        generation: committed.generation,
                    },
                    501,
                );
                store(&state, &ledger).expect("store");

                // Criterion 6: the LOGICAL seat is the same, and the record
                // says what answers at it now and why.
                assert_eq!(committed.short, seat_before.short, "{label} {moving:?}");
                assert_eq!(committed.runtime, target, "{label} {moving:?}");
                let line = load(&state, &short).expect("record").status_line();
                assert!(line.contains(&short), "{line}");
                assert!(line.contains(moving.as_str()), "{line}");

                // Criterion 2: for a native source, the checkpoint carries the
                // acknowledged input, the claim and the receipt across, and
                // criterion 4 halts the successor on the unsettled effect.
                if let Some(boundary) = boundary {
                    assert_eq!(
                        boundary.pending_input,
                        vec!["m1".to_string()],
                        "{label} {moving:?}: acknowledged input is never lost"
                    );
                    assert_eq!(boundary.claims, vec!["task-1".to_string()]);
                    assert_eq!(boundary.receipts, 1);
                    assert_eq!(boundary.outcome_unknown, vec!["mid".to_string()]);
                    assert!(boundary.halts_successor());
                }

                // Item 5: a displaced source keeps its OWN conversation
                // reference, under the runtime that reference belongs to. A
                // manual swap is the operator's own decision to move and is
                // owed no return, which is existing behaviour.
                match committed.displaced {
                    Some(displaced) => {
                        assert!(!matches!(cause, seat::Cause::Manual), "{label}");
                        assert_eq!(displaced.runtime, source, "{label} {moving:?}");
                        assert_eq!(
                            displaced.conversation.as_deref(),
                            Some("source-conversation"),
                            "{label} {moving:?}"
                        );
                    }
                    None => assert!(matches!(cause, seat::Cause::Manual), "{label}"),
                }
            }
        }
    }

    /// Criterion 5, for every direction: a successor that never comes up is
    /// refused BEFORE the seat moves, the source keeps the seat and its own
    /// conversation, and no conversation id is ever resumed under the wrong
    /// runtime.
    #[test]
    fn an_inaccessible_successor_never_takes_the_seat_or_the_wrong_conversation() {
        let route = offer(
            "anthropic",
            "claude-sonnet-4-5",
            BillingPosture::Api,
            route::RuntimeKind::Native,
        );
        let dead = SuccessorFacts {
            started: false,
            ..facts(&route)
        };
        let plan = ContinuationPlan::Rebuilt {
            checkpoint: None,
            messages: Vec::new(),
        };
        for (index, (source, target)) in runtimes().into_iter().enumerate() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = StateDir::from_root(tmp.path().join("state"));
            let session = format!("{index:04x}dead-2222-4000-8000-000000000000");
            let (short, _) = source_session(&state, &session, source);
            let before = seat::load(&state, &short).expect("seat");

            let moving = direction(source, target).expect("direction");
            let refusal = validate(&dead, &demand(), &plan, moving).expect_err("did not start");
            assert!(matches!(refusal, Refusal::StartupFailed { .. }));

            let mut ledger = Record::open(&before, Trigger::UsageExhaustion, "roll over", 500);
            ledger.direction = Some(moving);
            ledger.refused(&refusal, target, 501);
            ledger.settle(
                Settlement::Restored {
                    reason: "the successor never started".to_string(),
                },
                501,
            );
            store(&state, &ledger).expect("store");

            // The seat never moved: same generation, same agent, same runtime,
            // still idle and still able to prepare again.
            let after = seat::load(&state, &short).expect("seat");
            assert_eq!(after.generation, before.generation, "{moving:?}");
            assert_eq!(after.agent, before.agent, "{moving:?}");
            assert_eq!(after.runtime, source, "{moving:?}");
            assert!(matches!(after.phase, seat::Phase::Idle), "{moving:?}");
            assert!(after.displaced.is_none(), "{moving:?}");

            // And the source's own conversation is still resolvable under the
            // runtime it belongs to -- and under no other.
            let wrong = if source == RuntimeKind::Native {
                RuntimeKind::Harness
            } else {
                RuntimeKind::Native
            };
            assert_eq!(
                super::super::sessions::native_conversation(
                    &state,
                    &short,
                    &after.agent,
                    &session,
                    source
                )
                .as_deref(),
                Some("source-conversation"),
                "{moving:?}: the source session's recoverable state survives"
            );
            assert_eq!(
                super::super::sessions::native_conversation(
                    &state, &short, &after.agent, &session, wrong
                ),
                None,
                "{moving:?}: a conversation id is never resumed under the wrong runtime"
            );

            let line = load(&state, &short).expect("record").status_line();
            assert!(line.contains("kept the original session"), "{line}");
        }
    }
}
