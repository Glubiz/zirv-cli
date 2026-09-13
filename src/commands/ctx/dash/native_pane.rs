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
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::style::{self, Tone};

use super::super::CtxResult;
use super::super::config::{CtxConfig, EnvLookup};
use super::super::runtime::journal::{
    AssistantBlock, ContentRef, ConversationState, EventScope, ExecutionRecord, ExecutionState,
    Journal, MessageId, MessageRole, RouteIdentity, ToolCallId,
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
#[allow(dead_code)] // see `FileRef`'s own doc comment
pub fn resolve_file_refs(text: &str, workdir: &Path) -> Vec<FileRef> {
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
            let exists = workdir.join(&path).exists();
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

/// Renders one [`TranscriptItem`] as [`StyledLine`]s, collapsed or expanded
/// per `expanded`. Collapsed tool calls show one summary line; expanded
/// ones show the full classified outcome.
pub fn render_item(item: &TranscriptItem, expanded: bool) -> Vec<StyledLine> {
    match item {
        TranscriptItem::User { text, steering, .. } => {
            let mut lines = vec![StyledLine::toned(
                if *steering {
                    "\u{25b8} you (steering)"
                } else {
                    "\u{25b8} you"
                },
                Tone::Muted,
            )];
            lines.extend(markdown_lines(text));
            lines
        }
        TranscriptItem::AssistantText { text, .. } => {
            let mut lines = vec![StyledLine::toned("\u{25b8} assistant", Tone::Accent)];
            lines.extend(markdown_lines(text));
            lines
        }
        TranscriptItem::AssistantThinking { text, .. } => {
            let mut lines = vec![StyledLine::toned("\u{25b8} thinking", Tone::Muted)];
            lines.extend(markdown_lines(text).into_iter().map(|line| {
                StyledLine(
                    line.0
                        .into_iter()
                        .map(|s| StyledSpan {
                            tone: Tone::Muted,
                            ..s
                        })
                        .collect(),
                )
            }));
            lines
        }
        TranscriptItem::AssistantRefusal { text, .. } => {
            vec![
                StyledLine::toned("\u{25b8} assistant (refused)", Tone::Warn),
                StyledLine::toned(text.clone(), Tone::Warn),
            ]
        }
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

fn render_tool_call(
    name: &str,
    arguments_preview: &str,
    outcome: &ToolOutcomeView,
    expanded: bool,
) -> Vec<StyledLine> {
    let (glyph, tone, summary) = outcome_summary(outcome);
    let marker = if expanded { "\u{25bc}" } else { "\u{25b6}" };
    let header = StyledLine(vec![
        StyledSpan {
            text: format!("  {marker} tool: "),
            tone: Tone::Muted,
        },
        StyledSpan {
            text: name.to_string(),
            tone: Tone::Plain,
        },
        StyledSpan {
            text: format!(" {glyph} "),
            tone,
        },
        StyledSpan {
            text: summary,
            tone,
        },
    ]);
    let mut lines = vec![header];
    if !expanded {
        return lines;
    }
    if !arguments_preview.is_empty() {
        lines.push(StyledLine::toned(
            format!("    args: {arguments_preview}"),
            Tone::Muted,
        ));
    }
    lines.extend(render_outcome_body(outcome));
    lines
}

fn render_outcome_body(outcome: &ToolOutcomeView) -> Vec<StyledLine> {
    match outcome {
        ToolOutcomeView::Diff { unified } => unified
            .lines()
            .map(|line| {
                let tone = if line.starts_with('+') && !line.starts_with("+++") {
                    Tone::Ok
                } else if line.starts_with('-') && !line.starts_with("---") {
                    Tone::Err
                } else {
                    Tone::Muted
                };
                StyledLine::toned(format!("    {line}"), tone)
            })
            .collect(),
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
    format!(
        "{model}  {route}  {runtime}  {billing}  {glyph} {label}",
        model = facts.model,
        route = facts.route,
        runtime = facts.runtime,
        billing = facts.billing,
        glyph = status_glyph(status),
        label = status_label(status),
    )
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
    let composer_rows = composer_height(presentation, width).min(area.height.saturating_sub(1));
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
        let raw = render_lines(view, presentation);
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
        let lines = composer_lines(presentation, width);
        let text: Vec<Line> = lines
            .into_iter()
            .map(|line| Line::from(Span::raw(line)))
            .collect();
        f.render_widget(Paragraph::new(text), composer_area);
    }
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
    lines.push(
        "Enter submit \u{b7} Shift+Enter newline \u{b7} \u{2191} history \u{b7} @ file ref"
            .to_string(),
    );
    lines
}

fn composer_height(presentation: &NativePresentation, width: usize) -> u16 {
    (composer_lines(presentation, width).len() as u16).max(2)
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
    let raw = render_lines(view, presentation);
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

/// A live native pane: the one thing in this module that owns a running
/// session. Everything else it holds is either a pure derivation of that
/// session's journal ([`ConversationState`]/[`TranscriptView`], refreshed by
/// [`Self::tick`]) or this module's own already-tested presentation state.
pub struct NativePaneRuntime {
    session: InteractiveSession,
    journal: Journal,
    presentation: NativePresentation,
    conversation: ConversationState,
    transcript: TranscriptView,
    session_state: NativeSessionState,
    turn_state: Option<NativeTurnState>,
    billing: String,
    /// Set once an `InteractiveProgress::Ended` is observed; the dashboard
    /// loop's own cue to stop.
    pub ended: bool,
}

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
            },
            env,
        )?;
        let journal = Journal::open(state)?;
        let conversation = journal.replay(&session.session)?;
        let transcript = build_transcript(&conversation);
        let billing = resolve_billing(&session.route, &spec.repo);

        let mut presentation = NativePresentation::default();
        let draft = load_draft(state, &session.handle.short);
        draft.restore_onto(&mut presentation.composer);

        Ok(Self {
            session,
            journal,
            presentation,
            conversation,
            transcript,
            session_state: NativeSessionState::Idle,
            turn_state: None,
            billing,
            ended: false,
        })
    }

    /// Drains worker progress and re-reads the journal. Called once per
    /// dashboard tick; cheap (a `try_recv` loop plus one SQLite read) so a
    /// short poll interval costs nothing while the session is idle.
    pub fn tick(&mut self) {
        for progress in self.session.drain_progress() {
            match progress {
                InteractiveProgress::Busy => {
                    self.session_state = NativeSessionState::Running;
                    self.turn_state = Some(NativeTurnState::Requesting);
                }
                InteractiveProgress::Idle => {
                    self.session_state = NativeSessionState::Idle;
                    self.turn_state = None;
                }
                InteractiveProgress::Failed(_) => {
                    self.session_state = NativeSessionState::Idle;
                    self.turn_state = None;
                }
                InteractiveProgress::Ended => {
                    self.ended = true;
                    self.session_state = NativeSessionState::Completed;
                }
            }
        }
        self.refresh_transcript();
    }

    fn refresh_transcript(&mut self) {
        let Ok(conversation) = self.journal.replay(&self.session.session) else {
            return;
        };
        let before = self.transcript.items.len();
        self.conversation = conversation;
        self.transcript = build_transcript(&self.conversation);
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

    pub fn status_facts(&self) -> StatusFacts {
        StatusFacts {
            model: format!(
                "{}/{}",
                self.session.route.model.vendor, self.session.route.model.id
            ),
            route: self.session.route.route.to_string(),
            runtime: "native".to_string(),
            billing: self.billing.clone(),
            session_state: self.session_state,
            turn_state: self.turn_state,
            // Issue #480 (deferred, see design note): a live approval-
            // pending signal needs the enforcement broker's own facts,
            // which this pane does not yet read.
            blocked: false,
            unread_result: self.presentation.unread,
        }
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
        match classify_submit_intent(&self.status_facts()) {
            SubmitIntent::Immediate => {
                let _ = self.session.submit(text);
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
    }

    /// Commits one steering input straight to this pane's own journal
    /// handle, bypassing the busy worker thread entirely -- see `runtime::
    /// native::InteractiveSession::submit`'s own doc comment for why a
    /// turn already in flight cannot be reached through that channel, and
    /// `runtime::native::NativeLoop::queued_input`'s doc comment for how the
    /// running turn picks this up between requests without either side
    /// coordinating directly.
    fn write_steering(&mut self, text: &str) -> CtxResult<()> {
        let message_id = MessageId::new(format!("steer-{}", uuid::Uuid::new_v4().simple()))?;
        self.journal.acknowledge_input(
            &self.session.session,
            self.session.handle.generation,
            &EventScope::default(),
            message_id,
            text.to_string(),
            true,
            None,
            now_secs(),
        )?;
        Ok(())
    }

    pub fn interrupt(&self) {
        self.session.interrupt();
    }

    /// Persists the draft/queued input and stops the worker thread. Takes
    /// `self` by value: there is nothing left to drive afterward.
    pub fn shutdown(self, state: &StateDir) {
        let short = self.session.handle.short.clone();
        persist_draft(
            state,
            &short,
            &PersistedDraft::from_composer(&self.presentation.composer),
        );
        self.session.shutdown();
    }
}

/// A dedicated, single-pane dashboard loop for a native session -- `zirv
/// chat --runtime native`'s own entry point. Reuses `dash::mod`'s existing
/// terminal-setup/teardown helpers verbatim (same `install_panic_hook`/
/// `enable_raw_mode`/`EnterAlternateScreen`/`push_keyboard_enhancement`/
/// `teardown_terminal`/`restore_panic_hook` sequence `run_dashboard` itself
/// uses) rather than reimplementing raw-mode handling a second time, so
/// there is exactly one place in this codebase that enters/leaves raw mode
/// and the alternate screen.
///
/// **Key contract**, beyond the composer's own (see [`key_to_action`]):
/// `Ctrl+Q` quits (persisting the draft first); `Ctrl+C` interrupts the
/// current turn without quitting; `Up`/`Down` scroll the transcript when no
/// composer action claims them.
pub fn run_native_dashboard(
    cfg: &CtxConfig,
    state: &StateDir,
    env: EnvLookup<'_>,
    spec: NativeDashboardSpec,
) -> CtxResult<i32> {
    let mut pane = NativePaneRuntime::spawn(cfg, state, env, spec)?;

    let previous_panic_hook = super::install_panic_hook();
    if let Err(error) = crossterm::terminal::enable_raw_mode() {
        super::restore_panic_hook(&previous_panic_hook);
        pane.shutdown(state);
        return Err(format!("native chat: enable_raw_mode failed: {error}").into());
    }
    if let Err(error) = crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen)
    {
        super::teardown_terminal(false);
        super::restore_panic_hook(&previous_panic_hook);
        pane.shutdown(state);
        return Err(format!("native chat: EnterAlternateScreen failed: {error}").into());
    }
    let keyboard_enhancement_pushed = super::push_keyboard_enhancement();
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => {
            super::teardown_terminal(keyboard_enhancement_pushed);
            super::restore_panic_hook(&previous_panic_hook);
            pane.shutdown(state);
            return Err(format!("native chat: terminal init failed: {error}").into());
        }
    };

    let exit_code = 'outer: loop {
        pane.tick();
        let _ = terminal.draw(|f| {
            let facts = pane.status_facts();
            let (view, presentation) = pane.view();
            render_native_pane(f, f.area(), view, presentation, &facts);
        });
        if pane.ended {
            break 'outer 0;
        }
        if matches!(event::poll(Duration::from_millis(150)), Ok(true)) {
            match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    if ctrl && key.code == KeyCode::Char('q') {
                        break 'outer 0;
                    }
                    if ctrl && key.code == KeyCode::Char('c') {
                        pane.interrupt();
                        continue 'outer;
                    }
                    // Tab swaps which region has focus; every other key's
                    // meaning depends on that focus, exactly the split the
                    // composer's own key contract already assumes (Up/Down
                    // at a logical-line edge mean "browse submit history"
                    // only when the composer itself has focus -- a
                    // `Transcript`-focused Up/Down here means "scroll").
                    if key.code == KeyCode::Tab {
                        let presentation = pane.presentation_mut();
                        presentation.focus = match presentation.focus {
                            PaneFocus::Composer => PaneFocus::Transcript,
                            PaneFocus::Transcript => PaneFocus::Composer,
                        };
                        continue 'outer;
                    }
                    if pane.presentation_mut().focus == PaneFocus::Transcript {
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
                            // Expands/collapses the most recent tool call --
                            // a minimal binding until a per-item cursor
                            // exists to target an arbitrary one.
                            KeyCode::Char('e') | KeyCode::Enter => {
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
                            _ => {}
                        }
                    } else if let Some(action) = key_to_action(key) {
                        pane.handle_composer_action(action);
                    }
                }
                Ok(Event::Paste(text)) => {
                    pane.handle_composer_action(ComposerAction::InsertText(text));
                }
                _ => {}
            }
        }
    };

    super::teardown_terminal(keyboard_enhancement_pushed);
    super::restore_panic_hook(&previous_panic_hook);
    pane.shutdown(state);
    Ok(exit_code)
}

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
}
