//! Portable native checkpoints (issue #486, roadmap N17).
//!
//! A checkpoint is the provider-neutral answer to "what is this session
//! actually doing, and what does it still owe?": the objective and the hard
//! constraints it was given, the decisions taken, the task/workflow it is
//! bound to, every acknowledged input, the claims it holds, the receipts for
//! actions that really completed, the tool calls that are still outstanding,
//! and the evidence those actions produced -- each by content hash, never by
//! a retelling.
//!
//! Everything here is derived from the journal and from an explicitly typed
//! [`CheckpointContext`]. Nothing is inferred from prose and nothing is
//! invented: a tool whose outcome is unknown is carried as unknown, and an
//! acknowledged input is carried verbatim. [`build`] is pure -- no fs, clock,
//! env or net -- so the same conversation always produces the same
//! checkpoint; [`commit`] is the single I/O seam.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::super::CtxResult;
use super::super::state::{StateDir, create_private_dir_all, write_private};
use super::journal::{
    AssistantBlock, CheckpointId, CheckpointKind, ContentRef, ConversationState, EventScope,
    ExecutionState, Journal, JournalSessionId, MessageRole, RouteIdentity, SequenceId,
    TaskReceiptState,
};

/// Bumped on any shape change. A checkpoint written by a version this build
/// does not recognise is not "close enough": it is skipped, and an older
/// valid one is used instead (see [`latest_valid`]).
pub const CHECKPOINT_SCHEMA_VERSION: u32 = 1;

/// How much of one carried text a checkpoint stores. Long enough for a real
/// operator instruction, short enough that a checkpoint stays a checkpoint.
const MAX_TEXT_BYTES: usize = 4096;
/// How many items one list may carry. The OLDEST entries are dropped first
/// for narrative lists, and the NEWEST are never dropped for anything that
/// records an obligation.
const MAX_ITEMS: usize = 64;

/// The route a checkpoint was taken on, in portable string form. Compared
/// field-by-field against a target route to decide whether a resume may keep
/// the provider's own opaque continuation envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableRoute {
    pub route: String,
    pub provider: String,
    pub endpoint: String,
    pub account: String,
    pub protocol: String,
    pub vendor: String,
    pub model: String,
}

impl From<&RouteIdentity> for PortableRoute {
    fn from(route: &RouteIdentity) -> Self {
        Self {
            route: route.route.to_string(),
            provider: route.provider.to_string(),
            endpoint: route.endpoint.to_string(),
            account: route.account.to_string(),
            protocol: format!("{:?}", route.protocol),
            vendor: route.model.vendor.to_string(),
            model: route.model.id.clone(),
        }
    }
}

/// One acknowledged input, kept verbatim. Present whether or not it has been
/// delivered: compaction may never drop acknowledged user input, so the
/// checkpoint records all of it and marks what is still owed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcknowledgedInput {
    pub message_id: String,
    pub sequence: u64,
    pub text: String,
    pub steering: bool,
    /// `true` while this input has not yet been folded into a provider
    /// request. A restored session must still deliver it.
    pub pending: bool,
}

/// A task this session holds, from the journal's own receipts. A reference,
/// never a copy of `task.rs`'s card store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskClaim {
    pub task: String,
    pub state: String,
    pub since_sequence: u64,
}

/// A completed action, with the durable record a reader can go and check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionReceipt {
    pub tool_call: String,
    pub tool: String,
    pub state: String,
    pub sequence: u64,
    /// SHA-256 of the stored artifact when the result was offloaded; absent
    /// for an inline result.
    pub artifact_sha256: Option<String>,
    pub result_bytes: u64,
}

/// A tool call whose effect is not settled. `outcome_unknown` is its own
/// state, never folded into "failed": an effect that began and never
/// reported cannot be retried, only reconciled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutstandingTool {
    pub tool_call: String,
    pub tool: String,
    pub state: String,
    pub arguments_digest: String,
}

