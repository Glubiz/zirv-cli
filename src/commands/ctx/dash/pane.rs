//! One dashboard pane: a supervised ConPTY/pty child behind its own
//! PTY panes mirror wrap's ownership and environment scrub so each child is a
//! supervised session; native panes use the same dashboard identity.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use super::super::CtxResult;
use super::super::prompt::PromptRole;
use super::super::sessions::{self, Record, SessionGuard, Verb};
use super::super::signal::SignalServer;
use super::super::state::StateDir;
use super::super::supervise;
use super::super::wrap;

/// Matches `wrap::quit_child`'s own grace period for the same ask-then-
/// escalate shape.
const QUIT_GRACE: Duration = Duration::from_secs(5);

/// A pane's display state, driven by turn signals and the child's own exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneState {
    Working,
    Idle,
    Ended(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneBudgetNotice {
    SoftWarn { used: u64, limit: u64 },
    HardStop { used: u64, limit: u64 },
}

/// Carry the admitted delegation's ledger data on its pane until reap can record actual spend.
#[derive(Debug, Clone)]
pub struct DelegationFacts {
    pub requester: String,
    pub mode: super::super::permit::WorkerMode,
    pub principal: String,
    pub envelope_sha256: Option<String>,
    /// When this pane's child was launched -- the pane-side equivalent of
    /// `exec::ExecutionSegment::wall_ms`'s own clock.
    pub started_at: Instant,
}

/// What `Pane::spawn` needs to launch and register a pane. `argv` is the
/// full program-plus-arguments invocation -- built by the caller from an
/// adapter's `interactive_cmd`/`build_launch`, prompt composition and
/// `AgentAdapter::model_args` already folded in, exactly as `wrap.rs`'s own
/// `launch_command` is.
pub struct PaneSpec {
    pub agent_name: String,
    pub argv: Vec<String>,
    /// Keep the resolved role as the authority for prompt, argv and later delegation checks (#155, #169).
    pub role: PromptRole,
    pub verb: Verb,
    /// uuid, minted by the caller: the pane's registry short id and
    /// turn-signal socket are both derived from this.
    pub session_id: String,
    /// Sidebar label ("orch", "wrk codex", ...).
    pub title: String,
}

/// Derive one successor launch for handover and recovery so both use identical seat data (#552).
pub(crate) struct SwapLaunch {
    pub spec: PaneSpec,
    pub turn_env: Vec<(String, String)>,
    /// The successor adapter's own provider, carried rather than re-derived:
    /// this pane's token reservation moves onto it, and `AgentAdapter::
    /// provider` is the answer the adapter itself gives.
    pub provider: String,
    pub turn_signal_capable: bool,
    pub idle_quiet: Duration,
}

/// Allow a short redraw grace after a turn signal so prompt output does not count as a new turn.
pub(crate) const IDLE_DEBOUNCE: Duration = Duration::from_millis(500);

/// Compiled-in fallback for [`Pane::idle_quiet`] when a caller has no
/// `CtxConfig` to read `dash.idle_quiet_ms` from (every test call site in
/// this module). Matches `DashConfig::default`'s own `idle_quiet_ms` so a
/// test that does not care about this knob still exercises the real default.
pub(crate) const DEFAULT_IDLE_QUIET: Duration = Duration::from_millis(10_000);

/// A turn signal remains valid only until later child output outlives the redraw grace.
pub(crate) fn signal_still_stands(
    signal_at: Option<Instant>,
    output_at: Option<Instant>,
    now: Instant,
    debounce: Duration,
) -> bool {
    let Some(signal) = signal_at else {
        return false;
    };
    // Ignore output before the turn signal when measuring whether that signal still stands.
    now.duration_since(output_at.unwrap_or(signal)) >= debounce
}

/// Use output quiet time as the idle signal only for adapters without turn signals.
pub(crate) fn output_quiescent(output_at: Option<Instant>, now: Instant, quiet: Duration) -> bool {
    let Some(output) = output_at else {
        return false;
    };
    quiescent_since(output, now, quiet)
}

/// Use saturating elapsed-time arithmetic for every quiescence clock.
pub(crate) fn quiescent_since(latest: Instant, now: Instant, quiet: Duration) -> bool {
    now.duration_since(latest) >= quiet
}

/// Take the latest child output or zirv-written input as the quiet-time anchor.
fn latest_of(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

/// Retire pending injection state only after a signal-less pane has been quiet long enough.
fn signal_less_quiescent(
    output_at: Option<Instant>,
    local_input_at: Option<Instant>,
    now: Instant,
    quiet: Duration,
) -> bool {
    output_quiescent(latest_of(output_at, local_input_at), now, quiet)
}

/// Use turn signals when supported; otherwise infer idle only after output and input settle.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pane_is_idle(
    turn_signal_capable: bool,
    signal_at: Option<Instant>,
    output_at: Option<Instant>,
    local_input_at: Option<Instant>,
    now: Instant,
    debounce: Duration,
    idle_quiet: Duration,
) -> bool {
    if turn_signal_capable {
        signal_still_stands(signal_at, output_at, now, debounce)
    } else {
        signal_less_quiescent(output_at, local_input_at, now, idle_quiet)
    }
}

/// A pane remains working while an injection or operator input awaits a turn boundary.
fn state_from(
    signal_stands: bool,
    child_exit: Option<i32>,
    injected_awaiting_turn: bool,
) -> PaneState {
    if let Some(code) = child_exit {
        return PaneState::Ended(code);
    }
    if injected_awaiting_turn {
        return PaneState::Working;
    }
    if signal_stands {
        PaneState::Idle
    } else {
        PaneState::Working
    }
}

/// Inject only when idle and no prior injected or operator input is awaiting a turn.
fn injectable_from(
    state: PaneState,
    injected_awaiting_turn: bool,
    user_typed_since_turn: bool,
) -> bool {
    matches!(state, PaneState::Idle) && !injected_awaiting_turn && !user_typed_since_turn
}

/// Codex 0.159.2 approval dialog text, read from the installed binary's strings (#842). One heading
/// plus a "Yes, " and a "No, " option must all be on screen, so ordinary output never trips it.
const CODEX_APPROVAL_HEADINGS: [&str; 3] = [
    "Would you like to run the following command?",
    "Would you like to make the following edits?",
    "Would you like to grant these permissions?",
];
const CODEX_APPROVAL_OPTIONS: [&str; 2] = ["Yes, ", "No, "];

/// Rows from the bottom of the live screen in which the dialog heading must sit.
const CODEX_DIALOG_BOTTOM_ROWS: usize = 20;

/// An option row starts with a selection marker or a number.
fn is_dialog_option_line(line: &str) -> bool {
    line.trim_start()
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_digit() || matches!(c, '\u{203a}' | '>' | '\u{276f}'))
}

/// The heading row and every row below it, when the LIVE screen text ends in a dialog heading.
fn codex_dialog_rows(live_contents: &str) -> Option<(&str, Vec<&str>)> {
    let lines: Vec<&str> = live_contents.lines().collect();
    let tail = &lines[lines.len().saturating_sub(CODEX_DIALOG_BOTTOM_ROWS)..];
    let heading = tail.iter().rposition(|line| {
        CODEX_APPROVAL_HEADINGS
            .iter()
            .any(|h| line.trim().starts_with(h))
    })?;
    Some((tail[heading].trim(), tail[heading + 1..].to_vec()))
}

/// Whether the LIVE screen text shows a Codex approval dialog that a typed line or Enter would answer (#842):
/// a heading line near the bottom, with "Yes, " and "No, " option rows below it. The same strings inside
/// ordinary output (a diff, source code) have the wrong layout and never match.
pub(crate) fn codex_approval_dialog_shown(live_contents: &str) -> bool {
    let Some((_, options)) = codex_dialog_rows(live_contents) else {
        return false;
    };
    CODEX_APPROVAL_OPTIONS.iter().all(|wanted| {
        options
            .iter()
            .any(|line| is_dialog_option_line(line) && line.contains(wanted))
    })
}

/// What an open Codex approval dialog shows, read from the screen: the request details plus the key each
/// option answers with (the "(y)" and "(p)" hints Codex draws after the option text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodexDialog {
    pub tool: &'static str,
    pub command: String,
    pub details: super::super::approvals::RequestDetails,
    allow_key: Option<char>,
    always_key: Option<char>,
}

impl CodexDialog {
    /// The request record for this dialog, owned by dashboard `dash_pid`.
    pub fn request(&self, short: &str, dash_pid: u32) -> super::super::approvals::Request {
        super::super::approvals::Request::new(
            short,
            self.tool,
            &self.command,
            &self.command,
            dash_pid,
        )
        .with_details(self.details.clone())
    }

    /// The key that selects the option for `decision`; `None` when the dialog has no such option.
    /// AllowAlways answers only through Codex's own "don't ask again" option, never any other.
    pub fn answer_key(&self, decision: super::super::approvals::Decision) -> Option<char> {
        use super::super::approvals::Decision;
        match decision {
            Decision::Allow => self.allow_key,
            Decision::AllowAlways => self.always_key,
            _ => None,
        }
    }
}

/// An option row's text without its selection marker and number, and the one-character key it ends with.
fn dialog_option(line: &str) -> (String, Option<char>) {
    let text = line
        .trim_start()
        .trim_start_matches(['\u{203a}', '>', '\u{276f}', ' '])
        .trim_start_matches(|c: char| c.is_ascii_digit())
        .trim_start_matches('.')
        .trim();
    let key = text
        .strip_suffix(')')
        .and_then(|rest| rest.rsplit_once('('))
        .and_then(|(_, key)| {
            let mut chars = key.chars();
            chars.next().filter(|_| chars.next().is_none())
        });
    let label = match (key, text.rsplit_once('(')) {
        (Some(_), Some((label, _))) => label.trim(),
        _ => text,
    };
    (label.to_string(), key)
}

/// Read the open dialog's details from the LIVE screen text; `None` when no dialog is shown.
pub(crate) fn codex_approval_dialog(live_contents: &str) -> Option<CodexDialog> {
    if !codex_approval_dialog_shown(live_contents) {
        return None;
    }
    let (heading, rows) = codex_dialog_rows(live_contents)?;
    let first_option = rows.iter().position(|line| is_dialog_option_line(line))?;
    let mut reason = None;
    let mut command = Vec::new();
    for line in rows[..first_option].iter().map(|line| line.trim()) {
        if line.is_empty() {
            continue;
        }
        match line.strip_prefix("Reason:") {
            Some(text) => reason = Some(text.trim().to_string()),
            None => command.push(line.strip_prefix("$ ").unwrap_or(line)),
        }
    }
    let (mut allow_key, mut always_key, mut always) = (None, None, None);
    for line in &rows[first_option..] {
        if !is_dialog_option_line(line) {
            continue;
        }
        let (label, key) = dialog_option(line);
        if label.starts_with("Yes, proceed") || label.starts_with("Yes, just this once") {
            allow_key = allow_key.or(key);
        } else if let Some(rule) = label.strip_prefix("Yes, and don't ask again for ") {
            always_key = always_key.or(key);
            always = always.or(Some(rule.to_string()));
        }
    }
    Some(CodexDialog {
        tool: if heading.contains("edits") {
            "Edit"
        } else if heading.contains("permissions") {
            "Permissions"
        } else {
            "Bash"
        },
        command: command.join(" "),
        details: super::super::approvals::RequestDetails {
            cwd: None,
            outside_sandbox: heading.contains("permissions")
                || reason.as_deref().is_some_and(|r| r.contains("sandbox")),
            reason,
            // Only an option Codex drew with a key can be selected, so only that is offered.
            always: always.filter(|_| always_key.is_some()),
        },
        allow_key,
        always_key,
    })
}

/// Use the bottom nonblank screen row for sidebar preview.
fn last_line_of(screen: &vt100::Screen) -> String {
    let (rows, cols) = screen.size();
    for row in (0..rows).rev() {
        let mut line = String::new();
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                line.push_str(cell.contents());
            }
        }
        let trimmed = line.trim_end();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    String::new()
}

/// Mark injected text as a zirv announcement and bound its label and body.
fn visible_injection_line(label: &str, body: &str) -> String {
    format!("[zirv \u{25b8} {label}] {body}")
}

/// Use the same submit delay as wrap so the child can process the visible
/// line before a carriage return; a shorter gap can fold it into a paste (#114).
pub(crate) use super::super::INJECTION_SUBMIT_DELAY;

/// Flush the visible line without control bytes before scheduling its submit carriage return.
pub(crate) fn write_injection_phase1(
    writer: &mut dyn Write,
    label: &str,
    body: &str,
) -> std::io::Result<()> {
    let line = visible_injection_line(&scrub_controls(label), &scrub_controls(body));
    writer.write_all(line.as_bytes())?;
    writer.flush()?;
    Ok(())
}

/// Submit only after the visible line is flushed; carriage return is the sole
/// control byte so injected body text cannot send extra terminal commands.
pub(crate) fn write_submit_cr(writer: &mut dyn Write) -> std::io::Result<()> {
    writer.write_all(b"\r")?;
    writer.flush()?;
    Ok(())
}

/// A deferred submit is due only after its deadline.
pub(crate) fn submit_is_due(pending: Option<Instant>, now: Instant) -> bool {
    pending.is_some_and(|deadline| now >= deadline)
}

/// Clamp untrusted timeout seconds before Instant arithmetic; a forged huge
/// value must not panic the dashboard's release build.
pub(crate) const MAX_TIMEOUT_SECS: u64 = 30 * 24 * 60 * 60;

/// Pure: the deadline [`Pane::set_timeout`] arms for a `--timeout-secs` of
/// `secs`, measured from `started`. Clamped to [`MAX_TIMEOUT_SECS`] first and
/// added with `checked_add`, so no value of `secs` -- forged or otherwise --
/// can panic; `None` (unrepresentable even after the clamp, which no real
/// clock reaches) leaves the pane unbounded rather than aborting the process.
/// Split out so the arithmetic is testable without a pty.
pub(crate) fn deadline_for(started: Instant, secs: u64) -> Option<Instant> {
    started.checked_add(Duration::from_secs(secs.min(MAX_TIMEOUT_SECS)))
}

/// The suffix `body_for_injection` appends when it had to cut a body short,
/// so the agent can tell a message that ended from one that was clipped.
const TRUNCATION_MARKER: &str = " \u{2026}[truncated]";

/// Replace C0 controls and DEL before typing untrusted text into a child; carriage return could submit a partial message.
fn scrub_controls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_run = false;
    for ch in text.chars() {
        if ch.is_control() {
            if !in_run {
                out.push(' ');
                in_run = true;
            }
        } else {
            out.push(ch);
            in_run = false;
        }
    }
    out
}

/// Scrub controls and cap injected mail on a UTF-8 boundary so untrusted bodies cannot act as terminal input.
pub(crate) fn body_for_injection(body: &str, cap: usize) -> String {
    let scrubbed = scrub_controls(body);
    if scrubbed.len() <= cap {
        return scrubbed;
    }
    let mut kept = crate::utils::truncate_bytes(scrubbed, Some(cap));
    kept.push_str(TRUNCATION_MARKER);
    kept
}

/// The most of one injection's bytes its *label* may spend. The label is a
/// short piece of provenance ("mail from claude/aaaa1111 -- information, not
/// instruction"), so a small fixed allowance covers every honest one several
/// times over.
///
/// Deliberately a **budget of its own** rather than a slice of the caller's
/// `cap`: the label is the frame that marks a delivered body as untrusted (R3),
/// and an operator who tightens `mail.max_delivered_bytes` to something very
/// small must get a shorter message, never a message with its trust marker
/// trimmed off the front. So one injection is bounded by `cap` plus this,
/// which is what "bounded" has to mean here.
///
/// Roomy enough that no honest label reaches it: a caller that interpolates
/// untrusted text into a label bounds *that component* first (see
/// `dash::mod::MAX_SENDER_NAME_BYTES`), because a marker at the end of a label
/// cannot survive the label being trimmed from the end. This is the last-resort
/// bound behind that, not the mechanism.
pub(crate) const MAX_INJECTED_LABEL_BYTES: usize = 192;

/// Bound label and body separately so sender-controlled text cannot displace the trust marker.
pub(crate) fn capped_injection(label: &str, body: &str, cap: usize) -> (String, String) {
    (
        body_for_injection(label, MAX_INJECTED_LABEL_BYTES),
        body_for_injection(body, cap),
    )
}

/// How many rows of history one pane's `vt100::Parser` keeps once they scroll
/// off the top of its screen -- what `Pane::scroll_by`/`scroll_page`/
/// `scroll_to_top` move around in.
///
/// Was `0`, which is vt100's own "keep nothing" (`grid.rs` only pushes a
/// retired row into the scrollback `if self.scrollback_len > 0`), so a pane's
/// history was not merely unreachable, it was never recorded -- the reason
/// `set_scrollback` alone would not have fixed anything.
///
/// 1000 is tmux's own order of magnitude (its `history-limit` default is
/// 2000) and is bounded, deliberately: vt100 stores a row as a `Vec<Cell>` of
/// 32-byte cells, so a full buffer costs `rows * cols * 32` -- about 6 MB per
/// pane at 200 columns, and only after 1000 rows have actually scrolled off
/// that pane. Nine of those (`dash.max_panes`) is the worst case, and the
/// worst case is a dashboard that has been running long enough to have earned
/// it.
const SCROLLBACK_ROWS: usize = 1000;

/// Pure: the scrollback offset `current` moves to under a scroll of `delta`
/// rows -- positive back into history, negative toward the live view -- held
/// inside `[0, max]`.
///
/// Both ends are real: `0` is the live bottom, past which "scroll down" is a
/// no-op rather than an underflow (`current` is a `usize`), and `max` is
/// however much history that pane has actually accumulated, past which
/// "scroll up" stops instead of running off into blank rows. `isize`
/// arithmetic throughout, so a wheel burst of many notches cannot wrap.
pub(crate) fn scroll_offset(current: usize, delta: isize, max: usize) -> usize {
    let want = (current as isize).saturating_add(delta);
    if want <= 0 {
        return 0;
    }
    (want as usize).min(max)
}

/// What one scroll request actually did to a pane, so the dashboard can say so
/// instead of leaving the operator with a viewport that did not move and no
/// explanation (the reported failure mode: "the chat window is still not
/// scrollable").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollOutcome {
    /// A scrollback offset identifies rows behind the live screen.
    Scrolled(usize),
    /// Branch (A): asked to go further back, but this pane has no more
    /// recorded history.
    AtOldest,
    /// Branch (A): asked to come forward, but the pane is already live.
    AtLive,
    /// Branch (B): the child turned mouse reporting on, so the wheel event was
    /// encoded in the protocol it asked for and handed to it. The child scrolls
    /// itself; the dashboard's own scrollback is not involved.
    ForwardedMouse,
    /// Branch (C): the child is on the alternate screen and did *not* ask for
    /// mouse events. There is no history to show and nobody to hand the event
    /// to, so the only honest thing to do is say so.
    FullScreen,
}

/// Wheel-up's button number in the xterm mouse protocol; wheel-down is the
/// next one. The wheel is reported as buttons 64/65 (the 0b0100_0000 bit is
/// what marks a button number as a wheel event) in every encoding.
const MOUSE_WHEEL_UP: u8 = 64;
const MOUSE_WHEEL_DOWN: u8 = 65;

/// The largest coordinate the default (X10) encoding can express: it packs
/// `32 + coordinate` into one byte, so 223 is the end of the line. `?1006`
/// (SGR) exists precisely because terminals are routinely wider than that,
/// and it is what the dashboard asks its own terminal for
/// (`term::dash_mouse_on_bytes`) -- but a *child* picks its own encoding, so
/// the classic form still has to be encodable.
const MOUSE_X10_MAX: u16 = 223;

/// Pure: one mouse event encoded the way `encoding` says the child wants it.
///
/// `col`/`row` are **pane-local and 1-based** -- the child believes it owns a
/// terminal that starts at its own top-left, so a frame coordinate handed
/// straight through would make it act on the wrong row, which is worse than
/// not scrolling at all. `dash::pane_local_mouse` does the translation.
///
/// `press` picks SGR's final byte (`M` for a press, `m` for a release); wheel
/// events are always presses, in every encoding. The classic encodings cannot
/// say *which* button was released, so a release there is the protocol's
/// "some button came up" code (3) rather than the button's own number.
pub(crate) fn mouse_report_bytes(
    encoding: vt100::MouseProtocolEncoding,
    button: u8,
    col: u16,
    row: u16,
    press: bool,
) -> Vec<u8> {
    match encoding {
        vt100::MouseProtocolEncoding::Sgr => {
            let final_byte = if press { 'M' } else { 'm' };
            format!("\x1b[<{button};{col};{row}{final_byte}").into_bytes()
        }
        // The classic `ESC [ M` form, and its UTF-8 variant (`?1005`), which
        // differ only in how a coordinate past 95 is written: one raw byte
        // versus that code point encoded as UTF-8. Both offset by 32, and
        // both clamp rather than wrapping a coordinate they cannot express.
        encoding => {
            let mut out = b"\x1b[M".to_vec();
            let utf8 = matches!(encoding, vt100::MouseProtocolEncoding::Utf8);
            let button = if press { button } else { 3 };
            for value in [u16::from(button), col, row] {
                let value = value.min(MOUSE_X10_MAX) + 32;
                if utf8 {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(
                        char::from_u32(u32::from(value))
                            .unwrap_or(' ')
                            .encode_utf8(&mut buf)
                            .as_bytes(),
                    );
                } else {
                    out.push(value as u8);
                }
            }
            out
        }
    }
}

/// Branch (A), against a parser rather than a whole pane: moves `parser`'s
/// scrollback offset by `delta` rows and reports what happened. Split out so
/// the clamped ends -- and the alternate screen's total absence of scrollback
/// -- are testable without a pty child.
fn scroll_parser(parser: &mut vt100::Parser, delta: isize) -> ScrollOutcome {
    let before = parser.screen().scrollback();
    let want = scroll_offset(before, delta, usize::MAX);
    parser.screen_mut().set_scrollback(want);
    let after = parser.screen().scrollback();
    if after != before {
        ScrollOutcome::Scrolled(after)
    } else if delta > 0 {
        ScrollOutcome::AtOldest
    } else {
        ScrollOutcome::AtLive
    }
}

/// Bound vt100 parsing per tick so one noisy child cannot starve UI input or other panes.
pub(crate) const DRAIN_BUDGET_BYTES: usize = 256 * 1024;

/// Process at most the assigned byte budget and report whether output remains,
/// so the UI returns next tick instead of one noisy child blocking it.
fn drain_into(
    rx: &mpsc::Receiver<Vec<u8>>,
    parser: &mut vt100::Parser,
    budget: usize,
) -> (bool, bool, usize) {
    // A zero budget must consume nothing; otherwise each pane could overshoot the shared cap.
    if budget == 0 {
        return (false, true, 0);
    }
    let mut processed = 0usize;
    let mut any = false;
    loop {
        match rx.try_recv() {
            Ok(bytes) => {
                processed += bytes.len();
                parser.process(&bytes);
                any = true;
                if processed >= budget {
                    // Stopped on the budget, not on an empty channel: treat
                    // as "more may remain" so the loop returns here next
                    // tick. Nothing is dropped -- what is left stays queued
                    // exactly as it was.
                    return (any, true, processed);
                }
            }
            // Empty or Disconnected: this pane has nothing more to take.
            Err(_) => return (any, false, processed),
        }
    }
}

/// A supervised ConPTY/pty child rendered through its own `vt100` screen.
pub struct Pane {
    title: String,
    agent_name: String,
    /// Mail may be body-injected only into workers; the orchestrator receives a notice.
    verb: Verb,
    /// Keep the role granted at spawn as the authority for later delegation checks (#169).
    role: PromptRole,
    session_id: String,
    parser: vt100::Parser,
    /// A Codex approval dialog is on the LIVE screen, whatever the scrollback offset (#842).
    approval_dialog: bool,
    /// Separate wrapped PTY ownership from native in-process session ownership (#490).
    kind: PaneKind,
    /// An automatic successor under observation; the source still owns this pane.
    pending_handover: Option<PendingHandover>,
    /// Remember whether channel output remains after this pane's budget share, delaying reap until it drains (#330).
    pending_output: bool,
    guard: SessionGuard,
    state_dir: StateDir,
    /// Keep signal and output timestamps so prompt redraws can be distinguished from a new turn.
    last_signal_at: Option<Instant>,
    last_output_at: Option<Instant>,
    /// Whether this pane's adapter has a real turn-signal mechanism
    /// (`AgentAdapter::capabilities().turn_signal`), captured once at spawn
    /// time from the adapter the caller resolved for `spec.agent_name`. Drives
    /// which branch of [`pane_is_idle`] `state()` uses, and whether `drain()`
    /// clears a pending injection/operator-typing flag on quiescence rather
    /// than waiting for a turn signal that will never come for this pane.
    turn_signal_capable: bool,
    /// `dash.idle_quiet_ms`, resolved to a `Duration` once at spawn time --
    /// the quiet window [`output_quiescent`] measures a signal-less pane's
    /// idleness against. Unread by a signal-carrying pane.
    idle_quiet: Duration,
    /// Count zirv-written input as activity when deciding signal-less idle time.
    last_local_input_at: Option<Instant>,
    /// Set by a successful `inject_visible`, cleared by the next turn signal
    /// (`on_turn_signal`): "this pane was handed something to do and has not
    /// reported finishing it yet." See `state_from`'s own doc comment -- this
    /// is what keeps two idle-gated injections out of the same tick.
    injected_awaiting_turn: bool,
    /// Keep operator typing pending until the next turn signal, blocking idle-gated injection.
    user_typed_since_turn: bool,
    /// Set while this dashboard holds an `Approval` attention latch for a Codex approval dialog on screen (#842).
    pub(super) codex_approval_latched: bool,
    exit_code: Option<i32>,
    native_stop_code: Option<i32>,
    /// Monotonic launch age captured when the real child exit is observed.
    launched_at: Instant,
    exited_after: Option<Duration>,
    /// Idempotency guard for `shutdown` -- the release profile is
    /// `panic = "abort"`, so `Drop` is not guaranteed and every exit arm
    /// that leaves a pane's owner must call `shutdown` explicitly (mirrors
    /// `RawGuard`/`SessionGuard`'s own `done`/`released` fields).
    done: bool,
    /// Keep the worker's report-back address for one-shot reminder delivery (#115).
    report_to: Option<String>,
    /// Keep the exact directory given through DASH_REQUESTS_ENV; attribution
    /// is valid only when the child's path and dashboard's drain path match.
    intake_dir: Option<PathBuf>,
    /// Keep the admitted work group for closure and restoration.
    work_group_id: Option<String>,
    /// Per-child token ceiling carried by the spawn request. Evaluated from
    /// this pane's transcript with the same `agent::budget_state` and
    /// one-tick hard-stop grace as the headless exec supervisor.
    budget_tokens: Option<u64>,
    /// Reuse the budget sweep's usage; reset it after handover because a
    /// successor's spend must never inherit its predecessor's value (#354).
    measured_usage: Option<super::super::event::TranscriptUsage>,
    /// Read the launched model from resolved adapter argv, not untrusted request text (#354).
    launch_model: Option<String>,
    /// Keep the provider reservation ID until actual spend can settle it (#358).
    reservation_id: Option<String>,
    budget_soft_warned: bool,
    budget_grace_given: bool,
    /// Keep the pane's wall-clock deadline from its accepted request.
    deadline: Option<Instant>,
    /// Keep the server-verified parent session for steering trust (#249).
    parent_session: Option<String>,
    /// Whether `report_back_reminder_sweep`'s one-shot completion reminder
    /// has already been injected into this pane. Set the moment that
    /// injection succeeds and never cleared again -- unlike
    /// `injected_awaiting_turn`, a turn boundary does not reset it, because
    /// the whole point is "remind at most once in this pane's life," not
    /// "once per turn."
    report_reminder_sent: bool,
    pub(crate) settled_mail_sent: bool,
    /// Send at most one stalled-compaction mail per logical pane (#379).
    pub(crate) stalled_mail_sent: bool,
    /// Pair attention-blocked mail with its specific message ID for later delivery logging (#468).
    pub(crate) mail_block_log: Option<(&'static str, String)>,
    pub(crate) result_schema: Option<String>,
    /// Keep deferred submit deadline until its carriage return is written or cancelled (#116).
    pending_submit: Option<Instant>,
    submit_confirmation: Option<(Instant, bool)>,
    pub(crate) delivery_sender: Option<String>,
    pub(crate) last_injection_at: Instant,
    /// Derive actual launch mode from turn environment for fail-closed restoration (#160).
    launch_mode: super::super::adapters::LaunchMode,
    /// Hold the writer permit until this writing pane is reaped (#264).
    writer_permit: Option<super::super::permit::HeavyPermit>,
    /// Keep actual child cwd for transcript reads and worktree cleanup.
    cwd: PathBuf,
    /// Reclaim a linked worktree only when this pane's request explicitly owns it (#267).
    owns_cwd: bool,
    /// The ledger row this pane owes once its child exits, when it is
    /// fulfilling a delegation at all -- `None` for the dashboard's own
    /// orchestrator pane and for a restored pane, neither of which is
    /// anybody's delegation. See [`DelegationFacts`].
    delegation: Option<DelegationFacts>,
}

/// The pane driver owns either a wrapped child or a native session (#490).
pub enum PaneKind {
    Wrapped(PtyPane),
    Native(Box<super::native_pane::NativePaneRuntime>),
    /// A native pane whose session has already been ended. Shutting a native
    /// session down consumes its driver, and the release profile is
    /// `panic = "abort"` so `Drop` cannot be relied on -- parking the pane
    /// here is what makes `shutdown`/`finish_shutdown` idempotent for a
    /// native pane without a second flag beside `done`.
    Ended,
}

/// Keep process-owning state together in the wrapped driver.
pub struct PtyPane {
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// Hold the console-close registry and job guard for the child's lifetime so dashboard death cannot orphan it.
    lifecycle: supervise::ChildGuard,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    rx: mpsc::Receiver<Vec<u8>>,
    server: Option<SignalServer>,
}

fn rollover_receipt_prompt(
    req: &super::super::handover::HandoverRequest,
    prompt: String,
    session: &str,
) -> String {
    if req.generation.is_none() {
        return prompt;
    }
    format!(
        "{prompt} Rollover readiness check: acknowledge receipt of this handoff only. \
        Do not use tools, delegate, or continue the task yet. The original session still owns \
        the work. Reply with {} and wait for zirv's next message confirming the transfer before proceeding.",
        rollover_receipt_token(req, session),
    )
}

/// Keep the staged socket's file stem at the seat short ID: hooks file their
/// markers under that stem even before the successor commits (#681).
fn staged_socket_path(state: &StateDir, short: &str) -> PathBuf {
    let live = state.socket_for(short);
    loop {
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let path = live.with_extension(&nonce[..4]);
        if !path.exists() {
            return path;
        }
    }
}

fn rollover_receipt_token(req: &super::super::handover::HandoverRequest, session: &str) -> String {
    format!(
        "zirv-rollover-ready-{}-{}-{}",
        session,
        req.generation.unwrap_or(0),
        req.requested_at
    )
}

/// Fully assembled successor, kept separate until it proves ready. Its socket
/// and screen cannot consume the source's signals or replace its scrollback.
struct PendingHandover {
    pty: PtyPane,
    argv: Vec<String>,
    agent: String,
    provider: String,
    quit_sequence: String,
    turn_signal_capable: bool,
    idle_quiet: Duration,
    launched_at: Instant,
    handover_at: u64,
    parser: vt100::Parser,
    last_output_at: Option<Instant>,
    signal_seen: bool,
    forced_drain: bool,
    started_ms: u64,
    rollout: Option<PathBuf>,
    last_readiness_poll: Option<Instant>,
    source_input_at: Option<Instant>,
    receipt: String,
    source_conversation: Option<String>,
}

impl PendingHandover {
    fn stop(&mut self) {
        #[cfg(not(unix))]
        if let Some(pid) = self.pty.child.process_id() {
            supervise::kill_tree(pid);
        }
        let _ = self.pty.child.kill();
        let _ = self.pty.child.wait();
        self.pty.lifecycle.release();
    }
}

/// The one message every pty-only operation refuses a native pane with.
pub(crate) const NOT_A_PTY_PANE: &str =
    "dashboard pane: this is a native pane; it has no pty to write to";

impl Pane {
    /// Whether this pane is driven by an in-process native conversation
    /// rather than a wrapped harness behind a pty.
    pub fn is_native(&self) -> bool {
        matches!(self.kind, PaneKind::Native(_))
    }

