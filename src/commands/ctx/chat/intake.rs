//! PR3 (issue #799, dash-refresh design doc section "PR3 -- intake plan
//! card"): the harness proxy's intake step as a short, visible plan step
//! instead of a handful of blocking stderr lines.
//!
//! Runs as an inline ratatui region (`Viewport::Inline`) on the **normal**
//! screen -- never the alternate screen -- so it survives the harness's own
//! full-screen start (issue #701's own fix: the harness's alternate screen
//! wiped whatever the old stderr lines had printed). On the plain terminal,
//! before `wrap`/the dashboard ever touch it.
//!
//! Flow: a boxed prompt (Enter submits; Shift+Enter/Alt+Enter/Ctrl+J insert a
//! newline) -> a spinner while [`proxy::decide`] runs on a worker thread (Esc
//! abandons it, the thread's own result is then just ignored) -> at most one
//! round of clarification, only when the decision asks (`CLARIFY_THRESHOLD`,
//! decisively) -> a plan card in plain words with four numbered choices.
//! `Esc` at ANY point starts the harness without a plan, keeping whatever
//! task text was already typed. Every plan card waits for Enter -- no
//! countdown (operator decision, dash-refresh design doc).
//!
//! Kept pure where the design doc asks for it: the key-to-action mappings
//! ([`text_key_action`], [`plan_key_action`]), the plan-card text builder
//! ([`build_plan_card`]/[`plan_sentence`]/[`why_line`]), the fallback notice
//! ([`fallback_notice`]) and the summary line ([`summary_line`]) are all
//! plain functions of a [`ProxyDecision`], unit-tested below without a
//! terminal. Only [`run`] itself (and the per-screen `run_*` loops it calls)
//! touches a real terminal or spawns a thread.

use std::io::{self, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use ratatui::{TerminalOptions, Viewport};
use unicode_width::UnicodeWidthChar;

use crate::commands::ctx::config::CtxConfig;
use crate::commands::ctx::jev::{self, JevEffect};
use crate::commands::ctx::proxy::decision::{ProxyDecision, Roster, SeatRole};
use crate::commands::ctx::proxy::{self, CLARIFY_THRESHOLD};
use crate::commands::ctx::state::StateDir;
use crate::commands::workflow::classify::{Complexity, Intent};

/// The fixed height of the inline region: tall enough for the tallest screen
/// (the orchestrated plan card, with its extra "Helpers" row and the
/// fallback notice above it). Shorter screens simply use fewer of these rows
/// -- top-aligned, the rest left blank -- rather than resizing the region,
/// which ratatui's own `Viewport::Inline` does not support after
/// construction (its height is fixed at `Terminal::with_options` time).
const REGION_HEIGHT: u16 = 18;

/// What the whole intake flow produced.
pub(crate) enum IntakeOutcome {
    /// The operator confirmed a plan card (choice 1, 2 or 3).
    Decided {
        decision: Box<ProxyDecision>,
        request: String,
    },
    /// Esc somewhere in the flow, or nothing ever typed: `request` is
    /// whatever task text existed at that point (`None` only when the
    /// operator pressed Esc at an empty prompt).
    Unplanned { request: Option<String> },
}

/// Entry point: runs the whole prompt/sizing/clarify/plan flow on an inline
/// ratatui region anchored to the current cursor position on `io::stderr()`
/// (the same stream the old line editor echoed onto, and the one #701 keeps
/// this off the harness's own alternate screen), restoring the terminal
/// (raw mode off) on every exit path, including a panicking render (`panic =
/// "abort"` means `Drop` is not a safety net here -- see `install_panic_hook`).
pub(crate) fn run(cfg: &CtxConfig, state: &StateDir, repo: &Path) -> io::Result<IntakeOutcome> {
    enable_raw_mode()?;
    let hook = install_panic_hook();
    let backend = CrosstermBackend::new(io::stderr());
    let terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(REGION_HEIGHT),
        },
    );
    let outcome = match terminal {
        Ok(mut terminal) => {
            let flow = run_flow(&mut terminal, cfg, state, repo);
            let _ = terminal.clear();
            flow
        }
        Err(e) => Err(e),
    };
    let _ = disable_raw_mode();
    restore_panic_hook(&hook);
    outcome
}

type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

/// What the panic hook owes the terminal before the process aborts (`panic =
/// "abort"` skips `Drop` entirely, so `Terminal`'s own cursor-hiding never
/// gets put back, and the region's own stale content never gets cleared):
/// show the cursor again (ratatui hides it on every frame it draws), then
/// erase from wherever the cursor happens to be down to the end of the
/// screen. Deliberately NOT a move up to "the region's own top" first: the
/// region can have scrolled since it was created (a long request, a tall
/// clarify box), so a guessed offset risks erasing real scrollback above the
/// crash point instead of only the stale region -- erasing from the
/// cursor's own current position down is the largest reset that can never
/// do that. A fixed byte string for the same reason `term::EMERGENCY_RESET`/
/// `DASH_RESET` are (see that module): cheap and allocation-free, so it
/// costs nothing to run unconditionally from a panic hook.
const PANIC_RESET: &[u8] = b"\x1b[?25h\x1b[0J";

/// Mirrors `dash::run_dashboard`'s own `install_panic_hook`/`restore_panic_
/// hook` pair: a panic mid-render must still leave raw mode off AND the
/// screen in a usable state before the process aborts, since `panic =
/// "abort"` skips unwinding (and therefore every `Drop`) entirely.
fn install_panic_hook() -> std::sync::Arc<PanicHook> {
    let previous: std::sync::Arc<PanicHook> = std::sync::Arc::new(std::panic::take_hook());
    let chained = std::sync::Arc::clone(&previous);
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let mut stderr = io::stderr();
        let _ = stderr.write_all(PANIC_RESET);
        let _ = stderr.flush();
        chained(info);
    }));
    previous
}

fn restore_panic_hook(previous: &std::sync::Arc<PanicHook>) {
    let _ = std::panic::take_hook();
    let previous = std::sync::Arc::clone(previous);
    std::panic::set_hook(Box::new(move |info| previous(info)));
}

type Term = Terminal<CrosstermBackend<io::Stderr>>;

fn run_flow(
    terminal: &mut Term,
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
) -> io::Result<IntakeOutcome> {
    let mut seed = TextBuf::default();
    loop {
        let Some(request) = run_prompt(terminal, &mut seed)? else {
            return Ok(IntakeOutcome::Unplanned {
                request: none_if_empty(seed.text()),
            });
        };
        let Some(Sizing {
            mut decision,
            mut ready_harnesses,
        }) = run_sizing(terminal, cfg, state, repo, &request)?
        else {
            return Ok(IntakeOutcome::Unplanned {
                request: Some(request),
            });
        };
        let mut request = request;
        if decision.needs_clarification >= CLARIFY_THRESHOLD
            && decision.needs_clarification_decisive
        {
            record_clarification(cfg, state, &decision, "requested");
            match run_clarify(terminal, &decision)? {
                ClarifyOutcome::Abandon => {
                    return Ok(IntakeOutcome::Unplanned {
                        request: Some(request),
                    });
                }
                ClarifyOutcome::Skip => record_clarification(cfg, state, &decision, "unanswered"),
                ClarifyOutcome::Answer(addition) => {
                    record_clarification(cfg, state, &decision, "answered");
                    let combined = format!("{request}\n\n{addition}");
                    let Some(redecided) = run_sizing(terminal, cfg, state, repo, &combined)? else {
                        return Ok(IntakeOutcome::Unplanned {
                            request: Some(combined),
                        });
                    };
                    decision = redecided.decision;
                    ready_harnesses = redecided.ready_harnesses;
                    request = combined;
                }
            }
        }
        match run_plan(terminal, cfg, repo, &request, decision, &ready_harnesses)? {
            PlanResult::Abandon => {
                return Ok(IntakeOutcome::Unplanned {
                    request: Some(request),
                });
            }
            PlanResult::EditTask(text) => {
                seed = TextBuf::from_text(&text);
            }
            PlanResult::Confirm(final_decision) => {
                return Ok(IntakeOutcome::Decided {
                    decision: final_decision,
                    request,
                });
            }
        }
    }
}