/// Evidence by path/hash. `handle` is the journal artifact's SHA-256, which
/// `Journal::read_artifact` resolves; `path` is filled when the producing
/// call named one in its arguments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub handle: String,
    pub byte_len: u64,
    pub media_type: String,
    pub tool: String,
    pub path: Option<String>,
}

/// The narrative part of a checkpoint, and the only part a model may write.
/// `source` says which producer wrote it, so a reader never has to guess
/// whether a summary came from a model call or from the deterministic
/// structural fallback.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DistilledSummary {
    pub source: String,
    pub model: Option<String>,
    pub text: String,
    #[serde(default)]
    pub decisions: Vec<String>,
}

impl DistilledSummary {
    pub const STRUCTURAL: &'static str = "structural";
    pub const ROUTE: &'static str = "route";
}

/// The typed facts a checkpoint cannot derive from the journal. Supplied by
/// the caller, never parsed out of prose.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckpointContext {
    /// Operator-stated constraints for this session, from the same typed
    /// source `runtime::context::CompileRequest::constraints` reads.
    pub hard_constraints: Vec<String>,
    pub task: Option<String>,
    pub workflow: Option<String>,
    /// Why this checkpoint was taken, in the trigger vocabulary of
    /// `compaction::CompactionTrigger`.
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortableCheckpoint {
    pub schema_version: u32,
    pub checkpoint_id: String,
    pub session: String,
    pub generation: u64,
    pub created_at: u64,
    pub reason: String,
    pub route: PortableRoute,
    /// The highest journal sequence this checkpoint's summary stands in for.
    /// Everything after it is replayed verbatim.
    pub covers_through: u64,
    /// The journal's last sequence when the checkpoint was built.
    pub last_sequence: u64,
    /// The first acknowledged input: what this session was told to do.
    pub objective: Option<String>,
    pub hard_constraints: Vec<String>,
    pub task: Option<String>,
    pub workflow: Option<String>,
    pub acknowledged_input: Vec<AcknowledgedInput>,
    pub claims: Vec<TaskClaim>,
    pub receipts: Vec<ActionReceipt>,
    pub outstanding_tools: Vec<OutstandingTool>,
    pub evidence: Vec<EvidenceRef>,
    pub summary: DistilledSummary,
}

impl PortableCheckpoint {
    /// Whether this checkpoint is usable for `session` against a journal whose
    /// last sequence is `last_sequence`. A checkpoint from a schema this build
    /// does not know, for another session, or claiming to cover events the
    /// journal does not have is not repaired -- it is skipped.
    pub fn is_valid_for(&self, session: &JournalSessionId, last_sequence: SequenceId) -> bool {
        self.schema_version == CHECKPOINT_SCHEMA_VERSION
            && self.session == session.as_str()
            && self.covers_through <= last_sequence.0
            && self.covers_through <= self.last_sequence
    }

    /// Acknowledged input this checkpoint still owes a restored session.
    pub fn pending_input(&self) -> Vec<&AcknowledgedInput> {
        self.acknowledged_input
            .iter()
            .filter(|input| input.pending)
            .collect()
    }

    /// Whether the provider's own opaque continuation envelope may be reused.
    /// Every identity field must match; a model or endpoint change is a
    /// different conversation as far as any provider is concerned.
    pub fn same_route_as(&self, route: &RouteIdentity) -> bool {
        self.route == PortableRoute::from(route)
    }
}

/// The last sequence a summary may stand in for, or `None` when compacting
/// would settle nothing.
///
/// Two rules, both about never lying to the model:
///
/// 1. The boundary never crosses an unsettled tool call. A call whose latest
///    execution is `Prepared`, `Started` or `OutcomeUnknown` -- and every
///    message from its assistant message onward -- stays verbatim, so a
///    pending action can never be summarised into "done".
/// 2. The newest `retain_recent` messages stay verbatim, so the model keeps
///    the immediate context it is mid-way through.
pub fn boundary(state: &ConversationState, retain_recent: usize) -> Option<SequenceId> {
    let unsettled_from = first_unsettled_sequence(state);
    let keep_from = if state.messages.len() > retain_recent {
        state.messages[state.messages.len() - retain_recent].sequence
    } else {
        state.messages.first()?.sequence
    };
    let cut = match unsettled_from {
        Some(unsettled) => unsettled.min(keep_from),
        None => keep_from,
    };
    // The boundary is the last message STRICTLY before the cut: the cut
    // message itself is retained verbatim.
    let covered = state
        .messages
        .iter()
        .rfind(|message| message.sequence < cut)?;
    Some(covered.sequence)
}

