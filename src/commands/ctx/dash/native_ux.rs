//! Issue #490 (roadmap N21): the native harness's multi-agent attention,
//! evidence and rollover experience -- the view model N11's conversation
//! pane (`dash::native_pane`) deliberately stopped short of.
//!
//! # Why a second module rather than more of `native_pane`
//!
//! `native_pane` owns ONE conversation: its reducer turns one session's
//! journal into one transcript, and its presentation state is that pane's
//! scroll/composer/focus. Everything this module adds is about the work
//! AROUND that conversation -- the other agents, their tasks, the evidence
//! they produced, the capacity they are spending, the approvals they are
//! blocked on, and the rollovers/compactions/reconnects that move a seat
//! from one session to another underneath all of it. Those are fed by
//! completely different authorities (`coordinator`, `delegation`, `seat`,
//! `rollover_runtime`, `pool`), so mixing them into `native_pane` would make
//! its one clean "journal in, transcript out" contract answer to five more
//! record types.
//!
//! # The one rule every builder here follows
//!
//! **Authoritative records in, view model out -- never a transcript.** Every
//! `build_*` function below takes already-loaded, already-durable records
//! (a `coordinator::Coordinator`, a slice of `delegation::Record`, a slice of
//! `seat::Seat`, a `pool::PoolView`, a `delegation::Manifest`) and returns a
//! plain data structure. None of them read a JSONL transcript, replay a
//! journal, call the clock, or touch the network; `now` is a parameter
//! wherever elapsed time matters. That is what lets every behaviour in this
//! file -- including the cross-platform/accessibility/performance ones,
//! which have no terminal to run in -- be a deterministic unit test.
//!
//! Worker inspection ([`build_inspection`]) is the sharpest case: it reads a
//! `delegation::Manifest` -- the BOUNDED result N10 already publishes -- and
//! never the worker's own conversation. A coordinator inspecting a worker
//! therefore pays the manifest's bytes, not the worker's transcript's, no
//! matter how long that worker ran.
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::style::{self, Tone};

use super::super::coordinator::{self, NodeState};
use super::super::delegation;
use super::super::pool;
use super::super::rollover_runtime;
use super::super::runtime::RuntimeKind;
use super::super::seat;
use super::native_pane::{QueuedInput, ScrollState, StyledLine, StyledSpan};

// =========================================================================
// Item 1: the agent / task overview
// =========================================================================

/// Where a fact came from. The whole point of carrying this alongside a
/// value is that "we measured 0" and "we have no idea" must never render the
/// same way -- see [`Measure::text`] and [`AgentRow::model_text`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provenance {
    /// The provider (or the record) told us this value.
    Measured,
    /// Derived locally from something else -- a token estimate, a wrapped
    /// adapter's declared model rather than the one it actually billed.
    Estimated,
    /// Nothing known. Never rendered as a zero.
    Unknown,
}

impl Provenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Measured => "measured",
            Self::Estimated => "estimated",
            Self::Unknown => "unknown",
        }
    }
}

/// The distinct states criterion 2 demands. `ApprovalNeeded` is deliberately
/// separate from `Blocked`: both stop the worker, but only one of them is
/// waiting on the OPERATOR, and conflating them is exactly how a fleet
/// wedges unnoticed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AgentState {
    /// Waiting for the operator to answer an approval. Ranks first.
    ApprovalNeeded,
    /// Stopped on something that is not an operator decision (a lease, a
    /// dependency, a queued message it cannot consume yet).
    Blocked,
    Failed,
    /// Terminal, with a result the operator has not opened yet.
    DoneUnread,
    Running,
    /// Planned/launched but not yet doing work (no seat, no capacity).
    Queued,
    /// On its way out: parked for a rollover, or a draining route.
    Draining,
    Done,
    Cancelled,
}

impl AgentState {
    /// One glyph per state, all distinct: no state in this module is ever
    /// distinguished by colour alone (item 7).
    pub fn glyph(self) -> &'static str {
        match self {
            Self::ApprovalNeeded => "\u{2691}", // ⚑
            Self::Blocked => "\u{23f8}",        // ⏸
            Self::Failed => "\u{2716}",         // ✖
            Self::DoneUnread => "\u{2022}",     // •
            Self::Running => "\u{23f5}",        // ⏵
            Self::Queued => "\u{29d7}",         // ⧗
            Self::Draining => "\u{25cd}",       // ◍
            Self::Done => "\u{2714}",           // ✔
            Self::Cancelled => "\u{2298}",      // ⊘
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ApprovalNeeded => "approval",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::DoneUnread => "done-unread",
            Self::Running => "running",
            Self::Queued => "queued",
            Self::Draining => "draining",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn tone(self) -> Tone {
        match self {
            Self::ApprovalNeeded | Self::Failed => Tone::Err,
            Self::Blocked | Self::DoneUnread => Tone::Warn,
            Self::Running => Tone::Accent,
            Self::Queued | Self::Draining | Self::Cancelled => Tone::Muted,
            Self::Done => Tone::Ok,
        }
    }

    /// Whether this state needs the operator before anything moves. Drives
    /// the overview's own ordering and the "what needs me" count.
    pub fn needs_operator(self) -> bool {
        matches!(self, Self::ApprovalNeeded | Self::DoneUnread | Self::Failed)
    }
}

/// Bounded access to a finished worker's result -- a pointer, never the
/// content. Opening it is [`build_inspection`]'s job.
#[derive(Clone, Debug, PartialEq)]
pub struct ResultRef {
    pub delegation: String,
    pub path: Option<PathBuf>,
    /// Bytes of the worker's own bounded summary, not of its transcript.
    pub summary_bytes: usize,
    pub artifacts: usize,
}

/// One agent in the overview. Everything here comes from a durable record:
/// the coordinator graph (role/task/state), the delegation receipt (runtime,
/// worktree, result, ownership) or the seat (model, generation, phase).
#[derive(Clone, Debug, PartialEq)]
pub struct AgentRow {
    /// The delegation id for a worker, `seat:<short>` for the operator's own
    /// seat, `task:<id>` for a planned task nobody has taken yet.
    pub id: String,
    pub short: String,
    pub role: String,
    pub task: Option<String>,
    pub model: Option<String>,
    pub model_provenance: Provenance,
    /// `native/<provider>` or `wrapped/<adapter>`; never a guess.
    pub backend: String,
    pub worktree: Option<PathBuf>,
    /// Who owns this row's work -- the parent session's short id, or
    /// `"operator"` for the seat itself.
    pub owner: String,
    pub state: AgentState,
    /// The exact decision the operator owes this agent, when it has one.
    pub pending_decision: Option<String>,
    pub result: Option<ResultRef>,
    /// Seconds in the current state, from the caller's `now`.
    pub since_secs: u64,
}

impl AgentRow {
    /// The model with its provenance, or an explicit unknown. Never blank
    /// and never a fabricated default.
    pub fn model_text(&self) -> String {
        match (&self.model, self.model_provenance) {
            (Some(model), Provenance::Measured) => format!("{model} (measured)"),
            (Some(model), Provenance::Estimated) => format!("{model} (estimated)"),
            (Some(model), Provenance::Unknown) => format!("{model} (unverified)"),
            (None, _) => format!("{} (unknown)", style::PLACEHOLDER),
        }
    }

    /// Whether opening this row's result is worth an operator's time.
    pub fn has_evidence(&self) -> bool {
        self.result.is_some()
    }
}

/// The overview panel's own state: the rows plus which one has the cursor.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Overview {
    pub rows: Vec<AgentRow>,
    selected: usize,
}

/// How many rendered lines one row occupies. Two at a comfortable width (a
/// headline plus a detail line), one when the pane is narrow -- see
/// [`resolve_layout`].
pub const OVERVIEW_ROW_LINES_WIDE: usize = 2;
pub const OVERVIEW_ROW_LINES_NARROW: usize = 1;

impl Overview {
    pub fn new(rows: Vec<AgentRow>) -> Self {
        Self { rows, selected: 0 }
    }

    pub fn selected_index(&self) -> usize {
        self.selected.min(self.rows.len().saturating_sub(1))
    }

    pub fn selected(&self) -> Option<&AgentRow> {
        self.rows.get(self.selected_index())
    }

    pub fn select_next(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = (self.selected_index() + 1) % self.rows.len();
    }

    pub fn select_prev(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = match self.selected_index() {
            0 => self.rows.len() - 1,
            n => n - 1,
        };
    }

    /// Keeps the cursor on the same AGENT across a rebuild, rather than on
    /// the same index: the overview is rebuilt from records every tick, and
    /// a row appearing above the cursor must not silently move the selection
    /// onto a different worker.
    pub fn reselect(&mut self, id: &str) -> bool {
        match self.rows.iter().position(|row| row.id == id) {
            Some(index) => {
                self.selected = index;
                true
            }
            None => false,
        }
    }

    pub fn row_lines(width: usize) -> usize {
        if width >= 60 {
            OVERVIEW_ROW_LINES_WIDE
        } else {
            OVERVIEW_ROW_LINES_NARROW
        }
    }

    /// Which row a click at rendered line `line` landed on (#354's clickable
    /// rows), or `None` past the end.
    pub fn row_at_line(&self, line: usize, width: usize) -> Option<&AgentRow> {
        let per = Self::row_lines(width);
        self.rows.get(line / per.max(1))
    }

    /// How many rows are waiting on the operator right now.
    pub fn needs_operator(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.state.needs_operator())
            .count()
    }

    pub fn lines(&self, width: usize) -> Vec<StyledLine> {
        let per = Self::row_lines(width);
        let mut out = Vec::with_capacity(self.rows.len() * per);
        for (index, row) in self.rows.iter().enumerate() {
            let cursor = if index == self.selected_index() {
                "\u{25b8}"
            } else {
                " "
            };
            let mut head = StyledLine(vec![
                StyledSpan {
                    text: format!("{cursor}{} ", row.state.glyph()),
                    tone: row.state.tone(),
                },
                StyledSpan {
                    text: format!("{:<12}", row.state.label()),
                    tone: row.state.tone(),
                },
                StyledSpan {
                    text: format!("{} \u{b7} {}", row.short, row.role),
                    tone: Tone::Emphasis,
                },
            ]);
            if let Some(decision) = &row.pending_decision {
                head.0.push(StyledSpan {
                    text: format!("  \u{2691} {decision}"),
                    tone: Tone::Err,
                });
            } else if let Some(result) = &row.result {
                head.0.push(StyledSpan {
                    text: format!(
                        "  result {}B \u{b7} {} artifacts",
                        result.summary_bytes, result.artifacts
                    ),
                    tone: Tone::Warn,
                });
            }
            out.push(head);
            if per > 1 {
                let worktree = row
                    .worktree
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| style::PLACEHOLDER.to_string());
                out.push(StyledLine(vec![StyledSpan {
                    text: format!(
                        "    {} \u{b7} {} \u{b7} task {} \u{b7} owner {} \u{b7} {}s \u{b7} {}",
                        row.model_text(),
                        row.backend,
                        row.task.as_deref().unwrap_or(style::PLACEHOLDER),
                        row.owner,
                        row.since_secs,
                        worktree,
                    ),
                    tone: Tone::Muted,
                }]));
            }
        }
        out
    }
}