fn none_if_empty(text: String) -> Option<String> {
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Issue #537 (A2), carried over verbatim from the old `maybe_clarify`: a Jev
/// effect naming whether the clarify round was requested, answered or left
/// unanswered -- only ever recorded when a `Decider::Typesafe` decision is
/// what asked, since only that decider's own advice is what this measures
/// the outcome of.
fn record_clarification(
    cfg: &CtxConfig,
    state: &StateDir,
    decision: &ProxyDecision,
    action: &'static str,
) {
    if !matches!(decision.decider, proxy::decision::Decider::Typesafe) {
        return;
    }
    let mut effect = JevEffect::new("intake_clarification", action);
    effect.subject_id = Some(&decision.request_sha256);
    effect.reason = Some(match decision.clarification_category.as_deref() {
        Some("target") => "target",
        Some("behavior") => "behavior",
        Some("constraint") => "constraint",
        _ => "generic",
    });
    jev::record_effect(cfg, state, cfg.jev.intake_savings, &effect);
}

// ---------------------------------------------------------------------
// Pure: multi-line text buffer and key-action mappings
// ---------------------------------------------------------------------

/// A multi-line text buffer with an interior cursor, tracked as codepoints
/// (not bytes) with an embedded `'\n'` for a hard line break -- the prompt
/// and clarify boxes both edit one of these. Deliberately does not soft-wrap
/// long lines the way the old single-line `EditLine`/`redraw_edit_line` did
/// (PR3 scope decision, see the PR report): rendering splits on `'\n'` only,
/// and a line longer than the box is simply clipped by the terminal rather
/// than reflowed -- multi-line input here is the rare, deliberate case
/// (Shift+Enter/Alt+Enter/Ctrl+J), not the common one.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct TextBuf {
    chars: Vec<char>,
    cursor: usize,
}

impl TextBuf {
    pub(crate) fn from_text(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let cursor = chars.len();
        Self { chars, cursor }
    }

    pub(crate) fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        self.chars.remove(self.cursor);
        true
    }

    fn delete_forward(&mut self) -> bool {
        if self.cursor >= self.chars.len() {
            return false;
        }
        self.chars.remove(self.cursor);
        true
    }

    fn move_left(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        true
    }

    fn move_right(&mut self) -> bool {
        if self.cursor >= self.chars.len() {
            return false;
        }
        self.cursor += 1;
        true
    }

    fn move_home(&mut self) -> bool {
        let moved = self.cursor != 0;
        self.cursor = 0;
        moved
    }

    fn move_end(&mut self) -> bool {
        let moved = self.cursor != self.chars.len();
        self.cursor = self.chars.len();
        moved
    }

    /// `(row, col)` of the cursor, splitting only on `'\n'` (see this type's
    /// own doc comment on why there is no soft wrap to account for). `col`
    /// is in terminal cells, not codepoints -- the same CJK/combining-mark
    /// distinction the old `EditLine::cells_upto` made.
    fn cursor_row_col(&self) -> (u16, u16) {
        let upto = &self.chars[..self.cursor];
        let row = upto.iter().filter(|c| **c == '\n').count();
        let col: usize = upto
            .rsplit(|c| *c == '\n')
            .next()
            .unwrap_or(&[])
            .iter()
            .map(|c| UnicodeWidthChar::width(*c).unwrap_or(0))
            .sum();
        (row as u16, col as u16)
    }
}

/// What one raw key does to a [`TextBuf`] in the prompt/clarify boxes.
/// `Abandon` (Esc) and `Submit` (Enter) never mutate the buffer -- the
/// caller decides what to do with them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextKeyAction {
    Insert(char),
    Newline,
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    Submit,
    Abandon,
    Ignored,
}

/// Ctrl+C, delivered as a literal key event once raw mode has taken `ISIG`
/// away (the terminal's own SIGINT generation along with it): every screen
/// maps it to the same "abandon" action Esc already gives, rather than
/// letting it fall through to `Ignored` the way a bare `Char('c')` +
/// `CONTROL` otherwise would -- the operator's own reflexive way to bail out
/// of a CLI prompt must still work here.
fn is_ctrl_c(code: KeyCode, modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c' | 'C'))
}

/// Pure: Esc or Ctrl+C always abandons; Ctrl+J always inserts a newline (the
/// one newline chord every terminal delivers identically, raw mode or not);
/// Shift+Enter/Alt+Enter insert a newline when the terminal reports the
/// modifier on the Enter key itself (see the PR report's own note on
/// Windows Console API modifier delivery); a bare Enter submits.
pub(crate) fn text_key_action(code: KeyCode, modifiers: KeyModifiers) -> TextKeyAction {
    if code == KeyCode::Esc || is_ctrl_c(code, modifiers) {
        return TextKeyAction::Abandon;
    }
    if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('j' | 'J')) {
        return TextKeyAction::Newline;
    }
    match code {
        KeyCode::Enter if modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) => {
            TextKeyAction::Newline
        }
        KeyCode::Enter => TextKeyAction::Submit,
        KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => TextKeyAction::Insert(c),
        KeyCode::Backspace => TextKeyAction::Backspace,
        KeyCode::Delete => TextKeyAction::Delete,
        KeyCode::Left => TextKeyAction::Left,
        KeyCode::Right => TextKeyAction::Right,
        KeyCode::Home => TextKeyAction::Home,
        KeyCode::End => TextKeyAction::End,
        _ => TextKeyAction::Ignored,
    }
}

/// Applies `action` to `buf`; `true` when the buffer actually changed
/// (worth a redraw). `Submit`/`Abandon`/`Ignored` never touch `buf`.
pub(crate) fn apply_text_key(buf: &mut TextBuf, action: TextKeyAction) -> bool {
    match action {
        TextKeyAction::Insert(c) => {
            buf.insert(c);
            true
        }
        TextKeyAction::Newline => {
            buf.insert('\n');
            true
        }
        TextKeyAction::Backspace => buf.backspace(),
        TextKeyAction::Delete => buf.delete_forward(),
        TextKeyAction::Left => buf.move_left(),
        TextKeyAction::Right => buf.move_right(),
        TextKeyAction::Home => buf.move_home(),
        TextKeyAction::End => buf.move_end(),
        TextKeyAction::Submit | TextKeyAction::Abandon | TextKeyAction::Ignored => false,
    }
}

/// The plan card's own key handling: `1`-`4` choose directly, Up/Down move
/// the selection, Enter confirms whatever is currently selected, Esc
/// abandons -- literally "anywhere", including from inside the choice list
/// (the design's own wording; there is no separate "back one level" action).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanKeyAction {
    Up,
    Down,
    Choose(usize),
    Confirm,
    Abandon,
    Ignored,
}

/// Any digit is a candidate choose action; the caller (`run_plan`) is what
/// knows how many choices actually exist this render (see [`PlanCard::
/// choices`]) and ignores one out of range -- so this never has to know that
/// count itself. Esc or Ctrl+C abandons, same as every other screen.
pub(crate) fn plan_key_action(code: KeyCode, modifiers: KeyModifiers) -> PlanKeyAction {
    if code == KeyCode::Esc || is_ctrl_c(code, modifiers) {
        return PlanKeyAction::Abandon;
    }
    match code {
        KeyCode::Up => PlanKeyAction::Up,
        KeyCode::Down => PlanKeyAction::Down,
        KeyCode::Enter => PlanKeyAction::Confirm,
        KeyCode::Char(c @ '1'..='9') => PlanKeyAction::Choose(c as usize - '1' as usize),
        _ => PlanKeyAction::Ignored,
    }
}

/// One of the plan card's numbered choices. Choice 2 ("Start with a
/// different model") is CONDITIONAL -- see [`PlanCard::choices`] -- so a
/// choice's number is its position in that list, not a fixed digit of its
/// own; [`PlanChoice::text`] is the phrase alone, numbered by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanChoice {
    Start,
    DifferentModel,
    NoWorkflow,
    EditTask,
}

impl PlanChoice {
    fn text(self) -> &'static str {
        match self {
            Self::Start => "Start",
            Self::DifferentModel => "Start with a different model",
            Self::NoWorkflow => "Start without a workflow",
            Self::EditTask => "Edit the task",
        }
    }
}

/// Applies choice 2 (a picked seat) or 3 (drop the workflow) to `decision`;
/// pure clone-and-mutate so it is testable without a terminal. Choice 1
/// returns `decision` unchanged; choice 4 never reaches this (the caller
/// returns to the prompt instead). This is the ONLY place `decision.
/// workflow` is cleared for choice 3 -- `run_with`'s existing `start_proxy_
/// workflow` reads `decision.workflow` after intake returns, so clearing it
/// here is what makes "choice 3 starts none" true without any change to
/// that already-existing call. A picked seat (choice 2) replaces BOTH the
/// harness and the model: the operator's follow-up made the model list span
/// every enabled harness, not only the decided one's own ladder, so "a
/// different model" can mean a different harness too.
pub(crate) fn apply_choice(
    mut decision: ProxyDecision,
    choice: PlanChoice,
    seat_override: Option<&proxy::decision::Seat>,
) -> ProxyDecision {
    match choice {
        PlanChoice::Start | PlanChoice::EditTask => decision,
        PlanChoice::DifferentModel => {
            if let Some(seat) = seat_override {
                decision.orchestrator = seat.clone();
            }
            decision
        }
        PlanChoice::NoWorkflow => {
            decision.workflow = None;
            decision
        }
    }
}