/// The sequence of the earliest assistant message holding a tool call whose
/// latest execution has not settled, or `None` when everything settled.
fn first_unsettled_sequence(state: &ConversationState) -> Option<SequenceId> {
    let latest = latest_executions(state);
    let mut earliest: Option<SequenceId> = None;
    for message in &state.messages {
        if message.role != MessageRole::Assistant {
            continue;
        }
        for block in &message.blocks {
            let AssistantBlock::ToolCall { tool_call } = block else {
                continue;
            };
            let settled = latest
                .get(tool_call)
                .is_some_and(|execution| execution.state.is_terminal());
            if !settled {
                earliest = Some(earliest.map_or(message.sequence, |seq| seq.min(message.sequence)));
            }
        }
    }
    earliest
}

/// The authoritative execution per tool call: the highest journal sequence.
/// Same rule as `native::latest_executions`, kept here so this module needs
/// nothing from the loop.
fn latest_executions(
    state: &ConversationState,
) -> BTreeMap<&super::journal::ToolCallId, &super::journal::ExecutionRecord> {
    let mut latest: BTreeMap<&super::journal::ToolCallId, &super::journal::ExecutionRecord> =
        BTreeMap::new();
    for execution in state.executions.values() {
        match latest.entry(&execution.tool_call) {
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

/// Builds one checkpoint. PURE: every field comes from `state`, `context`,
/// `summary` and the two scalars, so the same inputs always give the same
/// checkpoint and a test needs no journal on disk to check the shape.
pub fn build(
    state: &ConversationState,
    covers_through: SequenceId,
    delivered_through: SequenceId,
    checkpoint_id: &CheckpointId,
    context: &CheckpointContext,
    summary: DistilledSummary,
    created_at: u64,
) -> PortableCheckpoint {
    let latest = latest_executions(state);

    let mut acknowledged_input = Vec::new();
    let mut objective = None;
    for message in &state.messages {
        if message.role != MessageRole::User {
            continue;
        }
        let text = bound(message.text.as_deref().unwrap_or_default());
        if objective.is_none() && !message.steering {
            objective = Some(text.clone());
        }
        acknowledged_input.push(AcknowledgedInput {
            message_id: message.message_id.to_string(),
            sequence: message.sequence.0,
            text,
            steering: message.steering,
            pending: message.sequence > delivered_through,
        });
    }

    let mut receipts = Vec::new();
    let mut outstanding_tools = Vec::new();
    let mut evidence = Vec::new();
    for (tool_call, execution) in &latest {
        let Some(record) = state.tool_calls.get(*tool_call) else {
            continue;
        };
        if execution.state.is_terminal() {
            let (artifact_sha256, result_bytes) = match &execution.result {
                Some(ContentRef::Artifact {
                    sha256,
                    byte_len,
                    media_type,
                    ..
                }) => {
                    evidence.push(EvidenceRef {
                        handle: sha256.clone(),
                        byte_len: *byte_len,
                        media_type: media_type.clone(),
                        tool: record.name.clone(),
                        path: argument_path(&record.arguments),
                    });
                    (Some(sha256.clone()), *byte_len)
                }
                Some(other) => (None, other.byte_len()),
                None => (None, 0),
            };
            receipts.push(ActionReceipt {
                tool_call: tool_call.to_string(),
                tool: record.name.clone(),
                state: state_label(execution.state),
                sequence: execution.sequence.0,
                artifact_sha256,
                result_bytes,
            });
        } else {
            outstanding_tools.push(OutstandingTool {
                tool_call: tool_call.to_string(),
                tool: record.name.clone(),
                state: state_label(execution.state),
                arguments_digest: format!(
                    "{:016x}",
                    super::super::event::input_hash(
                        &serde_json::to_string(&record.arguments).unwrap_or_default()
                    )
                ),
            });
        }
    }
    // A tool call with no execution record at all is outstanding too: it was
    // admitted and durably recorded, and nothing has reported on it.
    for (tool_call, record) in &state.tool_calls {
        if latest.contains_key(tool_call) {
            continue;
        }
        outstanding_tools.push(OutstandingTool {
            tool_call: tool_call.to_string(),
            tool: record.name.clone(),
            state: "prepared".to_string(),
            arguments_digest: format!(
                "{:016x}",
                super::super::event::input_hash(
                    &serde_json::to_string(&record.arguments).unwrap_or_default()
                )
            ),
        });
    }
    outstanding_tools.sort_by(|a, b| a.tool_call.cmp(&b.tool_call));

    let mut claims = Vec::new();
    for (task, history) in &state.task_receipts {
        let Some(last) = history.last() else {
            continue;
        };
        if matches!(
            last.state,
            TaskReceiptState::Completed | TaskReceiptState::Failed | TaskReceiptState::Cancelled
        ) {
            continue;
        }
        claims.push(TaskClaim {
            task: task.to_string(),
            state: receipt_label(last.state),
            since_sequence: last.sequence.0,
        });
    }

    // Obligations keep their NEWEST entries; a truncated receipt list only
    // costs a reader history, a truncated outstanding list would hide work.
    truncate_oldest(&mut receipts, MAX_ITEMS);
    truncate_newest_kept(&mut outstanding_tools, MAX_ITEMS);
    truncate_oldest(&mut evidence, MAX_ITEMS);
    truncate_newest_kept(&mut acknowledged_input, MAX_ITEMS);

    PortableCheckpoint {
        schema_version: CHECKPOINT_SCHEMA_VERSION,
        checkpoint_id: checkpoint_id.to_string(),
        session: state.identity.session.to_string(),
        generation: state.identity.generation,
        created_at,
        reason: context.reason.clone(),
        route: PortableRoute::from(&state.identity.route),
        covers_through: covers_through.0,
        last_sequence: state.last_sequence.0,
        objective,
        hard_constraints: context
            .hard_constraints
            .iter()
            .map(|text| bound(text))
            .take(MAX_ITEMS)
            .collect(),
        task: context.task.clone(),
        workflow: context.workflow.clone(),
        acknowledged_input,
        claims,
        receipts,
        outstanding_tools,
        evidence,
        summary,
    }
}

/// Keeps the newest `limit` entries, dropping from the front.
fn truncate_newest_kept<T>(items: &mut Vec<T>, limit: usize) {
    if items.len() > limit {
        items.drain(..items.len() - limit);
    }
}

/// Keeps the oldest `limit` entries, dropping from the back.
fn truncate_oldest<T>(items: &mut Vec<T>, limit: usize) {
    items.truncate(limit);
}

/// The wire spelling of an execution state. Shared with `compaction.rs` so
/// the checkpoint, the rebuilt history and the summary message all call an
/// unknown outcome by the same name.
pub fn state_label(state: ExecutionState) -> String {
    match state {
        ExecutionState::Prepared => "prepared",
        ExecutionState::Started => "started",
        ExecutionState::Completed => "completed",
        ExecutionState::Failed => "failed",
        ExecutionState::Cancelled => "cancelled",
        ExecutionState::OutcomeUnknown => "outcome_unknown",
    }
    .to_string()
}

fn receipt_label(state: TaskReceiptState) -> String {
    match state {
        TaskReceiptState::Accepted => "accepted",
        TaskReceiptState::Started => "started",
        TaskReceiptState::Blocked => "blocked",
        TaskReceiptState::Completed => "completed",
        TaskReceiptState::Failed => "failed",
        TaskReceiptState::Cancelled => "cancelled",
    }
    .to_string()
}

/// The `path`/`file`/`target` argument a tool call named, when it named one.
/// Presentation only: evidence is resolved by hash, never by this string.
fn argument_path(arguments: &serde_json::Value) -> Option<String> {
    for key in ["path", "file", "file_path", "target"] {
        if let Some(value) = arguments.get(key).and_then(serde_json::Value::as_str) {
            return Some(bound(value));
        }
    }
    None
}

/// Truncates on a char boundary, so a bounded text is always valid UTF-8.
fn bound(text: &str) -> String {
    if text.len() <= MAX_TEXT_BYTES {
        return text.to_string();
    }
    let mut end = MAX_TEXT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Commits one checkpoint.
///
/// ATOMIC AND ORDERED. The portable export is written first with a temp
/// sibling and a rename (`state::write_private`), so no reader ever sees a
/// half-written file. The journal event is appended second and IS the commit
/// point: before it the session is uncompacted, after it the checkpoint is in
/// force. A crash in between leaves an orphan export that nothing reads --
/// never a session that half-compacted.
///
/// The export failing is not a reason to refuse the commit: the journal event
/// carries the whole checkpoint, so the export is a convenience copy.
pub fn commit(
    journal: &mut Journal,
    state_dir: Option<&StateDir>,
    generation: u64,
    scope: &EventScope,
    kind: CheckpointKind,
    checkpoint: &PortableCheckpoint,
    committed_at: u64,
) -> CtxResult<SequenceId> {
    let session = JournalSessionId::new(checkpoint.session.clone())?;
    let portable = serde_json::to_value(checkpoint)?;
    if let Some(state_dir) = state_dir {
        let _ = export(state_dir, checkpoint);
    }
    let checkpoint_id = super::journal::CheckpointId::new(checkpoint.checkpoint_id.clone())?;
    Ok(journal.record_checkpoint(
        &session,
        generation,
        scope,
        checkpoint_id,
        kind,
        portable,
        committed_at,
    )?)
}

/// Writes the portable export. Best-effort by design -- see [`commit`].
pub fn export(state_dir: &StateDir, checkpoint: &PortableCheckpoint) -> std::io::Result<()> {
    let dir = state_dir.native_checkpoints();
    create_private_dir_all(&dir)?;
    let name = format!(
        "{:016x}-{}.json",
        super::super::event::input_hash(&checkpoint.session),
        sanitize(&checkpoint.checkpoint_id)
    );
    let json = serde_json::to_string_pretty(checkpoint)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    write_private(&dir.join(name), &json)
}

fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The newest checkpoint of `kind` that this build can actually use.
///
/// Walks newest-first and SKIPS anything unusable -- an unknown schema, a
/// payload that no longer deserialises, a checkpoint for another session, one
/// claiming to cover events the journal does not have -- so a bad checkpoint
/// costs a session the newest summary, never the ability to resume. Returns
/// `None` when no valid checkpoint exists at all, which is simply an
/// uncompacted session.
pub fn latest_valid(
    journal: &Journal,
    session: &JournalSessionId,
    kind: CheckpointKind,
) -> CtxResult<Option<PortableCheckpoint>> {
    let state = journal.replay(session)?;
    let mut candidates: Vec<(SequenceId, &super::journal::CheckpointRecord)> = state
        .checkpoints
        .values()
        .filter(|record| record.kind == kind)
        .map(|record| (record.sequence, record))
        .collect();
    candidates.sort_by_key(|(sequence, _)| *sequence);
    for (_, record) in candidates.iter().rev() {
        let Ok(checkpoint) =
            serde_json::from_value::<PortableCheckpoint>(record.portable_state.clone())
        else {
            continue;
        };
        if checkpoint.is_valid_for(session, state.last_sequence) {
            return Ok(Some(checkpoint));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::runtime::journal::{
        ExecutionRecord, MessageId, SessionIdentity, StoredMessage, ToolCallId, ToolCallRecord,
    };
    use crate::commands::ctx::runtime::testsupport::{route_identity, session_identity};

    fn empty_state(identity: SessionIdentity) -> ConversationState {
        ConversationState {
            identity,
            last_sequence: SequenceId(0),
            messages: Vec::new(),
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
            message_id: MessageId::new(format!("msg-{sequence}")).expect("id"),
            role: MessageRole::User,
            blocks: Vec::new(),
            text: Some(text.to_string()),
            steering: false,
            usage: None,
        }
    }

    fn assistant(sequence: u64, calls: &[&str]) -> StoredMessage {
        StoredMessage {
            sequence: SequenceId(sequence),
            message_id: MessageId::new(format!("msg-{sequence}")).expect("id"),
            role: MessageRole::Assistant,
            blocks: calls
                .iter()
                .map(|call| AssistantBlock::ToolCall {
                    tool_call: ToolCallId::new(*call).expect("id"),
                })
                .collect(),
            text: None,
            steering: false,
            usage: None,
        }
    }

    fn with_call(
        state: &mut ConversationState,
        call: &str,
        name: &str,
        sequence: u64,
        execution: Option<(ExecutionState, u64)>,
    ) {
        let id = ToolCallId::new(call).expect("id");
        state.tool_calls.insert(
            id.clone(),
            ToolCallRecord {
                sequence: SequenceId(sequence),
                name: name.to_string(),
                arguments: serde_json::json!({ "path": "src/lib.rs" }),
                policy: super::super::journal::PolicyProvenance {
                    fingerprint: String::new(),
                    source: "test".to_string(),
                    decision: "allow".to_string(),
                    scope: "worker".to_string(),
                },
            },
        );
        if let Some((execution_state, at)) = execution {
            state.executions.insert(
                super::super::journal::ExecutionId::new(format!("exec-{call}")).expect("id"),
                ExecutionRecord {
                    sequence: SequenceId(at),
                    tool_call: id,
                    state: execution_state,
                    result: Some(ContentRef::Inline {
                        text: "ok".to_string(),
                    }),
                    detail: None,
                },
            );
        }
    }

    fn structural() -> DistilledSummary {
        DistilledSummary {
            source: DistilledSummary::STRUCTURAL.to_string(),
            model: None,
            text: "summary".to_string(),
            decisions: Vec::new(),
        }
    }

    #[test]
    fn the_boundary_never_crosses_an_unsettled_tool_call() {
        let mut state = empty_state(session_identity("s1", route_identity()));
        state.messages = vec![
            user(1, "do the thing"),
            assistant(2, &["call-a"]),
            assistant(4, &["call-b"]),
            user(6, "and this too"),
            user(7, "and this"),
            user(8, "and this as well"),
        ];
        state.last_sequence = SequenceId(8);
        with_call(
            &mut state,
            "call-a",
            "read",
            3,
            Some((ExecutionState::Completed, 3)),
        );
        // `call-b` is still running: nothing from sequence 4 onward may be
        // summarised away.
        with_call(
            &mut state,
            "call-b",
            "bash",
            5,
            Some((ExecutionState::Started, 5)),
        );
        assert_eq!(boundary(&state, 2), Some(SequenceId(2)));
    }

    #[test]
    fn an_outcome_unknown_execution_is_outstanding_never_a_receipt() {
        let mut state = empty_state(session_identity("s1", route_identity()));
        state.messages = vec![user(1, "go"), assistant(2, &["call-a"])];
        state.last_sequence = SequenceId(3);
        with_call(
            &mut state,
            "call-a",
            "bash",
            3,
            Some((ExecutionState::OutcomeUnknown, 4)),
        );
        let checkpoint = build(
            &state,
            SequenceId(1),
            SequenceId(1),
            &CheckpointId::new("cp-1").expect("id"),
            &CheckpointContext::default(),
            structural(),
            10,
        );
        assert!(checkpoint.receipts.is_empty());
        assert_eq!(checkpoint.outstanding_tools.len(), 1);
        assert_eq!(checkpoint.outstanding_tools[0].state, "outcome_unknown");
    }

    #[test]
    fn every_acknowledged_input_is_carried_and_undelivered_input_stays_pending() {
        let mut state = empty_state(session_identity("s1", route_identity()));
        state.messages = vec![user(1, "objective"), user(5, "late steering")];
        state.last_sequence = SequenceId(5);
        let checkpoint = build(
            &state,
            SequenceId(3),
            SequenceId(3),
            &CheckpointId::new("cp-1").expect("id"),
            &CheckpointContext::default(),
            structural(),
            10,
        );
        assert_eq!(checkpoint.acknowledged_input.len(), 2);
        assert_eq!(checkpoint.objective.as_deref(), Some("objective"));
        assert_eq!(checkpoint.pending_input().len(), 1);
        assert_eq!(checkpoint.pending_input()[0].text, "late steering");
    }

    #[test]
    fn a_completed_call_becomes_a_receipt_with_its_evidence_handle() {
        let mut state = empty_state(session_identity("s1", route_identity()));
        state.messages = vec![user(1, "go"), assistant(2, &["call-a"])];
        state.last_sequence = SequenceId(4);
        with_call(&mut state, "call-a", "bash", 3, None);
        state.executions.insert(
            super::super::journal::ExecutionId::new("exec-a").expect("id"),
            ExecutionRecord {
                sequence: SequenceId(4),
                tool_call: ToolCallId::new("call-a").expect("id"),
                state: ExecutionState::Completed,
                result: Some(ContentRef::Artifact {
                    sha256: "abc123".to_string(),
                    byte_len: 900,
                    content_hash: 7,
                    media_type: "text/plain".to_string(),
                }),
                detail: None,
            },
        );
        let checkpoint = build(
            &state,
            SequenceId(2),
            SequenceId(2),
            &CheckpointId::new("cp-1").expect("id"),
            &CheckpointContext::default(),
            structural(),
            10,
        );
        assert_eq!(checkpoint.receipts.len(), 1);
        assert_eq!(
            checkpoint.receipts[0].artifact_sha256.as_deref(),
            Some("abc123")
        );
        assert_eq!(checkpoint.evidence.len(), 1);
        assert_eq!(checkpoint.evidence[0].handle, "abc123");
        assert_eq!(checkpoint.evidence[0].path.as_deref(), Some("src/lib.rs"));
    }

    #[test]
    fn a_checkpoint_from_another_session_or_schema_is_not_valid() {
        let state = empty_state(session_identity("s1", route_identity()));
        let mut checkpoint = build(
            &state,
            SequenceId(0),
            SequenceId(0),
            &CheckpointId::new("cp-1").expect("id"),
            &CheckpointContext::default(),
            structural(),
            10,
        );
        let session = JournalSessionId::new("s1").expect("id");
        assert!(checkpoint.is_valid_for(&session, SequenceId(4)));
        checkpoint.schema_version = CHECKPOINT_SCHEMA_VERSION + 1;
        assert!(!checkpoint.is_valid_for(&session, SequenceId(4)));
        checkpoint.schema_version = CHECKPOINT_SCHEMA_VERSION;
        checkpoint.session = "other".to_string();
        assert!(!checkpoint.is_valid_for(&session, SequenceId(4)));
    }

    #[test]
    fn a_checkpoint_covering_more_than_the_journal_holds_is_refused() {
        let state = empty_state(session_identity("s1", route_identity()));
        let mut checkpoint = build(
            &state,
            SequenceId(0),
            SequenceId(0),
            &CheckpointId::new("cp-1").expect("id"),
            &CheckpointContext::default(),
            structural(),
            10,
        );
        checkpoint.covers_through = 99;
        checkpoint.last_sequence = 99;
        let session = JournalSessionId::new("s1").expect("id");
        assert!(!checkpoint.is_valid_for(&session, SequenceId(4)));
    }

    #[test]
    fn same_route_as_compares_every_identity_field() {
        let state = empty_state(session_identity("s1", route_identity()));
        let checkpoint = build(
            &state,
            SequenceId(0),
            SequenceId(0),
            &CheckpointId::new("cp-1").expect("id"),
            &CheckpointContext::default(),
            structural(),
            10,
        );
        assert!(checkpoint.same_route_as(&route_identity()));
        let mut other = route_identity();
        other.model.id = "another-model".to_string();
        assert!(!checkpoint.same_route_as(&other));
    }
}