    pub fn native(&self) -> Option<&super::native_pane::NativePaneRuntime> {
        match &self.kind {
            PaneKind::Native(native) => Some(native),
            _ => None,
        }
    }

    pub fn native_mut(&mut self) -> Option<&mut super::native_pane::NativePaneRuntime> {
        match &mut self.kind {
            PaneKind::Native(native) => Some(native),
            _ => None,
        }
    }

    fn pty(&self) -> Option<&PtyPane> {
        match &self.kind {
            PaneKind::Wrapped(pty) => Some(pty),
            _ => None,
        }
    }

    fn pty_mut(&mut self) -> Option<&mut PtyPane> {
        match &mut self.kind {
            PaneKind::Wrapped(pty) => Some(pty),
            _ => None,
        }
    }

    /// The writer for a wrapped pane. A native pane has no pty at all, and
    /// saying so explicitly is what keeps every byte-level path (key
    /// forwarding, the quit sequence, a visible injection) from silently
    /// doing nothing on one.
    fn writer(&self) -> CtxResult<&Arc<Mutex<Box<dyn Write + Send>>>> {
        match &self.kind {
            PaneKind::Wrapped(pty) => Ok(&pty.writer),
            _ => Err(NOT_A_PTY_PANE.into()),
        }
    }

    /// Polls a wrapped pane's child. A native pane has no child, so it never
    /// reports one exiting -- its own end is observed through `tick_native`.
    fn pty_try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
        match &mut self.kind {
            PaneKind::Wrapped(pty) => pty.child.try_wait(),
            _ => Ok(None),
        }
    }

    fn release_lifecycle(&mut self) {
        if let PaneKind::Wrapped(pty) = &mut self.kind {
            pty.lifecycle.release();
        }
    }

    /// Advances a native pane one tick and reports whether anything changed.
    /// The wrapped pane's `drain` equivalent: it is the one place the
    /// conversation is re-read, the approval channel is polled, and the
    /// session's own end is turned into this pane's `exit_code` so
    /// `dash::reap_ended_panes` retires a finished native pane by exactly the
    /// same path it retires a finished wrapped one.
    fn tick_native(&mut self) -> bool {
        let PaneKind::Native(native) = &mut self.kind else {
            return false;
        };
        let before = native.last_sequence();
        native.tick();
        let ended = native.ended;
        let changed = native.last_sequence() != before;
        if ended && self.exit_code.is_none() {
            self.exit_code = Some(self.native_stop_code.unwrap_or(0));
            self.exited_after = Some(self.launched_at.elapsed());
        }
        if changed {
            self.last_output_at = Some(Instant::now());
        }
        changed
    }

    /// Ends a native pane's session exactly once. Idempotent, because both
    /// `shutdown` and `finish_shutdown` can reach it and the release profile
    /// is `panic = "abort"`, so neither may rely on `Drop`.
    fn finish_native(&mut self) {
        let state = self.state_dir.clone();
        if let PaneKind::Native(_) = &self.kind {
            // Replacing the driver with an already-ended one is how this stays
            // idempotent without a second flag: the shutdown consumes the
            // runtime, and what is left behind can only be shut down again as
            // a no-op.
            if let PaneKind::Native(native) = std::mem::replace(&mut self.kind, PaneKind::Ended) {
                native.shutdown(&state);
            }
        }
    }

    /// Open a native session through the runtime seam while retaining the dashboard pane identity (#490).
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_native(
        cfg: &super::super::config::CtxConfig,
        state: &StateDir,
        env: super::super::config::EnvLookup<'_>,
        repo: &Path,
        verb: Verb,
        title: String,
        size: (u16, u16),
        spec: super::native_pane::NativeDashboardSpec,
    ) -> CtxResult<Pane> {
        let (cols, rows) = size;
        // An unrecognised role label is the orchestrator's, the same
        // default a dashboard's own pane has always had.
        let role = PromptRole::from_label(&spec.role).unwrap_or(PromptRole::Orchestrator);
        let cwd = spec.repo.clone();
        let native = super::native_pane::open_native_pane(cfg, state, env, spec)?;
        // The design note's own rule, applied to the sidebar: an operator has
        // to be able to tell at a glance whether closing this pane stops the
        // conversation or merely detaches from it.
        let title = match native.attach() {
            super::native_pane::PaneAttach::Runtime { .. } => format!("{title} (runtime)"),
            super::native_pane::PaneAttach::InProcess => title,
        };
        let session_id = native.journal_session();
        let agent_name = super::super::runtime::RuntimeKind::Native
            .as_str()
            .to_string();

        let guard = SessionGuard::register(
            state,
            Record::new(&session_id, &agent_name, repo, verb)
                .with_stable_short(native.short())
                .with_role(role.label()),
        );

        Ok(Pane {
            title,
            agent_name,
            verb,
            role,
            session_id,
            parser: vt100::Parser::new(rows, cols, SCROLLBACK_ROWS),
            approval_dialog: false,
            kind: PaneKind::Native(Box::new(native)),
            pending_handover: None,
            pending_output: false,
            guard,
            state_dir: state.clone(),
            last_signal_at: None,
            last_output_at: None,
            // A native pane has no turn-signal socket and no quiet-window
            // heuristic: `state()` reads the driver's own session state
            // directly, which is authoritative rather than inferred.
            turn_signal_capable: true,
            idle_quiet: Duration::from_millis(0),
            last_local_input_at: None,
            injected_awaiting_turn: false,
            user_typed_since_turn: false,
            codex_approval_latched: false,
            exit_code: None,
            native_stop_code: None,
            launched_at: Instant::now(),
            exited_after: None,
            done: false,
            report_to: None,
            intake_dir: None,
            work_group_id: None,
            budget_tokens: None,
            measured_usage: None,
            launch_model: None,
            reservation_id: None,
            budget_soft_warned: false,
            budget_grace_given: false,
            deadline: None,
            parent_session: None,
            report_reminder_sent: false,
            settled_mail_sent: false,
            stalled_mail_sent: false,
            mail_block_log: None,
            result_schema: None,
            pending_submit: None,
            submit_confirmation: None,
            delivery_sender: None,
            last_injection_at: Instant::now(),
            launch_mode: super::super::adapters::LaunchMode::Interactive,
            writer_permit: None,
            cwd,
            owns_cwd: false,
            delegation: None,
        })
    }