// ---------------------------------------------------------------------
// Pure: plan-card text, fallback notice, clarify wording, summary line
// ---------------------------------------------------------------------

/// One row of the plan card, e.g. `("Model", "Sonnet 5, working alone")`.
pub(crate) type Row = (&'static str, String);

/// Everything the plan card shows, built once from a decision (and, for the
/// orchestrated "Helpers" row and the workflow row's step count, the live
/// harness/workflow roster) -- no seat tiers, decider names, domain scores
/// or confidence numbers anywhere in it (design constraint).
pub(crate) struct PlanCard {
    pub sentence: String,
    pub seat_rows: Vec<Row>,
    pub workflow_row: Row,
    pub why_row: Row,
    /// The seats choice 2 would offer -- every enabled harness's own tier
    /// ladder, minus the already-planned one (see `decision::model_choices`).
    /// Empty exactly when there is nothing else to offer.
    pub model_choices: Vec<proxy::decision::Seat>,
    /// The numbered choices this card actually shows, in order -- `Start`,
    /// `DifferentModel` (present only when `model_choices` is non-empty),
    /// `NoWorkflow`, `EditTask`. A choice's displayed number is `1 +` its
    /// position here, so omitting `DifferentModel` renumbers the rest
    /// automatically rather than leaving a gap at "2".
    pub choices: Vec<PlanChoice>,
}

pub(crate) fn build_plan_card(
    cfg: &CtxConfig,
    repo: &Path,
    decision: &ProxyDecision,
    ready_harnesses: &[String],
) -> PlanCard {
    let seat_rows = match decision.seat_role {
        SeatRole::Single => vec![(
            "Model",
            format!(
                "{}, working alone",
                plan_seat_label(&decision.orchestrator.harness, &decision.orchestrator.model)
            ),
        )],
        SeatRole::Orchestrator => {
            let worker = proxy::decision::worker_model(
                cfg,
                &decision.orchestrator.harness,
                decision.worker_tier,
            );
            vec![
                (
                    "Lead",
                    format!(
                        "{} coordinates and reviews",
                        plan_seat_label(
                            &decision.orchestrator.harness,
                            &decision.orchestrator.model
                        )
                    ),
                ),
                (
                    "Helpers",
                    format!(
                        "{} workers write code and tests",
                        plan_seat_label(&decision.orchestrator.harness, &worker)
                    ),
                ),
            ]
        }
    };
    let model_choices =
        proxy::decision::model_choices(cfg, &decision.orchestrator, ready_harnesses);
    let mut choices = vec![PlanChoice::Start];
    if !model_choices.is_empty() {
        choices.push(PlanChoice::DifferentModel);
    }
    choices.push(PlanChoice::NoWorkflow);
    choices.push(PlanChoice::EditTask);
    PlanCard {
        sentence: plan_sentence(decision),
        seat_rows,
        workflow_row: ("Workflow", workflow_line(cfg, repo, decision)),
        why_row: ("Why", why_line(decision)),
        model_choices,
        choices,
    }
}

fn plan_sentence(decision: &ProxyDecision) -> String {
    let size = match decision.complexity {
        Complexity::Trivial => "small",
        Complexity::Bounded => "bounded",
        Complexity::Substantial => "substantial",
        Complexity::Architectural => "architectural",
    };
    let intent = match decision.intent {
        Intent::Feature => "feature",
        Intent::Bugfix => "bug fix",
        Intent::Refactor => "refactor",
        Intent::Spike => "spike",
        Intent::Review => "review",
        Intent::Other => "change",
    };
    let scope = match decision.domains.len() {
        0 => String::new(),
        1 => format!(" in the {}", decision.domains[0]),
        _ => format!(" across the {}", decision.domains.join(" and ")),
    };
    format!("A {size} {intent}{scope}.")
}

fn why_line(decision: &ProxyDecision) -> String {
    let area = match decision.seat_role {
        SeatRole::Single => "one area",
        SeatRole::Orchestrator => "several areas",
    };
    let domains = if decision.domains.is_empty() {
        String::new()
    } else {
        format!(" ({})", decision.domains.join(", "))
    };
    let design = if decision.complexity == Complexity::Architectural {
        "design choices, "
    } else {
        ""
    };
    let risk = format!("{:?}", decision.risk).to_ascii_lowercase();
    format!("{area}{domains}, {design}{risk} risk")
}

fn workflow_line(cfg: &CtxConfig, repo: &Path, decision: &ProxyDecision) -> String {
    let Some(kind) = &decision.workflow else {
        return "none".to_string();
    };
    let roster = Roster::gather(cfg, repo);
    match roster.workflow_step_summary(kind) {
        Some((steps, first)) => format!("{kind} \u{b7} {steps} steps, starting at {first}"),
        None => kind.clone(),
    }
}

/// A model id or alias in plain words, prefixed with its harness (operator
/// follow-up, round 2): `("claude", "sonnet")` -> `Claude \u{b7} Sonnet`,
/// `("claude", "claude-sonnet-5")` -> `Claude \u{b7} Sonnet 5`, `("codex",
/// "gpt-5.6-luna")` -> `Codex \u{b7} GPT-5.6 Luna`. Used everywhere BUT the
/// plan card's own Model/Lead/Helpers rows, which use [`plan_seat_label`]
/// instead (its own doc comment says why). See [`bare_model_name`] for the
/// model half's own formatting rules (vendor casing, hyphenated version
/// numbers).
pub(crate) fn model_display_name(harness: &str, model: &str) -> String {
    format!(
        "{} \u{b7} {}",
        harness_display_name(harness),
        bare_model_name(harness, model)
    )
}

/// The plan card's own Model/Lead/Helpers row label (operator follow-up,
/// round 2): plain `Sonnet 5` style -- no harness prefix -- when `harness`
/// is `claude` (still the overwhelmingly common case, and the one the
/// original mock's own wording assumed), [`model_display_name`]'s prefixed
/// form (`Codex \u{b7} GPT-5.6 Terra`) otherwise, since a non-claude seat is
/// exactly the case where naming the harness is the useful information.
fn plan_seat_label(harness: &str, model: &str) -> String {
    if harness.eq_ignore_ascii_case("claude") {
        bare_model_name(harness, model)
    } else {
        model_display_name(harness, model)
    }
}

/// `claude` -> `Claude`, `cursor-agent` -> `Cursor Agent`: a harness
/// registry name in plain title-cased words, for the label half of
/// [`model_display_name`].
fn harness_display_name(harness: &str) -> String {
    harness
        .split(['-', '_'])
        .filter(|word| !word.is_empty())
        .map(title_case_word)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The model half alone, no harness prefix: strips a leading segment that
/// names `harness` itself (`claude-sonnet-5` on harness `claude` -> the
/// `claude` segment, since it says nothing `model_display_name`'s own
/// harness label does not already say), title-cases the rest, and keeps a
/// recognized vendor acronym in ITS OWN casing (`gpt` -> `GPT`, "keep vendor
/// casing" -- the operator's own follow-up) hyphen-joined to the segment
/// right after it, matching how the vendor itself spells the id (`gpt-5.6-
/// luna` -> `GPT-5.6 Luna`, not `Gpt 5.6 Luna` or `GPT 5.6 Luna`). A leading
/// digit's own word is left alone (a version number, not a word to
/// capitalize). Never empty: an alias with nothing left after stripping (or
/// this cannot recognize at all) falls back to the raw string.
fn bare_model_name(harness: &str, model: &str) -> String {
    let mut segments: Vec<&str> = model.split(['-', '_']).filter(|s| !s.is_empty()).collect();
    if segments
        .first()
        .is_some_and(|first| first.eq_ignore_ascii_case(harness))
    {
        segments.remove(0);
    }
    if segments.is_empty() {
        return model.to_string();
    }
    let mut words: Vec<String> = Vec::new();
    let mut i = 0;
    while i < segments.len() {
        match (vendor_acronym(segments[i]), segments.get(i + 1)) {
            (Some(acronym), Some(next)) => {
                words.push(format!("{acronym}-{next}"));
                i += 2;
            }
            (Some(acronym), None) => {
                words.push(acronym.to_string());
                i += 1;
            }
            (None, _) => {
                words.push(title_case_word(segments[i]));
                i += 1;
            }
        }
    }
    words.join(" ")
}

/// Vendor words this codebase knows to keep in a specific casing rather than
/// title-casing generically -- `gpt` is the operator's own named example
/// (`GPT-5.6 Luna`, never `Gpt`); add more here as they come up, never by
/// special-casing a whole model id.
fn vendor_acronym(word: &str) -> Option<&'static str> {
    match word.to_ascii_lowercase().as_str() {
        "gpt" => Some("GPT"),
        _ => None,
    }
}

/// Title-cases one word, leaving a leading digit's own word alone (a
/// version number).
fn title_case_word(word: &str) -> String {
    if word.starts_with(|c: char| c.is_ascii_digit()) {
        return word.to_string();
    }
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// The yellow `\u{26a0}` warning line and its details line, when `decision.
/// fallbacks` is non-empty (the configured decider failed or timed out, so a
/// fallback -- ultimately the deterministic baseline -- produced the plan).
/// `None` when nothing fell back.
pub(crate) fn fallback_notice(
    cfg: &CtxConfig,
    decision: &ProxyDecision,
) -> Option<(String, String)> {
    if decision.fallbacks.is_empty() {
        return None;
    }
    let attempted = decision
        .fallbacks
        .iter()
        .find_map(|line| line.split_once(':').map(|(who, _)| who.trim()));
    let warning = match attempted {
        Some("typesafe") => format!(
            "Typesafe did not answer within {}s. The plan below uses local rules.",
            cfg.proxy.typesafe.timeout_secs
        ),
        Some("helper") => {
            "The helper model did not answer. The plan below uses local rules.".to_string()
        }
        _ => "The configured planner did not answer. The plan below uses local rules.".to_string(),
    };
    let details = "Details: zirv ctx proxy --json \u{b7} turn the planner off: zirv ctx config \
                   set proxy.enabled false"
        .to_string();
    Some((warning, details))
}

/// The clarify box's reworded question -- plain words, no confidence number
/// (unlike the old `maybe_clarify`'s `"the request looks ambiguous (0.73)"`,
/// which this replaces).
pub(crate) fn clarify_question(decision: &ProxyDecision) -> &'static str {
    match decision.clarification_category.as_deref() {
        Some("target") => "Which part should change? Name a file, a module or a screen.",
        Some("behavior") => "What should happen when it's done? Describe the result.",
        Some("constraint") => "Is there a constraint or compatibility rule this must respect?",
        _ => "What's missing? Add a detail or two.",
    }
}

/// The one line that stays in scrollback once the region clears:
/// `\u{273b} zirv planned in {N}s \u{b7} {seat} \u{b7} {workflow}`. `decision.
/// elapsed_ms` is `decide()`'s own timing (the LAST call, if a clarify round
/// re-decided) -- rounded up so a near-instant deterministic decision still
/// reads as at least 1s rather than a slightly odd "planned in 0s".
/// `started_workflow_id` is `None` for choice 3 (no workflow) and for a
/// decision that never named one; `run_with` supplies it from the SAME
/// `start_proxy_workflow` call that already threads it to `proxy::prompt_
/// layer`.
pub(crate) fn summary_line(decision: &ProxyDecision, started_workflow_id: Option<&str>) -> String {
    let secs = decision.elapsed_ms.div_ceil(1000).max(1);
    let label = plan_seat_label(&decision.orchestrator.harness, &decision.orchestrator.model);
    let seat = match decision.seat_role {
        SeatRole::Single => format!("{label} alone"),
        SeatRole::Orchestrator => format!("{label} leads"),
    };
    let workflow = match (&decision.workflow, started_workflow_id) {
        (Some(kind), Some(id)) => format!("{kind} workflow {id}"),
        (Some(kind), None) => format!("{kind} workflow"),
        (None, _) => "no workflow".to_string(),
    };
    format!("\u{273b} zirv planned in {secs}s \u{b7} {seat} \u{b7} {workflow}")
}

// ---------------------------------------------------------------------
// Impure: per-screen render + key loops
// ---------------------------------------------------------------------

const HINT_STYLE: Style = Style::new().fg(Color::DarkGray);
const BOX_STYLE: Style = Style::new().fg(Color::Cyan);
const WARN_STYLE: Style = Style::new().fg(Color::Yellow);

type Buffer = ratatui::buffer::Buffer;

fn text_box<'a>(title: &'a str, style: Style) -> Block<'a> {
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(style);
    if !title.is_empty() {
        block = block.title(format!(" {title} "));
    }
    block
}