/// Builds the overview from the authoritative records. Ordering is by
/// urgency ([`AgentState`]'s own declaration order) and then by short id, so
/// the rows an operator must act on are always at the top and the order
/// never flickers between two ticks with the same facts.
///
/// `approvals` supplies the one fact no durable record carries: which
/// sessions have an approval outstanding right now. Passing it in (rather
/// than querying a broker here) keeps this function pure.
pub fn build_overview(
    graph: &coordinator::Coordinator,
    records: &[delegation::Record],
    seats: &[seat::Seat],
    approvals: &[ApprovalRequest],
    now: u64,
) -> Overview {
    let mut rows: Vec<AgentRow> = Vec::new();

    for seat in seats {
        let state = match &seat.phase {
            seat::Phase::Parked { .. } => AgentState::Draining,
            seat::Phase::Prepared { .. } => AgentState::Draining,
            seat::Phase::Idle => AgentState::Running,
        };
        rows.push(AgentRow {
            id: format!("seat:{}", seat.short),
            short: seat.short.clone(),
            role: seat.role.clone(),
            task: None,
            model: seat.model.clone(),
            model_provenance: match seat.runtime {
                RuntimeKind::Native => Provenance::Measured,
                _ => Provenance::Estimated,
            },
            backend: backend_label(seat.runtime, &seat.provider),
            worktree: None,
            owner: "operator".to_string(),
            state,
            pending_decision: seat
                .pending
                .as_ref()
                .map(|pending| format!("rollover: {:?}", pending.cause)),
            result: None,
            since_secs: now.saturating_sub(seat.updated_at),
        });
    }

    for record in records {
        let approval = approvals
            .iter()
            .find(|request| request.session == record.handle.worker_session);
        let node = graph.nodes.values().find(|node| {
            node.delegation.as_deref() == Some(record.handle.delegation.as_str())
        });
        let state = classify_agent_state(record, node, approval.is_some());
        let result = record.result_path.as_ref().map(|path| ResultRef {
            delegation: record.handle.delegation.clone(),
            path: Some(path.clone()),
            summary_bytes: record.summary.as_ref().map(String::len).unwrap_or(0),
            artifacts: node.map(|node| node.evidence.len()).unwrap_or(0),
        });
        rows.push(AgentRow {
            id: record.handle.delegation.clone(),
            short: record.handle.short.clone(),
            role: record.handle.role.clone(),
            task: record.handle.task.clone(),
            model: seats
                .iter()
                .find(|seat| seat.session == record.handle.worker_session)
                .and_then(|seat| seat.model.clone()),
            model_provenance: match record.handle.runtime {
                RuntimeKind::Native => Provenance::Measured,
                _ => Provenance::Estimated,
            },
            backend: backend_label(record.handle.runtime, ""),
            worktree: Some(record.handle.workdir.clone()),
            owner: record
                .parent_session
                .clone()
                .unwrap_or_else(|| "operator".to_string()),
            state,
            pending_decision: approval.map(ApprovalRequest::scope_text),
            result,
            since_secs: now.saturating_sub(record.updated_at),
        });
    }

    for (id, node) in &graph.nodes {
        if node.delegation.is_some() || node.state.is_settled() {
            continue;
        }
        rows.push(AgentRow {
            id: format!("task:{id}"),
            short: id.clone(),
            role: node.role.clone(),
            task: Some(node.task.clone()),
            model: None,
            model_provenance: Provenance::Unknown,
            backend: node
                .runtime
                .clone()
                .unwrap_or_else(|| style::PLACEHOLDER.to_string()),
            worktree: None,
            owner: "coordinator".to_string(),
            state: AgentState::Queued,
            pending_decision: None,
            result: None,
            since_secs: now.saturating_sub(node.updated_at),
        });
    }

    rows.sort_by(|a, b| a.state.cmp(&b.state).then_with(|| a.short.cmp(&b.short)));
    Overview::new(rows)
}

fn backend_label(runtime: RuntimeKind, provider: &str) -> String {
    let family = match runtime {
        RuntimeKind::Native => "native",
        _ => "wrapped",
    };
    if provider.is_empty() {
        family.to_string()
    } else {
        format!("{family}/{provider}")
    }
}

/// The single place a worker's state is decided, so the overview, the
/// headless status and the notifications can never disagree. An outstanding
/// approval outranks the record's own phase -- the worker may well still be
/// `Running` from the fleet's point of view while it waits on a human.
fn classify_agent_state(
    record: &delegation::Record,
    node: Option<&coordinator::Node>,
    approval_pending: bool,
) -> AgentState {
    if approval_pending {
        return AgentState::ApprovalNeeded;
    }
    match record.phase {
        delegation::Phase::Failed => AgentState::Failed,
        delegation::Phase::Cancelled => AgentState::Cancelled,
        delegation::Phase::Completed => {
            if record.published.len() > record.consumed.len() {
                AgentState::DoneUnread
            } else {
                AgentState::Done
            }
        }
        delegation::Phase::Closed => AgentState::Done,
        delegation::Phase::Launched => AgentState::Queued,
        delegation::Phase::Running => {
            if !record.queued.is_empty() {
                AgentState::Blocked
            } else if node.map(|node| node.state) == Some(NodeState::Failed) {
                AgentState::Failed
            } else {
                AgentState::Running
            }
        }
    }
}

// =========================================================================
// Item 2: focused worker inspection, over the BOUNDED result
// =========================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvidenceKind {
    Summary,
    Diff,
    Tests,
    Artifact,
    Frontend,
}

impl EvidenceKind {
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Summary => "\u{2261}",  // ≡
            Self::Diff => "\u{00b1}",     // ±
            Self::Tests => "\u{2713}",    // ✓
            Self::Artifact => "\u{29c9}", // ⧉
            Self::Frontend => "\u{25a3}", // ▣
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Summary => "summary",
            Self::Diff => "diff",
            Self::Tests => "tests",
            Self::Artifact => "artifact",
            Self::Frontend => "frontend",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct EvidenceRow {
    pub kind: EvidenceKind,
    pub label: String,
    pub detail: String,
    /// What an `[o]pen` would open. `None` for evidence that is already
    /// fully shown in `detail`.
    pub open: Option<PathBuf>,
}

/// A worker's bounded manifest, rendered. Contains no part of that worker's
/// conversation: `summary_bytes` is the size of the summary the worker
/// itself published, and `truncated` says whether even that was capped.
#[derive(Clone, Debug, PartialEq)]
pub struct Inspection {
    pub delegation: String,
    pub short: String,
    pub role: String,
    pub task: Option<String>,
    pub state: AgentState,
    pub runtime: String,
    pub workdir: PathBuf,
    pub exit_code: Option<i32>,
    pub summary: Option<String>,
    pub summary_truncated: bool,
    pub summary_bytes: usize,
    pub evidence: Vec<EvidenceRow>,
    /// Messages the worker could not consume; the operator's cue that a
    /// follow-up will queue rather than land.
    pub queued_messages: usize,
}

/// The cap a single inspection's summary is allowed to occupy in the
/// coordinator's own pane. A manifest is already bounded by N10; this is the
/// presentation layer's second, independent bound, so a manifest written by
/// an older or misbehaving build still cannot flood the pane.
pub const INSPECTION_SUMMARY_CAP: usize = 4096;

/// Builds a worker inspection from the already-bounded manifest. Takes the
/// manifest by reference and copies only what it renders: nothing here opens
/// the worker's transcript, journal or rollout, which is precisely what
/// keeps a coordinator's context cost independent of how long its workers
/// ran (item 2).
pub fn build_inspection(
    record: &delegation::Record,
    manifest: &delegation::Manifest,
    state: AgentState,
) -> Inspection {
    let mut evidence = Vec::new();
    if let Some(summary) = &manifest.summary {
        evidence.push(EvidenceRow {
            kind: EvidenceKind::Summary,
            label: "bounded summary".to_string(),
            detail: format!("{} bytes", summary.len()),
            open: None,
        });
    }
    if let Some(path) = &manifest.result_path {
        evidence.push(EvidenceRow {
            kind: classify_evidence_path(path),
            label: path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            detail: "result".to_string(),
            open: Some(path.clone()),
        });
    }
    for artifact in &record.published {
        evidence.push(EvidenceRow {
            kind: EvidenceKind::Artifact,
            label: artifact.clone(),
            detail: "receipt".to_string(),
            open: None,
        });
    }
    for unknown in &manifest.unknown_tool_outcomes {
        evidence.push(EvidenceRow {
            kind: EvidenceKind::Artifact,
            label: unknown.clone(),
            detail: "unknown tool outcome".to_string(),
            open: None,
        });
    }

    let summary = manifest.summary.as_ref().map(|text| {
        let mut capped = text.clone();
        if capped.len() > INSPECTION_SUMMARY_CAP {
            let mut end = INSPECTION_SUMMARY_CAP;
            while end > 0 && !capped.is_char_boundary(end) {
                end -= 1;
            }
            capped.truncate(end);
        }
        capped
    });

    Inspection {
        delegation: manifest.delegation.clone(),
        short: record.handle.short.clone(),
        role: record.handle.role.clone(),
        task: manifest.task.clone(),
        state,
        runtime: manifest.runtime.to_string(),
        workdir: record.handle.workdir.clone(),
        exit_code: manifest.exit_code,
        summary_bytes: manifest.summary.as_ref().map(String::len).unwrap_or(0),
        summary_truncated: manifest.summary_truncated
            || manifest
                .summary
                .as_ref()
                .is_some_and(|text| text.len() > INSPECTION_SUMMARY_CAP),
        summary,
        evidence,
        queued_messages: manifest.queued_messages,
    }
}

fn classify_evidence_path(path: &Path) -> EvidenceKind {
    let name = path.to_string_lossy().to_ascii_lowercase();
    if name.ends_with(".patch") || name.ends_with(".diff") {
        EvidenceKind::Diff
    } else if name.ends_with(".png") || name.ends_with(".jpg") || name.ends_with(".webp") {
        EvidenceKind::Frontend
    } else if name.contains("test") {
        EvidenceKind::Tests
    } else {
        EvidenceKind::Artifact
    }
}

impl Inspection {
    /// The delegation a follow-up is sent to. A follow-up always addresses
    /// the WORKER (N10's `follow_up`), never the coordinator's own session:
    /// there is no path in this module that turns an inspection into input
    /// for the pane it was opened from.
    pub fn follow_up_target(&self) -> &str {
        &self.delegation
    }

    pub fn lines(&self, width: usize) -> Vec<StyledLine> {
        let mut out = vec![
            StyledLine(vec![
                StyledSpan {
                    text: format!("{} ", self.state.glyph()),
                    tone: self.state.tone(),
                },
                StyledSpan {
                    text: format!("worker {} \u{b7} {}", self.short, self.role),
                    tone: Tone::Emphasis,
                },
            ]),
            StyledLine::toned(
                format!(
                    "  task {} \u{b7} {} \u{b7} {} \u{b7} exit {}",
                    self.task.as_deref().unwrap_or(style::PLACEHOLDER),
                    self.runtime,
                    self.workdir.display(),
                    self.exit_code
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| style::PLACEHOLDER.to_string()),
                ),
                Tone::Muted,
            ),
            StyledLine::toned(
                format!(
                    "  bounded result {} bytes{} \u{b7} {} evidence \u{b7} {} queued",
                    self.summary_bytes,
                    if self.summary_truncated {
                        " (truncated)"
                    } else {
                        ""
                    },
                    self.evidence.len(),
                    self.queued_messages,
                ),
                Tone::Muted,
            ),
        ];
        for row in &self.evidence {
            out.push(StyledLine(vec![
                StyledSpan {
                    text: format!("  {} ", row.kind.glyph()),
                    tone: Tone::Accent,
                },
                StyledSpan {
                    text: format!("{:<9} ", row.kind.label()),
                    tone: Tone::Muted,
                },
                StyledSpan {
                    text: row.label.clone(),
                    tone: Tone::Plain,
                },
                StyledSpan {
                    text: format!("  {}", row.detail),
                    tone: Tone::Muted,
                },
            ]));
        }
        if let Some(summary) = &self.summary {
            for line in summary.lines().take(bounded_summary_lines(width)) {
                out.push(StyledLine::toned(format!("  {line}"), Tone::Plain));
            }
        }
        out
    }
}