    /// Scrub inherited supervision env before applying this pane's turn env;
    /// a child must not inherit its dashboard parent's identity.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        spec: PaneSpec,
        state: &StateDir,
        cwd: &Path,
        repo: &Path,
        size: (u16, u16),
        turn_env: &[(String, String)],
        turn_signal_capable: bool,
        idle_quiet: Duration,
    ) -> CtxResult<Pane> {
        Self::spawn_on_seat(
            spec,
            state,
            cwd,
            repo,
            size,
            turn_env,
            turn_signal_capable,
            idle_quiet,
            None,
        )
    }

    /// Use an existing seat ID for a successor, otherwise derive a fresh one from the session (#552).
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_on_seat(
        spec: PaneSpec,
        state: &StateDir,
        cwd: &Path,
        repo: &Path,
        size: (u16, u16),
        turn_env: &[(String, String)],
        turn_signal_capable: bool,
        idle_quiet: Duration,
        seat_short: Option<&str>,
    ) -> CtxResult<Pane> {
        let PaneSpec {
            agent_name,
            mut argv,
            role,
            verb,
            session_id,
            title,
        } = spec;

        let mcp_args = super::super::mcp::launch::arguments(
            &agent_name,
            repo,
            state,
            seat_short.unwrap_or(&sessions::short_id(&session_id)),
            &argv,
        );
        super::super::mcp::launch::append(&mut argv, mcp_args);

        let (cols, rows) = size;
        let pair = native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let (program, rest) = argv
            .split_first()
            .ok_or("dashboard pane: empty argv, nothing to spawn")?;
        // Guard PTY argv against Windows command-shim reparsing before spawn.
        super::super::adapters::guard_cmd_shim_reparse(program, rest)?;
        let mut command = CommandBuilder::new(program);
        for arg in rest {
            command.arg(arg);
        }
        command.cwd(cwd);

        sessions::scrub_supervision_env(&mut command);
        let stripped = scrub_worker_pane_env(&mut command, role, repo, &agent_name);
        sessions::secret_env::log_withheld(state, &session_id, "pane", &stripped);
        // Derive launch mode from the same environment given to the child (#160).
        let launch_mode = if turn_env.iter().any(|(k, v)| {
            k == super::super::adapters::LAUNCH_MODE_ENV
                && v == super::super::adapters::LAUNCH_MODE_INTERACTIVE_VALUE
        }) {
            super::super::adapters::LaunchMode::Interactive
        } else {
            super::super::adapters::LaunchMode::Headless
        };
        for (key, value) in turn_env {
            command.env(key, value);
        }

        // Answer Windows console-host cursor inheritance before it services the child.
        let mut first_writer = pair.master.take_writer()?;
        wrap::answer_inherit_cursor_probe(&mut *first_writer);
        let writer = Arc::new(Mutex::new(first_writer));

        // Publish a fresh seat's inbox before the host bridge starts; keep an existing predecessor's record until commit.
        let register = || {
            let server = SignalServer::bind(&state.socket_for(&session_id)).ok();
            if let Some(server) = &server {
                wrap::publish_socket_path(state, &session_id, server.path());
            }

            let mut record =
                Record::new(&session_id, &agent_name, repo, verb).with_role(role.label());
            // Bind rollover successor reporting to the existing seat address (#552).
            if let Some(seat_short) = seat_short {
                record = record.with_stable_short(seat_short);
            }
            // `owner_pid` is left unset here: `SessionGuard::register` below
            // stamps it with this process's own pid -- the dashboard's -- for
            // every pane, orchestrator and worker alike, the same seam every
            // other registration path shares (`sessions::Record::owner_pid`,
            // `dash::assemble_sidebar`).
            let record = if server.is_some() {
                record
            } else {
                record.unreachable()
            };
            (server, SessionGuard::register(state, record))
        };
        let registry_short = seat_short
            .map(str::to_string)
            .unwrap_or_else(|| sessions::short_id(&session_id));
        let registered = (!state
            .sessions()
            .join(format!("{registry_short}.json"))
            .exists())
        .then(register);

        let launched_at = Instant::now();
        let child = pair.slave.spawn_command(command)?;
        // Adopt child ownership immediately after spawn before later fallible setup can leak it.
        let lifecycle = supervise::ChildGuard::adopt(child.process_id());
        let (server, mut guard) = registered.unwrap_or_else(register);
        if let Some(pid) = child.process_id() {
            guard.adopt_child_pid(pid);
        }
        // Lower child priority so build work cannot inherit dashboard UI priority (#330).
        if let Some(pid) = child.process_id() {
            super::super::priority::apply_to_child(pid, super::super::priority::posture_for(role));
        }
        // The slave side is not needed past the spawn; dropping it here
        // (rather than keeping the whole `PtyPair` alive) mirrors the
        // explicit `drop(pair.slave)` this codebase's own pty tests already
        // use after a spawn.
        drop(pair.slave);
        let master = pair.master;

        let mut reader = master.try_clone_reader()?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            // Raise pane reader priority for responsive output without blocking the UI (#330).
            super::super::priority::raise_current_thread();
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            return;
                        }
                    }
                }
            }
        });

        // Register an orchestrator pane's logical seat and model for rollover (#358).
        if role == PromptRole::Orchestrator {
            let seat_model = turn_env
                .iter()
                .find(|(key, _)| key == super::super::adapters::SEAT_MODEL_ENV)
                .map(|(_, value)| value.clone());
            let short = sessions::short_id(&session_id);
            if super::super::seat::register(
                state,
                &short,
                &session_id,
                &agent_name,
                seat_model.as_deref(),
                super::super::adapters::provider_for_agent_and_model(
                    Some(&agent_name),
                    seat_model.as_deref(),
                ),
                role.label(),
                super::super::seat::pin_from_env(&super::super::config::env_from_process()),
                super::super::state::now_secs(),
            )
            .is_ok()
            {
                // A dashboard that died mid-swap leaves the seat `Prepared`,
                // refusing every future rollover. Nothing that outlived that
                // crash can still be answering at this address, so recovery
                // always aborts rather than committing.
                super::super::rollover::on_startup(state, &short, &|_| None);
            }
        }

        Ok(Pane {
            title,
            agent_name,
            verb,
            role,
            session_id,
            parser: vt100::Parser::new(rows, cols, SCROLLBACK_ROWS),
            approval_dialog: false,
            kind: PaneKind::Wrapped(PtyPane {
                master,
                child,
                lifecycle,
                writer,
                rx,
                server,
            }),
            pending_handover: None,
            pending_output: false,
            guard,
            state_dir: state.clone(),
            last_signal_at: None,
            last_output_at: None,
            turn_signal_capable,
            idle_quiet,
            last_local_input_at: None,
            injected_awaiting_turn: false,
            user_typed_since_turn: false,
            codex_approval_latched: false,
            exit_code: None,
            native_stop_code: None,
            launched_at,
            exited_after: None,
            done: false,
            report_to: None,
            intake_dir: None,
            work_group_id: None,
            budget_tokens: None,
            measured_usage: None,
            launch_model: super::super::adapters::last_model_flag(&argv).map(str::to_string),
            reservation_id: None,
            budget_soft_warned: false,
            budget_grace_given: false,
            deadline: None,
            parent_session: None,
            report_reminder_sent: false,
            settled_mail_sent: false,
            stalled_mail_sent: false,
            mail_block_log: None,
            result_schema: turn_env
                .iter()
                .find(|(key, _)| key == super::super::agent::RESULT_SCHEMA_ENV)
                .map(|(_, value)| value.clone()),
            pending_submit: None,
            submit_confirmation: None,
            delivery_sender: None,
            last_injection_at: Instant::now(),
            launch_mode,
            writer_permit: None,
            cwd: cwd.to_path_buf(),
            owns_cwd: false,
            delegation: None,
        })
    }

    /// Report budget-cut output as pending so a child that exits with unread
    /// bytes gets another drain tick before reap.
    pub fn drain(&mut self) -> (bool, bool) {
        let (any, more, _used) = self.drain_with_budget(DRAIN_BUDGET_BYTES);
        (any, more)
    }

    /// Use the caller's share of the shared drain budget and return actual byte spend (#330).
    pub fn drain_with_budget(&mut self, budget: usize) -> (bool, bool, usize) {
        self.poll_exit();
        let (any, more, used) = match &self.kind {
            PaneKind::Wrapped(pty) => drain_into(&pty.rx, &mut self.parser, budget),
            // A native pane has no reader channel: its conversation lives in
            // the journal and `tick_native` is what re-reads it. It spends
            // none of the tick's shared parsing budget, so how many native
            // panes a mixed roster holds never costs a wrapped pane latency.
            _ => (self.tick_native(), false, 0),
        };
        self.pending_output = more;
        if any {
            self.refresh_approval_dialog();
            // Record output time; signal_still_stands decides whether it is a redraw or a new turn.
            self.last_output_at = Some(Instant::now());
            // Mail error output is not proof the child survived the confirmation window.
            if self.delivery_sender.is_none() {
                self.submit_confirmation = None;
            }
        }
        // Retire signal-less typing flags only after output and input become quiet.
        if !self.turn_signal_capable
            && signal_less_quiescent(
                self.last_output_at,
                self.last_local_input_at,
                Instant::now(),
                self.idle_quiet,
            )
        {
            self.injected_awaiting_turn = false;
            self.user_typed_since_turn = false;
        }
        (any, more, used)
    }

    /// Whether the last drain stopped on its budget with bytes still queued.
    /// `dash::reap_ended_panes`' hold-back gate -- see [`Pane::pending_output`].
    pub fn has_pending_output(&self) -> bool {
        self.pending_output
    }

    /// The current screen, for `dash::ui`'s renderers. Already reflects this
    /// pane's scrollback offset: `vt100::Screen::cell` reads through
    /// `Grid::visible_rows`, which splices in the scrolled-back rows, so
    /// `ui::render_grid` draws the scrolled view with no change of its own.
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// How many rows back from the live view this pane is currently showing;
    /// `0` is live. `ui::scroll_marker` turns it into the operator-facing
    /// marker.
    pub fn scrollback(&self) -> usize {
        self.parser.screen().scrollback()
    }

    /// Whether this pane's child currently holds the alternate screen
    /// (`\x1b[?1049h`), and whether it has asked to be sent mouse events --
    /// the two facts every scroll below branches on, stamped on each keylog
    /// scroll line so one capture explains a scroll that appeared to do
    /// nothing.
    pub fn alternate_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    /// Whether the child turned on any xterm mouse-reporting mode
    /// (`vt100::Screen::mouse_protocol_mode`). A real harness does: a probe of
    /// this machine's `claude.exe` startup, and the recorded
    /// `tests/fixtures/claude-session.raw`, both show `?1000h ?1002h ?1003h
    /// ?1006h` -- it is a full-screen TUI that scrolls itself and wants the
    /// events to do it with.
    pub fn wants_mouse(&self) -> bool {
        !matches!(
            self.parser.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None
        )
    }

    /// The wheel, on this pane. `delta` is positive for a scroll back into
    /// history; `col`/`row` are pane-local 1-based coordinates
    /// (`dash::pane_local_mouse`).
    ///
    /// Three branches, decided per pane at scroll time and in this order:
    ///
    /// * **(B) the child asked for mouse events.** It gets the wheel event,
    ///   encoded in the protocol it selected, and scrolls itself. The
    ///   dashboard's own scrollback is not touched -- it is structurally
    ///   always empty for such a child anyway (see below), so consuming the
    ///   wheel for it was the whole bug: the dashboard swallowed the event on
    ///   behalf of a buffer that could never fill, instead of passing it to
    ///   the child that had asked for it.
    /// * **(C) no mouse reporting, but the child is on the alternate screen.**
    ///   vt100 hard-codes the alternate grid's scrollback to zero
    ///   (`vt100-0.16.2/src/screen.rs:76`, `Grid::new(size, 0)`) and
    ///   `set_scrollback` clamps to `self.scrollback.len()` on whichever grid
    ///   is drawing, so nothing can move there no matter how large
    ///   [`SCROLLBACK_ROWS`] is. Nothing to show and nobody to hand the event
    ///   to: say so rather than failing silently.
    /// * **(A) the normal screen.** The vt100 scrollback offset moves, which
    ///   is correct and genuinely useful for a child that is not a full-screen
    ///   TUI. Clamped at both ends: [`scroll_offset`] holds the bottom at `0`,
    ///   and `set_scrollback` clamps the top to however much history this pane
    ///   actually has (which is why `max` is `usize::MAX` here -- vt100 owns
    ///   that bound and is the only thing that can see it).
    ///
    /// Branch (B) writes through [`Pane::write_input`], deliberately **not**
    /// `write_operator_input`: a forwarded wheel event is navigation, not
    /// prompt composition, and `write_operator_input` would additionally mark
    /// the pane as "the operator has typed since the last turn boundary" (F1),
    /// keeping it out of reach of the idle-gated injectors for as long as
    /// somebody keeps scrolling.
    pub fn scroll_wheel(&mut self, delta: isize, col: u16, row: u16) -> CtxResult<ScrollOutcome> {
        if self.wants_mouse() {
            let button = if delta > 0 {
                MOUSE_WHEEL_UP
            } else {
                MOUSE_WHEEL_DOWN
            };
            let bytes = mouse_report_bytes(
                self.parser.screen().mouse_protocol_encoding(),
                button,
                col,
                row,
                true,
            );
            self.write_input(&bytes)?;
            return Ok(ScrollOutcome::ForwardedMouse);
        }
        Ok(self.scroll_by(delta))
    }

    /// Forward buttons only on a child's grid when it asked for mouse events;
    /// otherwise a dashboard click must not become child input.
    pub fn forward_mouse_button(
        &mut self,
        button: u8,
        press: bool,
        col: u16,
        row: u16,
    ) -> CtxResult<bool> {
        if !self.wants_mouse() {
            return Ok(false);
        }
        let bytes = mouse_report_bytes(
            self.parser.screen().mouse_protocol_encoding(),
            button,
            col,
            row,
            press,
        );
        self.write_input(&bytes)?;
        Ok(true)
    }

    /// The keyboard scroll bindings' half of the same decision: the vt100
    /// scrollback offset when there is one, and [`ScrollOutcome::FullScreen`]
    /// when the child owns the screen. No mouse event is synthesised here --
    /// `Ctrl+A PageUp` is not a wheel notch, and an *unprefixed* `PageUp`
    /// already reaches the child as itself, which is how a full-screen TUI is
    /// meant to be paged.
    pub fn scroll_by(&mut self, delta: isize) -> ScrollOutcome {
        if self.alternate_screen() {
            return ScrollOutcome::FullScreen;
        }
        scroll_parser(&mut self.parser, delta)
    }

    /// A half-screen of scrolling, the step `Ctrl+A PageUp`/`PageDown` moves.
    /// Half rather than a full screen so the operator keeps a few lines of
    /// overlap to read against, which is what `less`, tmux and every pager
    /// converged on.
    pub fn scroll_page(&mut self, up: bool) -> ScrollOutcome {
        let (rows, _) = self.parser.screen().size();
        let half = (rows as isize / 2).max(1);
        self.scroll_by(if up { half } else { -half })
    }

    /// Jumps to the oldest row this pane still has (`Ctrl+A Home`).
    /// `set_scrollback` clamps to the real length, so `usize::MAX` means "as
    /// far back as there is". A full-screen child has no history to jump into,
    /// so it reports [`ScrollOutcome::FullScreen`] rather than pretending to
    /// have moved.
    pub fn scroll_to_top(&mut self) -> ScrollOutcome {
        if self.alternate_screen() {
            return ScrollOutcome::FullScreen;
        }
        let before = self.scrollback();
        self.parser.screen_mut().set_scrollback(usize::MAX);
        let after = self.scrollback();
        if after != before {
            ScrollOutcome::Scrolled(after)
        } else {
            ScrollOutcome::AtOldest
        }
    }

    /// Back to the live view (`Ctrl+A End`, and every keystroke the operator
    /// sends the child -- see [`Pane::write_operator_input`]).
    pub fn scroll_to_live(&mut self) -> ScrollOutcome {
        if self.alternate_screen() {
            return ScrollOutcome::FullScreen;
        }
        let before = self.scrollback();
        self.parser.screen_mut().set_scrollback(0);
        if before == 0 {
            ScrollOutcome::AtLive
        } else {
            ScrollOutcome::Scrolled(0)
        }
    }

    /// Forward operator input and mark it pending until the next turn boundary, preventing idle injection.
    pub fn write_operator_input(&mut self, bytes: &[u8]) -> CtxResult<()> {
        if self.has_pending_submit() {
            let _ = self.submit_pending();
        }
        // Operator input owns the composer; never submit it on an automatic retry.
        if let Some((_, retry_spent)) = self.submit_confirmation.as_mut() {
            *retry_spent = true;
        }
        self.user_typed_since_turn = true;
        self.last_local_input_at = Some(Instant::now());
        self.scroll_to_live();
        self.write_input(bytes)
    }

    pub fn write_input(&mut self, bytes: &[u8]) -> CtxResult<()> {
        let mut writer = self
            .writer()?
            .lock()
            .map_err(|_| "dashboard pane: writer lock poisoned")?;
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(())
    }

    /// Resizes both the pty and the `vt100` parser, so the two never
    /// disagree about how big this pane's screen is.
    pub fn resize(&mut self, rows: u16, cols: u16) -> CtxResult<()> {
        if let Some(pty) = self.pty_mut() {
            pty.master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })?;
        }
        self.parser.screen_mut().set_size(rows, cols);
        // A shrink can cut the dialog's rows until Codex redraws; only output may clear the flag.
        let open = self.approval_dialog;
        self.refresh_approval_dialog();
        self.approval_dialog |= open;
        Ok(())
    }

    /// This pane's current `PaneState`, from state already cached by
    /// `drain`/`on_turn_signal`/`poll_exit` -- no I/O of its own, so it is
    /// cheap enough to call every frame.
    pub fn state(&self) -> PaneState {
        // Native state comes from its session and approval driver, not terminal-output quiet time (#490).
        if let Some(native) = self.native() {
            return match self.exit_code {
                Some(code) => PaneState::Ended(code),
                None if native.busy() || native.blocked() => PaneState::Working,
                None => PaneState::Idle,
            };
        }
        if matches!(self.kind, PaneKind::Ended) {
            return PaneState::Ended(self.exit_code.unwrap_or(0));
        }
        state_from(
            pane_is_idle(
                self.turn_signal_capable,
                self.last_signal_at,
                self.last_output_at,
                self.last_local_input_at,
                Instant::now(),
                IDLE_DEBOUNCE,
                self.idle_quiet,
            ),
            self.exit_code,
            self.injected_awaiting_turn,
        )
    }

    /// Whether this Codex pane currently shows an approval dialog; other agents never do (#842).
    pub(crate) fn codex_approval_open(&self) -> bool {
        self.approval_dialog
    }

    /// The open Codex dialog's request details, read from the live screen; `None` when no dialog is open.
    pub(crate) fn codex_dialog(&mut self) -> Option<CodexDialog> {
        if !self.codex_approval_open() {
            return None;
        }
        let offset = self.scrollback();
        self.parser.screen_mut().set_scrollback(0);
        let dialog = codex_approval_dialog(&self.screen().contents());
        self.parser.screen_mut().set_scrollback(offset);
        dialog
    }

    /// Re-read the dialog from the live screen (scrollback 0, as `screen_tail` does), so scrolling back
    /// cannot hide an open dialog from the typing guards.
    fn refresh_approval_dialog(&mut self) {
        if self.agent_name != "codex" || !matches!(self.kind, PaneKind::Wrapped(_)) {
            return;
        }
        let offset = self.scrollback();
        self.parser.screen_mut().set_scrollback(0);
        self.approval_dialog = codex_approval_dialog_shown(&self.screen().contents());
        self.parser.screen_mut().set_scrollback(offset);
    }

    /// Inject mail or nudges only when pane state and turn signals make typing safe.
    pub fn injectable(&self) -> bool {
        if self.pending_submit.is_some() || self.submit_confirmation.is_some() {
            return false;
        }
        if self.codex_approval_open() {
            return false;
        }
        if self.native().is_some_and(|native| native.has_draft()) {
            return false;
        }
        injectable_from(
            self.state(),
            self.injected_awaiting_turn,
            self.user_typed_since_turn,
        )
    }

    /// Drain queued turn signals and observe child exit on the same pane tick.
    pub fn on_turn_signal(&mut self) {
        self.poll_exit();
        let signalled = self.pty().is_some_and(|pty| {
            let mut seen = false;
            if let Some(server) = pty.server.as_ref() {
                while server.try_recv().is_some() {
                    seen = true;
                }
            }
            seen
        });
        {
            if signalled {
                self.last_signal_at = Some(Instant::now());
                self.pending_submit = None;
                self.submit_confirmation = None;
                self.delivery_sender = None;
                self.injected_awaiting_turn = false;
                self.user_typed_since_turn = false;
            }
        }
    }

    /// This pane's registry short id -- its nudge/mail address.
    pub fn short(&self) -> &str {
        self.guard.short()
    }

    /// Route keys by pane driver: PTY writer for wrapped harnesses, native composer for native sessions (#490).
    pub fn accepts_native_controls(&self) -> bool {
        self.is_native()
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn agent(&self) -> &str {
        &self.agent_name
    }

    /// Use the actual child working directory for transcript and worktree policy.
    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Keep the state directory shared across successor launches at this seat (#552).
    pub(crate) fn state_dir(&self) -> &StateDir {
        &self.state_dir
    }

    /// Whether this pane owns its `cwd` as an agent-allocated worktree (see
    /// the [`Pane::owns_cwd`] field).
    pub(crate) fn owns_cwd(&self) -> bool {
        self.owns_cwd
    }

    /// Records that the spawning request allocated this pane's `cwd` with
    /// `--worktree`, so the reap path may reclaim it.
    pub(crate) fn set_owns_cwd(&mut self) {
        self.owns_cwd = true;
    }

    /// Report reachability only when the turn-signal socket actually bound.
    pub fn reachable(&self) -> bool {
        match &self.kind {
            PaneKind::Wrapped(pty) => pty.server.is_some(),
            // A native pane is reached directly -- there is no socket to bind
            // and nothing that can fail to bind, so it is never the degraded
            // "visible but unsupervised" session a failed bind produces.
            _ => true,
        }
    }

    pub(crate) fn started_at(&self) -> u64 {
        self.guard.record().started_at
    }

    /// This pane's own zirv session id (the uuid `PaneSpec::session_id`
    /// carried in) -- the roster's own `RosterPane::session_id`, and what a
    /// verified adapter's `resume_args` is asked to resume.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Record the report-back target once after spawn so reminder delivery can use it (#115).
    pub fn set_report_to(&mut self, report_to: Option<String>) {
        self.report_to = report_to;
        self.report_reminder_sent = false;
        self.settled_mail_sent = false;
    }

    /// The address `report_back_reminder_sweep` reminds this pane to report
    /// its outcome back to, if any.
    pub fn report_to(&self) -> Option<&str> {
        self.report_to.as_deref()
    }

    /// Store the directory this child inherited; it must be the same path
    /// the dashboard later drains to verify requester identity.
    pub fn set_intake_dir(&mut self, dir: PathBuf) {
        self.intake_dir = Some(dir);
    }

    /// This pane's own spawn-request intake directory, if it was given one.
    pub fn intake_dir(&self) -> Option<&Path> {
        self.intake_dir.as_deref()
    }

    /// Store the admitted work group on the pane for closing and restoration.
    pub fn set_work_group_id(&mut self, id: Option<String>) {
        self.work_group_id = id;
    }

    /// The work group this pane belongs to, if any.
    pub fn work_group_id(&self) -> Option<&str> {
        self.work_group_id.as_deref()
    }

    pub fn set_budget_tokens(&mut self, budget: Option<u64>) {
        self.budget_tokens = budget;
        self.budget_soft_warned = false;
        self.budget_grace_given = false;
    }

    /// Expose the model from actual launch argv so sidebar facts match the child (#354).
    pub fn launch_model(&self) -> Option<&str> {
        // A native pane knows its route first-hand; a runtime-attached one
        // honestly knows nothing, and says so rather than guessing.
        match self.native() {
            Some(native) => native.launch_model(),
            None => self.launch_model.as_deref(),
        }
    }

    /// Reuse the budget sweep's measured usage for sidebar disclosure (#354).
    pub fn measured_usage(&self) -> Option<super::super::event::TranscriptUsage> {
        // A native pane's usage is journalled by its own session, so it
        // never waits on the budget sweep's transcript read.
        match self.native() {
            Some(native) => Some(native.measured_usage()),
            None => self.measured_usage,
        }
    }

    /// Expose whether the pane still holds its writer permit (#354).
    pub fn holds_writer_permit(&self) -> bool {
        self.native()
            .map(super::native_pane::NativePaneRuntime::holds_writer_permit)
            .unwrap_or_else(|| self.writer_permit.is_some())
    }

    pub fn budget_tokens(&self) -> Option<u64> {
        self.budget_tokens
    }

    /// Clamp untrusted timeout before adding it to Instant; absent timeout
    /// leaves the pane unbounded.
    pub fn set_timeout(&mut self, started: Instant, timeout_secs: Option<u64>) {
        self.deadline = timeout_secs.and_then(|secs| deadline_for(started, secs));
    }

    /// The armed wall clock, if any.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// On deadline, use adapter quit sequence and the same timeout exit code as headless workers.
    pub fn enforce_deadline(&mut self, now: Instant, quit_sequence: &str) -> CtxResult<bool> {
        let Some(deadline) = self.deadline else {
            return Ok(false);
        };
        if now < deadline {
            return Ok(false);
        }
        if self.exit_code.is_some() {
            // Nothing left to stop: disarm so no later sweep looks at this
            // pane again, and report nothing -- the child's own exit is the
            // outcome, and overwriting it would invent one.
            self.deadline = None;
            return Ok(false);
        }
        if let Err(polite) = self.shutdown(quit_sequence) {
            self.finish_shutdown().map_err(|escalation| {
                format!(
                    "dashboard pane: timeout quit failed ({polite}); escalation failed too \
                     ({escalation})"
                )
            })?;
        }
        self.deadline = None;
        self.exit_code = Some(super::super::exec::EXIT_TIMEOUT);
        Ok(true)
    }

    /// Store the provider reservation ID so reap can settle it (#358).
    pub fn set_reservation_id(&mut self, id: Option<String>) {
        self.reservation_id = id;
    }

    pub fn reservation_id(&self) -> Option<&str> {
        self.reservation_id.as_deref()
    }

    /// Applies the pane's transcript usage to the same budget state machine
    /// as a headless worker. A naturally failed child keeps its own exit;
    /// a clean child whose final transcript is over budget becomes exit 77.
    pub fn enforce_token_budget(
        &mut self,
        usage: &super::super::event::TranscriptUsage,
        quit_sequence: &str,
    ) -> CtxResult<Option<PaneBudgetNotice>> {
        // Store usage before enforcing the ceiling so sidebar facts reflect
        // what was measured, even when the sweep then stops the child (#354).
        self.measured_usage = Some(*usage);
        let Some(limit) = self.budget_tokens else {
            return Ok(None);
        };
        let budget = super::super::agent::WorkerBudget {
            tokens: Some(limit),
            tool_calls: None,
        };
        match super::super::agent::budget_state(&budget, usage, 0) {
            super::super::agent::BudgetState::Ok => Ok(None),
            super::super::agent::BudgetState::SoftWarn { used, limit } => {
                if self.budget_soft_warned {
                    return Ok(None);
                }
                self.budget_soft_warned = true;
                Ok(Some(PaneBudgetNotice::SoftWarn { used, limit }))
            }
            super::super::agent::BudgetState::HardStop { used, limit } => {
                if self.exit_code.is_some_and(|code| code != 0) {
                    return Ok(None);
                }
                if self.exit_code == Some(0) {
                    self.exit_code = Some(super::super::exec::EXIT_BUDGET_EXHAUSTED);
                    return Ok(Some(PaneBudgetNotice::HardStop { used, limit }));
                }
                if !self.budget_grace_given {
                    self.budget_grace_given = true;
                    return Ok(None);
                }
                self.shutdown(quit_sequence)?;
                self.exit_code = Some(super::super::exec::EXIT_BUDGET_EXHAUSTED);
                Ok(Some(PaneBudgetNotice::HardStop { used, limit }))
            }
        }
    }

    /// Store the server-verified parent for steering mail, never a request-provided claim (#249).
    pub fn set_parent_session(&mut self, parent: Option<String>) {
        self.parent_session = parent;
    }

    /// This pane's own server-verified supervising session, if any --
    /// `dash::mod::sweep_one_pane`'s own trust-label input.
    pub fn parent_session(&self) -> Option<&str> {
        self.parent_session.as_deref()
    }

    /// Records what this pane owes the cost ledger. Set right after
    /// `Pane::spawn` by `dash::mod::fulfill_spawn_request`, the same
    /// "computed once, stored once" pattern `set_parent_session` above uses.
    pub fn set_delegation(&mut self, facts: DelegationFacts) {
        self.delegation = Some(facts);
    }

    /// See [`DelegationFacts`].
    pub fn delegation(&self) -> Option<&DelegationFacts> {
        self.delegation.as_ref()
    }

    /// This pane's own child process id, when the backend can report one --
    /// the same `process_id()` `Pane::spawn` itself already reads to stamp
    /// `Record::pid`, exposed here so a caller that acquired a writer permit
    /// AFTER the spawn (`dash::mod::fulfill_spawn_request`) can tie it to the
    /// real child (`permit::HeavyPermit::set_child_pid`) rather than the
    /// dashboard's own pid.
    pub fn child_pid(&self) -> Option<u32> {
        self.pty().and_then(|pty| pty.child.process_id())
    }

    /// Hold the acquired writer permit for the pane's lifetime (#264).
    pub fn set_writer_permit(&mut self, permit: super::super::permit::HeavyPermit) {
        self.writer_permit = Some(permit);
    }

    /// Whether the one-shot reminder was injected or suppressed by a sent report.
    pub fn report_reminder_sent(&self) -> bool {
        self.report_reminder_sent
    }

    /// Marks this pane as having received its one-shot report-back reminder,
    /// so `report_back_reminder_sweep` never injects a second one.
    pub fn mark_report_reminder_sent(&mut self) {
        self.report_reminder_sent = true;
    }

    /// Whether this pane's child has produced any output at all since it was
    /// spawned -- `report_back_reminder_sweep`'s cheapest available signal
    /// for "this worker actually ran and went quiet" as opposed to "this
    /// pane has never done anything yet" (both read as merely `injectable()`
    /// otherwise). Reuses `last_output_at`, the same timestamp `drain()`
    /// already stamps on every batch of bytes read from the child, rather
    /// than adding a new field that duplicates it.
    pub fn has_produced_output(&self) -> bool {
        self.last_output_at.is_some()
    }

    /// This pane's registry verb (`Verb::Chat` for the orchestrator,
    /// `Verb::Dash` for a worker pane) -- see the field's own doc comment.
    pub fn verb(&self) -> Verb {
        self.verb
    }

    /// Use the role granted at spawn for later lineage checks (#169).
    pub fn role(&self) -> PromptRole {
        self.role
    }

    /// Read launch mode from the child environment so roster restoration preserves actual posture (#160).
    pub fn launch_mode(&self) -> super::super::adapters::LaunchMode {
        self.launch_mode
    }

    /// Preview the bottom-most nonblank visible screen row.
    pub fn last_line(&self) -> String {
        last_line_of(self.screen())
    }

    /// Measure child age before teardown adds delay.
    pub fn exited_after(&self) -> Option<Duration> {
        self.exited_after
    }

    /// Write a bounded labelled line first and defer its submit carriage return until the echo settles.
    pub fn inject_visible(&mut self, label: &str, body: &str) -> CtxResult<()> {
        if self.codex_approval_open() {
            return Err("codex approval dialog is open; not typing into it".into());
        }
        // Deliver native messages through the native submit path, never PTY control bytes (#490).
        if let PaneKind::Native(native) = &mut self.kind {
            native.deliver(label, body)?;
            let now = Instant::now();
            self.last_local_input_at = Some(now);
            self.injected_awaiting_turn = true;
            self.last_injection_at = now;
            self.submit_confirmation = None;
            self.delivery_sender = None;
            return Ok(());
        }
        {
            let mut writer = self
                .writer()?
                .lock()
                .map_err(|_| "dashboard pane: writer lock poisoned")?;
            let sink: &mut dyn Write = &mut **writer;
            write_injection_phase1(sink, label, body)?;
        }
        let now = Instant::now();
        self.last_local_input_at = Some(now);
        self.injected_awaiting_turn = true;
        self.pending_submit = Some(now + INJECTION_SUBMIT_DELAY);
        self.last_injection_at = now;
        self.submit_confirmation = None;
        self.delivery_sender = None;
        Ok(())
    }

    /// Whether this pane has a deferred injection submission
    /// (`Self::pending_submit`) whose output has settled, bounded by two seconds. The
    /// dashboard's tick loop calls this for every pane, every tick, and
    /// [`Self::submit_pending`] on the ones that answer `true`.
    pub(crate) fn pending_submit_due(&self, now: Instant) -> bool {
        let Some(deadline) = self.pending_submit else {
            return false;
        };
        // An Enter now would answer the dialog, so hold the submit until it is gone (#842).
        if self.codex_approval_open() {
            return false;
        }
        let quiet_deadline = self
            .last_output_at
            .map(|output| (output + INJECTION_SUBMIT_DELAY).max(deadline))
            .unwrap_or(deadline);
        submit_is_due(
            Some(quiet_deadline.min(deadline + Duration::from_secs(2))),
            now,
        )
    }

    /// Block operator input while any deferred injection submit remains outstanding.
    pub(crate) fn has_pending_submit(&self) -> bool {
        self.pending_submit.is_some()
    }

    /// Writes one `\r`, clearing the pending submit only after a successful write.
    /// The caller reports a failed write; success starts the confirmation window.
    pub fn submit_pending(&mut self) -> CtxResult<()> {
        if self.pending_submit.is_none() {
            return Ok(());
        }
        {
            let mut writer = self
                .writer()?
                .lock()
                .map_err(|_| "dashboard pane: writer lock poisoned")?;
            let sink: &mut dyn Write = &mut **writer;
            write_submit_cr(sink)?;
        }
        self.pending_submit = None;
        self.submit_confirmation = Some((Instant::now(), false));
        Ok(())
    }

    pub(crate) fn cancel_submission(&mut self) {
        self.pending_submit = None;
        self.submit_confirmation = None;
    }

    pub(crate) fn screen_tail(&mut self) -> String {
        // Read native previews from the transcript, since native panes have no vt100 grid (#490).
        if let Some(native) = self.native() {
            let (view, presentation) = native.view();
            let lines = super::native_pane::render_lines(view, presentation);
            let tail: Vec<String> = lines
                .iter()
                .rev()
                .take(20)
                .map(super::native_pane::StyledLine::to_plain_string)
                .collect();
            let tail = tail.into_iter().rev().collect::<Vec<_>>().join(
                "
",
            );
            let mut start = tail.len().saturating_sub(2048);
            while !tail.is_char_boundary(start) {
                start += 1;
            }
            return tail[start..].to_string();
        }
        let offset = self.scrollback();
        self.parser.screen_mut().set_scrollback(0);
        let contents = self.screen().contents();
        self.parser.screen_mut().set_scrollback(offset);
        let mut lines: Vec<&str> = contents.lines().rev().take(20).collect();
        lines.reverse();
        let tail = lines.join("\n");
        let mut start = tail.len().saturating_sub(2048);
        while !tail.is_char_boundary(start) {
            start += 1;
        }
        tail[start..].to_string()
    }

    /// Retries one silent submission, then reports it unconfirmed without typing again.
    pub(crate) fn check_submission(&mut self, now: Instant) -> CtxResult<bool> {
        self.poll_exit();
        if matches!(self.state(), PaneState::Ended(_))
            && (self.pending_submit.is_some()
                || self.submit_confirmation.is_some()
                || self.delivery_sender.is_some())
        {
            self.cancel_submission();
            return Ok(true);
        }
        let Some((submitted, retry_spent)) = self.submit_confirmation else {
            return Ok(false);
        };
        if now.saturating_duration_since(submitted) < Duration::from_secs(1) {
            return Ok(false);
        }
        if self.last_output_at.is_some_and(|at| at > submitted)
            || self.last_signal_at.is_some_and(|at| at > submitted)
        {
            self.submit_confirmation = None;
            self.delivery_sender = None;
            return Ok(false);
        }
        self.submit_confirmation = None;
        if retry_spent || self.user_typed_since_turn {
            return Ok(true);
        }
        self.write_input(b"\r")?;
        self.last_local_input_at = Some(now);
        self.submit_confirmation = Some((now, true));
        Ok(false)
    }

    /// Idempotent: sends `quit_sequence` (grace period, then `kill`, exactly
    /// as `wrap::quit_child` does for its own child), releases this pane's
    /// registry record and unpublishes its socket path. A second call is a
    /// no-op -- see `done`'s own doc comment.
    pub fn shutdown(&mut self, quit_sequence: &str) -> CtxResult<()> {
        self.cancel_handover();
        if self.done {
            return Ok(());
        }
        // Mark polite quit complete only after it succeeds so a failed quit can still be retried.
        match &mut self.kind {
            PaneKind::Wrapped(pty) => {
                let mut writer = pty
                    .writer
                    .lock()
                    .map_err(|_| "dashboard pane: writer lock poisoned")?;
                let sink: &mut dyn Write = &mut **writer;
                // Typing the quit line would answer an open approval dialog; wait out the grace and kill.
                let quit = if self.approval_dialog {
                    ""
                } else {
                    quit_sequence
                };
                wrap::quit_child(sink, &mut pty.child, quit, QUIT_GRACE)?;
            }
            // A native pane has no child to ask politely: ending it is
            // `InteractiveSession::shutdown` (or a `session.detach` for a
            // runtime-owned one), which `finish_native` performs once.
            _ => self.finish_native(),
        }
        self.done = true;
        // Release console-close membership explicitly after child exit; release builds abort without running Drop.
        self.release_lifecycle();
        wrap::unpublish_socket_path(&self.state_dir, &self.session_id);
        // Release seat address with other per-session artifacts on shutdown (#358).
        if self.role == PromptRole::Orchestrator {
            super::super::rollover::forget(&self.state_dir, self.guard.short());
        }
        self.guard.release();
        self.writer_permit.take();
        Ok(())
    }

    /// Retire a predecessor without releasing the seat or registry identity now owned by its successor (#552).
    pub fn retire_for_successor(&mut self, quit_sequence: &str) {
        self.cancel_handover();
        if self.done {
            return;
        }
        match &mut self.kind {
            PaneKind::Wrapped(pty) => {
                if let Ok(mut writer) = pty.writer.lock() {
                    let sink: &mut dyn Write = &mut **writer;
                    let _ = wrap::quit_child(sink, &mut pty.child, quit_sequence, QUIT_GRACE);
                }
            }
            _ => self.finish_native(),
        }
        self.done = true;
        self.release_lifecycle();
        wrap::unpublish_socket_path(&self.state_dir, &self.session_id);
        self.guard.disown();
        self.writer_permit.take();
    }

    /// Send every pane's quit sequence first, then wait against one shared grace budget.
    pub fn request_quit(&mut self, quit_sequence: &str) {
        if self.done {
            return;
        }
        self.poll_exit();
        if self.exit_code.is_some() {
            return;
        }
        // Enter would accept the highlighted "Yes, proceed": with a dialog open, type nothing; the
        // shutdown escalation ends the child instead.
        if self.codex_approval_open() {
            return;
        }
        let _ = self.write_input(quit_sequence.as_bytes());
    }

    /// Whether this pane's child has exited (polls once). The batched-shutdown
    /// wait loop polls every pane through here within its shared grace window.
    pub fn try_exited(&mut self) -> bool {
        self.poll_exit();
        self.exit_code.is_some()
    }

    /// The escalation half of a batched shutdown, run once the shared grace
    /// window has elapsed: kills the child if it has not exited on its own,
    /// then releases this pane's registry record and unpublishes its socket.
    /// Idempotent via `done`, exactly like [`Pane::shutdown`] -- calling both
    /// is safe, the second is a no-op.
    pub fn finish_shutdown(&mut self) -> CtxResult<()> {
        self.cancel_handover();
        if self.done {
            return Ok(());
        }
        self.done = true;
        self.poll_exit();
        if self.exit_code.is_none() {
            // Kill the process tree before the direct child on Windows; npm command shims can leave the Node agent behind.
            #[cfg(not(unix))]
            if let Some(pid) = self.child_pid() {
                supervise::kill_tree(pid);
            }
            match &mut self.kind {
                PaneKind::Wrapped(pty) => {
                    let _ = pty.child.kill();
                    let _ = pty.child.wait();
                }
                _ => self.finish_native(),
            }
        }
        self.release_lifecycle();
        wrap::unpublish_socket_path(&self.state_dir, &self.session_id);
        // Release the live seat address when pane shutdown completes (#358).
        if self.role == PromptRole::Orchestrator {
            super::super::rollover::forget(&self.state_dir, self.guard.short());
        }
        self.guard.release();
        self.writer_permit.take();
        Ok(())
    }

    /// Stop the owned child immediately and record the deliberate kill code for reap (#403).
    pub fn stop_now(&mut self, code: i32) -> CtxResult<()> {
        self.poll_exit();
        if let PaneKind::Native(native) = &mut self.kind {
            native.stop(&self.state_dir)?;
            if native.ended {
                self.exit_code = Some(code);
            } else {
                self.native_stop_code.get_or_insert(code);
                return Ok(());
            }
        }
        self.finish_shutdown()?;
        if self.exit_code.is_none() {
            self.exit_code = Some(code);
        }
        Ok(())
    }

    /// Derive successor launch state once for both handover and recovery paths (#552).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn build_swap_launch(
        &self,
        cfg: &super::super::config::CtxConfig,
        req: &super::super::handover::HandoverRequest,
        handoff_note: &super::super::handoff::Handoff,
        role: PromptRole,
        repo: &Path,
        session_id: &str,
        socket: Option<&Path>,
        title: String,
    ) -> CtxResult<SwapLaunch> {
        // When resuming the same harness conversation, send only role context, not a second handoff packet (#440).
        let same_harness = req.target_agent.eq_ignore_ascii_case(self.agent());
        let carries_handoff = !same_harness;
        let (new_adapter, mut extra) =
            super::super::handover::resolve_swap_launch(cfg, req, carries_handoff, role)?;
        if let Ok(state) = StateDir::resolve(&|key| std::env::var(key).ok()) {
            extra = super::super::adapters::with_workload_writable_roots(
                extra,
                new_adapter.as_ref(),
                repo,
                &state,
            );
        }
        // Whether `resolve_swap_launch` above actually appended this
        // adapter's resume flags -- the same shared answer, so the argv and
        // the prompt decision below cannot disagree.
        let resuming = super::super::handover::resumes_conversation(
            new_adapter.as_ref(),
            req,
            carries_handoff,
        );
        let new_argv: Vec<String> = {
            // Deliver multiline handoff text off argv so Windows command-shim reparsing cannot reject it (#220).
            let command = if resuming {
                // Reapply Claude's system-prompt role layer on resume because its flag applies per invocation.
                extra.extend(super::super::prompt::role_layer_args(
                    new_adapter.as_ref(),
                    role,
                    &cfg.prompt,
                    &self.state_dir,
                    session_id,
                ));
                if carries_handoff {
                    // A return to a parked conversation: resume flags AND
                    // the interim harness's own packet, which is the only
                    // record of what happened while this conversation was
                    // parked.
                    let prompt_text = super::super::prompt::interactive_handoff_prompt(
                        new_adapter.as_ref(),
                        &[],
                        &mut extra,
                        &wrap::restart_prompt(handoff_note, &cfg.screen.thresholds()),
                        &self.state_dir,
                        session_id,
                    );
                    let prompt_text = if self.is_native() {
                        prompt_text
                    } else {
                        rollover_receipt_prompt(req, prompt_text, session_id)
                    };
                    new_adapter.interactive_cmd(Some(&prompt_text), &extra)
                } else {
                    // Source recovery carries the role layer without a new handoff packet (#440).
                    new_adapter.interactive_cmd(None, &extra)
                }
            } else {
                let prompt_text = super::super::prompt::interactive_handoff_prompt(
                    new_adapter.as_ref(),
                    &[],
                    &mut extra,
                    &wrap::restart_prompt(handoff_note, &cfg.screen.thresholds()),
                    &self.state_dir,
                    session_id,
                );
                let prompt_text = if self.is_native() {
                    prompt_text
                } else {
                    rollover_receipt_prompt(req, prompt_text, session_id)
                };
                new_adapter.interactive_cmd(Some(&prompt_text), &extra)
            };
            std::iter::once(command.get_program().to_string_lossy().to_string())
                .chain(command.get_args().map(|a| a.to_string_lossy().to_string()))
                .collect()
        };
        // build_turn_env_at omits the interactive pin; self.launch_mode still
        // describes the source, so it must not be taken as successor posture (#160).
        let successor_generation = req.generation.or_else(|| {
            super::super::seat::load(&self.state_dir, self.short()).map(|seat| seat.generation)
        });
        let mut turn_env = super::super::handover::build_turn_env_at(
            new_adapter.as_ref(),
            socket,
            session_id,
            repo,
            role,
            req.target_model.as_deref(),
            successor_generation,
        );
        // Re-export server-verified parent lineage after rebuilding turn environment (#249, #250).
        if let Some(parent) = self.parent_session() {
            turn_env.push((
                super::super::agent::PARENT_SESSION_ENV.to_string(),
                parent.to_string(),
            ));
        }
        Ok(SwapLaunch {
            spec: PaneSpec {
                agent_name: new_adapter.name().to_string(),
                argv: new_argv,
                role,
                verb: self.verb,
                session_id: session_id.to_string(),
                title,
            },
            turn_env,
            provider: new_adapter.provider().to_string(),
            turn_signal_capable: new_adapter.capabilities().turn_signal,
            idle_quiet: Duration::from_millis(cfg.dash.idle_quiet_ms),
        })
    }

    /// Swap harness and model in place while keeping the pane's registry short ID and mail address (#84).
    pub fn handover(
        &mut self,
        cfg: &super::super::config::CtxConfig,
        req: &super::super::handover::HandoverRequest,
        handoff_note: &super::super::handoff::Handoff,
        role: PromptRole,
        repo: &Path,
        size: (u16, u16),
    ) -> CtxResult<()> {
        // Reject wrapped handover for native panes; native rollover belongs to the runtime (#490).
        if !matches!(self.kind, PaneKind::Wrapped(_)) {
            return Err(
                "dashboard pane: a native pane has no harness child to hand over; its \
                        route moves through the runtime rollover instead"
                    .into(),
            );
        }
        if self.pending_handover.is_some() {
            return Err("dashboard pane: a successor is already being checked".into());
        }
        let quit_sequence = super::super::adapters::select(Some(self.agent()), &[], cfg)?
            .quit_sequence()
            .to_string();
        let source_conversation = sessions::native_conversation(
            &self.state_dir,
            self.short(),
            self.agent(),
            self.session_id(),
            super::super::runtime::RuntimeKind::Harness,
        );
        let staged_server = if req.generation.is_some() {
            Some(SignalServer::bind(&staged_socket_path(
                &self.state_dir,
                self.short(),
            ))?)
        } else {
            None
        };
        let launch = self.build_swap_launch(
            cfg,
            req,
            handoff_note,
            role,
            repo,
            &self.session_id.clone(),
            staged_server
                .as_ref()
                .or_else(|| self.pty().and_then(|pty| pty.server.as_ref()))
                .map(super::super::signal::SignalServer::path),
            self.title.clone(),
        )?;
        let SwapLaunch {
            spec:
                PaneSpec {
                    agent_name: new_agent_name,
                    argv: mut new_argv,
                    ..
                },
            turn_env,
            provider: new_provider,
            turn_signal_capable,
            idle_quiet,
        } = launch;
        if req.generation.is_some() && !turn_signal_capable && new_agent_name != "codex" {
            return Err("automatic rollover requires a verified successor answer signal; original session retained".into());
        }
        let mcp_args = super::super::mcp::launch::arguments(
            &new_agent_name,
            repo,
            &self.state_dir,
            self.short(),
            &new_argv,
        );
        super::super::mcp::launch::append(&mut new_argv, mcp_args);

        // Complete every fallible successor step before touching the old child, so failed handover leaves the source live.
        let handover_at = super::super::state::now_secs();
        let (cols, rows) = size;
        let pair = native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let (program, rest) = new_argv
            .split_first()
            .ok_or("dashboard pane: empty argv, nothing to spawn")?;
        super::super::adapters::guard_cmd_shim_reparse(program, rest)?;
        let mut command = CommandBuilder::new(program);
        for arg in rest {
            command.arg(arg);
        }
        command.cwd(repo);
        sessions::scrub_supervision_env(&mut command);
        // A rolled-over worker must not get its secrets back.
        let stripped = scrub_worker_pane_env(&mut command, role, repo, &new_agent_name);
        sessions::secret_env::log_withheld(&self.state_dir, &self.session_id, "pane", &stripped);
        for (key, value) in &turn_env {
            command.env(key, value);
        }

        let mut first_writer = pair.master.take_writer()?;
        wrap::answer_inherit_cursor_probe(&mut *first_writer);
        let writer = Arc::new(Mutex::new(first_writer));

        let started_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let launched_at = Instant::now();
        let child = pair.slave.spawn_command(command)?;
        let lifecycle = supervise::ChildGuard::adopt(child.process_id());
        // Give a successor the predecessor's process priority class (#330).
        if let Some(pid) = child.process_id() {
            super::super::priority::apply_to_child(pid, super::super::priority::posture_for(role));
        }
        drop(pair.slave);
        let master = pair.master;

        let mut reader = master.try_clone_reader()?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            // Give the successor reader the same priority as a freshly spawned pane (#330).
            super::super::priority::raise_current_thread();
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            return;
                        }
                    }
                }
            }
        });

        let prepared = PendingHandover {
            pty: PtyPane {
                master,
                child,
                lifecycle,
                writer,
                rx,
                server: staged_server,
            },
            argv: new_argv,
            agent: new_agent_name,
            provider: new_provider,
            quit_sequence,
            turn_signal_capable,
            idle_quiet,
            launched_at,
            handover_at,
            parser: vt100::Parser::new(rows, cols, SCROLLBACK_ROWS),
            last_output_at: None,
            signal_seen: false,
            forced_drain: req.structural_only,
            started_ms,
            rollout: None,
            last_readiness_poll: None,
            source_input_at: self.last_local_input_at,
            receipt: rollover_receipt_token(req, self.session_id()),
            source_conversation,
        };
        if req.generation.is_some() {
            self.pending_handover = Some(prepared);
            return Ok(());
        }
        self.install_handover(prepared)?;
        Ok(())
    }

    pub(crate) fn has_pending_handover(&self) -> bool {
        self.pending_handover.is_some()
    }

    /// Poll only the candidate. A source turn or repaint is never readiness
    /// evidence for its successor. Keep the UI's source pane alive throughout.
    pub(crate) fn poll_handover(
        &mut self,
        timeout: Duration,
    ) -> (super::super::rollover::Readiness, String) {
        use super::super::rollover::Readiness;
        let Some(pending) = self.pending_handover.as_mut() else {
            return (Readiness::Dead, "the staged successor is gone".to_string());
        };
        if pending.source_input_at != self.last_local_input_at {
            return (
                Readiness::Dead,
                "the source received new input; original session retained".to_string(),
            );
        }
        let (any, _, _) = drain_into(&pending.pty.rx, &mut pending.parser, DRAIN_BUDGET_BYTES);
        if any {
            pending.last_output_at = Some(Instant::now());
        }
        if let Some(server) = pending.pty.server.as_ref() {
            while server.try_recv().is_some() {
                pending.signal_seen = true;
            }
        }
        if let Ok(Some(status)) = pending.pty.child.try_wait() {
            let tail = pending
                .parser
                .screen()
                .contents()
                .lines()
                .rfind(|line| !line.trim().is_empty())
                .unwrap_or("")
                .chars()
                .take(160)
                .collect::<String>();
            return (
                Readiness::Dead,
                format!(
                    "the successor exited before it answered (exit {}): {}",
                    status.exit_code(),
                    tail
                ),
            );
        }
        let codex = pending.agent == "codex";
        if codex
            && !pending.signal_seen
            && pending
                .last_readiness_poll
                .is_none_or(|last| last.elapsed() >= Duration::from_secs(1))
        {
            pending.last_readiness_poll = Some(Instant::now());
            pending.signal_seen = super::super::adapters::codex::successor_answered(
                &self.cwd,
                pending.started_ms,
                &pending.receipt,
                &mut pending.rollout,
            );
        }
        let readiness = super::super::rollover::successor_readiness(
            true,
            pending.turn_signal_capable || codex,
            pending.signal_seen,
            signal_less_quiescent(
                pending.last_output_at,
                None,
                Instant::now(),
                pending.idle_quiet,
            ),
            pending.launched_at.elapsed(),
            timeout,
        );
        (
            readiness,
            "the successor did not answer within handoff.timeout_secs".to_string(),
        )
    }

    pub(crate) fn cancel_handover(&mut self) {
        if let Some(mut pending) = self.pending_handover.take() {
            pending.stop();
            // A candidate hook can report a conversation under this stable
            // address. On abort, keep the live source's observed reference.
            if let Some(conversation) = pending.source_conversation {
                sessions::record_native_conversation(
                    &self.state_dir,
                    self.short(),
                    self.agent(),
                    self.session_id(),
                    &conversation,
                );
            }
        }
    }

    pub(crate) fn commit_handover(&mut self, repo: &Path, generation: u64) -> CtxResult<()> {
        if self.pending_handover.is_none() {
            return Err("no staged successor".into());
        }
        // Preserve the displaced conversation even if the candidate's first
        // hook has already written its own conversation at this seat address.
        let candidate_conversation = self.pending_handover.as_ref().and_then(|pending| {
            sessions::native_conversation(
                &self.state_dir,
                self.short(),
                &pending.agent,
                self.session_id(),
                super::super::runtime::RuntimeKind::Harness,
            )
        });
        if let Some(conversation) = self
            .pending_handover
            .as_ref()
            .and_then(|pending| pending.source_conversation.as_deref())
        {
            sessions::record_native_conversation(
                &self.state_dir,
                self.short(),
                self.agent(),
                self.session_id(),
                conversation,
            );
        }
        // Commit staged ownership before retiring the source; a failed commit cancels only the successor.
        if let Err(error) = super::super::rollover::commit(
            &self.state_dir,
            "dash",
            self.short(),
            generation,
            self.session_id(),
            super::super::state::now_secs(),
        ) {
            self.cancel_handover();
            return Err(error);
        }
        let pending = self.pending_handover.take().ok_or("no staged successor")?;
        super::super::rollover::runtime::settle_subagents(
            &self.state_dir,
            repo,
            self.short(),
            Some(self.session_id()),
            if pending.forced_drain {
                super::super::rollover::runtime::Drain::Forced
            } else {
                super::super::rollover::runtime::Drain::Quiesced
            },
            super::super::state::now_secs(),
        );
        self.install_handover(pending)?;
        if let Some(conversation) = candidate_conversation {
            sessions::record_native_conversation(
                &self.state_dir,
                self.short(),
                self.agent(),
                self.session_id(),
                &conversation,
            );
        }
        self.inject_visible(
            "zirv rollover",
            "The transfer is confirmed. Continue the task from the handoff now.",
        )
    }

    fn install_handover(&mut self, prepared: PendingHandover) -> CtxResult<()> {
        let PendingHandover {
            pty:
                PtyPane {
                    master,
                    child,
                    lifecycle,
                    writer,
                    rx,
                    server,
                },
            argv: new_argv,
            agent: new_agent_name,
            provider: new_provider,
            quit_sequence,
            turn_signal_capable,
            idle_quiet,
            launched_at,
            handover_at,
            parser,
            last_output_at,
            signal_seen,
            ..
        } = prepared;
        // Keep the registry record while switching to the assembled successor, then retire the source.
        self.guard.adopt_child_pid(std::process::id());
        if let PaneKind::Wrapped(pty) = &mut self.kind {
            let mut writer_guard = pty
                .writer
                .lock()
                .map_err(|_| "dashboard pane: writer lock poisoned")?;
            let sink: &mut dyn Write = &mut **writer_guard;
            wrap::quit_child(sink, &mut pty.child, &quit_sequence, QUIT_GRACE)?;
        }
        // Release the retired child's console-close guard before adopting the successor's guard.
        self.release_lifecycle();

        if let Some(child_pid) = child.process_id() {
            self.guard.adopt_child_pid(child_pid);
        }

        // Settle the source reservation before replacing its provider; reap
        // derives provider from the successor and must not charge source tokens to it (#358).
        let old_provider =
            super::super::adapters::provider_for_agent_name(Some(&self.agent_name)).to_string();
        if let Some(old_id) = self.reservation_id.take() {
            let _ = super::super::reservation::release(&self.state_dir, &old_provider, &old_id);
        }
        self.reservation_id = match super::super::reservation::reserve(
            &self.state_dir,
            &new_provider,
            &self.session_id,
            self.budget_tokens().unwrap_or(0),
            super::super::state::now_secs(),
        ) {
            Ok(reservation) => Some(reservation.id),
            Err(e) => {
                eprintln!(
                    "zirv ctx dash: failed to record a token reservation for provider \
                     '{new_provider}' after handover: {e}"
                );
                None
            }
        };

        self.agent_name = new_agent_name;
        // The turn-signal socket is this pane's own and survives the swap:
        // the successor is told the same socket path, so the server moves to
        // the new backend rather than being rebound.
        let server = server.or_else(|| match &mut self.kind {
            PaneKind::Wrapped(pty) => pty.server.take(),
            _ => None,
        });
        if let Some(server) = &server {
            wrap::publish_socket_path(&self.state_dir, &self.session_id, server.path());
        }
        self.kind = PaneKind::Wrapped(PtyPane {
            master,
            child,
            lifecycle,
            writer,
            rx,
            server,
        });
        // A fresh channel has nothing outstanding on it: whatever the old
        // child left queued died with its receiver.
        self.pending_output = false;
        self.parser = parser;
        self.turn_signal_capable = turn_signal_capable;
        self.idle_quiet = idle_quiet;
        self.last_signal_at = signal_seen.then(Instant::now);
        self.launch_model = super::super::adapters::last_model_flag(&new_argv).map(str::to_string);
        self.measured_usage = None;
        // Reset child-scoped budget warnings and hard-stop grace for a fresh successor.
        self.budget_soft_warned = false;
        self.budget_grace_given = false;
        self.last_output_at = last_output_at;
        self.last_local_input_at = None;
        self.injected_awaiting_turn = false;
        self.user_typed_since_turn = false;
        self.exit_code = None;
        self.launched_at = launched_at;
        self.exited_after = None;
        // Drop deferred submit from the retired PTY so its carriage return cannot reach the successor.
        self.cancel_submission();
        self.delivery_sender = None;
        // Reset one-shot report reminder for a new child session while retaining its report target (#116).
        self.report_reminder_sent = false;
        self.settled_mail_sent = false;
        // Clear old-child mail dedup state when a successor takes over (#468).
        self.mail_block_log = None;
        // Clear the old Codex rollout pin only after commit; an aborted swap
        // leaves the source running and still needs its pinned transcript.
        super::super::adapters::codex::forget_transcript_pin(
            &self.state_dir,
            self.short(),
            handover_at,
        );

        Ok(())
    }

    /// Caches the child's exit code the first time it is observed, so
    /// `state()` can stay a cheap, side-effect-free read: `try_wait` needs
    /// `&mut Child`, `state()` does not take `&mut self`, so every mutating
    /// caller (`drain`, `on_turn_signal`) polls on the pane's behalf.
    fn poll_exit(&mut self) {
        if self.exit_code.is_some() {
            return;
        }
        if let Ok(Some(status)) = self.pty_try_wait() {
            self.exit_code = Some(status.exit_code() as i32);
            self.exited_after = Some(self.launched_at.elapsed());
        }
    }
}

