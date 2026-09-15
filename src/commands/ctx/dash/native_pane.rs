//! Issue #480 (roadmap N11): the native conversation view -- a pure,
//! deterministic reducer from N03's [`journal::ConversationState`] to a
//! transcript view model, plus the composer/scroll/status presentation state
//! that a native pane owns, and two renderers (ratatui and plain text) over
//! the exact same view model.
//!
//! # What lives here and what does not
//!
//! Presentation state (scroll, selection, focus, draft, expanded tool calls)
//! is a separate struct ([`NativePresentation`]) from runtime/session
//! ownership: [`NativePaneRuntime`] is the one thing in this module that
//! owns a live session (`runtime::native::InteractiveSession`, itself a
//! thin handle onto a background thread -- no PTY, no vt100 -- see that
//! module's own doc comment), and it never touches raw mode itself, only
//! the same `dash::mod` terminal-setup helpers a wrapped dashboard already
//! calls (`install_panic_hook`/`enable_raw_mode`/`push_keyboard_
//! enhancement`/`teardown_terminal`/`restore_panic_hook`, reused verbatim by
//! [`run_native_dashboard`], never modified).
//!
//! [`run_native_dashboard`] is a SEPARATE, additional entry point from
//! `dash::mod::run_dashboard`: it does not add a `PaneKind` to `dash::mod`'s
//! existing `Vec<Pane>` (that list, and the ~100 call sites that thread it
//! through mail sweep/budget accounting/attention projection/restore
//! roster, stay untouched, so nothing about a wrapped-harness dashboard's
//! existing behaviour changes). Today a native pane is its own dedicated,
//! single-pane dashboard mode, opened by `zirv chat --runtime native`
//! rather than mixed into a multi-pane wrapped dashboard; see the design
//! note (`docs/design/2026-09-13-native-pane.md`) for exactly what mixed-
//! pane integration this still owes and why it was scoped out here.
//!
//! [`build_transcript`] is the reducer: pure, total, and free of I/O, the
//! clock or randomness, so replaying the same
//! [`journal::ConversationState`] (itself already a pure reduction of the
//! same committed events, however many times they are replayed --
//! `journal::Journal::replay`) always yields byte-identical
//! [`TranscriptView`]s. `render_lines`/`render_plain`/`render_native_pane`
//! are likewise pure functions of a view model and presentation state, so
//! every rendering behaviour below (unicode/CJK/emoji width, wrapping, a
//! narrow pane, follow-mode scrolling) is covered by tests that construct a
//! view model directly and never touch a terminal.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::style::{self, Tone};

use super::super::CtxResult;
use super::super::config::{CtxConfig, EnvLookup};
use super::super::runtime::context::{ResolvedInstructionSource, SourceTrust};
use super::super::runtime::journal::{
    AssistantBlock, ContentRef, ConversationState, EventScope, ExecutionRecord, ExecutionState,
    Journal, JournalEvent, JournalSessionId, MessageId, MessageRole, RouteIdentity, ToolCallId,
};
use super::super::runtime::native::{
    self, InteractiveProgress, InteractiveRequest, InteractiveSession,
    SessionState as NativeSessionState, TurnState as NativeTurnState,
};
use super::super::state::{StateDir, now_secs};

// =========================================================================
// Transcript view model -- pure reduction of `ConversationState`
// =========================================================================

/// One tool call's classified outcome, derived from its latest
/// [`ExecutionRecord`]. Classification is a presentation-layer heuristic
/// (tool name plus result shape): the journal itself stores only a
/// `ContentRef` and an `ExecutionState`, never "this was a diff" -- see
/// [`classify_outcome`].
#[derive(Clone, Debug, PartialEq)]
pub enum ToolOutcomeView {
    /// Recorded, not yet started.
    Pending,
    /// The effect is in flight.
    Running,
    /// Never started, and now never will be.
    Cancelled,
    /// Completed with an outcome-unknown result: needs reconciliation before
    /// any retry (mirrors `journal::ExecutionState::OutcomeUnknown`).
    OutcomeUnknown,
    /// A unified diff (`--- `/`+++ `/`@@` markers detected in the result).
    Diff { unified: String },
    /// A test run's result. `passed`/`failed` are a best-effort parse of the
    /// raw text (`None` when no recognizable count is present) -- the raw
    /// text is always kept so nothing is lost when the parse misses.
    TestOutcome {
        raw: String,
        passed: Option<u32>,
        failed: Option<u32>,
    },
    /// A binary/non-text result stored content-addressed in the journal's
    /// artifact table.
    Artifact {
        sha256: String,
        media_type: String,
        byte_len: u64,
    },
    /// Plain text result, no more specific classification applied.
    Text { content: String },
    /// A failed execution.
    Error { message: String },
}

/// One entry in the native transcript, in the same order
/// `ConversationState::messages` (and each message's own `blocks`) already
/// establishes -- see [`build_transcript`]'s own doc comment for why that
/// order is authoritative and never re-sorted here.
#[derive(Clone, Debug, PartialEq)]
pub enum TranscriptItem {
    User {
        message_id: String,
        text: String,
        /// Mid-turn steering input (`journal::JournalEvent::InputAcknowledged
        /// { steering: true, .. }`), rendered distinctly from an ordinary
        /// turn-starting submission.
        steering: bool,
    },
    AssistantText {
        message_id: String,
        text: String,
    },
    AssistantThinking {
        message_id: String,
        text: String,
    },
    AssistantRefusal {
        message_id: String,
        text: String,
    },
    ToolCall {
        tool_call_id: String,
        message_id: String,
        name: String,
        /// A compact single-line rendering of the call arguments (JSON),
        /// shown even when the call is collapsed.
        arguments_preview: String,
        outcome: ToolOutcomeView,
    },
    SessionEnded {
        reason: String,
    },
    /// PR #531 review finding 4: a marker standing in for `hidden` older
    /// items dropped by [`cap_transcript_items`] once a transcript grows
    /// past [`MAX_TRANSCRIPT_ITEMS`]. Always the first item in a capped
    /// view, never produced by [`build_transcript`] itself.
    Elided {
        hidden: usize,
    },
}

impl TranscriptItem {
    /// The key [`NativePresentation::expanded`] toggles on, or `None` for an
    /// item with no collapsed/expanded distinction.
    pub fn expand_key(&self) -> Option<&str> {
        match self {
            TranscriptItem::ToolCall { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        }
    }
}

/// The complete, pure reduction of a session's [`ConversationState`] into
/// transcript entries. No presentation concerns -- expanded/collapsed,
/// scroll, focus -- live here; see [`NativePresentation`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TranscriptView {
    pub items: Vec<TranscriptItem>,
}

/// Reduces `state` into a [`TranscriptView`]. Pure and total: every
/// `AssistantBlock::ToolCall` is resolved against `state.tool_calls`/
/// `state.executions` at call time, so a tool call whose record is somehow
/// missing (never observed in practice -- the journal enforces the
/// foreign-key order -- but not a panic-worthy invariant to lean on here)
/// still renders, as `"unknown_tool"` with a `Pending` outcome, rather than
/// panicking or silently dropping the block.
///
/// Ordering is exactly `state.messages`' own sequence order, message by
/// message and block by block within an assistant message -- the same order
/// the provider actually emitted, since `AssistantBlock::ToolCall` records
/// are interleaved into `blocks` at commit time. This is what makes replay
/// deterministic: two `ConversationState`s reduced from the same committed
/// event sequence carry byte-identical `messages`/`tool_calls`/`executions`
/// maps (journal invariant), so `build_transcript` -- touching no clock, no
/// randomness, no HashMap iteration order (every map here is a `BTreeMap`)
/// -- always returns the same `TranscriptView`.
pub fn build_transcript(state: &ConversationState) -> TranscriptView {
    let mut items = Vec::new();
    for message in &state.messages {
        match message.role {
            MessageRole::User => items.push(TranscriptItem::User {
                message_id: message.message_id.as_str().to_string(),
                text: message.text.clone().unwrap_or_default(),
                steering: message.steering,
            }),
            MessageRole::Assistant => {
                for block in &message.blocks {
                    let message_id = message.message_id.as_str().to_string();
                    match block {
                        AssistantBlock::Text { text } => {
                            // Official process streaming persists incremental text
                            // as committed observations. Present adjacent chunks
                            // as one response rather than one bubble per token.
                            if message_id.starts_with("execution-text-")
                                && let Some(TranscriptItem::AssistantText {
                                    message_id: previous,
                                    text: accumulated,
                                }) = items.last_mut()
                                && previous.starts_with("execution-text-")
                            {
                                accumulated.push_str(text);
                                continue;
                            }
                            items.push(TranscriptItem::AssistantText {
                                message_id,
                                text: text.clone(),
                            });
                        }
                        AssistantBlock::Thinking { text, .. } => {
                            items.push(TranscriptItem::AssistantThinking {
                                message_id,
                                text: text.clone(),
                            });
                        }
                        AssistantBlock::RedactedThinking { .. } => {
                            items.push(TranscriptItem::AssistantThinking {
                                message_id,
                                text: "[redacted]".to_string(),
                            });
                        }
                        AssistantBlock::Refusal { text } => {
                            items.push(TranscriptItem::AssistantRefusal {
                                message_id,
                                text: text.clone(),
                            });
                        }
                        AssistantBlock::ToolCall { tool_call } => {
                            items.push(build_tool_call_item(state, message_id, tool_call));
                        }
                    }
                }
            }
        }
    }
    if let Some(reason) = &state.ended_reason {
        items.push(TranscriptItem::SessionEnded {
            reason: reason.clone(),
        });
    }
    TranscriptView { items }
}

/// PR #531 review finding 4: an unbounded transcript re-rendered from a full
/// journal replay on every ~150ms dashboard tick eventually re-lays out
/// (and re-allocates) an ever-growing item list even though only the tail
/// is ever new. This is the bound: displayed items are capped at
/// `max_items`, keeping the NEWEST ones (a live conversation cares about
/// what just happened, not the start), with a single [`TranscriptItem::
/// Elided`] marker standing in for however many older items were dropped.
/// A no-op when `view` is already at or under the cap. Pure, so it is
/// tested directly against a hand-built [`TranscriptView`] rather than
/// through a live session.
pub fn cap_transcript_items(view: TranscriptView, max_items: usize) -> TranscriptView {
    if view.items.len() <= max_items || max_items == 0 {
        return view;
    }
    // One slot of the cap is spent on the marker itself, so the visible
    // window plus the marker never exceeds `max_items`.
    let keep = max_items.saturating_sub(1);
    let hidden = view.items.len() - keep;
    let mut items = Vec::with_capacity(max_items);
    items.push(TranscriptItem::Elided { hidden });
    items.extend(view.items.into_iter().skip(hidden));
    TranscriptView { items }
}

/// The documented cap [`cap_transcript_items`] enforces on a live pane's
/// displayed transcript (see [`NativePaneRuntime::refresh_transcript`]).
/// Generous enough that an ordinary session never hits it in practice, but
/// bounded so a very long-running pane's per-tick rebuild cost stays flat
/// rather than growing without limit.
pub const MAX_TRANSCRIPT_ITEMS: usize = 500;

fn conversation_usage(conversation: &ConversationState) -> super::super::event::TranscriptUsage {
    let mut total = super::super::event::TranscriptUsage::default();
    for usage in conversation.usage.values() {
        total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
        total.cache_creation_input_tokens = total
            .cache_creation_input_tokens
            .saturating_add(usage.cache_creation_input_tokens);
        total.cache_read_input_tokens = total
            .cache_read_input_tokens
            .saturating_add(usage.cache_read_input_tokens);
        total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
    }
    total
}

/// Keeps only the replay watermark and immutable identity after the bounded
/// transcript and aggregate usage have been reduced. Older content remains in
/// the durable journal and is replayed only when its sequence changes.
fn compact_retained_conversation(conversation: &mut ConversationState) {
    conversation.messages.clear();
    conversation.usage.clear();
    conversation.tool_calls.clear();
    conversation.executions.clear();
    conversation.task_receipts.clear();
    conversation.checkpoints.clear();
    conversation.ended_reason = None;
}

fn build_tool_call_item(
    state: &ConversationState,
    message_id: String,
    tool_call: &ToolCallId,
) -> TranscriptItem {
    let record = state.tool_calls.get(tool_call);
    let name = record
        .map(|record| record.name.clone())
        .unwrap_or_else(|| "unknown_tool".to_string());
    let arguments_preview = record
        .map(|record| preview_arguments(&record.arguments))
        .unwrap_or_default();
    let execution = latest_execution_for(state, tool_call);
    TranscriptItem::ToolCall {
        tool_call_id: tool_call.as_str().to_string(),
        message_id,
        arguments_preview,
        outcome: classify_outcome(&name, execution),
        name,
    }
}

/// The most recent (highest-sequence) execution record for `tool_call`, if
/// any. A tool call can have more than one execution row across a retry;
/// the latest is authoritative for "what is true now", the same rule
/// `journal::ConversationState::executions` itself keys by `ExecutionId`
/// rather than `ToolCallId` in order to preserve.
fn latest_execution_for<'a>(
    state: &'a ConversationState,
    tool_call: &ToolCallId,
) -> Option<&'a ExecutionRecord> {
    state
        .executions
        .values()
        .filter(|execution| &execution.tool_call == tool_call)
        .max_by_key(|execution| execution.sequence)
}

fn preview_arguments(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn content_text(content: &ContentRef) -> String {
    match content {
        ContentRef::Inline { text } => text.clone(),
        ContentRef::Artifact {
            media_type,
            byte_len,
            ..
        } => format!("[{media_type}, {byte_len} bytes]"),
    }
}

fn classify_outcome(tool_name: &str, execution: Option<&ExecutionRecord>) -> ToolOutcomeView {
    let Some(execution) = execution else {
        return ToolOutcomeView::Pending;
    };
    match execution.state {
        ExecutionState::Prepared => ToolOutcomeView::Pending,
        ExecutionState::Started => ToolOutcomeView::Running,
        ExecutionState::Cancelled => ToolOutcomeView::Cancelled,
        ExecutionState::OutcomeUnknown => ToolOutcomeView::OutcomeUnknown,
        ExecutionState::Failed => {
            let message = execution
                .detail
                .clone()
                .or_else(|| execution.result.as_ref().map(content_text))
                .unwrap_or_else(|| "tool execution failed".to_string());
            ToolOutcomeView::Error { message }
        }
        ExecutionState::Completed => classify_completed(tool_name, execution),
    }
}

fn classify_completed(tool_name: &str, execution: &ExecutionRecord) -> ToolOutcomeView {
    match &execution.result {
        Some(ContentRef::Artifact {
            sha256,
            byte_len,
            media_type,
            ..
        }) => ToolOutcomeView::Artifact {
            sha256: sha256.clone(),
            media_type: media_type.clone(),
            byte_len: *byte_len,
        },
        Some(ContentRef::Inline { text }) => {
            if looks_like_diff(text) {
                ToolOutcomeView::Diff {
                    unified: text.clone(),
                }
            } else if tool_name_suggests_test(tool_name) {
                let (passed, failed) = parse_test_counts(text);
                ToolOutcomeView::TestOutcome {
                    raw: text.clone(),
                    passed,
                    failed,
                }
            } else {
                ToolOutcomeView::Text {
                    content: text.clone(),
                }
            }
        }
        None => ToolOutcomeView::Text {
            content: execution.detail.clone().unwrap_or_default(),
        },
    }
}

/// A unified diff, recognized by its own conventional markers rather than by
/// tool name -- a tool this build has never heard of still renders as a
/// diff if its result looks like one. Requires both a `---`/`+++` file
/// header pair AND a `@@` hunk marker, so an unrelated result that merely
/// contains a stray `@@` (a changelog entry, a code sample) is not
/// misclassified.
fn looks_like_diff(text: &str) -> bool {
    let has_headers = text.lines().any(|line| line.starts_with("--- "))
        && text.lines().any(|line| line.starts_with("+++ "));
    let has_hunk = text
        .lines()
        .any(|line| line.starts_with("@@ ") || line == "@@");
    has_headers && has_hunk
}

fn tool_name_suggests_test(name: &str) -> bool {
    name.to_ascii_lowercase().contains("test")
}

/// Best-effort `"<N> passed"` / `"<N> failed"` extraction from free-form test
/// output (`cargo test`/`nextest`'s own summary line shape, among others).
/// Returns `None` for a count it cannot find rather than guessing -- the raw
/// text is always kept alongside so nothing is lost either way.
fn parse_test_counts(text: &str) -> (Option<u32>, Option<u32>) {
    (scan_count(text, "passed"), scan_count(text, "failed"))
}

fn scan_count(text: &str, word: &str) -> Option<u32> {
    let words: Vec<&str> = text.split_whitespace().collect();
    for (index, candidate) in words.iter().enumerate() {
        let cleaned = candidate.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if !cleaned.eq_ignore_ascii_case(word) || index == 0 {
            continue;
        }
        let number = words[index - 1].trim_matches(|c: char| !c.is_ascii_digit());
        if let Ok(value) = number.parse::<u32>() {
            return Some(value);
        }
    }
    None
}

// =========================================================================
// Status facts and the seven presentation states
// =========================================================================

/// Model/route/runtime/billing facts plus the live runtime state a status
/// line needs, assembled by the (future) pane driver from protocol facts --
/// `journal::RouteIdentity` for model/route, `RuntimeKind` for runtime,
/// `provider::BillingClass` for billing, `native::SessionState`/`TurnState`
/// for the run state. Deliberately a plain data struct rather than a live
/// query: [`classify_status`] is then a pure function any test can call
/// directly, with no session, journal or clock in reach.
#[derive(Clone, Debug, PartialEq)]
pub struct StatusFacts {
    /// `"<vendor>/<id>"`, e.g. `"anthropic/claude-opus-4"`.
    pub model: String,
    pub route: String,
    pub runtime: String,
    /// `"api"` or `"subscription"` (`provider::BillingClass`'s own display).
    pub billing: String,
    pub session_state: NativeSessionState,
    pub turn_state: Option<NativeTurnState>,
    /// An approval (or other policy gate) is outstanding. Deliberately
    /// separate from `session_state`/`turn_state`: an approval can be
    /// pending while the turn is otherwise `ExecutingTools`, and it
    /// outranks every other classification (see [`classify_status`]) because
    /// it is the one state that changes what the composer is allowed to do
    /// with Enter -- see [`classify_submit_intent`].
    pub blocked: bool,
    /// The session reached a terminal state while the operator was not
    /// looking at the bottom of the transcript (`!ScrollState::follow`).
    /// Presentation-layer bookkeeping, not a journal fact -- see
    /// [`NativePresentation::note_terminal_reached`].
    pub unread_result: bool,
    /// PR #531 review finding 5: a non-fatal condition the worker thread
    /// wants the operator to see (today, only a standing-context compile
    /// failure -- `runtime::native::InteractiveProgress::Notice`) rather
    /// than swallowing it silently. Rendered on the status line by
    /// [`status_line_text`]; `None` on every path that constructs
    /// `StatusFacts` without a live [`NativePaneRuntime`] behind it.
    pub notice: Option<String>,
    /// Operator direction (PR #531 follow-up): the spinner/verb/elapsed/
    /// token/interrupt-hint line shown while a turn runs -- see
    /// [`activity_line_text`]. `None` while idle, and on every path that
    /// constructs `StatusFacts` without a live [`NativePaneRuntime`] behind
    /// it.
    pub activity: Option<ActivityFacts>,
    /// The repository this session is running in -- part of the bottom
    /// status line (operator direction, PR #531 follow-up).
    pub cwd: String,
    /// The checked out branch, read once from `.git/HEAD` at spawn time --
    /// `None` when `repo` is not a git checkout, is in a detached-HEAD
    /// state, or on every path that constructs `StatusFacts` without a live
    /// `NativePaneRuntime` behind it. "Unknown, not a guess", the same
    /// convention every other status fact here follows.
    pub git_branch: Option<String>,
    /// Percentage of the model's declared context window still free,
    /// estimated from the conversation's own recorded token usage --
    /// `None` when the model's context window is not declared at all.
    /// This is an ESTIMATE against raw usage, not the compaction budget's
    /// own accounting (which lives inside the worker thread's
    /// `NativeSessionConfig`, not read back by this pane) -- see the design
    /// note.
    pub context_left_pct: Option<u8>,
}

/// The seven states item 4 names, plus the natural eighth: "completed, and
/// the operator has already seen it" (`unread_result == false` on a terminal
/// state). Exhaustively covered by [`classify_status`]'s own tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresentationStatus {
    Generating,
    Executing,
    Waiting,
    Blocked,
    Cancelled,
    Failed,
    CompletedUnread,
    Completed,
}

/// Classifies `facts` into one [`PresentationStatus`]. `blocked` outranks
/// every runtime state (an approval can be pending mid-tool-execution and
/// the operator needs to see "blocked", not "executing"); everything else
/// follows `session_state`, refined by `turn_state` while the session is
/// `Running`.
pub fn classify_status(facts: &StatusFacts) -> PresentationStatus {
    if facts.blocked {
        return PresentationStatus::Blocked;
    }
    match facts.session_state {
        NativeSessionState::Interrupted => PresentationStatus::Cancelled,
        NativeSessionState::Failed => PresentationStatus::Failed,
        NativeSessionState::Completed => completed_status(facts.unread_result),
        NativeSessionState::Idle => PresentationStatus::Waiting,
        NativeSessionState::Running => match facts.turn_state {
            Some(NativeTurnState::Requesting) => PresentationStatus::Generating,
            Some(NativeTurnState::ExecutingTools) => PresentationStatus::Executing,
            Some(NativeTurnState::Pending) | Some(NativeTurnState::Continuing) | None => {
                PresentationStatus::Waiting
            }
            Some(NativeTurnState::Completed) => completed_status(facts.unread_result),
            Some(NativeTurnState::Interrupted) => PresentationStatus::Cancelled,
            Some(NativeTurnState::Failed) => PresentationStatus::Failed,
        },
    }
}

fn completed_status(unread: bool) -> PresentationStatus {
    if unread {
        PresentationStatus::CompletedUnread
    } else {
        PresentationStatus::Completed
    }
}

pub fn status_glyph(status: PresentationStatus) -> &'static str {
    match status {
        PresentationStatus::Generating => "\u{25cf}",      // ●
        PresentationStatus::Executing => "\u{27f3}",       // ⟳
        PresentationStatus::Waiting => "\u{2026}",         // …
        PresentationStatus::Blocked => "\u{25b2}",         // ▲
        PresentationStatus::Cancelled => "\u{2298}",       // ⊘
        PresentationStatus::Failed => "\u{2717}",          // ✗
        PresentationStatus::CompletedUnread => "\u{25c6}", // ◆
        PresentationStatus::Completed => "\u{2713}",       // ✓
    }
}

pub fn status_label(status: PresentationStatus) -> &'static str {
    match status {
        PresentationStatus::Generating => "generating",
        PresentationStatus::Executing => "executing",
        PresentationStatus::Waiting => "waiting",
        PresentationStatus::Blocked => "blocked",
        PresentationStatus::Cancelled => "cancelled",
        PresentationStatus::Failed => "failed",
        PresentationStatus::CompletedUnread => "completed \u{2014} unread",
        PresentationStatus::Completed => "completed",
    }
}

pub fn status_tone(status: PresentationStatus) -> Tone {
    match status {
        PresentationStatus::Generating => Tone::Accent,
        PresentationStatus::Executing => Tone::Warn,
        PresentationStatus::Waiting => Tone::Muted,
        PresentationStatus::Blocked => Tone::Err,
        PresentationStatus::Cancelled => Tone::Muted,
        PresentationStatus::Failed => Tone::Err,
        PresentationStatus::CompletedUnread => Tone::Warn,
        PresentationStatus::Completed => Tone::Ok,
    }
}

/// Where a composer submission goes, mapping onto N09's queue/steer/
/// interrupt semantics at the presentation boundary. Never a function of the
/// composer's own state -- only of `StatusFacts` -- so "what does Enter do
/// right now" is answerable without inspecting the draft at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitIntent {
    /// No turn is in flight: send immediately as a fresh `Submit`.
    Immediate,
    /// A turn is in flight: send as a `Steer` (N09), interleaved into the
    /// running turn rather than starting a new one.
    Steer,
    /// Blocked (an approval or other gate is outstanding) or otherwise not
    /// safe to send: hold as queued input. **Never** routed to the approval
    /// control -- an approval decision has its own explicit surface (the
    /// dashboard's Approval dialog), and a composer submission while
    /// blocked is queued precisely so it can never be mistaken for one.
    Queue,
}

pub fn classify_submit_intent(facts: &StatusFacts) -> SubmitIntent {
    if facts.blocked {
        return SubmitIntent::Queue;
    }
    match facts.session_state {
        NativeSessionState::Idle => SubmitIntent::Immediate,
        NativeSessionState::Running => SubmitIntent::Steer,
        NativeSessionState::Interrupted
        | NativeSessionState::Completed
        | NativeSessionState::Failed => SubmitIntent::Queue,
    }
}

// =========================================================================
// Composer: multiline draft, history, paste, @path references
// =========================================================================

/// One piece of input held because it could not be sent immediately
/// ([`SubmitIntent::Queue`]) -- persisted (see [`persist_draft`]) so it
/// survives a reconnect/resume rather than being silently dropped.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QueuedInput {
    pub text: String,
    pub steering: bool,
    pub queued_at_ms: u64,
}

/// The composer's own state: the in-progress multiline draft, submit
/// history and anything queued. Never touches the runtime directly --
/// [`apply_composer_action`] only ever mutates this struct and reports what
/// happened via [`ComposerOutcome`], leaving the caller to decide (via
/// [`classify_submit_intent`]) what a submit actually does.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ComposerState {
    pub draft: String,
    /// Byte offset into `draft`, always on a char boundary.
    pub cursor: usize,
    pub history: Vec<String>,
    /// `Some(index)` while browsing `history` (0 = most recent); `None`
    /// while editing the live draft.
    pub history_cursor: Option<usize>,
    /// The draft being edited before history browsing started, restored
    /// when `HistoryDown` walks past the newest history entry.
    history_stash: Option<String>,
    pub queued: Vec<QueuedInput>,
}

/// One editing/navigation action the composer understands. Constructed from
/// a raw key by [`key_to_action`] (the documented key contract) or directly
/// by a paste path ([`InsertText`](ComposerAction::InsertText)).
#[derive(Clone, Debug, PartialEq)]
pub enum ComposerAction {
    Insert(char),
    /// A pasted (or coalesced-fallback) block, inserted verbatim as one
    /// unit -- never as one `Insert`/`Newline` per character, so a large
    /// paste never fires history navigation or submit mid-insert.
    InsertText(String),
    Newline,
    Backspace,
    DeleteForward,
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    Home,
    End,
    /// Explicit history navigation, independent of cursor position.
    /// `key_to_action`'s own contract never emits these -- `MoveUp`/
    /// `MoveDown` already decide history-vs-cursor from where the cursor
    /// is (see their own handling in `apply_composer_action`) -- so no
    /// caller constructs these today; kept as an explicit action a future
    /// dedicated key binding (or a non-keyboard UI, e.g. a history picker)
    /// can reach without duplicating that cursor-position logic.
    #[allow(dead_code)]
    HistoryUp,
    #[allow(dead_code)]
    HistoryDown,
    Submit,
    ClearLine,
}

/// What [`apply_composer_action`] did, for a caller that needs to know
/// whether a submit actually happened (and what text it carries) without
/// re-inspecting `draft` (already cleared by the time it returns).
#[derive(Clone, Debug, PartialEq)]
pub enum ComposerOutcome {
    Changed,
    Submitted(String),
    Unchanged,
}

/// **The composer's documented key contract:**
/// - `Enter` (no modifiers) submits.
/// - `Shift+Enter` or `Alt+Enter` inserts a newline.
/// - `Backspace`/`Delete` delete a character; `Ctrl+U` clears the draft.
/// - Arrow keys move the cursor; `Home`/`End` move to the start/end of the
///   current logical line.
/// - `Up` at the first logical line (no `\n` before the cursor) and `Down`
///   at the last logical line (no `\n` after the cursor) walk submit
///   history instead of moving the cursor -- so history navigation never
///   fights cursor movement in the middle of a multi-line draft.
///
/// Returns `None` for a key this contract does not assign a composer
/// action to (function keys, unmodified control chars other than the ones
/// above, etc.) -- the caller's own key dispatch decides what, if anything,
/// such a key does elsewhere (pane switching, palette, ...).
pub fn key_to_action(key: KeyEvent) -> Option<ComposerAction> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Enter if shift || alt => Some(ComposerAction::Newline),
        KeyCode::Enter => Some(ComposerAction::Submit),
        KeyCode::Char('u') if ctrl => Some(ComposerAction::ClearLine),
        KeyCode::Char(c) if !ctrl => Some(ComposerAction::Insert(c)),
        KeyCode::Backspace => Some(ComposerAction::Backspace),
        KeyCode::Delete => Some(ComposerAction::DeleteForward),
        KeyCode::Left => Some(ComposerAction::MoveLeft),
        KeyCode::Right => Some(ComposerAction::MoveRight),
        KeyCode::Up => Some(ComposerAction::MoveUp),
        KeyCode::Down => Some(ComposerAction::MoveDown),
        KeyCode::Home => Some(ComposerAction::Home),
        KeyCode::End => Some(ComposerAction::End),
        _ => None,
    }
}

/// Byte offset of the start of the logical line `cursor` is in (the byte
/// right after the nearest preceding `\n`, or `0`).
fn line_start(draft: &str, cursor: usize) -> usize {
    draft[..cursor].rfind('\n').map_or(0, |idx| idx + 1)
}

/// Byte offset of the end of the logical line `cursor` is in (the nearest
/// following `\n`, or `draft.len()`).
fn line_end(draft: &str, cursor: usize) -> usize {
    draft[cursor..]
        .find('\n')
        .map_or(draft.len(), |idx| cursor + idx)
}

fn is_first_line(draft: &str, cursor: usize) -> bool {
    !draft[..cursor].contains('\n')
}

fn is_last_line(draft: &str, cursor: usize) -> bool {
    !draft[cursor..].contains('\n')
}

fn prev_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx == 0 {
        return 0;
    }
    idx -= 1;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn next_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    idx += 1;
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

/// Applies one action to `state`, in place. Pasted/large-block text
/// (`InsertText`) has any `\r\n`/`\r` normalized to `\n` first -- Windows
/// clipboard/terminal paste routinely carries CRLF, and an un-normalized
/// `\r` would otherwise render as a stray control character rather than a
/// line break (`resolve_file_refs`/`wrap_line` both assume `\n`-only line
/// endings, matching every other multiline draft in this module).
pub fn apply_composer_action(state: &mut ComposerState, action: ComposerAction) -> ComposerOutcome {
    match action {
        ComposerAction::Insert(c) => {
            leave_history_browsing(state);
            state.draft.insert(state.cursor, c);
            state.cursor += c.len_utf8();
            ComposerOutcome::Changed
        }
        ComposerAction::InsertText(text) => {
            leave_history_browsing(state);
            let normalized = normalize_line_endings(&text);
            state.draft.insert_str(state.cursor, &normalized);
            state.cursor += normalized.len();
            ComposerOutcome::Changed
        }
        ComposerAction::Newline => {
            leave_history_browsing(state);
            state.draft.insert(state.cursor, '\n');
            state.cursor += 1;
            ComposerOutcome::Changed
        }
        ComposerAction::Backspace => {
            leave_history_browsing(state);
            if state.cursor == 0 {
                return ComposerOutcome::Unchanged;
            }
            let start = prev_char_boundary(&state.draft, state.cursor);
            state.draft.replace_range(start..state.cursor, "");
            state.cursor = start;
            ComposerOutcome::Changed
        }
        ComposerAction::DeleteForward => {
            leave_history_browsing(state);
            if state.cursor >= state.draft.len() {
                return ComposerOutcome::Unchanged;
            }
            let end = next_char_boundary(&state.draft, state.cursor);
            state.draft.replace_range(state.cursor..end, "");
            ComposerOutcome::Changed
        }
        ComposerAction::MoveLeft => {
            state.cursor = prev_char_boundary(&state.draft, state.cursor);
            ComposerOutcome::Unchanged
        }
        ComposerAction::MoveRight => {
            state.cursor = next_char_boundary(&state.draft, state.cursor);
            ComposerOutcome::Unchanged
        }
        ComposerAction::Home => {
            state.cursor = line_start(&state.draft, state.cursor);
            ComposerOutcome::Unchanged
        }
        ComposerAction::End => {
            state.cursor = line_end(&state.draft, state.cursor);
            ComposerOutcome::Unchanged
        }
        ComposerAction::MoveUp => {
            if is_first_line(&state.draft, state.cursor) {
                history_up(state);
            } else {
                move_vertical(state, -1);
            }
            ComposerOutcome::Changed
        }
        ComposerAction::MoveDown => {
            if is_last_line(&state.draft, state.cursor) {
                history_down(state);
            } else {
                move_vertical(state, 1);
            }
            ComposerOutcome::Changed
        }
        ComposerAction::HistoryUp => {
            history_up(state);
            ComposerOutcome::Changed
        }
        ComposerAction::HistoryDown => {
            history_down(state);
            ComposerOutcome::Changed
        }
        ComposerAction::ClearLine => {
            leave_history_browsing(state);
            state.draft.clear();
            state.cursor = 0;
            ComposerOutcome::Changed
        }
        ComposerAction::Submit => {
            leave_history_browsing(state);
            if state.draft.is_empty() {
                return ComposerOutcome::Unchanged;
            }
            let text = std::mem::take(&mut state.draft);
            state.cursor = 0;
            state.history.insert(0, text.clone());
            ComposerOutcome::Submitted(text)
        }
    }
}

fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Operator direction (PR #531 follow-up): `Ctrl+C` no longer interrupts a
/// turn by itself -- `Esc` owns that now (see `run_native_dashboard`'s own
/// key contract). A single `Ctrl+C` only arms a quit confirmation; the pane
/// quits only when a SECOND `Ctrl+C` lands within `window` of the first.
/// Pure so the arming/window arithmetic is unit-testable without a real
/// terminal loop -- `run_native_dashboard` is the only caller, tracking
/// `last_press` as its own local `Option<Instant>`, replaced with `Some(now)`
/// on every `Ctrl+C` that does not itself confirm a quit and cleared by any
/// other key.
fn ctrl_c_confirms_quit(
    last_press: Option<std::time::Instant>,
    now: std::time::Instant,
    window: std::time::Duration,
) -> bool {
    last_press.is_some_and(|previous| now.saturating_duration_since(previous) <= window)
}

/// How long a first `Ctrl+C` stays armed for [`ctrl_c_confirms_quit`].
const CTRL_C_QUIT_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// `Ctrl+R`'s (and the composer's own `e`/`Enter`-while-`Transcript`-
/// focused) shared action: toggle the most recently rendered tool call's
/// expanded state. A no-op when the transcript has no tool call at all.
fn toggle_most_recent_tool_call(pane: &mut NativePaneRuntime) {
    let key = pane
        .view()
        .0
        .items
        .iter()
        .rev()
        .find_map(|item| item.expand_key())
        .map(str::to_string);
    if let Some(key) = key {
        pane.presentation_mut().toggle_expanded(&key);
    }
}

/// Operator direction (PR #531 follow-up): a submission that IS a
/// recognised slash command, handled entirely here rather than sent as a
/// turn. `/clear` has a real effect (drops the queued backlog); `/help` is
/// informational; `/compact` is an honest inert stub -- wiring it to the
/// real compaction envelope needs facts (`NativeSessionConfig`'s own
/// budget) the pane does not hold today, see the design note. `/status` and
/// `/context`/`/instructions` need live facts (`StatusFacts`/`context_view_
/// facts`) this pure function cannot produce, so `NativePaneRuntime::
/// handle_composer_action` handles both directly instead of routing through
/// here.
///
/// Returns `Some(notice)` for a recognised command (`notice` may be empty,
/// e.g. `/clear`, which has nothing to report), `None` for anything else --
/// including a `/`-prefixed line this list does not recognise, which falls
/// through to the normal submit path as ordinary text rather than being
/// silently swallowed.
fn apply_slash_command(presentation: &mut NativePresentation, text: &str) -> Option<String> {
    match text.trim() {
        "/clear" => {
            presentation.composer.queued.clear();
            Some(String::new())
        }
        "/help" => Some(
            "commands: /clear /compact /context /status \u{b7} keys: Enter submit, Esc \
             interrupt, Ctrl+C Ctrl+C quit, Shift+Tab cycle mode"
                .to_string(),
        ),
        "/compact" => {
            Some("/compact is not yet wired to the native pane's compaction envelope".to_string())
        }
        // Issue #538 (chunk C): `/context`/`/instructions` need this pane's
        // own live journal, so -- same shape of exception as `/status` --
        // `NativePaneRuntime::handle_composer_action` handles them directly
        // (`context_view_facts`) rather than through this pure helper.
        _ => None,
    }
}

fn leave_history_browsing(state: &mut ComposerState) {
    state.history_cursor = None;
    state.history_stash = None;
}

fn move_vertical(state: &mut ComposerState, delta: i32) {
    let col = state.cursor - line_start(&state.draft, state.cursor);
    let target_line_start = if delta < 0 {
        let this_start = line_start(&state.draft, state.cursor);
        if this_start == 0 {
            return;
        }
        line_start(&state.draft, this_start - 1)
    } else {
        let this_end = line_end(&state.draft, state.cursor);
        if this_end >= state.draft.len() {
            return;
        }
        this_end + 1
    };
    let target_line_end = line_end(&state.draft, target_line_start);
    state.cursor = (target_line_start + col).min(target_line_end);
    while !state.draft.is_char_boundary(state.cursor) {
        state.cursor -= 1;
    }
}

/// Walks one step further back into history (index 0 = most recent),
/// stashing the live draft the first time browsing starts so `HistoryDown`
/// can restore it once the operator walks back past the newest entry.
fn history_up(state: &mut ComposerState) {
    if state.history.is_empty() {
        return;
    }
    let next = match state.history_cursor {
        None => {
            state.history_stash = Some(state.draft.clone());
            0
        }
        Some(index) => (index + 1).min(state.history.len() - 1),
    };
    state.history_cursor = Some(next);
    state.draft = state.history[next].clone();
    state.cursor = state.draft.len();
}

fn history_down(state: &mut ComposerState) {
    let Some(index) = state.history_cursor else {
        return;
    };
    if index == 0 {
        state.history_cursor = None;
        state.draft = state.history_stash.take().unwrap_or_default();
    } else {
        let next = index - 1;
        state.history_cursor = Some(next);
        state.draft = state.history[next].clone();
    }
    state.cursor = state.draft.len();
}

/// One `@token` reference found in a draft, resolved against `workdir`.
/// `exists` is a plain filesystem check (not a repository-tracked check --
/// an untracked new file is still a legitimate reference), so a caller
/// deciding whether to *attach* the file still owes its own read/size
/// policy; this is purely "does this token look like a real path".
///
/// Tested (`resolve_file_refs_finds_existing_and_missing_paths`,
/// `resolve_file_refs_finds_a_unicode_path`) but not yet called from
/// `run_native_dashboard`'s own minimal loop -- rendering a live `@`-hint
/// line needs `composer_lines`/`render_native_pane` to take a workdir,
/// which is scoped out of this round; see the design note.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
pub struct FileRef {
    /// The raw token including the leading `@`.
    pub token: String,
    pub path: String,
    pub exists: bool,
    pub start: usize,
    pub end: usize,
}

/// Scans `text` for `@path` tokens (an `@` followed by non-whitespace) and
/// resolves each one against `workdir`. Pure aside from the filesystem
/// `exists` check, which is why this is a plain function rather than part
/// of [`apply_composer_action`] -- a caller re-runs it on demand (e.g. on
/// every draft change) rather than this module owning a debounce policy.
///
/// PR #531 review finding 2: `workdir.join(path).exists()` alone answers
/// "does something exist at this joined path", never "does it stay inside
/// `workdir`" -- `@../../secret` joins and exists just fine while pointing
/// somewhere the caller never meant to expose. Both sides are canonicalized
/// (resolving `..`, `.` and symlinks) and the candidate must fall under the
/// canonical workdir; anything that escapes it -- or that cannot be
/// canonicalized at all, e.g. because it does not exist -- reads as
/// `exists: false` rather than being trusted.
#[allow(dead_code)] // see `FileRef`'s own doc comment
pub fn resolve_file_refs(text: &str, workdir: &Path) -> Vec<FileRef> {
    let workdir_canonical = std::fs::canonicalize(workdir).ok();
    let mut refs = Vec::new();
    let mut idx = 0usize;
    while let Some(rel) = text[idx..].find('@') {
        let start = idx + rel;
        let mut end = start + 1;
        while end < text.len() {
            let ch = text[end..].chars().next().expect("end is a char boundary");
            if ch.is_whitespace() {
                break;
            }
            end += ch.len_utf8();
        }
        if end > start + 1 {
            let path = text[start + 1..end].to_string();
            let exists = workdir_canonical
                .as_deref()
                .map(|root| path_resolves_under(root, &workdir.join(&path)))
                .unwrap_or(false);
            refs.push(FileRef {
                token: text[start..end].to_string(),
                path,
                exists,
                start,
                end,
            });
        }
        idx = end.max(start + 1);
    }
    refs
}

/// Whether `candidate` canonicalizes to a path under the already-canonical
/// `root`. A candidate that fails to canonicalize (missing, a dangling
/// symlink, a permissions error) is never treated as inside `root` --
/// refusing is the safe default, not a guess.
fn path_resolves_under(root: &Path, candidate: &Path) -> bool {
    std::fs::canonicalize(candidate)
        .map(|resolved| resolved.starts_with(root))
        .unwrap_or(false)
}

/// Groups a sequence of input chunks (each with the [`Duration`] elapsed
/// since the previous one landed) into paste blocks: consecutive chunks
/// less than `gap` apart are joined into one block. This is the
/// fallback path for a terminal that never sends a single bracketed-paste
/// event -- crossterm's `Event::Paste` is used directly as one
/// `ComposerAction::InsertText` when the terminal supports it, and never
/// needs this coalescing at all. Pure and deterministic given the recorded
/// gaps, so a large paste's actual arrival timing can be fixture data
/// rather than a real terminal.
/// Tested (`coalesce_paste_chunks_joins_only_chunks_within_the_gap`) but not
/// yet called: `run_native_dashboard`'s loop only ever sees `Event::Paste`
/// (bracketed paste, the path this function's own doc comment says makes it
/// unnecessary) or single key presses -- no terminal-capability probe/
/// fallback path exists yet to decide when a rapid run of `Event::Key`
/// presses should be coalesced instead.
#[allow(dead_code)]
pub fn coalesce_paste_chunks(chunks: &[(String, Duration)], gap: Duration) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (text, elapsed) in chunks {
        if let Some(last) = out.last_mut()
            && *elapsed < gap
        {
            last.push_str(text);
            continue;
        }
        out.push(text.clone());
    }
    out
}

// =========================================================================
// Scroll / follow mode
// =========================================================================

/// Follow-mode scroll position, measured in transcript **items** (not
/// rendered lines) counted back from the newest item -- deliberately
/// item-based rather than line-based so a resize (which changes how many
/// wrapped lines an item occupies at the new width) never perturbs which
/// items are on screen; only the final line-budget slice
/// ([`visible_lines`]) is width-dependent, and it is recomputed fresh every
/// render.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollState {
    pub items_back: usize,
    pub follow: bool,
}

impl Default for ScrollState {
    fn default() -> Self {
        Self {
            items_back: 0,
            follow: true,
        }
    }
}

impl ScrollState {
    /// Scrolls up (toward older items) by `n` items, clamped to the oldest
    /// item, and disengages follow mode -- the one rule item 5/the scroll
    /// acceptance criterion asks for: a tool update must never force-scroll
    /// an operator who has scrolled up.
    pub fn scroll_up(&mut self, n: usize, total_items: usize) {
        let max_back = total_items.saturating_sub(1);
        self.items_back = self.items_back.saturating_add(n).min(max_back);
        self.follow = false;
    }

    /// Scrolls down (toward newer items) by `n` items, re-engaging follow
    /// mode once the bottom is reached.
    pub fn scroll_down(&mut self, n: usize) {
        self.items_back = self.items_back.saturating_sub(n);
        if self.items_back == 0 {
            self.follow = true;
        }
    }

    pub fn jump_to_bottom(&mut self) {
        self.items_back = 0;
        self.follow = true;
    }

    /// Called whenever the transcript grows by `new_items`. While following,
    /// this is a no-op (the renderer always shows the tail). While scrolled
    /// up, `items_back` grows by the same amount, so the same absolute
    /// window of items stays visible instead of sliding as new items land
    /// underneath it.
    pub fn on_items_appended(&mut self, new_items: usize) {
        if !self.follow {
            self.items_back += new_items;
        }
    }
}

// =========================================================================
// Presentation state
// =========================================================================

/// Which region of the pane has keyboard focus. Only meaningful for a
/// `Native` pane -- see this module's own doc comment on why a wrapped
/// pane's input routing is untouched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneFocus {
    Transcript,
    Composer,
}

/// Operator direction (PR #531 follow-up): a `Shift+Tab`-cycled composer
/// mode label, the same idea Claude Code's own CLI shows above its prompt.
/// **Decorative only, today**: no submit path reads this back to change
/// approval or tool-write behaviour -- an `AcceptEdits`/`Plan` mode that
/// actually gated the execution broker would be a policy change at the
/// enforcement layer, out of scope for a rendering/key-contract pass (see
/// the design note). Shown on the composer's own hint line so the key
/// binding is visibly real even while the behaviour it will eventually
/// drive is not wired yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ComposerMode {
    #[default]
    Default,
    AcceptEdits,
    Plan,
}

impl ComposerMode {
    /// `Shift+Tab`'s own action: the next mode in the cycle.
    pub fn next(self) -> Self {
        match self {
            ComposerMode::Default => ComposerMode::AcceptEdits,
            ComposerMode::AcceptEdits => ComposerMode::Plan,
            ComposerMode::Plan => ComposerMode::Default,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ComposerMode::Default => "default mode",
            ComposerMode::AcceptEdits => "accept-edits mode",
            ComposerMode::Plan => "plan mode",
        }
    }
}

/// Everything a native pane needs to redraw itself identically across a
/// resize or a streaming update, kept separate from the session/runtime
/// object that actually owns the conversation (per this module's own scope
/// note). Only [`PersistedDraft`] (below) is ever written to disk.
#[derive(Clone, Debug, PartialEq)]
pub struct NativePresentation {
    pub scroll: ScrollState,
    pub focus: PaneFocus,
    /// A selected range of transcript item indices (`start..=end`,
    /// inclusive), for future copy support. Untouched by scrolling/resize --
    /// only an explicit selection action changes it.
    pub selection: Option<(usize, usize)>,
    pub expanded: BTreeSet<String>,
    pub composer: ComposerState,
    pub unread: bool,
    /// `Shift+Tab`-cycled, decorative only -- see [`ComposerMode`]'s own
    /// doc comment.
    pub mode: ComposerMode,
    /// Issue #490: the worktree the `@` picker is allowed to offer paths
    /// from. `None` disables the picker entirely rather than falling back to
    /// the process's current directory -- a pane with no declared worktree
    /// must not be able to complete a path outside one.
    pub workdir: Option<PathBuf>,
    /// Review finding 3 (PR #544): true when this pane attached to a
    /// runtime-owned session without the controller seat -- either
    /// `RuntimeLink::attach` was refused outright, or it succeeded but
    /// another client already holds control. A read-only pane offers no
    /// send/steer/approve at all rather than attempting one that the
    /// server's own controller check would refuse anyway; the composer's
    /// hint line says so. Always `false` for an in-process pane.
    pub observer: bool,
}

impl Default for NativePresentation {
    fn default() -> Self {
        Self {
            scroll: ScrollState::default(),
            focus: PaneFocus::Composer,
            selection: None,
            expanded: BTreeSet::new(),
            composer: ComposerState::default(),
            unread: false,
            mode: ComposerMode::default(),
            workdir: None,
            observer: false,
        }
    }
}

impl NativePresentation {
    pub fn toggle_expanded(&mut self, key: &str) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_string());
        }
    }

    /// Tested (`selection_and_expanded_survive_a_resize_no_op`) but not yet
    /// called from `run_native_dashboard`'s loop -- no copy mechanism reads
    /// `selection` yet; see the design note.
    #[allow(dead_code)]
    pub fn set_selection(&mut self, start: usize, end: usize) {
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        self.selection = Some((start, end));
    }

    #[allow(dead_code)] // see `set_selection`'s own doc comment
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Marks the transcript as carrying an unread completed result if the
    /// operator is not currently following the tail. Called once, by the
    /// (future) pane driver, when a session's `PresentationStatus`
    /// transitions into `Completed`/`CompletedUnread` territory --
    /// deliberately idempotent (calling it again while already unread, or
    /// while following, changes nothing) so the driver never has to track
    /// whether it already fired for this completion.
    pub fn note_terminal_reached(&mut self) {
        if !self.scroll.follow {
            self.unread = true;
        }
    }

    /// Clears the unread flag -- called once the operator scrolls to the
    /// bottom (or otherwise acknowledges the result).
    pub fn mark_seen(&mut self) {
        self.unread = false;
    }
}

/// The subset of [`NativePresentation`] worth surviving a process restart:
/// the in-progress draft and anything queued but not yet sent. Scroll,
/// selection, focus and expanded tool calls are deliberately NOT persisted
/// -- they are meaningless once the process that computed them against a
/// specific viewport is gone, and re-deriving them fresh (follow mode on,
/// nothing expanded) on reconnect is the same "no false memory" rule
/// `roster::RosterPane` already follows for a restored pane's transient UI
/// state.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PersistedDraft {
    #[serde(default)]
    pub draft: String,
    #[serde(default)]
    pub queued: Vec<QueuedInput>,
}

impl PersistedDraft {
    pub fn from_composer(composer: &ComposerState) -> Self {
        Self {
            draft: composer.draft.clone(),
            queued: composer.queued.clone(),
        }
    }

    /// Applies this snapshot onto `composer`, cursor placed at the end of
    /// the restored draft.
    pub fn restore_onto(&self, composer: &mut ComposerState) {
        composer.draft = self.draft.clone();
        composer.cursor = composer.draft.len();
        composer.queued = self.queued.clone();
    }
}

fn draft_path(state: &super::super::state::StateDir, session: &str) -> std::path::PathBuf {
    state.native_panes().join(format!("{session}.json"))
}