fn bounded_summary_lines(width: usize) -> usize {
    if width >= 100 { 24 } else { 12 }
}

// =========================================================================
// Item 3: usage and health provenance
// =========================================================================

/// One number with its provenance. `value: None` renders as "unknown", never
/// as `0` -- the distinction criterion 5 and issue #490 item 3 both call for.
#[derive(Clone, Debug, PartialEq)]
pub struct Measure {
    pub label: String,
    pub value: Option<u64>,
    pub provenance: Provenance,
}

impl Measure {
    pub fn unknown(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: None,
            provenance: Provenance::Unknown,
        }
    }

    pub fn text(&self) -> String {
        match (self.value, self.provenance) {
            (_, Provenance::Unknown) | (None, _) => {
                format!("{}: unknown", self.label)
            }
            (Some(value), provenance) => {
                format!("{}: {value} ({})", self.label, provenance.as_str())
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RouteRow {
    pub route: String,
    pub ready: bool,
    /// Why this route is not usable, verbatim from the pool's own exclusion
    /// reason. Never summarised away: "excluded" with no reason is exactly
    /// the state an operator cannot act on.
    pub reason: Option<String>,
    pub draining: bool,
    pub headroom_pct: Option<f64>,
    pub provenance: Provenance,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SeatHealth {
    pub active: usize,
    pub parked: usize,
    pub draining: usize,
    pub billing: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageStrip {
    pub measures: Vec<Measure>,
    pub routes: Vec<RouteRow>,
    pub seats: SeatHealth,
    /// The snapshot was taken while some source was unavailable: every
    /// number below is therefore partial, and says so.
    pub degraded: bool,
}

/// Derives the provenance strip from N18's own pool view. `signal_quality`
/// and `provenance` strings come straight from `pool`/`route`; this function
/// maps them onto the three-way [`Provenance`] the UI renders and never
/// invents a value the pool did not have.
pub fn build_usage(view: &pool::PoolView, billing: &str) -> UsageStrip {
    let mut routes: Vec<RouteRow> = Vec::new();
    for harness in &view.harnesses {
        let excluded = view
            .exclusions
            .iter()
            .find(|(name, _)| name == &harness.name)
            .map(|(_, reason)| reason.clone());
        let draining = harness.state.eq_ignore_ascii_case("draining");
        let reason = excluded.or_else(|| {
            if harness.state_reason.is_empty() || harness.state.eq_ignore_ascii_case("ready") {
                None
            } else {
                Some(harness.state_reason.clone())
            }
        });
        routes.push(RouteRow {
            route: format!("{}/{}", harness.provider, harness.name),
            ready: reason.is_none() && !draining,
            reason,
            draining,
            headroom_pct: harness.headroom_pct,
            provenance: provenance_from_signal(harness.signal_quality.as_str()),
        });
    }
    // Exclusions the pool knows about for a harness that is not itself in
    // the harness list still have to appear: an operator cannot act on a
    // route that silently vanished.
    for (name, reason) in &view.exclusions {
        if routes.iter().any(|row| row.route.ends_with(name.as_str())) {
            continue;
        }
        routes.push(RouteRow {
            route: name.clone(),
            ready: false,
            reason: Some(reason.clone()),
            draining: false,
            headroom_pct: None,
            provenance: Provenance::Unknown,
        });
    }
    routes.sort_by(|a, b| a.route.cmp(&b.route));

    let reserved: u64 = view
        .providers
        .iter()
        .map(|provider| provider.reserved_tokens)
        .sum();
    let measures = vec![
        Measure {
            label: "reserved tokens".to_string(),
            value: Some(reserved),
            provenance: if view.degraded {
                Provenance::Estimated
            } else {
                Provenance::Measured
            },
        },
        Measure {
            label: "active".to_string(),
            value: Some(view.harnesses.iter().map(|row| u64::from(row.active)).sum()),
            provenance: Provenance::Measured,
        },
        Measure {
            label: "queued".to_string(),
            value: Some(view.harnesses.iter().map(|row| u64::from(row.queued)).sum()),
            provenance: Provenance::Measured,
        },
    ];

    let seat_phase = view
        .seat
        .as_ref()
        .map(|seat| seat.phase.clone())
        .unwrap_or_default();
    let seats = SeatHealth {
        active: usize::from(view.seat.is_some() && seat_phase == "idle"),
        parked: usize::from(seat_phase == "parked"),
        draining: usize::from(seat_phase == "prepared"),
        billing: if billing.is_empty() {
            style::PLACEHOLDER.to_string()
        } else {
            billing.to_string()
        },
    };

    UsageStrip {
        measures,
        routes,
        seats,
        degraded: view.degraded,
    }
}

fn provenance_from_signal(quality: &str) -> Provenance {
    match quality {
        "measured" | "observed" | "fresh" => Provenance::Measured,
        "unknown" | "" | "none" => Provenance::Unknown,
        _ => Provenance::Estimated,
    }
}

impl UsageStrip {
    pub fn lines(&self, width: usize) -> Vec<StyledLine> {
        let mut out = Vec::new();
        let mut usage = String::from("usage ");
        for (index, measure) in self.measures.iter().enumerate() {
            if index > 0 {
                usage.push_str(" \u{b7} ");
            }
            usage.push_str(&measure.text());
        }
        if self.degraded {
            usage.push_str(" \u{b7} degraded snapshot");
        }
        out.push(StyledLine::toned(
            usage,
            if self.degraded { Tone::Warn } else { Tone::Muted },
        ));
        out.push(StyledLine::toned(
            format!(
                "seats {} active \u{b7} {} parked \u{b7} {} draining \u{b7} billing {}",
                self.seats.active, self.seats.parked, self.seats.draining, self.seats.billing
            ),
            Tone::Muted,
        ));
        for row in bound_slice(&self.routes, route_rows_for(width)).items {
            let (glyph, tone) = if row.draining {
                ("\u{25cd}", Tone::Muted)
            } else if row.ready {
                ("\u{2714}", Tone::Ok)
            } else {
                ("\u{2716}", Tone::Err)
            };
            let headroom = row
                .headroom_pct
                .map(|pct| format!("{pct:.0}% ({})", row.provenance.as_str()))
                .unwrap_or_else(|| "headroom unknown".to_string());
            let reason = row
                .reason
                .as_ref()
                .map(|reason| format!(" \u{2014} {reason}"))
                .unwrap_or_default();
            out.push(StyledLine(vec![
                StyledSpan {
                    text: format!("{glyph} "),
                    tone,
                },
                StyledSpan {
                    text: format!("{} \u{b7} {headroom}{reason}", row.route),
                    tone: Tone::Muted,
                },
            ]));
        }
        out
    }
}

fn route_rows_for(width: usize) -> usize {
    if width >= 120 { 8 } else { 4 }
}

// =========================================================================
// Item 4: compaction / rollover / reconnect notices, and continuity
// =========================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    Compacted,
    Rollover,
    Reconnected,
    /// Mail/attention that was held while the session was blocked and has
    /// now been delivered. Criterion 2's "completion notices are not lost".
    DeferredDelivery,
}

impl NoticeKind {
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Compacted => "\u{27f3}",         // ⟳
            Self::Rollover => "\u{2913}",          // ⤓
            Self::Reconnected => "\u{21c4}",       // ⇄
            Self::DeferredDelivery => "\u{2709}",  // ✉
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Compacted => "compacted",
            Self::Rollover => "rollover",
            Self::Reconnected => "reconnected",
            Self::DeferredDelivery => "delivered",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub kind: NoticeKind,
    pub headline: String,
    pub detail: Vec<String>,
    pub at: u64,
}

impl Notice {
    pub fn lines(&self) -> Vec<StyledLine> {
        let mut out = vec![StyledLine(vec![
            StyledSpan {
                text: "\u{23fa} ".to_string(),
                tone: Tone::Accent,
            },
            StyledSpan {
                text: format!("{} ", self.kind.glyph()),
                tone: Tone::Muted,
            },
            StyledSpan {
                text: self.headline.clone(),
                tone: Tone::Emphasis,
            },
        ])];
        for detail in &self.detail {
            out.push(StyledLine(vec![
                StyledSpan {
                    text: "  \u{23bd}  ".to_string(),
                    tone: Tone::Muted,
                },
                StyledSpan {
                    text: detail.clone(),
                    tone: Tone::Muted,
                },
            ]));
        }
        out
    }
}

/// Renders N19's own durable rollover record -- trigger, every route tried
/// with its outcome, the decision, and the settlement -- as one notice. The
/// tried routes are the part an operator most needs and the part a "switched
/// model" toast always drops.
pub fn notice_from_rollover(record: &rollover_runtime::Record) -> Notice {
    let mut detail = vec![format!(
        "seat {} generation {} \u{b7} trigger {} \u{b7} {}",
        record.short,
        record.generation,
        record.trigger.as_str(),
        record.decision,
    )];
    for attempt in &record.attempts {
        detail.push(format!(
            "tried {} ({}) \u{2014} {}: {}",
            attempt.route, attempt.runtime, attempt.outcome, attempt.detail
        ));
    }
    if let Some(boundary) = &record.boundary {
        detail.push(format!(
            "boundary: {} receipts, {} evidence, {} pending input, drained {}",
            boundary.receipts,
            boundary.evidence,
            boundary.pending_input.len(),
            boundary.drained
        ));
    }
    Notice {
        kind: NoticeKind::Rollover,
        headline: format!(
            "rollover {} \u{2192} {}",
            record.source_agent,
            record
                .attempts
                .iter()
                .rev()
                .find(|attempt| attempt.outcome == "admitted")
                .map(|attempt| attempt.route.clone())
                .unwrap_or_else(|| record.decision.clone()),
        ),
        detail,
        at: record.updated_at,
    }
}

pub fn notice_compaction(before: usize, after_tokens: u64, checkpoint: Option<&str>) -> Notice {
    Notice {
        kind: NoticeKind::Compacted,
        headline: format!("compacted {before} messages \u{2192} {after_tokens} tokens"),
        detail: vec![format!(
            "checkpoint {} \u{b7} draft, selection, focus and scrollback preserved",
            checkpoint.unwrap_or(style::PLACEHOLDER)
        )],
        at: 0,
    }
}

pub fn notice_reconnect(gap_secs: u64, cursor: u64, lost: usize) -> Notice {
    Notice {
        kind: NoticeKind::Reconnected,
        headline: format!("reconnected after {gap_secs}s"),
        detail: vec![format!("journal cursor {cursor} \u{b7} {lost} events lost")],
        at: 0,
    }
}

/// A bounded, de-duplicating notice log. Consecutive identical notices are
/// collapsed rather than repeated, which is what "stable, low-noise
/// notifications" (item 7) means in practice: a reconnect loop that fires
/// every 150 ms tick must produce one line, not four hundred.
#[derive(Clone, Debug, Default)]
pub struct NoticeLog {
    notices: VecDeque<Notice>,
    cap: usize,
}

pub const NOTICE_LOG_CAP: usize = 64;

impl NoticeLog {
    pub fn new(cap: usize) -> Self {
        Self {
            notices: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    /// Returns whether this notice was new (and therefore worth surfacing).
    pub fn push(&mut self, notice: Notice) -> bool {
        if let Some(last) = self.notices.back()
            && last.kind == notice.kind
            && last.headline == notice.headline
        {
            return false;
        }
        if self.notices.len() >= self.cap {
            self.notices.pop_front();
        }
        self.notices.push_back(notice);
        true
    }

    pub fn len(&self) -> usize {
        self.notices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.notices.is_empty()
    }

    pub fn recent(&self, n: usize) -> Vec<&Notice> {
        self.notices.iter().rev().take(n).rev().collect()
    }
}

/// A logical seat and the session that currently answers for it. The pair is
/// the whole point: a rollover keeps `short` and bumps `generation`, so
/// "which session does my input go to" has exactly one answer at any moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeatIdentity {
    pub short: String,
    pub session: String,
    pub generation: u64,
}

impl SeatIdentity {
    pub fn from_seat(seat: &seat::Seat) -> Self {
        Self {
            short: seat.short.clone(),
            session: seat.session.clone(),
            generation: seat.generation,
        }
    }
}

/// Everything that must survive a compaction, a rollover or a reconnect.
/// Deliberately a plain struct the pane hands over and takes back, not
/// something reconstructed from the new session: a draft that has to be
/// re-derived is a draft that gets lost.
#[derive(Clone, Debug, PartialEq)]
pub struct Continuity {
    pub seat: SeatIdentity,
    pub draft: String,
    pub cursor: usize,
    pub queued: Vec<QueuedInput>,
    pub selection: Option<(usize, usize)>,
    pub focus: Focus,
    pub scroll: ScrollState,
    /// The last input this pane has seen ACKNOWLEDGED by the runtime. Input
    /// at or below this point must never be resent after a reconnect.
    pub acknowledged_upto: u64,
}

impl Continuity {
    pub fn new(seat: SeatIdentity) -> Self {
        Self {
            seat,
            draft: String::new(),
            cursor: 0,
            queued: Vec::new(),
            selection: None,
            focus: Focus::Composer,
            scroll: ScrollState::default(),
            acknowledged_upto: 0,
        }
    }
}

/// What [`Continuity::carry_across`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Retarget {
    /// Same session and generation: nothing to do.
    Unchanged,
    /// The logical seat advanced. Everything presentational was preserved
    /// and every queued item now addresses the NEW session.
    Retargeted {
        from_session: String,
        to_session: String,
        generation: u64,
        queued: usize,
    },
}

impl Continuity {
    /// Moves this pane's state onto the seat's new session. The draft,
    /// cursor, selection, focus, scrollback and acknowledged watermark are
    /// carried verbatim; the queued input is RE-TARGETED, never replayed
    /// into the retired session.
    pub fn carry_across(&mut self, next: SeatIdentity) -> Retarget {
        if next == self.seat {
            return Retarget::Unchanged;
        }
        let from_session = std::mem::replace(&mut self.seat.session, next.session.clone());
        self.seat.short = next.short;
        self.seat.generation = next.generation;
        Retarget::Retargeted {
            from_session,
            to_session: next.session,
            generation: next.generation,
            queued: self.queued.len(),
        }
    }
}

/// Where a submission is allowed to go right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmitTarget {
    /// Safe: this session is the logical seat's current one.
    Send { session: String, generation: u64 },
    /// Refused, with the reason. The input stays queued; it is NEVER sent to
    /// the session the pane happens to still be holding.
    Hold { reason: String },
}

/// The guard criterion 4 turns on: input can never be submitted to the wrong
/// session across a rollover. `current` is the seat's authoritative identity
/// (from the seat record); `continuity` is what the pane believes. They
/// agree only if the pane has been carried across every generation change,
/// and a stale pane is refused rather than silently sending into a retired
/// session that may still have a live process behind it.
pub fn resolve_submit_target(continuity: &Continuity, current: &SeatIdentity) -> SubmitTarget {
    if continuity.seat.short != current.short {
        return SubmitTarget::Hold {
            reason: format!(
                "pane holds seat {}, the current seat is {}",
                continuity.seat.short, current.short
            ),
        };
    }
    if continuity.seat.generation != current.generation {
        return SubmitTarget::Hold {
            reason: format!(
                "pane holds generation {}, the seat is at {}",
                continuity.seat.generation, current.generation
            ),
        };
    }
    if continuity.seat.session != current.session {
        return SubmitTarget::Hold {
            reason: format!(
                "pane holds session {}, the seat's current session is {}",
                continuity.seat.session, current.session
            ),
        };
    }
    SubmitTarget::Send {
        session: current.session.clone(),
        generation: current.generation,
    }
}

// =========================================================================
// Item 5: explainable approvals and deferred delivery
// =========================================================================

/// The exact authority an approval would grant. Rendered verbatim: an
/// approval dialog that describes less than it grants is worse than no
/// dialog at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub verb: String,
    pub paths: Vec<PathBuf>,
    /// What "don't ask again" would widen to. `None` means the broader
    /// option is not offered at all.
    pub directory: Option<PathBuf>,
}

impl Scope {
    pub fn text(&self) -> String {
        let paths = if self.paths.is_empty() {
            style::PLACEHOLDER.to_string()
        } else {
            self.paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        match &self.directory {
            Some(dir) => format!("{} {paths} inside {}", self.verb, dir.display()),
            None => format!("{} {paths}", self.verb),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub id: String,
    /// The session that is blocked on this decision.
    pub session: String,
    pub tool: String,
    pub scope: Scope,
    /// Which agent asked -- role and short id, so an operator answering five
    /// dialogs knows which worker each belongs to.
    pub actor: String,
    pub reason: String,
    /// A bounded preview (a diff excerpt, a command line). Capped by the
    /// caller; the dialog renders at most [`APPROVAL_PREVIEW_LINES`].
    pub preview: Vec<String>,
    pub asked_at: u64,
}

pub const APPROVAL_PREVIEW_LINES: usize = 8;

impl ApprovalRequest {
    pub fn scope_text(&self) -> String {
        format!("{}: {}", self.tool, self.scope.text())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow,
    /// Allow, and widen the standing grant to `Scope::directory`.
    AllowAlways,
    Deny,
}

impl ApprovalDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::AllowAlways => "allow-always",
            Self::Deny => "deny",
        }
    }
}

/// Where a decision is actually delivered. The in-process broker for a
/// session this process spawned itself (`runtime::native::spawn_interactive`),
/// and protocol v1's `session.approve` for one owned by the persistent
/// runtime (N20). The dialog itself is identical either way -- only the
/// delivery differs -- so an operator never has to know which kind of
/// session they are answering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalRoute {
    Broker,
    Protocol,
}

pub fn approval_route(persistent: bool) -> ApprovalRoute {
    if persistent {
        ApprovalRoute::Protocol
    } else {
        ApprovalRoute::Broker
    }
}

/// What a key press did to the dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogAction {
    Moved,
    Decided(ApprovalDecision),
    /// Not a key this dialog claims. The caller must NOT fall through to the
    /// composer with it while a dialog is open -- see [`dialog_action`]'s
    /// own doc comment.
    Ignored,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApprovalDialog {
    pub request: ApprovalRequest,
    selected: usize,
}

impl ApprovalDialog {
    pub fn new(request: ApprovalRequest) -> Self {
        Self {
            request,
            selected: 0,
        }
    }

    pub fn selected_index(&self) -> usize {
        self.selected.min(self.options().len() - 1)
    }

    /// The numbered options, in the order they are rendered. The
    /// "don't ask again" option is only offered when the request actually
    /// carries a directory to widen to -- an option whose scope cannot be
    /// stated is never shown.
    pub fn options(&self) -> Vec<(ApprovalDecision, String)> {
        let mut options = vec![(ApprovalDecision::Allow, "Yes".to_string())];
        if let Some(dir) = &self.request.scope.directory {
            options.push((
                ApprovalDecision::AllowAlways,
                format!(
                    "Yes, and don't ask again for {} in {}",
                    self.request.tool,
                    dir.display()
                ),
            ));
        }
        options.push((
            ApprovalDecision::Deny,
            "No, and tell the agent what to do differently (esc)".to_string(),
        ));
        options
    }

    pub fn move_down(&mut self) {
        let len = self.options().len();
        self.selected = (self.selected_index() + 1) % len;
    }

    pub fn move_up(&mut self) {
        self.selected = match self.selected_index() {
            0 => self.options().len() - 1,
            n => n - 1,
        };
    }

    pub fn confirm(&self) -> ApprovalDecision {
        self.options()[self.selected_index()].0
    }

    pub fn lines(&self, width: usize) -> Vec<StyledLine> {
        let mut out = vec![
            StyledLine::toned(
                format!("{} \u{2014} approval needed", self.request.tool),
                Tone::Emphasis,
            ),
            StyledLine::toned(format!("  scope  {}", self.request.scope.text()), Tone::Warn),
            StyledLine::toned(format!("  actor  {}", self.request.actor), Tone::Muted),
            StyledLine::toned(format!("  why    {}", self.request.reason), Tone::Muted),
        ];
        for line in self.request.preview.iter().take(APPROVAL_PREVIEW_LINES) {
            out.push(StyledLine::toned(format!("  {line}"), Tone::Plain));
        }
        out.push(StyledLine::toned(
            "Do you want to proceed?".to_string(),
            Tone::Emphasis,
        ));
        for (index, (_, label)) in self.options().iter().enumerate() {
            let marker = if index == self.selected_index() {
                "\u{276f}"
            } else {
                " "
            };
            let tone = if index == self.selected_index() {
                Tone::Accent
            } else {
                Tone::Plain
            };
            out.push(StyledLine::toned(
                format!("{marker} {}. {label}", index + 1),
                tone,
            ));
        }
        let _ = width;
        out
    }
}

/// Maps a key press onto a dialog action. Only digits within range, the
/// arrows, Enter and Esc are claimed; everything else is [`DialogAction::
/// Ignored`] so the caller can drop it. Crucially there is no path from
/// typed text to a decision: an approval is never answered from the composer
/// (the composer's own `classify_submit_intent` queues while blocked), and
/// this function never accepts an arbitrary character as consent.
pub fn dialog_action(dialog: &mut ApprovalDialog, key: KeyEvent) -> DialogAction {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        || key.modifiers.contains(KeyModifiers::ALT)
        || key.modifiers.contains(KeyModifiers::SUPER)
    {
        return DialogAction::Ignored;
    }
    match key.code {
        KeyCode::Up | KeyCode::BackTab => {
            dialog.move_up();
            DialogAction::Moved
        }
        KeyCode::Down | KeyCode::Tab => {
            dialog.move_down();
            DialogAction::Moved
        }
        KeyCode::Enter => DialogAction::Decided(dialog.confirm()),
        KeyCode::Esc => DialogAction::Decided(ApprovalDecision::Deny),
        KeyCode::Char(ch) if ch.is_ascii_digit() => {
            let options = dialog.options();
            match ch.to_digit(10) {
                Some(digit) if digit >= 1 && (digit as usize) <= options.len() => {
                    DialogAction::Decided(options[digit as usize - 1].0)
                }
                _ => DialogAction::Ignored,
            }
        }
        _ => DialogAction::Ignored,
    }
}

/// Mail or an attention ping that could not be delivered because the target
/// was blocked. Held in order and released the moment the block clears --
/// never dropped, which is criterion 2's "completion notices are not lost".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deferred {
    pub kind: &'static str,
    pub id: String,
    pub body: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeferredDelivery {
    items: Vec<Deferred>,
}

impl DeferredDelivery {
    pub fn defer(&mut self, item: Deferred) {
        if self.items.iter().any(|held| held.id == item.id) {
            return;
        }
        self.items.push(item);
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Releases everything held, in the order it was deferred, once
    /// `blocked` is false. While still blocked this drains nothing and holds
    /// everything -- it never drops on the floor and never reorders.
    pub fn resume(&mut self, blocked: bool) -> Vec<Deferred> {
        if blocked {
            return Vec::new();
        }
        std::mem::take(&mut self.items)
    }

    /// The notice to show when a resume actually released something.
    pub fn notice(released: &[Deferred]) -> Option<Notice> {
        if released.is_empty() {
            return None;
        }
        Some(Notice {
            kind: NoticeKind::DeferredDelivery,
            headline: format!("delivered {} held item(s)", released.len()),
            detail: released
                .iter()
                .map(|item| format!("{} {}", item.kind, item.id))
                .collect(),
            at: 0,
        })
    }
}

// =========================================================================
// Item 7: focus, shortcuts, layout
// =========================================================================

/// Every region that can hold focus. `Approval` is modal: while it is
/// focused, [`focus_next`] refuses to move, because an operator tabbing past
/// a blocking decision is how a fleet stalls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Transcript,
    Composer,
    Overview,
    Inspection,
    Approval,
    Help,
}

impl Focus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Transcript => "transcript",
            Self::Composer => "composer",
            Self::Overview => "agents",
            Self::Inspection => "worker",
            Self::Approval => "approval",
            Self::Help => "help",
        }
    }
}