/// Renders any `Widget` at `area` -- a thin wrapper so the `draw_*` helpers
/// below read as plain data-to-pixels functions of `(Rect, &mut Buffer)`
/// rather than needing a live `&mut Frame` (which they don't have: they take
/// the buffer directly so the same rendering is exercised from a
/// `TestBackend`-free unit test too, via a bare `Buffer::empty`).
fn render(widget: impl ratatui::widgets::Widget, area: Rect, buf: &mut Buffer) {
    ratatui::widgets::Widget::render(widget, area, buf);
}

/// Renders `block` at `area`, then `content` inside the space it leaves --
/// returns that inner `Rect` so the caller can place a cursor marker or
/// further content relative to it.
fn render_boxed(block: Block<'_>, content: Paragraph<'_>, area: Rect, buf: &mut Buffer) -> Rect {
    let inner = block.inner(area);
    render(block, area, buf);
    render(content, inner, buf);
    inner
}

fn run_prompt(terminal: &mut Term, buf: &mut TextBuf) -> io::Result<Option<String>> {
    loop {
        terminal.draw(|frame| draw_prompt(frame.area(), frame.buffer_mut(), buf))?;
        if event::poll(Duration::from_millis(90))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match text_key_action(key.code, key.modifiers) {
                TextKeyAction::Submit => {
                    if !buf.is_empty() {
                        return Ok(Some(buf.text()));
                    }
                }
                TextKeyAction::Abandon => return Ok(None),
                action => {
                    apply_text_key(buf, action);
                }
            }
        }
    }
}

fn draw_prompt(area: Rect, buf: &mut Buffer, line: &TextBuf) {
    let box_area = Rect::new(area.x, area.y, area.width, 3.min(area.height));
    let block = text_box("", BOX_STYLE);
    let content = Paragraph::new(format!("> {}", line.text()));
    let inner = render_boxed(block, content, box_area, buf);
    if inner.height > 0 {
        let (row, col) = line.cursor_row_col();
        let x = inner.x + 2 + col;
        let y = inner.y + row;
        if x < area.x + area.width && y < area.y + area.height {
            buf[(x, y)].set_style(Style::default().add_modifier(Modifier::REVERSED));
        }
    }
    let hint_y = box_area.y + box_area.height;
    if hint_y < area.y + area.height {
        render(
            Paragraph::new(Line::from(Span::styled(
                "  \u{23ce} plan and start \u{b7} shift+\u{23ce} new line \u{b7} esc start \
                 without a plan",
                HINT_STYLE,
            ))),
            Rect::new(area.x, hint_y, area.width, 1),
            buf,
        );
    }
}

/// What one `run_sizing` call produces: the decision itself, plus a snapshot
/// of which harnesses are actually usable right now (operator follow-up,
/// round 2) -- both computed on the SAME worker thread, once per call, never
/// re-derived on the UI thread or per frame. See `decision::ready_harness_
/// names`'s own doc comment for why this rides along with `decide()` rather
/// than being probed later, synchronously, from the plan card.
struct Sizing {
    decision: ProxyDecision,
    ready_harnesses: Vec<String>,
}

fn run_sizing(
    terminal: &mut Term,
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
    request: &str,
) -> io::Result<Option<Sizing>> {
    let (tx, rx) = mpsc::channel();
    let cfg_owned = cfg.clone();
    let state_root = state.root().to_path_buf();
    let repo_owned = repo.to_path_buf();
    let request_owned = request.to_string();
    std::thread::spawn(move || {
        let decision = proxy::decide(&cfg_owned, &state_root, &repo_owned, &request_owned, false);
        let ready_harnesses = proxy::decision::ready_harness_names(&cfg_owned, &repo_owned);
        // The receiver may already be gone (Esc abandoned this call): a send
        // error here just means the result is discarded, exactly as
        // intended -- never a reason to panic.
        let _ = tx.send(Sizing {
            decision,
            ready_harnesses,
        });
    });
    let started = Instant::now();
    loop {
        let elapsed = started.elapsed().as_secs();
        terminal.draw(|frame| draw_sizing(frame.area(), frame.buffer_mut(), elapsed))?;
        if event::poll(Duration::from_millis(90))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && (key.code == KeyCode::Esc || is_ctrl_c(key.code, key.modifiers))
        {
            return Ok(None);
        }
        if let Ok(sizing) = rx.try_recv() {
            return Ok(Some(sizing));
        }
    }
}