/// Reads the persisted draft for `session`, or a fresh default when the
/// file is missing or fails to parse -- the same tolerant-of-absence-and-
/// corruption contract `attention::load` already uses for its own per-
/// session file.
pub fn load_draft(state: &super::super::state::StateDir, session: &str) -> PersistedDraft {
    std::fs::read_to_string(draft_path(state, session))
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

/// Best-effort write of `draft` for `session`. A write failure is silent
/// (matching every other piece of state-dir housekeeping in this codebase --
/// see `attention::record`'s own doc comment): a native pane that cannot
/// persist its draft still works for the rest of the live process, it just
/// risks losing the draft across a restart, which is strictly better than a
/// panic or an error the caller has no useful way to act on.
pub fn persist_draft(state: &super::super::state::StateDir, session: &str, draft: &PersistedDraft) {
    let dir = state.native_panes();
    if super::super::state::create_private_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(body) = serde_json::to_string(draft) {
        let _ = super::super::state::write_private(&draft_path(state, session), &body);
    }
}

// =========================================================================
// Rendering: styled lines shared by the ratatui and plain-text renderers
// =========================================================================

/// One span of text carrying a single semantic [`Tone`] -- the same
/// vocabulary `crate::style::paint` already uses for plain-text/CLI output,
/// reused here so the ratatui and plain-text renderers below can never
/// disagree about what a piece of text *means*, only about how that meaning
/// is drawn.
#[derive(Clone, Debug, PartialEq)]
pub struct StyledSpan {
    pub text: String,
    pub tone: Tone,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct StyledLine(pub Vec<StyledSpan>);

impl StyledLine {
    pub fn plain(text: impl Into<String>) -> Self {
        Self(vec![StyledSpan {
            text: text.into(),
            tone: Tone::Plain,
        }])
    }

    pub fn toned(text: impl Into<String>, tone: Tone) -> Self {
        Self(vec![StyledSpan {
            text: text.into(),
            tone,
        }])
    }

    pub fn display_width(&self) -> usize {
        self.0
            .iter()
            .map(|span| style::display_width(&span.text))
            .sum()
    }

    pub fn to_plain_string(&self) -> String {
        self.0.iter().map(|span| span.text.as_str()).collect()
    }
}

/// Splits `line` into alternating plain/inline-code spans on backtick
/// delimiters: every even-indexed segment (0, 2, 4, ...) of `line.split('`')`
/// is outside any pair of backticks (`Tone::Plain`), every odd-indexed one is
/// inside one (`Tone::Accent`). An unmatched trailing backtick simply means
/// the last segment reads as code -- content is never dropped, only
/// mis-toned in that one edge case.
fn inline_spans(line: &str) -> Vec<StyledSpan> {
    let mut spans: Vec<StyledSpan> = line
        .split('`')
        .enumerate()
        .filter(|(_, part)| !part.is_empty())
        .map(|(index, part)| StyledSpan {
            text: part.to_string(),
            tone: if index % 2 == 1 {
                Tone::Accent
            } else {
                Tone::Plain
            },
        })
        .collect();
    if spans.is_empty() {
        spans.push(StyledSpan {
            text: String::new(),
            tone: Tone::Plain,
        });
    }
    spans
}

fn heading_text(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    rest.strip_prefix(' ')
        .or(if rest.is_empty() { Some(rest) } else { None })
}

fn list_item_text(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
    {
        return Some(rest);
    }
    // "N. rest" for one or more ASCII digits.
    let digits: usize = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        let rest = &trimmed[digits..];
        if let Some(after) = rest.strip_prefix(". ") {
            return Some(after);
        }
    }
    None
}

/// A minimal Markdown-ish renderer: headings, `-`/`*`/numbered lists, fenced
/// code blocks and inline code. Deliberately not a full CommonMark parser --
/// no dependency does this today (see this module's own doc comment /
/// `docs/design/2026-09-13-native-pane.md` for why none was added) -- so
/// this is a line classifier, not a tree parser: nested structure (a list
/// inside a blockquote, etc.) renders as its outermost recognized line kind.
pub fn markdown_lines(text: &str) -> Vec<StyledLine> {
    let mut out = Vec::new();
    let mut in_code = false;
    for raw in text.split('\n') {
        if raw.trim_start().starts_with("```") {
            in_code = !in_code;
            out.push(StyledLine::toned(raw.to_string(), Tone::Muted));
            continue;
        }
        if in_code {
            out.push(StyledLine::toned(raw.to_string(), Tone::Muted));
            continue;
        }
        if let Some(rest) = heading_text(raw) {
            out.push(StyledLine::toned(rest.to_string(), Tone::Emphasis));
            continue;
        }
        if let Some(rest) = list_item_text(raw) {
            let mut spans = vec![StyledSpan {
                text: "\u{2022} ".to_string(),
                tone: Tone::Muted,
            }];
            spans.extend(inline_spans(rest));
            out.push(StyledLine(spans));
            continue;
        }
        out.push(StyledLine(inline_spans(raw)));
    }
    out
}

/// The display-column width of a single grapheme-agnostic character, never
/// splitting a codepoint -- the same convention `crate::style` already
/// documents (scalar-value width, not grapheme clusters).
fn char_cols(c: char) -> usize {
    unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Width-aware greedy word wrap of one [`StyledLine`] into however many
/// display lines are needed to keep every line's display width `<= width`
/// (using [`style::display_width`]'s own column counting, so CJK/emoji are
/// measured correctly). A single token wider than `width` is hard-split at
/// a column boundary (never inside a codepoint) rather than overflowing --
/// the same situation a long unbroken path or URL in a code block produces.
/// `width == 0` returns one empty line rather than looping forever.
pub fn wrap_line(line: &StyledLine, width: usize) -> Vec<StyledLine> {
    if width == 0 {
        return vec![StyledLine::default()];
    }
    if line.display_width() <= width {
        return vec![line.clone()];
    }

    let mut lines: Vec<StyledLine> = vec![StyledLine::default()];
    let mut col = 0usize;

    for span in &line.0 {
        for word in split_keep_spaces(&span.text) {
            if word == " " {
                // Never start a wrapped line with a space: drop it at a
                // break, otherwise place it normally.
                if col == 0 {
                    continue;
                }
                if col + 1 > width {
                    lines.push(StyledLine::default());
                    col = 0;
                    continue;
                }
                push_chunk(&mut lines, &mut col, word, span.tone);
                continue;
            }
            let word_width = style::display_width(word);
            if word_width <= width {
                if col > 0 && col + word_width > width {
                    lines.push(StyledLine::default());
                    col = 0;
                }
                push_chunk(&mut lines, &mut col, word, span.tone);
                continue;
            }
            // Hard-split an overlong token across as many lines as it
            // takes. Every iteration either consumes at least one whole
            // character of `remaining` (via `take_columns`, when it fits)
            // or forces exactly one character through when even a fresh
            // (`col == 0`) line cannot fit it (a double-width glyph in a
            // pane narrower than 2 columns) -- so `remaining` strictly
            // shrinks every pass and this always terminates, unlike a
            // budget-only loop that can spin forever pushing empty lines
            // when no forward progress is possible.
            let mut remaining = word;
            while !remaining.is_empty() {
                if col >= width {
                    lines.push(StyledLine::default());
                    col = 0;
                }
                let budget = width - col;
                let (chunk, rest) = take_columns(remaining, budget);
                if !chunk.is_empty() {
                    push_chunk(&mut lines, &mut col, chunk, span.tone);
                    remaining = rest;
                    continue;
                }
                let mut boundaries = remaining.char_indices().map(|(idx, _)| idx);
                boundaries.next();
                let end = boundaries.next().unwrap_or(remaining.len());
                push_chunk(&mut lines, &mut col, &remaining[..end], span.tone);
                remaining = &remaining[end..];
            }
        }
    }
    lines
}

fn push_chunk(lines: &mut [StyledLine], col: &mut usize, text: &str, tone: Tone) {
    lines
        .last_mut()
        .expect("wrap_line always seeds at least one line")
        .0
        .push(StyledSpan {
            text: text.to_string(),
            tone,
        });
    *col += style::display_width(text);
}

/// Splits `text` into tokens where every run of non-space characters is one
/// token and every individual space is its own one-character token -- so a
/// greedy wrap can drop a trailing space at a line break without losing any
/// non-space content.
fn split_keep_spaces(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut in_space = false;
    let mut first = true;
    for (idx, ch) in text.char_indices() {
        let is_space = ch == ' ';
        if first {
            in_space = is_space;
            first = false;
            continue;
        }
        if is_space != in_space || is_space {
            out.push(&text[start..idx]);
            start = idx;
            in_space = is_space;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// Splits `text` at the byte offset of the longest prefix whose display
/// width is `<= budget`, never inside a codepoint.
fn take_columns(text: &str, budget: usize) -> (&str, &str) {
    let mut width = 0usize;
    let mut end = 0usize;
    for (idx, ch) in text.char_indices() {
        let w = char_cols(ch);
        if width + w > budget {
            break;
        }
        width += w;
        end = idx + ch.len_utf8();
    }
    (&text[..end], &text[end..])
}

/// The bullet operator direction (PR #531 follow-up) puts in front of every
/// assistant text block and tool call, Claude Code style.
const BULLET: &str = "⏺";
/// The indented tree marker a tool call's own result line hangs off, one
/// level under [`BULLET`].
const TREE_MARKER: &str = "⎿";

/// Prefixes `lines`' first line with `marker` (styled `tone`) and indents
/// every continuation line by two columns to align under it -- the
/// "`⏺ text...`" / "`  ⎿ text...`" shape used throughout this renderer.
/// `lines.is_empty()` still produces the marker on its own line, so a caller
/// never has to special-case an empty block.
fn with_marker(marker: &str, tone: Tone, mut lines: Vec<StyledLine>) -> Vec<StyledLine> {
    if lines.is_empty() {
        return vec![StyledLine::toned(marker.to_string(), tone)];
    }
    let mut first = vec![StyledSpan {
        text: format!("{marker} "),
        tone,
    }];
    first.extend(lines[0].0.clone());
    lines[0] = StyledLine(first);
    for line in lines.iter_mut().skip(1) {
        let mut indented = vec![StyledSpan {
            text: "  ".to_string(),
            tone: Tone::Plain,
        }];
        indented.extend(line.0.clone());
        *line = StyledLine(indented);
    }
    lines
}

/// Renders one [`TranscriptItem`] as [`StyledLine`]s, collapsed or expanded
/// per `expanded`. Collapsed tool calls show one summary line; expanded
/// ones show the full classified outcome.
pub fn render_item(item: &TranscriptItem, expanded: bool) -> Vec<StyledLine> {
    match item {
        TranscriptItem::User { text, steering, .. } => {
            // Operator direction (PR #531 follow-up): user turns render as
            // `>` lines. A per-line shaded background is deferred -- the
            // shared `Tone`/`StyledSpan` model this renderer and the plain-
            // text one both use carries no per-line background today, and
            // adding one is a crate-wide change well past this pane's own
            // scope (see the design note).
            let marker = if *steering { "> (steering)" } else { ">" };
            with_marker(marker, Tone::Muted, markdown_lines(text))
        }
        TranscriptItem::AssistantText { text, .. } => {
            with_marker(BULLET, Tone::Accent, markdown_lines(text))
        }
        TranscriptItem::AssistantThinking { text, .. } => with_marker(
            BULLET,
            Tone::Muted,
            markdown_lines(text)
                .into_iter()
                .map(|line| {
                    StyledLine(
                        line.0
                            .into_iter()
                            .map(|s| StyledSpan {
                                tone: Tone::Muted,
                                ..s
                            })
                            .collect(),
                    )
                })
                .collect(),
        ),
        TranscriptItem::AssistantRefusal { text, .. } => with_marker(
            BULLET,
            Tone::Warn,
            vec![StyledLine::toned(text.clone(), Tone::Warn)],
        ),
        TranscriptItem::ToolCall {
            name,
            arguments_preview,
            outcome,
            ..
        } => render_tool_call(name, arguments_preview, outcome, expanded),
        TranscriptItem::SessionEnded { reason } => {
            vec![StyledLine::toned(
                format!("\u{2014} session ended: {reason}"),
                Tone::Muted,
            )]
        }
        TranscriptItem::Elided { hidden } => {
            vec![StyledLine::toned(
                format!("\u{22ef} {hidden} older item(s) elided"),
                Tone::Muted,
            )]
        }
    }
}

fn outcome_summary(outcome: &ToolOutcomeView) -> (&'static str, Tone, String) {
    match outcome {
        ToolOutcomeView::Pending => ("\u{25b7}", Tone::Muted, "pending".to_string()),
        ToolOutcomeView::Running => ("\u{25b7}", Tone::Warn, "running\u{2026}".to_string()),
        ToolOutcomeView::Cancelled => ("\u{2298}", Tone::Muted, "cancelled".to_string()),
        ToolOutcomeView::OutcomeUnknown => (
            "\u{2049}",
            Tone::Warn,
            "outcome unknown \u{2014} needs reconciliation".to_string(),
        ),
        ToolOutcomeView::Diff { .. } => ("\u{2713}", Tone::Ok, "diff".to_string()),
        ToolOutcomeView::TestOutcome { passed, failed, .. } => {
            let tone = if failed.unwrap_or(0) > 0 {
                Tone::Err
            } else {
                Tone::Ok
            };
            let summary = match (passed, failed) {
                (Some(p), Some(f)) => format!("{p} passed, {f} failed"),
                (Some(p), None) => format!("{p} passed"),
                (None, Some(f)) => format!("{f} failed"),
                (None, None) => "test result".to_string(),
            };
            ("\u{2713}", tone, summary)
        }
        ToolOutcomeView::Artifact {
            media_type,
            byte_len,
            ..
        } => (
            "\u{2713}",
            Tone::Ok,
            format!("artifact \u{2014} {media_type}, {byte_len} bytes"),
        ),
        ToolOutcomeView::Text { .. } => ("\u{2713}", Tone::Ok, "done".to_string()),
        ToolOutcomeView::Error { message } => ("\u{2717}", Tone::Err, message.clone()),
    }
}

/// Operator direction (PR #531 follow-up): a tool call is a `⏺ name(args)`
/// bullet header followed by one indented `⎿` tree line summarising the
/// result, with "(ctrl+r to expand)" on the tree line while collapsed and
/// an outcome with more to show (a pending/running/cancelled outcome never
/// gets more detailed by expanding it, so no hint is offered for those).
/// Expanding replaces the hint with the full classified outcome body,
/// indented one level further under the tree line -- unchanged from the
/// pre-restyle layout's own column 4.
fn render_tool_call(
    name: &str,
    arguments_preview: &str,
    outcome: &ToolOutcomeView,
    expanded: bool,
) -> Vec<StyledLine> {
    let (glyph, tone, summary) = outcome_summary(outcome);
    let args = if arguments_preview.is_empty() {
        String::new()
    } else {
        format!("({arguments_preview})")
    };
    let header = StyledLine(vec![
        StyledSpan {
            text: format!("{BULLET} "),
            tone: Tone::Accent,
        },
        StyledSpan {
            text: name.to_string(),
            tone: Tone::Plain,
        },
        StyledSpan {
            text: args,
            tone: Tone::Muted,
        },
    ]);
    let mut tree_spans = vec![
        StyledSpan {
            text: format!("  {TREE_MARKER} "),
            tone: Tone::Muted,
        },
        StyledSpan {
            text: format!("{glyph} "),
            tone,
        },
        StyledSpan {
            text: summary,
            tone,
        },
    ];
    if !expanded && outcome_is_expandable(outcome) {
        tree_spans.push(StyledSpan {
            text: " (ctrl+r to expand)".to_string(),
            tone: Tone::Muted,
        });
    }
    let mut lines = vec![header, StyledLine(tree_spans)];
    if !expanded {
        return lines;
    }
    lines.extend(render_outcome_body(outcome));
    lines
}

/// Whether a collapsed tool result has anything more to show once
/// expanded -- see [`render_tool_call`]'s own doc comment.
fn outcome_is_expandable(outcome: &ToolOutcomeView) -> bool {
    !matches!(
        outcome,
        ToolOutcomeView::Pending | ToolOutcomeView::Running | ToolOutcomeView::Cancelled
    )
}

/// The two-column old/new line-number gutter a unified diff's `@@ -a,b +c,d
/// @@` hunk header establishes -- `None` once a line falls outside any hunk
/// this function has parsed a header for (malformed input, or a diff that
/// starts mid-hunk), which callers fall back to a blank gutter for rather
/// than guessing.
fn parse_hunk_header(line: &str) -> Option<(u64, u64)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old_part, rest) = rest.split_once(' ')?;
    let new_part = rest.strip_prefix('+')?;
    let new_part = new_part.split(' ').next()?;
    let old_start: u64 = old_part.split(',').next()?.parse().ok()?;
    let new_start: u64 = new_part.split(',').next()?.parse().ok()?;
    Some((old_start, new_start))
}

/// Renders a unified diff with an old/new line-number gutter and coloured
/// +/- rows (operator direction, PR #531 follow-up). Pure and total: a line
/// outside any parsed hunk (before the first `@@` header, or a header this
/// parser cannot read) gets no gutter numbers rather than a guess.
fn render_diff_lines(unified: &str) -> Vec<StyledLine> {
    let mut old_line: Option<u64> = None;
    let mut new_line: Option<u64> = None;
    let mut out = Vec::new();
    for line in unified.lines() {
        if let Some((old_start, new_start)) = parse_hunk_header(line) {
            old_line = Some(old_start);
            new_line = Some(new_start);
            out.push(StyledLine::toned(format!("    {line}"), Tone::Accent));
            continue;
        }
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            out.push(StyledLine::toned(format!("    {line}"), Tone::Muted));
            continue;
        }
        let gutter = |old: Option<u64>, new: Option<u64>| -> String {
            let old = old.map(|n| n.to_string()).unwrap_or_default();
            let new = new.map(|n| n.to_string()).unwrap_or_default();
            format!("{old:>5} {new:>5}")
        };
        if let Some(body) = line.strip_prefix('+') {
            out.push(StyledLine::toned(
                format!("    {} + {body}", gutter(None, new_line)),
                Tone::Ok,
            ));
            new_line = new_line.map(|n| n + 1);
        } else if let Some(body) = line.strip_prefix('-') {
            out.push(StyledLine::toned(
                format!("    {} - {body}", gutter(old_line, None)),
                Tone::Err,
            ));
            old_line = old_line.map(|n| n + 1);
        } else {
            let body = line.strip_prefix(' ').unwrap_or(line);
            out.push(StyledLine::toned(
                format!("    {}   {body}", gutter(old_line, new_line)),
                Tone::Muted,
            ));
            old_line = old_line.map(|n| n + 1);
            new_line = new_line.map(|n| n + 1);
        }
    }
    out
}

fn render_outcome_body(outcome: &ToolOutcomeView) -> Vec<StyledLine> {
    match outcome {
        ToolOutcomeView::Diff { unified } => render_diff_lines(unified),
        ToolOutcomeView::TestOutcome { raw, .. } => raw
            .lines()
            .map(|line| StyledLine::toned(format!("    {line}"), Tone::Plain))
            .collect(),
        ToolOutcomeView::Artifact { sha256, .. } => {
            vec![StyledLine::toned(
                format!("    sha256:{sha256}"),
                Tone::Muted,
            )]
        }
        ToolOutcomeView::Text { content } => content
            .lines()
            .map(|line| StyledLine::toned(format!("    {line}"), Tone::Plain))
            .collect(),
        ToolOutcomeView::Error { message } => {
            vec![StyledLine::toned(format!("    {message}"), Tone::Err)]
        }
        ToolOutcomeView::Pending
        | ToolOutcomeView::Running
        | ToolOutcomeView::Cancelled
        | ToolOutcomeView::OutcomeUnknown => Vec::new(),
    }
}

/// Renders every item's lines, in order, up through (and including) the
/// last visible item (see [`ScrollState`]'s own doc comment for why that
/// cut is item-based rather than line-based). Does **not** slice to a
/// viewport height -- callers take the final `viewport_lines` of the result
/// for display, which is what makes a resize a pure re-slice of the same
/// underlying content rather than a re-derivation of scroll position.
pub fn render_lines(view: &TranscriptView, presentation: &NativePresentation) -> Vec<StyledLine> {
    if view.items.is_empty() {
        return Vec::new();
    }
    let last_visible = view
        .items
        .len()
        .saturating_sub(1)
        .saturating_sub(presentation.scroll.items_back.min(view.items.len() - 1));
    let mut lines = Vec::new();
    for item in &view.items[..=last_visible] {
        let expanded = item
            .expand_key()
            .is_some_and(|key| presentation.expanded.contains(key));
        lines.extend(render_item(item, expanded));
    }
    lines
}

/// [`render_lines`] plus, when `activity` is `Some`, one final line showing
/// it -- operator direction (PR #531 follow-up): the spinner/verb/elapsed/
/// token/interrupt-hint line shown while a turn runs. Appended to the
/// transcript's own content rather than a separately reserved row, so it
/// scrolls and wraps exactly like everything else and follow-mode (already
/// "stay at the bottom") keeps it in view for free.
pub fn render_lines_with_activity(
    view: &TranscriptView,
    presentation: &NativePresentation,
    activity: Option<ActivityFacts>,
    width: usize,
) -> Vec<StyledLine> {
    let mut lines = render_lines(view, presentation);
    if let Some(facts) = activity {
        lines.push(StyledLine::toned(
            activity_line_text(facts.elapsed, facts.tokens, width),
            Tone::Accent,
        ));
    }
    lines
}

/// Word-wraps every line in `lines` to `width` columns, in order.
pub fn wrap_all(lines: &[StyledLine], width: usize) -> Vec<StyledLine> {
    lines
        .iter()
        .flat_map(|line| wrap_line(line, width))
        .collect()
}

/// The last `viewport` (wrapped) lines of `lines` -- what a renderer with a
/// pane content area `viewport` rows tall actually draws.
pub fn viewport_slice(lines: &[StyledLine], viewport: usize) -> &[StyledLine] {
    let start = lines.len().saturating_sub(viewport);
    &lines[start..]
}

fn tone_to_style(tone: Tone) -> Style {
    match tone {
        Tone::Plain => Style::default(),
        Tone::Accent => style::tui::accent(),
        Tone::Emphasis => style::tui::title(),
        Tone::Muted => style::tui::muted(),
        Tone::Ok => style::tui::ok(),
        Tone::Warn => style::tui::warning(),
        Tone::Err => style::tui::error(),
    }
}

/// One status-line's worth of text -- shared by [`render_native_pane`] and
/// [`render_plain`] so the ratatui and headless renderers can never drift.
pub fn status_line_text(facts: &StatusFacts) -> String {
    let status = classify_status(facts);
    let mut line = format!(
        "{model}  {route}  {runtime}  {billing}  {glyph} {label}",
        model = facts.model,
        route = facts.route,
        runtime = facts.runtime,
        billing = facts.billing,
        glyph = status_glyph(status),
        label = status_label(status),
    );
    // Operator direction (PR #531 follow-up): context-left%, cwd and git
    // branch join the bottom status line, in that order, each omitted
    // (rather than shown as a placeholder) when unknown -- "unknown, not a
    // guess", the same convention `resolve_billing` already documents for
    // this same struct's other fields.
    if let Some(pct) = facts.context_left_pct {
        line.push_str(&format!("  context left {pct}%"));
    }
    line.push_str("  ");
    line.push_str(&facts.cwd);
    if let Some(branch) = &facts.git_branch {
        line.push_str(&format!(" ({branch})"));
    }
    if let Some(notice) = &facts.notice {
        line.push_str("  \u{26a0} ");
        line.push_str(notice);
    }
    line
}

/// Draws the native pane's content -- status line, transcript, composer --
/// into `area`. Chrome (the pane's own border/title, the sidebar, the
/// header/footer) is the caller's, exactly as `render_grid` never draws a
/// border of its own either; this function only owns the interior.
pub fn render_native_pane(
    f: &mut Frame,
    area: Rect,
    view: &TranscriptView,
    presentation: &NativePresentation,
    facts: &StatusFacts,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let width = area.width as usize;
    let composer = composer_block(presentation, facts, width);
    // The in-flight activity line (spinner frame, rotating verb, elapsed
    // time, token total, "esc to interrupt") is the head's
    // `activity_line_text`, rendered with the transcript by
    // `render_lines_with_activity` -- issue #490 does not add a second one.
    let composer_rows = (composer.len() as u16).min(area.height.saturating_sub(1));
    let status_rows: u16 = 1;
    let transcript_height = area
        .height
        .saturating_sub(status_rows)
        .saturating_sub(composer_rows);

    let status_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: status_rows.min(area.height),
    };
    let status = classify_status(facts);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            status_line_text(facts),
            tone_to_style(status_tone(status)),
        ))),
        status_area,
    );

    if transcript_height > 0 {
        let transcript_area = Rect {
            x: area.x,
            y: area.y + status_rows,
            width: area.width,
            height: transcript_height,
        };
        let raw = render_lines_with_activity(view, presentation, facts.activity, width);
        let wrapped = wrap_all(&raw, width);
        let visible = viewport_slice(&wrapped, transcript_height as usize);
        let text: Vec<Line> = visible
            .iter()
            .map(|line| {
                Line::from(
                    line.0
                        .iter()
                        .map(|span| Span::styled(span.text.clone(), tone_to_style(span.tone)))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        f.render_widget(Paragraph::new(text), transcript_area);
    }

    if composer_rows > 0 {
        let composer_area = Rect {
            x: area.x,
            y: area.y + status_rows + transcript_height,
            width: area.width,
            height: composer_rows,
        };
        render_styled(f, composer_area, &composer);
    }
}

fn render_styled(f: &mut Frame, area: Rect, lines: &[StyledLine]) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let width = area.width as usize;
    let wrapped = wrap_all(lines, width);
    let visible = viewport_slice(&wrapped, area.height as usize);
    let text: Vec<Line> = visible
        .iter()
        .map(|line| {
            Line::from(
                line.0
                    .iter()
                    .map(|span| Span::styled(span.text.clone(), tone_to_style(span.tone)))
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    f.render_widget(Paragraph::new(text), area);
}

/// Issue #490: the whole native dashboard frame -- the conversation pane, the
/// agent/task overview beside it, the usage/health provenance strip beneath
/// it, and whichever modal (approval dialog, shortcut list, worker
/// inspection) is open. Which of those exist at all is
/// [`super::native_ux::resolve_layout`]'s decision, so the same code draws
/// 40, 80, 120 and 200 columns with no size-specific branches of its own.
pub fn render_native_dashboard(
    f: &mut Frame,
    area: Rect,
    view: &TranscriptView,
    presentation: &NativePresentation,
    facts: &StatusFacts,
    ux: &super::native_ux::UxState,
) -> bool {
    use super::native_ux::{Focus, OVERVIEW_WIDTH};
    if area.height == 0 || area.width == 0 {
        return false;
    }
    let plan = super::native_ux::resolve_layout(area.width as usize, area.height as usize);
    let usage_rows = (plan.usage_rows as u16).min(area.height.saturating_sub(6));
    let body_height = area.height - usage_rows;
    let panel_width = if plan.overview || ux.inspection.is_some() || ux.help {
        (OVERVIEW_WIDTH as u16 + 1).min(area.width / 2)
    } else {
        0
    };

    let main = Rect {
        x: area.x,
        y: area.y,
        width: area.width - panel_width,
        height: body_height,
    };
    // An open approval takes the bottom of the conversation pane, replacing
    // the composer: an approval is never answered from the composer, so
    // leaving it drawn and focusable there would advertise the wrong control.
    let approval_rendered = match &ux.approval {
        Some(dialog) => {
            let lines = dialog.lines(main.width as usize);
            let dialog_rows = (lines.len() as u16 + 1).min(main.height.saturating_sub(2));
            let transcript = Rect {
                height: main.height - dialog_rows,
                ..main
            };
            render_native_pane(f, transcript, view, presentation, facts);
            render_styled(
                f,
                Rect {
                    x: main.x,
                    y: main.y + transcript.height,
                    width: main.width,
                    height: dialog_rows,
                },
                &lines,
            );
            dialog_rows > 0 && main.width > 0
        }
        None => {
            render_native_pane(f, main, view, presentation, facts);
            false
        }
    };

    if panel_width > 0 {
        let panel = Rect {
            x: area.x + main.width + 1,
            y: area.y,
            width: panel_width - 1,
            height: body_height,
        };
        let mut lines = vec![StyledLine::toned(
            match (ux.help, ux.inspection.is_some()) {
                (true, _) => "shortcuts".to_string(),
                (false, true) => "worker".to_string(),
                (false, false) => format!(
                    "agents \u{b7} {} need you \u{b7} {} notices{}",
                    ux.overview.needs_operator(),
                    ux.notices.len(),
                    if ux.deferred.is_empty() {
                        String::new()
                    } else {
                        format!(" \u{b7} {} held", ux.deferred.len())
                    }
                ),
            },
            Tone::Emphasis,
        )];
        lines.extend(ux.panel_lines(panel.width as usize));
        render_styled(f, panel, &lines);
    }

    if usage_rows > 0 {
        let mut lines = ux.usage.lines(area.width as usize);
        if !ux.notices.is_empty() {
            for notice in ux.notices.recent(2) {
                lines.extend(notice.lines());
            }
        }
        render_styled(
            f,
            Rect {
                x: area.x,
                y: area.y + body_height,
                width: area.width,
                height: usage_rows,
            },
            &lines,
        );
    }
    let _ = Focus::Composer;
    approval_rendered
}

/// The composer's own draft rendered as plain display lines (`> ` on the
/// first line, two-space continuation on the rest -- the same convention
/// `dash::ui::draft_lines` already uses for every other dialog draft in
/// this codebase), plus one hint line naming the key contract.
pub fn composer_lines(presentation: &NativePresentation, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = presentation
        .composer
        .draft
        .split('\n')
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                format!("> {line}")
            } else {
                format!("  {line}")
            }
        })
        .flat_map(|line| {
            let styled = StyledLine::plain(line);
            wrap_line(&styled, width.max(1))
                .into_iter()
                .map(|l| l.to_plain_string())
                .collect::<Vec<_>>()
        })
        .collect();
    lines.push(composer_hint_line(presentation));
    lines
}

/// The composer's own hint line: the key contract, the `Shift+Tab`-cycled
/// [`ComposerMode`] (decorative -- see its own doc comment) and how many
/// inputs are queued behind an in-flight turn, if any. Split out of
/// [`composer_lines`] so it is directly testable without wrapping/width
/// concerns.
fn composer_hint_line(presentation: &NativePresentation) -> String {
    let queued = presentation.composer.queued.len();
    let queued_note = if queued > 0 {
        format!(" \u{b7} {queued} queued")
    } else {
        String::new()
    };
    // Review finding 3 (PR #544): an observer pane offers no send/steer/
    // approve at all -- the hint line says so instead of naming keys that
    // would only queue input no one is going to deliver.
    if presentation.observer {
        return format!(
            "? for shortcuts \u{b7} observer: read-only, the controller seat is held elsewhere{queued_note}"
        );
    }
    format!(
        "? for shortcuts \u{b7} {mode}{queued_note} \u{b7} Enter submit \u{b7} Shift+Enter \
         newline \u{b7} \u{2191} history \u{b7} / commands \u{b7} Esc interrupt",
        mode = presentation.mode.label(),
    )
}

/// The plain composer's height, for [`render_plain`]'s own sizing. The
/// bordered pane composer sizes itself from [`composer_block`].
#[allow(dead_code)]
fn composer_height(presentation: &NativePresentation, width: usize) -> u16 {
    (composer_lines(presentation, width).len() as u16).max(2)
}

/// How many completion rows the `/`, `@` and `!` entry modes may show.
pub const COMPLETION_ROWS: usize = 6;

/// Issue #490: the bordered composer, its hint line, and -- when the draft
/// starts an entry mode -- the completion list above it. The box is drawn
/// here rather than with a ratatui `Block` so `render_plain` and every
/// deterministic test below see exactly the same characters a terminal does.
pub fn composer_block(
    presentation: &NativePresentation,
    facts: &StatusFacts,
    width: usize,
) -> Vec<StyledLine> {
    use super::native_ux::{self, EntryMode};
    let width = width.max(8);
    let inner = width.saturating_sub(4);
    let mut out: Vec<StyledLine> = Vec::new();

    // Completions first: they sit above the box, the way a picker does.
    let draft = &presentation.composer.draft;
    let completions = match native_ux::classify_entry(draft) {
        EntryMode::Slash => native_ux::slash_completions(draft),
        EntryMode::File | EntryMode::Shell | EntryMode::Text => Vec::new(),
    };
    for completion in completions.iter().take(COMPLETION_ROWS) {
        out.push(StyledLine(vec![
            StyledSpan {
                text: "  ".to_string(),
                tone: Tone::Muted,
            },
            StyledSpan {
                text: format!("{:<28}", completion.label),
                tone: Tone::Accent,
            },
            StyledSpan {
                text: completion.detail.clone(),
                tone: Tone::Muted,
            },
        ]));
    }

    out.push(StyledLine::toned(
        format!("\u{256d}{}\u{256e}", "\u{2500}".repeat(width - 2)),
        Tone::Muted,
    ));
    for (index, line) in draft.split('\n').enumerate() {
        let marker = if index == 0 { ">" } else { " " };
        let body = StyledLine::plain(line.to_string());
        for (row, wrapped) in wrap_line(&body, inner).into_iter().enumerate() {
            let text = wrapped.to_plain_string();
            let pad = inner.saturating_sub(style::display_width(&text));
            out.push(StyledLine(vec![
                StyledSpan {
                    text: "\u{2502} ".to_string(),
                    tone: Tone::Muted,
                },
                StyledSpan {
                    text: if index == 0 && row == 0 {
                        format!("{marker} ")
                    } else {
                        "  ".to_string()
                    },
                    tone: Tone::Accent,
                },
                StyledSpan {
                    text: format!("{text}{}", " ".repeat(pad.saturating_sub(2))),
                    tone: Tone::Plain,
                },
                StyledSpan {
                    text: " \u{2502}".to_string(),
                    tone: Tone::Muted,
                },
            ]));
        }
    }
    out.push(StyledLine::toned(
        format!("\u{2570}{}\u{256f}", "\u{2500}".repeat(width - 2)),
        Tone::Muted,
    ));

    out.push(composer_hint_row(presentation, facts, width));
    out
}

/// Issue #490 (N21, operator direction): the mock's hint line, three columns
/// spread across the composer's own width -- `? for shortcuts` hard left, the
/// mode and what `Enter` does centred, `\u{29d7} N queued` hard right and only
/// when something IS queued.
///
/// Laid out here rather than by the renderer so the exact character positions
/// are asserted by a deterministic test at every terminal width the mock
/// draws, and so `render_plain` and a real terminal cannot disagree about
/// them.
fn composer_hint_row(
    presentation: &NativePresentation,
    facts: &StatusFacts,
    width: usize,
) -> StyledLine {
    let queued = presentation.composer.queued.len();
    let mode = match classify_submit_intent(facts) {
        SubmitIntent::Immediate => format!("{} (shift+tab)", presentation.mode.label()),
        SubmitIntent::Steer => "enter steers this turn (shift+tab)".to_string(),
        SubmitIntent::Queue => "blocked \u{2014} enter queues (shift+tab)".to_string(),
    };
    // Full form first; the narrow floor (the mock's own 40-column frame)
    // drops each column to its shortest honest spelling rather than letting
    // any of the three fall off the edge.
    let mut left = "  ? for shortcuts".to_string();
    let mut mid = mode;
    let mut right = if queued > 0 {
        format!("\u{29d7} {queued} queued")
    } else {
        String::new()
    };
    let fits = |left: &str, mid: &str, right: &str| {
        style::display_width(left) + style::display_width(mid) + style::display_width(right) + 2
            <= width
    };
    if !fits(&left, &mid, &right) {
        left = "  ?".to_string();
        mid = mid
            .split(" (shift+tab)")
            .next()
            .unwrap_or_default()
            .to_string();
        right = if queued > 0 {
            format!("\u{29d7}{queued}")
        } else {
            String::new()
        };
    }
    while !fits(&left, &mid, &right) && !mid.is_empty() {
        mid.pop();
    }
    let left_w = style::display_width(&left);
    let mid_w = style::display_width(&mid);
    let right_w = style::display_width(&right);
    // The centre column is centred on the WHOLE line, then clamped so it can
    // never overlap either side -- at the narrow floor the three simply abut.
    let centre_start = width.saturating_sub(mid_w) / 2;
    let lead = centre_start
        .max(left_w + 1)
        .saturating_sub(left_w)
        .min(width.saturating_sub(left_w + mid_w + right_w));
    let tail = width.saturating_sub(left_w + lead + mid_w + right_w);
    StyledLine(vec![
        StyledSpan {
            text: left,
            tone: Tone::Muted,
        },
        StyledSpan {
            text: " ".repeat(lead),
            tone: Tone::Muted,
        },
        StyledSpan {
            text: mid,
            tone: Tone::Muted,
        },
        StyledSpan {
            text: " ".repeat(tail),
            tone: Tone::Muted,
        },
        StyledSpan {
            text: right,
            tone: Tone::Warn,
        },
    ])
}

/// Renders the exact same view model as plain text, for a non-TTY/headless
/// surface (item 7) and for tests that want a single string to assert
/// against rather than walking [`StyledLine`]s. No ratatui, no ANSI color
/// codes -- byte-identical whether or not a terminal is attached, which is
/// the point: `zirv ctx exec --runtime native` and every deterministic test
/// below can call this without a `Frame`.
pub fn render_plain(
    view: &TranscriptView,
    presentation: &NativePresentation,
    facts: &StatusFacts,
    width: usize,
) -> String {
    let mut out = String::new();
    out.push_str(&status_line_text(facts));
    out.push('\n');
    let raw = render_lines_with_activity(view, presentation, facts.activity, width.max(1));
    let wrapped = wrap_all(&raw, width.max(1));
    for line in &wrapped {
        out.push_str(&line.to_plain_string());
        out.push('\n');
    }
    for line in composer_lines(presentation, width.max(1)) {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

// =========================================================================
// The live pane: an interactive native session driving this module's own
// reducer/composer/renderer, and a dedicated dashboard loop that opens one.
// =========================================================================

/// What `zirv chat --runtime native` needs to open one.
pub struct NativeDashboardSpec {
    pub repo: PathBuf,
    pub role: String,
    pub route: Option<String>,
    /// Whether this session should hold a writer permit for `repo` -- see
    /// `runtime::native::InteractiveRequest::writing`. A plain `zirv chat
    /// --runtime native` is the operator's own seat, the same as the
    /// orchestrator pane of a wrapped dashboard, so it is always `true`
    /// from `chat.rs`'s own call; a future read-only spawn path (a native
    /// reviewer pane, say) would pass `false`.
    pub writing: bool,
    /// `runtime::native::InteractiveRequest::provider`'s own escape hatch,
    /// threaded through so a deterministic test can open a REAL native pane
    /// against `fixture::FixtureProvider` instead of the operator's native
    /// provider configuration. `None` on every production call site, which
    /// resolves the real configuration exactly as before this field existed.
    pub provider: Option<String>,
    /// Issue #552: the SEAT this pane is taking over, as
    /// `(short, generation)` -- set only by a rollover successor
    /// (`dash::PaneSuccessorLauncher`). It keeps the seat's stable short id
    /// and runs under the generation `seat::commit` promoted. It also forces
    /// the in-process spawn: a successor is a NEW conversation under a
    /// committed generation, never an attach to whatever a persistent runtime
    /// already holds for this repository.
    pub seat: Option<(String, u64)>,
    /// Issue #552: what the successor is told first -- the handoff packet,
    /// every acknowledged input the source never delivered, and the
    /// reconciliation it is halted on. Submitted as this session's first
    /// turn, which is the only way a fresh native conversation can be handed
    /// what the source still owed.
    pub initial_input: Option<String>,
}

/// The billing label (`"api"`/`"subscription"`) for `route`'s own account,
/// read from the operator's native provider configuration. `style::
/// PLACEHOLDER` when nothing resolves (no configuration, an account the
/// config no longer names) -- the same "unknown, not a guess" convention
/// every other status fact in this module uses.
pub fn resolve_billing(route: &RouteIdentity, repo: &Path) -> String {
    use super::super::provider::BillingClass;
    use super::super::provider::config::NativeConfig;
    let Ok(home) = crate::utils::home_dir() else {
        return style::PLACEHOLDER.to_string();
    };
    let Ok(Some(native)) = NativeConfig::load(&home, repo) else {
        return style::PLACEHOLDER.to_string();
    };
    match native
        .accounts
        .get(&route.account)
        .map(|account| account.billing)
    {
        Some(BillingClass::Api) => "api".to_string(),
        Some(BillingClass::Subscription) => "subscription".to_string(),
        None => style::PLACEHOLDER.to_string(),
    }
}

fn now_ms_u64() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Best-effort checked-out branch for `repo`, read directly from `.git/
/// HEAD` rather than shelling out to `git` -- this pane polls on a ~150ms
/// tick, and spawning a process that often is not acceptable (operator
/// direction, PR #531 follow-up). `None` when `repo` is not a git checkout,
/// is in a detached-HEAD state, or its `.git` is a worktree link this
/// cannot resolve -- "unknown, not a guess", the same convention
/// [`resolve_billing`] already documents. Called once at spawn time (the
/// checked-out branch essentially never changes for the life of one chat
/// session), never per-tick.
fn git_branch(repo: &Path) -> Option<String> {
    let git_path = repo.join(".git");
    let head_path = if git_path.is_dir() {
        git_path.join("HEAD")
    } else {
        // A linked worktree's `.git` is a file: "gitdir: <real git dir>".
        let contents = std::fs::read_to_string(&git_path).ok()?;
        let real = contents.trim().strip_prefix("gitdir: ")?;
        PathBuf::from(real).join("HEAD")
    };
    let contents = std::fs::read_to_string(head_path).ok()?;
    contents
        .trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_string)
}

/// Percentage of `route`'s declared context window still free, estimated
/// from `conversation`'s own recorded usage so far -- `None` when the
/// window itself is not declared, or is declared as `0` (nothing to
/// divide by). This is an ESTIMATE against raw input+output token counts,
/// not the compaction budget's own accounting (which also weighs
/// distillation and lives inside the worker thread's own
/// `NativeSessionConfig`, not read back by this pane) -- see the design
/// note for what a truer reading would need.
fn context_left_pct(
    route: &RouteIdentity,
    usage: &super::super::event::TranscriptUsage,
) -> Option<u8> {
    let window = super::super::provider::capability::declared(route.protocol, &route.model, None)
        .context_window?;
    if window == 0 {
        return None;
    }
    let used = usage.context_total().saturating_add(usage.output_tokens);
    let used_pct = used.saturating_mul(100) / window;
    Some(100u64.saturating_sub(used_pct).min(100) as u8)
}

/// Spinner frames for [`activity_line_text`], cycled on a ~120ms cadence --
/// fast enough to read as motion, slow enough not to flicker at this pane's
/// own ~150ms tick.
const ACTIVITY_SPINNER_FRAMES: [&str; 8] = [
    "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
];

/// Rotating verbs for [`activity_line_text`] -- purely decorative (the same
/// convention Claude Code's own CLI uses), cycled on a slower ~2.5s cadence
/// than the spinner so the label is still readable between changes.
const ACTIVITY_VERBS: [&str; 6] = [
    "Thinking",
    "Working",
    "Puzzling",
    "Synthesizing",
    "Composing",
    "Reticulating",
];

/// Operator direction (PR #531 follow-up): the activity line shown while a
/// turn runs -- a spinner frame, a rotating verb, real elapsed time and a
/// running token total, ending with the interrupt hint. Pure and total:
/// `elapsed`/`tokens` are the caller's own (`NativePaneRuntime::
/// activity_line`), so this is directly testable without a live session or
/// a wall clock.
pub fn activity_line_text(elapsed: std::time::Duration, tokens: u64, width: usize) -> String {
    let millis = elapsed.as_millis() as u64;
    let spinner = ACTIVITY_SPINNER_FRAMES[(millis / 120) as usize % ACTIVITY_SPINNER_FRAMES.len()];
    let verb = ACTIVITY_VERBS[(millis / 2_500) as usize % ACTIVITY_VERBS.len()];
    let full = format!(
        "{spinner} {verb}\u{2026} (esc to interrupt \u{b7} {elapsed} \u{b7} \u{2193} {tokens} tokens)",
        elapsed = elapsed_text(elapsed),
        tokens = token_text(tokens),
    );
    if style::display_width(&full) <= width {
        return full;
    }
    // The mock's narrow floor (`docs/design/mocks/2026-09-13-native-pane.html`,
    // the 40-column frame): `\u{273b} Wrangling\u{2026} (esc \u{b7} 1m12s)`. The interrupt hint
    // and the elapsed reading are what an operator acts on; the token total is
    // the one part that can be read off the usage strip instead, so it is what
    // goes first. Narrowing beats wrapping: a wrapped activity line eats a
    // transcript row every tick and moves the whole conversation under it.
    let narrow = format!(
        "{spinner} {verb}\u{2026} (esc \u{b7} {elapsed})",
        elapsed = elapsed_text(elapsed).replace(' ', ""),
    );
    if style::display_width(&narrow) <= width {
        return narrow;
    }
    // Narrower still than the mock ever draws: keep the spinner and the hint,
    // which are the two things that say "a turn is running and esc stops it",
    // and drop the decorative verb rather than let anything wrap.
    let bare = format!(
        "{spinner} (esc \u{b7} {elapsed})",
        elapsed = elapsed_text(elapsed).replace(' ', ""),
    );
    if style::display_width(&bare) <= width {
        return bare;
    }
    spinner.to_string()
}

/// Issue #490 (PR #545 review finding 1): the inputs an activity line is
/// rendered from, carried on [`StatusFacts`] instead of a pre-rendered
/// string.
///
/// The line is width-aware now, and the width belongs to whoever is drawing --
/// `render_native_pane` and `render_plain` each know theirs, and
/// `NativePaneRuntime::status_facts` knows none. Carrying the facts rather
/// than the text is what lets both renderers narrow correctly from one place,
/// instead of one of them wrapping a string the other had already baked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActivityFacts {
    pub elapsed: std::time::Duration,
    /// The conversation's OWN recorded usage so far -- see
    /// [`NativePaneRuntime::activity_facts`] for why this is not per-turn.
    pub tokens: u64,
}

/// Issue #490 (N21 item B): the dialog's request, built from the enforcement
/// broker's OWN request -- the one whose `scope_digest` the grant is signed
/// against.
///
/// Nothing here re-derives or widens the scope: the tool name and the paths
/// come straight off `ExecutionAction`/`resolved_paths`, so the dialog can
/// never describe less authority than the grant actually carries. No
/// directory widening is offered at all, because the digest is the exact
/// thing a session-scoped "don't ask again" remembers, and inventing a
/// directory the request never carried is precisely what
/// `native_ux::detect_pending_approval` already refuses to do.
///
/// It lives here, not in `dash::native_ux`, for review finding 8's own
/// reason (PR #544): that module is a view model over durable records and
/// keeps no `enforcement` dependency. This module is the one that owns an
/// `enforcement::ApprovalPrompt`, and this is its only caller.
fn dialog_request_from_broker(
    request: &super::super::runtime::enforcement::ApprovalRequest,
    actor: impl Into<String>,
    session: impl Into<String>,
) -> super::native_ux::ApprovalRequest {
    use super::super::runtime::enforcement::ExecutionAction;
    let (tool, verb) = match &request.action {
        ExecutionAction::ReadFile { .. } => ("Read", "read"),
        ExecutionAction::WriteFile { .. } | ExecutionAction::WriteFileExact { .. } => {
            ("Write", "write")
        }
        ExecutionAction::Process { .. } => ("Bash", "run"),
        ExecutionAction::ProcessControl { .. } => ("Process", "control"),
        ExecutionAction::OutputRead { .. } => ("Output", "read"),
        ExecutionAction::Knowledge { write: true, .. } => ("Knowledge", "write"),
        ExecutionAction::Knowledge { .. } => ("Knowledge", "read"),
        ExecutionAction::Network { .. } => ("Network", "reach"),
        ExecutionAction::Mcp { .. } => ("Mcp", "call"),
        ExecutionAction::ArtifactRead { .. } => ("Artifact", "read"),
        ExecutionAction::ArtifactWrite { .. } => ("Artifact", "write"),
        ExecutionAction::Delegate { .. } => ("Task", "delegate"),
    };
    let detail = match &request.action {
        ExecutionAction::Process { invocation, .. } => format!("{invocation:?}"),
        ExecutionAction::Network { target } => format!("{target:?}"),
        ExecutionAction::Mcp { server, tool, .. } => format!("{server}/{tool}"),
        ExecutionAction::Delegate { role, task } => format!("{role}: {task}"),
        ExecutionAction::Knowledge {
            service, operation, ..
        } => format!("{service}.{operation}"),
        _ => String::new(),
    };
    super::native_ux::ApprovalRequest {
        id: request.scope_digest.clone(),
        session: session.into(),
        tool: tool.to_string(),
        scope: super::native_ux::Scope {
            verb: verb.to_string(),
            paths: request.resolved_paths.clone(),
            directory: None,
        },
        actor: actor.into(),
        reason: if detail.is_empty() {
            format!("policy {}", request.policy_fingerprint)
        } else {
            detail
        },
        preview: request
            .resolved_paths
            .iter()
            .take(super::native_ux::APPROVAL_PREVIEW_LINES)
            .map(|path| path.display().to_string())
            .collect(),
        asked_at: request.created_at,
    }
}

/// The mock's own elapsed reading: `1m 12s` past a minute, `48s` below one,
/// `1h 04m` past an hour. Never a bare second count once it stops being
/// readable as one.
fn elapsed_text(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// The mock's own token reading: `3.4k` past a thousand, the plain count
/// below one. Truncating, never rounding up -- a token total that reads
/// higher than it is, is the one direction an operator cannot check.
fn token_text(tokens: u64) -> String {
    match tokens {
        0..=999 => tokens.to_string(),
        1_000..=999_999 => format!("{}.{}k", tokens / 1_000, (tokens % 1_000) / 100),
        _ => format!("{}.{}M", tokens / 1_000_000, (tokens % 1_000_000) / 100_000),
    }
}

/// Issue #490 (N20 integration): where a native pane's conversation actually
/// lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneAttach {
    /// This process owns it -- `runtime::native::spawn_interactive`, a
    /// background thread, a journal handle of our own. The mode every native
    /// pane used before the persistent runtime existed, and still the mode
    /// whenever the operator has not opted in.
    InProcess,
    /// The persistent runtime owns it; the pane is a protocol v1 client
    /// (`dash::link::RuntimeLink`). Opening a second in-process session for
    /// the same seat is exactly the two-supervisors-on-one-conversation
    /// failure `link::RUNTIME_OWNS_IT` names.
    Runtime { session_id: String, generation: u64 },
}

/// Pure: which attachment a native pane gets. Every condition is required
/// and each for its own reason -- the operator's `[session] persistent` gate
/// (`RuntimeLink::connect` already returns `None` without it AND without
/// something listening), the runtime having actually advertised
/// `session.native` (a capability the server never offered is disabled here,
/// not attempted and refused), and a live native seat for this repository to
/// attach TO. Anything missing falls back to in-process, which is a working
/// mode rather than a failure.
pub fn resolve_attach(
    link: Option<&super::link::RuntimeLink>,
    seat: Option<&super::super::api::wire::SessionFacts>,
) -> PaneAttach {
    let Some(link) = link else {
        return PaneAttach::InProcess;
    };
    if !link.serves_native() {
        return PaneAttach::InProcess;
    }
    match seat {
        Some(facts)
            if facts.runtime == super::super::runtime::RuntimeKind::Native && facts.reachable =>
        {
            PaneAttach::Runtime {
                session_id: facts.session_id.clone(),
                generation: facts.generation,
            }
        }
        _ => PaneAttach::InProcess,
    }
}

/// A live native pane: the one thing in this module that owns a running
/// session. Everything else it holds is either a pure derivation of that
/// session's journal ([`ConversationState`]/[`TranscriptView`], refreshed by
/// [`Self::tick`]) or this module's own already-tested presentation state.
pub struct NativePaneRuntime {
    /// `None` when the persistent runtime owns this conversation: opening a
    /// second in-process session for the same seat is precisely the
    /// two-supervisors failure `link::RUNTIME_OWNS_IT` exists to prevent.
    session: Option<InteractiveSession>,
    journal: Journal,
    presentation: NativePresentation,
    conversation: ConversationState,
    /// Aggregate usage retained separately because the conversation body is
    /// discarded after each view reduction.
    recorded_usage: super::super::event::TranscriptUsage,
    transcript: TranscriptView,
    session_state: NativeSessionState,
    turn_state: Option<NativeTurnState>,
    billing: String,
    /// Issue #490: the multi-agent/attention/rollover surfaces around this
    /// one conversation -- the overview, the usage strip, notices, the
    /// approval dialog and the worker inspection. All of it is a view model
    /// over durable records; see `dash::native_ux`.
    ux: super::native_ux::UxState,
    repo: PathBuf,
    state: StateDir,
    /// Issue #490 (item 4): what must survive a compaction, a rollover or a
    /// reconnect, and the guard that refuses a submission aimed at a retired
    /// generation.
    continuity: super::native_ux::Continuity,
    /// Consecutive journal replay failures, so a recovery can be announced as
    /// a reconnect rather than passing unnoticed.
    replay_failures: u32,
    /// Compactions/resumes already announced, so the same durable record is
    /// never re-announced on the next tick.
    announced_recoveries: usize,
    /// The rollover record's `updated_at` as last announced.
    announced_rollover_at: u64,
    /// Delegations whose terminal phase has already been announced, so a
    /// completion is surfaced exactly once.
    announced_terminal: BTreeSet<String>,
    /// Set once an `InteractiveProgress::Ended` is observed; the dashboard
    /// loop's own cue to stop.
    pub ended: bool,
    /// PR #531 review finding 5: the most recent `InteractiveProgress::
    /// Notice`, surfaced on the status line. `None` until the worker thread
    /// sends one; never cleared automatically -- a notice describes a
    /// degraded session for as long as that session runs, not a one-off
    /// toast.
    notice: Option<String>,
    /// Operator direction (PR #531 follow-up): when the current turn
    /// started, for the activity line's elapsed-time reading -- `None`
    /// while idle. Set the first time `tick()` observes `Busy` for a turn
    /// and cleared on `Idle`/`Failed`/`Ended`.
    turn_started_at: Option<std::time::Instant>,
    stop_state: NativeStopState,
    /// The repo this pane is running in, for the bottom status line.
    cwd: PathBuf,
    /// The checked-out branch, read once at spawn time -- see `git_branch`'s
    /// own doc comment for why this is not re-read every tick.
    git_branch: Option<String>,
    // -- issue #490 (N20 integration): identity and transport ------------
    /// The seat's short id, the journal session and the generation this pane
    /// answers for. Held directly rather than read back off `session`,
    /// because a runtime-attached pane HAS no local `InteractiveSession`.
    short: String,
    session_id: JournalSessionId,
    generation: u64,
    /// The route, when this process resolved one. `None` for a
    /// runtime-attached pane: protocol v1's `SessionFacts` publishes no route
    /// identity, and a guessed one would be worse than an honest placeholder.
    route: Option<RouteIdentity>,
    /// Where the conversation lives. See [`resolve_attach`].
    attach: PaneAttach,
    /// The protocol client, for a runtime-attached pane only.
    link: Option<super::link::RuntimeLink>,
    runtime_stop:
        Option<std::sync::mpsc::Receiver<(super::link::RuntimeLink, Result<bool, String>)>>,
    /// The journal cursor this pane has consumed through, so a reconnect
    /// carries on rather than re-reading the conversation.
    link_cursor: u64,
    /// Review finding 6 (PR #544): a monotonic counter mixed into every
    /// [`Self::next_idempotency_key`], so two submits minted in the same
    /// millisecond never collide.
    idempotency_seq: u64,
    /// Issue #490 (N21 item B): the LIVE approval request this pane's own
    /// in-process broker is blocked on, held for exactly as long as the dialog
    /// is open. `Some` means a tool call is parked on the operator right now;
    /// answering it consumes the prompt, so a decision is applied once and
    /// only once. Always `None` for a runtime-attached pane (which answers
    /// over the protocol) and therefore for an observer pane, which holds no
    /// in-process session to block in the first place.
    live_approval: Option<super::super::runtime::enforcement::ApprovalPrompt>,
    #[cfg(test)]
    journal_payload_reads: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeStopState {
    Active,
    Requested(Instant),
    TimedOut,
    Escalated,
}

const STOP_REAP_TIMEOUT: Duration = Duration::from_secs(5);

impl NativePaneRuntime {
    pub fn spawn(
        cfg: &CtxConfig,
        state: &StateDir,
        env: EnvLookup<'_>,
        spec: NativeDashboardSpec,
    ) -> CtxResult<Self> {
        let _ = cfg;
        let session = native::spawn_interactive(
            InteractiveRequest {
                repo: spec.repo.clone(),
                role: spec.role.clone(),
                route: spec.route.clone(),
                limits: native::NativeLimits::default(),
                task: None,
                writing: spec.writing,
                provider: spec.provider.clone(),
                seat: spec.seat.clone(),
            },
            env,
        )?;
        let journal = Journal::open(state)?;
        let mut conversation = journal.replay(&session.session)?;
        let transcript =
            cap_transcript_items(build_transcript(&conversation), MAX_TRANSCRIPT_ITEMS);
        let recorded_usage = conversation_usage(&conversation);
        compact_retained_conversation(&mut conversation);
        let billing = resolve_billing(&session.route, &spec.repo);
        let git_branch = git_branch(&spec.repo);

        // Issue #490: the `@` picker may only ever offer paths from this
        // pane's own repository.
        let mut presentation = NativePresentation {
            workdir: Some(spec.repo.clone()),
            ..NativePresentation::default()
        };
        let draft = load_draft(state, &session.handle.short);
        draft.restore_onto(&mut presentation.composer);

        let continuity = super::native_ux::Continuity::new(super::native_ux::SeatIdentity {
            short: session.handle.short.clone(),
            session: session.session.to_string(),
            generation: session.handle.generation,
        });

        // Issue #552: what the source still owed, handed to the successor as
        // its first turn. Submitted AFTER the opening replay, so this pane is
        // fully built before a turn can start under it; the pane re-reads the
        // journal every tick, so the input appears on the next one.
        if let Some(initial) = spec.initial_input.as_deref()
            && !initial.trim().is_empty()
        {
            let _ = session.submit(initial.to_string());
        }

        Ok(Self {
            short: session.handle.short.clone(),
            session_id: session.session.clone(),
            generation: session.handle.generation,
            route: Some(session.route.clone()),
            attach: PaneAttach::InProcess,
            link: None,
            runtime_stop: None,
            link_cursor: 0,
            idempotency_seq: 0,
            live_approval: None,
            #[cfg(test)]
            journal_payload_reads: 1,
            session: Some(session),
            journal,
            presentation,
            conversation,
            recorded_usage,
            transcript,
            session_state: NativeSessionState::Idle,
            turn_state: None,
            billing,
            ux: super::native_ux::UxState::default(),
            repo: spec.repo.clone(),
            state: state.clone(),
            continuity,
            replay_failures: 0,
            announced_recoveries: 0,
            announced_rollover_at: 0,
            announced_terminal: BTreeSet::new(),
            ended: false,
            notice: None,
            turn_started_at: None,
            stop_state: NativeStopState::Active,
            cwd: spec.repo,
            git_branch,
        })
    }

    /// Issue #490: a pane over a conversation the persistent runtime already
    /// owns. Nothing is spawned: the journal is the same durable SQLite file
    /// the runtime writes, so the transcript reducer is unchanged, and every
    /// ACTION (submit, steer, interrupt, approve) goes out over protocol v1
    /// instead of into a local session -- see [`Self::send_submit`],
    /// [`Self::interrupt`] and [`Self::decide_approval`].
    pub fn attach_runtime(
        state: &StateDir,
        mut link: super::link::RuntimeLink,
        facts: &super::super::api::wire::SessionFacts,
        repo: PathBuf,
    ) -> CtxResult<Self> {
        let session_id = JournalSessionId::new(facts.session_id.clone())
            .map_err(|error| format!("native chat: runtime session id: {error}"))?;
        let journal = Journal::open(state)?;
        let mut conversation = journal.replay(&session_id)?;
        let transcript =
            cap_transcript_items(build_transcript(&conversation), MAX_TRANSCRIPT_ITEMS);
        let recorded_usage = conversation_usage(&conversation);
        compact_retained_conversation(&mut conversation);
        let git_branch = git_branch(&repo);

        let mut presentation = NativePresentation {
            workdir: Some(repo.clone()),
            ..NativePresentation::default()
        };
        load_draft(state, &facts.short).restore_onto(&mut presentation.composer);

        // Review finding 3 (PR #544): register this pane as the session's
        // controller instead of relying on the server's "nobody attached
        // yet" bypass in `native_controller_check`, which stops applying
        // silently the moment any other client attaches. A refusal, or a
        // non-controller outcome, falls back to observer mode: no
        // send/steer/approve is offered (`composer_hint_line`,
        // `send_submit`, `interrupt`, `decide_approval`), and the failure is
        // surfaced as a notice rather than swallowed.
        let mut attach_notice = None;
        match link.attach(&facts.session_id, true) {
            Ok(attachment) => {
                presentation.observer =
                    attachment.role != super::super::api::wire::AttachRole::Controller;
                if presentation.observer {
                    attach_notice = Some(format!(
                        "attach: observer only -- {} holds the controller seat",
                        attachment.controller.as_deref().unwrap_or("another client")
                    ));
                }
            }
            Err(error) => {
                presentation.observer = true;
                attach_notice = Some(format!(
                    "attach refused by the runtime: {error} -- falling back to observer mode"
                ));
            }
        }

        let continuity = super::native_ux::Continuity::new(super::native_ux::SeatIdentity {
            short: facts.short.clone(),
            session: facts.session_id.clone(),
            generation: facts.generation,
        });

        Ok(Self {
            short: facts.short.clone(),
            session_id,
            generation: facts.generation,
            route: None,
            attach: PaneAttach::Runtime {
                session_id: facts.session_id.clone(),
                generation: facts.generation,
            },
            link: Some(link),
            runtime_stop: None,
            link_cursor: 0,
            idempotency_seq: 0,
            live_approval: None,
            #[cfg(test)]
            journal_payload_reads: 1,
            session: None,
            journal,
            presentation,
            conversation,
            recorded_usage,
            transcript,
            session_state: NativeSessionState::Idle,
            turn_state: None,
            billing: style::PLACEHOLDER.to_string(),
            ux: super::native_ux::UxState::default(),
            repo: repo.clone(),
            state: state.clone(),
            continuity,
            replay_failures: 0,
            announced_recoveries: 0,
            announced_rollover_at: 0,
            announced_terminal: BTreeSet::new(),
            ended: false,
            notice: attach_notice,
            turn_started_at: None,
            stop_state: NativeStopState::Active,
            cwd: repo,
            git_branch,
        })
    }

    /// What the status line calls this pane's runtime. A runtime-attached
    /// pane says so: the operator must be able to tell at a glance whether
    /// closing this window stops the conversation or merely detaches from it.
    fn runtime_label(&self) -> &'static str {
        match self.attach {
            PaneAttach::InProcess => "native",
            PaneAttach::Runtime { .. } => "native \u{b7} runtime",
        }
    }

    fn route_model_vendor(&self) -> String {
        self.route
            .as_ref()
            .map(|route| route.model.vendor.to_string())
            .unwrap_or_else(|| style::PLACEHOLDER.to_string())
    }

    fn route_model_id(&self) -> String {
        self.route
            .as_ref()
            .map(|route| route.model.id.to_string())
            .unwrap_or_else(|| style::PLACEHOLDER.to_string())
    }

    fn route_label(&self) -> String {
        self.route
            .as_ref()
            .map(
                |route| match super::super::runtime::execution::spec(route.endpoint.as_ref()) {
                    Ok(spec) => format!(
                        "{} via {} (subscription; spend unknown)",
                        route.route, spec.id
                    ),
                    Err(_) => route.route.to_string(),
                },
            )
            .unwrap_or_else(|| style::PLACEHOLDER.to_string())
    }

    fn context_left(&self) -> Option<u8> {
        if self.route.as_ref().is_some_and(|route| {
            super::super::runtime::execution::spec(route.endpoint.as_ref()).is_ok()
        }) {
            return None; // The official harness owns compaction and its context window.
        }
        self.route
            .as_ref()
            .and_then(|route| context_left_pct(route, &self.recorded_usage))
    }

    /// Progress, for an in-process pane. A runtime-attached one learns the
    /// same thing from the journal cursor instead -- protocol v1 publishes
    /// durable events, not a progress channel.
    fn drain_progress(&mut self) -> Vec<InteractiveProgress> {
        match self.session.as_ref() {
            Some(session) => session.drain_progress(),
            None => Vec::new(),
        }
    }

    /// Starts a turn. One place, two transports: `session.send_input` over
    /// protocol v1 for a runtime-owned conversation (with this pane's own
    /// idempotency key, so a reconnect that resends cannot start a second
    /// turn), the in-process channel otherwise.
    fn send_submit(&mut self, text: &str) {
        // Review finding 3 (PR #544): an observer pane (no controller seat)
        // never attempts a send -- the server would refuse it anyway, and
        // attempting it silently would contradict the hint line that just
        // told the operator this pane is read-only.
        if self.link.is_some() && self.presentation.observer {
            self.notice = Some(
                "submit unavailable: this pane holds no controller seat (observer mode)"
                    .to_string(),
            );
            return;
        }
        if self.link.is_some() {
            let session_id = self.session_id.to_string();
            // Review finding 6 (PR #544): `short-{now_ms}` alone can
            // collide within the same millisecond (two queued sends
            // draining back to back, or a fast double-Enter). Append a
            // per-pane monotonic counter so two keys minted in the same
            // tick are always distinct. Computed before borrowing
            // `self.link` mutably below, since it needs `&mut self` too.
            let key = self.next_idempotency_key();
            if let Some(link) = self.link.as_mut()
                && let Err(error) = link.submit(&session_id, text, Some(&key))
            {
                self.notice = Some(format!("submit refused by the runtime: {error}"));
            }
        } else if let Some(session) = self.session.as_ref() {
            let _ = session.submit(text.to_string());
        }
    }

    /// Review finding 6 (PR #544): this pane's own idempotency identity for
    /// [`Self::send_submit`] -- `short-now_ms-seq`, where `seq` is a
    /// monotonic counter that makes two keys minted in the same millisecond
    /// distinct even though `now_ms_u64()` alone would not.
    fn next_idempotency_key(&mut self) -> String {
        self.idempotency_seq = self.idempotency_seq.wrapping_add(1);
        format!("{}-{}-{}", self.short, now_ms_u64(), self.idempotency_seq)
    }

    // -- issue #490 (N21 item A): what a `dash::pane::Pane` asks a native
    //    driver for, so a native pane can live in the ordinary dashboard's
    //    pane vector beside wrapped ones. -----------------------------------

    /// The seat short id this pane answers at -- its mail/nudge address, and
    /// the id the restore roster and the budget/attention sweeps key on. The
    /// same short id `runtime::native::spawn_interactive` registered the seat
    /// under, never a second one minted here.
    pub fn short(&self) -> &str {
        &self.short
    }

    /// The journal session backing this conversation.
    pub fn journal_session(&self) -> String {
        self.session_id.to_string()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Where this conversation lives -- see [`PaneAttach`]. A runtime-owned
    /// pane is detached on shutdown rather than stopped, which is exactly
    /// what a restore needs to know.
    pub fn attach(&self) -> &PaneAttach {
        &self.attach
    }

    /// The journal sequence this pane has rendered through. The dashboard's
    /// drain uses it as "did anything change this tick" without needing to
    /// know anything else about the conversation.
    pub fn last_sequence(&self) -> u64 {
        self.conversation.last_sequence.0
    }

    /// Whether a turn is running right now.
    pub fn busy(&self) -> bool {
        !matches!(self.stop_state, NativeStopState::Active)
            || matches!(self.session_state, NativeSessionState::Running)
    }

    /// Whether this pane is waiting on a human -- an open approval dialog.
    /// Distinct from [`Self::busy`] for the reason the design note gives:
    /// both stop a session, only one is waiting on an operator.
    pub fn blocked(&self) -> bool {
        self.ux.approval.is_some()
    }

    /// Whether the operator is composing text that automatic delivery must
    /// not replace.
    pub fn has_draft(&self) -> bool {
        !self.presentation.composer.draft.is_empty()
    }

    pub fn holds_writer_permit(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(InteractiveSession::holds_writer_permit)
    }

    /// The model this conversation is actually running on, for the sidebar's
    /// own disclosure line. `None` for a runtime-attached pane, whose
    /// `SessionFacts` publishes no route identity -- an honest unknown rather
    /// than a guess.
    pub fn launch_model(&self) -> Option<&str> {
        self.route.as_ref().map(|route| route.model.id.as_str())
    }

    /// The conversation's own measured token usage, for the budget sweep and
    /// the session-spend accounting -- the same shape a wrapped pane reports
    /// from its transcript.
    pub fn measured_usage(&self) -> super::super::event::TranscriptUsage {
        self.recorded_usage
    }

    /// Issue #490 (N21 item A): the mail sweep's delivery path for a native
    /// pane. A wrapped pane is typed into and then submitted with a carriage
    /// return; a native pane has no composer to type into, so the message
    /// goes through the SAME submit path the operator's own Enter uses --
    /// which means it is subject to the same rollover/generation guard
    /// (`resolve_submit_target`) and can never be written into a retired
    /// generation.
    pub fn deliver(&mut self, label: &str, body: &str) -> CtxResult<()> {
        if self.has_draft() {
            return Err("native pane: operator draft is still being composed".into());
        }
        let text = if label.is_empty() {
            body.to_string()
        } else {
            format!("[{label}] {body}")
        };
        self.presentation.composer.draft = text;
        self.presentation.composer.cursor = self.presentation.composer.draft.len();
        self.handle_composer_action(ComposerAction::Submit);
        Ok(())
    }

    pub fn ux(&self) -> &super::native_ux::UxState {
        &self.ux
    }

    pub fn ux_mut(&mut self) -> &mut super::native_ux::UxState {
        &mut self.ux
    }

    /// Re-derives the agent/task overview and the usage strip from the
    /// authoritative records, and carries this pane across a seat generation
    /// change when one happened. Rate-limited by the caller (see
    /// `UxState::refreshed_at`) because it is the one part of a tick that
    /// touches the filesystem for anything other than this session's journal.
    pub fn refresh_records(&mut self, cfg: &CtxConfig, env: EnvLookup<'_>, now: u64) {
        use super::super::{coordinator, delegation, pool, seat};

        let graph = coordinator::load(&self.state, &self.repo);
        let records = delegation::list(&self.state, &self.repo);
        // Item 8: a tick reads a BOUNDED number of seat records however large
        // the fleet is. `fanout_plan` decides how many; the rest are picked up
        // on a later tick rather than turning one 150 ms frame into hundreds
        // of filesystem reads.
        let shorts: Vec<&str> = std::iter::once(self.short.as_str())
            .chain(records.iter().map(|record| record.handle.short.as_str()))
            .collect();
        let fanout = super::native_ux::fanout_plan(shorts.len(), &self.ux.budget);
        let seats: Vec<seat::Seat> = shorts
            .iter()
            .take(fanout.polled)
            .filter_map(|short| seat::load(&self.state, short))
            .collect();

        // Item 4: the seat may have rolled over underneath this pane. Carry
        // the draft/selection/focus/scrollback across and re-target anything
        // queued -- never replay it into the retired session.
        if let Some(current) = seats
            .iter()
            .find(|seat| seat.short == self.continuity.seat.short)
        {
            let next = super::native_ux::SeatIdentity::from_seat(current);
            if let super::native_ux::Retarget::Retargeted {
                from_session,
                to_session,
                generation,
                queued,
            } = self.continuity.carry_across(next)
            {
                // Review finding 1 (PR #544): `carry_across` only updates
                // `self.continuity.seat` -- this pane's OWN identity
                // (`self.session_id`/`self.generation`, read by every tick's
                // journal replay, link polling and `current_identity`'s own
                // guard) must be resynced too, or `resolve_submit_target`
                // disagrees with `continuity.seat` forever after the first
                // rollover. See `apply_retarget`.
                self.apply_retarget(&to_session, generation);
                self.ux.notices.push(super::native_ux::Notice {
                    kind: super::native_ux::NoticeKind::Rollover,
                    headline: format!("seat moved to generation {generation}"),
                    detail: vec![
                        format!("{from_session} \u{2192} {to_session}"),
                        format!("{queued} queued line(s) re-targeted; draft and scrollback kept"),
                    ],
                    at: now,
                });
            }
        }

        // Item 4: N19's own durable rollover record, and N17's compaction/
        // resume history -- both read straight off the durable store, so a
        // rollover or a compaction that happened while this pane was not
        // looking is still announced exactly once.
        if let Some(record) = super::super::rollover_runtime::load(&self.state, &self.short)
            && record.updated_at > self.announced_rollover_at
        {
            self.announced_rollover_at = record.updated_at;
            let notice = super::native_ux::notice_from_rollover(&record);
            self.ux.notices.push(notice);
        }
        if let Ok(history) =
            super::super::runtime::compaction::history(&self.journal, &self.session_id)
        {
            let total = history.compactions.len() + history.resumes.len();
            if total > self.announced_recoveries {
                self.announced_recoveries = total;
                if let Some(last) = history.compactions.last() {
                    self.ux.notices.push(super::native_ux::notice_compaction(
                        last.covers_through as usize,
                        last.sequence,
                        Some(&last.kind),
                    ));
                }
                for resume in &history.resumes {
                    self.ux
                        .notices
                        .push(super::native_ux::notice_reconnect(0, resume.sequence, 0));
                }
            }
        }

        let view = pool::build(&self.state, cfg, now, Some(self.session_id.as_str()), None);
        let approvals: Vec<super::native_ux::ApprovalRequest> = self
            .ux
            .approval
            .as_ref()
            .map(|dialog| vec![dialog.request.clone()])
            .unwrap_or_default();
        self.ux.refresh(
            &graph,
            &records,
            &seats,
            &approvals,
            &view,
            &self.billing,
            now,
        );

        // Criterion 2: completion notices are not lost. A worker that
        // finishes while the operator is stuck in an approval dialog has its
        // completion DEFERRED, not dropped and not drawn over the modal --
        // `close_approval` releases everything held, in order.
        let blocked = self.ux.blocked();
        for record in &records {
            if !record.phase.is_terminal()
                || !self
                    .announced_terminal
                    .insert(record.handle.delegation.clone())
            {
                continue;
            }
            let item = super::native_ux::Deferred {
                kind: "worker",
                id: record.handle.delegation.clone(),
                body: format!(
                    "{} \u{b7} {} \u{b7} exit {}",
                    record.handle.short,
                    record.phase.as_str(),
                    record
                        .exit_code
                        .map(|code| code.to_string())
                        .unwrap_or_else(|| style::PLACEHOLDER.to_string())
                ),
            };
            if blocked {
                self.ux.deferred.defer(item);
            } else {
                self.ux.notices.push(super::native_ux::Notice {
                    kind: super::native_ux::NoticeKind::DeferredDelivery,
                    headline: format!("{} finished", record.handle.short),
                    detail: vec![item.body],
                    at: now,
                });
            }
        }
        let _ = env;
    }

    /// Opens a worker's BOUNDED manifest -- never its transcript.
    pub fn open_inspection(&mut self, delegation_id: &str) {
        use super::super::delegation;
        let Some(record) = delegation::load(&self.state, &self.repo, delegation_id) else {
            return;
        };
        let Ok(manifest) = delegation::result(
            &self.state,
            &self.repo,
            delegation_id,
            super::native_ux::INSPECTION_SUMMARY_CAP,
        ) else {
            return;
        };
        let state = self
            .ux
            .overview
            .rows
            .iter()
            .find(|row| row.id == delegation_id)
            .map(|row| row.state)
            .unwrap_or(super::native_ux::AgentState::Done);
        self.ux.inspection = Some(super::native_ux::build_inspection(
            &record, &manifest, state,
        ));
        self.ux.focus = super::native_ux::Focus::Inspection;
    }

    /// Sends the composer's current draft to one worker as a bounded
    /// follow-up. `delegation::send` is the whole mechanism: a worker that is
    /// itself blocked has the message QUEUED and retried at its next idle
    /// boundary rather than typed at its dialog, which is exactly the
    /// "deferred delivery resumes when the block clears" contract.
    pub fn follow_up(&mut self, delegation_id: &str, cfg: &CtxConfig, now: u64) {
        use super::super::delegation;
        let body = self.presentation.composer.draft.trim().to_string();
        if body.is_empty() {
            return;
        }
        // An observer pane offers nothing that steers a worker, the mailbox
        // included: the same rule the composer hint, submit, interrupt and
        // approval paths apply.
        if self.link.is_some() && self.presentation.observer {
            self.notice = Some(
                "follow-up unavailable: this pane holds no controller seat (observer mode)"
                    .to_string(),
            );
            return;
        }
        // Review finding 2 (PR #544): route through the same current-session
        // guard a composer submit uses (finding 1) rather than calling
        // `delegation::send` unconditionally. `delegation::send` addresses
        // its target by the worker's short id through the shared mailbox,
        // not by this pane's own session/generation, so it cannot itself
        // tell a live pane from a stale one -- that is this pane's own
        // identity to know, not the mail path's.
        if let super::native_ux::SubmitTarget::Hold { reason } =
            super::native_ux::resolve_submit_target(&self.continuity, &self.current_identity())
        {
            self.ux.notices.push(super::native_ux::Notice {
                kind: super::native_ux::NoticeKind::Rollover,
                headline: format!(
                    "follow-up to {delegation_id} held: this pane no longer owns the seat's session"
                ),
                detail: vec![reason],
                at: now,
            });
            return;
        }
        let headline =
            match delegation::send(&self.state, &self.repo, cfg, delegation_id, &body, now) {
                Ok(delegation::Dispatch::Queued { reason, .. }) => {
                    format!("follow-up to {delegation_id} queued ({reason})")
                }
                Ok(_) => format!("follow-up delivered to {delegation_id}"),
                Err(error) => format!("follow-up to {delegation_id} failed: {error}"),
            };
        self.ux.notices.push(super::native_ux::Notice {
            kind: super::native_ux::NoticeKind::DeferredDelivery,
            headline,
            detail: Vec::new(),
            at: now,
        });
        self.presentation.composer.draft.clear();
        self.presentation.composer.cursor = 0;
    }

    /// Delivers the operator's approval decision. A denial is a complete,
    /// real action: the guidance is committed to the journal as steering, so
    /// the running loop picks it up between requests. An allow is only ever
    /// offered when the session's broker can actually issue a grant (see
    /// `native_ux::PendingApproval::grantable`).
    pub fn decide_approval(
        &mut self,
        request: &super::native_ux::ApprovalRequest,
        decision: super::native_ux::ApprovalDecision,
        persistent: bool,
    ) {
        use super::native_ux::{ApprovalDecision, ApprovalRoute};
        let released = self.ux.close_approval();
        let route = super::native_ux::approval_route(persistent && self.link.is_some());
        let guidance = format!(
            "The operator denied {}. Do not retry it; choose a different approach and say what you changed.",
            request.scope_text()
        );
        match route {
            // Issue #490 + N20: a runtime-owned conversation's decision goes
            // to the service that is actually holding the request open, by
            // ITS request id -- the dashboard never mints a grant of its own,
            // and a note carries the "tell the agent what to do differently"
            // text of a denial.
            // Review finding 3 (PR #544): an observer pane holds no
            // controller seat -- approving or denying is not offered at
            // all, per the same rule as submit/steer/interrupt.
            ApprovalRoute::Protocol if self.presentation.observer => {
                self.notice = Some(
                    "approval unavailable: this pane holds no controller seat (observer mode)"
                        .to_string(),
                );
            }
            ApprovalRoute::Protocol => {
                let session_id = self.session_id.to_string();
                let wire = match decision {
                    ApprovalDecision::Allow | ApprovalDecision::AllowAlways => {
                        super::super::api::wire::ApprovalDecision::Allow
                    }
                    ApprovalDecision::Deny => super::super::api::wire::ApprovalDecision::Deny,
                };
                let note = (decision == ApprovalDecision::Deny).then_some(guidance.as_str());
                let outcome = self
                    .link
                    .as_mut()
                    .map(|link| link.approve(&session_id, &request.id, wire, note));
                if let Some(Err(error)) = outcome {
                    self.notice = Some(format!("approval refused by the runtime: {error}"));
                }
            }
            // Review finding 3 (PR #544), extended to the broker route by
            // issue #490's own live-approval path: an observer pane holds no
            // controller seat, so consent is not its to give by EITHER route.
            // A live prompt is deliberately left parked rather than answered
            // or dropped -- the controller's own pane still holds it, and a
            // tool call that fails closed because a bystander said no is
            // exactly the outcome observer mode exists to prevent.
            ApprovalRoute::Broker if self.presentation.observer => {
                self.notice = Some(
                    "approval unavailable: this pane holds no controller seat (observer mode)"
                        .to_string(),
                );
            }
            // The in-process broker. Issue #490 (N21 item B): when a LIVE
            // request is held, the decision goes straight back to the tool
            // call that is blocked on it -- Yes releases it once,
            // "don't ask again" also remembers this exact scope for the rest
            // of the session, and No fails the call with the operator's own
            // guidance AND commits that guidance as steering so the loop picks
            // it up between requests. A decision is applied exactly once: the
            // prompt is consumed here and cannot be answered again.
            ApprovalRoute::Broker => {
                match self.live_approval.take() {
                    Some(prompt) => {
                        use super::super::runtime::enforcement::InteractiveDecision;
                        if decision == ApprovalDecision::Deny {
                            self.commit_denial_guidance(&guidance);
                        }
                        let answer = match decision {
                            ApprovalDecision::Allow => InteractiveDecision::Once,
                            ApprovalDecision::AllowAlways => InteractiveDecision::Remember,
                            ApprovalDecision::Deny => InteractiveDecision::Deny {
                                guidance: guidance.clone(),
                            },
                        };
                        if !prompt.decide(answer) {
                            // The call was already cancelled (an interrupt, or
                            // the session ended) -- nothing was released, and
                            // saying so is better than implying the tool ran.
                            self.notice = Some(format!(
                                "the approval for {} was already cancelled; nothing ran",
                                request.scope_text()
                            ));
                        }
                    }
                    // No live request: the dialog was reconstructed from a
                    // refusal the journal already recorded, so a denial's
                    // guidance is still a complete action and an allow has
                    // nothing left to release.
                    None => match decision {
                        ApprovalDecision::Deny => self.commit_denial_guidance(&guidance),
                        ApprovalDecision::Allow | ApprovalDecision::AllowAlways => {
                            self.ux.notices.push(super::native_ux::Notice {
                                kind: super::native_ux::NoticeKind::DeferredDelivery,
                                headline: format!(
                                    "approval {} via {route:?} \u{2014} {} (the call it belonged \
                                     to is no longer waiting)",
                                    decision.as_str(),
                                    request.scope_text()
                                ),
                                detail: Vec::new(),
                                at: 0,
                            });
                        }
                    },
                }
            }
        }
        for item in released {
            self.ux.notices.push(super::native_ux::Notice {
                kind: super::native_ux::NoticeKind::DeferredDelivery,
                headline: format!("{} {} delivered", item.kind, item.id),
                detail: vec![item.body],
                at: 0,
            });
        }
    }

    /// Review finding 7 (PR #544): a denial's guidance used to be written as
    /// steering off `self.session_id` unconditionally, with none of the
    /// generation guard finding 1 gives every other send/steer path. It is
    /// routed through the same current-session resolution instead: held --
    /// never written into a retired generation -- exactly as a composer
    /// submit is, and re-targeted to the seat's CURRENT session when the pane
    /// is carried across a rollover.
    ///
    /// Issue #490 (N21 item B) shares it between both denial paths: the live
    /// in-process request blocked on the operator right now, and the one
    /// reconstructed from a refusal the journal already recorded. Both commit
    /// the same guidance under the same guard.
    fn commit_denial_guidance(&mut self, guidance: &str) {
        match super::native_ux::resolve_submit_target(&self.continuity, &self.current_identity()) {
            super::native_ux::SubmitTarget::Send { .. } => {
                let _ = self.write_steering(guidance);
            }
            super::native_ux::SubmitTarget::Hold { reason } => {
                self.ux.notices.push(super::native_ux::Notice {
                    kind: super::native_ux::NoticeKind::Rollover,
                    headline: "denial held: this pane no longer owns the seat's session"
                        .to_string(),
                    detail: vec![reason],
                    at: 0,
                });
                self.presentation.composer.queued.push(QueuedInput {
                    text: guidance.to_string(),
                    steering: true,
                    queued_at_ms: now_ms_u64(),
                });
            }
        }
    }

    /// Drains worker progress and re-reads the journal. Called once per
    /// dashboard tick; cheap (a `try_recv` loop plus one SQLite read) so a
    /// short poll interval costs nothing while the session is idle.
    pub fn tick(&mut self) {
        for progress in self.drain_progress() {
            match progress {
                InteractiveProgress::Busy => {
                    self.session_state = NativeSessionState::Running;
                    self.turn_state = Some(NativeTurnState::Requesting);
                    // `NativePaneRuntime::turn_started_at` is the ONE clock
                    // for "how long has this turn been running" -- the
                    // activity line and issue #490's spinner line both read
                    // it through `elapsed_turn_secs`, so they can never
                    // disagree.
                    if self.turn_started_at.is_none() {
                        self.turn_started_at = Some(std::time::Instant::now());
                    }
                }
                InteractiveProgress::Idle => {
                    self.session_state = NativeSessionState::Idle;
                    self.turn_state = None;
                    self.turn_started_at = None;
                }
                InteractiveProgress::Failed(_) => {
                    self.session_state = NativeSessionState::Idle;
                    self.turn_state = None;
                    self.turn_started_at = None;
                }
                InteractiveProgress::Notice(message) => {
                    // Issue #490: a runtime notice is also a pane notice, so
                    // it survives in the scrollback rather than only in the
                    // single-slot `notice` the activity line shows.
                    self.ux.notices.push(super::native_ux::Notice {
                        kind: super::native_ux::NoticeKind::Reconnected,
                        headline: message.clone(),
                        detail: Vec::new(),
                        at: 0,
                    });
                    self.notice = Some(message);
                }
                InteractiveProgress::Ended => {
                    if matches!(self.stop_state, NativeStopState::Active) {
                        self.ended = true;
                        self.session_state = NativeSessionState::Completed;
                    }
                    self.turn_started_at = None;
                }
            }
        }
        self.reap_stopping_session();
        if self.ended {
            self.refresh_transcript();
            return;
        }
        // Issue #490 + N20: a runtime-attached pane has no progress channel.
        // Its cue that something happened is protocol v1's journal cursor --
        // the same durable sequence the transcript is reduced from -- and a
        // `gap` is the runtime telling us the cursor cannot be continued,
        // which is a reconnect the operator must see rather than a silent
        // resynchronization.
        if let Some(link) = self.link.as_mut() {
            let session_id = self.session_id.to_string();
            match link.events(&session_id, self.link_cursor) {
                Ok(page) => {
                    if page.gap {
                        self.ux.notices.push(super::native_ux::notice_reconnect(
                            0,
                            page.cursor,
                            page.last_sequence.saturating_sub(self.link_cursor) as usize,
                        ));
                    }
                    self.link_cursor = page.cursor;
                    self.session_state = if page.cursor < page.last_sequence {
                        NativeSessionState::Running
                    } else {
                        NativeSessionState::Idle
                    };
                }
                Err(error) => {
                    self.notice = Some(format!("runtime journal unavailable: {error}"));
                }
            }
        }
        self.refresh_transcript();
        let actor = format!("{} \u{b7} {}", self.short, "orchestrator");
        // Issue #490 (N21 item B): a LIVE request outranks a transcript-
        // derived one. A tool call is blocked on this answer right now, the
        // request carries the broker's own scope digest, and the dialog it
        // opens can actually grant -- so it is polled first and, while it is
        // held, the journal-derived detector is not allowed to replace it with
        // a reconstruction of an older refusal.
        self.poll_live_approval(&actor);
        if self.live_approval.is_some() {
            return;
        }
        // Issue #490 (item 5): `blocked` is now a fact read from what the
        // journal recorded -- the broker's own approval refusal on a tool
        // call -- rather than the hardcoded `false` N11 shipped.
        let pending = super::native_ux::detect_pending_approval(
            &self.transcript.items,
            &self.session_id.to_string(),
            &actor,
        );
        self.ux.sync_approval(pending);
    }

    fn reap_stopping_session(&mut self) {
        let stopping = !matches!(self.stop_state, NativeStopState::Active);
        if !stopping {
            return;
        }
        if let Some(receiver) = self.runtime_stop.as_ref() {
            match receiver.try_recv() {
                Ok((link, result)) => {
                    self.runtime_stop = None;
                    self.link = Some(link);
                    match result {
                        Ok(_) => {
                            self.session_state = NativeSessionState::Interrupted;
                            self.turn_state = None;
                            self.turn_started_at = None;
                            self.ended = true;
                        }
                        Err(error) => {
                            self.stop_state = NativeStopState::TimedOut;
                            self.notice =
                                Some(format!("native pane: runtime stop failed: {error}"));
                        }
                    }
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.runtime_stop = None;
                    self.stop_state = NativeStopState::TimedOut;
                    self.notice = Some("native pane: runtime stop worker disconnected".to_string());
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        if matches!(self.stop_state, NativeStopState::Requested(_))
            && !matches!(self.session_state, NativeSessionState::Running)
            && let Some(session) = self.session.as_mut()
        {
            session.request_shutdown();
        }
        if self
            .session
            .as_mut()
            .is_some_and(InteractiveSession::try_finish_shutdown)
        {
            let _ = self.session.take();
            self.session_state = NativeSessionState::Interrupted;
            self.turn_state = None;
            self.turn_started_at = None;
            self.ended = true;
            return;
        }
        if matches!(self.stop_state, NativeStopState::Requested(at) if at.elapsed() >= STOP_REAP_TIMEOUT)
        {
            self.stop_state = NativeStopState::TimedOut;
            self.notice = Some(
                "native pane: stop is still waiting for the worker; press Stop again to escalate"
                    .to_string(),
            );
        }
    }

    fn request_runtime_stop(&mut self) {
        let Some(mut link) = self.link.take() else {
            return;
        };
        let session_id = self.session_id.to_string();
        let (sender, receiver) = std::sync::mpsc::channel();
        match std::thread::Builder::new()
            .name("zirv-runtime-stop".to_string())
            .spawn(move || {
                let result = link.stop(&session_id).map_err(|error| error.to_string());
                let _ = sender.send((link, result));
            }) {
            Ok(_) => self.runtime_stop = Some(receiver),
            Err(error) => {
                self.stop_state = NativeStopState::TimedOut;
                self.notice = Some(format!(
                    "native pane: could not start runtime stop: {error}"
                ));
            }
        }
    }

    /// Issue #490 (N21 item B): drains at most one live approval request from
    /// the in-process broker and opens the operator's dialog for it. Never
    /// blocks, and never replaces a dialog that is already open -- the prompt
    /// behind that one is still parked on an answer.
    fn poll_live_approval(&mut self, actor: &str) {
        if self.live_approval.is_some() {
            return;
        }
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let Some(prompt) = session.next_approval() else {
            return;
        };
        let request =
            dialog_request_from_broker(prompt.request(), actor, self.session_id.to_string());
        self.live_approval = Some(prompt);
        self.ux.open_live_approval(request);
    }

    /// PR #531 review finding 4: this used to do a full journal replay AND a
    /// full `build_transcript` rebuild on every ~150ms tick regardless of
    /// whether anything changed. The journal exposes no cursor read (a
    /// "replay since sequence N" call), so the replay itself stays
    /// unavoidable -- but the (heavier, allocation-per-item) transcript
    /// rebuild is now skipped whenever `last_sequence` has not moved since
    /// the last one, and the rebuilt view is capped at
    /// [`MAX_TRANSCRIPT_ITEMS`] so a very long session's per-tick cost (and
    /// the pane's own memory) stays flat rather than growing without bound.
    fn refresh_transcript(&mut self) {
        let Ok((_first, last)) = self.journal.sequence_bounds(&self.session_id) else {
            self.replay_failures = self.replay_failures.saturating_add(1);
            return;
        };
        if last == self.conversation.last_sequence {
            return;
        }
        let Ok(conversation) = self.journal.replay(&self.session_id) else {
            // Issue #490 (item 4): a replay failure is a lost connection to
            // the durable record, not a reason to redraw a stale pane
            // silently. Count it; the recovery emits the reconnect notice.
            self.replay_failures = self.replay_failures.saturating_add(1);
            return;
        };
        #[cfg(test)]
        {
            self.journal_payload_reads += 1;
        }
        // Issue #490: a recovered replay is a reconnect the operator should
        // see. Announced BEFORE the watermark check below, because a
        // reconnect that brought no new events is still a reconnect.
        if self.replay_failures > 0 {
            let missed = self.replay_failures;
            self.replay_failures = 0;
            self.ux.notices.push(super::native_ux::notice_reconnect(
                u64::from(missed) * 150 / 1000,
                conversation.last_sequence.0,
                0,
            ));
        }
        let before = self.transcript.items.len();
        self.recorded_usage = conversation_usage(&conversation);
        self.transcript =
            cap_transcript_items(build_transcript(&conversation), MAX_TRANSCRIPT_ITEMS);
        self.conversation = conversation;
        compact_retained_conversation(&mut self.conversation);
        let grown = self.transcript.items.len().saturating_sub(before);
        if grown > 0 {
            self.presentation.scroll.on_items_appended(grown);
            let terminal = matches!(
                self.session_state,
                NativeSessionState::Idle
                    | NativeSessionState::Completed
                    | NativeSessionState::Failed
                    | NativeSessionState::Interrupted
            );
            if terminal {
                self.presentation.note_terminal_reached();
            }
        }
    }

    /// Issue #538 (chunk C), decision 3: the live data the native `/context`
    /// (alias `/instructions`) view needs -- read from the journal's own
    /// most recent `ContextCompiled` event (`Journal::latest_event_of_type`,
    /// the same read `zirv ctx sessions show`-style tooling would use), never
    /// a fresh re-derivation from disk. That is deliberate: what shaped the
    /// live session is exactly what was recorded when it compiled, which can
    /// disagree with "what would `resolve_active_scope_instructions` say
    /// right now" if a file changed again since. Returns `(rows, context_
    /// version, found)`; `found` is `false` when nothing has compiled this
    /// session yet (a fresh pane before its first turn).
    fn context_view_facts(&self) -> (Vec<super::native_ux::ContextViewSource>, String, bool) {
        let Ok(Some(stored)) = self
            .journal
            .latest_event_of_type(&self.session_id, "context_compiled")
        else {
            return (Vec::new(), "none yet".to_string(), false);
        };
        let JournalEvent::ContextCompiled {
            context_version,
            sources,
            ..
        } = stored.event
        else {
            return (Vec::new(), "none yet".to_string(), false);
        };
        let parsed: Vec<ResolvedInstructionSource> =
            serde_json::from_value(sources).unwrap_or_default();
        let rows = parsed
            .into_iter()
            .map(|source| super::native_ux::ContextViewSource {
                path: source.path.display().to_string(),
                trust: match source.trust {
                    SourceTrust::Operator => "operator",
                    _ => "repo-untrusted",
                },
                scope: source.scope,
                // Issue #538 (chunk C): the journal's own `ContextCompiled`
                // provenance does not carry a byte count (only path/scope/
                // trust/decision/sha256) -- see the design note's chunk C
                // section for why this is a deliberate, documented gap
                // rather than a fresh re-read of each file's size.
                bytes: 0,
                sha256: source.sha256,
                decision: source.decision,
            })
            .collect();
        (rows, context_version, true)
    }

    pub fn status_facts(&self) -> StatusFacts {
        StatusFacts {
            model: format!("{}/{}", self.route_model_vendor(), self.route_model_id()),
            route: self.route_label(),
            runtime: self.runtime_label().to_string(),
            billing: self.billing.clone(),
            session_state: self.session_state,
            turn_state: self.turn_state,
            // Issue #490: an approval the journal says is outstanding and
            // the operator has not answered. One fact, one place: the same
            // flag that opens the dialog is the one that makes Enter queue.
            blocked: self.ux.blocked(),
            unread_result: self.presentation.unread,
            notice: self.notice.clone(),
            activity: self.activity_facts(),
            cwd: self.cwd.display().to_string(),
            git_branch: self.git_branch.clone(),
            context_left_pct: self.context_left(),
        }
    }

    /// Operator direction (PR #531 follow-up): the spinner/verb/elapsed/
    /// token/interrupt-hint line for the activity area, or `None` while
    /// idle. The token count is an approximation -- the conversation's
    /// OWN recorded usage so far, not a per-turn count (the journal has no
    /// "usage recorded since this turn started" read), so it only ever
    /// grows across turns rather than resetting at each one; documented in
    /// the design note.
    fn activity_facts(&self) -> Option<ActivityFacts> {
        let started = self.turn_started_at?;
        let tokens = self
            .recorded_usage
            .context_total()
            .saturating_add(self.recorded_usage.output_tokens);
        Some(ActivityFacts {
            elapsed: started.elapsed(),
            tokens,
        })
    }

    pub fn view(&self) -> (&TranscriptView, &NativePresentation) {
        (&self.transcript, &self.presentation)
    }

    pub fn presentation_mut(&mut self) -> &mut NativePresentation {
        &mut self.presentation
    }

    /// Applies one composer action, and -- when it was a `Submit` -- routes
    /// the submitted text per [`classify_submit_intent`]. A blocked/mid-turn
    /// submission is never sent through `InteractiveSession::submit`
    /// (`Queue`) or treated as an approval answer; it is either queued in
    /// the composer's own `queued` list or, mid-turn, written straight to
    /// the journal as steering (`Steer`) -- see `InteractiveSession::submit`
    /// and `write_steering`'s own doc comments for why those are two
    /// different paths.
    pub fn handle_composer_action(&mut self, action: ComposerAction) {
        let outcome = apply_composer_action(&mut self.presentation.composer, action);
        let ComposerOutcome::Submitted(text) = outcome else {
            return;
        };
        // Operator direction (PR #531 follow-up): a `/`-prefixed submission
        // is a pane-local command, never a turn -- see `apply_slash_
        // command`'s own doc comment for which ones actually do something
        // and which are honest stubs. `/status` needs live `StatusFacts`
        // this method alone can produce, so it stays here rather than in
        // that pure helper.
        if text.trim() == "/status" {
            let facts = self.status_facts();
            self.notice = Some(format!(
                "{} \u{b7} {} \u{b7} {}",
                facts.model,
                status_label(classify_status(&facts)),
                facts.billing
            ));
            return;
        }
        // Issue #538 (chunk C), decision 3: `/context`/`/instructions` need
        // this pane's own live journal (`context_view_facts`), so -- same
        // shape of exception as `/status` above -- they are handled here
        // rather than in the pure `apply_slash_command` helper.
        if matches!(text.trim(), "/context" | "/instructions") {
            // `found` doubles as the render's "recompiled" flag: the journal
            // has no cheap way to say "was THIS specific event tied to the
            // most recent turn" without correlating turn ids across records,
            // so `found` (a compile has been recorded at all) is the best
            // available signal -- documented in the design note.
            let (rows, context_version, found) = self.context_view_facts();
            self.notice = Some(super::native_ux::render_context_view(
                &rows,
                &context_version,
                found,
            ));
            return;
        }
        if let Some(notice) = apply_slash_command(&mut self.presentation, &text) {
            if !notice.is_empty() {
                self.notice = Some(notice);
            }
            return;
        }
        let intent = classify_submit_intent(&self.status_facts());
        // Issue #490 (item 4, criterion 4): even an otherwise-sendable
        // submission is refused when this pane no longer answers for the
        // logical seat's CURRENT session -- a rollover that moved the seat on
        // must never let a keystroke land in the retired generation.
        let intent = match intent {
            SubmitIntent::Queue => SubmitIntent::Queue,
            // Review finding 3 (PR #544): an observer pane offers no
            // send/steer at all -- held exactly like a rollover mismatch,
            // never attempted against a controller seat this pane does not
            // hold.
            _other if self.presentation.observer => {
                self.ux.notices.push(super::native_ux::Notice {
                    kind: super::native_ux::NoticeKind::Rollover,
                    headline: "input held: this pane holds no controller seat (observer mode)"
                        .to_string(),
                    detail: Vec::new(),
                    at: 0,
                });
                SubmitIntent::Queue
            }
            other => match super::native_ux::resolve_submit_target(
                &self.continuity,
                &self.current_identity(),
            ) {
                super::native_ux::SubmitTarget::Send { .. } => other,
                super::native_ux::SubmitTarget::Hold { reason } => {
                    self.ux.notices.push(super::native_ux::Notice {
                        kind: super::native_ux::NoticeKind::Rollover,
                        headline: "input held: this pane no longer owns the seat's session"
                            .to_string(),
                        detail: vec![reason],
                        at: 0,
                    });
                    SubmitIntent::Queue
                }
            },
        };
        match intent {
            SubmitIntent::Immediate => {
                self.send_submit(&text);
            }
            SubmitIntent::Steer => {
                let _ = self.write_steering(&text);
            }
            SubmitIntent::Queue => {
                self.presentation.composer.queued.push(QueuedInput {
                    text,
                    steering: false,
                    queued_at_ms: now_ms_u64(),
                });
            }
        }
        self.sync_continuity();
    }

    fn current_identity(&self) -> super::native_ux::SeatIdentity {
        super::native_ux::SeatIdentity {
            short: self.short.clone(),
            session: self.session_id.to_string(),
            generation: self.generation,
        }
    }

    /// Review finding 1 (PR #544): resyncs this pane's OWN identity after
    /// `Continuity::carry_across` retargets the seat it watches. Without
    /// this, `self.session_id`/`self.generation` stayed at their spawn-time
    /// value forever -- `current_identity` kept disagreeing with
    /// `self.continuity.seat` after the FIRST rollover, so
    /// `resolve_submit_target` returned `Hold` on every submit/steer from
    /// then on. Also resets the runtime-link cursor (a new session starts
    /// its own durable event sequence at zero, so the old cursor means
    /// nothing for it) and re-reads the journal for the new session id
    /// immediately, rather than waiting for the next `tick()` to notice a
    /// `last_sequence` that no longer describes this identity at all.
    fn apply_retarget(&mut self, to_session: &str, generation: u64) {
        match JournalSessionId::new(to_session.to_string()) {
            Ok(session_id) => {
                self.session_id = session_id;
                self.generation = generation;
                self.link_cursor = 0;
                self.replay_failures = 0;
                if let Ok(conversation) = self.journal.replay(&self.session_id) {
                    #[cfg(test)]
                    {
                        self.journal_payload_reads += 1;
                    }
                    self.recorded_usage = conversation_usage(&conversation);
                    self.transcript =
                        cap_transcript_items(build_transcript(&conversation), MAX_TRANSCRIPT_ITEMS);
                    self.conversation = conversation;
                    compact_retained_conversation(&mut self.conversation);
                }
            }
            Err(error) => {
                self.notice = Some(format!(
                    "rollover retarget: invalid session id {to_session}: {error}"
                ));
            }
        }
    }

    /// Mirrors the composer/selection/focus/scroll state into the continuity
    /// record, so a rollover or a reconnect carries the operator's actual
    /// draft rather than a stale copy of it.
    fn sync_continuity(&mut self) {
        self.continuity.draft = self.presentation.composer.draft.clone();
        self.continuity.cursor = self.presentation.composer.cursor;
        self.continuity.queued = self.presentation.composer.queued.clone();
        self.continuity.selection = self.presentation.selection;
        self.continuity.scroll = self.presentation.scroll;
    }

    /// Commits one steering input straight to this pane's own journal
    /// handle, bypassing the busy worker thread entirely -- see `runtime::
    /// native::InteractiveSession::submit`'s own doc comment for why a
    /// turn already in flight cannot be reached through that channel, and
    /// `runtime::native::NativeLoop::queued_input`'s doc comment for how the
    /// running turn picks this up between requests without either side
    /// coordinating directly.
    fn write_steering(&mut self, text: &str) -> CtxResult<()> {
        // Issue #490: a runtime-owned conversation is steered through the
        // service that owns it, never by a second writer on its journal --
        // the runtime is the one supervisor, and `session.send_input` is the
        // documented way in.
        if self.link.is_some() {
            self.send_submit(text);
            return Ok(());
        }
        let message_id = MessageId::new(format!("steer-{}", uuid::Uuid::new_v4().simple()))?;
        self.journal.acknowledge_input(
            &self.session_id,
            self.generation,
            &EventScope::default(),
            message_id,
            text.to_string(),
            true,
            None,
            now_secs(),
        )?;
        Ok(())
    }

    pub fn interrupt(&mut self) {
        // Review finding 3 (PR #544): an observer pane holds no controller
        // seat, so an interrupt would only be refused by the server -- say
        // so directly rather than making the round trip.
        if self.link.is_some() && self.presentation.observer {
            self.notice = Some(
                "interrupt unavailable: this pane holds no controller seat (observer mode)"
                    .to_string(),
            );
            return;
        }
        match (self.link.as_mut(), self.session.as_ref()) {
            (Some(link), _) => {
                let session_id = self.session_id.to_string();
                // Review finding 4 (PR #544): a refused interrupt used to be
                // silently swallowed. Surface it exactly like `send_submit`/
                // `decide_approval` do -- an operator who pressed Esc and saw
                // nothing happen has no way to tell "refused" from "still in
                // flight" otherwise.
                if let Err(error) = link.interrupt(&session_id) {
                    self.notice = Some(format!("interrupt refused by the runtime: {error}"));
                }
            }
            (None, Some(session)) => session.interrupt(),
            (None, None) => {}
        }
        // Issue #490 (N21 item B): an interrupt cancels the tool call that is
        // blocked on the operator too. `InteractiveSession::interrupt` has
        // already cancelled the gate, so dropping the prompt here releases
        // nothing -- it only stops the dashboard from drawing a dialog whose
        // call has already failed closed, and stops a later answer from being
        // delivered to a call that is gone.
        if self.live_approval.take().is_some() {
            let _ = self.ux.close_approval();
            self.notice = Some("the pending approval was cancelled by the interrupt".to_string());
        }
    }

    /// Persists the draft/queued input and releases this pane's hold. For an
    /// in-process pane that stops the worker thread; for a runtime-attached
    /// one it is a `session.detach` -- the session, its journal and its
    /// supervisor are untouched, which is the whole point of the persistent
    /// runtime. Takes `self` by value: there is nothing left to drive.
    pub fn shutdown(mut self, state: &StateDir) {
        let short = self.short.clone();
        persist_draft(
            state,
            &short,
            &PersistedDraft::from_composer(&self.presentation.composer),
        );
        let session_id = self.session_id.to_string();
        if let Some(link) = self.link.as_mut() {
            let _ = link.detach(&session_id);
        }
        if let Some(session) = self.session {
            session.shutdown();
        }
    }

    /// Stops the conversation itself, as distinct from closing the dashboard
    /// and merely detaching from a persistent runtime-owned conversation. An
    /// in-process worker is cancelled here and reaped by later ticks; a
    /// repeated request closes its input channel as the escalation step.
    pub fn stop(&mut self, state: &StateDir) -> CtxResult<()> {
        persist_draft(
            state,
            &self.short,
            &PersistedDraft::from_composer(&self.presentation.composer),
        );
        if self.runtime_stop.is_some() {
            self.stop_state = NativeStopState::Escalated;
            return Ok(());
        }
        if self.link.is_some() {
            self.stop_state = if matches!(self.stop_state, NativeStopState::Active) {
                NativeStopState::Requested(Instant::now())
            } else {
                NativeStopState::Escalated
            };
            self.request_runtime_stop();
            return Ok(());
        }
        if let Some(session) = self.session.as_mut() {
            self.stop_state = match self.stop_state {
                NativeStopState::Active => {
                    session.interrupt();
                    NativeStopState::Requested(Instant::now())
                }
                NativeStopState::Requested(_)
                | NativeStopState::TimedOut
                | NativeStopState::Escalated => {
                    session.request_shutdown();
                    NativeStopState::Escalated
                }
            };
        }
        Ok(())
    }
}

/// Issue #490 (PR #545 review finding 3): a mouse wheel notch over a focused
/// NATIVE pane.
///
/// A native pane has no `vt100` grid and no pty scrollback, so routing the
/// wheel to `Pane::scroll_wheel` moved a buffer that is never rendered while
/// the transcript the operator is actually looking at sat still. This moves
/// the one scroll position that exists -- `NativePresentation::scroll`, the
/// same state `Up`/`Down`/`PageUp`/`PageDown` reach through
/// [`handle_native_key`] -- so the wheel and the keyboard agree.
///
/// Reaching the bottom marks the transcript seen, exactly as `End` does:
/// scrolling back to the live view IS having looked at it.
pub fn wheel_scroll(pane: &mut NativePaneRuntime, delta: isize) -> bool {
    if delta == 0 {
        return false;
    }
    let total = pane.view().0.items.len().max(1);
    let presentation = pane.presentation_mut();
    if delta > 0 {
        presentation.scroll.scroll_up(delta.unsigned_abs(), total);
    } else {
        presentation.scroll.scroll_down(delta.unsigned_abs());
        if presentation.scroll.follow {
            presentation.mark_seen();
        }
    }
    true
}

/// Issue #490 (roadmap N21 item A): #354's clickable overview rows, for a
/// native pane living inside the ordinary dashboard.
///
/// `area` is the pane's own main area, so the panel column is computed
/// against what was actually drawn rather than against the whole terminal --
/// a dashboard with a sidebar would otherwise map every click one panel to
/// the left. A click outside the panel (or on a layout with no panel at all)
/// selects nothing, which is the same "not ours" answer the single-pane loop
/// gave.
pub fn click_overview_row(pane: &mut NativePaneRuntime, area: Rect, column: u16, row: u16) -> bool {
    let width = area.width as usize;
    let height = area.height as usize;
    if !super::native_ux::resolve_layout(width, height).overview {
        return false;
    }
    if column < area.x || row <= area.y {
        return false;
    }
    let panel_x = area.x as usize + width.saturating_sub(super::native_ux::OVERVIEW_WIDTH);
    if (column as usize) < panel_x {
        return false;
    }
    let line = (row - area.y) as usize - 1;
    let Some(id) = pane
        .ux()
        .overview
        .row_at_line(line, super::native_ux::OVERVIEW_WIDTH)
        .map(|row| row.id.clone())
    else {
        return false;
    };
    pane.ux_mut().overview.reselect(&id);
    pane.ux_mut().focus = super::native_ux::Focus::Overview;
    true
}

/// What a key press did to a native pane, from its host loop's point of view.
///
/// Issue #490 (roadmap N21 item A): the key contract lives in ONE function so
/// the single-pane `zirv chat --runtime native` loop and the ordinary
/// dashboard's mixed roster can never drift on what `Esc`, `Ctrl+C`,
/// `Ctrl+R`, `Tab` or a digit means inside a native pane. A wrapped pane
/// never reaches it at all, which is what "native controls are offered only
/// on a native pane" means in practice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeKey {
    /// Handled by the pane. The host loop does nothing else with it.
    Consumed,
    /// The operator asked to close this pane (Ctrl+Q, or a second Ctrl+C
    /// inside [`CTRL_C_QUIT_WINDOW`]).
    Quit,
}

/// The native pane's whole key contract, beyond the composer's own (see
/// [`key_to_action`]).
///
/// `Ctrl+Q` quits immediately (the draft is persisted by the caller's
/// shutdown, kept for backward compatibility); `Esc` interrupts the current
/// turn without quitting -- unless a modal (an approval dialog, a worker
/// inspection, the shortcut list) is open, which the first `Esc` closes;
/// `Ctrl+C` no longer interrupts by itself, it only arms a quit confirmation
/// and quits on a SECOND `Ctrl+C` within [`CTRL_C_QUIT_WINDOW`] (see
/// [`ctrl_c_confirms_quit`]); `Ctrl+R` toggles the most recent tool call's
/// expanded state regardless of focus; `Shift+Tab` cycles [`ComposerMode`];
/// plain `Tab` swaps focus; `Up`/`Down` scroll the transcript when no
/// composer action claims them. Everything else goes through
/// `UxState::handle_key`, which decides between the open modal, the overview,
/// the transcript and the composer.
pub fn handle_native_key(
    pane: &mut NativePaneRuntime,
    key: KeyEvent,
    last_ctrl_c: &mut Option<std::time::Instant>,
    overview_visible: bool,
    persistent: bool,
    cfg: &CtxConfig,
) -> NativeKey {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    if ctrl && key.code == KeyCode::Char('q') {
        return NativeKey::Quit;
    }
    // Operator direction (PR #531 follow-up): `Esc` owns interrupt now;
    // `Ctrl+C` only arms/confirms a quit -- see `ctrl_c_confirms_quit`'s own
    // doc comment.
    //
    // Issue #490 refines only WHEN that applies: while a modal is open (an
    // approval dialog, a worker inspection, the shortcut list) `Esc` closes it
    // first, and only a second `Esc` reaches the turn.
    if key.code == KeyCode::Esc && !pane.ux().modal_open() {
        *last_ctrl_c = None;
        pane.interrupt();
        return NativeKey::Consumed;
    }
    if ctrl && key.code == KeyCode::Char('c') {
        let now = std::time::Instant::now();
        if ctrl_c_confirms_quit(*last_ctrl_c, now, CTRL_C_QUIT_WINDOW) {
            return NativeKey::Quit;
        }
        *last_ctrl_c = Some(now);
        return NativeKey::Consumed;
    }
    *last_ctrl_c = None;
    // `Ctrl+R` toggles the most recent tool call's expanded state regardless
    // of focus -- the same action the composer's own `e`/`Enter`-while-
    // `Transcript`-focused binding below reaches, just reachable from either
    // region (matching the "(ctrl+r to expand)" hint `render_tool_call` shows
    // on a collapsed result).
    if ctrl && key.code == KeyCode::Char('r') {
        toggle_most_recent_tool_call(pane);
        return NativeKey::Consumed;
    }
    // `Shift+Tab` cycles the composer's decorative mode label; most terminals
    // report it as `BackTab` rather than `Tab` with the shift modifier set, so
    // both are accepted.
    if key.code == KeyCode::BackTab || (shift && key.code == KeyCode::Tab) {
        let presentation = pane.presentation_mut();
        presentation.mode = presentation.mode.next();
        return NativeKey::Consumed;
    }
    // Plain Tab swaps which region has focus; every other key's meaning
    // depends on that focus, exactly the split the composer's own key contract
    // already assumes (Up/Down at a logical-line edge mean "browse submit
    // history" only when the composer itself has focus -- a
    // `Transcript`-focused Up/Down here means "scroll").
    if key.code == KeyCode::Tab {
        let presentation = pane.presentation_mut();
        presentation.focus = match presentation.focus {
            PaneFocus::Composer => PaneFocus::Transcript,
            PaneFocus::Transcript => PaneFocus::Composer,
        };
        return NativeKey::Consumed;
    }
    // Issue #490: everything the dashboard's own regions claim -- Tab focus,
    // `?`, `a`, the overview cursor, the open modal -- goes through one
    // router, which also decides whether the key belongs to the composer or
    // the transcript. The pane-global bindings above (Ctrl+Q, Esc, Ctrl+C,
    // Ctrl+R, Shift+Tab) have already had their say and never reach it.
    match pane.ux_mut().handle_key(key, overview_visible) {
        super::native_ux::UxKey::Consumed => return NativeKey::Consumed,
        super::native_ux::UxKey::Quit => return NativeKey::Quit,
        super::native_ux::UxKey::Interrupt => {
            pane.interrupt();
            return NativeKey::Consumed;
        }
        super::native_ux::UxKey::Inspect(id) => {
            pane.open_inspection(&id);
            return NativeKey::Consumed;
        }
        super::native_ux::UxKey::FollowUp(id) => {
            pane.follow_up(&id, cfg, now_secs());
            return NativeKey::Consumed;
        }
        super::native_ux::UxKey::Decided(request, decision) => {
            pane.decide_approval(&request, decision, persistent);
            return NativeKey::Consumed;
        }
        super::native_ux::UxKey::Transcript => {
            let total = pane.view().0.items.len().max(1);
            match key.code {
                KeyCode::Up => pane.presentation_mut().scroll.scroll_up(1, total),
                KeyCode::Down => pane.presentation_mut().scroll.scroll_down(1),
                KeyCode::PageUp => pane.presentation_mut().scroll.scroll_up(10, total),
                KeyCode::PageDown => pane.presentation_mut().scroll.scroll_down(10),
                KeyCode::Home => pane.presentation_mut().scroll.scroll_up(total, total),
                KeyCode::End => {
                    let presentation = pane.presentation_mut();
                    presentation.scroll.jump_to_bottom();
                    presentation.mark_seen();
                }
                // Expands/collapses the most recent tool call -- the same
                // action `Ctrl+R` reaches from any focus, via the one helper.
                KeyCode::Char('e') | KeyCode::Enter => {
                    toggle_most_recent_tool_call(pane);
                }
                _ => {}
            }
        }
        super::native_ux::UxKey::Composer => {
            // Keep the two focus models in step: `UxState` owns the
            // dashboard's focus, `NativePresentation` owns the pane's own
            // composer/transcript split.
            pane.presentation_mut().focus = PaneFocus::Composer;
            if let Some(action) = key_to_action(key) {
                pane.handle_composer_action(action);
            }
        }
    }
    if pane.ux().focus == super::native_ux::Focus::Transcript {
        pane.presentation_mut().focus = PaneFocus::Transcript;
    }
    NativeKey::Consumed
}

/// Issue #490 + N20: opens the pane on whichever transport
/// [`resolve_attach`] selects. Kept separate from [`run_native_dashboard`]
/// so the decision is one small, readable function rather than a branch
/// buried in a terminal-setup sequence.
pub(crate) fn open_native_pane(
    cfg: &CtxConfig,
    state: &StateDir,
    env: EnvLookup<'_>,
    spec: NativeDashboardSpec,
) -> CtxResult<NativePaneRuntime> {
    // Issue #552: a rollover successor never attaches. It is a brand-new
    // conversation taking a seat under a generation that was just committed;
    // attaching to whatever the persistent runtime already holds for this
    // repository would put the OLD conversation back in the seat the
    // rollover just moved.
    if spec.seat.is_some() {
        return NativePaneRuntime::spawn(cfg, state, env, spec);
    }
    let mut link = super::link::RuntimeLink::connect(state, cfg.session.persistent);
    let seat = link.as_mut().and_then(|link| {
        let slug = super::super::state::repo_slug(&spec.repo);
        link.seat_for(&slug, "native").ok().flatten()
    });
    match resolve_attach(link.as_ref(), seat.as_ref()) {
        PaneAttach::Runtime { .. } => {
            let (link, facts) = (link.expect("link"), seat.expect("seat"));
            NativePaneRuntime::attach_runtime(state, link, &facts, spec.repo.clone())
        }
        PaneAttach::InProcess => NativePaneRuntime::spawn(cfg, state, env, spec),
    }
}

// Issue #490 (roadmap N21 item A): the dedicated single-pane loop that used
// to live here is gone. `zirv chat --runtime native` opens its conversation
// as the FIRST PANE of the ordinary dashboard (`dash::run_dashboard`), so
// there is one event loop, one raw-mode/alternate-screen sequence and one key
// contract ([`handle_native_key`]) for wrapped and native panes alike -- and a
// native pane sits in the same roster, mail sweep, attention projection,
// budget sweep and restore roster as every wrapped one. [`open_native_pane`]
// above is what `dash::pane::Pane::spawn_native` calls, so the attachment
// decision ([`resolve_attach`]) is unchanged and still the only one.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::provider::{
        AccountId, BillingPoolId, EndpointId, ModelId, ProviderId, RouteId,
    };
    use crate::commands::ctx::runtime::journal::{
        ExecutionRecord, PolicyProvenance, SequenceId, SessionIdentity, StoredMessage,
        ToolCallRecord,
    };
    use std::collections::BTreeMap;

    // -- fixtures ---------------------------------------------------------

    fn seq(n: u64) -> SequenceId {
        SequenceId(n)
    }

    fn sample_identity() -> SessionIdentity {
        use crate::commands::ctx::provider::Protocol;
        use crate::commands::ctx::runtime::journal::{JournalSessionId, RouteIdentity, SeatId};
        SessionIdentity {
            session: JournalSessionId::new("sess-1").unwrap(),
            seat: SeatId::new("seat-1").unwrap(),
            generation: 1,
            task: None,
            route: RouteIdentity {
                route: RouteId::new("default").unwrap(),
                provider: ProviderId::new("anthropic").unwrap(),
                endpoint: EndpointId::new("anthropic-api").unwrap(),
                account: AccountId::new("acct-1").unwrap(),
                billing_pool: BillingPoolId::new("pool-1").unwrap(),
                protocol: Protocol::AnthropicMessages,
                model: ModelId {
                    vendor: "anthropic".to_string(),
                    id: "claude-opus-4".to_string(),
                },
            },
            repo: std::path::PathBuf::from("/native-test-repo"),
            created_at: 0,
            completed_at: None,
        }
    }

    fn empty_state() -> ConversationState {
        ConversationState {
            identity: sample_identity(),
            last_sequence: seq(0),
            messages: Vec::new(),
            usage: BTreeMap::new(),
            tool_calls: BTreeMap::new(),
            executions: BTreeMap::new(),
            task_receipts: BTreeMap::new(),
            checkpoints: BTreeMap::new(),
            ended_reason: None,
        }
    }

    fn policy() -> PolicyProvenance {
        PolicyProvenance {
            fingerprint: "fp".to_string(),
            source: "test".to_string(),
            decision: "allow".to_string(),
            scope: "repo".to_string(),
        }
    }

    // -- build_transcript ---------------------------------------------------

    #[test]
    fn build_transcript_of_an_empty_conversation_is_empty() {
        assert_eq!(build_transcript(&empty_state()).items, Vec::new());
    }

    #[test]
    fn build_transcript_orders_user_and_assistant_messages_by_sequence() {
        use crate::commands::ctx::runtime::journal::MessageId;
        let mut state = empty_state();
        state.messages.push(StoredMessage {
            sequence: seq(1),
            message_id: MessageId::new("m1").unwrap(),
            role: MessageRole::User,
            blocks: Vec::new(),
            text: Some("hello".to_string()),
            steering: false,
            usage: None,
        });
        state.messages.push(StoredMessage {
            sequence: seq(2),
            message_id: MessageId::new("m2").unwrap(),
            role: MessageRole::Assistant,
            blocks: vec![AssistantBlock::Text {
                text: "hi there".to_string(),
            }],
            text: None,
            steering: false,
            usage: None,
        });
        let view = build_transcript(&state);
        assert_eq!(
            view.items,
            vec![
                TranscriptItem::User {
                    message_id: "m1".to_string(),
                    text: "hello".to_string(),
                    steering: false,
                },
                TranscriptItem::AssistantText {
                    message_id: "m2".to_string(),
                    text: "hi there".to_string(),
                },
            ]
        );
    }

    #[test]
    fn build_transcript_interleaves_a_tool_call_inline_with_its_outcome() {
        use crate::commands::ctx::runtime::journal::{ExecutionId, MessageId};
        let mut state = empty_state();
        let tool_call_id = crate::commands::ctx::runtime::journal::ToolCallId::new("tc1").unwrap();
        state.tool_calls.insert(
            tool_call_id.clone(),
            ToolCallRecord {
                sequence: seq(2),
                name: "run_tests".to_string(),
                arguments: serde_json::json!({"filter": "dash::"}),
                policy: policy(),
            },
        );
        let execution_id = ExecutionId::new("ex1").unwrap();
        state.executions.insert(
            execution_id,
            ExecutionRecord {
                sequence: seq(3),
                tool_call: tool_call_id.clone(),
                state: ExecutionState::Completed,
                result: Some(ContentRef::Inline {
                    text: "3 passed, 0 failed".to_string(),
                }),
                detail: None,
            },
        );
        state.messages.push(StoredMessage {
            sequence: seq(2),
            message_id: MessageId::new("m1").unwrap(),
            role: MessageRole::Assistant,
            blocks: vec![
                AssistantBlock::Text {
                    text: "Running the tests.".to_string(),
                },
                AssistantBlock::ToolCall {
                    tool_call: tool_call_id,
                },
            ],
            text: None,
            steering: false,
            usage: None,
        });
        let view = build_transcript(&state);
        assert_eq!(view.items.len(), 2);
        assert!(matches!(
            view.items[0],
            TranscriptItem::AssistantText { .. }
        ));
        match &view.items[1] {
            TranscriptItem::ToolCall { name, outcome, .. } => {
                assert_eq!(name, "run_tests");
                assert_eq!(
                    *outcome,
                    ToolOutcomeView::TestOutcome {
                        raw: "3 passed, 0 failed".to_string(),
                        passed: Some(3),
                        failed: Some(0),
                    }
                );
            }
            other => panic!("expected a tool call item, got {other:?}"),
        }
    }

    #[test]
    fn build_transcript_appends_session_ended_last() {
        let mut state = empty_state();
        state.ended_reason = Some("turn limit reached".to_string());
        let view = build_transcript(&state);
        assert_eq!(
            view.items,
            vec![TranscriptItem::SessionEnded {
                reason: "turn limit reached".to_string(),
            }]
        );
    }

    /// PR #531 review finding 4: appending events beyond the cap must keep
    /// the displayed view bounded, with the newest items still visible and
    /// an `Elided` marker standing in for however many were dropped.
    #[test]
    fn cap_transcript_items_keeps_the_newest_and_marks_the_rest_elided() {
        let items: Vec<TranscriptItem> = (0..12)
            .map(|i| TranscriptItem::AssistantText {
                message_id: format!("m{i}"),
                text: format!("turn {i}"),
            })
            .collect();
        let capped = cap_transcript_items(TranscriptView { items }, 5);
        assert_eq!(capped.items.len(), 5, "bounded to the cap");
        assert!(matches!(
            capped.items[0],
            TranscriptItem::Elided { hidden: 8 }
        ));
        for (offset, item) in capped.items[1..].iter().enumerate() {
            let expected = 8 + offset;
            match item {
                TranscriptItem::AssistantText { text, .. } => {
                    assert_eq!(text, &format!("turn {expected}"), "newest items kept");
                }
                other => panic!("expected assistant text, got {other:?}"),
            }
        }
    }

    #[test]
    fn cap_transcript_items_is_a_no_op_under_the_cap() {
        let view = TranscriptView {
            items: vec![TranscriptItem::AssistantText {
                message_id: "m0".to_string(),
                text: "hi".to_string(),
            }],
        };
        let capped = cap_transcript_items(view.clone(), MAX_TRANSCRIPT_ITEMS);
        assert_eq!(capped, view);
    }

    #[test]
    fn native_pane_memory_state_stays_bounded_over_long_history() {
        use crate::commands::ctx::runtime::journal::MessageId;

        let mut state = empty_state();
        for index in 0..(MAX_TRANSCRIPT_ITEMS * 4) {
            state.messages.push(StoredMessage {
                sequence: seq(index as u64 + 1),
                message_id: MessageId::new(format!("m-{index}")).unwrap(),
                role: MessageRole::User,
                blocks: Vec::new(),
                text: Some(format!("turn {index}")),
                steering: false,
                usage: None,
            });
        }
        state.last_sequence = seq((MAX_TRANSCRIPT_ITEMS * 4) as u64);
        let view = cap_transcript_items(build_transcript(&state), MAX_TRANSCRIPT_ITEMS);
        compact_retained_conversation(&mut state);

        assert!(state.messages.is_empty());
        assert!(state.tool_calls.is_empty());
        assert!(state.executions.is_empty());
        assert!(state.usage.is_empty());
        assert_eq!(view.items.len(), MAX_TRANSCRIPT_ITEMS);
        assert!(matches!(view.items[0], TranscriptItem::Elided { .. }));
        assert!(matches!(
            view.items.last(),
            Some(TranscriptItem::User { text, .. }) if text == "turn 1999"
        ));
    }

    #[test]
    fn replaying_the_same_journal_events_twice_yields_an_identical_transcript() {
        // Exercises the REAL N03 reducer (`Journal::replay`), not a hand-built
        // fixture: two independent journals fed the identical event sequence
        // must reduce to byte-identical `ConversationState`s, and this
        // module's own reducer on top of that must therefore also agree.
        use crate::commands::ctx::runtime::journal::{EventScope, Journal, MessageId, ToolCallId};

        fn build_one(dir: &std::path::Path, name: &str) -> TranscriptView {
            let mut journal = Journal::open_path(dir.join(name)).unwrap();
            let identity = sample_identity();
            journal.create_session(&identity).unwrap();
            let session = identity.session.clone();
            let scope = EventScope::default();
            journal
                .acknowledge_input(
                    &session,
                    1,
                    &scope,
                    MessageId::new("m-user").unwrap(),
                    "add a retry".to_string(),
                    false,
                    None,
                    1,
                )
                .unwrap();
            journal
                .record_assistant_message(
                    &session,
                    1,
                    &scope,
                    MessageId::new("m-asst").unwrap(),
                    vec![
                        AssistantBlock::Text {
                            text: "Adding a retry.".to_string(),
                        },
                        AssistantBlock::ToolCall {
                            tool_call: ToolCallId::new("tc-1").unwrap(),
                        },
                    ],
                    None,
                    None,
                    2,
                )
                .unwrap();
            journal
                .prepare_tool_call(
                    &session,
                    1,
                    &scope,
                    ToolCallId::new("tc-1").unwrap(),
                    "edit_file".to_string(),
                    serde_json::json!({"path": "src/poll.rs"}),
                    policy(),
                    None,
                    3,
                )
                .unwrap();
            let execution_id =
                crate::commands::ctx::runtime::journal::ExecutionId::new("ex-1").unwrap();
            journal
                .prepare_execution(
                    &session,
                    1,
                    &scope,
                    execution_id.clone(),
                    ToolCallId::new("tc-1").unwrap(),
                    None,
                    4,
                )
                .unwrap();
            journal
                .transition_execution(
                    &session,
                    1,
                    &scope,
                    &execution_id,
                    ExecutionState::Started,
                    None,
                    None,
                    None,
                    5,
                )
                .unwrap();
            journal
                .transition_execution(
                    &session,
                    1,
                    &scope,
                    &execution_id,
                    ExecutionState::Completed,
                    Some(ContentRef::Inline {
                        text: "--- a/src/poll.rs\n+++ b/src/poll.rs\n@@ -1,1 +1,1 @@\n-old\n+new\n"
                            .to_string(),
                    }),
                    None,
                    None,
                    6,
                )
                .unwrap();
            let state = journal.replay(&session).unwrap();
            build_transcript(&state)
        }

        let dir = tempfile::tempdir().unwrap();
        let first = build_one(dir.path(), "a.sqlite");
        let second = build_one(dir.path(), "b.sqlite");
        assert_eq!(first, second);
        // User input, the assistant's text block, then the interleaved tool
        // call -- exactly the three journal events committed above, in order.
        assert_eq!(first.items.len(), 3);
        assert!(matches!(first.items[0], TranscriptItem::User { .. }));
        assert!(matches!(
            first.items[1],
            TranscriptItem::AssistantText { .. }
        ));
        match &first.items[2] {
            TranscriptItem::ToolCall { outcome, .. } => {
                assert!(matches!(outcome, ToolOutcomeView::Diff { .. }));
            }
            other => panic!("expected a tool call item, got {other:?}"),
        }
    }

    // -- classify_outcome ---------------------------------------------------

    fn exec(
        state: ExecutionState,
        result: Option<ContentRef>,
        detail: Option<String>,
    ) -> ExecutionRecord {
        ExecutionRecord {
            sequence: seq(1),
            tool_call: crate::commands::ctx::runtime::journal::ToolCallId::new("tc").unwrap(),
            state,
            result,
            detail,
        }
    }

    #[test]
    fn classify_outcome_with_no_execution_is_pending() {
        assert_eq!(
            classify_outcome("edit_file", None),
            ToolOutcomeView::Pending
        );
    }

    #[test]
    fn classify_outcome_maps_every_execution_state() {
        assert_eq!(
            classify_outcome("t", Some(&exec(ExecutionState::Prepared, None, None))),
            ToolOutcomeView::Pending
        );
        assert_eq!(
            classify_outcome("t", Some(&exec(ExecutionState::Started, None, None))),
            ToolOutcomeView::Running
        );
        assert_eq!(
            classify_outcome("t", Some(&exec(ExecutionState::Cancelled, None, None))),
            ToolOutcomeView::Cancelled
        );
        assert_eq!(
            classify_outcome("t", Some(&exec(ExecutionState::OutcomeUnknown, None, None))),
            ToolOutcomeView::OutcomeUnknown
        );
    }

    #[test]
    fn classify_outcome_failed_prefers_detail_over_result() {
        let outcome = classify_outcome(
            "t",
            Some(&exec(
                ExecutionState::Failed,
                Some(ContentRef::Inline {
                    text: "raw".to_string(),
                }),
                Some("permission denied".to_string()),
            )),
        );
        assert_eq!(
            outcome,
            ToolOutcomeView::Error {
                message: "permission denied".to_string()
            }
        );
    }

    #[test]
    fn classify_outcome_detects_a_unified_diff() {
        let text = "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n".to_string();
        let outcome = classify_outcome(
            "write_file",
            Some(&exec(
                ExecutionState::Completed,
                Some(ContentRef::Inline { text: text.clone() }),
                None,
            )),
        );
        assert_eq!(outcome, ToolOutcomeView::Diff { unified: text });
    }

    #[test]
    fn classify_outcome_requires_both_header_and_hunk_for_a_diff() {
        let text = "--- a/x\n+++ b/x\nno hunk marker here\n".to_string();
        let outcome = classify_outcome(
            "write_file",
            Some(&exec(
                ExecutionState::Completed,
                Some(ContentRef::Inline { text: text.clone() }),
                None,
            )),
        );
        assert_eq!(outcome, ToolOutcomeView::Text { content: text });
    }

    #[test]
    fn classify_outcome_parses_a_cargo_style_test_summary() {
        let text = "test result: FAILED. 11 passed; 1 failed".to_string();
        let outcome = classify_outcome(
            "run_tests",
            Some(&exec(
                ExecutionState::Completed,
                Some(ContentRef::Inline { text: text.clone() }),
                None,
            )),
        );
        assert_eq!(
            outcome,
            ToolOutcomeView::TestOutcome {
                raw: text,
                passed: Some(11),
                failed: Some(1),
            }
        );
    }

    #[test]
    fn classify_outcome_recognizes_an_artifact_regardless_of_tool_name() {
        let outcome = classify_outcome(
            "screenshot",
            Some(&exec(
                ExecutionState::Completed,
                Some(ContentRef::Artifact {
                    sha256: "abc".to_string(),
                    byte_len: 42,
                    content_hash: 1,
                    media_type: "image/png".to_string(),
                }),
                None,
            )),
        );
        assert_eq!(
            outcome,
            ToolOutcomeView::Artifact {
                sha256: "abc".to_string(),
                media_type: "image/png".to_string(),
                byte_len: 42,
            }
        );
    }

    #[test]
    fn classify_outcome_falls_back_to_plain_text() {
        let outcome = classify_outcome(
            "read_file",
            Some(&exec(
                ExecutionState::Completed,
                Some(ContentRef::Inline {
                    text: "fn main() {}".to_string(),
                }),
                None,
            )),
        );
        assert_eq!(
            outcome,
            ToolOutcomeView::Text {
                content: "fn main() {}".to_string()
            }
        );
    }

    // -- classify_status / classify_submit_intent ---------------------------

    // -- issue #490: the bordered composer, the spinner and the entry modes --

    #[test]
    fn the_composer_is_a_bordered_box_with_a_marker_and_a_hint_line() {
        let mut presentation = NativePresentation::default();
        presentation.composer.draft = "keep the shim".to_string();
        let lines = composer_block(
            &presentation,
            &facts(NativeSessionState::Idle, None, false, false),
            60,
        );
        let text: Vec<String> = lines.iter().map(StyledLine::to_plain_string).collect();
        assert!(text[0].starts_with('\u{256d}') && text[0].ends_with('\u{256e}'));
        assert!(text[1].contains("> keep the shim"));
        assert!(text[2].starts_with('\u{2570}'));
        assert!(text[3].contains("? for shortcuts"));
        // The mock's centre column is the composer MODE plus the key that
        // cycles it, not a restatement of what Enter does -- that is only
        // spelled out when Enter does something other than send.
        assert!(text[3].contains("(shift+tab)"), "{:?}", text[3]);
        assert!(
            text[3].contains(ComposerMode::Default.label()),
            "{:?}",
            text[3]
        );
        // Every box row is exactly the pane's width.
        for row in &text[..3] {
            assert_eq!(style::display_width(row), 60, "row {row:?} is not 60 wide");
        }
    }

    #[test]
    fn the_hint_line_says_what_enter_does_right_now() {
        let presentation = NativePresentation::default();
        let steering = composer_block(
            &presentation,
            &facts(NativeSessionState::Running, None, false, false),
            60,
        );
        assert!(
            steering
                .last()
                .expect("hint")
                .to_plain_string()
                .contains("steers")
        );
        let blocked = composer_block(
            &presentation,
            &facts(NativeSessionState::Idle, None, true, false),
            60,
        );
        let hint = blocked.last().expect("hint").to_plain_string();
        assert!(hint.contains("blocked") && hint.contains("queues"));
    }

    #[test]
    fn a_slash_draft_lists_commands_above_the_box() {
        let mut presentation = NativePresentation::default();
        presentation.composer.draft = "/".to_string();
        let text: Vec<String> = composer_block(
            &presentation,
            &facts(NativeSessionState::Idle, None, false, false),
            80,
        )
        .iter()
        .map(StyledLine::to_plain_string)
        .collect();
        assert!(text[0].contains("/clear"));
        assert!(text.iter().any(|line| line.contains("/status")));
        assert!(!text.iter().any(|line| line.contains("/agents")));
    }

    #[test]
    fn a_bang_draft_advertises_no_unwired_process_tool() {
        let mut presentation = NativePresentation::default();
        presentation.composer.draft = "!cargo build".to_string();
        let text = composer_block(
            &presentation,
            &facts(NativeSessionState::Idle, None, false, false),
            80,
        )
        .iter()
        .map(StyledLine::to_plain_string)
        .collect::<Vec<_>>()
        .join("\n");
        assert!(!text.contains("process tool"));
        assert!(
            text.lines()
                .next()
                .is_some_and(|line| line.starts_with('\u{256d}'))
        );
    }

    #[test]
    fn an_at_draft_without_a_workdir_offers_nothing_at_all() {
        let mut presentation = NativePresentation::default();
        presentation.composer.draft = "look at @src".to_string();
        assert!(presentation.workdir.is_none());
        let text: Vec<String> = composer_block(
            &presentation,
            &facts(NativeSessionState::Idle, None, false, false),
            80,
        )
        .iter()
        .map(StyledLine::to_plain_string)
        .collect();
        // Straight to the box: no completion rows.
        assert!(text[0].starts_with('\u{256d}'));
    }

    // -- issue #490 + N20: which transport a native pane attaches through --

    #[test]
    fn a_pane_attaches_in_process_unless_the_runtime_owns_a_live_native_seat() {
        use crate::commands::ctx::api::wire::{SessionFacts, SessionState};
        use crate::commands::ctx::runtime::RuntimeKind;

        let native_seat = |state: SessionState, reachable: bool| {
            let mut facts = SessionFacts::new("sess-1");
            facts.runtime = RuntimeKind::Native;
            facts.state = state;
            facts.reachable = reachable;
            facts.generation = 3;
            facts
        };

        // No link at all (the gate is off, or nothing is listening):
        // in-process, which is a working mode rather than a failure.
        assert_eq!(
            resolve_attach(None, Some(&native_seat(SessionState::Idle, true))),
            PaneAttach::InProcess
        );

        // A link, but a seat this runtime cannot actually serve as a
        // conversation -- a wrapped session, or one it says is unreachable --
        // is never attached to either. (`serves_native` needs a live
        // negotiated client, which `link.rs`'s own tests cover against a real
        // server; the seat half of the rule is what this pins.)
        let mut harness_seat = native_seat(SessionState::Idle, true);
        harness_seat.runtime = RuntimeKind::Harness;
        assert_eq!(
            resolve_attach(None, Some(&harness_seat)),
            PaneAttach::InProcess
        );
        assert_eq!(
            resolve_attach(None, Some(&native_seat(SessionState::Idle, false))),
            PaneAttach::InProcess
        );
        assert_eq!(resolve_attach(None, None), PaneAttach::InProcess);
    }

    #[test]
    fn an_approval_on_a_runtime_owned_session_routes_over_the_protocol() {
        use crate::commands::ctx::dash::native_ux::{ApprovalRoute, approval_route};
        // The pane's own rule: the protocol path is taken only when the
        // operator's gate is on AND this pane actually holds a link. A gate
        // with no link is the in-process broker, never a silent no-op.
        assert_eq!(approval_route(true && true), ApprovalRoute::Protocol);
        assert_eq!(approval_route(true && false), ApprovalRoute::Broker);
        assert_eq!(approval_route(false), ApprovalRoute::Broker);
    }

    /// Review finding 8 (PR #544) removed `ApprovalRequest::from_enforcement`
    /// from `dash::native_ux` as dead code; issue #490's live-approval path
    /// needs the conversion, so it lives here instead -- beside its one
    /// caller, in the module that owns an `enforcement::ApprovalPrompt`. This
    /// is what that move owes: the dialog describes exactly the authority the
    /// grant is signed against, and never a directory the request never
    /// carried.
    #[test]
    fn a_live_dialog_request_carries_the_brokers_own_digest_and_paths() {
        use crate::commands::ctx::runtime::enforcement::{
            ApprovalRequest as BrokerRequest, ExecutionAction, ExecutionIdentity,
        };
        let broker = BrokerRequest {
            scope_digest: "digest-1".to_string(),
            identity: ExecutionIdentity {
                session: "sess-w1".to_string(),
                short: "s7".to_string(),
                generation: 2,
                role: "implementer".to_string(),
                task: Some("T2".to_string()),
            },
            action: ExecutionAction::WriteFile {
                path: PathBuf::from("/repo/wt/src/journal.rs"),
            },
            policy_fingerprint: "pf".to_string(),
            claims_fingerprint: "cf".to_string(),
            resolved_paths: vec![PathBuf::from("/repo/wt/src/journal.rs")],
            execution_scope_fingerprint: "ef".to_string(),
            created_at: 140,
        };
        let request = dialog_request_from_broker(&broker, "w1 implementer", "sess-w1");
        // The id IS the digest the grant is signed against, so answering this
        // dialog can only ever release this exact scope.
        assert_eq!(request.id, "digest-1");
        assert_eq!(request.tool, "Write");
        assert_eq!(request.scope.paths, broker.resolved_paths);
        assert_eq!(request.asked_at, 140);
        assert!(
            request.scope.directory.is_none(),
            "no directory the request never carried"
        );
        // And the dialog built from it offers the session-scoped remember,
        // which names its own scope rather than a tree.
        let dialog = super::super::native_ux::ApprovalDialog::new(request);
        assert!(dialog.session_remember);
        assert!(
            dialog
                .lines(100)
                .iter()
                .map(|line| line.to_plain_string())
                .any(|line| line.contains("/repo/wt/src/journal.rs"))
        );
    }

    // The in-flight spinner/verb/elapsed/interrupt-hint line is the head's
    // `activity_line_text`, already covered by
    // `activity_line_text_carries_real_elapsed_seconds_tokens_and_the_
    // interrupt_hint` and `activity_line_text_is_a_pure_function_of_elapsed_
    // time`; issue #490's own second copy was dropped rather than kept in
    // step with it.

    #[test]
    fn the_composer_box_survives_a_forty_column_pane() {
        let mut presentation = NativePresentation::default();
        presentation.composer.draft =
            "a very long line that has to wrap several times at forty columns".to_string();
        let lines = composer_block(
            &presentation,
            &facts(NativeSessionState::Idle, None, false, false),
            40,
        );
        for line in &lines {
            assert!(
                line.display_width() <= 40,
                "line wider than the pane: {:?}",
                line.to_plain_string()
            );
        }
    }

    fn facts(
        session_state: NativeSessionState,
        turn_state: Option<NativeTurnState>,
        blocked: bool,
        unread: bool,
    ) -> StatusFacts {
        StatusFacts {
            model: "anthropic/claude-opus-4".to_string(),
            route: "default".to_string(),
            runtime: "native".to_string(),
            billing: "api".to_string(),
            session_state,
            turn_state,
            blocked,
            unread_result: unread,
            notice: None,
            activity: None,
            cwd: "/repo".to_string(),
            git_branch: Some("main".to_string()),
            context_left_pct: Some(87),
        }
    }

    #[test]
    fn classify_status_blocked_outranks_every_runtime_state() {
        let f = facts(
            NativeSessionState::Running,
            Some(NativeTurnState::ExecutingTools),
            true,
            false,
        );
        assert_eq!(classify_status(&f), PresentationStatus::Blocked);
    }

    #[test]
    fn classify_status_covers_generating_executing_waiting() {
        assert_eq!(
            classify_status(&facts(
                NativeSessionState::Running,
                Some(NativeTurnState::Requesting),
                false,
                false
            )),
            PresentationStatus::Generating
        );
        assert_eq!(
            classify_status(&facts(
                NativeSessionState::Running,
                Some(NativeTurnState::ExecutingTools),
                false,
                false
            )),
            PresentationStatus::Executing
        );
        assert_eq!(
            classify_status(&facts(
                NativeSessionState::Running,
                Some(NativeTurnState::Pending),
                false,
                false
            )),
            PresentationStatus::Waiting
        );
        assert_eq!(
            classify_status(&facts(NativeSessionState::Idle, None, false, false)),
            PresentationStatus::Waiting
        );
    }

    #[test]
    fn classify_status_covers_cancelled_failed() {
        assert_eq!(
            classify_status(&facts(NativeSessionState::Interrupted, None, false, false)),
            PresentationStatus::Cancelled
        );
        assert_eq!(
            classify_status(&facts(NativeSessionState::Failed, None, false, false)),
            PresentationStatus::Failed
        );
    }

    #[test]
    fn classify_status_completed_with_and_without_unread() {
        assert_eq!(
            classify_status(&facts(NativeSessionState::Completed, None, false, true)),
            PresentationStatus::CompletedUnread
        );
        assert_eq!(
            classify_status(&facts(NativeSessionState::Completed, None, false, false)),
            PresentationStatus::Completed
        );
    }

    #[test]
    fn every_presentation_status_has_a_distinct_single_column_glyph() {
        let all = [
            PresentationStatus::Generating,
            PresentationStatus::Executing,
            PresentationStatus::Waiting,
            PresentationStatus::Blocked,
            PresentationStatus::Cancelled,
            PresentationStatus::Failed,
            PresentationStatus::CompletedUnread,
            PresentationStatus::Completed,
        ];
        let glyphs: std::collections::HashSet<&str> =
            all.iter().copied().map(status_glyph).collect();
        assert_eq!(glyphs.len(), all.len());
        for status in all {
            assert_eq!(style::display_width(status_glyph(status)), 1);
            assert!(!status_label(status).is_empty());
        }
    }

    #[test]
    fn submit_intent_never_queues_as_an_approval_and_is_queue_while_blocked() {
        let blocked = facts(NativeSessionState::Idle, None, true, false);
        assert_eq!(classify_submit_intent(&blocked), SubmitIntent::Queue);
        let idle = facts(NativeSessionState::Idle, None, false, false);
        assert_eq!(classify_submit_intent(&idle), SubmitIntent::Immediate);
        let running = facts(
            NativeSessionState::Running,
            Some(NativeTurnState::Requesting),
            false,
            false,
        );
        assert_eq!(classify_submit_intent(&running), SubmitIntent::Steer);
    }

    // -- composer -------------------------------------------------------

    #[test]
    fn insert_and_submit_round_trips_and_clears_the_draft() {
        let mut state = ComposerState::default();
        for c in "hello".chars() {
            apply_composer_action(&mut state, ComposerAction::Insert(c));
        }
        assert_eq!(state.draft, "hello");
        assert_eq!(state.cursor, 5);
        let outcome = apply_composer_action(&mut state, ComposerAction::Submit);
        assert_eq!(outcome, ComposerOutcome::Submitted("hello".to_string()));
        assert_eq!(state.draft, "");
        assert_eq!(state.history, vec!["hello".to_string()]);
    }

    #[test]
    fn submitting_an_empty_draft_is_a_no_op() {
        let mut state = ComposerState::default();
        assert_eq!(
            apply_composer_action(&mut state, ComposerAction::Submit),
            ComposerOutcome::Unchanged
        );
        assert!(state.history.is_empty());
    }

    #[test]
    fn enter_submits_shift_enter_and_alt_enter_insert_a_newline() {
        let plain = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(key_to_action(plain), Some(ComposerAction::Submit));
        let shift = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(key_to_action(shift), Some(ComposerAction::Newline));
        let alt = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        assert_eq!(key_to_action(alt), Some(ComposerAction::Newline));
    }

    #[test]
    fn newline_then_backspace_removes_the_break_not_a_whole_word() {
        let mut state = ComposerState::default();
        apply_composer_action(
            &mut state,
            ComposerAction::InsertText("first line".to_string()),
        );
        apply_composer_action(&mut state, ComposerAction::Newline);
        apply_composer_action(&mut state, ComposerAction::InsertText("second".to_string()));
        assert_eq!(state.draft, "first line\nsecond");
        apply_composer_action(&mut state, ComposerAction::Backspace);
        apply_composer_action(&mut state, ComposerAction::Backspace);
        apply_composer_action(&mut state, ComposerAction::Backspace);
        apply_composer_action(&mut state, ComposerAction::Backspace);
        apply_composer_action(&mut state, ComposerAction::Backspace);
        apply_composer_action(&mut state, ComposerAction::Backspace);
        assert_eq!(state.draft, "first line\n");
    }

    #[test]
    fn up_at_the_first_line_browses_history_not_the_cursor() {
        let mut state = ComposerState {
            history: vec!["second submit".to_string(), "first submit".to_string()],
            ..Default::default()
        };
        apply_composer_action(&mut state, ComposerAction::MoveUp);
        assert_eq!(state.draft, "second submit");
        apply_composer_action(&mut state, ComposerAction::MoveUp);
        assert_eq!(state.draft, "first submit");
        // Walking past the oldest entry stays put rather than wrapping.
        apply_composer_action(&mut state, ComposerAction::MoveUp);
        assert_eq!(state.draft, "first submit");
    }

    #[test]
    fn down_past_the_newest_history_entry_restores_the_stashed_live_draft() {
        let mut state = ComposerState {
            history: vec!["only submit".to_string()],
            ..Default::default()
        };
        apply_composer_action(
            &mut state,
            ComposerAction::InsertText("unsent draft".to_string()),
        );
        apply_composer_action(&mut state, ComposerAction::MoveUp);
        assert_eq!(state.draft, "only submit");
        apply_composer_action(&mut state, ComposerAction::MoveDown);
        assert_eq!(state.draft, "unsent draft");
    }

    #[test]
    fn up_in_the_middle_of_a_multiline_draft_moves_the_cursor_not_history() {
        let mut state = ComposerState {
            history: vec!["should not appear".to_string()],
            ..Default::default()
        };
        apply_composer_action(
            &mut state,
            ComposerAction::InsertText("line one\nline two".to_string()),
        );
        // Cursor is at the end of "line two" (last line): Up should move
        // within the draft, not browse history, because it is not the FIRST
        // line.
        apply_composer_action(&mut state, ComposerAction::MoveUp);
        assert_eq!(state.draft, "line one\nline two");
    }

    #[test]
    fn insert_text_normalizes_crlf_and_bare_cr_from_a_windows_paste() {
        let mut state = ComposerState::default();
        apply_composer_action(
            &mut state,
            ComposerAction::InsertText("a\r\nb\rc".to_string()),
        );
        assert_eq!(state.draft, "a\nb\nc");
    }

    #[test]
    fn a_large_paste_inserts_as_one_block_at_the_cursor() {
        let mut state = ComposerState::default();
        let big: String = "x".repeat(10_000);
        apply_composer_action(&mut state, ComposerAction::InsertText(big.clone()));
        assert_eq!(state.draft, big);
        assert_eq!(state.cursor, big.len());
        // A paste never fires history browsing or a submit -- one action, one change.
        assert!(state.history.is_empty());
    }

    #[test]
    fn coalesce_paste_chunks_joins_only_chunks_within_the_gap() {
        let gap = Duration::from_millis(30);
        let chunks = vec![
            ("ab".to_string(), Duration::from_millis(0)),
            ("cd".to_string(), Duration::from_millis(5)),
            ("ef".to_string(), Duration::from_millis(200)),
        ];
        let out = coalesce_paste_chunks(&chunks, gap);
        assert_eq!(out, vec!["abcd".to_string(), "ef".to_string()]);
    }

    #[test]
    fn resolve_file_refs_finds_existing_and_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.rs"), b"fn main() {}").unwrap();
        let text = "fix @real.rs and also @missing.rs please";
        let refs = resolve_file_refs(text, dir.path());
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].path, "real.rs");
        assert!(refs[0].exists);
        assert_eq!(refs[1].path, "missing.rs");
        assert!(!refs[1].exists);
    }

    #[test]
    fn resolve_file_refs_finds_a_unicode_path() {
        let dir = tempfile::tempdir().unwrap();
        let text = "see @src/\u{65e5}\u{672c}\u{8a9e}.rs";
        let refs = resolve_file_refs(text, dir.path());
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, "src/\u{65e5}\u{672c}\u{8a9e}.rs");
    }

    /// PR #531 review finding 2: a `@path` that escapes the workdir via `..`
    /// must never be trusted as "exists", even when it genuinely resolves to
    /// a real file outside the tree -- and an ordinary in-tree path must
    /// still resolve.
    #[test]
    fn resolve_file_refs_refuses_a_path_that_escapes_the_workdir() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"top secret").unwrap();

        let workdir = root.path().join("work");
        std::fs::create_dir_all(workdir.join("src")).unwrap();
        std::fs::write(workdir.join("src/lib.rs"), b"fn lib() {}").unwrap();

        let text = "@../outside/secret and @src/lib.rs";
        let refs = resolve_file_refs(text, &workdir);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].path, "../outside/secret");
        assert!(
            !refs[0].exists,
            "a path that escapes the workdir must never resolve"
        );
        assert_eq!(refs[1].path, "src/lib.rs");
        assert!(refs[1].exists, "an ordinary in-tree path must resolve");
    }

    // -- scroll / follow mode --------------------------------------------

    #[test]
    fn scroll_up_disengages_follow_and_scroll_to_bottom_reengages_it() {
        let mut scroll = ScrollState::default();
        assert!(scroll.follow);
        scroll.scroll_up(3, 10);
        assert!(!scroll.follow);
        assert_eq!(scroll.items_back, 3);
        scroll.scroll_down(3);
        assert!(scroll.follow);
        assert_eq!(scroll.items_back, 0);
    }

    #[test]
    fn scroll_up_clamps_to_the_oldest_item() {
        let mut scroll = ScrollState::default();
        scroll.scroll_up(100, 5);
        assert_eq!(scroll.items_back, 4);
    }

    #[test]
    fn appending_items_while_following_does_not_change_items_back() {
        let mut scroll = ScrollState::default();
        scroll.on_items_appended(5);
        assert_eq!(scroll.items_back, 0);
        assert!(scroll.follow);
    }

    #[test]
    fn a_tool_update_never_force_scrolls_an_operator_who_scrolled_up() {
        // Ten items, scrolled up so the visible window is items [4..=6]
        // (items_back = 3 -> last_visible index 6).
        let mut view = TranscriptView::default();
        for i in 0..10 {
            view.items.push(TranscriptItem::SessionEnded {
                reason: format!("item {i}"),
            });
        }
        let mut presentation = NativePresentation::default();
        presentation.scroll.scroll_up(3, view.items.len());
        let before = render_lines(&view, &presentation);
        let before_last = before.last().unwrap().to_plain_string();
        assert!(before_last.contains("item 6"));

        // Three more items land (a tool update); with `follow` off the
        // window must not slide -- item 6 must still be the last one shown.
        for i in 10..13 {
            view.items.push(TranscriptItem::SessionEnded {
                reason: format!("item {i}"),
            });
        }
        presentation.scroll.on_items_appended(3);
        let after = render_lines(&view, &presentation);
        let after_last = after.last().unwrap().to_plain_string();
        assert!(
            after_last.contains("item 6"),
            "scrolled-up window slid: {after_last:?}"
        );
    }

    #[test]
    fn jump_to_bottom_shows_the_newest_item() {
        let mut view = TranscriptView::default();
        for i in 0..5 {
            view.items.push(TranscriptItem::SessionEnded {
                reason: format!("item {i}"),
            });
        }
        let mut presentation = NativePresentation::default();
        presentation.scroll.scroll_up(4, view.items.len());
        presentation.scroll.jump_to_bottom();
        let lines = render_lines(&view, &presentation);
        assert!(lines.last().unwrap().to_plain_string().contains("item 4"));
    }

    // -- presentation / persistence ---------------------------------------

    #[test]
    fn selection_and_expanded_survive_a_resize_no_op() {
        // A "resize" in this design is just a fresh render call at a new
        // width -- nothing in `NativePresentation` is width-dependent, so
        // proving these fields are untouched by rendering at two different
        // widths IS the resize-survival property.
        let mut presentation = NativePresentation::default();
        presentation.set_selection(1, 3);
        presentation.toggle_expanded("tc-1");
        let view = TranscriptView::default();
        let facts = facts(NativeSessionState::Idle, None, false, false);
        let _ = render_plain(&view, &presentation, &facts, 80);
        let _ = render_plain(&view, &presentation, &facts, 40);
        assert_eq!(presentation.selection, Some((1, 3)));
        assert!(presentation.expanded.contains("tc-1"));
    }

    #[test]
    fn note_terminal_reached_sets_unread_only_when_not_following() {
        let mut presentation = NativePresentation::default();
        presentation.note_terminal_reached();
        assert!(
            !presentation.unread,
            "still following: must not mark unread"
        );

        presentation.scroll.follow = false;
        presentation.note_terminal_reached();
        assert!(presentation.unread);

        presentation.mark_seen();
        assert!(!presentation.unread);
    }

    #[test]
    fn draft_persistence_round_trips_and_tolerates_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let state = super::super::super::state::StateDir::from_root(dir.path().to_path_buf());
        let missing = load_draft(&state, "sess-1");
        assert_eq!(missing, PersistedDraft::default());

        let draft = PersistedDraft {
            draft: "unsent \u{1f389} text".to_string(),
            queued: vec![QueuedInput {
                text: "queued while blocked".to_string(),
                steering: false,
                queued_at_ms: 123,
            }],
        };
        persist_draft(&state, "sess-1", &draft);
        let loaded = load_draft(&state, "sess-1");
        assert_eq!(loaded, draft);
    }

    #[test]
    fn draft_persistence_tolerates_a_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let state = super::super::super::state::StateDir::from_root(dir.path().to_path_buf());
        std::fs::create_dir_all(state.native_panes()).unwrap();
        std::fs::write(state.native_panes().join("sess-1.json"), b"{ not json").unwrap();
        assert_eq!(load_draft(&state, "sess-1"), PersistedDraft::default());
    }

    #[test]
    fn persisted_draft_restores_onto_a_composer_with_cursor_at_the_end() {
        let mut composer = ComposerState::default();
        let draft = PersistedDraft {
            draft: "resume here".to_string(),
            queued: vec![QueuedInput {
                text: "still queued".to_string(),
                steering: true,
                queued_at_ms: 1,
            }],
        };
        draft.restore_onto(&mut composer);
        assert_eq!(composer.draft, "resume here");
        assert_eq!(composer.cursor, composer.draft.len());
        assert_eq!(composer.queued, draft.queued);
    }

    // -- operator direction (PR #531 follow-up): bullet/tree restyle ------

    #[test]
    fn render_item_prefixes_assistant_text_with_the_bullet_marker() {
        let item = TranscriptItem::AssistantText {
            message_id: "m1".to_string(),
            text: "reading the file now".to_string(),
        };
        let lines = render_item(&item, false);
        assert_eq!(lines[0].to_plain_string(), "⏺ reading the file now");
        assert_eq!(lines[0].0[0].tone, Tone::Accent, "the bullet is accented");
    }

    #[test]
    fn render_item_prefixes_user_text_with_a_gt_marker() {
        let item = TranscriptItem::User {
            message_id: "m1".to_string(),
            text: "fix the bug".to_string(),
            steering: false,
        };
        let lines = render_item(&item, false);
        assert_eq!(lines[0].to_plain_string(), "> fix the bug");
    }

    #[test]
    fn render_tool_call_shows_a_collapsed_tree_line_with_the_expand_hint() {
        let item = TranscriptItem::ToolCall {
            tool_call_id: "tc1".to_string(),
            message_id: "m1".to_string(),
            name: "read_file".to_string(),
            arguments_preview: "{\"path\":\"src/lib.rs\"}".to_string(),
            outcome: ToolOutcomeView::Text {
                content: "fn main() {}".to_string(),
            },
        };
        let lines = render_item(&item, false);
        assert_eq!(
            lines[0].to_plain_string(),
            "⏺ read_file({\"path\":\"src/lib.rs\"})"
        );
        let tree = lines[1].to_plain_string();
        assert!(tree.starts_with("  ⎿ "), "got {tree:?}");
        assert!(
            tree.ends_with("(ctrl+r to expand)"),
            "a collapsed result with more to show carries the hint: {tree:?}"
        );
        assert_eq!(
            lines.len(),
            2,
            "collapsed shows only the header and tree line"
        );
    }

    #[test]
    fn render_tool_call_drops_the_hint_and_shows_the_body_once_expanded() {
        let item = TranscriptItem::ToolCall {
            tool_call_id: "tc1".to_string(),
            message_id: "m1".to_string(),
            name: "read_file".to_string(),
            arguments_preview: String::new(),
            outcome: ToolOutcomeView::Text {
                content: "fn main() {}".to_string(),
            },
        };
        let lines = render_item(&item, true);
        assert!(
            !lines[1].to_plain_string().contains("ctrl+r"),
            "an expanded result no longer offers to expand it: {:?}",
            lines[1].to_plain_string()
        );
        assert!(
            lines
                .iter()
                .any(|line| line.to_plain_string().contains("fn main()")),
            "the full body is shown once expanded"
        );
    }

    #[test]
    fn render_tool_call_offers_no_hint_for_a_pending_or_running_outcome() {
        for outcome in [ToolOutcomeView::Pending, ToolOutcomeView::Running] {
            let item = TranscriptItem::ToolCall {
                tool_call_id: "tc1".to_string(),
                message_id: "m1".to_string(),
                name: "run_tests".to_string(),
                arguments_preview: String::new(),
                outcome,
            };
            let lines = render_item(&item, false);
            assert!(
                !lines[1].to_plain_string().contains("ctrl+r"),
                "nothing more to show yet: {:?}",
                lines[1].to_plain_string()
            );
        }
    }

    #[test]
    fn render_diff_lines_numbers_old_and_new_lines_and_colours_added_removed_rows() {
        let unified = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -10,3 +10,4 @@\n context\n-old line\n+new line\n+another new line\n";
        let lines = render_diff_lines(unified);
        // header, ---, +++, @@, context(10/10), -(10), +(11), +(12)
        let texts: Vec<String> = lines.iter().map(StyledLine::to_plain_string).collect();
        assert!(texts[2].contains("@@ -10,3 +10,4 @@"), "{texts:?}");
        let context = &texts[3];
        assert!(
            context.contains("10") && context.contains("context"),
            "{context:?}"
        );
        let removed = &texts[4];
        // The hunk's first (context) line is old/new line 10; the removed
        // line that follows is the NEXT old line, 11 -- it consumes no new
        // line number at all.
        assert!(
            removed.contains("11") && removed.contains("- old line"),
            "{removed:?}"
        );
        assert_eq!(lines[4].0[0].tone, Tone::Err);
        let added = &texts[5];
        assert!(
            added.contains("11") && added.contains("+ new line"),
            "{added:?}"
        );
        assert_eq!(lines[5].0[0].tone, Tone::Ok);
        let added2 = &texts[6];
        assert!(
            added2.contains("12") && added2.contains("+ another new line"),
            "{added2:?}"
        );
    }

    // -- operator direction (PR #531 follow-up): activity line, status ----
    // -- bar, key contract, slash commands ---------------------------------

    #[test]
    fn activity_line_text_carries_real_elapsed_seconds_tokens_and_the_interrupt_hint() {
        // The mock's own reading: `(esc to interrupt · <elapsed> · ↓ <tokens>)`,
        // with a minute-aware elapsed and a `k`-scaled token count.
        let text = activity_line_text(std::time::Duration::from_secs(12), 1_234, 120);
        assert!(
            text.contains("(esc to interrupt \u{b7} 12s \u{b7} \u{2193} 1.2k tokens)"),
            "{text:?}"
        );
        let longer = activity_line_text(std::time::Duration::from_secs(72), 420, 120);
        assert!(longer.contains("1m 12s"), "{longer:?}");
        assert!(longer.contains("\u{2193} 420 tokens"), "{longer:?}");
    }

    #[test]
    fn activity_line_text_is_a_pure_function_of_elapsed_time() {
        let a = activity_line_text(std::time::Duration::from_millis(500), 0, 120);
        let b = activity_line_text(std::time::Duration::from_millis(500), 0, 120);
        assert_eq!(a, b);
    }

    #[test]
    fn status_line_text_shows_context_left_cwd_and_git_branch() {
        let mut f = facts(NativeSessionState::Idle, None, false, false);
        f.context_left_pct = Some(42);
        f.cwd = "/repo/zirv".to_string();
        f.git_branch = Some("native/480".to_string());
        let text = status_line_text(&f);
        assert!(text.contains("context left 42%"), "{text:?}");
        assert!(text.contains("/repo/zirv"), "{text:?}");
        assert!(text.contains("(native/480)"), "{text:?}");
    }

    #[test]
    fn status_line_text_omits_unknown_context_and_branch_rather_than_guessing() {
        let mut f = facts(NativeSessionState::Idle, None, false, false);
        f.context_left_pct = None;
        f.git_branch = None;
        let text = status_line_text(&f);
        assert!(!text.contains("context left"), "{text:?}");
        assert!(!text.contains('('), "{text:?}");
    }

    #[test]
    fn context_left_pct_estimates_from_recorded_usage_against_the_declared_window() {
        use crate::commands::ctx::runtime::journal::{UsageId, UsageRecord};

        let route = sample_identity().route;
        let mut state = empty_state();
        let window =
            super::super::super::provider::capability::declared(route.protocol, &route.model, None)
                .context_window
                .expect("a real vendor/model pair declares a context window");
        state.usage.insert(
            UsageId::new("u1").unwrap(),
            UsageRecord {
                id: UsageId::new("u1").unwrap(),
                input_tokens: window / 4,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                output_tokens: 0,
                reasoning_tokens: None,
                provider_request_id: None,
                estimated: false,
            },
        );
        let pct = context_left_pct(&route, &conversation_usage(&state)).expect("declared window");
        assert_eq!(pct, 75, "a quarter of the window used leaves 75% free");
    }

    #[test]
    fn git_branch_reads_the_checked_out_branch_from_dot_git_head() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/native/480\n").unwrap();
        assert_eq!(git_branch(dir.path()), Some("native/480".to_string()));
    }

    #[test]
    fn git_branch_is_none_for_a_detached_head_or_a_non_git_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(git_branch(dir.path()), None, "not a git checkout at all");

        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(
            dir.path().join(".git/HEAD"),
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n",
        )
        .unwrap();
        assert_eq!(
            git_branch(dir.path()),
            None,
            "a detached HEAD names no branch"
        );
    }

    #[test]
    fn ctrl_c_confirms_quit_only_within_the_window_of_a_prior_press() {
        let t0 = std::time::Instant::now();
        assert!(
            !ctrl_c_confirms_quit(None, t0, CTRL_C_QUIT_WINDOW),
            "a first press never confirms by itself"
        );
        let soon = t0 + std::time::Duration::from_millis(500);
        assert!(
            ctrl_c_confirms_quit(Some(t0), soon, CTRL_C_QUIT_WINDOW),
            "a second press inside the window confirms"
        );
        let late = t0 + CTRL_C_QUIT_WINDOW + std::time::Duration::from_secs(1);
        assert!(
            !ctrl_c_confirms_quit(Some(t0), late, CTRL_C_QUIT_WINDOW),
            "a second press outside the window does not confirm"
        );
    }

    #[test]
    fn composer_mode_cycles_through_all_three_and_back() {
        let m = ComposerMode::default();
        assert_eq!(m, ComposerMode::Default);
        let m = m.next();
        assert_eq!(m, ComposerMode::AcceptEdits);
        let m = m.next();
        assert_eq!(m, ComposerMode::Plan);
        let m = m.next();
        assert_eq!(m, ComposerMode::Default, "the cycle wraps back around");
    }

    #[test]
    fn composer_hint_line_shows_shortcuts_hint_mode_and_queued_count() {
        let mut presentation = NativePresentation::default();
        let hint = composer_hint_line(&presentation);
        assert!(hint.contains("? for shortcuts"), "{hint:?}");
        assert!(hint.contains(ComposerMode::Default.label()), "{hint:?}");
        assert!(!hint.contains("queued"), "nothing queued yet: {hint:?}");

        presentation.composer.queued.push(QueuedInput {
            text: "later".to_string(),
            steering: false,
            queued_at_ms: 0,
        });
        presentation.mode = ComposerMode::Plan;
        let hint = composer_hint_line(&presentation);
        assert!(hint.contains("1 queued"), "{hint:?}");
        assert!(hint.contains(ComposerMode::Plan.label()), "{hint:?}");
    }

    #[test]
    fn apply_slash_command_clear_drops_the_queued_backlog() {
        let mut presentation = NativePresentation::default();
        presentation.composer.queued.push(QueuedInput {
            text: "later".to_string(),
            steering: false,
            queued_at_ms: 0,
        });
        let notice = apply_slash_command(&mut presentation, "/clear");
        assert_eq!(notice, Some(String::new()));
        assert!(presentation.composer.queued.is_empty());
    }

    #[test]
    fn apply_slash_command_help_names_the_key_contract() {
        let mut presentation = NativePresentation::default();
        let notice = apply_slash_command(&mut presentation, "/help").expect("recognised");
        assert!(notice.contains("Esc interrupt"), "{notice:?}");
    }

    #[test]
    fn apply_slash_command_ignores_unrecognised_and_ordinary_text() {
        let mut presentation = NativePresentation::default();
        assert_eq!(apply_slash_command(&mut presentation, "/nope"), None);
        assert_eq!(
            apply_slash_command(&mut presentation, "not a command"),
            None
        );
    }

    // -- markdown / wrapping -------------------------------------------

    #[test]
    fn markdown_lines_renders_a_heading_as_emphasis() {
        let lines = markdown_lines("## Plan");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].to_plain_string(), "Plan");
        assert_eq!(lines[0].0[0].tone, Tone::Emphasis);
    }

    #[test]
    fn markdown_lines_renders_list_items_with_a_bullet() {
        let lines = markdown_lines("- first\n* second\n3. third");
        assert_eq!(lines[0].to_plain_string(), "\u{2022} first");
        assert_eq!(lines[1].to_plain_string(), "\u{2022} second");
        assert_eq!(lines[2].to_plain_string(), "\u{2022} third");
    }

    #[test]
    fn markdown_lines_dims_a_fenced_code_block_without_treating_it_as_a_heading() {
        let lines = markdown_lines("```rust\n# not a heading\n```");
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert_eq!(line.0[0].tone, Tone::Muted);
        }
        assert_eq!(lines[1].to_plain_string(), "# not a heading");
    }

    #[test]
    fn markdown_lines_splits_inline_code_into_its_own_span() {
        let lines = markdown_lines("run `cargo test` now");
        let line = &lines[0];
        assert!(
            line.0
                .iter()
                .any(|s| s.text == "cargo test" && s.tone == Tone::Accent)
        );
    }

    #[test]
    fn wrap_line_respects_cjk_double_width_columns() {
        let line = StyledLine::plain("\u{65e5}\u{672c}\u{8a9e}test");
        let wrapped = wrap_line(&line, 6);
        for w in &wrapped {
            assert!(w.display_width() <= 6, "{w:?} exceeds width 6");
        }
        assert_eq!(
            wrapped
                .iter()
                .map(|l| l.to_plain_string())
                .collect::<String>(),
            "\u{65e5}\u{672c}\u{8a9e}test"
        );
    }

    #[test]
    fn wrap_line_never_splits_an_emoji() {
        let line = StyledLine::plain("hello \u{1f389} party");
        for width in 1..20 {
            let wrapped = wrap_line(&line, width);
            // Bounded, not exact: there is no legal way to split a single
            // double-width codepoint, so a pane narrower than that glyph
            // (width 1) overflows by at most one wide glyph's worth of
            // columns -- but never more, and it must still terminate (this
            // test's own regression: an earlier version of `wrap_line`
            // spun forever / OOMed at width 1 rather than making progress).
            for w in &wrapped {
                assert!(
                    w.display_width() <= width + 2,
                    "{w:?} exceeds width {width} by more than one wide glyph"
                );
            }
            let rejoined: String = wrapped.iter().map(|l| l.to_plain_string()).collect();
            assert!(rejoined.contains('\u{1f389}'));
        }
    }

    #[test]
    fn wrap_line_at_narrow_40_column_width_never_exceeds_it() {
        let line = StyledLine::plain(
            "This is a fairly long sentence that should wrap cleanly across several forty-column lines without losing any words.",
        );
        let wrapped = wrap_line(&line, 40);
        for w in &wrapped {
            assert!(w.display_width() <= 40, "{w:?} exceeds 40 columns");
        }
        let rejoined: String = wrapped
            .iter()
            .map(|l| l.to_plain_string())
            .collect::<Vec<_>>()
            .join(" ");
        for word in line.to_plain_string().split_whitespace() {
            assert!(rejoined.contains(word), "lost word {word:?} while wrapping");
        }
    }

    #[test]
    fn wrap_line_hard_splits_a_token_wider_than_the_width() {
        let line = StyledLine::plain("a".repeat(100));
        let wrapped = wrap_line(&line, 10);
        assert!(wrapped.len() >= 10);
        for w in &wrapped {
            assert!(w.display_width() <= 10);
        }
    }

    #[test]
    fn wrap_line_at_zero_width_does_not_loop_forever() {
        let line = StyledLine::plain("anything");
        let wrapped = wrap_line(&line, 0);
        assert_eq!(wrapped.len(), 1);
    }

    #[test]
    fn render_plain_is_stable_across_two_calls_with_the_same_inputs() {
        let mut view = TranscriptView::default();
        view.items.push(TranscriptItem::AssistantText {
            message_id: "m1".to_string(),
            text: "## Done\nAll tests pass.".to_string(),
        });
        let presentation = NativePresentation::default();
        let facts = facts(NativeSessionState::Idle, None, false, false);
        let a = render_plain(&view, &presentation, &facts, 80);
        let b = render_plain(&view, &presentation, &facts, 80);
        assert_eq!(a, b);
        assert!(a.contains("Done"));
        assert!(a.contains("All tests pass."));
    }

    #[test]
    fn render_plain_never_panics_at_a_zero_width_pane() {
        let view = TranscriptView::default();
        let presentation = NativePresentation::default();
        let facts = facts(NativeSessionState::Idle, None, false, false);
        let _ = render_plain(&view, &presentation, &facts, 0);
    }

    #[test]
    fn a_zero_sized_dashboard_main_area_does_not_make_an_approval_actionable() {
        let mut ux = super::super::native_ux::UxState::default();
        ux.open_live_approval(super::super::native_ux::ApprovalRequest {
            id: "approval-1".to_string(),
            session: "session-1".to_string(),
            tool: "Write".to_string(),
            scope: super::super::native_ux::Scope {
                verb: "write".to_string(),
                paths: vec![PathBuf::from("src/lib.rs")],
                directory: None,
            },
            actor: "worker".to_string(),
            reason: "policy".to_string(),
            preview: Vec::new(),
            asked_at: 0,
        });
        let backend = ratatui::backend::TestBackend::new(1, 1);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        let mut approval_rendered = false;
        terminal
            .draw(|frame| {
                approval_rendered = render_native_dashboard(
                    frame,
                    Rect::new(0, 0, 0, 0),
                    &TranscriptView::default(),
                    &NativePresentation::default(),
                    &facts(NativeSessionState::Running, None, true, false),
                    &ux,
                );
            })
            .expect("draw");
        if approval_rendered {
            ux.mark_approval_visible();
        }

        assert_eq!(
            ux.handle_key(KeyEvent::from(KeyCode::Enter), false),
            super::super::native_ux::UxKey::Consumed,
            "an Enter queued before an undrawn approval must not authorize it"
        );
        assert!(ux.approval.is_some());
    }

    #[test]
    fn status_line_text_shows_model_route_runtime_billing_and_state() {
        let f = facts(
            NativeSessionState::Running,
            Some(NativeTurnState::Requesting),
            false,
            false,
        );
        let text = status_line_text(&f);
        assert!(text.contains("anthropic/claude-opus-4"));
        assert!(text.contains("default"));
        assert!(text.contains("native"));
        assert!(text.contains("api"));
        assert!(text.contains("generating"));
    }

    /// PR #531 review finding 5: `spawn_interactive` used to swallow a
    /// standing-context compile failure with `.unwrap_or_default()` --
    /// silently, with no trace an operator could see. Once surfaced through
    /// `InteractiveProgress::Notice`, the pane's status line must show it.
    #[test]
    fn status_line_text_shows_a_notice_when_one_is_set() {
        let mut f = facts(NativeSessionState::Idle, None, false, false);
        f.notice = Some("standing context could not be compiled".to_string());
        let text = status_line_text(&f);
        assert!(
            text.contains("standing context could not be compiled"),
            "got {text}"
        );
    }

    // =====================================================================
    // PR #544 review findings 1, 2, 6, 7: a rollover must resync THIS
    // pane's own identity, not just the continuity record, or every
    // send/steer/follow-up path stays held forever after the first
    // generation change.
    // =====================================================================

    fn identity_for(session: &str, generation: u64) -> SessionIdentity {
        SessionIdentity {
            session: crate::commands::ctx::runtime::journal::JournalSessionId::new(session)
                .unwrap(),
            generation,
            ..sample_identity()
        }
    }

    /// A minimal, directly-constructed pane for testing the rollover/
    /// identity plumbing in isolation -- no spawned process and (unless a
    /// link is passed) no runtime link, so this exercises exactly the bug
    /// findings 1/2/6/7 named rather than any process or network machinery.
    fn pane_fixture(
        state: &StateDir,
        short: &str,
        session: &str,
        generation: u64,
        link: Option<super::super::link::RuntimeLink>,
    ) -> NativePaneRuntime {
        let journal = Journal::open(state).expect("open journal");
        let session_id = JournalSessionId::new(session).expect("session id");
        let continuity =
            super::super::native_ux::Continuity::new(super::super::native_ux::SeatIdentity {
                short: short.to_string(),
                session: session.to_string(),
                generation,
            });
        NativePaneRuntime {
            short: short.to_string(),
            session_id,
            generation,
            route: None,
            attach: PaneAttach::InProcess,
            link,
            runtime_stop: None,
            link_cursor: 0,
            idempotency_seq: 0,
            live_approval: None,
            journal_payload_reads: 0,
            session: None,
            journal,
            presentation: NativePresentation::default(),
            conversation: empty_state(),
            recorded_usage: Default::default(),
            transcript: TranscriptView::default(),
            session_state: NativeSessionState::Idle,
            turn_state: None,
            billing: style::PLACEHOLDER.to_string(),
            ux: super::super::native_ux::UxState::default(),
            repo: PathBuf::from("."),
            state: state.clone(),
            continuity,
            replay_failures: 0,
            announced_recoveries: 0,
            announced_rollover_at: 0,
            announced_terminal: BTreeSet::new(),
            ended: false,
            notice: None,
            turn_started_at: None,
            stop_state: NativeStopState::Active,
            cwd: PathBuf::from("."),
            git_branch: None,
        }
    }

    /// Issue #538 (chunk C), decision 3: `/context` renders real rows from
    /// the journal's own recorded provenance -- a repo `ZIRV.md` (`Included`)
    /// and a same-content `AGENTS.md` (`Duplicate`, per chunk A's dedup
    /// rule), both present with their decisions, not an empty placeholder.
    #[test]
    fn context_view_renders_both_a_zirv_md_and_a_shadowed_agents_md_row() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let repo = tempfile::tempdir().expect("repo");
        std::fs::write(repo.path().join("ZIRV.md"), "- always run tests\n").expect("write");
        std::fs::write(repo.path().join("AGENTS.md"), "- always run tests\n").expect("write");

        let mut pane = pane_fixture(&state, "s1", "sess-1", 1, None);
        pane.repo = repo.path().to_path_buf();
        pane.journal
            .create_session(&identity_for("sess-1", 1))
            .expect("create session");

        let sources = super::super::runtime::context::resolve_active_scope_instructions(
            repo.path(),
            None,
            &[],
            1_000_000,
        );
        pane.journal
            .record_context_compiled(
                &pane.session_id,
                pane.generation,
                &EventScope::default(),
                "test-context-version".to_string(),
                serde_json::to_value(&sources).expect("serialize sources"),
                1_000,
            )
            .expect("record context compiled");

        pane.presentation.composer.draft = "/context".to_string();
        pane.presentation.composer.cursor = pane.presentation.composer.draft.len();
        pane.handle_composer_action(ComposerAction::Submit);

        let notice = pane.notice.clone().expect("a /context notice was set");
        assert!(notice.contains("test-context-version"), "{notice}");
        assert!(notice.contains("ZIRV.md"), "{notice}");
        assert!(notice.contains("AGENTS.md"), "{notice}");
        assert!(notice.contains("included"), "{notice}");
        assert!(
            notice.contains("duplicate of"),
            "the identical AGENTS.md must show its chunk A decision: {notice}"
        );
    }

    #[test]
    fn native_draft_queues_mail_and_nudge_until_submission() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let mut pane = pane_fixture(&state, "s1", "sess-1", 1, None);
        let draft = "half-typed operator draft \u{1f642}";
        pane.presentation.composer.draft = draft.to_string();
        pane.presentation.composer.cursor = pane.presentation.composer.draft.len();

        assert!(pane.deliver("mail", "message").is_err());
        assert!(pane.deliver("nudge", "direction").is_err());
        assert_eq!(
            pane.presentation.composer.draft.as_bytes(),
            draft.as_bytes()
        );

        pane.handle_composer_action(ComposerAction::Submit);
        pane.deliver("mail", "message")
            .expect("mail after submission");
        pane.deliver("nudge", "direction")
            .expect("nudge after submission");
        assert!(
            pane.presentation
                .composer
                .history
                .iter()
                .any(|entry| entry == "[mail] message")
        );
        assert!(
            pane.presentation
                .composer
                .history
                .iter()
                .any(|entry| entry == "[nudge] direction")
        );
    }

    #[test]
    fn idle_native_pane_reads_no_unchanged_journal_payload() {
        use crate::commands::ctx::runtime::journal::{EventScope, MessageId};

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let identity = identity_for("sess-idle", 1);
        let mut writer = Journal::open(&state).expect("journal");
        writer.create_session(&identity).expect("session");
        writer
            .acknowledge_input(
                &identity.session,
                1,
                &EventScope::default(),
                MessageId::new("m-1").unwrap(),
                "new output".to_string(),
                false,
                None,
                1,
            )
            .expect("event");

        let mut pane = pane_fixture(&state, "s1", "sess-idle", 1, None);
        pane.refresh_transcript();
        assert_eq!(pane.journal_payload_reads, 1);
        for _ in 0..20 {
            pane.refresh_transcript();
            let _ = render_plain(
                &pane.transcript,
                &pane.presentation,
                &pane.status_facts(),
                80,
            );
        }
        assert_eq!(
            pane.journal_payload_reads, 1,
            "unchanged ticks use only the constant-size sequence watermark query"
        );
    }

    #[test]
    fn a_rollover_resyncs_the_panes_own_identity_so_every_send_path_targets_the_new_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let cfg = CtxConfig::default();
        let mut pane = pane_fixture(&state, "s1", "sess-old", 1, None);

        // What `refresh_records` observed: the seat's logical identity moved
        // on. `Continuity::carry_across` (tested directly in native_ux.rs)
        // already retargets `continuity.seat`; the bug finding 1 names is
        // that nothing resynced the PANE's own `session_id`/`generation` to
        // match it.
        let next = super::super::native_ux::SeatIdentity {
            short: "s1".to_string(),
            session: "sess-new".to_string(),
            generation: 2,
        };
        let (to_session, generation) = match pane.continuity.carry_across(next) {
            super::super::native_ux::Retarget::Retargeted {
                to_session,
                generation,
                ..
            } => (to_session, generation),
            super::super::native_ux::Retarget::Unchanged => panic!("expected a retarget"),
        };

        // Findings 1 + 7, "before": with the pane's own fields still stale,
        // a composer submit is held -- never sent to the retired
        // `sess-old` -- and a denial's steering guidance is held too.
        pane.presentation.composer.draft = "before the retarget".to_string();
        pane.presentation.composer.cursor = pane.presentation.composer.draft.len();
        pane.handle_composer_action(ComposerAction::Submit);
        assert_eq!(
            pane.presentation.composer.queued.len(),
            1,
            "held, not sent to the retired session"
        );
        assert_eq!(
            pane.presentation.composer.queued[0].text,
            "before the retarget"
        );

        let request = super::super::native_ux::ApprovalRequest {
            id: "tc-1".to_string(),
            session: "sess-old".to_string(),
            tool: "Bash".to_string(),
            scope: super::super::native_ux::Scope {
                verb: "run".to_string(),
                paths: Vec::new(),
                directory: None,
            },
            actor: "s1 \u{b7} orchestrator".to_string(),
            reason: "policy".to_string(),
            preview: Vec::new(),
            asked_at: 0,
        };
        pane.decide_approval(
            &request,
            super::super::native_ux::ApprovalDecision::Deny,
            false,
        );
        assert_eq!(
            pane.presentation.composer.queued.len(),
            2,
            "finding 7: a denial's steering is held too, not written into the retired session"
        );
        assert!(pane.presentation.composer.queued[1].steering);

        // Finding 2, "before": a worker follow-up is held with a notice and
        // `delegation::send` is never reached -- the draft is untouched.
        pane.presentation.composer.draft = "please retry with -v".to_string();
        let notices_before = pane.ux.notices.recent(usize::MAX).len();
        pane.follow_up("worker-1", &cfg, 0);
        assert_eq!(
            pane.presentation.composer.draft, "please retry with -v",
            "finding 2: held, so the draft is never cleared"
        );
        assert!(pane.ux.notices.recent(usize::MAX).len() > notices_before);

        // Apply the fix under test.
        pane.apply_retarget(&to_session, generation);
        assert_eq!(pane.session_id.to_string(), "sess-new");
        assert_eq!(pane.generation, 2);
        assert_eq!(
            super::super::native_ux::resolve_submit_target(
                &pane.continuity,
                &pane.current_identity()
            ),
            super::super::native_ux::SubmitTarget::Send {
                session: "sess-new".to_string(),
                generation: 2,
            },
            "finding 1: the next submit targets the new session and is not held"
        );

        // "After": a composer submit is no longer held (still just the one
        // item queued before the fix).
        pane.presentation.composer.draft = "after the retarget".to_string();
        pane.presentation.composer.cursor = pane.presentation.composer.draft.len();
        pane.handle_composer_action(ComposerAction::Submit);
        assert_eq!(
            pane.presentation.composer.queued.len(),
            2,
            "sent, not queued, once the pane's own identity is resynced"
        );

        // "After": the follow-up guard no longer holds it -- it reaches
        // `delegation::send` (which fails for lack of an on-disk record,
        // distinct from being held by this pane's own guard).
        pane.presentation.composer.draft = "please retry with -v again".to_string();
        pane.follow_up("worker-1", &cfg, 0);
        assert_eq!(
            pane.presentation.composer.draft, "",
            "finding 2: no longer held, so the draft is cleared once delegation::send is attempted"
        );
    }

    #[test]
    fn two_submits_in_the_same_tick_get_distinct_idempotency_keys() {
        // Finding 6: `short-{now_ms}` alone can collide within the same
        // millisecond. The per-pane counter makes two keys minted back to
        // back distinct regardless of the clock's resolution.
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let mut pane = pane_fixture(&state, "s1", "sess-1", 1, None);
        let first = pane.next_idempotency_key();
        let second = pane.next_idempotency_key();
        assert_ne!(first, second);
    }

    // =====================================================================
    // PR #544 review findings 3, 4: `attach_runtime` must actually call
    // `RuntimeLink::attach`, fall back to observer mode when it is refused,
    // and never swallow a refused interrupt.
    // =====================================================================

    fn wire_facts(
        id: &str,
        short: &str,
        runtime: crate::commands::ctx::runtime::RuntimeKind,
    ) -> crate::commands::ctx::api::wire::SessionFacts {
        crate::commands::ctx::api::wire::SessionFacts {
            session_id: id.to_string(),
            short: short.to_string(),
            runtime,
            generation: 1,
            surface: crate::commands::ctx::runtime::UiSurface::Headless,
            state: crate::commands::ctx::api::wire::SessionState::Idle,
            role: Some("orchestrator".to_string()),
            agent: Some("claude".to_string()),
            repo_slug: Some("zirv-cli".to_string()),
            started_at: Some(1_757_000_000),
            reachable: true,
        }
    }

    /// A minimal `SessionHost` double: enough to prove `attach_runtime`
    /// actually calls `RuntimeLink::attach` and registers as controller,
    /// without a real pty. Every other method is unreachable by these
    /// tests and refuses cleanly rather than panicking if that ever
    /// changes.
    #[derive(Debug, Default)]
    struct FakeHost {
        inner: std::sync::Mutex<FakeHostState>,
    }

    #[derive(Debug, Default)]
    struct FakeHostState {
        facts: Vec<crate::commands::ctx::api::wire::SessionFacts>,
        controller: Option<String>,
        stopped: bool,
    }

    impl FakeHost {
        fn with(facts: Vec<crate::commands::ctx::api::wire::SessionFacts>) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                inner: std::sync::Mutex::new(FakeHostState {
                    facts,
                    controller: None,
                    stopped: false,
                }),
            })
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, FakeHostState> {
            self.inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }
    }

    impl crate::commands::ctx::api::server::SessionHost for FakeHost {
        fn sessions(&self) -> Vec<crate::commands::ctx::api::wire::SessionFacts> {
            self.lock().facts.clone()
        }

        fn start(
            &self,
            _spec: &crate::commands::ctx::runtime::SessionSpec,
        ) -> Result<
            crate::commands::ctx::api::wire::SessionFacts,
            crate::commands::ctx::api::wire::ApiError,
        > {
            Err(crate::commands::ctx::api::wire::ApiError::new(
                crate::commands::ctx::api::wire::ErrorCode::Unsupported,
                "the test host opens no terminals",
            ))
        }

        fn attach(
            &self,
            _session_id: &str,
            client_id: &str,
            mode: crate::commands::ctx::api::wire::AttachMode,
            _size: Option<(u16, u16)>,
        ) -> Result<
            crate::commands::ctx::api::wire::Attachment,
            crate::commands::ctx::api::wire::ApiError,
        > {
            let mut state = self.lock();
            if mode == crate::commands::ctx::api::wire::AttachMode::Controller {
                state.controller = Some(client_id.to_string());
            }
            let role = if state.controller.as_deref() == Some(client_id) {
                crate::commands::ctx::api::wire::AttachRole::Controller
            } else {
                crate::commands::ctx::api::wire::AttachRole::Observer
            };
            Ok(crate::commands::ctx::api::wire::Attachment {
                controller: state.controller.clone(),
                clients: vec![client_id.to_string()],
                rows: 24,
                cols: 80,
                role,
            })
        }

        fn detach(
            &self,
            _session_id: &str,
            client_id: &str,
        ) -> Result<
            crate::commands::ctx::api::wire::Attachment,
            crate::commands::ctx::api::wire::ApiError,
        > {
            let mut state = self.lock();
            if state.controller.as_deref() == Some(client_id) {
                state.controller = None;
            }
            Ok(crate::commands::ctx::api::wire::Attachment {
                controller: state.controller.clone(),
                clients: Vec::new(),
                rows: 24,
                cols: 80,
                role: crate::commands::ctx::api::wire::AttachRole::Detached,
            })
        }

        fn takeover(
            &self,
            _session_id: &str,
            _client_id: &str,
        ) -> Result<
            crate::commands::ctx::api::wire::Attachment,
            crate::commands::ctx::api::wire::ApiError,
        > {
            Err(crate::commands::ctx::api::wire::ApiError::new(
                crate::commands::ctx::api::wire::ErrorCode::Unsupported,
                "not exercised by this test",
            ))
        }

        fn resize(
            &self,
            _session_id: &str,
            _client_id: &str,
            _rows: u16,
            _cols: u16,
        ) -> Result<
            crate::commands::ctx::api::wire::Attachment,
            crate::commands::ctx::api::wire::ApiError,
        > {
            Err(crate::commands::ctx::api::wire::ApiError::new(
                crate::commands::ctx::api::wire::ErrorCode::Unsupported,
                "not exercised by this test",
            ))
        }

        fn screen(
            &self,
            _session_id: &str,
            _client_id: &str,
        ) -> Result<
            crate::commands::ctx::api::wire::ScreenView,
            crate::commands::ctx::api::wire::ApiError,
        > {
            Err(crate::commands::ctx::api::wire::ApiError::new(
                crate::commands::ctx::api::wire::ErrorCode::Unsupported,
                "not exercised by this test",
            ))
        }

        fn write_raw(
            &self,
            _session_id: &str,
            _client_id: &str,
            _bytes: &[u8],
        ) -> Result<(), crate::commands::ctx::api::wire::ApiError> {
            Err(crate::commands::ctx::api::wire::ApiError::new(
                crate::commands::ctx::api::wire::ErrorCode::Unsupported,
                "not exercised by this test",
            ))
        }

        fn stop(
            &self,
            _session_id: &str,
        ) -> Result<bool, crate::commands::ctx::api::wire::ApiError> {
            self.lock().stopped = true;
            Ok(true)
        }
    }

    #[test]
    fn dashboard_stop_terminates_native_pane_session() {
        use crate::commands::ctx::provider::adapter::Cancellation;

        let tmp = tempfile::tempdir().expect("tempdir");
        let state_dir = tmp.path().join("state");
        let state = StateDir::from_root(state_dir.clone());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo");
        let env = std::collections::HashMap::from([(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state_dir.display().to_string(),
        )]);
        let lookup = |key: &str| env.get(key).cloned();
        let provider = format!(
            "fixture:{}",
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/runtime/native/helper-answer.json")
                .display()
        );
        let mut pane = NativePaneRuntime::spawn(
            &CtxConfig::default(),
            &state,
            &lookup,
            NativeDashboardSpec {
                repo,
                role: "worker".to_string(),
                route: None,
                writing: true,
                provider: Some(provider),
                seat: None,
                initial_input: None,
            },
        )
        .expect("spawn native pane");
        let session_id = pane.session_id.clone();
        let cancellation = pane
            .session
            .as_ref()
            .expect("in-process session")
            .cancellation_flag();

        pane.stop(&state).expect("Stop reports success");

        assert!(cancellation.is_cancelled());
        assert!(
            pane.session.is_some(),
            "stop retains the worker until a tick reaps it"
        );
        assert!(
            !pane.ended,
            "requesting stop is not proof that the worker ended"
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pane.ended && Instant::now() < deadline {
            pane.tick();
            std::thread::yield_now();
        }
        assert!(
            Journal::open(&state)
                .expect("journal")
                .replay(&session_id)
                .expect("replay")
                .ended_reason
                .is_some()
        );
        assert!(pane.ended);
        assert!(pane.session.is_none(), "the terminated worker was reaped");
        assert_eq!(pane.session_state, NativeSessionState::Interrupted);
    }

    #[test]
    fn dashboard_stop_times_out_without_retiring_a_non_finishing_worker_and_escalates() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state_dir = tmp.path().join("state");
        let state = StateDir::from_root(state_dir.clone());
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("repo");
        let env = std::collections::HashMap::from([(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state_dir.display().to_string(),
        )]);
        let lookup = |key: &str| env.get(key).cloned();
        let provider = format!(
            "fixture:{}",
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/runtime/native/helper-answer.json")
                .display()
        );
        let mut pane = NativePaneRuntime::spawn(
            &CtxConfig::default(),
            &state,
            &lookup,
            NativeDashboardSpec {
                repo,
                role: "worker".to_string(),
                route: None,
                writing: true,
                provider: Some(provider),
                seat: None,
                initial_input: None,
            },
        )
        .expect("spawn native pane");
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            let _ = blocked.recv();
        });
        assert!(
            pane.session
                .as_mut()
                .expect("in-process session")
                .replace_worker_for_test(worker),
            "replace the fixture worker with the deliberately blocked one"
        );

        let started = Instant::now();
        pane.stop(&state).expect("request stop");
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "Stop must not wait for the worker"
        );
        assert!(matches!(pane.stop_state, NativeStopState::Requested(_)));
        assert!(pane.session.is_some());
        assert!(!pane.ended);

        pane.stop_state = NativeStopState::Requested(Instant::now() - STOP_REAP_TIMEOUT);
        pane.tick();
        assert_eq!(pane.stop_state, NativeStopState::TimedOut);
        assert!(pane.session.is_some(), "the timed-out worker remains owned");
        assert!(!pane.ended, "timeout is not termination confirmation");

        pane.stop(&state).expect("escalate stop");
        assert_eq!(pane.stop_state, NativeStopState::Escalated);
        assert!(pane.session.is_some());
        assert!(!pane.ended);

        release.send(()).expect("release blocked worker");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pane.ended && Instant::now() < deadline {
            pane.tick();
            std::thread::yield_now();
        }
        assert!(pane.ended, "the released worker is reaped on a later tick");
    }

    #[test]
    fn dashboard_stop_sends_session_stop_over_a_runtime_link() {
        use crate::commands::ctx::api::server::{ApiServer, RunningServer, StaticSource};
        use crate::commands::ctx::runtime::RuntimeKind;

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let facts = wire_facts("session-stop", "stop1", RuntimeKind::Native);
        let mut setup_journal = Journal::open(&state).expect("open journal");
        setup_journal
            .create_session(&identity_for(&facts.session_id, facts.generation))
            .expect("create session");

        let host = FakeHost::with(vec![facts.clone()]);
        let endpoint = crate::commands::ctx::api::server::endpoint_for(&state);
        let server = ApiServer::new(Box::new(StaticSource(vec![facts.clone()])), None);
        server.attach_host(std::sync::Arc::clone(&host)
            as std::sync::Arc<dyn crate::commands::ctx::api::server::SessionHost>);
        let running =
            RunningServer::start(&endpoint, std::sync::Arc::clone(&server)).expect("start");
        let link = super::super::link::RuntimeLink::connect(&state, true).expect("runtime link");
        let mut pane = NativePaneRuntime::attach_runtime(&state, link, &facts, PathBuf::from("."))
            .expect("attach runtime pane");

        pane.stop(&state).expect("runtime stop");

        assert!(!pane.ended, "the UI tick has not observed confirmation yet");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pane.ended && Instant::now() < deadline {
            pane.tick();
            std::thread::yield_now();
        }
        assert!(host.lock().stopped, "session.stop reached the runtime host");
        assert!(
            pane.ended,
            "the attached pane ends after runtime confirmation"
        );
        assert_eq!(pane.session_state, NativeSessionState::Interrupted);
        pane.shutdown(&state);
        drop(running);
    }

    #[test]
    fn attach_runtime_falls_back_to_observer_mode_when_the_attach_is_refused() {
        use crate::commands::ctx::api::server::{ApiServer, RunningServer, StaticSource};
        use crate::commands::ctx::runtime::RuntimeKind;

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let facts = wire_facts("session-a", "s1", RuntimeKind::Native);

        let mut setup_journal = Journal::open(&state).expect("open journal");
        setup_journal
            .create_session(&identity_for(&facts.session_id, facts.generation))
            .expect("create session");

        let endpoint = crate::commands::ctx::api::server::endpoint_for(&state);
        let server = ApiServer::new(Box::new(StaticSource(vec![facts.clone()])), None);
        let running =
            RunningServer::start(&endpoint, std::sync::Arc::clone(&server)).expect("start");

        let link =
            super::super::link::RuntimeLink::connect(&state, true).expect("a runtime is listening");

        // Review finding 3 (PR #544): `attach_runtime` used to never call
        // `RuntimeLink::attach()` at all -- submit/interrupt/approve relied
        // entirely on the server's "nobody attached yet" bypass. This bare
        // server owns no terminal host, so the attach this pane now makes
        // is refused, and the pane must fall back to a read-only observer
        // rather than silently keep acting as if it held the controller
        // seat.
        let mut pane = NativePaneRuntime::attach_runtime(&state, link, &facts, PathBuf::from("."))
            .expect("attach_runtime");

        assert!(
            pane.presentation.observer,
            "a refused attach falls back to observer mode"
        );
        assert!(
            pane.notice
                .as_deref()
                .is_some_and(|text| text.contains("attach refused")),
            "the refusal is surfaced as a notice: {:?}",
            pane.notice
        );
        assert!(
            composer_hint_line(&pane.presentation).contains("observer"),
            "the composer hint says this pane is read-only"
        );

        // No send is offered: a composer submit is held, never attempted
        // over a link this pane does not control.
        pane.presentation.composer.draft = "hello".to_string();
        pane.presentation.composer.cursor = pane.presentation.composer.draft.len();
        pane.handle_composer_action(ComposerAction::Submit);
        assert_eq!(
            pane.presentation.composer.queued.len(),
            1,
            "held, not sent, in observer mode"
        );

        // No interrupt is attempted either.
        pane.notice = None;
        pane.interrupt();
        assert_eq!(
            pane.notice.as_deref(),
            Some("interrupt unavailable: this pane holds no controller seat (observer mode)"),
            "no network call is attempted in observer mode"
        );

        pane.shutdown(&state);
        drop(running);
    }

    #[test]
    fn attach_runtime_registers_as_controller_and_a_refused_interrupt_is_surfaced() {
        use crate::commands::ctx::api::server::{ApiServer, RunningServer, StaticSource};
        use crate::commands::ctx::runtime::RuntimeKind;

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let facts = wire_facts("session-b", "s2", RuntimeKind::Native);

        let mut setup_journal = Journal::open(&state).expect("open journal");
        setup_journal
            .create_session(&identity_for(&facts.session_id, facts.generation))
            .expect("create session");

        let endpoint = crate::commands::ctx::api::server::endpoint_for(&state);
        let server = ApiServer::new(Box::new(StaticSource(vec![facts.clone()])), None);
        server.attach_host(FakeHost::with(vec![facts.clone()]));
        let running =
            RunningServer::start(&endpoint, std::sync::Arc::clone(&server)).expect("start");

        let link =
            super::super::link::RuntimeLink::connect(&state, true).expect("a runtime is listening");

        // Review finding 3 (PR #544): with a host that CAN honour the
        // attach, this pane registers as the session's controller instead
        // of relying on the server's "nobody attached yet" bypass.
        let mut pane = NativePaneRuntime::attach_runtime(&state, link, &facts, PathBuf::from("."))
            .expect("attach_runtime");
        assert!(
            !pane.presentation.observer,
            "the attach succeeded as controller"
        );
        assert!(
            pane.notice.is_none(),
            "no fallback notice when the attach succeeds: {:?}",
            pane.notice
        );

        // Review finding 4 (PR #544): this server owns no NATIVE
        // conversations (only the generic terminal host above), so
        // `session.interrupt` itself is refused. That refusal used to be
        // swallowed by `let _ = link.interrupt(...)`; it must now reach the
        // operator as a notice instead.
        pane.interrupt();
        assert!(
            pane.notice
                .as_deref()
                .is_some_and(|text| text.contains("interrupt refused by the runtime")),
            "a refused interrupt is surfaced, not swallowed: {:?}",
            pane.notice
        );

        pane.shutdown(&state);
        drop(running);
    }

    // =====================================================================
    // Operator direction (2026-09-14): the regenerated mock
    // `docs/design/mocks/2026-09-13-native-pane.html` is the acceptance
    // target. One snapshot-style test per terminal size the mock draws,
    // pinning its LAYOUT SKELETON -- the composer's box rows, the hint
    // line's three columns and the activity line's exact reading -- from
    // fixed fixture facts, with no clock, no terminal and no session.
    //
    // The skeleton, not the prose: what these assert is that a row that
    // should be a full-width box border IS one at that width, that the hint
    // line's three columns are laid out left/centre/right and never overflow,
    // and that the activity line reads exactly as the mock draws it. A
    // wording change to a verb or a mode label is not a layout regression and
    // is deliberately not pinned here.
    // =====================================================================

    /// Every terminal size the mock draws, narrow floor first.
    const MOCK_WIDTHS: [usize; 4] = [40, 80, 120, 200];

    fn mock_presentation(queued: usize) -> NativePresentation {
        let mut presentation = NativePresentation::default();
        presentation.composer.draft = "keep the old constructor as a deprecated shim".to_string();
        presentation.composer.queued = (0..queued)
            .map(|i| QueuedInput {
                text: format!("queued {i}"),
                steering: false,
                queued_at_ms: i as u64,
            })
            .collect();
        presentation
    }

    fn skeleton(width: usize, queued: usize) -> Vec<String> {
        composer_block(
            &mock_presentation(queued),
            &facts(NativeSessionState::Idle, None, false, false),
            width,
        )
        .iter()
        .map(StyledLine::to_plain_string)
        .collect()
    }

    #[test]
    fn the_composer_box_matches_the_mock_at_every_terminal_size() {
        for width in MOCK_WIDTHS {
            let rows = skeleton(width, 1);
            let top = &rows[0];
            let bottom = &rows[rows.len() - 2];
            assert!(
                top.starts_with('\u{256d}') && top.ends_with('\u{256e}'),
                "{width}: the box opens with the mock's rounded corners: {top:?}"
            );
            assert!(
                bottom.starts_with('\u{2570}') && bottom.ends_with('\u{256f}'),
                "{width}: the box closes with the mock's rounded corners: {bottom:?}"
            );
            for row in &rows[..rows.len() - 1] {
                assert_eq!(
                    style::display_width(row),
                    width,
                    "{width}: a box row is not the full width: {row:?}"
                );
            }
            assert!(
                rows[1].contains("> keep the old constructor"),
                "{width}: the draft row carries the mock's `>` marker: {:?}",
                rows[1]
            );
        }
    }

    #[test]
    fn the_hint_line_keeps_its_three_columns_at_every_terminal_size() {
        for width in MOCK_WIDTHS {
            let rows = skeleton(width, 1);
            let hint = rows.last().expect("hint line").clone();
            assert!(
                style::display_width(&hint) <= width,
                "{width}: the hint line overflows: {hint:?}"
            );
            // Left column, hard left.
            assert!(
                hint.starts_with("  ?"),
                "{width}: the shortcut hint is hard left: {hint:?}"
            );
            // Right column, hard right, and only because something is queued.
            assert!(
                hint.trim_end().ends_with("queued") || hint.trim_end().ends_with("\u{29d7}1"),
                "{width}: the queue count is hard right: {hint:?}"
            );
            // Centre column, between the two and touching neither.
            let centre = ComposerMode::Default.label();
            let centre = centre.split(' ').next().expect("a mode label word");
            let at = hint.find(centre).unwrap_or_else(|| {
                panic!("{width}: the mode is missing from the hint line: {hint:?}")
            });
            assert!(
                at > 3,
                "{width}: the mode column must not touch the left one: {hint:?}"
            );
            // And with nothing queued the right column is absent entirely,
            // never rendered as a zero.
            let empty = skeleton(width, 0);
            let hint = empty.last().expect("hint line");
            assert!(
                !hint.contains('\u{29d7}'),
                "{width}: an empty queue is absent, not `0 queued`: {hint:?}"
            );
        }
    }

    #[test]
    fn the_activity_line_matches_the_mocks_exact_reading() {
        // The mock draws TWO readings: the wide frame
        // `(esc to interrupt · 1m 12s · ↓ 3.4k tokens)` at 80 columns and up,
        // and the narrow floor `(esc · 1m12s)` at 40. Both are pinned here,
        // and at every size the line must FIT rather than wrap -- a wrapped
        // activity line eats a transcript row on every tick and walks the
        // whole conversation up the screen under the operator.
        let elapsed = std::time::Duration::from_secs(72);
        for width in MOCK_WIDTHS {
            let text = activity_line_text(elapsed, 3_400, width);
            assert!(
                style::display_width(&text) <= width,
                "{width}: the activity line wraps instead of narrowing: {text:?}"
            );
            let (_spinner, rest) = text.split_once(' ').expect("a spinner then the verb");
            assert!(
                rest.split('\u{2026}')
                    .next()
                    .is_some_and(|verb| !verb.is_empty() && !verb.contains('(')),
                "{width}: a rotating verb precedes the ellipsis: {text:?}"
            );
            if width >= 80 {
                assert!(
                    rest.ends_with("(esc to interrupt \u{b7} 1m 12s \u{b7} \u{2193} 3.4k tokens)"),
                    "{width}: {text:?}"
                );
            } else {
                assert!(rest.ends_with("(esc \u{b7} 1m12s)"), "{width}: {text:?}");
            }
        }
    }

    /// PR #545 review finding 3: the wheel moves the transcript a native pane
    /// actually renders, not the vt100 scrollback it does not have.
    #[test]
    fn a_wheel_notch_over_a_native_pane_moves_its_own_transcript_scroll() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().to_path_buf());
        let mut pane = pane_fixture(&state, "s1", "sess-1", 1, None);
        // A transcript long enough to have somewhere to scroll back to.
        pane.transcript = TranscriptView {
            items: (0..40)
                .map(|i| TranscriptItem::AssistantText {
                    message_id: format!("m{i}"),
                    text: format!("line {i}"),
                })
                .collect(),
        };
        assert!(pane.view().1.scroll.follow, "a fresh pane follows the tail");

        assert!(wheel_scroll(&mut pane, 3));
        assert!(
            !pane.view().1.scroll.follow,
            "scrolling up disengages auto-follow"
        );
        let back = pane.view().1.scroll.items_back;
        assert!(back > 0, "the wheel moved the transcript: {back}");

        // And back down to the live view, which counts as having seen it.
        assert!(wheel_scroll(&mut pane, -3));
        assert!(pane.view().1.scroll.follow, "the tail is live again");
        assert!(!wheel_scroll(&mut pane, 0), "a zero notch is not a scroll");
    }

    #[test]
    fn the_mock_sizes_all_resolve_a_layout_that_keeps_the_conversation() {
        // The mock's own panel story: 40 columns has neither sidebar nor
        // overview, 80 has neither, 120 has the sidebar, 200 has both -- and
        // the conversation is never starved at any of them.
        let expected = [
            (40, false, false),
            (80, false, false),
            (120, true, false),
            (200, true, true),
        ];
        for (width, sidebar, overview) in expected {
            let plan = super::super::native_ux::resolve_layout(width, 40);
            assert_eq!(plan.sidebar, sidebar, "{width}: sidebar");
            assert_eq!(plan.overview, overview, "{width}: overview");
            assert!(plan.main_width >= 20, "{width}: the pane is never starved");
        }
    }
}