/// The Tab order. Composer first: it is where an operator is most of the
/// time, and it is where focus returns after every modal closes.
pub const FOCUS_ORDER: [Focus; 4] = [
    Focus::Composer,
    Focus::Transcript,
    Focus::Overview,
    Focus::Inspection,
];

pub fn focus_next(current: Focus, overview_visible: bool, inspection_open: bool) -> Focus {
    step_focus(current, overview_visible, inspection_open, 1)
}

pub fn focus_prev(current: Focus, overview_visible: bool, inspection_open: bool) -> Focus {
    step_focus(current, overview_visible, inspection_open, -1)
}

fn step_focus(current: Focus, overview_visible: bool, inspection_open: bool, delta: i32) -> Focus {
    if matches!(current, Focus::Approval) {
        return Focus::Approval;
    }
    let available: Vec<Focus> = FOCUS_ORDER
        .iter()
        .copied()
        .filter(|focus| match focus {
            Focus::Overview => overview_visible,
            Focus::Inspection => inspection_open,
            _ => true,
        })
        .collect();
    if available.is_empty() {
        return Focus::Composer;
    }
    let index = available
        .iter()
        .position(|focus| *focus == current)
        .unwrap_or(0) as i32;
    let len = available.len() as i32;
    let next = (index + delta).rem_euclid(len) as usize;
    available[next]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shortcut {
    pub keys: &'static str,
    pub what: &'static str,
    /// The surface this key belongs to, or `None` for a global key.
    pub focus: Option<Focus>,
}

/// The one table `?` renders and the one table the design note documents.
/// Anything bound in the dashboard loop appears here: a shortcut that is not
/// discoverable is not a shortcut.
pub const SHORTCUTS: &[Shortcut] = &[
    Shortcut {
        keys: "?",
        what: "show this list",
        focus: None,
    },
    Shortcut {
        keys: "tab / shift+tab",
        what: "move focus between composer, transcript, agents and worker",
        focus: None,
    },
    Shortcut {
        keys: "esc",
        what: "interrupt the current turn; close a dialog",
        focus: None,
    },
    Shortcut {
        keys: "ctrl+c ctrl+c",
        what: "quit (twice, within two seconds)",
        focus: None,
    },
    Shortcut {
        keys: "enter",
        what: "submit; queue while blocked; never answers an approval",
        focus: Some(Focus::Composer),
    },
    Shortcut {
        keys: "shift+enter",
        what: "newline",
        focus: Some(Focus::Composer),
    },
    Shortcut {
        keys: "/",
        what: "slash commands",
        focus: Some(Focus::Composer),
    },
    Shortcut {
        keys: "@",
        what: "file picker, restricted to this worktree",
        focus: Some(Focus::Composer),
    },
    Shortcut {
        keys: "!",
        what: "run a shell command through the pane's process tool",
        focus: Some(Focus::Composer),
    },
    Shortcut {
        keys: "ctrl+r",
        what: "expand the selected tool result",
        focus: Some(Focus::Transcript),
    },
    Shortcut {
        keys: "up / down",
        what: "scroll; page with pgup/pgdn, end re-follows",
        focus: Some(Focus::Transcript),
    },
    Shortcut {
        keys: "a",
        what: "open the agent & task overview",
        focus: None,
    },
    Shortcut {
        keys: "up / down / enter",
        what: "select an agent; enter inspects it",
        focus: Some(Focus::Overview),
    },
    Shortcut {
        keys: "f",
        what: "send a bounded follow-up to this worker",
        focus: Some(Focus::Inspection),
    },
    Shortcut {
        keys: "1 / 2 / 3",
        what: "answer the approval; esc denies",
        focus: Some(Focus::Approval),
    },
];

pub fn help_lines(focus: Option<Focus>) -> Vec<StyledLine> {
    let mut out = vec![StyledLine::toned(
        "shortcuts".to_string(),
        Tone::Emphasis,
    )];
    for shortcut in SHORTCUTS {
        if let Some(want) = focus
            && let Some(theirs) = shortcut.focus
            && theirs != want
        {
            continue;
        }
        let scope = shortcut
            .focus
            .map(|focus| format!(" [{}]", focus.label()))
            .unwrap_or_default();
        out.push(StyledLine(vec![
            StyledSpan {
                text: format!("  {:<18}", shortcut.keys),
                tone: Tone::Accent,
            },
            StyledSpan {
                text: format!("{}{scope}", shortcut.what),
                tone: Tone::Muted,
            },
        ]));
    }
    out
}

/// What fits at this terminal size. The three sizes the mock draws (80x24,
/// 120x40, 200x50) plus the 40-column floor are the cases the tests pin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutPlan {
    pub sidebar: bool,
    pub overview: bool,
    pub usage_rows: usize,
    pub main_width: usize,
    /// One line per overview row rather than two, and elided paths.
    pub compact: bool,
}