/// Ratatui's own `SPIN` frames, on an 90ms clock -- close to the mock's own
/// 80ms cadence (see the design doc's motion notes), not redrawn per poll.
const SPINNER: [char; 10] = [
    '\u{280b}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283c}', '\u{2834}', '\u{2826}', '\u{2827}',
    '\u{2807}', '\u{280f}',
];

fn spinner_frame() -> char {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    SPINNER[((millis / 90) % SPINNER.len() as u128) as usize]
}

fn draw_sizing(area: Rect, buf: &mut Buffer, elapsed_secs: u64) {
    let text = Line::from(vec![
        Span::styled(format!("{} ", spinner_frame()), BOX_STYLE),
        Span::styled("Sizing the task\u{2026}", BOX_STYLE),
        Span::styled(
            format!(" {elapsed_secs}s \u{b7} esc to start without a plan"),
            HINT_STYLE,
        ),
    ]);
    render(
        Paragraph::new(text),
        Rect::new(area.x, area.y, area.width, 1),
        buf,
    );
}

enum ClarifyOutcome {
    Abandon,
    Skip,
    Answer(String),
}

fn run_clarify(terminal: &mut Term, decision: &ProxyDecision) -> io::Result<ClarifyOutcome> {
    let question = clarify_question(decision);
    let mut buf = TextBuf::default();
    loop {
        terminal.draw(|frame| draw_clarify(frame.area(), frame.buffer_mut(), question, &buf))?;
        if event::poll(Duration::from_millis(90))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            if key.code == KeyCode::Tab {
                return Ok(ClarifyOutcome::Skip);
            }
            match text_key_action(key.code, key.modifiers) {
                TextKeyAction::Submit => {
                    return Ok(if buf.is_empty() {
                        ClarifyOutcome::Skip
                    } else {
                        ClarifyOutcome::Answer(buf.text())
                    });
                }
                TextKeyAction::Abandon => return Ok(ClarifyOutcome::Abandon),
                action => {
                    apply_text_key(&mut buf, action);
                }
            }
        }
    }
}

fn draw_clarify(area: Rect, buf: &mut Buffer, question: &str, line: &TextBuf) {
    let box_area = Rect::new(area.x, area.y, area.width, 4.min(area.height));
    let block = text_box("One question", WARN_STYLE);
    let text = format!("{question}\n> {}", line.text());
    let content = Paragraph::new(text).wrap(Wrap { trim: false });
    let inner = render_boxed(block, content, box_area, buf);
    if inner.height > 0 {
        let (row, col) = line.cursor_row_col();
        let x = inner.x + 2 + col;
        let y = inner.y + 1 + row;
        if x < area.x + area.width && y < area.y + area.height {
            buf[(x, y)].set_style(Style::default().add_modifier(Modifier::REVERSED));
        }
    }
    let hint_y = box_area.y + box_area.height;
    if hint_y < area.y + area.height {
        render(
            Paragraph::new(Line::from(Span::styled(
                "  \u{23ce} answer \u{b7} tab skip and plan anyway",
                HINT_STYLE,
            ))),
            Rect::new(area.x, hint_y, area.width, 1),
            buf,
        );
    }
}

enum PlanResult {
    Abandon,
    EditTask(String),
    Confirm(Box<ProxyDecision>),
}