/// Strip secret-shaped env from a Worker pane's child; an orchestrator or operator pane is untouched.
/// A refused or broken config scrubs with the trusted layers or defaults; only an unselectable adapter leaves the env as it was.
fn scrub_worker_pane_env(
    command: &mut CommandBuilder,
    role: PromptRole,
    repo: &Path,
    agent_name: &str,
) -> Vec<String> {
    if role != PromptRole::Worker {
        return Vec::new();
    }
    let env = super::super::config::env_from_process();
    scrub_worker_pane_env_in(
        command,
        repo,
        agent_name,
        &env,
        std::env::vars_os().filter_map(|(name, _)| name.into_string().ok()),
    )
}

fn scrub_worker_pane_env_in(
    command: &mut CommandBuilder,
    repo: &Path,
    agent_name: &str,
    env: super::super::config::EnvLookup<'_>,
    ambient: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let cfg = super::super::config::CtxConfig::load_refusal_safe(repo, env);
    let Ok(adapter) = super::super::adapters::select(Some(agent_name), &[], &cfg) else {
        return Vec::new();
    };
    scrub_pane_env_with(
        command,
        PromptRole::Worker,
        cfg.sandbox.scrub_worker_secrets,
        adapter.credential_env(env).as_deref(),
        ambient,
    )
}