pub const SIDEBAR_WIDTH: usize = 24;
pub const OVERVIEW_WIDTH: usize = 63;

/// Deterministic, monotone layout: adding columns never removes a panel, and
/// the main pane is never narrower than 20 columns whatever else is dropped.
pub fn resolve_layout(width: usize, height: usize) -> LayoutPlan {
    let overview = width >= 160 && height >= 24;
    let sidebar = width >= 100 && height >= 16;
    let mut main_width = width;
    if sidebar {
        main_width = main_width.saturating_sub(SIDEBAR_WIDTH + 1);
    }
    if overview {
        main_width = main_width.saturating_sub(OVERVIEW_WIDTH + 1);
    }
    LayoutPlan {
        sidebar,
        overview,
        usage_rows: if height >= 30 {
            4
        } else if height >= 20 {
            2
        } else {
            0
        },
        main_width: main_width.max(20),
        compact: width < 60,
    }
}

// =========================================================================
// Item 7: `/`, `@` and `!` entry modes
// =========================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryMode {
    Text,
    Slash,
    File,
    Shell,
}

/// What the draft currently means. Only the FIRST character decides: a `/`
/// halfway through a sentence is a path separator, not a command.
pub fn classify_entry(draft: &str) -> EntryMode {
    match draft.chars().next() {
        Some('/') => EntryMode::Slash,
        Some('!') => EntryMode::Shell,
        _ if draft.rsplit(char::is_whitespace).next().is_some_and(|token| {
            token.starts_with('@') && token.len() > 1
        }) =>
        {
            EntryMode::File
        }
        _ => EntryMode::Text,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    /// What replaces the token in the draft.
    pub insert: String,
    pub label: String,
    pub detail: String,
}

/// The slash commands this pane offers. Each maps onto something the pane
/// can already do, so the list never advertises a verb with no
/// implementation behind it.
pub const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/agents", "open the agent & task overview"),
    ("/approve", "focus the pending approval dialog"),
    ("/artifacts", "open the selected worker's bounded evidence"),
    ("/compact", "compact this conversation now"),
    ("/follow-up", "send a bounded follow-up to the selected worker"),
    ("/help", "show the shortcut list"),
    ("/status", "print the authoritative facts as JSON"),
];

pub fn slash_completions(draft: &str) -> Vec<Completion> {
    let prefix = draft.split_whitespace().next().unwrap_or("");
    SLASH_COMMANDS
        .iter()
        .filter(|(name, _)| name.starts_with(prefix))
        .map(|(name, what)| Completion {
            insert: (*name).to_string(),
            label: (*name).to_string(),
            detail: (*what).to_string(),
        })
        .collect()
}

pub const FILE_COMPLETION_CAP: usize = 50;
const FILE_SCAN_CAP: usize = 4000;

/// Resolves a relative candidate against `workdir` and refuses anything that
/// escapes it, WITHOUT touching the filesystem (so a symlink race cannot
/// change the answer between the check and the use, and a missing path is
/// still judged): `..` is resolved lexically and any segment that would pop
/// above the root is a rejection.
pub fn contained_ref(workdir: &Path, candidate: &str) -> Option<PathBuf> {
    let raw = Path::new(candidate);
    if raw.is_absolute() {
        let rel = raw.strip_prefix(workdir).ok()?;
        return contained_ref(workdir, &rel.to_string_lossy());
    }
    let mut parts: Vec<String> = Vec::new();
    for component in raw.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if parts.pop().is_none() {
                    return None;
                }
            }
            _ => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(PathBuf::from(parts.join("/")))
}

/// Candidates for the `@` picker, always relative to `workdir` and always
/// inside it. Bounded twice: at most [`FILE_COMPLETION_CAP`] results, and at
/// most `FILE_SCAN_CAP` directory entries examined, so a picker keystroke in
/// a huge worktree costs a bounded amount of work rather than a full walk.
pub fn file_completions(draft: &str, workdir: &Path) -> Vec<Completion> {
    let token = draft
        .rsplit(char::is_whitespace)
        .next()
        .unwrap_or("")
        .trim_start_matches('@');
    let needle = token.to_ascii_lowercase();
    let mut out: Vec<Completion> = Vec::new();
    let mut scanned = 0usize;
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    queue.push_back(workdir.to_path_buf());
    while let Some(dir) = queue.pop_front() {
        if out.len() >= FILE_COMPLETION_CAP || scanned >= FILE_SCAN_CAP {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            scanned += 1;
            if scanned >= FILE_SCAN_CAP || out.len() >= FILE_COMPLETION_CAP {
                break;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
            if is_dir {
                queue.push_back(path);
                continue;
            }
            let Ok(relative) = path.strip_prefix(workdir) else {
                continue;
            };
            let shown = relative.to_string_lossy().replace('\\', "/");
            if !needle.is_empty() && !shown.to_ascii_lowercase().contains(&needle) {
                continue;
            }
            let Some(contained) = contained_ref(workdir, &shown) else {
                continue;
            };
            out.push(Completion {
                insert: format!("@{}", contained.to_string_lossy().replace('\\', "/")),
                label: shown,
                detail: "file".to_string(),
            });
        }
    }
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out
}

/// The command a `!` line means, or `None` when there is nothing after the
/// bang. Never executed here: the caller routes it to the pane's own process
/// tool, so it goes through exactly the same approval and safety policy as
/// any other tool call.
pub fn shell_command(draft: &str) -> Option<&str> {
    let rest = draft.strip_prefix('!')?.trim();
    if rest.is_empty() { None } else { Some(rest) }
}

// =========================================================================
// Item 8: bounded history and fanout
// =========================================================================

/// The bounds the pane renders within, regardless of how much history or how
/// many sessions exist. Every one of these is a hard cap, not a hint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// Transcript items considered for rendering (the newest ones).
    pub max_items: usize,
    /// Rendered lines kept after wrapping.
    pub max_lines: usize,
    /// Sessions polled per tick.
    pub max_sessions: usize,
    /// Agent rows rendered in the overview.
    pub max_rows: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_items: 400,
            max_lines: 2000,
            max_sessions: 16,
            max_rows: 64,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bounded<'a, T> {
    pub items: &'a [T],
    pub elided: usize,
}