fn run_plan(
    terminal: &mut Term,
    cfg: &CtxConfig,
    repo: &Path,
    request: &str,
    decision: ProxyDecision,
    ready_harnesses: &[String],
) -> io::Result<PlanResult> {
    let card = build_plan_card(cfg, repo, &decision, ready_harnesses);
    let fallback = fallback_notice(cfg, &decision);
    let choice_count = card.choices.len();
    let mut selected = 0usize;
    loop {
        terminal.draw(|frame| {
            draw_plan(
                frame.area(),
                frame.buffer_mut(),
                &card,
                fallback.as_ref(),
                selected,
            )
        })?;
        if event::poll(Duration::from_millis(90))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match plan_key_action(key.code, key.modifiers) {
                PlanKeyAction::Abandon => return Ok(PlanResult::Abandon),
                PlanKeyAction::Up => {
                    selected = selected.checked_sub(1).unwrap_or(choice_count - 1);
                }
                PlanKeyAction::Down => selected = (selected + 1) % choice_count,
                PlanKeyAction::Choose(i) if i < choice_count => selected = i,
                PlanKeyAction::Choose(_) | PlanKeyAction::Ignored => {}
                PlanKeyAction::Confirm => {
                    let Some(choice) = card.choices.get(selected).copied() else {
                        continue;
                    };
                    match choice {
                        PlanChoice::Start => {
                            return Ok(PlanResult::Confirm(Box::new(decision)));
                        }
                        PlanChoice::NoWorkflow => {
                            return Ok(PlanResult::Confirm(Box::new(apply_choice(
                                decision, choice, None,
                            ))));
                        }
                        PlanChoice::EditTask => {
                            return Ok(PlanResult::EditTask(request.to_string()));
                        }
                        PlanChoice::DifferentModel => {
                            if card.model_choices.is_empty() {
                                continue;
                            }
                            match run_model_pick(terminal, &card.model_choices)? {
                                None => return Ok(PlanResult::Abandon),
                                Some(seat) => {
                                    return Ok(PlanResult::Confirm(Box::new(apply_choice(
                                        decision,
                                        choice,
                                        Some(&seat),
                                    ))));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn draw_plan(
    area: Rect,
    buf: &mut Buffer,
    card: &PlanCard,
    fallback: Option<&(String, String)>,
    selected: usize,
) {
    let mut y = area.y;
    if let Some((warning, details)) = fallback {
        render(
            Paragraph::new(Line::from(vec![
                Span::styled("\u{26a0} ", WARN_STYLE),
                Span::styled(warning.clone(), WARN_STYLE),
            ])),
            Rect::new(area.x, y, area.width, 1),
            buf,
        );
        y += 1;
        render(
            Paragraph::new(Line::from(Span::styled(format!("  {details}"), HINT_STYLE))),
            Rect::new(area.x, y, area.width, 1),
            buf,
        );
        y += 2;
    }
    let choice_count = card.choices.len() as u16;
    // sentence(1) + blank(1) + seat_rows + workflow(1) + why(1) + blank(1) + choices.
    let inner_lines = 5 + card.seat_rows.len() as u16 + choice_count;
    let box_height = (inner_lines + 2).min(area.height.saturating_sub(y - area.y));
    let box_area = Rect::new(area.x, y, area.width, box_height);
    let block = text_box("Plan", BOX_STYLE);

    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            card.sentence.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    for (label, value) in &card.seat_rows {
        lines.push(plan_row(label, value));
    }
    lines.push(plan_row(card.workflow_row.0, &card.workflow_row.1));
    lines.push(plan_row(card.why_row.0, &card.why_row.1));
    lines.push(Line::from(""));
    for (index, choice) in card.choices.iter().enumerate() {
        let marker = if index == selected { "\u{276f} " } else { "  " };
        lines.push(Line::from(Span::styled(
            format!("{marker}{}. {}", index + 1, choice.text()),
            BOX_STYLE,
        )));
    }
    render_boxed(block, Paragraph::new(lines), box_area, buf);

    let hint_y = box_area.y + box_area.height;
    if hint_y < area.y + area.height {
        render(
            Paragraph::new(Line::from(Span::styled(
                "  \u{2191}\u{2193} choose \u{b7} \u{23ce} confirm \u{b7} esc start without a plan",
                HINT_STYLE,
            ))),
            Rect::new(area.x, hint_y, area.width, 1),
            buf,
        );
    }
}

fn plan_row(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {label:<10}", label = label), HINT_STYLE),
        Span::raw(value.to_string()),
    ])
}

fn run_model_pick(
    terminal: &mut Term,
    choices: &[proxy::decision::Seat],
) -> io::Result<Option<proxy::decision::Seat>> {
    let mut selected = 0usize;
    loop {
        terminal
            .draw(|frame| draw_model_pick(frame.area(), frame.buffer_mut(), choices, selected))?;
        if event::poll(Duration::from_millis(90))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Esc => return Ok(None),
                code if is_ctrl_c(code, key.modifiers) => return Ok(None),
                KeyCode::Up => selected = selected.checked_sub(1).unwrap_or(choices.len() - 1),
                KeyCode::Down => selected = (selected + 1) % choices.len(),
                KeyCode::Enter => return Ok(Some(choices[selected].clone())),
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    if let Some(n) = c.to_digit(10) {
                        let n = n as usize;
                        if n >= 1 && n <= choices.len() {
                            return Ok(Some(choices[n - 1].clone()));
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn draw_model_pick(
    area: Rect,
    buf: &mut Buffer,
    choices: &[proxy::decision::Seat],
    selected: usize,
) {
    let box_height = (choices.len() as u16 + 2).min(area.height);
    let box_area = Rect::new(area.x, area.y, area.width, box_height);
    let block = text_box("Model", BOX_STYLE);
    let lines: Vec<Line> = choices
        .iter()
        .enumerate()
        .map(|(i, seat)| {
            let marker = if i == selected { "\u{276f} " } else { "  " };
            Line::from(Span::styled(
                format!(
                    "{marker}{}. {}",
                    i + 1,
                    model_display_name(&seat.harness, &seat.model)
                ),
                BOX_STYLE,
            ))
        })
        .collect();
    render_boxed(block, Paragraph::new(lines), box_area, buf);
    let hint_y = box_area.y + box_area.height;
    if hint_y < area.y + area.height {
        render(
            Paragraph::new(Line::from(Span::styled(
                "  \u{2191}\u{2193} choose \u{b7} \u{23ce} confirm \u{b7} esc start without a plan",
                HINT_STYLE,
            ))),
            Rect::new(area.x, hint_y, area.width, 1),
            buf,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::catalogue::Tier;
    use crate::commands::ctx::proxy::decision::{Decider, Seat, SeatTier};
    use crate::commands::workflow::classify::{Complexity, Intent, RiskBand};
    use crate::commands::workflow::profile::{ExecutionMode, ValidationProfile};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn sample_single(workflow: Option<&str>) -> ProxyDecision {
        ProxyDecision {
            request_sha256: "x".repeat(64),
            repo: PathBuf::from("/tmp/repo"),
            intent: Intent::Bugfix,
            complexity: Complexity::Bounded,
            risk: RiskBand::Low,
            execution: ExecutionMode::Bounded,
            seat_role: SeatRole::Single,
            validation: ValidationProfile::default(),
            workflow: workflow.map(str::to_string),
            orchestrator: Seat {
                harness: "claude".to_string(),
                model: "claude-sonnet-5".to_string(),
            },
            seat_tier: SeatTier::Standard,
            worker_tier: Tier::Standard,
            needs_clarification: 0.0,
            needs_clarification_decisive: false,
            clarification_category: None,
            domains: Vec::new(),
            decider: Decider::Deterministic,
            confidence: BTreeMap::new(),
            reasons: Vec::new(),
            fallbacks: Vec::new(),
            elapsed_ms: 2_600,
            usage: None,
            created_at: 0,
            headless: false,
        }
    }

    fn sample_orchestrated() -> ProxyDecision {
        let mut decision = sample_single(Some("feature"));
        decision.intent = Intent::Feature;
        decision.complexity = Complexity::Architectural;
        decision.risk = RiskBand::Medium;
        decision.execution = ExecutionMode::Orchestrated;
        decision.seat_role = SeatRole::Orchestrator;
        decision.orchestrator.model = "claude-opus-5".to_string();
        decision.worker_tier = Tier::Standard;
        decision
    }

    // -- model_display_name / plan_seat_label --

    fn ready(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn model_display_name_always_prefixes_the_harness_and_keeps_vendor_casing() {
        assert_eq!(
            model_display_name("claude", "claude-sonnet-5"),
            "Claude \u{b7} Sonnet 5"
        );
        assert_eq!(
            model_display_name("claude", "sonnet"),
            "Claude \u{b7} Sonnet"
        );
        assert_eq!(
            model_display_name("codex", "gpt-5.6-luna"),
            "Codex \u{b7} GPT-5.6 Luna"
        );
        assert_eq!(
            model_display_name("codex", "gpt-6-astra"),
            "Codex \u{b7} GPT-6 Astra"
        );
        assert_eq!(
            model_display_name("cursor-agent", "sonnet"),
            "Cursor Agent \u{b7} Sonnet"
        );
    }

    /// Operator follow-up (round 2): the plan card's own Model/Lead/Helpers
    /// rows drop the harness prefix for claude (still plain `Sonnet 5`
    /// style) but keep it for any other harness (`Codex \u{b7} GPT-5.6
    /// Terra`) -- the coordinator's own examples, verbatim.
    #[test]
    fn plan_seat_label_is_bare_for_claude_and_prefixed_otherwise() {
        assert_eq!(plan_seat_label("claude", "claude-sonnet-5"), "Sonnet 5");
        assert_eq!(plan_seat_label("claude", "opus"), "Opus");
        assert_eq!(
            plan_seat_label("codex", "gpt-5.6-terra"),
            "Codex \u{b7} GPT-5.6 Terra"
        );
    }

    // -- plan card text: single-seat and orchestrated --

    #[test]
    fn plan_card_for_a_single_seat_decision_names_the_model_alone_no_jargon() {
        let repo = crate::commands::ctx::testenv::repo();
        let cfg = CtxConfig::default();
        let decision = sample_single(None);
        let card = build_plan_card(&cfg, repo.path(), &decision, &ready(&["claude"]));

        assert_eq!(card.sentence, "A bounded bug fix.");
        assert_eq!(
            card.seat_rows,
            vec![("Model", "Sonnet 5, working alone".to_string())]
        );
        assert_eq!(card.workflow_row, ("Workflow", "none".to_string()));
        assert_eq!(card.why_row, ("Why", "one area, low risk".to_string()));

        // No seat tiers, decider names, domain scores or confidence numbers.
        for text in [
            &card.sentence,
            &card.seat_rows[0].1,
            &card.workflow_row.1,
            &card.why_row.1,
        ] {
            for banned in ["standard", "cheap", "frontier", "deterministic", "typesafe"] {
                assert!(
                    !text.to_ascii_lowercase().contains(banned),
                    "{text} must not name {banned}"
                );
            }
        }
    }

    #[test]
    fn plan_card_for_an_orchestrated_decision_shows_lead_and_helpers() {
        let repo = crate::commands::ctx::testenv::repo();
        let cfg = CtxConfig::default();
        let decision = sample_orchestrated();
        let card = build_plan_card(&cfg, repo.path(), &decision, &ready(&["claude"]));

        assert_eq!(card.sentence, "A architectural feature.");
        assert_eq!(
            card.seat_rows,
            vec![
                ("Lead", "Opus 5 coordinates and reviews".to_string()),
                ("Helpers", "Sonnet workers write code and tests".to_string()),
            ]
        );
        assert!(
            card.why_row.1.contains("several areas"),
            "{:?}",
            card.why_row
        );
        assert!(
            card.why_row.1.contains("design choices"),
            "{:?}",
            card.why_row
        );
        assert!(card.why_row.1.contains("medium risk"), "{:?}", card.why_row);
    }

    /// Operator follow-up (2026-09-26): choice 2 only exists when there is
    /// an actual alternative to offer. With no harness at all named ready --
    /// standing in for the fully-disabled or nothing-installed case --
    /// `model_choices` is empty and the card's own `choices` list omits
    /// `DifferentModel` entirely -- renumbering the rest (`NoWorkflow`/
    /// `EditTask` become "2."/"3." instead of leaving a gap at "2") rather
    /// than showing a choice that leads nowhere.
    #[test]
    fn plan_card_omits_choice_two_and_renumbers_when_no_alternative_model_exists() {
        let repo = crate::commands::ctx::testenv::repo();
        let cfg = CtxConfig::default();
        let decision = sample_single(Some("bugfix"));
        let card = build_plan_card(&cfg, repo.path(), &decision, &[]);

        assert!(card.model_choices.is_empty(), "{:?}", card.model_choices);
        assert_eq!(
            card.choices,
            vec![
                PlanChoice::Start,
                PlanChoice::NoWorkflow,
                PlanChoice::EditTask
            ]
        );

        // At least one OTHER ready harness (or the same harness's own other
        // tiers) keeps choice 2.
        let default_card = build_plan_card(&cfg, repo.path(), &decision, &ready(&["claude"]));
        assert!(default_card.choices.contains(&PlanChoice::DifferentModel));
    }

    #[test]
    fn workflow_row_names_the_step_count_and_first_step_when_the_kind_is_known() {
        let repo = crate::commands::ctx::testenv::repo();
        let cfg = CtxConfig::default();
        let decision = sample_single(Some("bugfix"));
        let card = build_plan_card(&cfg, repo.path(), &decision, &ready(&["claude"]));
        assert!(
            card.workflow_row.1.starts_with("bugfix \u{b7} "),
            "{:?}",
            card.workflow_row
        );
        assert!(card.workflow_row.1.contains("steps, starting at"));
    }

    // -- fallback notice --

    #[test]
    fn fallback_notice_is_none_when_nothing_fell_back() {
        let cfg = CtxConfig::default();
        let decision = sample_single(None);
        assert!(fallback_notice(&cfg, &decision).is_none());
    }

    #[test]
    fn fallback_notice_names_typesafe_and_its_configured_timeout() {
        let mut cfg = CtxConfig::default();
        cfg.proxy.typesafe.timeout_secs = 10;
        let mut decision = sample_single(None);
        decision.fallbacks = vec!["typesafe: timed out waiting for a response".to_string()];
        let (warning, details) = fallback_notice(&cfg, &decision).expect("a fallback notice");
        assert_eq!(
            warning,
            "Typesafe did not answer within 10s. The plan below uses local rules."
        );
        assert!(details.contains("zirv ctx proxy --json"));
        assert!(details.contains("proxy.enabled false"));
    }

    // -- apply_choice: workflow started for choice 1, not for choice 3 --

    #[test]
    fn choice_one_leaves_the_decision_unchanged() {
        let decision = sample_single(Some("bugfix"));
        let applied = apply_choice(decision.clone(), PlanChoice::Start, None);
        assert_eq!(applied, decision);
    }

    #[test]
    fn choice_two_overrides_the_harness_and_model_together() {
        let decision = sample_single(Some("bugfix"));
        let picked = Seat {
            harness: "codex".to_string(),
            model: "gpt-5.6-luna".to_string(),
        };
        let applied = apply_choice(decision.clone(), PlanChoice::DifferentModel, Some(&picked));
        assert_eq!(applied.orchestrator, picked);
        assert_eq!(applied.workflow, decision.workflow);
    }

    #[test]
    fn choice_three_clears_the_workflow_so_start_proxy_workflow_skips_it() {
        let decision = sample_single(Some("bugfix"));
        assert!(decision.workflow.is_some());
        let applied = apply_choice(decision, PlanChoice::NoWorkflow, None);
        assert_eq!(
            applied.workflow, None,
            "run_with's existing start_proxy_workflow reads this field, so clearing it here is \
             what makes choice 3 start no workflow"
        );
    }

    // -- summary line --

    #[test]
    fn summary_line_single_seat_with_a_started_workflow() {
        let mut decision = sample_single(Some("bugfix"));
        decision.elapsed_ms = 2_600;
        assert_eq!(
            summary_line(&decision, Some("w-3f2a")),
            "\u{273b} zirv planned in 3s \u{b7} Sonnet 5 alone \u{b7} bugfix workflow w-3f2a"
        );
    }

    #[test]
    fn summary_line_orchestrated_with_no_workflow() {
        let mut decision = sample_orchestrated();
        decision.workflow = None;
        decision.elapsed_ms = 900;
        assert_eq!(
            summary_line(&decision, None),
            "\u{273b} zirv planned in 1s \u{b7} Opus 5 leads \u{b7} no workflow"
        );
    }

    // -- key-action mappings --

    #[test]
    fn esc_abandons_from_the_text_box() {
        assert_eq!(
            text_key_action(KeyCode::Esc, KeyModifiers::NONE),
            TextKeyAction::Abandon
        );
    }

    #[test]
    fn plain_enter_submits_shift_and_alt_enter_insert_a_newline() {
        assert_eq!(
            text_key_action(KeyCode::Enter, KeyModifiers::NONE),
            TextKeyAction::Submit
        );
        assert_eq!(
            text_key_action(KeyCode::Enter, KeyModifiers::SHIFT),
            TextKeyAction::Newline
        );
        assert_eq!(
            text_key_action(KeyCode::Enter, KeyModifiers::ALT),
            TextKeyAction::Newline
        );
    }

    #[test]
    fn ctrl_j_inserts_a_newline_regardless_of_the_enter_key() {
        assert_eq!(
            text_key_action(KeyCode::Char('j'), KeyModifiers::CONTROL),
            TextKeyAction::Newline
        );
    }

    #[test]
    fn apply_text_key_newline_inserts_a_literal_line_break() {
        let mut buf = TextBuf::default();
        apply_text_key(&mut buf, TextKeyAction::Insert('a'));
        apply_text_key(&mut buf, TextKeyAction::Newline);
        apply_text_key(&mut buf, TextKeyAction::Insert('b'));
        assert_eq!(buf.text(), "a\nb");
    }

    #[test]
    fn plan_key_action_maps_digits_arrows_enter_and_esc() {
        assert_eq!(
            plan_key_action(KeyCode::Char('1'), KeyModifiers::NONE),
            PlanKeyAction::Choose(0)
        );
        assert_eq!(
            plan_key_action(KeyCode::Char('4'), KeyModifiers::NONE),
            PlanKeyAction::Choose(3)
        );
        assert_eq!(
            plan_key_action(KeyCode::Up, KeyModifiers::NONE),
            PlanKeyAction::Up
        );
        assert_eq!(
            plan_key_action(KeyCode::Down, KeyModifiers::NONE),
            PlanKeyAction::Down
        );
        assert_eq!(
            plan_key_action(KeyCode::Enter, KeyModifiers::NONE),
            PlanKeyAction::Confirm
        );
        assert_eq!(
            plan_key_action(KeyCode::Esc, KeyModifiers::NONE),
            PlanKeyAction::Abandon
        );
    }

    /// Review finding: raw mode delivers Ctrl+C as a literal `Char('c')` +
    /// `CONTROL` key event (raw mode takes `ISIG`, and with it the
    /// terminal's own SIGINT generation, away) -- both `text_key_action` and
    /// `plan_key_action` used to fall through to `Ignored` for it, silently
    /// swallowing the operator's reflexive way to bail out. Both now map it
    /// to the same `Abandon` Esc already gives; lowercase and uppercase
    /// (Shift+Ctrl+C, which some terminals report as `'C'`) both count.
    #[test]
    fn ctrl_c_abandons_from_the_text_box_and_the_plan_card() {
        for c in ['c', 'C'] {
            assert_eq!(
                text_key_action(KeyCode::Char(c), KeyModifiers::CONTROL),
                TextKeyAction::Abandon,
                "Ctrl+{c} in the text box"
            );
            assert_eq!(
                plan_key_action(KeyCode::Char(c), KeyModifiers::CONTROL),
                PlanKeyAction::Abandon,
                "Ctrl+{c} on the plan card"
            );
        }
        // A bare 'c' (no Control) is ordinary text/a digit-less choice, not
        // an abandon -- the modifier is what makes it Ctrl+C.
        assert_eq!(
            text_key_action(KeyCode::Char('c'), KeyModifiers::NONE),
            TextKeyAction::Insert('c')
        );
        assert_eq!(
            plan_key_action(KeyCode::Char('c'), KeyModifiers::NONE),
            PlanKeyAction::Ignored
        );
    }

    // -- TextBuf cursor editing (ported from the deleted EditLine tests) --

    #[test]
    fn text_buf_move_left_right_are_no_ops_at_either_edge() {
        let mut buf = TextBuf::default();
        buf.insert('a');
        buf.insert('b');
        buf.cursor = 0;
        assert!(
            !buf.move_left(),
            "already at the start -- nothing to move left into"
        );
        assert_eq!(buf.cursor, 0);
        buf.cursor = buf.chars.len();
        assert!(
            !buf.move_right(),
            "already past the last char -- nothing to move right into"
        );
        assert_eq!(buf.cursor, 2);
    }

    #[test]
    fn text_buf_move_home_and_end_are_no_ops_once_already_there() {
        let mut buf = TextBuf::default();
        buf.insert('a');
        buf.insert('b');
        assert!(buf.move_home());
        assert_eq!(buf.cursor, 0);
        assert!(!buf.move_home(), "already at column 0");
        assert!(buf.move_end());
        assert_eq!(buf.cursor, 2);
        assert!(!buf.move_end(), "already at the last column");
    }

    #[test]
    fn text_buf_arrow_keys_relocate_the_cursor_instead_of_only_ever_appending() {
        let mut buf = TextBuf::default();
        for c in "hllo".chars() {
            buf.insert(c);
        }
        assert!(buf.move_left());
        assert!(buf.move_left());
        assert!(buf.move_left());
        assert_eq!(
            buf.cursor, 1,
            "three lefts from the end sits right after 'h'"
        );
        buf.insert('e');
        assert_eq!(
            buf.text(),
            "hello",
            "inserts at the relocated cursor, not the end"
        );
        assert_eq!(buf.cursor, 2);
    }

    #[test]
    fn text_buf_backspace_and_delete_act_on_the_cursor_not_the_end() {
        let mut buf = TextBuf::default();
        for c in "abcd".chars() {
            buf.insert(c);
        }
        buf.cursor = 2; // between 'b' and 'c'
        assert!(buf.backspace());
        assert_eq!(buf.text(), "acd", "erases 'b', the char before the cursor");
        assert_eq!(buf.cursor, 1);
        assert!(buf.delete_forward());
        assert_eq!(buf.text(), "ad", "erases 'c', the char at/after the cursor");
        assert_eq!(buf.cursor, 1);
    }

    #[test]
    fn text_buf_backspace_at_the_start_and_delete_at_the_end_are_no_ops() {
        let mut buf = TextBuf::default();
        buf.insert('a');
        buf.cursor = 0;
        assert!(!buf.backspace());
        buf.cursor = buf.chars.len();
        assert!(!buf.delete_forward());
        assert_eq!(buf.text(), "a");
    }

    /// New in this rewrite (the old single-line `EditLine` never had a
    /// newline to backspace across): backspacing right after a `'\n'`
    /// removes that newline specifically, merging the two lines -- not the
    /// last character of the line above it or a no-op.
    #[test]
    fn text_buf_backspace_across_a_newline_merges_the_two_lines() {
        let mut buf = TextBuf::from_text("a\nb");
        buf.cursor = 2; // right after the newline, before 'b'
        assert!(buf.backspace());
        assert_eq!(buf.text(), "ab");
        assert_eq!(buf.cursor, 1);
    }

    /// The other half of the same review finding as the old `EditLine`'s
    /// `cursor_position_counts_display_cells_not_codepoints`: a CJK glyph is
    /// two cells wide, so counting codepoints instead of cells would put the
    /// cursor a whole column out on any non-ASCII line. `cursor_row_col`
    /// splits on `'\n'` only (this type never soft-wraps -- see its own doc
    /// comment), so this also covers the row half: a multi-byte `char` (CJK
    /// or otherwise) is still exactly one element of `chars`, never split.
    #[test]
    fn text_buf_cursor_row_col_counts_display_cells_for_wide_chars() {
        let mut buf = TextBuf::from_text("日本\na");
        // After "日本\n" (3 chars: 日, 本, '\n'), cursor sits at the start
        // of row 1, column 0.
        buf.cursor = 3;
        assert_eq!(buf.cursor_row_col(), (1, 0));
        // Cursor after just "日" (one CJK glyph, two cells) on row 0.
        buf.cursor = 1;
        assert_eq!(
            buf.cursor_row_col(),
            (0, 2),
            "one CJK glyph occupies two columns"
        );
        // Cursor after "日本" (four cells) still on row 0.
        buf.cursor = 2;
        assert_eq!(buf.cursor_row_col(), (0, 4));
        // Cursor at the very end, row 1 (after the newline), one ASCII
        // column in.
        buf.cursor = 4;
        assert_eq!(buf.cursor_row_col(), (1, 1));
    }

    // -- panic-hook reset bytes --

    /// Review finding: the panic hook used to only disable raw mode.
    /// `panic = "abort"` skips `Drop` entirely, so `Terminal`'s own
    /// cursor-hiding is never undone by it either -- `PANIC_RESET` must show
    /// the cursor again and clear the stale region, the same two things
    /// `install_panic_hook` now writes before chaining into the previous
    /// hook.
    #[test]
    fn panic_reset_shows_the_cursor_and_clears_from_it_to_end_of_screen() {
        assert_eq!(PANIC_RESET, b"\x1b[?25h\x1b[0J");
    }

    // -- rendered screens, captured as plain text via a TestBackend --

    fn render_to_lines(
        width: u16,
        height: u16,
        draw: impl FnOnce(Rect, &mut ratatui::buffer::Buffer),
    ) -> Vec<String> {
        let mut buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, width, height));
        draw(Rect::new(0, 0, width, height), &mut buffer);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn prompt_screen_renders_the_box_and_hint_line() {
        let lines = render_to_lines(100, 4, |area, buf| {
            let mut line = TextBuf::default();
            for c in "fix the flaky retry test".chars() {
                line.insert(c);
            }
            draw_prompt(area, buf, &line);
        });
        assert!(lines[1].contains("fix the flaky retry test"), "{lines:?}");
        assert!(lines[3].contains("esc start without a plan"), "{lines:?}");
    }

    #[test]
    fn sizing_screen_renders_the_elapsed_seconds_and_esc_hint() {
        let lines = render_to_lines(100, 1, |area, buf| draw_sizing(area, buf, 2));
        assert!(lines[0].contains("Sizing the task"), "{lines:?}");
        assert!(lines[0].contains("2s"), "{lines:?}");
        assert!(
            lines[0].contains("esc to start without a plan"),
            "{lines:?}"
        );
    }

    #[test]
    fn plan_screen_renders_every_choice_with_the_selection_marker() {
        let repo = crate::commands::ctx::testenv::repo();
        let cfg = CtxConfig::default();
        let decision = sample_single(Some("bugfix"));
        let card = build_plan_card(&cfg, repo.path(), &decision, &ready(&["claude"]));
        let lines = render_to_lines(76, 12, |area, buf| draw_plan(area, buf, &card, None, 0));
        let text = lines.join("\n");
        assert!(text.contains("1. Start"), "{text}");
        assert!(text.contains("2. Start with a different model"), "{text}");
        assert!(text.contains("3. Start without a workflow"), "{text}");
        assert!(text.contains("4. Edit the task"), "{text}");
        assert!(text.contains("\u{276f} 1. Start"), "{text}");
    }

    #[test]
    fn plan_screen_shows_the_fallback_warning_above_the_card() {
        let repo = crate::commands::ctx::testenv::repo();
        let mut cfg = CtxConfig::default();
        cfg.proxy.typesafe.timeout_secs = 10;
        let mut decision = sample_single(None);
        decision.fallbacks = vec!["typesafe: timed out".to_string()];
        let card = build_plan_card(&cfg, repo.path(), &decision, &ready(&["claude"]));
        let fallback = fallback_notice(&cfg, &decision);
        let lines = render_to_lines(76, 14, |area, buf| {
            draw_plan(area, buf, &card, fallback.as_ref(), 0)
        });
        let text = lines.join("\n");
        assert!(text.contains("did not answer within 10s"), "{text}");
        assert!(text.contains("zirv ctx proxy --json"), "{text}");
    }

    /// Operator follow-up (round 2): the model-pick sub-screen spans every
    /// ready harness, so it ALWAYS uses the prefixed `model_display_name`
    /// form -- never the bare, claude-only style the plan card itself uses.
    #[test]
    fn model_pick_screen_renders_prefixed_harness_and_model_labels() {
        let choices = vec![
            Seat {
                harness: "claude".to_string(),
                model: "haiku".to_string(),
            },
            Seat {
                harness: "codex".to_string(),
                model: "gpt-5.6-luna".to_string(),
            },
        ];
        let lines = render_to_lines(60, 4, |area, buf| draw_model_pick(area, buf, &choices, 1));
        let text = lines.join("\n");
        assert!(text.contains("1. Claude \u{b7} Haiku"), "{text}");
        assert!(
            text.contains("\u{276f} 2. Codex \u{b7} GPT-5.6 Luna"),
            "{text}"
        );
    }

    #[test]
    fn clarify_screen_renders_the_reworded_question_and_hint() {
        let mut decision = sample_single(None);
        decision.clarification_category = Some("target".to_string());
        let lines = render_to_lines(90, 5, |area, buf| {
            draw_clarify(area, buf, clarify_question(&decision), &TextBuf::default())
        });
        let text = lines.join("\n");
        assert!(text.contains("Which part should change"), "{text}");
        assert!(text.contains("tab skip and plan anyway"), "{text}");
    }
}