fn scrub_pane_env_with(
    command: &mut CommandBuilder,
    role: PromptRole,
    enabled: bool,
    keep: Option<&[String]>,
    ambient: impl IntoIterator<Item = String>,
) -> Vec<String> {
    if role != PromptRole::Worker {
        return Vec::new();
    }
    sessions::secret_env::scrub_worker_secrets_pty(command, enabled, keep, ambient)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn only_a_worker_pane_loses_secret_env() {
        let keep = vec!["ANTHROPIC_API_KEY".to_string()];
        let env = || {
            vec![
                "MY_SERVICE_PASSWORD".to_string(),
                "ANTHROPIC_API_KEY".to_string(),
            ]
        };
        let builder = || {
            let mut b = CommandBuilder::new("agent");
            b.env("MY_SERVICE_PASSWORD", "x");
            b.env("ANTHROPIC_API_KEY", "k");
            b
        };
        let mut worker = builder();
        let stripped =
            scrub_pane_env_with(&mut worker, PromptRole::Worker, true, Some(&keep), env());
        assert_eq!(stripped, vec!["MY_SERVICE_PASSWORD".to_string()]);
        assert!(worker.get_env("MY_SERVICE_PASSWORD").is_none());
        assert!(worker.get_env("ANTHROPIC_API_KEY").is_some());
        for role in [PromptRole::Orchestrator, PromptRole::Single] {
            let mut seat = builder();
            assert!(scrub_pane_env_with(&mut seat, role, true, Some(&keep), env()).is_empty());
            assert!(seat.get_env("MY_SERVICE_PASSWORD").is_some());
        }
    }

    /// A repo config the loader refuses (here, switching the scrub off itself) must not
    /// switch the scrub off: the secret goes, the harness credential stays.
    #[test]
    fn a_refused_repo_config_still_scrubs_a_worker_pane() {
        let repo = tempfile::tempdir().expect("repo");
        let home = tempfile::tempdir().expect("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/ctx.toml"),
            "[sandbox]\nscrub_worker_secrets = false\n",
        )
        .expect("write");
        let mut worker = CommandBuilder::new("agent");
        worker.env("MY_SERVICE_PASSWORD", "x");
        worker.env("ANTHROPIC_API_KEY", "k");
        let stripped = scrub_worker_pane_env_in(
            &mut worker,
            repo.path(),
            "claude",
            &|_| None,
            [
                "MY_SERVICE_PASSWORD".to_string(),
                "ANTHROPIC_API_KEY".to_string(),
            ],
        );
        assert_eq!(stripped, vec!["MY_SERVICE_PASSWORD".to_string()]);
        assert!(worker.get_env("MY_SERVICE_PASSWORD").is_none());
        assert!(worker.get_env("ANTHROPIC_API_KEY").is_some());
    }

    /// #681: a lifecycle hook files a session's conversation marker under its
    /// socket's file stem. The staged successor's socket must therefore carry
    /// the seat's short, or after a committed rollover every marker lands on a
    /// random address and the seat keeps resuming the conversation from before.
    #[test]
    fn staged_socket_keeps_the_seat_short_as_its_stem() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(tmp.path().join("state"));
        let short = "abcd1234";
        let live = state.socket_for(short);
        crate::commands::ctx::state::create_private_dir_all(&state.sockets()).unwrap();
        std::fs::write(&live, "").unwrap();
        let staged = staged_socket_path(&state, short);
        assert_ne!(staged, live);
        assert!(!staged.exists());
        assert_eq!(staged.file_stem().and_then(|s| s.to_str()), Some(short));
        assert!(
            staged.as_os_str().len() <= live.as_os_str().len(),
            "{}",
            staged.display()
        );
    }

    /// #710: exercise usage admission, a real PTY successor, and the dashboard's
    /// settlement loop. The source process, its worker, socket and screen survive
    /// both an exit-2 launch failure and a successor that never answers.
    #[cfg(unix)]
    #[test]
    fn usage_rollover_keeps_source_until_successor_answers() {
        use crate::commands::ctx::{rollover, seat, window};
        for outcome in ["exit2", "timeout", "ready", "commit-failure", "new-input"] {
            let tmp = tempfile::tempdir().unwrap();
            let state = StateDir::from_root(tmp.path().join("s"));
            let repo = tmp.path().join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            let script = tmp.path().join("successor.sh");
            std::fs::write(
                &script,
                if outcome == "exit2" {
                    "#!/bin/sh\necho 'error: incompatible CLI flags'\nexit 2\n"
                } else {
                    "#!/bin/sh\necho 'candidate startup screen'\nexec sleep 60\n"
                },
            )
            .unwrap();
            let mut cfg = super::super::CtxConfig {
                agent_bin: Some(format!("sh {}", script.display())),
                ..Default::default()
            };
            cfg.pace.estimator = false;
            cfg.fallback.auto_orchestrator_rollover = Some(true);
            let session = "71000000-3333-4444-8888-555555555555";
            let mut spec = test_spec(session);
            spec.agent_name = "claude".into();
            spec.role = PromptRole::Orchestrator;
            spec.verb = Verb::Chat;
            spec.argv = vec![
                "sh".into(),
                "-c".into(),
                "echo source-context; sleep 60 & echo $! > worker.pid; wait".into(),
            ];
            let mut source = Pane::spawn(
                spec,
                &state,
                &repo,
                &repo,
                (80, 24),
                &[],
                true,
                DEFAULT_IDLE_QUIET,
            )
            .unwrap();
            let pid = source.child_pid().unwrap();
            let short = source.short().to_string();
            sessions::record_native_conversation(
                &state,
                &short,
                "claude",
                session,
                "original-conversation",
            );
            let socket = source
                .pty()
                .unwrap()
                .server
                .as_ref()
                .unwrap()
                .path()
                .to_path_buf();
            let now = crate::commands::ctx::state::now_secs();
            for (provider, used) in [("anthropic", 90.0), ("openai", 10.0)] {
                window::store_for(
                    &state,
                    provider,
                    &window::UsageWindows {
                        five_hour: None,
                        seven_day: Some(window::Window {
                            used_percentage: used,
                            resets_at: now + 86_400,
                            observed_at: now,
                            overage_covered: false,
                            limit_reached: false,
                        }),
                    },
                )
                .unwrap();
            }
            let rollover::Evaluation::Rollover {
                request,
                generation,
                ..
            } = rollover::evaluate(
                &state, &cfg, "dash", &short, now, true, None, true, &mut None,
            )
            else {
                panic!("usage exhaustion must prepare a successor")
            };
            let note = crate::commands::ctx::handoff::structural(
                &crate::commands::ctx::event::StructuralContext {
                    user_messages: vec!["Finish the task while preserving the worker".into()],
                    ..Default::default()
                },
            );
            let plan = rollover::runtime::plan_successor(
                crate::commands::ctx::runtime::RuntimeKind::Harness,
                crate::commands::ctx::runtime::RuntimeKind::Harness,
                &short,
                generation,
                Some("codex"),
                request.target_model.as_deref(),
                None,
                None,
                None,
            );
            let mut launcher = super::super::PaneSuccessorLauncher {
                pane: &mut source,
                cfg: &cfg,
                req: &request,
                note: &note,
                role: PromptRole::Orchestrator,
                repo: &repo,
                size: (80, 24),
                native: Default::default(),
            };
            rollover::runtime::launch_successor(
                &state,
                &repo,
                &mut launcher,
                &plan,
                Some(session),
                rollover::runtime::Drain::Quiesced,
                now,
            )
            .unwrap();
            assert_eq!(source.child_pid(), Some(pid));
            sessions::record_native_conversation(
                &state,
                &short,
                "codex",
                session,
                "unconfirmed-conversation",
            );
            assert_ne!(
                source
                    .pending_handover
                    .as_ref()
                    .unwrap()
                    .pty
                    .server
                    .as_ref()
                    .unwrap()
                    .path(),
                socket
            );
            let deadline = Instant::now() + Duration::from_secs(10);
            while !repo.join("worker.pid").is_file() && Instant::now() < deadline {
                source.drain();
                std::thread::sleep(Duration::from_millis(10));
            }
            source.drain();
            let worker: u32 = std::fs::read_to_string(repo.join("worker.pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            // Neither a source turn nor quiet startup output proves a Codex answer.
            source.last_signal_at = Some(Instant::now());
            if outcome == "new-input" {
                source.last_local_input_at = Some(Instant::now());
            }
            let staged = source.pending_handover.as_mut().unwrap();
            staged.last_output_at = Some(Instant::now() - Duration::from_secs(30));
            staged.last_readiness_poll = Some(Instant::now());
            if outcome == "timeout" {
                staged.launched_at =
                    Instant::now() - Duration::from_secs(cfg.handoff.timeout_secs + 1);
            } else if matches!(outcome, "ready" | "commit-failure") {
                let rollout = tmp.path().join("answer.jsonl");
                std::fs::write(&rollout,
                    serde_json::json!({"type":"event_msg", "payload": {"type":"task_complete", "last_agent_message": staged.receipt}}).to_string()).unwrap();
                staged.rollout = Some(rollout);
                staged.last_readiness_poll = None;
                if outcome == "commit-failure" {
                    seat::abort(&state, &short, generation, now).unwrap();
                }
            }
            let mut panes = vec![source];
            let mut pending = Some((short.clone(), generation, Instant::now()));
            let mut errors = super::super::ErrorLog::default();
            while pending.is_some() && Instant::now() < deadline {
                panes[0].drain();
                super::super::settle_pending_rollover(
                    &mut panes,
                    &cfg,
                    &repo,
                    &state,
                    &mut pending,
                    &mut errors,
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(pending.is_none(), "{outcome}: candidate did not settle");
            let saved = seat::load(&state, &short).unwrap();
            let log =
                std::fs::read_to_string(state.logs().join(crate::commands::ctx::log::LOG_FILE))
                    .unwrap();
            assert_eq!(log.matches(rollover::PREPARED).count(), 1, "{log}");
            if outcome == "ready" {
                assert_ne!(panes[0].child_pid(), Some(pid));
                assert_eq!(saved.generation, generation);
                assert_eq!(log.matches(rollover::COMMITTED).count(), 1);
                assert_eq!(saved.rollover_failures, 0);
            } else {
                assert_eq!(panes[0].child_pid(), Some(pid));
                assert_eq!(panes[0].agent(), "claude");
                assert!(sessions::is_alive(pid));
                assert!(
                    sessions::is_alive(worker),
                    "{outcome}: the subagent must survive"
                );
                assert!(panes[0].screen().contents().contains("source-context"));
                assert!(
                    !panes[0]
                        .screen()
                        .contents()
                        .contains("Continue from the handoff")
                );
                assert_eq!(
                    panes[0].pty().unwrap().server.as_ref().unwrap().path(),
                    socket
                );
                assert_eq!(
                    sessions::native_conversation(
                        &state,
                        &short,
                        "claude",
                        session,
                        crate::commands::ctx::runtime::RuntimeKind::Harness
                    )
                    .as_deref(),
                    Some("original-conversation")
                );
                assert_eq!(saved.generation, 1);
                assert_eq!(saved.rollover_failures, 1);
                assert!(saved.last_rollover_at.is_some());
                assert_eq!(log.matches(rollover::FAILED).count(), 1, "{log}");
                assert!(!log.contains(rollover::COMMITTED));
                if outcome == "exit2" {
                    assert!(log.contains("exit 2"), "{log}");
                }
                let refreshed = now + 1;
                window::store_for(
                    &state,
                    "anthropic",
                    &window::UsageWindows {
                        five_hour: None,
                        seven_day: Some(window::Window {
                            used_percentage: 91.0,
                            resets_at: now + 86_400,
                            observed_at: refreshed,
                            overage_covered: false,
                            limit_reached: false,
                        }),
                    },
                )
                .unwrap();
                assert!(
                    matches!(rollover::evaluate(&state, &cfg, "dash", &short, refreshed, true, None, true, &mut None),
                    rollover::Evaluation::Skip(reason) if reason.contains("backoff"))
                );
            }
            panes[0].finish_shutdown().unwrap();
        }
    }

    #[test]
    fn pane_state_maps_turn_signals_to_glyph_states() {
        assert!(matches!(state_from(false, None, false), PaneState::Working));
        assert!(matches!(state_from(true, None, false), PaneState::Idle));
        assert!(matches!(
            state_from(true, Some(0), false),
            PaneState::Ended(0)
        ));
        assert!(matches!(
            state_from(false, Some(3), false),
            PaneState::Ended(3)
        ));
    }

    // O1: the post-turn repaint debounce. Every case is decided from two
    // timestamps and a window, so none of it needs a real child.

    /// A harness repainting its prompt straight after a turn must not latch
    /// the pane into `Working`: once the debounce window has elapsed with
    /// nothing further from the child, the signal still stands.
    #[test]
    fn a_repaint_right_after_a_turn_signal_leaves_the_pane_idle() {
        let debounce = Duration::from_millis(500);
        let signal = Instant::now();
        let repaint = signal + Duration::from_millis(50);

        assert!(
            !signal_still_stands(Some(signal), Some(repaint), repaint, debounce),
            "inside the window the burst is still undecided, so the pane is not yet idle"
        );
        assert!(
            signal_still_stands(
                Some(signal),
                Some(repaint),
                signal + Duration::from_millis(600),
                debounce
            ),
            "and once the window closes with nothing further, the signal stands"
        );
        assert!(
            signal_still_stands(
                Some(signal),
                Some(repaint),
                signal + Duration::from_secs(30),
                debounce
            ),
            "it does not decay: a pane idle at its prompt stays reachable"
        );
    }

    /// F1: output that keeps coming keeps the pane `Working` for as long as it
    /// lasts -- the quiet window restarts on every byte, so a burst that runs
    /// for a minute never looks idle part way through it.
    #[test]
    fn continuous_output_after_a_turn_signal_keeps_the_pane_working() {
        let debounce = Duration::from_millis(500);
        let signal = Instant::now();

        // A byte every 100ms for three seconds: at no point is the pane idle,
        // because the last byte is never more than 100ms old.
        for step in 1..=30u64 {
            let at = signal + Duration::from_millis(100 * step);
            assert!(
                !signal_still_stands(Some(signal), Some(at), at, debounce),
                "streaming output at +{}ms must not read as idle",
                100 * step
            );
        }
    }

    /// F1, the bug the old rule had on the far side of the window: output
    /// arriving *after* `signal + debounce` used to latch the pane into
    /// `Working` until a next turn signal that, for a harness sitting at its
    /// prompt, never comes -- so a zoom repaint or an echoed keystroke killed
    /// delivery to that pane for the rest of the session. A burst is now just
    /// a burst: once it stops, one debounce later the pane is idle again.
    #[test]
    fn a_late_repaint_burst_goes_idle_again_once_it_stops() {
        let debounce = Duration::from_millis(500);
        let signal = Instant::now();
        let burst_end = signal + Duration::from_millis(900);

        assert!(
            !signal_still_stands(Some(signal), Some(burst_end), burst_end, debounce),
            "while the burst is running the pane is working"
        );
        assert!(
            signal_still_stands(
                Some(signal),
                Some(burst_end),
                burst_end + Duration::from_millis(600),
                debounce
            ),
            "and a debounce after the last byte it is reachable again"
        );
        assert!(
            signal_still_stands(
                Some(signal),
                Some(burst_end),
                burst_end + Duration::from_secs(300),
                debounce
            ),
            "it does not decay back to working with nothing further happening"
        );
    }

    #[test]
    fn a_pane_is_working_until_it_first_reports_a_turn_boundary() {
        let debounce = Duration::from_millis(500);
        let now = Instant::now();
        assert!(!signal_still_stands(None, None, now, debounce));
        assert!(
            !signal_still_stands(None, Some(now), now, debounce),
            "output alone never makes a pane idle"
        );
        assert!(
            !signal_still_stands(None, None, now + Duration::from_secs(30), debounce),
            "and no amount of quiet substitutes for a turn boundary"
        );
        assert!(
            !signal_still_stands(Some(now), None, now, debounce),
            "a fresh signal with no output recorded measures its quiet from the signal"
        );
        assert!(
            signal_still_stands(Some(now), None, now + Duration::from_millis(600), debounce),
            "and is idle once that window elapses"
        );
        assert!(
            signal_still_stands(Some(now), Some(now - Duration::from_secs(5)), now, debounce),
            "output from before the signal is what the signal already accounted for"
        );
    }

    // Task A: the signal-less idleness path (`output_quiescent`/
    // `pane_is_idle`). A codex-shaped adapter never reports a turn boundary at
    // all, so gating idleness on one leaves such a pane `Working` forever;
    // these cover the output-quiescence stand-in on its own terms, pure and
    // without a real child.

    /// No output ever recorded is not quiet, whatever else has happened --
    /// the signal-less mirror of `signal_still_stands`' own "no signal ever
    /// seen -- not idle" rule. A pane still starting up (its harness has not
    /// drawn a first frame yet) must not read the same as one sitting quietly
    /// at its prompt just because both currently have no timestamp.
    #[test]
    fn output_quiescent_is_false_with_no_output_ever_recorded() {
        let quiet = Duration::from_millis(200);
        let now = Instant::now();
        assert!(!output_quiescent(None, now, quiet));
        assert!(
            !output_quiescent(None, now + Duration::from_secs(30), quiet),
            "no amount of elapsed time substitutes for output that never happened"
        );
    }

    /// Once there is a last-output timestamp, quiet is a plain elapsed-time
    /// check against it -- false inside the window, true once it closes, and
    /// it does not decay back to working with nothing further happening.
    #[test]
    fn output_quiescent_flips_once_the_quiet_window_elapses_since_the_last_output() {
        let quiet = Duration::from_millis(200);
        let output = Instant::now();
        assert!(
            !output_quiescent(Some(output), output + Duration::from_millis(50), quiet),
            "still inside the quiet window"
        );
        assert!(
            !output_quiescent(Some(output), output + Duration::from_millis(199), quiet),
            "one tick short of the window is still not quiet"
        );
        assert!(
            output_quiescent(Some(output), output + Duration::from_millis(200), quiet),
            "exactly at the window is quiet"
        );
        assert!(
            output_quiescent(Some(output), output + Duration::from_secs(30), quiet),
            "and it does not decay back once quiet"
        );
    }

    /// `pane_is_idle`'s branch selection: a signal-capable pane is exactly
    /// `signal_still_stands` -- quiet output alone, with no signal ever seen,
    /// is not enough -- while a signal-less pane is exactly
    /// `signal_less_quiescent`, and `signal_at` is never consulted for it at
    /// all (matching reality: nothing ever writes to a signal-less pane's own
    /// turn-signal socket, so a `Some` there could not honestly arise, but
    /// the branch must not depend on that being true to behave correctly).
    /// `local_input_at` is likewise never consulted on the signal-capable
    /// branch.
    #[test]
    fn pane_is_idle_branches_on_turn_signal_capability() {
        let debounce = Duration::from_millis(500);
        let idle_quiet = Duration::from_millis(200);
        let output = Instant::now();

        assert!(
            !pane_is_idle(
                true,
                None,
                Some(output),
                None,
                output + Duration::from_secs(30),
                debounce,
                idle_quiet
            ),
            "signal-capable: no signal ever seen stays working, however long the output has been quiet"
        );
        assert!(
            !pane_is_idle(
                true,
                None,
                Some(output),
                Some(output + Duration::from_secs(29)),
                output + Duration::from_secs(30),
                debounce,
                idle_quiet
            ),
            "signal-capable: local_input_at is never consulted on this branch either"
        );

        assert!(
            !pane_is_idle(
                false,
                Some(output),
                Some(output),
                None,
                output + Duration::from_millis(50),
                debounce,
                idle_quiet
            ),
            "signal-less: still inside its own quiet window"
        );
        assert!(
            pane_is_idle(
                false,
                Some(output),
                Some(output),
                None,
                output + Duration::from_millis(200),
                debounce,
                idle_quiet
            ),
            "signal-less: quiet window elapsed, idle with a signal present"
        );
        assert!(
            pane_is_idle(
                false,
                None,
                Some(output),
                None,
                output + Duration::from_millis(200),
                debounce,
                idle_quiet
            ),
            "signal-less: same outcome with no signal_at at all -- it is never consulted"
        );
    }

    /// H1 (review): local input holds a signal-less pane non-idle for a full
    /// `idle_quiet` window measured from *itself*, even with no child output
    /// at all recorded since -- the fix for the bug where an injection or a
    /// keystroke, which only ever happen while the pane already reads quiet,
    /// left `output_at` untouched and so read as still-quiet on the very next
    /// tick.
    #[test]
    fn pane_is_idle_measures_signal_less_quiet_from_the_latest_of_output_and_local_input() {
        let debounce = Duration::from_millis(500);
        let idle_quiet = Duration::from_millis(200);
        let output = Instant::now();

        // No output at all, only local input: not idle until idle_quiet has
        // elapsed since that input, and idle after.
        assert!(
            !pane_is_idle(
                false,
                None,
                None,
                Some(output),
                output + Duration::from_millis(50),
                debounce,
                idle_quiet
            ),
            "a fresh local input alone must hold the pane non-idle"
        );
        assert!(
            pane_is_idle(
                false,
                None,
                None,
                Some(output),
                output + Duration::from_millis(200),
                debounce,
                idle_quiet
            ),
            "and release it once idle_quiet has elapsed since that input"
        );

        // Output happened first and is already quiet, but local input landed
        // later: the later timestamp governs, not the older output.
        let later_input = output + Duration::from_millis(150);
        assert!(
            !pane_is_idle(
                false,
                None,
                Some(output),
                Some(later_input),
                later_input + Duration::from_millis(50),
                debounce,
                idle_quiet
            ),
            "output alone looks quiet (150ms+50ms=200ms old) but the later local \
             input must be what quiescence is measured from"
        );
        assert!(
            pane_is_idle(
                false,
                None,
                Some(output),
                Some(later_input),
                later_input + Duration::from_millis(200),
                debounce,
                idle_quiet
            ),
            "idle once idle_quiet has elapsed since the later of the two"
        );

        // And symmetrically, output arriving after an old local input is what
        // governs.
        let later_output = output + Duration::from_millis(150);
        assert!(
            !pane_is_idle(
                false,
                None,
                Some(later_output),
                Some(output),
                later_output + Duration::from_millis(50),
                debounce,
                idle_quiet
            ),
            "fresh output after old local input must also restart the window"
        );
    }

    /// R3: a pane that was just injected into is `Working` even though its
    /// last observed signal still says "idle" -- and an exit still wins over
    /// both.
    #[test]
    fn a_pending_injection_reports_working_until_the_next_turn_signal() {
        assert!(matches!(state_from(true, None, true), PaneState::Working));
        assert!(matches!(state_from(false, None, true), PaneState::Working));
        assert!(
            matches!(state_from(true, Some(0), true), PaneState::Ended(0)),
            "an exited pane is Ended regardless of a pending injection"
        );
    }

    /// G1: operator typing is the same "do not inject" signal
    /// `wrap::may_inject` already honours -- a half-composed prompt must not be
    /// submitted by an injected line landing on top of it -- but, unlike
    /// before, it no longer changes what `PaneState` the pane reports: a pane
    /// the operator typed into and then left alone still renders `Idle`, it is
    /// just not `injectable` until its next turn signal.
    #[test]
    fn operator_typing_keeps_a_pane_uninjectable_but_still_renders_idle() {
        assert!(
            !injectable_from(PaneState::Idle, false, true),
            "typing makes the pane ineligible for injection"
        );
        assert!(
            injectable_from(PaneState::Idle, false, false),
            "and the very next turn boundary, which clears the flag, makes it eligible again"
        );
        assert!(
            !injectable_from(PaneState::Working, false, true),
            "a working pane is never injectable regardless of typing"
        );
        assert!(
            !injectable_from(PaneState::Ended(0), false, true),
            "an ended pane is never injectable regardless of typing"
        );
    }

    /// G1: `injectable_from`'s explicit `injected_awaiting_turn` check is
    /// belt-and-suspenders (state `Idle` already implies it is false), but it
    /// must still hold on its own terms.
    #[test]
    fn a_pending_injection_is_never_injectable_even_if_state_somehow_says_idle() {
        assert!(!injectable_from(PaneState::Idle, true, false));
    }

    /// The clamp/step arithmetic behind the wheel and `Ctrl+A PageUp`: neither
    /// end may run away, and a `usize` offset must never underflow past the
    /// live view.
    #[test]
    fn scroll_offset_clamps_at_the_live_view_and_at_the_end_of_history() {
        assert_eq!(scroll_offset(0, 3, 100), 3, "a wheel notch scrolls back");
        assert_eq!(scroll_offset(3, -3, 100), 0, "and back down again");
        assert_eq!(
            scroll_offset(0, -3, 100),
            0,
            "scrolling down at the live view is a no-op, not an underflow"
        );
        assert_eq!(
            scroll_offset(98, 3, 100),
            100,
            "scrolling up stops at the end of the recorded history"
        );
        assert_eq!(
            scroll_offset(100, 1, 100),
            100,
            "and stays there rather than running into blank rows"
        );
        assert_eq!(
            scroll_offset(5, 0, 100),
            5,
            "a zero-row scroll changes nothing"
        );
        assert_eq!(
            scroll_offset(0, 10, 0),
            0,
            "a pane with no history at all cannot be scrolled"
        );
    }

    /// A burst of wheel notches (or a `usize::MAX` "jump to the top") must
    /// saturate rather than wrap: the arithmetic runs in `isize`, and both
    /// extremes are reachable from a real terminal.
    #[test]
    fn scroll_offset_saturates_instead_of_wrapping() {
        assert_eq!(scroll_offset(0, isize::MAX, 100), 100);
        assert_eq!(scroll_offset(100, isize::MIN, 100), 0);
        assert_eq!(
            scroll_offset(1000, isize::MAX, usize::MAX),
            isize::MAX as usize,
            "a jump to the top saturates; vt100's own clamp then cuts it to the real history"
        );
    }

    /// End to end through the real parser, no child needed: rows that scroll
    /// off the top are recorded, `scroll_by`/`scroll_to_top`/`scroll_to_live`
    /// move the viewport over them, and the *rendered* screen follows -- which
    /// is what lets `ui::render_grid` stay unchanged.
    #[test]
    fn a_parser_with_scrollback_shows_retired_rows_when_scrolled_back() {
        let mut parser = vt100::Parser::new(3, 20, SCROLLBACK_ROWS);
        for line in 0..10 {
            parser.process(format!("line{line}\r\n").as_bytes());
        }
        assert_eq!(parser.screen().scrollback(), 0, "starts at the live view");

        parser
            .screen_mut()
            .set_scrollback(scroll_offset(0, 3, 1000));
        assert_eq!(parser.screen().scrollback(), 3);
        assert_eq!(
            last_line_of(parser.screen()),
            "line7",
            "three rows back, the bottom row is three lines earlier"
        );

        // Past the end of the recorded history: vt100 clamps rather than
        // showing blanks, and reports the clamped value back.
        parser.screen_mut().set_scrollback(usize::MAX);
        let top = parser.screen().scrollback();
        assert!(
            top > 0 && top < usize::MAX,
            "clamped to real history: {top}"
        );

        parser.screen_mut().set_scrollback(0);
        assert_eq!(parser.screen().scrollback(), 0);
        assert_eq!(last_line_of(parser.screen()), "line9", "back to live");
    }

    /// The regression that made scrollback unreachable in the first place: the
    /// parser was built with a scrollback length of `0`, so vt100 discarded
    /// every retired row instead of keeping it. With no recorded history there
    /// is nothing for any amount of `set_scrollback` to show.
    #[test]
    fn a_parser_without_scrollback_records_no_history_at_all() {
        let mut parser = vt100::Parser::new(3, 20, 0);
        for line in 0..10 {
            parser.process(format!("line{line}\r\n").as_bytes());
        }
        parser.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(
            parser.screen().scrollback(),
            0,
            "nothing was ever recorded, so the offset clamps straight back to live"
        );
    }

    /// The root cause of the second scrolling report, pinned against the real
    /// vt100: the alternate screen has **no scrollback at all**. vt100 builds
    /// the alternate grid with `Grid::new(size, 0)` and `set_scrollback` acts
    /// on whichever grid is drawing, so while a full-screen TUI child holds
    /// `\x1b[?1049h` every scroll request clamps straight back to `0` -- no
    /// value of `SCROLLBACK_ROWS` can change that, which is why branch (B)
    /// forwards arrows instead of moving an offset that cannot move.
    #[test]
    fn the_alternate_screen_has_no_scrollback_for_any_offset_to_move_in() {
        let mut parser = vt100::Parser::new(3, 20, SCROLLBACK_ROWS);
        for line in 0..10 {
            parser.process(format!("line{line}\r\n").as_bytes());
        }
        assert!(!parser.screen().alternate_screen());
        assert!(
            matches!(scroll_parser(&mut parser, 3), ScrollOutcome::Scrolled(3)),
            "sanity: the normal screen scrolls"
        );
        parser.screen_mut().set_scrollback(0);

        // The harness enters full-screen mode.
        parser.process(b"\x1b[?1049h");
        assert!(parser.screen().alternate_screen());
        for line in 0..10 {
            parser.process(format!("alt{line}\r\n").as_bytes());
        }
        assert_eq!(
            scroll_parser(&mut parser, 3),
            ScrollOutcome::AtOldest,
            "nothing was recorded and nothing can move: this is the whole bug"
        );
        assert_eq!(parser.screen().scrollback(), 0);
    }

    /// The same fact against a **real recorded claude session** rather than a
    /// hand-written escape sequence, since two rounds of this bug have already
    /// been fixed against the wrong mechanism.
    /// `tests/fixtures/claude-session.raw` is a gitignored capture of a real
    /// interactive session (present on the machine that recorded it, absent in
    /// CI -- skipped there, like `ui`'s own fixture test): claude sends
    /// `\x1b[?1049h` about six kilobytes in and never leaves for the remaining
    /// ~550 KB, so for essentially the whole session the pane is on the
    /// alternate screen, where vt100 records no history at all. That is why
    /// the previous fix -- raising the parser's scrollback length -- changed
    /// nothing the operator could see.
    #[test]
    fn a_real_claude_session_spends_itself_on_the_alternate_screen() {
        let path = std::path::Path::new("tests/fixtures/claude-session.raw");
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!(
                "skipping a_real_claude_session_spends_itself_on_the_alternate_screen: {} not present",
                path.display()
            );
            return;
        };

        let mut parser = vt100::Parser::new(40, 120, SCROLLBACK_ROWS);
        parser.process(&bytes);
        assert!(
            parser.screen().alternate_screen(),
            "a real claude session ends on the alternate screen"
        );
        assert_eq!(
            scroll_parser(&mut parser, 3),
            ScrollOutcome::AtOldest,
            "and there is no scrollback there for any offset to move in -- \
             which is the whole of the reported bug"
        );
        // And it is not merely full-screen: it asked to be sent mouse events
        // (`?1000h ?1002h ?1003h ?1006h` are all in the capture), in the SGR
        // encoding. So the wheel is *its* event -- the dashboard consuming it
        // for a buffer that can never fill is the bug, and branch (B) hands it
        // over instead.
        assert_ne!(
            parser.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None,
            "a real claude session turns mouse reporting on"
        );
        assert_eq!(
            parser.screen().mouse_protocol_encoding(),
            vt100::MouseProtocolEncoding::Sgr,
            "and selects SGR coordinates (?1006h)"
        );
    }

    /// Branch (A)'s reported outcomes, both ends included -- "already at the
    /// oldest line" and "already at the live view" are what the header says
    /// instead of the silence that got this reported twice.
    #[test]
    fn scroll_parser_reports_movement_and_both_clamped_ends() {
        let mut parser = vt100::Parser::new(3, 20, SCROLLBACK_ROWS);
        assert_eq!(
            scroll_parser(&mut parser, 3),
            ScrollOutcome::AtOldest,
            "a pane with no history yet cannot scroll back"
        );
        assert_eq!(scroll_parser(&mut parser, -3), ScrollOutcome::AtLive);

        for line in 0..10 {
            parser.process(format!("line{line}\r\n").as_bytes());
        }
        assert_eq!(scroll_parser(&mut parser, 3), ScrollOutcome::Scrolled(3));
        assert_eq!(scroll_parser(&mut parser, -1), ScrollOutcome::Scrolled(2));
        assert_eq!(scroll_parser(&mut parser, -5), ScrollOutcome::Scrolled(0));
        assert_eq!(scroll_parser(&mut parser, -5), ScrollOutcome::AtLive);
        // Past the oldest recorded row: vt100 clamps, and the second attempt
        // has genuinely nowhere left to go.
        assert!(matches!(
            scroll_parser(&mut parser, 10_000),
            ScrollOutcome::Scrolled(_)
        ));
        assert_eq!(scroll_parser(&mut parser, 10_000), ScrollOutcome::AtOldest);
    }

    /// Branch (B)'s bytes, in both encodings a child can select. Getting these
    /// wrong makes the child act on the wrong row (or read them as typed
    /// input), which is worse than not scrolling, so the exact sequences are
    /// pinned.
    #[test]
    fn a_forwarded_wheel_encodes_the_way_the_child_asked_for() {
        // SGR (`?1006h`), which is what a real claude session selects: wheel
        // up is button 64, wheel down 65, and a wheel event is a press (`M`).
        assert_eq!(
            mouse_report_bytes(
                vt100::MouseProtocolEncoding::Sgr,
                MOUSE_WHEEL_UP,
                7,
                3,
                true
            ),
            b"\x1b[<64;7;3M".to_vec()
        );
        assert_eq!(
            mouse_report_bytes(
                vt100::MouseProtocolEncoding::Sgr,
                MOUSE_WHEEL_DOWN,
                7,
                3,
                true
            ),
            b"\x1b[<65;7;3M".to_vec()
        );
        // A release only differs in the final byte -- SGR is the encoding that
        // can still say *which* button came up.
        assert_eq!(
            mouse_report_bytes(vt100::MouseProtocolEncoding::Sgr, 0, 1, 1, false),
            b"\x1b[<0;1;1m".to_vec()
        );
        assert_eq!(
            mouse_report_bytes(vt100::MouseProtocolEncoding::Sgr, 2, 4, 9, true),
            b"\x1b[<2;4;9M".to_vec()
        );
        // The classic form cannot, so a release there is the protocol's
        // "some button came up" code (3), whichever button it was.
        assert_eq!(
            mouse_report_bytes(vt100::MouseProtocolEncoding::Default, 2, 1, 1, false),
            vec![0x1b, b'[', b'M', 3 + 32, 33, 33]
        );
        // SGR is not limited to a byte, which is the whole reason it exists.
        assert_eq!(
            mouse_report_bytes(
                vt100::MouseProtocolEncoding::Sgr,
                MOUSE_WHEEL_UP,
                400,
                90,
                true
            ),
            b"\x1b[<64;400;90M".to_vec()
        );

        // The classic X10 form: `ESC [ M` then three bytes, each offset by 32.
        assert_eq!(
            mouse_report_bytes(
                vt100::MouseProtocolEncoding::Default,
                MOUSE_WHEEL_UP,
                7,
                3,
                true
            ),
            vec![0x1b, b'[', b'M', 64 + 32, 7 + 32, 3 + 32]
        );
        assert_eq!(
            mouse_report_bytes(
                vt100::MouseProtocolEncoding::Default,
                MOUSE_WHEEL_DOWN,
                1,
                1,
                true
            ),
            vec![0x1b, b'[', b'M', 65 + 32, 33, 33]
        );
        // A coordinate the single byte cannot express clamps instead of
        // wrapping round to the top-left corner.
        assert_eq!(
            mouse_report_bytes(
                vt100::MouseProtocolEncoding::Default,
                MOUSE_WHEEL_UP,
                400,
                3,
                true
            ),
            vec![0x1b, b'[', b'M', 96, (MOUSE_X10_MAX + 32) as u8, 35]
        );
        // The UTF-8 variant writes the same numbers as code points.
        assert_eq!(
            mouse_report_bytes(
                vt100::MouseProtocolEncoding::Utf8,
                MOUSE_WHEEL_UP,
                200,
                3,
                true
            ),
            {
                let mut want = b"\x1b[M".to_vec();
                want.push(96);
                want.extend_from_slice("\u{e8}".as_bytes());
                want.push(35);
                want
            }
        );
    }

    /// The encoding is the child's own, read off the parser rather than
    /// assumed: `?1006h` selects SGR, `?1006l` puts it back, and a harness
    /// that never asks for mouse reporting at all must not be sent events.
    #[test]
    fn the_mouse_protocol_is_read_from_the_child_not_assumed() {
        let mut parser = vt100::Parser::new(3, 20, SCROLLBACK_ROWS);
        assert_eq!(
            parser.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None,
            "a child that has asked for nothing gets nothing"
        );
        assert_eq!(
            parser.screen().mouse_protocol_encoding(),
            vt100::MouseProtocolEncoding::Default
        );

        // `?1000h` is VT200 press/release tracking in vt100's own mapping
        // (`?9h` is the X10 press-only mode).
        parser.process(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(
            parser.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::PressRelease
        );
        assert_eq!(
            parser.screen().mouse_protocol_encoding(),
            vt100::MouseProtocolEncoding::Sgr
        );

        parser.process(b"\x1b[?1006l\x1b[?1000l");
        assert_eq!(
            parser.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None
        );
        assert_eq!(
            parser.screen().mouse_protocol_encoding(),
            vt100::MouseProtocolEncoding::Default
        );
    }

    /// Return-to-live is the operator's own typing, and only that: output the
    /// child produces while the operator is reading history must leave the
    /// viewport where they put it (vt100 pins a non-zero offset to its row as
    /// rows retire past it), and so must an idle-gated injection.
    #[test]
    fn new_output_does_not_yank_a_scrolled_back_view() {
        let mut parser = vt100::Parser::new(3, 20, SCROLLBACK_ROWS);
        for line in 0..10 {
            parser.process(format!("line{line}\r\n").as_bytes());
        }
        parser.screen_mut().set_scrollback(3);
        let pinned = last_line_of(parser.screen());

        parser.process(b"fresh output\r\n");
        assert_eq!(
            last_line_of(parser.screen()),
            pinned,
            "the scrolled-back view stays on the same text as the child keeps printing"
        );
        assert!(
            parser.screen().scrollback() > 3,
            "the offset grew with the history so the view could stay put"
        );
    }

    #[test]
    fn last_line_returns_bottom_most_non_blank_row() {
        let mut parser = vt100::Parser::new(4, 10, 0);
        parser.process(b"hello\r\nworld\r\n");
        assert_eq!(last_line_of(parser.screen()), "world");
    }

    #[test]
    fn last_line_is_empty_on_a_blank_screen() {
        let parser = vt100::Parser::new(4, 10, 0);
        assert_eq!(last_line_of(parser.screen()), "");
    }

    /// Pure: the exact text a visible injection writes, matching
    /// `announce.rs`'s own `zirv ▸` marker.
    #[test]
    fn visible_injection_line_matches_the_zirv_announce_format() {
        assert_eq!(
            visible_injection_line("nudge from operator", "hello"),
            "[zirv \u{25b8} nudge from operator] hello"
        );
    }

    /// R4: the line carries no control characters at all. A leading `\r\n`
    /// used to submit whatever the operator had half-typed at the prompt
    /// before the injected text was ever entered; the lone trailing `\r`
    /// `inject_visible` adds is the only submission in the whole framing,
    /// exactly as in `wrap::inject_compact`.
    #[test]
    fn visible_injection_line_submits_nothing_of_its_own() {
        let line = visible_injection_line("mail from claude/aaaa1111", "check the build");
        assert!(
            !line.contains('\r') && !line.contains('\n'),
            "no control characters may frame the line: {line:?}"
        );
    }

    /// R3: every control character in an untrusted body becomes one space,
    /// and a run of them becomes one space, not several.
    #[test]
    fn body_for_injection_scrubs_every_control_character() {
        assert_eq!(
            body_for_injection("first\r\nsecond", 4096),
            "first second",
            "an interior CRLF must not survive to submit the message halfway"
        );
        assert_eq!(body_for_injection("a\rb", 4096), "a b");
        assert_eq!(
            body_for_injection("a\u{1b}[31mred\u{7f}", 4096),
            "a [31mred ",
            "ESC and DEL are text to be quoted, never bytes for the child TUI"
        );
        assert_eq!(
            body_for_injection("a\r\n\r\n\tb", 4096),
            "a b",
            "a run of control characters collapses to a single space"
        );
        assert_eq!(
            body_for_injection("plain text", 4096),
            "plain text",
            "an ordinary body is passed through untouched"
        );
    }

    /// R3: the delivered-mail cap (`cfg.mail.max_delivered_bytes`) applies at
    /// this seam too, and cutting never splits a char.
    #[test]
    fn body_for_injection_truncates_at_the_cap_on_a_char_boundary() {
        let long = "x".repeat(100);
        let got = body_for_injection(&long, 10);
        assert_eq!(got, format!("{}{TRUNCATION_MARKER}", "x".repeat(10)));

        // 'é' is two bytes: a cap landing inside it drops the whole char.
        let got = body_for_injection("aé", 2);
        assert_eq!(got, format!("a{TRUNCATION_MARKER}"));

        assert_eq!(
            body_for_injection("short", 5),
            "short",
            "a body exactly at the cap is not marked truncated"
        );
    }

    /// F7 (review, PR #116): the byte-level invariant that used to be proved
    /// against `injection_bytes` -- a production-dead function that
    /// duplicated `write_injection_phase1`/`write_submit_cr`'s own logic --
    /// is now proved directly against the two real functions the shipped
    /// path calls, via the same `RecordingWriter` seam every other test in
    /// this section uses. Whatever control characters an untrusted body
    /// carries, the bytes that land in the pty across both writes contain
    /// exactly one -- the trailing `\r` that submits the line. Anything else
    /// would be a second submission (an interior `\r`) or an escape sequence
    /// typed at the child.
    #[test]
    fn an_injection_writes_exactly_one_control_byte() {
        let mut writer = RecordingWriter { chunks: Vec::new() };
        write_injection_phase1(
            &mut writer,
            "mail from claude\r/aaaa1111",
            "line one\r\nline two\u{1b}[2Jline three\u{7f}",
        )
        .expect("phase 1 write must succeed against an in-memory sink");
        write_submit_cr(&mut writer).expect("phase 2 write must succeed");

        let bytes: Vec<u8> = writer.chunks.iter().flatten().copied().collect();
        let controls: Vec<usize> = bytes
            .iter()
            .enumerate()
            .filter(|(_, b)| **b < 0x20 || **b == 0x7f)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            controls.len(),
            1,
            "exactly one control byte may reach the pty: {:?}",
            String::from_utf8_lossy(&bytes)
        );
        assert_eq!(controls[0], bytes.len() - 1, "and it is the last byte");
        assert_eq!(bytes[bytes.len() - 1], b'\r', "and it is the submission");
    }

    /// Records each `write_all` call as its own chunk (this impl always
    /// accepts the whole buffer in one `write` call, so one `write_all`
    /// produces exactly one chunk here), so a test can assert the two-write
    /// shape issue #114 requires without a real pty.
    struct RecordingWriter {
        chunks: Vec<Vec<u8>>,
    }

    impl Write for RecordingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.chunks.push(buf.to_vec());
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Issue #114 / review F1/F2 (PR #116): phase 1 writes only the labelled
    /// line, with no control bytes of its own -- the settle gap before the
    /// submitting `\r` is no longer enforced by a sleep inside this write at
    /// all (see [`INJECTION_SUBMIT_DELAY`]'s own doc comment); it is a
    /// deadline the caller (`Pane::inject_visible`/`Pane::pending_submit`)
    /// schedules and the dashboard's tick loop later drains.
    #[test]
    fn write_injection_phase1_writes_only_the_labelled_line() {
        let mut writer = RecordingWriter { chunks: Vec::new() };

        write_injection_phase1(&mut writer, "nudge from operator", "hello")
            .expect("write must succeed against an in-memory sink");

        assert_eq!(
            writer.chunks.len(),
            1,
            "phase 1 is exactly one write: {:?}",
            writer.chunks
        );
        assert_eq!(
            String::from_utf8_lossy(&writer.chunks[0]),
            "[zirv \u{25b8} nudge from operator] hello",
            "the write is the visible line, with nothing appended"
        );
        assert!(
            !writer.chunks[0].iter().any(|b| *b < 0x20 || *b == 0x7f),
            "phase 1 carries no control bytes of its own: {:?}",
            String::from_utf8_lossy(&writer.chunks[0])
        );
    }

    /// F4 (review, PR #116; issue #118): `write_submit_cr` is the one
    /// function both `dash::pane`'s deferred injections and `wrap`'s own
    /// T13 mail-advisory injection into a `defer_injection_submit` adapter
    /// call for phase 2 -- a due pane's submission is always exactly this
    /// one byte. (`wrap::inject_compact`/`Action::Compact` stays
    /// single-burst; that call site is only ever reachable for claude, see
    /// its own doc comment.)
    #[test]
    fn write_submit_cr_writes_exactly_one_byte() {
        let mut writer = RecordingWriter { chunks: Vec::new() };
        write_submit_cr(&mut writer).expect("write must succeed against an in-memory sink");
        assert_eq!(writer.chunks, vec![b"\r".to_vec()]);
    }

    /// Pure: `submit_is_due` is what `Pane::pending_submit_due` delegates to,
    /// so its three cases are testable without a real clock race -- only
    /// `Instant::now()` plus/minus a `Duration`.
    #[test]
    fn submit_is_due_true_only_once_the_deadline_has_passed() {
        let now = Instant::now();
        assert!(
            !submit_is_due(Some(now + Duration::from_millis(10)), now),
            "not yet due"
        );
        assert!(submit_is_due(Some(now), now), "due exactly at the deadline");
        assert!(
            submit_is_due(Some(now - Duration::from_millis(1)), now),
            "still due once the deadline has passed"
        );
        assert!(!submit_is_due(None, now), "nothing pending is never due");
    }

    /// A writer whose Nth `write` call fails, so a phase-2 retry can be
    /// exercised without a real pty. Every other call (before and after the
    /// failure) records its chunk exactly like `RecordingWriter`.
    struct FlakyWriter {
        calls: usize,
        fail_at: usize,
        chunks: Vec<Vec<u8>>,
    }

    impl Write for FlakyWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.calls += 1;
            if self.calls == self.fail_at {
                return Err(std::io::Error::other("simulated write failure"));
            }
            self.chunks.push(buf.to_vec());
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// F1/F2 (review, PR #116): a phase-2 write failure must retry as a lone
    /// `\r` on the next attempt -- never by re-sending phase 1, which would
    /// type a second copy of the injected line onto the still-unsubmitted
    /// first (the exact bug the redesign exists to close). This is provable
    /// structurally at the function level: `write_submit_cr` never touches
    /// phase 1's text at all, so a retry that only ever calls
    /// `write_submit_cr` again cannot duplicate the line, whatever `Pane`
    /// state wraps it (`Pane::submit_pending` only clears `pending_submit`
    /// after this call returns `Ok`, so a failure here leaves it set for
    /// exactly this retry).
    #[test]
    fn a_failed_submit_cr_write_is_safely_retryable_without_resending_the_line() {
        let mut writer = FlakyWriter {
            calls: 0,
            fail_at: 1,
            chunks: Vec::new(),
        };

        write_submit_cr(&mut writer).expect_err("the first write call fails");
        assert!(
            writer.chunks.is_empty(),
            "a failed write leaves nothing recorded: {:?}",
            writer.chunks
        );

        // Retry: no further failures scheduled.
        writer.fail_at = 0;
        write_submit_cr(&mut writer).expect("the retry succeeds");
        assert_eq!(
            writer.chunks,
            vec![b"\r".to_vec()],
            "the retry writes exactly one lone CR -- never the line again"
        );
    }

    /// A trivial, immediately-exiting command: never a real agent (the
    /// ABSOLUTE rule this plan spells out), just enough of a child for
    /// `Pane::spawn` to have something real to supervise. Mirrors the
    /// platform split `wrap.rs`'s own pty tests already use (`cmd /c` on
    /// Windows, `sh -c` on unix) rather than depending on either being on
    /// the other platform's `PATH`.
    #[cfg(windows)]
    fn trivial_argv() -> Vec<String> {
        vec![
            "cmd".to_string(),
            "/c".to_string(),
            "exit".to_string(),
            "0".to_string(),
        ]
    }

    #[cfg(unix)]
    fn trivial_argv() -> Vec<String> {
        vec!["sh".to_string(), "-c".to_string(), "exit 0".to_string()]
    }

    /// A trivial child that stays alive well past any of these tests' own
    /// deadlines, and reaps itself if the test somehow never shuts it down.
    /// Same never-a-real-agent rule and same platform split as `trivial_argv`;
    /// `ping -n N 127.0.0.1` is already this codebase's own long-lived
    /// Windows test child (`wrap.rs`'s turn-signal transport test).
    #[cfg(windows)]
    pub(crate) fn long_lived_argv() -> Vec<String> {
        vec![
            "cmd".to_string(),
            "/c".to_string(),
            "ping -n 60 127.0.0.1".to_string(),
        ]
    }

    #[cfg(unix)]
    pub(crate) fn long_lived_argv() -> Vec<String> {
        vec!["sh".to_string(), "-c".to_string(), "sleep 60".to_string()]
    }

    /// Task A: a child that prints exactly one line at startup -- standing in
    /// for a real harness drawing its first frame -- and then produces
    /// nothing further for the rest of its (long) life. Lets a test observe a
    /// signal-less pane's quiet window closing against a real, deterministic
    /// last-output timestamp rather than racing a harness that might repaint
    /// on its own.
    #[cfg(windows)]
    pub(crate) fn silent_after_first_line_argv() -> Vec<String> {
        vec![
            "cmd".to_string(),
            "/c".to_string(),
            "echo hello & ping -n 60 127.0.0.1 >nul".to_string(),
        ]
    }

    #[cfg(unix)]
    pub(crate) fn silent_after_first_line_argv() -> Vec<String> {
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo hello; sleep 60".to_string(),
        ]
    }

    /// Drives one turn signal into `pane`'s own socket and waits, bounded, for
    /// the pane to report `Idle`. Returns whether it got there. Gives up
    /// immediately if the child exited (`Ended` outranks every other state, so
    /// no number of signals would ever move it back to `Idle`).
    ///
    /// Two phases, because of F1: idleness is now "quiet for a debounce",
    /// measured from the last output or, with none recorded, from the signal
    /// itself. So the retry loop stops sending the moment a signal has been
    /// observed -- each further signal would restart the quiet window and this
    /// helper would spin until its own deadline.
    pub(crate) fn signal_until_idle(pane: &mut Pane, state: &StateDir, session_id: &str) -> bool {
        let socket = state.socket_for(session_id);
        let signal = crate::commands::ctx::signal::TurnSignal {
            session_id: session_id.to_string(),
            turn: 1,
            score: 0,
            verdict: crate::commands::ctx::rot::Verdict::Healthy,
            transcript_path: None,
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let before = pane.last_signal_at;

        // Phase 1: land exactly one signal, retrying until the pane observes
        // a newer one than it already had.
        while std::time::Instant::now() < deadline {
            pane.on_turn_signal();
            if matches!(pane.state(), PaneState::Ended(_)) {
                return false;
            }
            if pane.last_signal_at != before {
                break;
            }
            let _ = crate::commands::ctx::signal::send(&socket, &signal);
            std::thread::sleep(Duration::from_millis(50));
        }

        // Phase 2: wait out the debounce with nothing further sent.
        while std::time::Instant::now() < deadline {
            pane.on_turn_signal();
            match pane.state() {
                PaneState::Idle => return true,
                PaneState::Ended(_) => return false,
                _ => {}
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn test_spec(session_id: &str) -> PaneSpec {
        PaneSpec {
            agent_name: "test-agent".to_string(),
            argv: trivial_argv(),
            role: PromptRole::Worker,
            verb: Verb::Dash,
            session_id: session_id.to_string(),
            title: "wrk test".to_string(),
        }
    }

    #[test]
    fn failed_successor_spawn_preserves_the_existing_seat_and_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let old = "11111111-2222-4333-8444-555555555555";
        let _guard =
            SessionGuard::register(&state, Record::new(old, "test-agent", &repo, Verb::Dash));
        let _server = SignalServer::bind(&state.socket_for(old)).unwrap();
        let record = state.sessions().join("11111111.json");
        let before = std::fs::read(&record).unwrap();
        for successor in [old, "aaaaaaaa-2222-4333-8444-555555555555"] {
            let mut spec = test_spec(successor);
            spec.argv = vec![
                tmp.path()
                    .join("missing-agent")
                    .to_string_lossy()
                    .to_string(),
            ];
            assert!(
                Pane::spawn_on_seat(
                    spec,
                    &state,
                    &repo,
                    &repo,
                    (80, 24),
                    &[],
                    false,
                    DEFAULT_IDLE_QUIET,
                    Some("11111111")
                )
                .is_err()
            );
            assert_eq!(std::fs::read(&record).unwrap(), before);
            assert!(state.socket_for(old).exists());
        }
    }

    #[test]
    fn spawn_drain_and_shutdown_round_trip_on_a_real_child() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let mut spec = test_spec("11111111-2222-4333-8444-555555555555");
        // Keep the child alive for the write below. The default test child
        // exits immediately, so whether the pty accepts an injection before
        // observing EOF is scheduler-dependent (and reliably returns EIO on
        // macOS once the child wins that race).
        spec.argv = long_lived_argv();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        assert_eq!(pane.agent(), "test-agent");
        assert_eq!(pane.title(), "wrk test");
        assert!(!pane.short().is_empty());
        assert_eq!(pane.verb(), Verb::Dash);

        // Smoke test: a live pane's writer accepts a visible injection
        // (content correctness is covered separately by
        // `visible_injection_line_matches_the_zirv_announce_format`, which
        // does not need a real child at all).
        pane.inject_visible("nudge from operator", "hello")
            .expect("inject_visible must succeed while the child is alive");

        pane.drain();
        assert!(
            !matches!(pane.state(), PaneState::Ended(_)),
            "the long-lived child must still be alive after draining"
        );

        pane.finish_shutdown().expect("first shutdown");
        pane.finish_shutdown().expect("shutdown must be idempotent");
    }

    /// Code review (issue #119, round 2), BLOCKER: a worktree-hosted pane's
    /// child runs at `cwd` (the worktree), but its registry `Record` -- and
    /// therefore `repo_slug`, and therefore which mailbox `mail_sweep`/
    /// `zirv ctx nudge --to-session` reads for it -- must stay keyed off the
    /// dashboard's own `repo`, never the worktree it happens to run in. Two
    /// distinct paths prove the split actually reached `Record::new`
    /// (`sessions::resolve_prefix`, the real lookup path `zirv ctx nudge`
    /// itself uses) rather than only the two `Pane::spawn` parameters being
    /// accepted syntactically.
    #[test]
    fn a_pane_spawned_at_a_different_cwd_keeps_the_dashboard_repo_in_its_record() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let dashboard_repo = tmp.path().join("dashboard-repo");
        let worktree_cwd = tmp.path().join("linked-worktree");
        std::fs::create_dir_all(&dashboard_repo).expect("mkdir dashboard-repo");
        std::fs::create_dir_all(&worktree_cwd).expect("mkdir linked-worktree");
        assert_ne!(
            dashboard_repo, worktree_cwd,
            "the two paths must actually differ for this test to mean anything"
        );

        let mut spec = test_spec("33333333-2222-4333-8444-555555555555");
        // Long-lived, not `trivial_argv()`: `sessions::resolve_prefix` below
        // only returns a `Liveness::Live` record, and a process that has
        // already exited by the time this test gets to it would make the
        // lookup racy rather than proving anything about `Record::repo`.
        spec.argv = long_lived_argv();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &worktree_cwd,
            &dashboard_repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        let record = sessions::resolve_prefix(&state, pane.short())
            .expect("the freshly spawned pane must be live and resolvable");
        assert_eq!(
            record.repo, dashboard_repo,
            "Record::repo must be the dashboard's own repo, not the pane's cwd"
        );
        assert_eq!(
            record.repo_slug,
            super::super::super::state::repo_slug(&dashboard_repo),
            "repo_slug (the mailbox key mail_sweep/nudge --to-session actually use) must \
             follow Record::repo"
        );

        pane.shutdown("").expect("shutdown");
    }

    /// Finding #2: a failure in the *successor's* setup (here, a missing
    /// adapter binary -- `resolve_program` does not check existence, so
    /// `ready()`/`resolve_swap_launch` succeed and the OS spawn itself is
    /// what fails) must leave the old pane running untouched, not dead and
    /// pinned to the dashboard's own pid. Before the fix, the old child was
    /// quit and its lifecycle released before this failure could even be
    /// observed.
    #[test]
    fn handover_failure_in_successor_setup_leaves_the_old_pane_untouched() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "22222222-2222-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = long_lived_argv();
        spec.agent_name = "claude".to_string();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        let old_agent_name = pane.agent().to_string();
        assert!(
            !matches!(pane.state(), PaneState::Ended(_)),
            "the long-lived child must still be alive before the failing handover"
        );

        let cfg = crate::commands::ctx::config::CtxConfig {
            agent_bin: Some(
                tmp.path()
                    .join("no-such-adapter-binary")
                    .display()
                    .to_string(),
            ),
            ..Default::default()
        };
        let req = crate::commands::ctx::handover::HandoverRequest {
            target_agent: "claude".to_string(),
            target_model: None,
            force: true,
            requested_at: 0,
            interactive: false,
            automatic: false,
            generation: None,
            structural_only: false,
            resume_session: None,
            target_runtime: None,
            target_route: None,
        };
        let handoff_note = crate::commands::ctx::handoff::Handoff::default();

        let err = pane
            .handover(
                &cfg,
                &req,
                &handoff_note,
                PromptRole::Worker,
                &repo,
                (80, 24),
            )
            .expect_err("a missing adapter binary must fail the swap, not silently succeed");
        assert!(
            !err.to_string().is_empty(),
            "must report why the swap failed"
        );

        assert_eq!(
            pane.agent(),
            old_agent_name,
            "the old pane's identity must be unchanged after a failed swap"
        );
        assert!(
            !matches!(pane.state(), PaneState::Ended(_)),
            "the old child must still be running -- it must never have been quit"
        );

        // Test-plumbing only: this child (`long_lived_argv`) never reads its
        // pty input, so `shutdown`'s polite ask-then-wait always burns the
        // full `QUIT_GRACE` (production, unchanged) before falling through to
        // the same kill. `finish_shutdown` is the escalation half on its own
        // -- already public, already used by the batched-shutdown path -- so
        // teardown here is immediate instead of a real multi-second wait.
        pane.finish_shutdown().expect("shutdown");
    }

    /// Review round 2 (S2): the rollout floor a handover records must be the
    /// instant BEFORE the successor was spawned, not after. `resolve_rollout`
    /// drops every rollout whose `session_meta` predates the floor, codex
    /// writes that meta the moment it starts, and the swap spends up to
    /// `QUIT_GRACE` retiring the old child between the two -- so a floor
    /// captured at the end of `handover` excludes the successor's own rollout
    /// and `pinned_rollout` answers `None` for the rest of the pane's life.
    /// The old child here never reads its pty, so the quit really does burn
    /// the full grace: the window the bug lived in is genuinely open.
    #[test]
    fn a_pane_handover_floors_the_rollout_pin_before_the_successor_spawns() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        // Root cause (not timing): `agent_name` is "claude" below, so
        // `Pane::spawn`'s MCP autoregistration silently appends
        // `--mcp-config=...`/`--allowedTools=...` onto the stand-in `cmd /c
        // ping` argv, which `ping`/`cmd` then reject immediately -- the old
        // child dies in well under a second instead of surviving the swap's
        // real `QUIT_GRACE`. Opt out for this test.
        let _guard = crate::commands::ctx::testenv::VarGuard::set(&[(
            "ZIRV_CTX_MCP_AUTOREGISTER",
            Some("0"),
        )]);

        let session_id = "33333333-2222-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = long_lived_argv();
        spec.agent_name = "claude".to_string();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");
        let short = pane.short().to_string();

        // A successor that spawns (so the swap commits and the floor is
        // written) without needing to survive: only the ORDER is under test.
        #[cfg(windows)]
        let successor_bin = "ping";
        #[cfg(not(windows))]
        let successor_bin = "sleep";
        let cfg = crate::commands::ctx::config::CtxConfig {
            agent_bin: Some(successor_bin.to_string()),
            ..Default::default()
        };
        let req = crate::commands::ctx::handover::HandoverRequest {
            target_agent: "claude".to_string(),
            target_model: None,
            force: true,
            requested_at: 0,
            interactive: false,
            automatic: false,
            generation: None,
            structural_only: false,
            resume_session: None,
            target_runtime: None,
            target_route: None,
        };
        let handoff_note = crate::commands::ctx::handoff::Handoff::default();

        let before = crate::commands::ctx::state::now_secs();
        pane.handover(
            &cfg,
            &req,
            &handoff_note,
            PromptRole::Worker,
            &repo,
            (80, 24),
        )
        .expect("handover succeeds");
        let after = crate::commands::ctx::state::now_secs();

        assert!(
            after.saturating_sub(before) >= 2,
            "test premise: the swap must actually span the quit grace, else an end-of-handover \
             floor would be indistinguishable from a pre-spawn one (took {}s)",
            after.saturating_sub(before)
        );
        let floor_ms: u64 =
            std::fs::read_to_string(state.rollouts().join(format!("{short}.floor")))
                .expect("the swap records a handover floor")
                .trim()
                .parse()
                .expect("the floor is epoch milliseconds");
        assert!(
            floor_ms <= before.saturating_add(1).saturating_mul(1_000),
            "the floor must be captured before the successor spawns: floor {floor_ms}ms vs the \
             pre-spawn instant {before}s (swap ended at {after}s)"
        );

        // Test-plumbing only, as in the sibling handover tests above: the old
        // child never read its pty, and the successor may already be gone.
        pane.finish_shutdown().expect("shutdown");
    }

    /// A3-1: `shutdown` used to set `done` BEFORE the fallible work, so a
    /// failure (here a poisoned writer lock) skipped `lifecycle.release`,
    /// `unpublish_socket_path`, `rollover::forget` and `guard.release`
    /// permanently AND made `finish_shutdown` -- the escalation half, guarded
    /// by the same `done` -- a silent no-op. A pane whose polite quit failed
    /// is exactly the pane that still needs escalating.
    #[test]
    fn a_shutdown_that_failed_before_its_cleanup_stays_escalatable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let mut spec = test_spec("44444444-2222-4333-8444-555555555555");
        spec.argv = long_lived_argv();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        let writer = Arc::clone(pane.writer().expect("a wrapped pane has a writer"));
        let _ = std::thread::spawn(move || {
            let _held = writer.lock().expect("lock");
            panic!("poison the pane writer");
        })
        .join();
        assert!(
            pane.writer()
                .expect("a wrapped pane has a writer")
                .is_poisoned(),
            "sanity: the writer lock is poisoned"
        );

        let socket_file = state
            .root()
            .join(wrap::socket_path_file_for(&pane.session_id));
        assert!(
            socket_file.exists(),
            "sanity: the socket path was published"
        );

        pane.shutdown("")
            .expect_err("a poisoned writer must fail the polite quit");
        assert!(
            !pane.done,
            "a shutdown that never reached its cleanup must not mark the pane done"
        );

        pane.finish_shutdown()
            .expect("the escalation half must still run");
        assert!(pane.done, "the escalation half completed the shutdown");
        assert!(
            !socket_file.exists(),
            "and ran the cleanup the failed shutdown skipped"
        );
    }

    /// A3-2: `handover` resets every other piece of per-child state but left
    /// `budget_soft_warned`/`budget_grace_given` latched from the
    /// PREDECESSOR, so the successor -- a brand new child with its own fresh
    /// transcript -- silently got no soft warning and no HardStop grace tick.
    /// `set_budget_tokens` has always reset both; a swap is the same kind of
    /// event.
    #[cfg(unix)]
    #[test]
    fn a_handover_re_arms_the_successors_budget_warnings() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let mut spec = test_spec("55555555-2222-4333-8444-555555555555");
        spec.argv = long_lived_argv();
        spec.agent_name = "claude".to_string();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");
        pane.set_budget_tokens(Some(1_000));

        let usage = crate::commands::ctx::event::TranscriptUsage {
            input_tokens: 900,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            output_tokens: 0,
        };
        assert!(
            matches!(
                pane.enforce_token_budget(&usage, "").expect("enforce"),
                Some(PaneBudgetNotice::SoftWarn { .. })
            ),
            "the predecessor warns once"
        );
        assert!(
            pane.enforce_token_budget(&usage, "")
                .expect("enforce")
                .is_none(),
            "and never repeats itself for the same child"
        );

        let cfg = crate::commands::ctx::config::CtxConfig {
            agent_bin: Some("sleep 5".to_string()),
            ..Default::default()
        };
        let req = crate::commands::ctx::handover::HandoverRequest {
            target_agent: "claude".to_string(),
            target_model: None,
            force: true,
            requested_at: 0,
            interactive: false,
            automatic: false,
            generation: None,
            structural_only: false,
            resume_session: None,
            target_runtime: None,
            target_route: None,
        };
        pane.handover(
            &cfg,
            &req,
            &crate::commands::ctx::handoff::Handoff::default(),
            PromptRole::Worker,
            &repo,
            (80, 24),
        )
        .expect("handover succeeds");

        assert!(
            matches!(
                pane.enforce_token_budget(&usage, "").expect("enforce"),
                Some(PaneBudgetNotice::SoftWarn { .. })
            ),
            "the successor is a new child and must get its own soft warning"
        );

        pane.finish_shutdown().expect("shutdown");
    }

    /// 2026-09-06: a pane-bound delegation carrying `--timeout-secs` used to
    /// hard-error rather than spawn a pane. It spawns one now, and this is
    /// the ceiling being real: the child is stopped with the same
    /// `exec::EXIT_TIMEOUT` an inline supervised run reports, exactly once.
    #[cfg(unix)]
    #[test]
    fn an_armed_deadline_stops_the_pane_once_with_the_timeout_exit_code() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let mut spec = test_spec("66666666-2222-4333-8444-666666666666");
        spec.argv = long_lived_argv();
        spec.agent_name = "claude".to_string();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        let started = Instant::now();
        pane.set_timeout(started, None);
        assert!(pane.deadline().is_none(), "no ceiling leaves it unbounded");
        assert!(
            !pane.enforce_deadline(started, "").expect("enforce"),
            "an unbounded pane is never stopped"
        );

        pane.set_timeout(started, Some(60));
        assert!(pane.deadline().is_some());
        assert!(
            !pane.enforce_deadline(started, "").expect("enforce"),
            "the wall clock has not run out yet"
        );

        assert!(
            pane.enforce_deadline(started + Duration::from_secs(61), "")
                .expect("enforce"),
            "past the deadline the pane is stopped"
        );
        assert!(
            matches!(
                pane.state(),
                PaneState::Ended(code) if code == crate::commands::ctx::exec::EXIT_TIMEOUT
            ),
            "the pane reports the same exit code an inline supervised timeout does: {:?}",
            pane.state()
        );
        assert!(
            pane.deadline().is_none(),
            "and the deadline is disarmed, so the sweep never reports it twice"
        );
        assert!(
            !pane
                .enforce_deadline(started + Duration::from_secs(120), "")
                .expect("enforce"),
            "a second sweep says nothing"
        );

        pane.finish_shutdown().expect("shutdown");
    }

    /// R1-1: `timeout_secs` is untrusted JSON. `u64::MAX` used to reach
    /// `started + Duration::from_secs(secs)`, whose `Instant` overflow panics
    /// -- and the release profile is `panic = "abort"`, so one forged request
    /// took the whole dashboard down with every pane it was hosting. Pure, so
    /// the arithmetic is pinned without a pty.
    #[test]
    fn an_unrepresentable_timeout_is_clamped_instead_of_panicking() {
        let started = Instant::now();
        assert_eq!(
            deadline_for(started, u64::MAX),
            started.checked_add(Duration::from_secs(MAX_TIMEOUT_SECS)),
            "a forged ceiling is clamped to the pane ceiling, never added raw"
        );
        assert_eq!(
            deadline_for(started, 900),
            started.checked_add(Duration::from_secs(900)),
            "an honest ceiling is exactly the ceiling that was asked for"
        );
        assert_eq!(deadline_for(started, 0), Some(started));
    }

    /// The same value through a real pane: arming it must neither panic nor
    /// leave the pane immediately overdue.
    #[test]
    fn a_pane_armed_with_a_forged_timeout_is_not_instantly_overdue() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let mut spec = test_spec("77777777-2222-4333-8444-777777777777");
        spec.argv = long_lived_argv();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        let started = Instant::now();
        pane.set_timeout(started, Some(u64::MAX));
        assert_eq!(
            pane.deadline(),
            started.checked_add(Duration::from_secs(MAX_TIMEOUT_SECS)),
            "the clamped ceiling is what the pane actually arms"
        );
        assert!(
            !pane.enforce_deadline(started, "").expect("enforce"),
            "and it is not already overdue"
        );

        pane.finish_shutdown().expect("shutdown");
    }

    /// R1-5: `enforce_pane_deadlines` runs BEFORE `reap_ended_panes`, so a
    /// child that finished cleanly a moment before its deadline is still in
    /// `panes` when the sweep arrives. Its observed `0` used to be rewritten
    /// to `EXIT_TIMEOUT`, reporting a successful worker as timed out.
    #[test]
    fn a_child_that_already_exited_keeps_its_own_exit_code_through_a_deadline_sweep() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        // `trivial_argv` exits 0 immediately -- exactly the race this is
        // about, with the exit already observed when the sweep runs.
        let mut pane = Pane::spawn(
            test_spec("55555555-2222-4333-8444-999999999999"),
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        let waited = std::time::Instant::now() + Duration::from_secs(30);
        while !pane.try_exited() && std::time::Instant::now() < waited {
            let _ = pane.drain();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            matches!(pane.state(), PaneState::Ended(0)),
            "sanity: the child exited cleanly first: {:?}",
            pane.state()
        );

        let started = Instant::now();
        pane.set_timeout(started, Some(1));
        assert!(
            !pane
                .enforce_deadline(started + Duration::from_secs(2), "")
                .expect("enforce"),
            "a finished pane is not something a deadline sweep stops"
        );
        assert!(
            matches!(pane.state(), PaneState::Ended(0)),
            "and its own exit code survives the sweep: {:?}",
            pane.state()
        );
        assert!(
            pane.deadline().is_none(),
            "the deadline is disarmed either way, so no later sweep looks again"
        );

        pane.finish_shutdown().expect("shutdown");
    }

    /// R1-6: the deadline used to be disarmed before the polite quit ran, so
    /// a quit that failed (a poisoned writer mutex, as in `a_shutdown_that_
    /// failed_before_its_cleanup_stays_escalatable`) left a LIVE child behind
    /// a `None` deadline every later sweep skipped. The failed quit now
    /// escalates instead.
    #[test]
    fn a_deadline_whose_polite_quit_fails_still_terminates_the_child() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let mut spec = test_spec("99999999-2222-4333-8444-555555555555");
        spec.argv = long_lived_argv();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        let writer = Arc::clone(pane.writer().expect("a wrapped pane has a writer"));
        let _ = std::thread::spawn(move || {
            let _held = writer.lock().expect("lock");
            panic!("poison the pane writer");
        })
        .join();
        assert!(
            pane.writer()
                .expect("a wrapped pane has a writer")
                .is_poisoned(),
            "sanity: the writer lock is poisoned, so the polite quit must fail"
        );

        let started = Instant::now();
        pane.set_timeout(started, Some(1));
        assert!(
            pane.enforce_deadline(started + Duration::from_secs(2), "")
                .expect("a failed polite quit must escalate, not propagate"),
            "the sweep reports the stop it actually performed"
        );
        assert!(
            pane.done,
            "the child is terminated by the escalation half, never left running"
        );
        assert!(
            matches!(
                pane.state(),
                PaneState::Ended(code) if code == crate::commands::ctx::exec::EXIT_TIMEOUT
            ),
            "and it reports the timeout exit: {:?}",
            pane.state()
        );
        assert!(pane.deadline().is_none(), "disarmed once it actually died");
    }

    /// Fix 3 (issue #249/#250 review): a handover's own successor child must
    /// carry `PARENT_SESSION_ENV` in its real process environment when this
    /// pane records a parent (`Pane::parent_session`) -- `handover::build_
    /// turn_env` has no knowledge of pane-level state and never adds it, so
    /// without the fix the dashboard's own sweep (reading `Pane::parent_
    /// session`, untouched by a handover) and a nested `zirv ctx` call
    /// inside the successor (reading its own real env) would disagree about
    /// the same mail's trust.
    #[cfg(unix)]
    #[test]
    fn handover_re_exports_the_panes_parent_session_to_the_successor_child() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "88888888-2222-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = long_lived_argv();
        spec.agent_name = "claude".to_string();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        // The fact under test: this pane's own recorded parent, the same way
        // `fulfill_spawn_request` would have set it from `verified_parent` at
        // first spawn.
        pane.set_parent_session(Some("grandpar".to_string()));

        let env_log = tmp.path().join("successor-parent-env.log");
        let script = tmp.path().join("log-parent-env.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s' \"${{ZIRV_CTX_PARENT_SESSION:-}}\" > \"{}\"\nsleep 3\n",
                env_log.display()
            ),
        )
        .expect("write script");

        let cfg = crate::commands::ctx::config::CtxConfig {
            agent_bin: Some(format!("sh {}", script.display())),
            ..Default::default()
        };
        let req = crate::commands::ctx::handover::HandoverRequest {
            target_agent: "claude".to_string(),
            target_model: None,
            force: true,
            requested_at: 0,
            interactive: false,
            automatic: false,
            generation: None,
            structural_only: false,
            resume_session: None,
            target_runtime: None,
            target_route: None,
        };
        let handoff_note = crate::commands::ctx::handoff::Handoff::default();

        pane.handover(
            &cfg,
            &req,
            &handoff_note,
            PromptRole::Worker,
            &repo,
            (80, 24),
        )
        .expect("handover succeeds");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !env_log.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let logged = std::fs::read_to_string(&env_log).unwrap_or_default();
        assert_eq!(
            logged.trim(),
            "grandpar",
            "the successor child's own real environment must carry the pane's own parent \
             session"
        );

        pane.finish_shutdown().expect("shutdown");
    }

    /// Finding #10 (issue #358 review): a successor spawned by `Pane::
    /// handover` must carry its own `ZIRV_CTX_SEAT_GENERATION` in its real
    /// process environment -- `handover::build_turn_env` never pushed it at
    /// all before this fix, which left every post-rollover successor
    /// permanently unfenced (`seat::fence` has nothing to compare against).
    /// This is a MANUAL swap (`generation: None` on the request, opening no
    /// seat transaction), so the successor must get the seat's CURRENT,
    /// unchanged on-disk generation -- `1`, for a seat this test freshly
    /// registers and never rolls over.
    #[cfg(unix)]
    #[test]
    fn handover_carries_the_seats_current_generation_for_a_manual_swap() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "99999999-2222-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = long_lived_argv();
        spec.agent_name = "claude".to_string();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        crate::commands::ctx::seat::register(
            &state,
            pane.short(),
            session_id,
            "claude",
            None,
            "anthropic",
            "orchestrator",
            false,
            1_700_000_000,
        )
        .expect("register seat");

        let env_log = tmp.path().join("successor-generation-env.log");
        let script = tmp.path().join("log-generation-env.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s' \"${{ZIRV_CTX_SEAT_GENERATION:-}}\" > \"{}\"\nsleep 3\n",
                env_log.display()
            ),
        )
        .expect("write script");

        let cfg = crate::commands::ctx::config::CtxConfig {
            agent_bin: Some(format!("sh {}", script.display())),
            ..Default::default()
        };
        let req = crate::commands::ctx::handover::HandoverRequest {
            target_agent: "claude".to_string(),
            target_model: None,
            force: true,
            requested_at: 0,
            interactive: false,
            automatic: false,
            generation: None,
            structural_only: false,
            resume_session: None,
            target_runtime: None,
            target_route: None,
        };
        let handoff_note = crate::commands::ctx::handoff::Handoff::default();

        pane.handover(
            &cfg,
            &req,
            &handoff_note,
            PromptRole::Orchestrator,
            &repo,
            (80, 24),
        )
        .expect("handover succeeds");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !env_log.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let logged = std::fs::read_to_string(&env_log).unwrap_or_default();
        assert_eq!(
            logged.trim(),
            "1",
            "a manual swap must carry the seat's current (unchanged) generation, not leave the \
             successor unfenced"
        );

        pane.finish_shutdown().expect("shutdown");
    }

    /// The other half of that rule: a RETURN to a harness that was parked
    /// while a DIFFERENT harness held the seat resumes that parked
    /// conversation AND carries the interim harness's handoff packet. The
    /// conversation is continuous (never a cold restart), but it missed the
    /// whole interim turn, so the packet is exactly what it lacks.
    #[test]
    fn a_return_to_a_parked_harness_resumes_it_and_carries_the_interim_handoff() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        // The pane is running the INTERIM harness; the seat is returning to
        // claude, whose own conversation id is not this pane's session.
        let session_id = "55555555-2222-4333-8444-555555555555";
        let parked_conversation = "49195b07-217f-4401-8681-c857fcea294e";
        let mut spec = test_spec(session_id);
        spec.argv = long_lived_argv();
        spec.agent_name = "codex".to_string();
        spec.role = PromptRole::Orchestrator;
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        // Issue #450: a relative filename here used to rely on the
        // successor's cwd being `repo`, but `detect_help_flag`'s own
        // `--append-system-prompt-file` probe spawns this same `agent_bin`
        // shim with no `current_dir` set at all, so a relative path landed
        // in the real process cwd (the checkout root) instead. An absolute
        // tempdir path, quoted, is immune to both which cwd a spawn actually
        // used and to a tempdir path containing spaces. The probe itself
        // exits unlogged, or its `--help` would land in the log first.
        let argv_log = tmp.path().join("return-argv.log");
        let script = tmp.path().join("log-argv.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n[ \"$1\" = --help ] && exit 0\nprintf '%s\\n' \"$@\" > \"{}\"\nsleep 3\n",
                argv_log.display()
            ),
        )
        .expect("write script");

        let cfg = crate::commands::ctx::config::CtxConfig {
            agent_bin: Some(format!("sh {}", script.display())),
            ..Default::default()
        };
        let req = crate::commands::ctx::handover::HandoverRequest {
            target_agent: "claude".to_string(),
            target_model: None,
            force: true,
            requested_at: 0,
            interactive: true,
            automatic: true,
            generation: None,
            structural_only: false,
            resume_session: Some(parked_conversation.to_string()),
            target_runtime: None,
            target_route: None,
        };

        pane.handover(
            &cfg,
            &req,
            &crate::commands::ctx::handoff::Handoff::default(),
            PromptRole::Orchestrator,
            &repo,
            (80, 24),
        )
        .expect("the parked harness returns");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !argv_log.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let logged = std::fs::read_to_string(&argv_log).unwrap_or_default();
        let args: Vec<&str> = logged.lines().collect();

        let at = args
            .iter()
            .position(|arg| *arg == "--resume")
            .unwrap_or_else(|| panic!("the parked conversation must be resumed: {args:?}"));
        assert_eq!(
            args.get(at + 1).copied(),
            Some(parked_conversation),
            "the resume flag names the PARKED conversation, not this pane's session: {args:?}"
        );
        assert!(
            args.iter().any(
                |arg| *arg == "--append-system-prompt-file" || *arg == "--append-system-prompt"
            ),
            "the role layer must survive the return: {args:?}"
        );
        // Either delivery form, the same idiom the sibling test above uses
        // for the role layer: the by-file pointer where the adapter's own
        // probe verified that flag, the inline packet otherwise.
        assert!(
            args.iter().any(|arg| {
                arg.contains(crate::commands::ctx::prompt::HANDOFF_BY_FILE_PROMPT)
                    || arg.contains("Continue from the handoff below")
            }),
            "the interim harness's handoff must reach the resumed conversation: {args:?}"
        );

        pane.finish_shutdown().expect("shutdown");
    }

    /// Issue #440 (delta review): a SOURCE recovery resumes the harness's own
    /// conversation, and claude applies its system-prompt flag per
    /// invocation -- so the relaunch must carry the role layer again or a
    /// recovered orchestrator runs without zirv's posture for the rest of its
    /// life. It must NOT carry a handoff prompt: the resumed conversation
    /// already holds everything a handoff could summarise.
    #[test]
    fn a_resume_relaunch_carries_the_role_layer_and_no_handoff_prompt() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "44444444-2222-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = long_lived_argv();
        spec.agent_name = "claude".to_string();
        spec.role = PromptRole::Orchestrator;
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        let argv_log = tmp.path().join("successor-argv.log");
        let script = tmp.path().join("log-argv.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\nsleep 3\n",
                argv_log.display()
            ),
        )
        .expect("write script");

        let cfg = crate::commands::ctx::config::CtxConfig {
            agent_bin: Some(format!("sh {}", script.display())),
            ..Default::default()
        };
        let req = crate::commands::ctx::handover::HandoverRequest {
            target_agent: "claude".to_string(),
            target_model: None,
            force: true,
            requested_at: 0,
            interactive: true,
            automatic: true,
            generation: None,
            structural_only: true,
            resume_session: Some(session_id.to_string()),
            target_runtime: None,
            target_route: None,
        };

        pane.handover(
            &cfg,
            &req,
            &crate::commands::ctx::handoff::Handoff::default(),
            PromptRole::Orchestrator,
            &repo,
            (80, 24),
        )
        .expect("the source resumes");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !argv_log.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let logged = std::fs::read_to_string(&argv_log).unwrap_or_default();
        let args: Vec<&str> = logged.lines().collect();

        let at = args
            .iter()
            .position(|arg| *arg == "--resume")
            .unwrap_or_else(|| panic!("the resume flag must be there: {args:?}"));
        assert_eq!(
            args.get(at + 1).copied(),
            Some(session_id),
            "the resume flag names the source session: {args:?}"
        );
        // Either delivery form: the file flag where the adapter's own `--help`
        // probe verified it, the inline flag otherwise. The invariant is that
        // the layer is carried at all, not which flag carries it.
        let layer_at = args
            .iter()
            .position(|arg| {
                *arg == "--append-system-prompt-file" || *arg == "--append-system-prompt"
            })
            .unwrap_or_else(|| panic!("the role layer must survive the relaunch: {args:?}"));
        assert!(
            args.get(layer_at + 1).is_some_and(|arg| !arg.is_empty()),
            "the flag must actually name a layer: {args:?}"
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains(crate::commands::ctx::prompt::HANDOFF_BY_FILE_PROMPT)),
            "a resumed conversation gets no handoff prompt: {args:?}"
        );

        pane.finish_shutdown().expect("shutdown");
    }

    /// F5 (review, PR #116): a handover swaps in a successor SESSION
    /// continuing the same task -- `report_to` stays (the requester is still
    /// owed a report from whatever is now running in this pane), but the
    /// one-shot completion reminder is scoped per child session, so the
    /// successor must be eligible for its own reminder even if the
    /// predecessor had already received one. `report_back_reminder_sweep`
    /// (`dash::mod`) gates on exactly this flag, so a pane reading `false`
    /// here is what makes it eligible to be reminded again.
    ///
    /// The successor's own argv is deliberately not a real agent (`ping`
    /// with extra positional args it will reject and exit on almost
    /// immediately) -- only the pty spawn itself has to succeed for this
    /// test's purposes, the same ABSOLUTE rule every other test in this
    /// module already follows.
    #[test]
    fn handover_resets_the_report_reminder_flag_for_the_successor_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "77777777-2222-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = long_lived_argv();
        spec.agent_name = "claude".to_string();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        pane.set_report_to(Some("aaaa1111".to_string()));
        pane.mark_report_reminder_sent();
        assert!(
            pane.report_reminder_sent(),
            "sanity: the predecessor session was already reminded"
        );

        let cfg = crate::commands::ctx::config::CtxConfig {
            #[cfg(windows)]
            agent_bin: Some("ping -n 3 127.0.0.1".to_string()),
            #[cfg(unix)]
            agent_bin: Some("sleep 3".to_string()),
            ..Default::default()
        };
        let req = crate::commands::ctx::handover::HandoverRequest {
            target_agent: "claude".to_string(),
            target_model: None,
            force: true,
            requested_at: 0,
            interactive: false,
            automatic: false,
            generation: None,
            structural_only: false,
            resume_session: None,
            target_runtime: None,
            target_route: None,
        };
        let handoff_note = crate::commands::ctx::handoff::Handoff::default();

        pane.handover(
            &cfg,
            &req,
            &handoff_note,
            PromptRole::Worker,
            &repo,
            (80, 24),
        )
        .expect("the swap must succeed against a trivially spawnable program");

        assert_eq!(
            pane.report_to(),
            Some("aaaa1111"),
            "F5: the requester is still owed a report from the successor session"
        );
        assert!(
            !pane.report_reminder_sent(),
            "F5: a fresh child session must be eligible for its own one-shot reminder"
        );

        // finish_shutdown: immediate, no QUIT_GRACE wait -- see the identical
        // comment on `handover_failure_in_successor_setup_leaves_the_old_pane_untouched`.
        pane.finish_shutdown().expect("shutdown");
    }

    /// Finding #3 (issue #358 review): a handover across providers must move
    /// this pane's provider-level token reservation, not silently orphan it.
    /// Before the fix, `Pane::handover` never touched `reservation_id` -- it
    /// still named the OLD provider's ledger entry after the swap, and
    /// `account_reaped_pane_spend` (`dash::mod`) derives its provider from
    /// `pane.agent()`, which by then names the NEW adapter -- so settle on
    /// reap would look the id up in the wrong ledger, find nothing, and leak
    /// the old entry for the rest of this dashboard's life.
    #[test]
    fn handover_moves_the_token_reservation_to_the_new_providers_ledger() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "66666666-2222-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = long_lived_argv();
        spec.agent_name = "claude".to_string();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        pane.set_budget_tokens(Some(4_200));
        let old_reservation = crate::commands::ctx::reservation::reserve(
            &state,
            "anthropic",
            session_id,
            4_200,
            1_700_000_000,
        )
        .expect("seed old reservation");
        pane.set_reservation_id(Some(old_reservation.id.clone()));

        let cfg = crate::commands::ctx::config::CtxConfig {
            #[cfg(windows)]
            agent_bin: Some("ping -n 3 127.0.0.1".to_string()),
            #[cfg(unix)]
            agent_bin: Some("sleep 3".to_string()),
            ..Default::default()
        };
        let req = crate::commands::ctx::handover::HandoverRequest {
            target_agent: "codex".to_string(),
            target_model: None,
            force: true,
            requested_at: 0,
            interactive: false,
            automatic: false,
            generation: None,
            structural_only: false,
            resume_session: None,
            target_runtime: None,
            target_route: None,
        };
        let handoff_note = crate::commands::ctx::handoff::Handoff::default();

        pane.handover(
            &cfg,
            &req,
            &handoff_note,
            PromptRole::Worker,
            &repo,
            (80, 24),
        )
        .expect("the swap must succeed against a trivially spawnable program");

        assert_eq!(
            crate::commands::ctx::reservation::outstanding(&state, "anthropic", 1_700_000_100),
            0,
            "the old provider's reservation must be released on handover"
        );
        let new_id = pane
            .reservation_id()
            .expect("a fresh reservation must be opened on the new provider")
            .to_string();
        assert_ne!(
            new_id, old_reservation.id,
            "the moved reservation must get a fresh id"
        );
        assert_eq!(
            crate::commands::ctx::reservation::outstanding(&state, "openai", 1_700_000_100),
            4_200,
            "the new provider's ledger must carry the same token ceiling"
        );

        // finish_shutdown: immediate, no QUIT_GRACE wait -- see the identical
        // comment on `handover_failure_in_successor_setup_leaves_the_old_pane_untouched`.
        pane.finish_shutdown().expect("shutdown");
    }

    /// R3, end to end on a real supervised child: an idle pane that is
    /// injected into reports `Working` immediately -- so a second idle-gated
    /// caller in the same tick skips it -- and goes back to `Idle` only once
    /// the turn the injection started reports finishing.
    #[test]
    fn an_injection_makes_a_pane_busy_until_its_next_turn_signal() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "33333333-2222-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = long_lived_argv();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        assert!(
            signal_until_idle(&mut pane, &state, session_id),
            "the pane must report a turn boundary before this test can mean anything"
        );

        pane.inject_visible("nudge from operator", "hello")
            .expect("inject");
        assert!(
            matches!(pane.state(), PaneState::Working),
            "a freshly injected pane is busy, not idle: {:?}",
            pane.state()
        );

        assert!(
            signal_until_idle(&mut pane, &state, session_id),
            "the next turn signal must clear the pending injection"
        );

        // finish_shutdown: immediate, no QUIT_GRACE wait -- see the identical
        // comment on `handover_failure_in_successor_setup_leaves_the_old_pane_untouched`.
        pane.finish_shutdown().expect("shutdown");
    }

    const DIALOG: &str = "Would you like to run the following command?\r\n  1. Yes, proceed (y)\r\n  3. No, and tell Codex what to do differently (esc)\r\n";

    #[test]
    fn the_dialog_matcher_needs_the_layout_not_just_the_strings() {
        assert!(codex_approval_dialog_shown(&DIALOG.replace("\r\n", "\n")));
        assert!(codex_approval_dialog_shown(
            "x\nWould you like to make the following edits?\n\u{203a} 1. Yes, proceed\n  2. No, cancel\n"
        ));
        // The same strings inside a diff or code are not a dialog.
        let diff =
            "+    \"Would you like to run the following command?\",\n+    \"Yes, \", \"No, \"\n";
        assert!(!codex_approval_dialog_shown(diff));
        let real_dialog_layout =
            "Would you like to run the following command?\nsome output\n  1. Yes, x\n  2. No, y\n";
        assert!(codex_approval_dialog_shown(real_dialog_layout));
        let far_above = format!(
            "Would you like to run the following command?\n1. Yes, a\n2. No, b\n{}",
            "output\n".repeat(CODEX_DIALOG_BOTTOM_ROWS + 1)
        );
        assert!(
            !codex_approval_dialog_shown(&far_above),
            "not in the bottom part"
        );
        assert!(!codex_approval_dialog_shown(
            "Would you like to run the following command?\nsay Yes, then No, later\n"
        ));
    }

    const FULL_DIALOG: &str = "  Would you like to run the following command?\n\n  Reason: Needs network to fetch crates\n\n  $ cargo nextest run --no-fail-fast\n\n\u{203a} 1. Yes, proceed (y)\n  2. Yes, and don't ask again for commands that start with `cargo nextest run` (p)\n  3. No, and tell Codex what to do differently (esc)\n";

    #[test]
    fn the_dialog_reader_extracts_command_reason_and_the_offered_always_option() {
        use super::super::super::approvals::Decision;
        let dialog = codex_approval_dialog(FULL_DIALOG).expect("dialog");
        assert_eq!(dialog.tool, "Bash");
        assert_eq!(dialog.command, "cargo nextest run --no-fail-fast");
        assert_eq!(
            dialog.details.reason.as_deref(),
            Some("Needs network to fetch crates")
        );
        assert_eq!(
            dialog.details.always.as_deref(),
            Some("commands that start with `cargo nextest run`")
        );
        assert_eq!(dialog.answer_key(Decision::Allow), Some('y'));
        assert_eq!(dialog.answer_key(Decision::AllowAlways), Some('p'));
        assert_eq!(dialog.answer_key(Decision::Deny), None);
        let request = dialog.request("abc123", 7);
        assert_eq!(request.command, "cargo nextest run --no-fail-fast");
        assert!(request.always.is_some());
    }

    #[test]
    fn a_dialog_without_a_dont_ask_again_option_never_offers_always() {
        use super::super::super::approvals::Decision;
        let dialog = codex_approval_dialog(DIALOG).expect("dialog");
        assert_eq!(dialog.details.always, None);
        assert_eq!(dialog.answer_key(Decision::AllowAlways), None);
        assert_eq!(dialog.answer_key(Decision::Allow), Some('y'));
        assert_eq!(codex_approval_dialog("ordinary output\n"), None);
    }

    #[cfg(unix)]
    fn codex_dialog_pane(tmp: &Path, script: &str) -> Pane {
        let state = StateDir::from_root(tmp.join("state"));
        let mut spec = test_spec("77777777-2222-4333-8444-555555555555");
        spec.agent_name = "codex".to_string();
        spec.argv = vec!["sh".to_string(), "-c".to_string(), script.to_string()];
        Pane::spawn(
            spec,
            &state,
            tmp,
            tmp,
            (100, 24),
            &[],
            false,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn")
    }

    #[cfg(unix)]
    fn wait_for_dialog(pane: &mut Pane) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !pane.codex_approval_open() {
            pane.drain();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(pane.codex_approval_open());
    }

    #[cfg(unix)]
    #[test]
    fn a_scrolled_back_pane_still_reports_its_open_dialog() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let script = format!(
            "i=0; while [ $i -lt 60 ]; do echo filler$i; i=$((i+1)); done; printf '{DIALOG}'; sleep 60"
        );
        let mut pane = codex_dialog_pane(tmp.path(), &script);
        wait_for_dialog(&mut pane);
        pane.parser.screen_mut().set_scrollback(30);
        assert!(
            !pane.screen().contents().contains("Yes, proceed"),
            "scrolled out of view"
        );
        pane.refresh_approval_dialog();
        assert!(pane.codex_approval_open());
        assert!(
            !pane.injectable(),
            "nothing is typed while the dialog is open"
        );
        assert_eq!(pane.scrollback(), 30, "the operator's view is left alone");
        let _ = pane.finish_shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn a_shrink_that_cuts_the_dialog_rows_keeps_it_open() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut pane = codex_dialog_pane(tmp.path(), &format!("printf '{DIALOG}'; sleep 60"));
        wait_for_dialog(&mut pane);
        pane.resize(2, 80).expect("resize");
        assert!(
            !pane.screen().contents().contains("No, "),
            "the shrink cut the option rows"
        );
        assert!(pane.codex_approval_open());
        assert!(!pane.injectable(), "nothing is typed before Codex redraws");
        let _ = pane.finish_shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn quitting_a_pane_with_an_open_dialog_types_nothing_into_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let marker = tmp.path().join("typed");
        let script = format!(
            "printf '{DIALOG}'; read x; touch {}; sleep 60",
            marker.display()
        );
        let mut pane = codex_dialog_pane(tmp.path(), &script);
        wait_for_dialog(&mut pane);
        pane.request_quit("/quit\r");
        std::thread::sleep(Duration::from_millis(600));
        assert!(!marker.exists(), "no byte reached the dialog");
        let _ = pane.finish_shutdown();
    }

    #[test]
    fn screen_tail_keeps_the_latest_output_within_its_caps() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut spec = test_spec("55555555-2222-4333-8444-555555555555");
        spec.argv = long_lived_argv();
        let mut pane = Pane::spawn(
            spec,
            &state,
            tmp.path(),
            tmp.path(),
            (200, 30),
            &[],
            false,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");
        pane.parser
            .process(format!("{}latest", format!("{}\r\n", "ø".repeat(180)).repeat(40)).as_bytes());
        pane.parser.screen_mut().set_scrollback(5);
        let offset = pane.scrollback();
        let tail = pane.screen_tail();
        assert!(tail.len() <= 2048);
        assert!(tail.lines().count() <= 20);
        assert!(tail.ends_with("latest"));
        assert_eq!(pane.scrollback(), offset);
        pane.finish_shutdown().expect("shutdown");
    }

    #[test]
    fn injection_retries_once_then_stops_on_output_or_operator_input() {
        for (responds, operator_types) in [(false, false), (true, false), (false, true)] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = StateDir::from_root(tmp.path().join("state"));
            let mut spec = test_spec("55555555-2222-4333-8444-555555555555");
            spec.argv = long_lived_argv();
            let mut pane = Pane::spawn(
                spec,
                &state,
                tmp.path(),
                tmp.path(),
                (80, 24),
                &[],
                false,
                DEFAULT_IDLE_QUIET,
            )
            .expect("spawn");
            let capture = tmp.path().join("input");
            if let PaneKind::Wrapped(pty) = &mut pane.kind {
                pty.writer = Arc::new(Mutex::new(Box::new(
                    std::fs::File::create(&capture).expect("capture"),
                )));
            }
            pane.inject_visible("mail", "hello").expect("inject");
            pane.submit_pending().expect("submit");
            let submitted = pane.submit_confirmation.expect("confirmation").0;
            if responds {
                pane.last_output_at = Some(submitted + Duration::from_millis(10));
            }
            if operator_types {
                pane.write_operator_input(b"x").expect("operator input");
                pane.user_typed_since_turn = false;
            }
            assert_eq!(
                pane.check_submission(submitted + Duration::from_secs(1))
                    .expect("first check"),
                operator_types
            );
            assert_eq!(
                pane.check_submission(submitted + Duration::from_secs(2))
                    .expect("second check"),
                !responds && !operator_types
            );
            assert!(
                !pane
                    .check_submission(submitted + Duration::from_secs(3))
                    .expect("finished")
            );
            let bytes = std::fs::read(capture).expect("input bytes");
            assert_eq!(
                bytes.iter().filter(|byte| **byte == b'\r').count(),
                if responds || operator_types { 1 } else { 2 }
            );
            assert_eq!(String::from_utf8_lossy(&bytes).matches("hello").count(), 1);
            pane.finish_shutdown().expect("shutdown");
        }
    }

    #[test]
    fn injection_submit_waits_for_echo_quiet_with_a_ceiling() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut spec = test_spec("55555555-2222-4333-8444-555555555555");
        spec.argv = long_lived_argv();
        let mut pane = Pane::spawn(
            spec,
            &state,
            tmp.path(),
            tmp.path(),
            (80, 24),
            &[],
            false,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");
        pane.inject_visible("mail", "hello").expect("inject");
        let due = pane.pending_submit.expect("pending");
        pane.last_output_at = Some(due);
        assert!(!pane.pending_submit_due(due), "echo is still arriving");
        assert!(pane.pending_submit_due(due + INJECTION_SUBMIT_DELAY));
        let ceiling = due + Duration::from_secs(2);
        pane.last_output_at = Some(ceiling);
        assert!(
            pane.pending_submit_due(ceiling),
            "continuous output must not starve Enter"
        );
        pane.finish_shutdown().expect("shutdown");
    }

    /// F1/F2, end to end on a real supervised child: `inject_visible` must
    /// not block the caller (no inline sleep), phase 1's stamping is
    /// immediate, `pending_submit_due` only flips true once
    /// `INJECTION_SUBMIT_DELAY` has actually elapsed, and `submit_pending`
    /// drains it -- writing the lone `\r` against the real pty writer -- and
    /// clears `has_pending_submit` once that write lands.
    #[test]
    fn inject_visible_does_not_block_and_its_pending_submit_drains_after_the_deadline() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "55555555-2222-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = long_lived_argv();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        assert!(
            signal_until_idle(&mut pane, &state, session_id),
            "the pane must report a turn boundary before this test can mean anything"
        );

        let started = Instant::now();
        pane.inject_visible("nudge from operator", "hello")
            .expect("inject");
        assert!(
            started.elapsed() < INJECTION_SUBMIT_DELAY,
            "F2: inject_visible must return immediately, never sleep for the settle gap"
        );

        // Stamped at phase 1, not deferred to the CR.
        assert!(
            matches!(pane.state(), PaneState::Working),
            "injected_awaiting_turn is set immediately: {:?}",
            pane.state()
        );
        assert!(
            pane.has_pending_submit(),
            "a submission is now owed for this injection"
        );
        assert!(
            !pane.pending_submit_due(Instant::now()),
            "the deadline has not elapsed yet"
        );

        std::thread::sleep(INJECTION_SUBMIT_DELAY + Duration::from_millis(20));
        assert!(
            pane.pending_submit_due(Instant::now()),
            "due once the settle gap has actually passed"
        );

        pane.submit_pending().expect("the deferred CR write");
        assert!(
            !pane.has_pending_submit(),
            "draining a due submit clears it"
        );

        // finish_shutdown: immediate, no QUIT_GRACE wait -- see the identical
        // comment on `handover_failure_in_successor_setup_leaves_the_old_pane_untouched`.
        pane.finish_shutdown().expect("shutdown");
    }

    /// F1/F2, end to end: an operator who starts typing before an
    /// injection's own settle deadline has elapsed must have the pending
    /// `\r` flushed first, so their own keystroke never lands ahead of the
    /// still-unsubmitted injected line.
    #[test]
    fn write_operator_input_flushes_a_pending_submit_before_the_keystroke() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "66666666-2222-4333-8444-777777777777";
        let mut spec = test_spec(session_id);
        spec.argv = long_lived_argv();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        assert!(
            signal_until_idle(&mut pane, &state, session_id),
            "the pane must report a turn boundary before this test can mean anything"
        );

        pane.inject_visible("nudge from operator", "hello")
            .expect("inject");
        assert!(
            pane.has_pending_submit(),
            "sanity: a submission is owed before the operator types"
        );

        pane.write_operator_input(b"half a thought")
            .expect("forwarding a keystroke must succeed while the child is alive");
        assert!(
            !pane.has_pending_submit(),
            "the pending CR must be flushed before the keystroke reaches the composer"
        );

        // finish_shutdown: immediate, no QUIT_GRACE wait -- see the identical
        // comment on `handover_failure_in_successor_setup_leaves_the_old_pane_untouched`.
        pane.finish_shutdown().expect("shutdown");
    }

    /// F1/G1, end to end on a real supervised child: a keystroke the dashboard
    /// forwards to a pane takes it out of reach of both idle-gated injectors
    /// until the pane reports its next turn boundary -- but, per G1, this must
    /// no longer show up in the pane's own **displayed** state: the sidebar
    /// glyph and the quit-confirm dialog (both driven by `state()`) must keep
    /// reading the pane as `Idle`, only `injectable()` may say otherwise.
    #[test]
    fn operator_typing_makes_a_pane_ineligible_but_leaves_its_glyph_idle() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "44444444-2222-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = long_lived_argv();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        assert!(
            signal_until_idle(&mut pane, &state, session_id),
            "the pane must report a turn boundary before this test can mean anything"
        );
        assert!(pane.injectable(), "idle with nothing typed is injectable");

        pane.write_operator_input(b"half a thought")
            .expect("forwarding a keystroke must succeed while the child is alive");
        assert!(
            matches!(pane.state(), PaneState::Idle),
            "G1: typing with no turn signal following it must not change the \
             displayed state -- the pane is not mid-turn, it is mid-thought: {:?}",
            pane.state()
        );
        assert!(
            !pane.injectable(),
            "an operator mid-thought is not an injection target"
        );

        assert!(
            signal_until_idle(&mut pane, &state, session_id),
            "the next turn boundary clears the operator-typing flag"
        );
        assert!(
            pane.injectable(),
            "and the pane is reachable again once it does"
        );

        // finish_shutdown: immediate, no QUIT_GRACE wait -- see the identical
        // comment on `handover_failure_in_successor_setup_leaves_the_old_pane_untouched`.
        pane.finish_shutdown().expect("shutdown");
    }

    /// Task A end to end: a signal-less pane's real supervised child prints
    /// once at startup and then goes quiet for the rest of its life. The pane
    /// must read `Working` while still inside the quiet window and
    /// `Idle`/`injectable` once the window closes -- with no turn signal ever
    /// sent to it (a codex-shaped adapter never sends one; nothing in this
    /// test's own child does either).
    #[test]
    fn a_signal_less_pane_becomes_idle_after_the_quiet_period_and_not_before() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "77777777-2222-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = silent_after_first_line_argv();
        let idle_quiet = Duration::from_millis(1000);
        let mut pane = Pane::spawn(spec, &state, &repo, &repo, (80, 24), &[], false, idle_quiet)
            .expect("spawn");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            pane.drain();
            if pane.last_line().contains("hello") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            pane.last_line().contains("hello"),
            "the startup line must have landed before the rest of this test can mean anything: {:?}",
            pane.last_line()
        );
        assert!(
            matches!(pane.state(), PaneState::Working),
            "still inside the quiet window right after the startup line: {:?}",
            pane.state()
        );
        assert!(!pane.injectable(), "not yet reachable while still working");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut became_idle = false;
        while std::time::Instant::now() < deadline {
            pane.drain();
            if matches!(pane.state(), PaneState::Idle) {
                became_idle = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            became_idle,
            "must become idle once the quiet window closes, with no turn signal ever sent"
        );
        assert!(
            pane.injectable(),
            "and therefore reachable by the mail sweep/nudge drain"
        );

        // finish_shutdown: immediate, no QUIT_GRACE wait -- see the identical
        // comment on `handover_failure_in_successor_setup_leaves_the_old_pane_untouched`.
        pane.finish_shutdown().expect("shutdown");
    }

    /// Task A regression guard: a signal-carrying pane must ignore output
    /// quiescence entirely, exactly as before this feature existed -- it
    /// stays `Working` well past the quiet window with no turn signal ever
    /// sent, so the two branches of `pane_is_idle` provably do not bleed into
    /// each other.
    #[test]
    fn a_signal_carrying_pane_ignores_output_quiescence_entirely() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "88888888-2222-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = silent_after_first_line_argv();
        let idle_quiet = Duration::from_millis(200);
        let mut pane = Pane::spawn(spec, &state, &repo, &repo, (80, 24), &[], true, idle_quiet)
            .expect("spawn");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            pane.drain();
            if pane.last_line().contains("hello") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(pane.last_line().contains("hello"), "sanity: the child ran");

        std::thread::sleep(idle_quiet * 10);
        pane.drain();
        assert!(
            matches!(pane.state(), PaneState::Working),
            "a signal-carrying pane stays working through a long quiet period with no \
             signal sent: {:?}",
            pane.state()
        );
        assert!(!pane.injectable());

        // finish_shutdown: immediate, no QUIT_GRACE wait -- see the identical
        // comment on `handover_failure_in_successor_setup_leaves_the_old_pane_untouched`.
        pane.finish_shutdown().expect("shutdown");
    }

    /// H1 (review): the bug this test pins -- a signal-less pane's own
    /// `inject_visible` must hold it uninjectable for a full `idle_quiet`
    /// window measured from the injection itself, not from the child's
    /// (already-old) last output. Before the fix, an injection never moved
    /// `last_output_at`, so the very next `drain()` tick -- tens of
    /// milliseconds later, nowhere near a full `idle_quiet` -- still read the
    /// pane as quiet and immediately cleared `injected_awaiting_turn`,
    /// letting a second injector (the nudge drain running right after the
    /// mail sweep in the same tick) land straight on top of the first.
    #[test]
    fn a_signal_less_pane_stays_uninjectable_for_a_full_window_after_its_own_injection() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "aaaaaaaa-3333-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = silent_after_first_line_argv();
        let idle_quiet = Duration::from_millis(500);
        let mut pane = Pane::spawn(spec, &state, &repo, &repo, (80, 24), &[], false, idle_quiet)
            .expect("spawn");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            pane.drain();
            if pane.last_line().contains("hello") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(pane.last_line().contains("hello"), "sanity: the child ran");

        // Wait out the quiet window from the startup line so the pane is
        // genuinely idle before this test's own injection.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            pane.drain();
            if pane.injectable() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "must become injectable before this test can mean anything"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        pane.inject_visible("test", "one").expect("first injection");
        pane.submit_pending().expect("submit injected line");
        assert!(
            matches!(pane.state(), PaneState::Working),
            "immediately busy after a successful injection"
        );

        // `submit_confirmation` is a separate, echo-driven confirmation/retry
        // lifecycle (covered by `injection_retries_once_then_stops_on_
        // output_or_operator_input`/`injection_submit_waits_for_echo_quiet_
        // with_a_ceiling`), not this test's subject; clear it so only
        // `injected_awaiting_turn`'s idle_quiet windowing (H1) is under test.
        pane.submit_confirmation = None;

        // On a real Unix pty the kernel echoes the injected bytes, so
        // `drain()` keeps refreshing `last_output_at` to real "now" for a
        // little while after the injection -- back-dating just
        // `last_local_input_at` is not sound there (`signal_less_quiescent`
        // takes the LATEST of the two). Instead, poll for the observable
        // state change and check a lower bound that scheduler lateness can
        // only ever help satisfy, never violate: quiescence is computed from
        // a fresh `Instant::now()` against the injection's own stamp, so
        // becoming injectable before a full `idle_quiet` has really elapsed
        // is impossible regardless of how late this thread runs.
        let injected_at = pane
            .last_local_input_at
            .expect("inject_visible just stamped it");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let first_injectable_at = loop {
            pane.drain();
            if pane.injectable() {
                break std::time::Instant::now();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "must become injectable again within a generous deadline"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(
            first_injectable_at.duration_since(injected_at) >= idle_quiet,
            "H1: must not become injectable again before a full idle_quiet has elapsed since \
             the injection"
        );

        // finish_shutdown: immediate, no QUIT_GRACE wait -- see the identical
        // comment on `handover_failure_in_successor_setup_leaves_the_old_pane_untouched`.
        pane.finish_shutdown().expect("shutdown");
    }

    /// H1 (review): a signal-less pane has no turn signal to tell "the
    /// operator is still typing" from "the child finished", so -- unlike a
    /// signal-carrying pane, where G1 deliberately keeps typing off the
    /// *displayed* state and only gates `injectable()` -- a signal-less
    /// pane's own idleness clock blends local input in on the same axis
    /// (`pane_is_idle`'s signal-less branch, via `signal_less_quiescent`):
    /// a keystroke holds it `Working`, not merely uninjectable, for a full
    /// `idle_quiet` window measured from that keystroke, even with no child
    /// output at all following it. Also pins that the flag is not cleared by
    /// stale quiescence on the very next tick.
    #[test]
    fn operator_typing_holds_a_signal_less_pane_working_for_a_full_quiet_window() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "bbbbbbbb-3333-4333-8444-555555555555";
        let mut spec = test_spec(session_id);
        spec.argv = silent_after_first_line_argv();
        let idle_quiet = Duration::from_millis(500);
        let mut pane = Pane::spawn(spec, &state, &repo, &repo, (80, 24), &[], false, idle_quiet)
            .expect("spawn");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            pane.drain();
            if pane.last_line().contains("hello") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(pane.last_line().contains("hello"), "sanity: the child ran");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            pane.drain();
            if pane.injectable() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "must become idle before this test can mean anything"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        pane.write_operator_input(b"half a thought")
            .expect("forwarding a keystroke must succeed while the child is alive");

        // The very next tick, well under idle_quiet later: must still read
        // as busy -- not cleared by the stale (already-old) output
        // timestamp.
        std::thread::sleep(Duration::from_millis(50));
        pane.drain();
        assert!(
            matches!(pane.state(), PaneState::Working),
            "H1: a keystroke into a signal-less pane must hold it non-idle for \
             a full quiet window, not just until the next drain tick: {:?}",
            pane.state()
        );
        assert!(!pane.injectable());

        // Still working only partway through the window.
        std::thread::sleep(idle_quiet / 2);
        pane.drain();
        assert!(
            matches!(pane.state(), PaneState::Working),
            "still short of a full idle_quiet window since the keystroke: {:?}",
            pane.state()
        );

        // And idle again once a full idle_quiet has elapsed since the last
        // keystroke.
        std::thread::sleep(idle_quiet);
        pane.drain();
        assert!(
            matches!(pane.state(), PaneState::Idle),
            "idle again once idle_quiet has elapsed since the last keystroke: {:?}",
            pane.state()
        );
        assert!(pane.injectable());

        // finish_shutdown: immediate, no QUIT_GRACE wait -- see the identical
        // comment on `handover_failure_in_successor_setup_leaves_the_old_pane_untouched`.
        pane.finish_shutdown().expect("shutdown");
    }

    #[test]
    fn shutdown_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let spec = test_spec("22222222-2222-4333-8444-555555555555");
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        pane.set_writer_permit(
            super::super::super::permit::acquire_writer(&state, 1, "first", &repo, None)
                .expect("first permit"),
        );
        pane.shutdown("")
            .expect("first shutdown releases the guard");
        assert!(!pane.holds_writer_permit());
        let _successor =
            super::super::super::permit::acquire_writer(&state, 1, "successor", &repo, None)
                .expect("shutdown releases the permit before the pane is dropped");
        let short = pane.short().to_string();
        let record_path = state.sessions().join(format!("{short}.json"));
        assert!(
            !record_path.exists(),
            "the registry record must be gone after shutdown"
        );

        // Second call must not error and must not touch anything that is
        // already gone.
        pane.shutdown("")
            .expect("second shutdown is a no-op, not an error");
        pane.finish_shutdown()
            .expect("second cleanup path is also idempotent");
        drop(pane);
        assert!(
            super::super::super::permit::acquire_writer(&state, 1, "third", &repo, None).is_err(),
            "old pane cleanup must not release its successor's permit"
        );
        assert!(!record_path.exists());
    }

    #[test]
    fn native_roster_reports_live_writer_permit() {
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
        let mut pane = Pane::spawn_native(
            &super::super::super::config::CtxConfig::default(),
            &state,
            &lookup,
            &repo,
            Verb::Dash,
            "native".to_string(),
            (80, 24),
            super::super::native_pane::NativeDashboardSpec {
                repo: repo.clone(),
                role: "worker".to_string(),
                route: None,
                writing: true,
                provider: Some(provider),
                seat: None,
                initial_input: None,
            },
        )
        .expect("spawn native pane");

        assert!(pane.holds_writer_permit());
        pane.stop_now(0).expect("stop native pane");
        wait_for_native_pane_end(&mut pane);
        assert!(!pane.holds_writer_permit());
    }

    /// M10: a drain stops once it has processed its byte budget and reports
    /// that more remains, so the event loop is never starved by a firehose.
    #[test]
    fn drain_into_stops_at_the_budget_and_reports_more_remaining() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        // Five 4-byte messages = 20 bytes; a 10-byte budget stops partway.
        for _ in 0..5 {
            tx.send(b"abcd".to_vec()).expect("send");
        }
        let mut parser = vt100::Parser::new(4, 40, 0);
        let (any, more, used) = drain_into(&rx, &mut parser, 10);
        assert!(any, "some bytes were processed");
        assert_eq!(
            used, 12,
            "the spend it reports back is what it actually parsed, so a caller \
             sharing one budget across panes can subtract it"
        );
        assert!(
            more,
            "the budget cut the drain short with bytes still queued"
        );
        assert!(rx.try_recv().is_ok(), "messages remain on the channel");
    }

    /// Final review: a zero share must not take even one message, or the
    /// tick's overshoot grows with the pane count.
    #[test]
    fn drain_into_with_no_budget_takes_nothing_and_keeps_the_hold() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        tx.send(b"abcd".to_vec()).expect("send");
        let mut parser = vt100::Parser::new(4, 40, 0);
        let (any, more, used) = drain_into(&rx, &mut parser, 0);
        assert!(!any);
        assert_eq!(used, 0);
        assert!(more, "nothing was observed, so the pane stays held");
        assert!(rx.try_recv().is_ok(), "the message is still queued");
    }

    /// A channel that empties under budget reports nothing remaining; a drained
    /// (and disconnected) channel reports neither work done nor more remaining.
    #[test]
    fn drain_into_reports_no_more_when_the_channel_empties() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        tx.send(b"hi".to_vec()).expect("send");
        let mut parser = vt100::Parser::new(4, 40, 0);
        let (any, more, used) = drain_into(&rx, &mut parser, 1024);
        assert!(any);
        assert_eq!(used, 2);
        assert!(
            !more,
            "an emptied channel under budget has nothing remaining"
        );

        drop(tx);
        let (any2, more2, used2) = drain_into(&rx, &mut parser, 1024);
        assert!(!any2 && !more2, "a drained, closed channel is quiet");
        assert_eq!(used2, 0);
    }

    /// M9: the batched-shutdown primitives -- ask to quit without waiting, then
    /// escalate and release the record -- take a live pane down and free its
    /// registry entry, idempotently.
    #[test]
    fn request_quit_then_finish_shutdown_releases_the_record() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let mut spec = test_spec("66666666-2222-4333-8444-555555555555");
        spec.argv = long_lived_argv();
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");
        let short = pane.short().to_string();
        let record_path = state.sessions().join(format!("{short}.json"));
        assert!(
            record_path.exists(),
            "the record exists while the pane runs"
        );

        // No real quit sequence for a sleep/ping child, so the escalation half
        // (kill) is what ends it; either way the record must be released.
        pane.request_quit("");
        pane.finish_shutdown().expect("finish_shutdown");
        assert!(!record_path.exists(), "the record is released");

        // Idempotent, and interchangeable with `shutdown`.
        pane.finish_shutdown()
            .expect("finish_shutdown is idempotent");
        pane.shutdown("")
            .expect("shutdown after finish_shutdown is a no-op");
    }

    // =====================================================================
    // Issue #490 (roadmap N21 item A): a mixed roster -- wrapped and native
    // panes in ONE dashboard's pane vector.
    //
    // Every test below builds a REAL native pane (`Pane::spawn_native`)
    // against `runtime::fixture::FixtureProvider`, so the routing assertions
    // are made against the same `PaneKind` the dashboard actually holds
    // rather than against a stand-in.
    // =====================================================================

    /// A state dir, a repo tree for the writer lease to claim, and the env
    /// lookup that resolves the former -- the same shape
    /// `runtime::native::tests::interactive_shutdown_fixture` uses.
    fn native_pane_fixture() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        StateDir,
        std::collections::HashMap<String, String>,
    ) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state_root = tmp.path().join("state");
        let state = StateDir::from_root(state_root.clone());
        let env: std::collections::HashMap<String, String> = [(
            super::super::super::state::STATE_ENV.to_string(),
            state_root.to_str().expect("utf8").to_string(),
        )]
        .into();
        (tmp, repo, state, env)
    }

    fn native_spec(repo: &Path) -> super::super::native_pane::NativeDashboardSpec {
        super::super::native_pane::NativeDashboardSpec {
            repo: repo.to_path_buf(),
            role: "worker".to_string(),
            route: None,
            // Not writing: a writer lease needs a linked worktree, and this
            // fixture's repo is a bare temp directory. The pane kind, its
            // routing and its delivery path are what these tests are about.
            writing: false,
            provider: Some(format!(
                "fixture:{}",
                super::super::super::runtime::fixture::fixture_root()
                    .join("helper-answer.json")
                    .display()
            )),
            seat: None,
            initial_input: None,
        }
    }

    fn spawn_native_test_pane(
        state: &StateDir,
        env: &std::collections::HashMap<String, String>,
        repo: &Path,
    ) -> Pane {
        let lookup = |key: &str| env.get(key).cloned();
        Pane::spawn_native(
            &super::super::super::config::CtxConfig::default(),
            state,
            &lookup,
            repo,
            Verb::Dash,
            "wrk native".to_string(),
            (80, 24),
            native_spec(repo),
        )
        .expect("a native pane opens")
    }

    fn wait_for_native_pane_end(pane: &mut Pane) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !matches!(pane.state(), PaneState::Ended(_)) && Instant::now() < deadline {
            let _ = pane.drain_with_budget(DRAIN_BUDGET_BYTES);
            std::thread::yield_now();
        }
        assert!(
            matches!(pane.state(), PaneState::Ended(_)),
            "native worker did not terminate after stop"
        );
    }

    #[test]
    fn a_mixed_roster_holds_both_pane_kinds_and_renders_each_its_own_way() {
        let (_tmp, repo_dir, state, env) = native_pane_fixture();
        let repo = repo_dir.path();

        let mut spec = test_spec("31111111-2222-4333-8444-555555555555");
        spec.argv = long_lived_argv();
        let wrapped = Pane::spawn(
            spec,
            &state,
            repo,
            repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("wrapped pane");
        let native = spawn_native_test_pane(&state, &env, repo);

        // ONE vector, two kinds -- the whole point of the retrofit.
        // One roster, two kinds -- the dashboard's own `Vec<Pane>` shape.
        let mut panes: Vec<Pane> = vec![wrapped, native];
        assert_eq!(
            panes.iter().map(Pane::is_native).collect::<Vec<_>>(),
            vec![false, true]
        );
        // Rendering dispatch: the wrapped pane has a vt100 grid and no native
        // driver; the native pane has a driver and its own transcript view.
        assert!(panes[0].native().is_none());
        assert_eq!(panes[0].screen().size(), (24, 80));
        let (view, _presentation) = panes[1].native().expect("driver").view();
        assert!(view.items.is_empty(), "a fresh conversation has no items");

        // Both report the identity the roster, mail and attention key on.
        assert!(!panes[0].short().is_empty());
        assert!(!panes[1].short().is_empty());
        assert_ne!(panes[0].short(), panes[1].short());

        for pane in panes.iter_mut() {
            let _ = pane.stop_now(0);
        }
    }

    #[test]
    fn focus_routes_a_key_to_the_focused_panes_own_kind() {
        let (_tmp, repo_dir, state, env) = native_pane_fixture();
        let repo = repo_dir.path();
        let mut spec = test_spec("32111111-2222-4333-8444-555555555555");
        spec.argv = long_lived_argv();
        let wrapped = Pane::spawn(
            spec,
            &state,
            repo,
            repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("wrapped pane");
        let native = spawn_native_test_pane(&state, &env, repo);
        // One roster, two kinds -- the dashboard's own `Vec<Pane>` shape.
        let mut panes: Vec<Pane> = vec![wrapped, native];

        // Focus on the native pane: the key reaches the composer, not a pty.
        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('x'),
            crossterm::event::KeyModifiers::NONE,
        );
        let mut ctrl_c = None;
        let routed = panes[1].native_mut().map(|native| {
            super::super::native_pane::handle_native_key(
                native,
                key,
                &mut ctrl_c,
                false,
                false,
                &super::super::super::config::CtxConfig::default(),
            )
        });
        assert_eq!(routed, Some(super::super::native_pane::NativeKey::Consumed));
        assert_eq!(
            panes[1]
                .native()
                .expect("driver")
                .view()
                .1
                .composer
                .draft
                .as_str(),
            "x",
            "the key went to the native composer"
        );

        // Focus on the wrapped pane: there is no native driver to route to at
        // all, so the dashboard falls through to the pty writer.
        assert!(panes[0].native_mut().is_none());
        panes[0]
            .write_operator_input(b"x")
            .expect("the wrapped pane takes bytes");

        for pane in panes.iter_mut() {
            let _ = pane.stop_now(0);
        }
    }

    #[test]
    fn a_wrapped_pane_is_never_offered_a_native_control() {
        let (_tmp, repo_dir, state, _env) = native_pane_fixture();
        let repo = repo_dir.path();
        let mut spec = test_spec("33111111-2222-4333-8444-555555555555");
        spec.argv = long_lived_argv();
        let mut pane = Pane::spawn(
            spec,
            &state,
            repo,
            repo,
            (80, 24),
            &[],
            true,
            DEFAULT_IDLE_QUIET,
        )
        .expect("wrapped pane");

        assert!(!pane.is_native());
        assert!(!pane.accepts_native_controls());
        assert!(pane.native().is_none());
        assert!(pane.native_mut().is_none());
        // And the pty-only surface still works on it, which is the other half
        // of the same rule: neither kind silently gets the other's controls.
        assert!(pane.writer().is_ok());
        let _ = pane.stop_now(0);
    }

    #[test]
    fn mail_to_a_native_pane_goes_through_its_submit_path_not_a_pty_injection() {
        let (_tmp, repo_dir, state, env) = native_pane_fixture();
        let repo = repo_dir.path();
        let mut pane = spawn_native_test_pane(&state, &env, repo);

        // The mail sweep's own call. On a wrapped pane this types a visible
        // line and arms a deferred carriage return; on a native pane it must
        // reach the composer's submit path instead -- there is nothing to type
        // into and no `\r` to send.
        pane.inject_visible("mail", "please review the seat fence")
            .expect("delivery");

        let native = pane.native().expect("driver");
        let (_view, presentation) = native.view();
        assert!(
            presentation.composer.draft.is_empty(),
            "the message was submitted, not left sitting in the draft"
        );
        assert!(
            !pane.has_pending_submit(),
            "a native delivery arms no two-phase pty submit"
        );
        assert!(
            pane.writer().is_err(),
            "and there is no pty writer it could have been typed into"
        );
        let _ = pane.stop_now(0);
    }

    #[test]
    fn a_native_pane_refuses_a_harness_handover_and_reports_its_own_facts() {
        let (_tmp, repo_dir, state, env) = native_pane_fixture();
        let repo = repo_dir.path();
        let mut pane = spawn_native_test_pane(&state, &env, repo);

        // Budget/attention/roster facts: a native pane answers all three.
        assert_eq!(pane.state(), PaneState::Idle);
        assert!(pane.reachable(), "a native pane binds no socket to fail on");
        assert!(pane.child_pid().is_none());
        assert!(
            pane.measured_usage().is_some(),
            "usage comes from the session's own journal, not a transcript read"
        );
        assert_eq!(
            pane.session_id(),
            pane.native().expect("driver").journal_session(),
        );

        // A handover swaps one wrapped harness child for another; a native
        // pane has none, and says so rather than silently doing nothing.
        let error = pane
            .handover(
                &super::super::super::config::CtxConfig::default(),
                &super::super::super::handover::HandoverRequest {
                    target_agent: "claude".to_string(),
                    target_model: None,
                    force: true,
                    requested_at: 0,
                    interactive: false,
                    automatic: false,
                    generation: None,
                    structural_only: false,
                    resume_session: None,
                    target_runtime: None,
                    target_route: None,
                },
                &super::super::super::handoff::Handoff::default(),
                PromptRole::Worker,
                repo,
                (80, 24),
            )
            .expect_err("a native pane has no harness child");
        assert!(
            error.to_string().contains("no harness child"),
            "unexpected refusal: {error}"
        );
        let _ = pane.stop_now(0);
    }

    #[test]
    fn ending_a_native_pane_is_idempotent_and_retires_it_like_any_other() {
        let (_tmp, repo_dir, state, env) = native_pane_fixture();
        let repo = repo_dir.path();
        let mut pane = spawn_native_test_pane(&state, &env, repo);
        let short = pane.short().to_string();
        let record_path = state.sessions().join(format!("{short}.json"));
        assert!(record_path.exists(), "the record exists while it runs");

        pane.stop_now(7).expect("stop_now");
        assert!(
            !matches!(pane.state(), PaneState::Ended(_)),
            "requesting cancellation is not proof of worker termination"
        );
        assert!(
            record_path.exists(),
            "lifecycle remains held while the worker is stopping"
        );
        wait_for_native_pane_end(&mut pane);
        assert_eq!(pane.state(), PaneState::Ended(7));
        assert!(
            record_path.exists(),
            "the reap has not released lifecycle yet"
        );
        pane.finish_shutdown().expect("finish shutdown");
        assert!(
            !record_path.exists(),
            "the record is released after termination"
        );
        // Both halves are idempotent, exactly as they are for a wrapped pane.
        pane.finish_shutdown().expect("idempotent");
        pane.shutdown("").expect("idempotent");
    }
}