/// Keeps the newest `max` items. Returns a SLICE of the input, so bounding a
/// 200,000-item history allocates nothing.
pub fn bound_slice<T>(items: &[T], max: usize) -> Bounded<'_, T> {
    if items.len() <= max {
        return Bounded { items, elided: 0 };
    }
    let start = items.len() - max;
    Bounded {
        items: &items[start..],
        elided: start,
    }
}

/// Keeps the last `max` rendered lines and prepends one elision marker when
/// anything was dropped, so the operator can always tell a bounded view from
/// a complete one.
pub fn bound_lines(lines: Vec<StyledLine>, max: usize) -> Vec<StyledLine> {
    if lines.len() <= max {
        return lines;
    }
    let dropped = lines.len() - max;
    let mut out = Vec::with_capacity(max + 1);
    out.push(StyledLine::toned(
        format!("\u{2026} {dropped} earlier lines not shown (scroll up to load)"),
        Tone::Muted,
    ));
    out.extend(lines.into_iter().skip(dropped));
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FanoutPlan {
    pub polled: usize,
    pub deferred: usize,
}

/// How many sessions this tick actually touches. The rest wait for a later
/// tick: a 200-session fleet must not turn one 150 ms frame into 200 SQLite
/// reads. Deterministic in the inputs, so the bound is a testable property
/// rather than a wall-clock measurement.
pub fn fanout_plan(sessions: usize, budget: &Budget) -> FanoutPlan {
    let polled = sessions.min(budget.max_sessions);
    FanoutPlan {
        polled,
        deferred: sessions - polled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn seat_fixture(short: &str, generation: u64) -> seat::Seat {
        seat::Seat {
            short: short.to_string(),
            session: format!("sess-{short}-{generation}"),
            generation,
            agent: "claude".to_string(),
            model: Some("opus-5".to_string()),
            provider: "anthropic".to_string(),
            role: "orchestrator".to_string(),
            pinned: false,
            phase: seat::Phase::Idle,
            visited: Vec::new(),
            last_rollover_at: None,
            pending: None,
            displaced: None,
            created_at: 100,
            updated_at: 200,
            runtime: RuntimeKind::Native,
        }
    }

    fn delegation_fixture(id: &str, phase: delegation::Phase) -> delegation::Record {
        delegation::Record {
            schema_version: 1,
            handle: delegation::WorkerHandle {
                delegation: id.to_string(),
                attempt: 1,
                runtime: RuntimeKind::Native,
                worker_session: format!("sess-{id}"),
                short: id.to_string(),
                role: "implementer".to_string(),
                task: Some("T2".to_string()),
                group: None,
                objective: None,
                workdir: PathBuf::from("/repo/wt"),
            },
            parent_session: Some("orch".to_string()),
            phase,
            revision: 1,
            launched_at: 10,
            updated_at: 150,
            exit_code: None,
            result_path: None,
            summary: None,
            published: Vec::new(),
            consumed: Vec::new(),
            queued: Vec::new(),
            reservation: None,
            write_claim: None,
            cancel_requested: false,
            unknown_tool_outcomes: Vec::new(),
            attempts: Vec::new(),
        }
    }

    fn approval_fixture(session: &str) -> ApprovalRequest {
        ApprovalRequest {
            id: "ap-1".to_string(),
            session: session.to_string(),
            tool: "Write".to_string(),
            scope: Scope {
                verb: "write".to_string(),
                paths: vec![PathBuf::from("src/journal.rs")],
                directory: Some(PathBuf::from("/repo/wt")),
            },
            actor: "w/482 implementer".to_string(),
            reason: "outside the pane's last granted scope".to_string(),
            preview: vec!["+ let cursor = committed;".to_string()],
            asked_at: 140,
        }
    }

    // ---------------- item 1: the overview ----------------

    #[test]
    fn an_outstanding_approval_outranks_the_delegation_phase() {
        let record = delegation_fixture("w1", delegation::Phase::Running);
        let approvals = [approval_fixture("sess-w1")];
        let overview = build_overview(
            &coordinator::Coordinator::default(),
            &[record],
            &[],
            &approvals,
            300,
        );
        let row = overview.rows.iter().find(|row| row.id == "w1").expect("row");
        assert_eq!(row.state, AgentState::ApprovalNeeded);
        assert_eq!(
            row.pending_decision.as_deref(),
            Some("Write: write src/journal.rs inside /repo/wt")
        );
    }

    #[test]
    fn a_completed_worker_with_an_unconsumed_receipt_is_done_unread() {
        let mut record = delegation_fixture("w2", delegation::Phase::Completed);
        record.published = vec!["r1".to_string()];
        let overview = build_overview(
            &coordinator::Coordinator::default(),
            &[record],
            &[],
            &[],
            300,
        );
        assert_eq!(overview.rows[0].state, AgentState::DoneUnread);
    }

    #[test]
    fn a_consumed_receipt_downgrades_done_unread_to_done() {
        let mut record = delegation_fixture("w3", delegation::Phase::Completed);
        record.published = vec!["r1".to_string()];
        record.consumed = vec!["r1".to_string()];
        let overview = build_overview(
            &coordinator::Coordinator::default(),
            &[record],
            &[],
            &[],
            300,
        );
        assert_eq!(overview.rows[0].state, AgentState::Done);
    }

    #[test]
    fn rows_are_ordered_by_urgency_then_short_id() {
        let records = vec![
            delegation_fixture("zz", delegation::Phase::Completed),
            delegation_fixture("aa", delegation::Phase::Failed),
            delegation_fixture("mm", delegation::Phase::Running),
        ];
        let overview = build_overview(
            &coordinator::Coordinator::default(),
            &records,
            &[],
            &[],
            300,
        );
        let states: Vec<AgentState> = overview.rows.iter().map(|row| row.state).collect();
        assert_eq!(
            states,
            vec![AgentState::Failed, AgentState::Running, AgentState::Done]
        );
    }

    #[test]
    fn an_undelegated_task_node_appears_as_a_queued_row() {
        let mut graph = coordinator::Coordinator {
            nodes: BTreeMap::new(),
            ..Default::default()
        };
        graph.nodes.insert(
            "T9".to_string(),
            coordinator::Node {
                task: "write the bench".to_string(),
                role: "tester".to_string(),
                updated_at: 250,
                ..Default::default()
            },
        );
        let overview = build_overview(&graph, &[], &[], &[], 300);
        let row = overview.rows.iter().find(|row| row.id == "task:T9").expect("row");
        assert_eq!(row.state, AgentState::Queued);
        assert_eq!(row.since_secs, 50);
        assert_eq!(row.model_provenance, Provenance::Unknown);
    }

    #[test]
    fn a_parked_seat_reads_as_draining_not_running() {
        let mut seat = seat_fixture("s7", 2);
        seat.phase = seat::Phase::Parked {
            until: 900,
            window: "5h".to_string(),
            reason: "limit".to_string(),
            since: 200,
        };
        let overview = build_overview(
            &coordinator::Coordinator::default(),
            &[],
            &[seat],
            &[],
            300,
        );
        assert_eq!(overview.rows[0].state, AgentState::Draining);
    }

    #[test]
    fn a_missing_model_renders_as_unknown_never_as_a_blank() {
        let row = AgentRow {
            id: "x".to_string(),
            short: "x".to_string(),
            role: "r".to_string(),
            task: None,
            model: None,
            model_provenance: Provenance::Unknown,
            backend: "native".to_string(),
            worktree: None,
            owner: "operator".to_string(),
            state: AgentState::Running,
            pending_decision: None,
            result: None,
            since_secs: 0,
        };
        assert!(row.model_text().contains("unknown"));
        assert!(!row.model_text().trim().is_empty());
    }

    #[test]
    fn selection_follows_the_agent_across_a_rebuild_not_the_index() {
        let mut overview = build_overview(
            &coordinator::Coordinator::default(),
            &[
                delegation_fixture("bb", delegation::Phase::Running),
                delegation_fixture("cc", delegation::Phase::Running),
            ],
            &[],
            &[],
            300,
        );
        overview.select_next();
        let chosen = overview.selected().expect("selected").id.clone();
        assert_eq!(chosen, "cc");

        let mut rebuilt = build_overview(
            &coordinator::Coordinator::default(),
            &[
                delegation_fixture("aa", delegation::Phase::Running),
                delegation_fixture("bb", delegation::Phase::Running),
                delegation_fixture("cc", delegation::Phase::Running),
            ],
            &[],
            &[],
            300,
        );
        assert!(rebuilt.reselect(&chosen));
        assert_eq!(rebuilt.selected().expect("selected").id, "cc");
    }

    #[test]
    fn a_click_maps_to_the_row_whose_lines_it_landed_in() {
        let overview = build_overview(
            &coordinator::Coordinator::default(),
            &[
                delegation_fixture("aa", delegation::Phase::Running),
                delegation_fixture("bb", delegation::Phase::Running),
            ],
            &[],
            &[],
            300,
        );
        assert_eq!(overview.row_at_line(0, 120).expect("row").id, "aa");
        assert_eq!(overview.row_at_line(1, 120).expect("row").id, "aa");
        assert_eq!(overview.row_at_line(2, 120).expect("row").id, "bb");
        assert!(overview.row_at_line(40, 120).is_none());
    }

    #[test]
    fn every_agent_state_has_its_own_glyph_and_label() {
        let states = [
            AgentState::ApprovalNeeded,
            AgentState::Blocked,
            AgentState::Failed,
            AgentState::DoneUnread,
            AgentState::Running,
            AgentState::Queued,
            AgentState::Draining,
            AgentState::Done,
            AgentState::Cancelled,
        ];
        let glyphs: std::collections::BTreeSet<&str> =
            states.iter().map(|state| state.glyph()).collect();
        let labels: std::collections::BTreeSet<&str> =
            states.iter().map(|state| state.label()).collect();
        assert_eq!(glyphs.len(), states.len(), "a state is colour-only");
        assert_eq!(labels.len(), states.len());
        assert!(glyphs.iter().all(|glyph| !glyph.trim().is_empty()));
    }

    // ---------------- item 2: worker inspection ----------------

    fn manifest_fixture(summary: Option<String>) -> delegation::Manifest {
        delegation::Manifest {
            schema_version: 1,
            delegation: "w1".to_string(),
            attempt: 1,
            runtime: "native",
            phase: "completed",
            task: Some("T2".to_string()),
            exit_code: Some(0),
            summary,
            summary_truncated: false,
            result_path: Some(PathBuf::from("/repo/wt/journal.patch")),
            deliveries: Vec::new(),
            unknown_tool_outcomes: Vec::new(),
            queued_messages: 0,
            continuation: "none",
        }
    }

    #[test]
    fn an_inspection_reads_only_the_bounded_manifest() {
        let record = delegation_fixture("w1", delegation::Phase::Completed);
        let manifest = manifest_fixture(Some("did the thing".to_string()));
        let inspection = build_inspection(&record, &manifest, AgentState::Done);
        assert_eq!(inspection.summary_bytes, "did the thing".len());
        assert_eq!(inspection.follow_up_target(), "w1");
        assert!(
            inspection
                .evidence
                .iter()
                .any(|row| row.kind == EvidenceKind::Diff)
        );
    }

    #[test]
    fn an_oversized_manifest_summary_is_capped_and_says_so() {
        let record = delegation_fixture("w1", delegation::Phase::Completed);
        let manifest = manifest_fixture(Some("x".repeat(INSPECTION_SUMMARY_CAP * 3)));
        let inspection = build_inspection(&record, &manifest, AgentState::Done);
        assert!(inspection.summary_truncated);
        assert_eq!(
            inspection.summary.as_ref().map(String::len),
            Some(INSPECTION_SUMMARY_CAP)
        );
    }

    #[test]
    fn a_png_result_is_frontend_evidence_and_a_patch_is_a_diff() {
        assert_eq!(
            classify_evidence_path(Path::new("a/shot.png")),
            EvidenceKind::Frontend
        );
        assert_eq!(
            classify_evidence_path(Path::new("a/fix.patch")),
            EvidenceKind::Diff
        );
        assert_eq!(
            classify_evidence_path(Path::new("a/test-output.txt")),
            EvidenceKind::Tests
        );
    }

    #[test]
    fn inspection_lines_never_exceed_the_bounded_summary_line_cap() {
        let record = delegation_fixture("w1", delegation::Phase::Completed);
        let manifest = manifest_fixture(Some("line\n".repeat(500)));
        let inspection = build_inspection(&record, &manifest, AgentState::Done);
        let lines = inspection.lines(80);
        assert!(lines.len() <= 3 + inspection.evidence.len() + bounded_summary_lines(80));
    }

    // ---------------- item 3: usage provenance ----------------

    fn pool_fixture() -> pool::PoolView {
        pool::PoolView {
            taken_at: 100,
            degraded: false,
            seat: None,
            harnesses: vec![
                pool::HarnessRow {
                    name: "claude".to_string(),
                    provider: "anthropic".to_string(),
                    state: "ready".to_string(),
                    state_reason: String::new(),
                    used_pct: Some(38.0),
                    headroom_pct: Some(62.0),
                    projected_headroom_pct: Some(60.0),
                    signal_age_secs: Some(4),
                    signal_source: Some("statusline".to_string()),
                    signal_quality: "measured".to_string(),
                    active: 1,
                    max_active: Some(3),
                    queued: 0,
                    reserved_tokens: 0,
                    resets_at: None,
                    runtime: "native".to_string(),
                    pool: "anthropic".to_string(),
                    dimensions: Vec::new(),
                    binding_dimension: None,
                },
                pool::HarnessRow {
                    name: "codex".to_string(),
                    provider: "openai".to_string(),
                    state: "excluded".to_string(),
                    state_reason: "429 cooldown".to_string(),
                    used_pct: None,
                    headroom_pct: None,
                    projected_headroom_pct: None,
                    signal_age_secs: None,
                    signal_source: None,
                    signal_quality: "unknown".to_string(),
                    active: 0,
                    max_active: None,
                    queued: 2,
                    reserved_tokens: 0,
                    resets_at: None,
                    runtime: "harness".to_string(),
                    pool: "openai".to_string(),
                    dimensions: Vec::new(),
                    binding_dimension: None,
                },
            ],
            providers: Vec::new(),
            exclusions: vec![("codex".to_string(), "429 cooldown 4m".to_string())],
            health: Vec::new(),
            shared: Vec::new(),
        }
    }

    #[test]
    fn an_exclusion_keeps_its_reason_verbatim() {
        let strip = build_usage(&pool_fixture(), "subscription");
        let row = strip
            .routes
            .iter()
            .find(|row| row.route.ends_with("codex"))
            .expect("row");
        assert!(!row.ready);
        assert_eq!(row.reason.as_deref(), Some("429 cooldown 4m"));
    }

    #[test]
    fn measured_and_unknown_signals_get_different_provenance() {
        let strip = build_usage(&pool_fixture(), "api");
        let claude = strip
            .routes
            .iter()
            .find(|row| row.route.ends_with("claude"))
            .expect("row");
        let codex = strip
            .routes
            .iter()
            .find(|row| row.route.ends_with("codex"))
            .expect("row");
        assert_eq!(claude.provenance, Provenance::Measured);
        assert_eq!(codex.provenance, Provenance::Unknown);
    }

    #[test]
    fn an_unknown_measure_never_renders_as_a_zero() {
        let measure = Measure::unknown("pool");
        assert_eq!(measure.text(), "pool: unknown");
        assert!(!measure.text().contains('0'));
    }

    #[test]
    fn a_measured_zero_is_rendered_as_a_measured_zero() {
        let measure = Measure {
            label: "queued".to_string(),
            value: Some(0),
            provenance: Provenance::Measured,
        };
        assert_eq!(measure.text(), "queued: 0 (measured)");
    }

    #[test]
    fn an_empty_billing_class_is_a_placeholder_not_an_invented_default() {
        let strip = build_usage(&pool_fixture(), "");
        assert_eq!(strip.seats.billing, style::PLACEHOLDER);
    }

    #[test]
    fn a_degraded_snapshot_says_so_on_the_strip() {
        let mut view = pool_fixture();
        view.degraded = true;
        let strip = build_usage(&view, "api");
        assert!(strip.degraded);
        let rendered: String = strip
            .lines(120)
            .iter()
            .map(StyledLine::to_plain_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("degraded"));
    }

    // ---------------- item 4: notices and continuity ----------------

    #[test]
    fn a_rollover_notice_names_every_route_that_was_tried() {
        let record = rollover_runtime::Record {
            schema_version: 1,
            short: "s7".to_string(),
            trigger: rollover_runtime::Trigger::UsageExhaustion,
            direction: None,
            source_agent: "opus-5".to_string(),
            source_runtime: "native".to_string(),
            generation: 2,
            decision: "admitted sonnet-4.6".to_string(),
            attempts: vec![
                rollover_runtime::Attempt {
                    route: "anthropic/opus-5".to_string(),
                    runtime: "native".to_string(),
                    outcome: "refused".to_string(),
                    detail: "5h window exhausted".to_string(),
                    at: 10,
                },
                rollover_runtime::Attempt {
                    route: "anthropic/sonnet-4.6".to_string(),
                    runtime: "native".to_string(),
                    outcome: "admitted".to_string(),
                    detail: "ok".to_string(),
                    at: 11,
                },
            ],
            boundary: None,
            subagents: Vec::new(),
            settlement: None,
            started_at: 5,
            updated_at: 12,
        };
        let notice = notice_from_rollover(&record);
        let text = notice
            .lines()
            .iter()
            .map(StyledLine::to_plain_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("anthropic/opus-5"));
        assert!(text.contains("5h window exhausted"));
        assert!(text.contains("anthropic/sonnet-4.6"));
    }

    #[test]
    fn repeated_identical_notices_are_collapsed_to_one() {
        let mut log = NoticeLog::new(NOTICE_LOG_CAP);
        assert!(log.push(notice_reconnect(12, 4182, 0)));
        assert!(!log.push(notice_reconnect(12, 4182, 0)));
        assert!(!log.push(notice_reconnect(12, 4182, 0)));
        assert_eq!(log.len(), 1);
    }

    #[test]
    fn the_notice_log_is_bounded_by_its_cap() {
        let mut log = NoticeLog::new(4);
        for i in 0..100u64 {
            log.push(notice_reconnect(i, i, 0));
        }
        assert_eq!(log.len(), 4);
    }

    #[test]
    fn carrying_a_pane_across_a_rollover_keeps_the_draft_and_retargets_the_queue() {
        let mut continuity = Continuity::new(SeatIdentity::from_seat(&seat_fixture("s7", 1)));
        continuity.draft = "keep the shim".to_string();
        continuity.cursor = 4;
        continuity.selection = Some((2, 6));
        continuity.focus = Focus::Transcript;
        continuity.scroll.scroll_up(3, 50);
        continuity.acknowledged_upto = 17;
        continuity.queued.push(QueuedInput {
            text: "queued line".to_string(),
            steering: false,
            queued_at_ms: 1,
        });

        let next = SeatIdentity::from_seat(&seat_fixture("s7", 2));
        let outcome = continuity.carry_across(next.clone());

        assert_eq!(
            outcome,
            Retarget::Retargeted {
                from_session: "sess-s7-1".to_string(),
                to_session: "sess-s7-2".to_string(),
                generation: 2,
                queued: 1,
            }
        );
        assert_eq!(continuity.draft, "keep the shim");
        assert_eq!(continuity.cursor, 4);
        assert_eq!(continuity.selection, Some((2, 6)));
        assert_eq!(continuity.focus, Focus::Transcript);
        assert_eq!(continuity.scroll.items_back, 3);
        assert_eq!(continuity.acknowledged_upto, 17);
        assert_eq!(continuity.queued.len(), 1);
        assert_eq!(continuity.seat, next);
    }

    #[test]
    fn after_a_generation_change_a_queued_draft_targets_only_the_current_session() {
        let old = SeatIdentity::from_seat(&seat_fixture("s7", 1));
        let new = SeatIdentity::from_seat(&seat_fixture("s7", 2));
        let mut continuity = Continuity::new(old.clone());

        // A pane that has NOT been carried across is refused outright.
        assert!(matches!(
            resolve_submit_target(&continuity, &new),
            SubmitTarget::Hold { .. }
        ));

        continuity.carry_across(new.clone());

        // Carried across: the current session accepts...
        assert_eq!(
            resolve_submit_target(&continuity, &new),
            SubmitTarget::Send {
                session: "sess-s7-2".to_string(),
                generation: 2,
            }
        );
        // ...and the retired generation never does.
        assert!(matches!(
            resolve_submit_target(&continuity, &old),
            SubmitTarget::Hold { .. }
        ));
    }

    #[test]
    fn a_pane_holding_a_different_seat_is_refused_with_a_reason() {
        let continuity = Continuity::new(SeatIdentity::from_seat(&seat_fixture("s7", 1)));
        let other = SeatIdentity::from_seat(&seat_fixture("s9", 1));
        match resolve_submit_target(&continuity, &other) {
            SubmitTarget::Hold { reason } => assert!(reason.contains("s9")),
            other => panic!("expected a hold, got {other:?}"),
        }
    }

    #[test]
    fn carrying_across_the_same_identity_changes_nothing() {
        let identity = SeatIdentity::from_seat(&seat_fixture("s7", 1));
        let mut continuity = Continuity::new(identity.clone());
        assert_eq!(continuity.carry_across(identity), Retarget::Unchanged);
    }

    // ---------------- item 5: approvals ----------------

    #[test]
    fn the_dialog_offers_three_numbered_options_with_the_exact_scope() {
        let dialog = ApprovalDialog::new(approval_fixture("s"));
        let options = dialog.options();
        assert_eq!(options.len(), 3);
        assert_eq!(options[0].0, ApprovalDecision::Allow);
        assert_eq!(options[1].0, ApprovalDecision::AllowAlways);
        assert!(options[1].1.contains("/repo/wt"));
        assert_eq!(options[2].0, ApprovalDecision::Deny);
        let text = dialog
            .lines(80)
            .iter()
            .map(StyledLine::to_plain_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("write src/journal.rs inside /repo/wt"));
        assert!(text.contains("1. Yes"));
    }

    #[test]
    fn a_request_without_a_directory_never_offers_dont_ask_again() {
        let mut request = approval_fixture("s");
        request.scope.directory = None;
        let dialog = ApprovalDialog::new(request);
        assert_eq!(dialog.options().len(), 2);
        assert!(
            !dialog
                .options()
                .iter()
                .any(|(decision, _)| *decision == ApprovalDecision::AllowAlways)
        );
    }

    #[test]
    fn number_keys_and_arrows_both_answer_the_dialog() {
        let mut dialog = ApprovalDialog::new(approval_fixture("s"));
        assert_eq!(
            dialog_action(&mut dialog, KeyEvent::from(KeyCode::Char('2'))),
            DialogAction::Decided(ApprovalDecision::AllowAlways)
        );
        assert_eq!(
            dialog_action(&mut dialog, KeyEvent::from(KeyCode::Down)),
            DialogAction::Moved
        );
        assert_eq!(
            dialog_action(&mut dialog, KeyEvent::from(KeyCode::Enter)),
            DialogAction::Decided(ApprovalDecision::AllowAlways)
        );
        assert_eq!(
            dialog_action(&mut dialog, KeyEvent::from(KeyCode::Esc)),
            DialogAction::Decided(ApprovalDecision::Deny)
        );
    }

    #[test]
    fn arbitrary_typed_text_can_never_answer_an_approval() {
        let mut dialog = ApprovalDialog::new(approval_fixture("s"));
        for ch in "yes ok sure Y".chars() {
            assert_eq!(
                dialog_action(&mut dialog, KeyEvent::from(KeyCode::Char(ch))),
                DialogAction::Ignored,
                "character {ch:?} answered an approval"
            );
        }
        // A digit past the last option is not a decision either.
        assert_eq!(
            dialog_action(&mut dialog, KeyEvent::from(KeyCode::Char('9'))),
            DialogAction::Ignored
        );
    }

    #[test]
    fn a_persistent_session_routes_its_decision_over_the_protocol() {
        assert_eq!(approval_route(true), ApprovalRoute::Protocol);
        assert_eq!(approval_route(false), ApprovalRoute::Broker);
    }

    #[test]
    fn deferred_mail_is_held_while_blocked_and_released_in_order_when_it_clears() {
        let mut deferred = DeferredDelivery::default();
        deferred.defer(Deferred {
            kind: "mail",
            id: "m1".to_string(),
            body: "result ready".to_string(),
        });
        deferred.defer(Deferred {
            kind: "attention",
            id: "a1".to_string(),
            body: "ping".to_string(),
        });
        assert!(deferred.resume(true).is_empty());
        assert_eq!(deferred.len(), 2);

        let released = deferred.resume(false);
        assert_eq!(
            released.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(),
            vec!["m1", "a1"]
        );
        assert!(deferred.is_empty());
        let notice = DeferredDelivery::notice(&released).expect("notice");
        assert_eq!(notice.kind, NoticeKind::DeferredDelivery);
    }

    #[test]
    fn deferring_the_same_item_twice_delivers_it_once() {
        let mut deferred = DeferredDelivery::default();
        for _ in 0..5 {
            deferred.defer(Deferred {
                kind: "mail",
                id: "m1".to_string(),
                body: "same".to_string(),
            });
        }
        assert_eq!(deferred.resume(false).len(), 1);
    }

    // ---------------- item 7: focus, shortcuts, layout, entry modes ----------------

    #[test]
    fn focus_cycles_only_through_visible_regions() {
        assert_eq!(
            focus_next(Focus::Composer, false, false),
            Focus::Transcript
        );
        assert_eq!(
            focus_next(Focus::Transcript, false, false),
            Focus::Composer
        );
        assert_eq!(focus_next(Focus::Transcript, true, false), Focus::Overview);
        assert_eq!(focus_next(Focus::Overview, true, true), Focus::Inspection);
        assert_eq!(focus_prev(Focus::Composer, true, true), Focus::Inspection);
    }

    #[test]
    fn an_open_approval_is_modal_and_tab_cannot_escape_it() {
        assert_eq!(focus_next(Focus::Approval, true, true), Focus::Approval);
        assert_eq!(focus_prev(Focus::Approval, true, true), Focus::Approval);
    }

    #[test]
    fn every_shortcut_is_discoverable_from_the_help_list() {
        let text = help_lines(None)
            .iter()
            .map(StyledLine::to_plain_string)
            .collect::<Vec<_>>()
            .join("\n");
        for shortcut in SHORTCUTS {
            assert!(text.contains(shortcut.keys), "{} missing", shortcut.keys);
            assert!(text.contains(shortcut.what));
        }
    }

    #[test]
    fn the_help_list_can_be_filtered_to_one_surface() {
        let text = help_lines(Some(Focus::Approval))
            .iter()
            .map(StyledLine::to_plain_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("1 / 2 / 3"));
        assert!(!text.contains("shift+enter"));
    }

    #[test]
    fn layout_drops_panels_as_the_terminal_narrows_and_never_starves_the_pane() {
        let narrow = resolve_layout(40, 22);
        assert!(!narrow.sidebar && !narrow.overview);
        assert!(narrow.compact);
        assert!(narrow.main_width >= 20);

        let eighty = resolve_layout(80, 24);
        assert!(!eighty.sidebar && !eighty.overview);
        assert!(!eighty.compact);

        let wide = resolve_layout(120, 40);
        assert!(wide.sidebar && !wide.overview);

        let widest = resolve_layout(200, 50);
        assert!(widest.sidebar && widest.overview);
        assert!(widest.main_width >= 20);
    }

    #[test]
    fn widening_the_terminal_never_removes_a_panel() {
        let mut had_sidebar = false;
        let mut had_overview = false;
        for width in 20..=240 {
            let plan = resolve_layout(width, 50);
            if plan.sidebar {
                had_sidebar = true;
            } else {
                assert!(!had_sidebar, "sidebar disappeared at width {width}");
            }
            if plan.overview {
                had_overview = true;
            } else {
                assert!(!had_overview, "overview disappeared at width {width}");
            }
            assert!(plan.main_width >= 20);
        }
    }

    #[test]
    fn a_resize_never_changes_which_rows_exist_only_how_they_wrap() {
        let overview = build_overview(
            &coordinator::Coordinator::default(),
            &[
                delegation_fixture("aa", delegation::Phase::Running),
                delegation_fixture("bb", delegation::Phase::Completed),
            ],
            &[],
            &[],
            300,
        );
        assert_eq!(overview.lines(200).len(), overview.rows.len() * 2);
        assert_eq!(overview.lines(40).len(), overview.rows.len());
    }

    #[test]
    fn a_cjk_worktree_path_is_measured_by_display_width_not_bytes() {
        let mut record = delegation_fixture("aa", delegation::Phase::Running);
        record.handle.workdir = PathBuf::from("/repo/\u{4f60}\u{597d}");
        let overview = build_overview(
            &coordinator::Coordinator::default(),
            &[record],
            &[],
            &[],
            300,
        );
        let line = &overview.lines(120)[1];
        assert!(line.display_width() < line.to_plain_string().len());
    }

    #[test]
    fn entry_mode_is_decided_by_the_first_character_only() {
        assert_eq!(classify_entry("/agents"), EntryMode::Slash);
        assert_eq!(classify_entry("!cargo build"), EntryMode::Shell);
        assert_eq!(classify_entry("look at @src/seat.rs"), EntryMode::File);
        assert_eq!(classify_entry("a/b is a path"), EntryMode::Text);
        assert_eq!(classify_entry(""), EntryMode::Text);
    }

    #[test]
    fn slash_completions_only_offer_commands_this_pane_implements() {
        let completions = slash_completions("/a");
        let labels: Vec<&str> = completions
            .iter()
            .map(|completion| completion.label.as_str())
            .collect();
        assert!(labels.contains(&"/agents"));
        assert!(labels.contains(&"/approve"));
        assert!(!labels.contains(&"/compact"));
        assert!(slash_completions("/zzz").is_empty());
    }

    #[test]
    fn the_file_picker_never_offers_a_path_outside_the_worktree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("wt");
        std::fs::create_dir_all(root.join("src")).expect("mkdir");
        std::fs::write(root.join("src/seat.rs"), "fn main() {}").expect("write");
        std::fs::write(dir.path().join("outside.txt"), "nope").expect("write");

        let completions = file_completions("@seat", &root);
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].insert, "@src/seat.rs");

        assert!(contained_ref(&root, "../outside.txt").is_none());
        assert!(contained_ref(&root, "src/../../outside.txt").is_none());
        assert_eq!(
            contained_ref(&root, "src/./seat.rs"),
            Some(PathBuf::from("src/seat.rs"))
        );
    }

    #[test]
    fn the_file_picker_is_bounded_even_in_a_large_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        for i in 0..(FILE_COMPLETION_CAP * 3) {
            std::fs::write(dir.path().join(format!("file{i}.rs")), "x").expect("write");
        }
        assert!(file_completions("@file", dir.path()).len() <= FILE_COMPLETION_CAP);
    }

    #[test]
    fn a_bang_line_yields_a_command_for_the_process_tool_not_an_execution() {
        assert_eq!(shell_command("!cargo nextest run"), Some("cargo nextest run"));
        assert_eq!(shell_command("!   "), None);
        assert_eq!(shell_command("cargo build"), None);
    }

    // ---------------- item 8: bounds ----------------

    #[test]
    fn a_very_long_history_is_bounded_without_copying_it() {
        let items: Vec<usize> = (0..200_000).collect();
        let budget = Budget::default();
        let bounded = bound_slice(&items, budget.max_items);
        assert_eq!(bounded.items.len(), budget.max_items);
        assert_eq!(bounded.elided, 200_000 - budget.max_items);
        // The newest items are the ones kept.
        assert_eq!(*bounded.items.last().expect("last"), 199_999);
    }

    #[test]
    fn bounding_lines_keeps_the_tail_and_marks_what_was_dropped() {
        let lines: Vec<StyledLine> = (0..5000)
            .map(|i| StyledLine::plain(format!("line {i}")))
            .collect();
        let bounded = bound_lines(lines, 100);
        assert_eq!(bounded.len(), 101);
        assert!(bounded[0].to_plain_string().contains("4900 earlier lines"));
        assert!(bounded.last().expect("last").to_plain_string().contains("4999"));
    }

    #[test]
    fn a_large_fleet_polls_a_bounded_number_of_sessions_per_tick() {
        let budget = Budget::default();
        let plan = fanout_plan(500, &budget);
        assert_eq!(plan.polled, budget.max_sessions);
        assert_eq!(plan.deferred, 500 - budget.max_sessions);
        // A small fleet is never deferred at all.
        assert_eq!(fanout_plan(3, &budget), FanoutPlan { polled: 3, deferred: 0 });
    }

    #[test]
    fn rendering_a_huge_overview_stays_within_the_row_budget() {
        let records: Vec<delegation::Record> = (0..2_000)
            .map(|i| delegation_fixture(&format!("w{i:04}"), delegation::Phase::Running))
            .collect();
        let overview = build_overview(
            &coordinator::Coordinator::default(),
            &records,
            &[],
            &[],
            300,
        );
        let budget = Budget::default();
        let bounded = bound_slice(&overview.rows, budget.max_rows);
        assert_eq!(bounded.items.len(), budget.max_rows);
        let lines = bound_lines(overview.lines(120), budget.max_lines);
        assert!(lines.len() <= budget.max_lines + 1);
    }

    #[test]
    fn building_the_overview_twice_from_the_same_records_is_identical() {
        let records = vec![
            delegation_fixture("aa", delegation::Phase::Running),
            delegation_fixture("bb", delegation::Phase::Completed),
        ];
        let seats = vec![seat_fixture("s7", 1)];
        let first = build_overview(
            &coordinator::Coordinator::default(),
            &records,
            &seats,
            &[],
            300,
        );
        let second = build_overview(
            &coordinator::Coordinator::default(),
            &records,
            &seats,
            &[],
            300,
        );
        assert_eq!(first, second);
    }
}
