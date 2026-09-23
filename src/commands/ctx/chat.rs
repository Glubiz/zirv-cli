//! `zirv ctx chat`: an interactive session launched through the same `wrap`
//! supervision every other interactive verb uses, but the launch is built
//! from the resolved adapter rather than from a user-supplied argv (there is
//! nothing on the command line for `wrap`'s own detection to guess at), and
//! the session is flagged `PromptRole::Orchestrator` rather than `Worker`:
//! this is the session a human is talking to directly, so it is the one
//! allowed to hear about delegating to other harnesses (`zirv ctx send`,
//! `zirv ctx inbox`, `zirv ctx agent`).

use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::Path;

use crossterm::cursor::{MoveDown, MoveToColumn, MoveUp};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode};
use unicode_width::UnicodeWidthChar;

use super::adapters::{self, AgentAdapter, DefaultOrigin};
use super::chrome::{self, BannerFacts, ChromeCaps, HarnessRule};
use super::config::{CtxConfig, EnvLookup, env_from_process};
use super::dash;
use super::dash::pane::PaneSpec;
use super::event::SessionId;
use super::jev::{self, JevEffect};
use super::prompt::PromptRole;
use super::proxy::{
    self,
    decision::{ProxyDecision, SeatRole},
};
use super::runtime::{self as runtime_kind, RuntimeKind};
use super::state::StateDir;
use super::term;
use super::wrap::{self, WrapArgs};
use super::{CtxResult, handoff, resume};

#[derive(Debug, clap::Args)]
pub struct ChatArgs {
    /// Adapter name: claude or codex. Falls back to the configured default,
    /// then to the registry's own fallback rule.
    #[arg(long)]
    pub agent: Option<String>,
    /// Fold the latest stored handoff into the first prompt.
    #[arg(long, default_value_t = false)]
    pub resume: bool,
    /// Simple run: skip every zirv-injected instruction, including the shipped
    /// default. Supervision, pacing and hooks still apply.
    #[arg(long, default_value_t = false)]
    pub simple: bool,
    /// Suppress the `zirv ▸` announcement channel. Errors and warnings are
    /// never suppressed; the launch banner and status bar have their own
    /// `[chrome]` toggles.
    #[arg(long, default_value_t = false)]
    pub quiet: bool,
    /// Start even though this process looks like it is already inside an
    /// agent session. Off by default: a nested interactive supervisor can
    /// take the outer session down.
    #[arg(long, default_value_t = false)]
    pub allow_nested: bool,
    /// T10: see `WrapArgs::force_pace` -- threaded straight through, since a
    /// `chat` launch becomes a `wrap` launch (`wrap_args_for`).
    #[arg(long, default_value_t = false)]
    pub force_pace: bool,
    /// Issue #358 (task 4): keep this orchestrator seat off the automatic
    /// cross-harness rollover path entirely (`fallback.auto_orchestrator_
    /// rollover`), regardless of headroom. Folded into the same `EnvLookup`
    /// `--quiet` already rides (`pin_env`, mirroring `quiet_env`) as
    /// `ZIRV_CTX_SEAT_PIN=true`, so `seat::pin_from_env` reads it correctly
    /// wherever a seat is registered downstream, without this flag having to
    /// be threaded through `WrapArgs`/`PaneSpec` by hand.
    #[arg(long, default_value_t = false)]
    pub pin_harness: bool,
    /// Issue #352: never use the persistent runtime, even when the operator
    /// has turned it on. The compatibility and debugging escape hatch -- this
    /// process owns the pty, and the session ends when it does, exactly as
    /// every `zirv chat` did before the runtime existed.
    #[arg(long, default_value_t = false)]
    pub no_session: bool,
    /// Issue #480 (roadmap N11): `native` opens a structured native
    /// conversation pane (N09's in-process agent loop, no coding harness
    /// installed, no PTY) instead of a wrapped-harness session. Every other
    /// value, and the default (unset), is today's wrapped-harness dashboard.
    /// A native pane never accepts `--agent`, `--simple`, `--resume` or
    /// `extra` -- see `run_with`'s own refusal for a value combined with any
    /// of those.
    #[arg(long)]
    pub runtime: Option<String>,
    /// Issue #537: force this launch through the harness proxy's intake
    /// view, overriding `[proxy] enabled` for this one launch. Mutually
    /// exclusive with `--no-proxy`; still skipped outright under `--simple`
    /// or `--resume` (see `run_with`'s own intake step).
    #[arg(long, conflicts_with = "no_proxy")]
    pub proxy: bool,
    /// The inverse of `--proxy`: never take over this launch through the
    /// intake view, even when `[proxy] enabled = true`.
    #[arg(long, default_value_t = false)]
    pub no_proxy: bool,
    /// Extra arguments passed through to the agent, after `--`.
    //
    // `allow_hyphen_values`, because what gets passed through here is the
    // agent's own flags.
    #[arg(allow_hyphen_values = true, last = true)]
    pub extra: Vec<String>,
}

/// What `run_with` hands to `wrap::run_with`: the resolved agent's own name
/// (so wrap never has to guess), the argv `interactive_cmd` built, and the
/// role every chat session carries.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatLaunch {
    pub agent_name: String,
    pub argv: Vec<String>,
    pub role: PromptRole,
    /// Always `Verb::Chat`: a chat session's registry record must say so
    /// rather than falling back to `wrap`'s own default, the same way
    /// `role` is always `Orchestrator` regardless of resuming or extra
    /// flags.
    pub verb: super::sessions::Verb,
}

/// Builds the launch from the adapter rather than from any user-supplied
/// argv: a chat session names no wrapped command on the command line, so
/// there is nothing for `wrap`'s own detection to guess at. Always
/// `PromptRole::Orchestrator`: a `chat` session is the one a human is
/// talking to directly, so it is the one allowed to hear about delegating to
/// other harnesses.
///
/// A pure function of the adapter and the two pieces of caller-supplied
/// state (the initial prompt, the extra flags), so it is testable without
/// spawning a pty.
pub fn build_launch(
    adapter: &dyn AgentAdapter,
    initial_prompt: Option<&str>,
    extra: &[String],
) -> ChatLaunch {
    let command = adapter.interactive_cmd(initial_prompt, extra);
    let mut argv = vec![command.get_program().to_string_lossy().to_string()];
    argv.extend(command.get_args().map(|a| a.to_string_lossy().to_string()));
    ChatLaunch {
        agent_name: adapter.name().to_string(),
        argv,
        role: PromptRole::Orchestrator,
        verb: super::sessions::Verb::Chat,
    }
}

/// When `adapter` has no verified system-prompt injection mechanism for the
/// launch shape it is about to use, folds the composed session context onto
/// the positional initial-prompt slot as a fallback -- the same task-prompt-
/// text channel `exec.rs`/`run_loop.rs`/`dash::compose_worker_prompt` already
/// use for exactly this adapter shape (see `prompt::task_prompt_with_
/// composed_fallback`'s own doc comment for why the task prompt is the one
/// channel such an adapter has). Whenever the adapter *is* supported, the
/// composed fallback stays a no-op, but the positional prompt still passes
/// through the same masking boundary as the fallback path.
///
/// `adapter.system_prompt_supported(&[])` (an empty launch) is deliberately
/// probed here rather than against the eventual full launch argv: this runs
/// before `build_launch` exists, and `CodexAdapter::system_prompt_supported`'s
/// own contract is to probe `self.interactive_cmd(None, &[])` when handed an
/// empty launch, answering the same question (does *this adapter's own
/// program* resolve to a reparsing shell shim) without needing the real argv
/// in hand -- `compose_worker_prompt` makes the identical `adapter.system_
/// prompt_supported(&[])` call for the same reason, a launch that also does
/// not exist yet at that point.
///
/// `simple`/`cfg.prompt.enabled` are not checked directly here: `compile::
/// compile` already returns `composed: None` for either (mirroring `prompt::
/// compose`'s own gate), and `task_prompt_with_composed_fallback` is a no-op
/// when handed `None`, so both degrade to returning `initial_prompt`
/// unchanged -- the correct answer either way.
///
/// `role` (issue #537 T3) is the seat this launch actually runs as --
/// `PromptRole::Single` for the proxy's own direct/bounded decision,
/// `Orchestrator` for everything else (see `proxy_prompt_role`) -- so this
/// fallback composes the SAME layers the launch's own env/hook plumbing
/// assumes, rather than always hardcoding `Orchestrator`.
#[allow(clippy::too_many_arguments)]
fn orchestrator_initial_prompt(
    adapter: &dyn AgentAdapter,
    initial_prompt: Option<String>,
    cfg: &CtxConfig,
    home: Option<&Path>,
    repo: &Path,
    simple: bool,
    state: &StateDir,
    proxy_layer: Option<&str>,
    role: PromptRole,
) -> Option<String> {
    let text = if adapter.system_prompt_supported(&[]) {
        initial_prompt.unwrap_or_default()
    } else {
        let mut compiled = super::compile::compile(
            home,
            repo,
            simple,
            cfg,
            adapter,
            role,
            state,
            super::state::now_secs(),
            super::adapters::LaunchMode::Interactive,
            false,
        );
        if let Some(task) = initial_prompt
            .as_deref()
            .filter(|task| !task.trim().is_empty())
        {
            super::compile::select_skill_descriptions_for_task(
                &mut compiled,
                cfg,
                state,
                repo,
                home,
                task,
            );
        }
        // Issue #537 (T2a): the harness proxy's own bounded layer, when an
        // active decision took over this launch -- a no-op for every other
        // launch (`proxy_layer` is `None`). Every adapter with verified
        // system-prompt injection gets this layer through `wrap.rs`'s or
        // `dash_orchestrator_pane`'s own `compile::with_proxy_layer` call.
        let compiled = super::compile::with_proxy_layer(compiled, proxy_layer);
        let base = initial_prompt.unwrap_or_default();
        super::prompt::task_prompt_with_composed_fallback(&base, false, compiled.composed.as_ref())
    };
    if text.is_empty() {
        None
    } else {
        match super::obfuscate_store::protect_text(state, repo, cfg, &text, "chat_initial_prompt") {
            Ok(protected) => Some(protected.0),
            Err(error) => {
                // Fail closed: never send the unprotected text. This is
                // rare (obfuscate.mode is off by default) and otherwise
                // silent, so the operator has something to act on rather
                // than a session that quietly opened with no initial task.
                crate::output::warn(format!(
                    "sensitive-data masking failed ({error}); starting without the initial task prompt"
                ));
                None
            }
        }
    }
}

/// The explicit `--agent`, else the configured default, else the registry's
/// own fallback rule -- whose aggregated error (naming every disabled or
/// unready candidate, and why) is the message a chat session with nothing
/// available shows. Refusing here, before `wrap` is ever reached, is what
/// keeps an explicitly named disabled agent from touching the terminal at
/// all: `wrap::run_with` performs the identical `adapters::select` check of
/// its own before opening a pty, so the same refusal holds even if this
/// function's own check were ever bypassed.
///
/// Also returns the `HarnessRule` that picked the adapter, for the launch
/// banner: an explicit `--agent` never reaches `resolve_default`, so that
/// rule cannot come from `DefaultOrigin` alone.
pub(crate) fn resolve_adapter(
    cfg: &CtxConfig,
    requested: Option<&str>,
) -> CtxResult<(Box<dyn AgentAdapter>, HarnessRule)> {
    resolve_adapter_with_presence(cfg, requested, &adapters::liveness_probe)
}

/// [`resolve_adapter`] with the presence oracle passed in rather than read
/// off the ambient `PATH` -- the same seam, and for the same reason, as
/// `adapters::resolve_default_with_presence`'s own doc comment gives: issue
/// #690 made the last arm's answer depend on what this machine has, so a
/// test that reads the real `PATH` proves only what the developer happens to
/// have installed. Threaded through every arm, not just the fallback, so an
/// injected machine state is the whole truth for a test rather than most of
/// it; the explicit and configured arms consult it no more than they did
/// before (`select_with_presence` reaches the oracle only in its own
/// fallback).
pub(crate) fn resolve_adapter_with_presence(
    cfg: &CtxConfig,
    requested: Option<&str>,
    present: &dyn Fn(&str, &str) -> adapters::Liveness,
) -> CtxResult<(Box<dyn AgentAdapter>, HarnessRule)> {
    // `true`, the `adapter_builds_launch` answer every empty command carries
    // (`adapters::select` derives exactly this for a `&[]` caller): `chat`
    // has no wrapped argv at all, so whatever it resolves is a harness zirv
    // itself would launch.
    if requested.is_some() {
        let adapter = adapters::select_with_presence(requested, &[], cfg, true, present)?;
        return Ok((adapter, HarnessRule::Explicit));
    }
    match cfg.agent.as_deref() {
        Some(name) => Ok((
            adapters::select_with_presence(Some(name), &[], cfg, true, present)?,
            HarnessRule::Configured,
        )),
        None => adapters::resolve_default_with_presence(cfg, present).map(|(adapter, origin)| {
            let rule = match origin {
                DefaultOrigin::Configured => HarnessRule::Configured,
                DefaultOrigin::FirstEnabledReady => HarnessRule::FirstEnabledReady,
                DefaultOrigin::FirstInstalledReady { not_found } => {
                    HarnessRule::FirstInstalledReady { not_found }
                }
            };
            (adapter, rule)
        }),
    }
}

/// Every known harness in registry order, alongside whether the gate
/// currently enables it -- the banner's own harness list. A disabled harness
/// still gets a `(name, false)` entry (its glyph tells the operator it is
/// there but off); an *enabled* harness whose binary is confirmed absent
/// (`adapters::adapter_liveness`, the same issue #298 probe the injected
/// roster gates on) is omitted outright rather than rendered `false` --
/// absence, not a green light this session cannot actually use. `ready()`
/// failing, or a probe that cannot reach a confident verdict, both still
/// count as present: the same fail-open posture `harness_roster_lines`
/// already holds to.
fn harness_list(cfg: &CtxConfig) -> Vec<(String, bool)> {
    adapters::ADAPTERS
        .iter()
        .filter_map(|(name, _)| {
            let enabled = cfg.agents.is_enabled(name);
            if enabled {
                let present = match adapters::adapter_liveness(cfg, name, None) {
                    Ok((_, verdict)) => verdict.emits_line(),
                    Err(_) => true,
                };
                if !present {
                    return None;
                }
            }
            Some(((*name).to_string(), enabled))
        })
        .collect()
}

/// `--resume`'s initial prompt: the latest stored handoff, folded in the same
/// words `zirv ctx resume` uses. When nothing is stored, prints a note and
/// starts fresh rather than failing the session outright: `--resume` is a
/// request to continue if there is something to continue from, not a
/// precondition for starting at all.
pub fn resolve_initial_prompt<W: Write>(
    resume_requested: bool,
    state: &StateDir,
    repo: &Path,
    w: &mut W,
    screen_thresholds: &super::screen::Thresholds,
) -> CtxResult<Option<String>> {
    if !resume_requested {
        return Ok(None);
    }
    match handoff::latest_for_repo(state, repo)? {
        // Issue #281: no session id has been minted for this launch yet at
        // this point in `chat`'s own flow (unlike `resume::run_with`, which
        // now mints its session before composing this same prompt) --
        // `resume_prompt`'s `session` parameter is unused by `working_set`
        // today, so an empty string costs nothing real.
        Some((_path, found)) => Ok(Some(resume::resume_prompt(
            state,
            repo,
            "",
            &found,
            screen_thresholds,
        ))),
        None => {
            writeln!(
                w,
                "zirv ctx chat: --resume requested but no handoff is stored for this repo; \
                 starting a fresh session"
            )?;
            Ok(None)
        }
    }
}

/// Issue #537 (T2): what the harness proxy's intake step decided for this
/// launch, evaluated once, before `resolve_adapter`.
#[derive(Debug, PartialEq)]
enum ProxyIntakeOutcome {
    /// The proxy took no part in this launch; `advisory`, when present, is
    /// the one line `run_with` prints on the same `zirv \u{25b8}` channel as
    /// every other announcement (so it still honors `--quiet`).
    Inactive { advisory: Option<String> },
    /// Activation succeeded but stdin is not a terminal, so there is
    /// nowhere to read the task description from: `run_with` refuses the
    /// whole launch with this message rather than silently skipping the
    /// proxy (unlike every other `Inactive` case, this one was never given
    /// a chance to say anything at all).
    Refuse { message: String },
    /// The proxy decided this launch: `request` is the raw text `decide`
    /// classified, carried alongside so it can also become the launch's own
    /// initial prompt. Boxed: `ProxyDecision` is far larger than every other
    /// variant here (clippy's `large_enum_variant`), and this variant is
    /// matched far less often than it is passed around.
    Decided {
        decision: Box<ProxyDecision>,
        request: String,
    },
}

/// One line of text under construction by the operator, tracked as
/// codepoints with an interior edit point (`cursor`, a codepoint index into
/// `chars`) rather than only ever appending at the end. See
/// [`read_edited_line`]'s own doc comment for why this exists at all.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct EditLine {
    chars: Vec<char>,
    cursor: usize,
}

impl EditLine {
    fn text(&self) -> String {
        self.chars.iter().collect()
    }

    /// How many terminal cells the first `upto` codepoints occupy -- NOT how
    /// many codepoints they are. A CJK ideograph or a wide emoji occupies two
    /// cells and a combining mark occupies none, so cursor positioning that
    /// counted codepoints (as this did before review) put the cursor in the
    /// wrong column the moment the line held either. `None` from
    /// `UnicodeWidthChar::width` means a control character, which this editor
    /// never inserts (`apply_key` only ever inserts what `KeyCode::Char`
    /// carries, and the control chords are matched out before it).
    fn cells_upto(&self, upto: usize) -> usize {
        self.chars[..upto.min(self.chars.len())]
            .iter()
            .map(|c| UnicodeWidthChar::width(*c).unwrap_or(0))
            .sum()
    }

    /// Terminal cells the whole line occupies.
    fn cells(&self) -> usize {
        self.cells_upto(self.chars.len())
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
}

/// What one raw key does to an [`EditLine`] in progress -- pure so the
/// mapping from a key to an edit is unit-tested without a real terminal.
/// `Eof`/`Cancel` exist because raw mode (needed to see Left/Right at all --
/// a canonical-mode tty has no concept of them beyond their raw escape
/// bytes) disables `ICANON`/`ISIG` along with it, which otherwise silently
/// takes Ctrl+D's end-of-input and Ctrl+C's interrupt away too; see
/// [`read_edited_line`]'s own doc comment for how each is put back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditAction {
    Edited,
    Submit,
    Eof,
    Cancel,
    Ignored,
}

/// Pure: what pressing `code` (with `modifiers`) does to `line`.
///
/// Ctrl+D only ends input on an EMPTY line. A canonical-mode tty delivers
/// whatever is already typed when `VEOF` arrives mid-line rather than
/// throwing it away (confirmed on a real pty during review), so treating
/// every Ctrl+D as end-of-input -- as this did before review -- silently
/// discarded a line the operator had finished typing but not yet sent.
fn apply_key(line: &mut EditLine, code: KeyCode, modifiers: KeyModifiers) -> EditAction {
    if modifiers.contains(KeyModifiers::CONTROL) {
        return match code {
            KeyCode::Char('c' | 'C') => EditAction::Cancel,
            KeyCode::Char('d' | 'D') => {
                if line.chars.is_empty() {
                    EditAction::Eof
                } else {
                    EditAction::Submit
                }
            }
            _ => EditAction::Ignored,
        };
    }
    let edited = match code {
        KeyCode::Enter => return EditAction::Submit,
        KeyCode::Char(c) => {
            line.insert(c);
            true
        }
        KeyCode::Backspace => line.backspace(),
        KeyCode::Delete => line.delete_forward(),
        KeyCode::Left => line.move_left(),
        KeyCode::Right => line.move_right(),
        KeyCode::Home => line.move_home(),
        KeyCode::End => line.move_end(),
        _ => false,
    };
    if edited {
        EditAction::Edited
    } else {
        EditAction::Ignored
    }
}

/// Pure: where a point `cursor_cells` cells into a line sits, as `(row, col)`
/// relative to the row the line started on, when the terminal is `width`
/// columns wide. Split out of [`redraw_edit_line`] so the wrapping arithmetic
/// -- the part review found wrong, and the part no terminal is needed to
/// check -- is unit-tested directly. `width` of 0 is treated as 1: a
/// zero-width terminal would divide by zero, and one column is the smallest
/// layout that still makes sense to draw into.
fn edit_line_layout(cursor_cells: usize, width: usize) -> (usize, usize) {
    let width = width.max(1);
    (cursor_cells / width, cursor_cells % width)
}

/// Redraws `line`, which may occupy more than one terminal row once it is
/// longer than the terminal is wide.
///
/// `previous_cursor_row` is how many rows below the line's own first row the
/// cursor was left on by the last redraw -- the only state this needs, and
/// the fix for what review found: the old version issued a bare
/// `MoveToColumn(0)` + `Clear(CurrentLine)`, which on a wrapped line returns
/// to the start of whichever row the cursor happens to be on and clears only
/// that row, so every further keystroke reprinted the whole line one row
/// further down. Moving up by the tracked row count first anchors the redraw
/// back at the line's own first row, and `FromCursorDown` then clears every
/// row the previous render used.
///
/// Deliberately relative (move up from wherever the cursor is) rather than
/// absolute (remember the origin row from `cursor::position()`): when the
/// content grows past the bottom of the screen the terminal scrolls, which
/// moves an absolute origin row out from under itself but leaves every
/// relative move still correct.
///
/// Returns the cursor's new row offset, for the next call to pass back in.
fn redraw_edit_line(
    out: &mut impl Write,
    line: &EditLine,
    previous_cursor_row: usize,
    width: u16,
) -> io::Result<usize> {
    let cells = line.cells();
    let cursor_cells = line.cells_upto(line.cursor);
    let (cursor_row, cursor_col) = edit_line_layout(cursor_cells, usize::from(width));

    if previous_cursor_row > 0 {
        crossterm::execute!(out, MoveUp(previous_cursor_row as u16))?;
    }
    crossterm::execute!(
        out,
        MoveToColumn(0),
        Clear(ClearType::FromCursorDown),
        Print(line.text())
    )?;
    // A line that ends exactly on a row boundary leaves the cursor somewhere
    // terminals disagree about -- at the end of the row just filled (deferred
    // wrap, the common behaviour) or at the start of the next one. Printing
    // one space forces the wrap to have happened either way, and erasing it
    // again leaves the screen as if it never did, so the arithmetic below has
    // exactly one cursor position to reason about.
    let width_cells = usize::from(width.max(1));
    if cells > 0 && cells.is_multiple_of(width_cells) {
        crossterm::execute!(out, Print(" "), Clear(ClearType::UntilNewLine))?;
    }
    // The cursor is now on the line's last row; step back up to the row the
    // edit point is on and into its column.
    let last_row = cells / width_cells;
    if last_row > cursor_row {
        crossterm::execute!(out, MoveUp((last_row - cursor_row) as u16))?;
    }
    crossterm::execute!(out, MoveToColumn(cursor_col as u16))?;
    Ok(cursor_row)
}

/// What one call to [`read_edited_line`] produced: a submitted line, or
/// end of input (Ctrl+D) -- Ctrl+C exits the process directly (see that
/// function's own doc comment) rather than surfacing as a third variant
/// here, so every caller of this type only ever has these two to handle,
/// same as a plain `read_line`'s `Some`/`None`.
enum LineOutcome {
    Line(String),
    Eof,
}

/// Reads one line from the operator with a real, relocatable cursor --
/// Left/Right/Home/End actually move the edit point, and Backspace/Delete
/// act on wherever it is -- instead of what bare canonical-mode
/// `stdin().read_line()` gives: appending is the only edit there is, since
/// the tty's own line discipline has no notion of an interior cursor at all
/// (an arrow key's raw escape bytes just get inserted as literal text, or
/// swallowed by whatever the terminal makes of them). That was reported as
/// the harness-proxy intake prompt being stuck in "insert mode".
///
/// Needs raw mode to see Left/Right as `KeyCode`s at all, which as a side
/// effect disables `ISIG`/`ICANON`, so this hand-restores what those
/// otherwise gave for free: a bare Ctrl+C would otherwise do nothing
/// (silently swallowed instead of raising `SIGINT`), so it exits the
/// process itself with 130 (128 + `SIGINT`, the conventional code an
/// interrupted process reports) -- the same outward result canonical mode's
/// own signal delivery always had here. A bare Ctrl+D would otherwise be
/// read back as a literal `KeyCode::Char('d')` instead of ending input, so
/// it maps to [`LineOutcome::Eof`] by hand instead. Raw mode is always
/// disabled again before returning, on every exit path including an I/O
/// error, so a failure here can never leave the terminal stuck in it.
fn read_edited_line() -> io::Result<LineOutcome> {
    enable_raw_mode()?;
    let mut term = io::stderr();
    let mut line = EditLine::default();
    // How far below the line's own first row the cursor was left by the last
    // redraw -- see `redraw_edit_line`, which needs it to anchor a wrapped
    // line's redraw back at the row it started on.
    let mut cursor_row = 0usize;
    let outcome = loop {
        match event::read() {
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                match apply_key(&mut line, key.code, key.modifiers) {
                    EditAction::Submit => break Ok(LineOutcome::Line(line.text())),
                    EditAction::Eof => break Ok(LineOutcome::Eof),
                    EditAction::Cancel => {
                        let _ = disable_raw_mode();
                        let _ = writeln!(term);
                        std::process::exit(130);
                    }
                    EditAction::Edited => {
                        // A terminal that cannot report its width still gets a
                        // usable editor: 80 columns is the conventional
                        // fallback, and the only cost of guessing it wrong is
                        // the wrapped-line redraw this width feeds.
                        let width = crossterm::terminal::size().map_or(80, |(cols, _)| cols);
                        match redraw_edit_line(&mut term, &line, cursor_row, width) {
                            Ok(row) => cursor_row = row,
                            Err(e) => break Err(e),
                        }
                    }
                    EditAction::Ignored => {}
                }
            }
            Ok(_) => {}
            Err(e) => break Err(e),
        }
    };
    let _ = disable_raw_mode();
    if outcome.is_ok() {
        // From wherever the edit point was, drop past the LAST row the line
        // occupies before ending it, so a wrapped line's tail is not
        // overwritten by whatever prints next.
        let width = usize::from(
            crossterm::terminal::size()
                .map_or(80u16, |(cols, _)| cols)
                .max(1),
        );
        let last_row = line.cells() / width;
        if last_row > cursor_row {
            let _ = crossterm::execute!(term, MoveDown((last_row - cursor_row) as u16));
        }
        let _ = writeln!(term);
    }
    outcome
}

/// A `Read` source, meant to be wrapped in a `BufReader` (which then
/// satisfies the `BufRead` [`proxy_intake`] takes), that serves each line
/// from [`read_edited_line`] instead of raw stdin bytes. `proxy::
/// read_request`'s own multi-line-until-blank-or-EOF loop does not change at
/// all -- only where its bytes come from.
struct EditedStdin {
    pending: Vec<u8>,
    pos: usize,
    eof: bool,
}

impl EditedStdin {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
            pos: 0,
            eof: false,
        }
    }
}

impl Read for EditedStdin {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.pending.len() {
            if self.eof {
                return Ok(0);
            }
            self.pending.clear();
            self.pos = 0;
            match read_edited_line()? {
                LineOutcome::Line(text) => {
                    self.pending.extend_from_slice(text.as_bytes());
                    self.pending.push(b'\n');
                }
                LineOutcome::Eof => {
                    self.eof = true;
                    return Ok(0);
                }
            }
        }
        let n = buf.len().min(self.pending.len() - self.pos);
        buf[..n].copy_from_slice(&self.pending[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// The `reader` a real, interactive `proxy_intake` call reads the task
/// description from: [`EditedStdin`] (real cursor editing) when stdin is a
/// terminal capable of rendering the raw-mode escape codes that needs
/// (`vt_ok`), plain stdin otherwise -- an older Windows console without VT
/// processing enabled cannot be assumed to render `redraw_edit_line`'s
/// cursor-movement codes correctly, so it keeps today's append-only
/// behaviour rather than risking a garbled prompt.
fn intake_reader(stdin_is_tty: bool, vt_ok: bool) -> Box<dyn BufRead> {
    // `io::stderr()` is where `read_edited_line` echoes what is typed, so its
    // OWN tty-ness is what decides whether the operator can see the editor at
    // all -- gating on stdin/stdout alone (as this did before review) left
    // `2>file` with a live raw-mode editor echoing into the file and nothing
    // on screen.
    if stdin_is_tty && vt_ok && io::stderr().is_terminal() {
        Box::new(io::BufReader::new(EditedStdin::new()))
    } else {
        Box::new(io::stdin().lock())
    }
}

/// The harness proxy's intake step (issue #537 T2), evaluated before any
/// dashboard/TUI or `wrap` launch and before `resolve_adapter`. Pure of the
/// real terminal/stdin: `stdin_is_tty` and `reader` are both passed in
/// (`run_with` supplies `std::io::stdin()`'s own tty probe and a locked
/// handle onto it), so the decision logic here is testable without one.
///
/// `--simple`/`--resume` always skip the proxy outright -- a resumed
/// session's first prompt is the stored handoff, and `--simple` promises no
/// zirv-injected step at all -- recording why only when `--proxy` was
/// explicitly requested (an operator asking for the proxy and silently not
/// getting it would otherwise look like a bug). Otherwise `proxy::activation`
/// decides (`--proxy`/`--no-proxy` already folded into `cfg.proxy.enabled`
/// by the caller): `Err` skips with that reason as the advisory; `Ok` opens
/// the intake view, refusing outright on a non-tty stdin (there is nowhere
/// to read a request from) and falling back to `Inactive` on an empty
/// request, exactly like every other skip.
fn proxy_intake<E: Write>(
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
    args: &ChatArgs,
    stdin_is_tty: bool,
    reader: &mut dyn BufRead,
    stderr: &mut E,
) -> CtxResult<ProxyIntakeOutcome> {
    if args.simple || args.resume {
        let advisory = args.proxy.then(|| {
            let flag = if args.simple { "--simple" } else { "--resume" };
            format!("proxy: skipped ({flag}); starting the orchestrator harness")
        });
        return Ok(ProxyIntakeOutcome::Inactive { advisory });
    }
    if let Err(reason) = proxy::activation(cfg) {
        // Issue #537 review: the plain `[proxy] enabled = false` default --
        // no flag either way -- must stay byte-identical to today,
        // announcements included. `cfg.proxy.enabled` here already has
        // `--proxy`/`--no-proxy` folded in by the caller, so this is exactly
        // "enabled (by config or --proxy) but not usable"; the disabled
        // default (or an explicit `--no-proxy`) prints nothing.
        let advisory = cfg.proxy.enabled.then_some(reason);
        return Ok(ProxyIntakeOutcome::Inactive { advisory });
    }
    if !stdin_is_tty {
        return Ok(ProxyIntakeOutcome::Refuse {
            message: "zirv ctx chat: the harness proxy needs an interactive terminal on stdin to \
                      read the task description; pass --no-proxy (or disable [proxy]) to skip it"
                .to_string(),
        });
    }
    writeln!(
        stderr,
        "zirv \u{25b8} proxy: describe the task (empty line to send)"
    )?;
    // Issue #701 (operator field report): a blank FIRST line used to skip the
    // proxy outright, and that skip is invisible -- the advisory below is
    // wiped by the harness's own alternate screen a moment later, so the
    // launch looks exactly like one where the proxy never ran at all. Two
    // ordinary things produce that blank line: the reflexive Enter at a
    // prompt an operator did not expect, and a stray newline left in the
    // console input buffer by a line editor (clink on `cmd.exe` here). Ask
    // once more, naming both ways out; only a SECOND blank line (or EOF)
    // skips, so a deliberate skip still costs one keypress.
    let request = match proxy::read_request(reader) {
        Some(request) => request,
        None => {
            writeln!(
                stderr,
                "zirv \u{25b8} proxy: nothing typed -- describe the task, or press Enter again to \
                 start the harness without the proxy"
            )?;
            match proxy::read_request(reader) {
                Some(request) => request,
                None => {
                    return Ok(ProxyIntakeOutcome::Inactive {
                        advisory: Some(
                            "proxy: no request given; starting the orchestrator harness"
                                .to_string(),
                        ),
                    });
                }
            }
        }
    };
    // Issue #537 review (operator field report): nothing on screen showed
    // that the request was actually sent to the configured decider, so a
    // slow or falling-back `decide()` looked identical to a hung session.
    // Same `zirv \u{25b8}` channel and `--quiet` gate every other proxy
    // advisory uses, printed immediately before the call it describes.
    super::announce::Announcer::new(
        cfg.chrome.events && !args.quiet,
        console::colors_enabled_stderr(),
    )
    .emit_to(
        stderr,
        &super::announce::Event::ProxyAdvisory {
            text: proxy::asking_line(cfg),
        },
    );
    let decision = proxy::decide(cfg, state.root(), repo, &request);
    let (decision, request) = maybe_clarify(cfg, state, repo, decision, request, reader, stderr)?;
    Ok(ProxyIntakeOutcome::Decided {
        decision: Box::new(decision),
        request,
    })
}

/// Issue #537 (A2): one round of interactive follow-up when `decision.
/// needs_clarification` is at or above `proxy::CLARIFY_THRESHOLD` AND
/// `decision.needs_clarification_decisive` (the margin gate `proxy::decision
/// ::merge` applied -- a confident-looking but thin-margin "ambiguous"
/// reading must not interrupt a launch on its own) -- prints one prompt
/// (unconditional, same as `proxy_intake`'s own "describe the task" prompt
/// right above: this blocks on stdin, so it must stay visible regardless of
/// `--quiet`) and reads one line. An empty line (or EOF)
/// leaves `decision`/`request` untouched -- an operator who has nothing to
/// add is not forced to add anything. A non-empty line is appended to
/// `request` (separated by a blank line, so the harness's own first prompt
/// still reads as one coherent task) and `decide` runs exactly once more --
/// never a second clarification round, however ambiguous the new decision
/// still looks.
fn maybe_clarify<E: Write>(
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
    decision: ProxyDecision,
    request: String,
    reader: &mut (impl BufRead + ?Sized),
    stderr: &mut E,
) -> CtxResult<(ProxyDecision, String)> {
    if decision.needs_clarification < proxy::CLARIFY_THRESHOLD
        || !decision.needs_clarification_decisive
    {
        return Ok((decision, request));
    }
    let clarification = match decision.clarification_category.as_deref() {
        Some("target") => {
            "Which service or files should change? Add detail and press Enter, or press Enter to launch as is:"
        }
        Some("behavior") => {
            "What should happen when the change is complete? Add detail and press Enter, or press Enter to launch as is:"
        }
        Some("constraint") => {
            "Which constraint or compatibility requirement must hold? Add detail and press Enter, or press Enter to launch as is:"
        }
        _ => "Add detail and press Enter, or press Enter to launch as is:",
    };
    writeln!(
        stderr,
        "zirv \u{25b8} proxy: the request looks ambiguous ({:.2}). {clarification}",
        decision.needs_clarification,
    )?;
    let record_clarification = |action| {
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
    };
    record_clarification("requested");
    let mut line = String::new();
    let read = reader.read_line(&mut line).unwrap_or(0);
    let addition = line.trim_end_matches(['\n', '\r']);
    if read == 0 || addition.trim().is_empty() {
        record_clarification("unanswered");
        return Ok((decision, request));
    }
    record_clarification("answered");
    let combined_request = format!("{request}\n\n{addition}");
    let combined_decision = proxy::decide(cfg, state.root(), repo, &combined_request);
    Ok((combined_decision, combined_request))
}

/// The chat-launch overrides an active harness-proxy decision applies:
/// `cfg.chat.model` (so `extra_with_model`/`SEAT_MODEL_ENV`/the banner all
/// disclose the SAME model the proxy chose, through the exact seams that
/// already carry an operator-configured `chat.model` today) and the
/// requested adapter name, returned for the caller to fold into
/// `resolve_adapter` in place of `--agent`. Pure and given an already-
/// computed [`ProxyDecision`] (not `decide` itself), so the effect on a
/// launch's argv is testable without a pty, a real decider, or a live
/// `proxy::decide` call.
fn apply_proxy_decision(cfg: &mut CtxConfig, decision: &ProxyDecision) -> String {
    cfg.chat.model = Some(decision.orchestrator.model.clone());
    decision.orchestrator.harness.clone()
}

/// Issue #537 (T3, operator field report): the `PromptRole` this launch's own
/// prompt/env/hook plumbing runs as. `SeatRole::Single` (a `direct`/`bounded`
/// decision -- one seat working alone) launches as `PromptRole::Single`
/// rather than today's hardcoded `Orchestrator`, so it never receives the
/// harness's orchestrator conventions, the operator's orchestrator `system-
/// prompt.md`, or the write-guard denial those imply (see `PromptRole::
/// Single`'s own doc comment). `SeatRole::Orchestrator` and every outcome
/// that never decided (today's launch, unchanged) keep `Orchestrator`.
fn proxy_prompt_role(intake: &ProxyIntakeOutcome) -> PromptRole {
    match intake {
        ProxyIntakeOutcome::Decided { decision, .. } if decision.seat_role == SeatRole::Single => {
            PromptRole::Single
        }
        _ => PromptRole::Orchestrator,
    }
}

/// Issue #703 (follow-up to #702): the model half of the same decision
/// `proxy_prompt_role` already reads the seat role from. `apply_proxy_
/// decision`'s `cfg.chat.model` seam (which flows into `extra_with_model`,
/// the harness argv and the banner) has no equivalent on the native path --
/// there is no `--model` flag and no harness argv to fold one into. This
/// hands `native_pane_spec` the decided model as-is, for `NativeDashboardSpec
/// ::route`: `NativePaneRuntime::spawn` is what validates it against the
/// operator's own native provider configuration and falls back to the role's
/// default route for anything that is not an existing, policy-allowed route
/// name, rather than failing the launch the way an unresolvable explicit
/// `--route` would. `None` for every outcome but `Decided`, exactly like
/// `proxy_prompt_role`, which leaves the pane's route untouched.
fn proxy_decided_model(intake: &ProxyIntakeOutcome) -> Option<String> {
    match intake {
        ProxyIntakeOutcome::Decided { decision, .. } => Some(decision.orchestrator.model.clone()),
        _ => None,
    }
}

/// The harness proxy's own bounded `[zirv proxy]` layer text
/// (`proxy::prompt_layer`), when [`proxy_intake`] decided this launch;
/// `None` for every other outcome. Computed once and threaded to every
/// place that needs it (`orchestrator_initial_prompt`'s fallback,
/// `dash_orchestrator_pane`, `wrap_args_for`), so the launch shape actually
/// taken can never disagree with the others about it.
///
/// `started_workflow_id` (change 2, wrapper-overhead benchmark): the id
/// [`start_proxy_workflow`] actually started for this SAME launch, when it
/// did -- forwarded straight to `proxy::prompt_layer` so the workflow line
/// names the concrete running instance, not only its kind. It must come
/// from a workflow start that ran BEFORE this function, which is why
/// [`start_proxy_workflow`] is now called once at the top of `run_with`,
/// ahead of every place that reads this text.
fn proxy_layer_text(
    intake: &ProxyIntakeOutcome,
    started_workflow_id: Option<&str>,
) -> Option<String> {
    match intake {
        ProxyIntakeOutcome::Decided { decision, .. } => {
            Some(proxy::prompt_layer(decision, started_workflow_id))
        }
        _ => None,
    }
}

/// Starts the proxy's chosen workflow (when [`proxy_intake`] decided one),
/// once, before any launch shape's own prompt is built -- so the started id
/// can reach [`proxy_layer_text`] and, through it, every one of the three
/// launch shapes (change 2, wrapper-overhead benchmark field evidence: a
/// workflow started in 27/36 replayed runs, never consulted, because
/// nothing named the running instance or what to do with it). A no-op
/// (`None`) when the intake never took over this launch.
///
/// Issue #537 review: never silently discards an outcome the operator has
/// no other way to learn about. `announce` is called through the exact same
/// `zirv \u{25b8}` channel every other proxy line uses (a `Skipped` start or
/// a `start_workflow_for` error); a `Started` workflow stays silent here,
/// same as today -- it is only ever reported through the prompt layer or,
/// on a failed spawn, [`close_proxy_workflow_on_failure`].
fn start_proxy_workflow(
    outcome: &ProxyIntakeOutcome,
    state: &StateDir,
    repo: &Path,
    mut announce: impl FnMut(String),
) -> Option<String> {
    let ProxyIntakeOutcome::Decided { decision, request } = outcome else {
        return None;
    };
    match proxy::launch::start_workflow_for(decision, state.root(), repo, request) {
        Ok(proxy::launch::WorkflowStart::Started { id }) => Some(id),
        Ok(proxy::launch::WorkflowStart::Skipped { reason }) => {
            announce(format!("proxy: workflow not started; {reason}"));
            None
        }
        Err(err) => {
            announce(format!("proxy: workflow start failed; {err}"));
            None
        }
    }
}

/// Runs `spawn`, closing `started_id` (when [`start_proxy_workflow`] named
/// one) with `"proxy launch failed"` if `spawn` itself returns `Err` -- so a
/// failed launch never leaves an orphaned workflow reported as this
/// repository's active one forever. A plain pass-through to `spawn` when no
/// workflow was started (nothing to close either way). Shared by every
/// launch shape below that can actually be the LAST one attempted -- the
/// dashboard pane (pane build included) and the `wrap` fallback -- each
/// called with the SAME `started_id` from the one [`start_proxy_workflow`]
/// call at the top of `run_with`. The persistent-runtime spawn is
/// deliberately NOT wrapped: it always falls through to one of the other two
/// on failure, so closing the workflow there would leave that live prompt
/// text naming a workflow this same function had just closed.
fn close_proxy_workflow_on_failure<T>(
    started_id: Option<&str>,
    state: &StateDir,
    repo: &Path,
    mut announce: impl FnMut(String),
    spawn: impl FnOnce() -> CtxResult<T>,
) -> CtxResult<T> {
    let result = spawn();
    if result.is_err()
        && let Some(id) = started_id
        && let Err(close_error) =
            proxy::launch::close_started(state.root(), repo, id, "proxy launch failed")
    {
        announce(format!(
            "proxy: could not close workflow {id} after the failed launch; {close_error} -- \
             run `zirv workflow close {id}` yourself"
        ));
    }
    result
}

/// The dashboard launch shape of `run_with` (its `chrome::dash_eligible`
/// branch): builds the orchestrator pane and runs the dashboard, both
/// wrapped in the SAME [`close_proxy_workflow_on_failure`] call -- extracted
/// (review fix, finding 2) so an `Err` from `dash_orchestrator_pane` itself
/// closes `started_workflow_id` exactly like a failure inside `dash::
/// run_dashboard` already did; before this fix the pane build sat outside
/// the wrapper's own `?`, orphaning the started workflow (the engine never
/// overwrites an active pointer, so it would block every later `zirv chat`
/// on this repo). This is also the launch shape that actually runs once
/// `dash_eligible` is true -- nothing after it in `run_with` reuses
/// `started_workflow_id` -- which is why it, like the `wrap` fallback, is
/// wrapped at all (contrast the persistent-runtime attempt just above it in
/// `run_with`, which never is: see [`close_proxy_workflow_on_failure`]'s own
/// doc comment).
///
/// A free function taking every input explicitly, rather than inlined in
/// `run_with`, so a test can drive this exact composition directly:
/// `chrome::dash_eligible` requires a real interactive terminal on both
/// streams, which `cargo test`'s piped stdio never provides.
#[allow(clippy::too_many_arguments)]
fn run_dash_branch(
    adapter: &dyn AgentAdapter,
    launch: ChatLaunch,
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
    env: EnvLookup<'_>,
    session: &str,
    simple: bool,
    proxy_layer: Option<&str>,
    task: Option<&str>,
    started_workflow_id: Option<&str>,
    force_pace: bool,
    announce: impl FnMut(String),
) -> CtxResult<i32> {
    close_proxy_workflow_on_failure(started_workflow_id, state, repo, announce, || {
        let pane = dash_orchestrator_pane_with_task(
            adapter,
            launch,
            cfg,
            state,
            repo,
            session,
            simple,
            proxy_layer,
            task,
        )?;
        // Issue #753: marks the first pane as proxy-decided (see
        // `adapters::PROXY_DECIDED_ENV`); every other key reads through.
        let proxied = |key: &str| {
            if proxy_layer.is_some() && key == super::adapters::PROXY_DECIDED_ENV {
                Some("1".to_string())
            } else {
                env(key)
            }
        };
        dash::run_dashboard(cfg, repo, &proxied, state, pane, None, force_pace)
    })
}

/// Probes stdout for the launch banner: whether it is a terminal at all, its
/// current size, and whether VT output could be enabled. The returned guard
/// must outlive the whole session -- it is what keeps VT processing on for
/// `wrap`'s own raw-mode session that follows -- so callers hold it rather
/// than letting it drop immediately.
///
/// `stdout_is_tty` comes from `IsTerminal` on stdout specifically, not from
/// whether `window_size` succeeded: on unix that call reads `STDIN_FD`'s own
/// terminal-ness, so `zirv chat > log` -- stdout redirected, stdin still a
/// real terminal -- used to still print the banner straight into the log
/// file. The size itself still has to come from `window_size`: it is the
/// only source for it either way.
///
/// `stdin_is_tty` is a second, independent probe (not derived from
/// `window_size` succeeding): `ChromeCaps::probe` never needed it -- the
/// banner and status bar only ever write to stdout -- but `dash_eligible`
/// does, since the dashboard reads keystrokes from stdin to drive pane
/// selection and overlays, and a piped stdin can never make a usable session
/// even when stdout happens to be a terminal.
fn probe_terminal() -> (bool, bool, bool, (u16, u16), Option<term::VtGuard>) {
    let stdout_is_tty = std::io::stdout().is_terminal();
    let stdin_is_tty = std::io::stdin().is_terminal();
    let size = term::window_size(term::STDIN_FD).unwrap_or((0, 0));
    let vt_guard = term::enable_vt_output().ok();
    let vt_ok = vt_guard.is_some();
    (stdout_is_tty, stdin_is_tty, vt_ok, size, vt_guard)
}

/// Issue #540: set by `main.rs`'s `zirv native` alias rewrite
/// (`rewrite_native_alias_args`) on the process environment, immediately
/// before it calls `ctx::dispatch` -- never by an operator directly. Read
/// back here (through the same `EnvLookup` closure every other environment
/// signal in this function already goes through, e.g. `quiet_env`'s
/// `ZIRV_CTX_QUIET`) so `run_native_chat` can tell the `zirv native` alias
/// apart from an explicit `zirv chat --runtime native`, even though both
/// launch through this exact same function -- there is no second native
/// launch path anywhere for the alias to have its own copy of. An argv-based
/// signal (a hidden flag on `ChatArgs`) was the alternative; the environment
/// was chosen because it needs no new clap surface on a struct an operator's
/// own `--help` already renders, and it keeps `command_schema.rs`'s
/// `zirv chat` flag list identical to what an operator can actually pass.
pub const NATIVE_ALIAS_ENV: &str = "ZIRV_CTX_NATIVE_ALIAS";

/// The one-time, low-noise notice `run_native_chat` prints on `stderr` when
/// launched through the `zirv native` alias (see [`NATIVE_ALIAS_ENV`]) --
/// never for an explicit `zirv chat --runtime native`, and never repeated
/// per turn or folded into the model's own context.
pub const NATIVE_ALIAS_BANNER: &str =
    "zirv native is experimental; `zirv chat` remains the stable harness.";

/// Shared verbatim between `run_native_chat`'s own refusal and `zirv native
/// --help`'s prose (`main.rs`'s `native_help_text`), so the two descriptions
/// of the same limitation can never drift apart.
pub const NATIVE_WRAPPED_ONLY_FLAGS_REFUSAL: &str = "--runtime native accepts no --agent, --simple, --resume, --pin-harness or trailing \
     arguments -- those are wrapped-harness-only";

/// Same sharing as [`NATIVE_WRAPPED_ONLY_FLAGS_REFUSAL`], for the TTY
/// requirement.
pub const NATIVE_TTY_REFUSAL: &str =
    "zirv chat --runtime native needs an interactive terminal on both stdin and stdout";

/// `zirv native --help`'s own text (`main.rs` prints this verbatim and exits
/// 0 for `zirv native --help`/`-h`, before the argv rewrite, so this never
/// falls through to clap's generated help for the ordinary `chat` verb tree,
/// which does not mention any of this). Syntax, prerequisites, limitations
/// and where state/journal live, plus a prominent experimental notice --
/// the limitations reuse [`NATIVE_WRAPPED_ONLY_FLAGS_REFUSAL`]/
/// [`NATIVE_TTY_REFUSAL`] verbatim rather than restating them, so this text
/// and `run_native_chat`'s own refusals can never drift apart.
pub fn native_help_text() -> String {
    format!("{}\n", runtime_kind::NATIVE_COMING_SOON)
}

/// `zirv chat --runtime native`'s own refusal/dispatch, split out of
/// `run_with` so the wrapped-harness path above it never has to know this
/// branch exists. Refuses a runtime value this build does not recognize and
/// every wrapped-harness-only flag (`--agent`, `--simple`, `--resume`, a
/// trailing `extra` argv) rather than silently ignoring them -- a flag that
/// looks accepted but does nothing is worse than a refusal that says why.
#[allow(clippy::too_many_arguments)]
fn run_native_chat<E: Write>(
    runtime: &str,
    cfg: &CtxConfig,
    repo: &Path,
    env: EnvLookup<'_>,
    stderr: &mut E,
    args: &ChatArgs,
    stdout_is_tty: bool,
    stdin_is_tty: bool,
    vt_ok: bool,
) -> CtxResult<i32> {
    runtime_kind::require_native_available()?;
    // Issue #540: printed exactly once -- this function runs once per
    // process invocation -- and only for the `zirv native` alias spelling,
    // never for a direct `zirv chat --runtime native` (which never sets
    // `NATIVE_ALIAS_ENV`). Before the runtime/flag/TTY checks below, so an
    // operator sees it even when the launch goes on to refuse for some other
    // reason -- the notice is about which spelling was used, not about
    // whether the launch succeeds.
    if env(NATIVE_ALIAS_ENV).as_deref() == Some("true") {
        writeln!(stderr, "{NATIVE_ALIAS_BANNER}")?;
    }
    // Review finding (issue #540): the read above is the ONE consumer of
    // this signal -- clear it from the real process environment immediately
    // afterward, whether or not it was set, so it never outlives that one
    // read. Left set, every child this session spawns onward (`wrap.rs`'s
    // harness PTY, `dash/pane.rs`'s worker panes) would inherit it too,
    // since neither clears the environment before spawning; harmless today
    // (nothing else reads this key), but a latent trap for a future reader
    // who adds one.
    //
    // SAFETY: this still runs before `dash::run_dashboard`/`wrap::run_with`
    // below have spawned anything or handed control to another thread --
    // `main.rs`'s own `set_var` call (this key's only writer) already
    // documents why the environment is not read or written concurrently
    // this early in the process, and nothing between that call and this one
    // has changed that.
    unsafe {
        std::env::remove_var(NATIVE_ALIAS_ENV);
    }
    // Issue #531 review: this used to reimplement the harness/native decision
    // inline. Routing through `runtime::selected()` makes it the one place a
    // `--runtime` flag is turned into a decision, and reusing its own error
    // text for a value it has never heard of keeps the two from drifting.
    match runtime_kind::selected(runtime) {
        Ok(RuntimeKind::Native) => {}
        Ok(_) => {
            writeln!(
                stderr,
                "--runtime '{runtime}': expected `native` (omit --runtime for a wrapped harness)"
            )?;
            return Ok(1);
        }
        Err(error) => {
            writeln!(stderr, "{error}")?;
            return Ok(1);
        }
    }
    if args.agent.is_some()
        || args.simple
        || args.resume
        || args.pin_harness
        || !args.extra.is_empty()
    {
        writeln!(stderr, "{NATIVE_WRAPPED_ONLY_FLAGS_REFUSAL}")?;
        return Ok(1);
    }
    if !(stdout_is_tty && stdin_is_tty && vt_ok) {
        writeln!(stderr, "{NATIVE_TTY_REFUSAL}")?;
        return Ok(1);
    }
    // `run_with`'s own nesting refusal (F2) already ran, before `cfg` was
    // even loaded, and covers this branch too -- not repeated here.
    let state = StateDir::resolve(env)?;
    // Issue #537 (T2/T3, native seam): the harness proxy's own intake used
    // to be reachable ONLY from the wrapped path below (`run_with`'s own
    // call, after `resolve_adapter`'s ChromeCaps/adapter setup) -- a native
    // launch never ran it at all, and unconditionally hardcoded the
    // `Orchestrator` seat below even with the proxy enabled and answering
    // correctly. Called here instead -- after every earlier refusal above
    // (bogus `--runtime`, a wrapped-only flag, no TTY), exactly where the
    // wrapped path calls it relative to ITS OWN earlier refusals -- so a
    // native launch honors the same decision through the same guards; see
    // `proxy_intake`'s own doc comment for the full skip/refuse/decide
    // sequence.
    let intake = proxy_intake(
        cfg,
        &state,
        repo,
        args,
        stdin_is_tty,
        &mut *intake_reader(stdin_is_tty, vt_ok),
        stderr,
    )?;
    if let ProxyIntakeOutcome::Refuse { message } = &intake {
        writeln!(stderr, "{message}")?;
        return Ok(1);
    }
    // Issue #537 (T3): the seat this launch actually runs as -- `Single` for
    // the proxy's own direct/bounded decision, `Orchestrator` for every
    // other outcome -- the SAME mapping `run_with` applies for the wrapped
    // path, reused rather than re-derived. See `native_pane_spec` for where
    // it lands.
    let seat_role = proxy_prompt_role(&intake);
    // Issue #490 (roadmap N21 item A): the native conversation is the FIRST
    // PANE of the ordinary dashboard now, not a loop of its own. Everything
    // the dashboard already provides -- the sidebar roster, the mail sweep,
    // attention, the spawn-request channel, the restore roster, the footer
    // spend -- therefore applies to it unchanged, and a wrapped harness pane
    // can be spawned beside it in the same process.
    let session = uuid::Uuid::new_v4().to_string();
    // Issue #703: the proxy's decided model, threaded alongside the seat role
    // it was decided together with -- see `proxy_decided_model`'s own doc
    // comment for why this is a route CANDIDATE rather than a guaranteed one.
    let model = proxy_decided_model(&intake);
    let (pane_spec, native_spec) = native_pane_spec(repo, session, seat_role, model);
    dash::run_dashboard(
        cfg,
        repo,
        env,
        &state,
        pane_spec,
        Some(native_spec),
        args.force_pace,
    )
}

/// Issue #537 (T3, native seam): the native pane's own seat spec -- both
/// `PaneSpec.role` and `NativeDashboardSpec.role` (its string form, via
/// `PromptRole::label`, the same inverse `dash::mod.rs`'s worker spawn path
/// already uses for a `requested_role`) come from the ONE `seat_role`
/// `run_native_chat` already resolved through `proxy_prompt_role` -- never a
/// hardcoded `Orchestrator` literal. Split out of `run_native_chat` so the
/// mapping is testable without a real TTY or an actual native session:
/// `dash::run_dashboard` below it is an interactive loop that would
/// otherwise need one.
///
/// Issue #703: `model` (from `proxy_decided_model`) lands on
/// `NativeDashboardSpec::route` -- the SAME field `dash::mod.rs`'s worker
/// spawn path already feeds a delegation's own model alias into. `None`
/// (every outcome but a decided proxy launch) reproduces today's behaviour
/// exactly: the role's own configured default route, untouched.
fn native_pane_spec(
    repo: &Path,
    session: String,
    seat_role: PromptRole,
    model: Option<String>,
) -> (dash::PaneSpec, dash::native_pane::NativeDashboardSpec) {
    (
        dash::PaneSpec {
            agent_name: super::runtime::RuntimeKind::Native.as_str().to_string(),
            argv: Vec::new(),
            role: seat_role,
            verb: super::sessions::Verb::Chat,
            session_id: session,
            title: "orch".to_string(),
        },
        dash::native_pane::NativeDashboardSpec {
            repo: repo.to_path_buf(),
            role: seat_role.label().to_string(),
            route: model,
            writing: true,
            provider: None,
            seat: None,
            initial_input: None,
        },
    )
}

/// `stderr` is a second, explicit writer -- not `std::io::stderr()` reached
/// for directly -- so the one diagnostic this function ever prints on its
/// own (the no-adapter/config error below) stays testable the same way
/// every message on `w` already is, without resorting to capturing the real
/// process stream.
pub fn run_with<W: Write, E: Write>(
    args: &ChatArgs,
    w: &mut W,
    stderr: &mut E,
    repo: &Path,
    env: EnvLookup<'_>,
) -> CtxResult<i32> {
    if args
        .runtime
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case("native"))
    {
        runtime_kind::require_native_available()?;
    }
    // F2, first of all: before any config load, adapter resolution, terminal
    // probe or VT mode change. A `chat` started inside an existing agent
    // session can take that outer session down (see
    // `sessions::nested_session_evidence`), so it must refuse without having
    // touched the shared console at all.
    //
    // Printed on `stderr` and reported as exit code 1 rather than returned
    // as an `Err`, matching every other refusal in this function: a returned
    // `Err` would be printed a second time, unstyled, by `ctx`'s own
    // dispatch. `wrap::run_with` re-checks this independently (it has no
    // writer of its own, so it returns the `Err` there), which is what keeps
    // the guard holding even if this path were bypassed -- and why
    // `allow_nested` has to be threaded into `WrapArgs` below.
    if let Some(refusal) = super::sessions::nesting_refusal("chat", env, args.allow_nested) {
        writeln!(stderr, "{refusal}")?;
        return Ok(1);
    }

    let mut cfg = CtxConfig::load_for_launch(repo, env)?;
    // Issue #537 (T2): `--proxy`/`--no-proxy` override `[proxy] enabled` for
    // this one launch only -- the rest of the activation predicate (the
    // configured decider, its own credential/model checks) is untouched, so
    // an operator cannot use the flag to bypass those.
    if args.proxy {
        cfg.proxy.enabled = true;
    } else if args.no_proxy {
        cfg.proxy.enabled = false;
    }
    // Held for the rest of this function: dropping it early would restore
    // the console's original VT mode before `wrap`'s own raw-mode session
    // (which relies on VT already being on) even opens.
    let (stdout_is_tty, stdin_is_tty, vt_ok, size, _vt_guard) = probe_terminal();

    // Issue #480 (roadmap N11): `--runtime native` branches out to the
    // structured native pane before any of the wrapped-harness-ONLY setup
    // below (adapter resolution, `ChromeCaps`, `dash_eligible`) -- none of
    // it applies to a session with no coding harness and no PTY. `_vt_guard`
    // stays in scope across this call (it is a `let`-bound local of this
    // same function, not dropped until `run_with` itself returns), so the
    // native pane's own `ratatui`/`crossterm` setup sees the same VT mode
    // `wrap`'s raw-mode session would have.
    //
    // Issue #537 (T2/T3, native seam): the harness proxy's own intake used
    // to be reachable ONLY below, once this function had already committed
    // to the wrapped path -- a native launch never ran it at all, and
    // `run_native_chat` always started as a hardcoded `Orchestrator` seat
    // even with the proxy enabled and answering correctly. `run_native_chat`
    // now calls `proxy_intake` itself, after its own earlier refusals
    // (bogus `--runtime`, a wrapped-only flag, no TTY) and before building
    // its pane spec -- see that function's own doc comment and
    // `native_pane_spec`. Nothing here has to change to make that happen:
    // `native` is still decided purely from `args.runtime`/`configured`,
    // with no state or proxy dependency of its own.
    //
    // Issue #491 (roadmap N22): with no `--runtime` at all, the operator's
    // own `[runtime]` table decides, through the same `runtime::resolve`
    // ladder `exec`/`agent` use, at the `orchestrator` role this seat runs
    // as. An unconfigured table resolves to the harness, so the wrapped path
    // below stays the behaviour of every build before N22.
    let configured = runtime_kind::resolve(
        args.runtime.as_deref().unwrap_or(runtime_kind::CONFIGURED),
        &cfg.runtime,
        "orchestrator",
    );
    if !runtime_kind::native_available() {
        configured.as_ref().map_err(|error| error.to_string())?;
    }
    if let Ok(choice) = &configured
        && let Some(note) = &choice.note
    {
        writeln!(stderr, "zirv chat: {note}")?;
    }
    // Issue #593 (roadmap N22): an explicit `--runtime` always wins over the
    // configured default -- including an explicit `--runtime harness`, which
    // must launch the ordinary wrapped chat even when `[runtime] default =
    // "native"`. Only the ABSENCE of the flag falls back to `configured`
    // (which already folds in `[runtime.roles]`/`[runtime] default`). An
    // explicit value this build does not recognise (anything but `harness`)
    // still routes into `run_native_chat`, which re-validates it through
    // `runtime_kind::selected` and reuses that function's own error text.
    let native = match args.runtime.as_deref() {
        Some(flag) => !flag.eq_ignore_ascii_case(RuntimeKind::Harness.as_str()),
        None => configured.is_ok_and(|choice| choice.kind == RuntimeKind::Native),
    };
    if native {
        return run_native_chat(
            args.runtime
                .as_deref()
                .unwrap_or(RuntimeKind::Native.as_str()),
            &cfg,
            repo,
            env,
            stderr,
            args,
            stdout_is_tty,
            stdin_is_tty,
            vt_ok,
        );
    }

    let chrome = ChromeCaps::probe(stdout_is_tty, vt_ok, size, &cfg.chrome, args.simple, false);
    let state = StateDir::resolve(env)?;

    // Issue #537 (T2): the harness proxy's own intake, before any adapter
    // resolution or dashboard/wrap launch -- see `proxy_intake`'s own doc
    // comment for the full skip/refuse/decide sequence.
    let intake = proxy_intake(
        &cfg,
        &state,
        repo,
        args,
        stdin_is_tty,
        &mut *intake_reader(stdin_is_tty, vt_ok),
        stderr,
    )?;
    if let ProxyIntakeOutcome::Refuse { message } = &intake {
        writeln!(stderr, "{message}")?;
        return Ok(1);
    }
    let proxy_announcer = super::announce::Announcer::new(
        cfg.chrome.events && !args.quiet,
        console::colors_enabled_stderr(),
    );
    // Issue #537: the decided orchestrator harness overrides `--agent`
    // outright when the proxy took over this launch (via `apply_proxy_
    // decision`, which also folds the decided model into `cfg.chat.model`);
    // every other case keeps today's `--agent`/configured/first-ready
    // resolution untouched.
    let mut requested_agent = args.agent.clone();
    match &intake {
        ProxyIntakeOutcome::Inactive {
            advisory: Some(reason),
        } => {
            proxy_announcer.emit_to(
                stderr,
                &super::announce::Event::ProxyAdvisory {
                    text: reason.clone(),
                },
            );
        }
        ProxyIntakeOutcome::Decided { decision, .. } => {
            requested_agent = Some(apply_proxy_decision(&mut cfg, decision));
            proxy_announcer.emit_to(
                stderr,
                &super::announce::Event::ProxyAdvisory {
                    text: proxy::announce_line(decision),
                },
            );
        }
        ProxyIntakeOutcome::Inactive { advisory: None } | ProxyIntakeOutcome::Refuse { .. } => {}
    }

    let (adapter, rule) = match resolve_adapter(&cfg, requested_agent.as_deref()) {
        Ok(found) => found,
        Err(err) => {
            // Printed once, here, rather than propagated as `Err`: `zirv
            // ctx`'s own top-level dispatch prints any returned `Err` a
            // second time, unstyled, through `output::error`. Styling only
            // when `chrome.colour` (not gating whether this prints at all
            // on `chrome.banner`, an old bug -- a piped or redirected run
            // still needs to see why it refused to start) and returning
            // `Ok(1)` instead is what keeps this to one printed copy;
            // main.rs's own early-exit branches use the same shape,
            // printing their own message and choosing the exit code
            // directly rather than bubbling an error up to be printed
            // again.
            //
            // On `stderr`, not `w`: `w` is stdout, and `zirv chat > log`
            // must still show the operator *something* on the terminal
            // when it refuses to start, exactly like `output::error`
            // elsewhere in this codebase -- an error silently landing only
            // in a redirected stdout file is indistinguishable from a
            // session that hung or was killed.
            writeln!(
                stderr,
                "{}",
                chrome::style_no_adapter_error(&err.to_string(), chrome.colour)
            )?;
            return Ok(1);
        }
    };
    // Issue #537: the decided request stands in for `--resume`'s own
    // initial-prompt resolution -- both name what the first prompt should
    // be, and the two never coexist (`proxy_intake` always skips under
    // `--resume`, so `ProxyIntakeOutcome::Decided` and a real `--resume`
    // request never race for this slot).
    let initial_prompt = match &intake {
        ProxyIntakeOutcome::Decided { request, .. } => Some(request.clone()),
        _ => resolve_initial_prompt(args.resume, &state, repo, w, &cfg.screen.thresholds())?,
    };
    let resuming = args.resume && initial_prompt.is_some();
    let session = SessionId::new_v4();

    // Bug B (harness parity): an adapter with no verified system-prompt
    // injection mechanism for this launch shape (codex's own `system_prompt_
    // supported` narrows to `false` on a Windows shell-shim launch) never
    // reaches `injection_args_for_session`'s output at all. `dash_
    // orchestrator_pane` and `wrap::run_with`'s own fallback below both
    // correctly skip that call for such an adapter, but neither has anywhere
    // left to deliver the composed context, because the positional
    // initial-prompt slot `build_launch` bakes into `launch.argv` is already
    // fixed by the time either of them runs. Every *other* Zirv launch path
    // (`exec`, `loop`, and the dashboard's own worker panes via `dash::
    // compose_worker_prompt`) already folds the composed context onto its
    // task-prompt text as a fallback for exactly this adapter shape; this
    // Orchestrator launch was the one path that never got the same
    // treatment, so a codex orchestrator (a standalone `wrap` fallback, or
    // the dashboard's own orchestrator pane) started with no zirv context at
    // all -- not even the shipped default layer -- while a claude
    // orchestrator always gets one. Folded in here, once, before `build_
    // launch` bakes the positional prompt slot: both branches below reuse
    // this same `launch`.
    // Change 2 (wrapper-overhead benchmark): the proxy's chosen workflow is
    // started ONCE, here, before `proxy_layer_text` (and therefore before
    // `initial_prompt`/`launch`/`wrap_args` bake it in) -- every one of the
    // three launch shapes below shares this SAME started id, exactly like
    // they already share `proxy_layer` itself, so the prompt each of them
    // eventually carries can name the concrete running instance rather than
    // only the workflow's kind.
    let started_workflow_id = start_proxy_workflow(&intake, &state, repo, |text| {
        proxy_announcer.emit_to(stderr, &super::announce::Event::ProxyAdvisory { text });
    });
    // Issue #537 (T2a): the harness proxy's own bounded layer text, computed
    // once here and threaded to every place that needs it -- the fallback
    // just below, `dash_orchestrator_pane` and `wrap_args_for` -- so all
    // three launch shapes carry exactly the same layer or none at all.
    let proxy_layer = proxy_layer_text(&intake, started_workflow_id.as_deref());
    // Issue #537 (T3): the seat this launch actually runs as -- `Single` for
    // the proxy's own direct/bounded decision, `Orchestrator` for everything
    // else -- resolved once and threaded to every place a role currently
    // hardcodes `Orchestrator`, exactly like `proxy_layer` just above.
    let seat_role = proxy_prompt_role(&intake);
    let initial_prompt = orchestrator_initial_prompt(
        adapter.as_ref(),
        initial_prompt,
        &cfg,
        crate::utils::home_dir().ok().as_deref(),
        repo,
        args.simple,
        &state,
        proxy_layer.as_deref(),
        seat_role,
    );

    // Applies to both branches below (the dashboard's orchestrator pane and
    // the wrap fallback): `chat.model` shapes `zirv chat` generally, not only
    // the dashboard, so the model flags are folded into the launch's own
    // extra arguments once, here, before either path reads `launch.argv`.
    let extra = extra_with_model(&cfg, adapter.as_ref(), &args.extra);
    let mut launch = build_launch(adapter.as_ref(), initial_prompt.as_deref(), &extra);
    // Issue #537 (T3): overrides `build_launch`'s own hardcoded `Orchestrator`
    // default when the proxy decided a Single seat -- every downstream
    // consumer of `launch.role` (the banner, `dash_orchestrator_pane`,
    // `wrap::run_with`, `chat_via_runtime`) already threads it through
    // generically, so this is the one place that has to change.
    launch.role = seat_role;

    if chrome.banner {
        let facts = BannerFacts {
            harness: adapter.name().to_string(),
            rule,
            session: session.as_str().to_string(),
            harnesses: harness_list(&cfg),
            resuming: resuming.then(|| "the last stored handoff for this repo".to_string()),
            model: cfg.chat.model.clone(),
        };
        // `size.0 == 0` only ever means the terminal-size probe itself
        // failed (`probe_terminal`'s own `unwrap_or((0, 0))`), not a real
        // zero-width terminal -- treated as "unknown" so the banner falls
        // back to its compact tier instead of rendering a zero-width box.
        let banner_cols = (size.0 > 0).then_some(size.0);
        writeln!(
            w,
            "{}",
            chrome::banner(&facts, chrome.colour, vt_ok, banner_cols)
        )?;
    }

    let env = quiet_env(env, args.quiet);
    let env = pin_env(&env, args.pin_harness);

    // Emitted here -- after `quiet_env`, before either launch path -- so the
    // dashboard branch and the `wrap` fallback disclose identically, and
    // independently of whether a banner was printed at all.
    announce_model_choice(stderr, &cfg, args.quiet);
    announce_harness_choice(stderr, &cfg, args.quiet, adapter.name(), rule);

    // Issue #352: the persistent runtime, when the operator has turned it on
    // and there is a terminal to attach. Checked before the dashboard branch
    // because it replaces BOTH launch paths below -- the session is opened on
    // the runtime and this process becomes a client of it.
    //
    // A failure here falls back to the ordinary in-process launch with one
    // line on stderr rather than failing the invocation: the runtime is
    // experimental, and an experiment must not be able to stop an operator
    // from getting a session.
    if super::session::chat_route(
        cfg.session.persistent,
        args.no_session,
        stdin_is_tty,
        stdout_is_tty,
    ) == super::session::ChatRoute::Runtime
    {
        // Issue #537 (T2a), change 2, review fix: this attempt is NEVER the
        // last one -- an `Err` here always falls through to either the
        // dashboard pane or the `wrap` fallback below, both of which still
        // need `started_workflow_id` to name a LIVE workflow (it is already
        // baked into `proxy_layer`/`initial_prompt`, which neither of those
        // branches recomputes). Closing it here on failure, as an earlier
        // version of this fix did, would launch that live prompt text
        // against a workflow this same function had just closed. Only the
        // launch shape that actually runs -- the dashboard pane below, or
        // the `wrap` fallback at the very end -- is wrapped in `close_proxy_
        // workflow_on_failure`.
        match super::session::chat_via_runtime(
            &state,
            adapter.name(),
            initial_prompt.as_deref(),
            &extra,
            repo,
            w,
            launch.role,
        ) {
            Ok(code) => return Ok(code),
            Err(error) => writeln!(
                stderr,
                "zirv chat: the persistent runtime is unavailable ({error}); \
                 starting a session in this process instead"
            )?,
        }
    }

    if chrome::dash_eligible(
        stdout_is_tty,
        stdin_is_tty,
        vt_ok,
        size,
        &cfg.dash,
        args.simple,
    ) {
        return run_dash_branch(
            adapter.as_ref(),
            launch,
            &cfg,
            &state,
            repo,
            &env,
            session.as_str(),
            args.simple,
            proxy_layer.as_deref(),
            initial_prompt.as_deref(),
            started_workflow_id.as_deref(),
            args.force_pace,
            |text| {
                proxy_announcer.emit_to(stderr, &super::announce::Event::ProxyAdvisory { text });
            },
        );
    }

    // Ineligible because the dashboard is on but the terminal is too small
    // (every other axis -- both streams a terminal, VT available, not
    // `--simple` -- already passed): the operator gets one line naming the
    // floor and how to silence the notice, then the same wrap passthrough
    // every other ineligible terminal already reaches. `--simple` itself
    // never reaches here (it is excluded from `dash_eligible`'s failure by
    // construction: it is checked first there, and it is checked here too so
    // an explicit `--simple` never prints a notice about a size it was never
    // going to use anyway).
    if cfg.dash.enabled
        && !args.simple
        && stdout_is_tty
        && stdin_is_tty
        && vt_ok
        && (size.0 < chrome::MIN_DASH_COLS || size.1 < chrome::MIN_DASH_ROWS)
    {
        crate::output::error(format!(
            "the terminal is too small for the dashboard (need at least {}x{}, got {}x{}); \
             falling back to a plain session. Pass --simple to silence this.",
            chrome::MIN_DASH_COLS,
            chrome::MIN_DASH_ROWS,
            size.0,
            size.1
        ));
    }

    let wrap_args = wrap_args_for(args, launch.clone(), proxy_layer.clone());
    close_proxy_workflow_on_failure(
        started_workflow_id.as_deref(),
        &state,
        repo,
        |text| {
            proxy_announcer.emit_to(stderr, &super::announce::Event::ProxyAdvisory { text });
        },
        || {
            wrap::run_with(
                &wrap_args,
                repo,
                &env,
                launch.role,
                Some(session),
                launch.verb,
            )
        },
    )
}

/// Discloses a configured model on the `zirv \u{25b8}` announcement channel, once,
/// before the launch path is chosen. A no-op when no model is configured.
///
/// `chat.model` is one of the few keys a **repo** `ctx.toml` may set, and that
/// exemption was granted on the strength of the choice being visible on
/// screen. It was not: the only disclosure was `chrome::banner`, and
/// `chrome.banner` is **not** `REPO_FORBIDDEN` -- so a checked-out repo could
/// pair `[chrome] banner = false` with `[chat] model = "..."` and select the
/// model for every session with nothing shown anywhere at all (the `wrap`
/// fallback carries no other model surface; the dashboard header carries one,
/// but only the dashboard has one).
///
/// `chrome.events` **is** `REPO_FORBIDDEN`, so this line survives any repo
/// configuration. The operator's own `--quiet`/`ZIRV_CTX_QUIET` still silences
/// it -- operator over repo, the same asymmetry every other trust decision in
/// this codebase makes.
///
/// `quiet` is passed separately because `cfg` was loaded before `--quiet` was
/// folded into the environment (`quiet_env`), so `cfg.chrome.events` alone
/// knows about `ZIRV_CTX_QUIET` and `[chrome] events` but not about the flag.
fn announce_model_choice<E: Write>(stderr: &mut E, cfg: &CtxConfig, quiet: bool) {
    let Some(model) = &cfg.chat.model else {
        return;
    };
    super::announce::Announcer::new(
        cfg.chrome.events && !quiet,
        console::colors_enabled_stderr(),
    )
    .emit_to(
        stderr,
        &super::announce::Event::ChatModel {
            model: model.clone(),
        },
    );
}

/// Issue #690: discloses that the harness was chosen because the one ahead
/// of it in registry order is not installed -- which one was picked, which
/// one was missing, and how to pin the choice instead of leaving it to this
/// rule. A no-op for every other `HarnessRule`, so the common case gains no
/// line at all.
///
/// On the same channel, and for the same reason, as `announce_model_choice`
/// above: `chrome.banner` is not `REPO_FORBIDDEN` and the banner's compact
/// tiers have no room for this anyway, while `chrome.events` **is**, so this
/// is a disclosure a repo checkout cannot silence and the operator still
/// can (`--quiet`/`ZIRV_CTX_QUIET`). Presence is an operator-owned fact and
/// acting on it is right; acting on it without saying so is what would make
/// it a silent provider switch.
fn announce_harness_choice<E: Write>(
    stderr: &mut E,
    cfg: &CtxConfig,
    quiet: bool,
    chosen: &str,
    rule: HarnessRule,
) {
    let HarnessRule::FirstInstalledReady { not_found } = rule else {
        return;
    };
    super::announce::Announcer::new(
        cfg.chrome.events && !quiet,
        console::colors_enabled_stderr(),
    )
    .emit_to(
        stderr,
        &super::announce::Event::HarnessAutoSelected {
            chosen: chosen.to_string(),
            not_found: not_found.to_string(),
        },
    );
}

/// The dashboard's orchestrator pane, composed prompt and all.
///
/// F3: the dashboard branch used to hand `dash::run_dashboard` the bare
/// `build_launch` argv, so the one session a human actually talks to was the
/// only session in the whole codebase that got **no** zirv prompt at all --
/// no shipped default layer, no harness meta-teaching, no user/repo/memory
/// layers, and no injection log line -- while the `wrap` fallback below and
/// every worker pane the dashboard spawns all get the full recipe. An
/// operator could not tell the two launch paths apart from the outside, and
/// the orchestrator is precisely the session that is supposed to know how to
/// delegate.
///
/// This is `wrap::run_with`'s own recipe, in its order and with its
/// arguments: `compile::compile` (memory, the derived harness roster --
/// `adapters::harness_prompt_lines`, only for an `Orchestrator` launch --
/// `prompt::compose` as an `Orchestrator`, and the canonical `.zirv/context/`
/// layer on top, issue #44), `merge_command_line_prompt` so an operator's own
/// `--append-system-prompt` in `--` extras is folded in rather than silently
/// duplicated, `injection_args_for_session`, then `log_injection`.
///
/// Deliberately **no** `prompt::with_mail_layer`, exactly like the `wrap`
/// path it mirrors: an interactive Orchestrator session is never given mail
/// bodies, only the one-line unread-count advisory the dashboard header
/// already carries. Only a headless Worker session (`exec`/`loop`, and the
/// worker panes `dash::fulfill_spawn_request` builds) is body-delivered.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dash_orchestrator_pane(
    adapter: &dyn AgentAdapter,
    launch: ChatLaunch,
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
    session: &str,
    simple: bool,
    proxy_layer: Option<&str>,
) -> CtxResult<PaneSpec> {
    dash_orchestrator_pane_with_task(
        adapter,
        launch,
        cfg,
        state,
        repo,
        session,
        simple,
        proxy_layer,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn dash_orchestrator_pane_with_task(
    adapter: &dyn AgentAdapter,
    launch: ChatLaunch,
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
    session: &str,
    simple: bool,
    proxy_layer: Option<&str>,
    task: Option<&str>,
) -> CtxResult<PaneSpec> {
    // Issue #44: gathers memory, the derived harness roster and the
    // canonical `.zirv/context/` layer, and attaches the policy report --
    // see `compile::compile`'s own doc comment.
    let home = crate::utils::home_dir().ok();
    let mut compiled = super::compile::compile(
        home.as_deref(),
        repo,
        simple,
        cfg,
        adapter,
        launch.role,
        state,
        super::state::now_secs(),
        super::adapters::LaunchMode::Interactive,
        true,
    );
    if let Some(task) = task.filter(|task| !task.trim().is_empty()) {
        super::compile::select_skill_descriptions_for_task(
            &mut compiled,
            cfg,
            state,
            repo,
            home.as_deref(),
            task,
        );
    }
    // Issue #537 (T2a): the harness proxy's own bounded layer, when an
    // active decision took over this launch -- a no-op otherwise.
    let compiled = super::compile::with_proxy_layer(compiled, proxy_layer);
    let (mut argv, mut composed) = super::prompt::merge_command_line_prompt(
        adapter,
        &launch.argv,
        compiled.composed,
        None,
        launch.role,
        &cfg.prompt,
    );
    composed = super::obfuscate_store::protect_composed(
        state,
        repo,
        cfg,
        composed,
        "chat_orchestrator_prompt",
    )?;
    let prompt_args = super::prompt::injection_args_for_session(
        adapter,
        &argv,
        composed.as_ref(),
        state,
        session,
    )?;
    super::prompt::log_injection(
        state,
        "chat",
        session,
        composed.as_ref(),
        adapter.system_prompt_supported(&argv),
    );
    // Bug B (harness/model parity, 2026-08-22): the same seam every real
    // launch now calls (`adapters::policy_launch_args`) -- the shipped-
    // default "sandboxed, no prompts" posture plus any explicit `[policy]`
    // restriction. This is the dashboard's own orchestrator pane, the
    // interactive session a human is actually watching, so an approval
    // prompt here is at least answerable -- but the posture still applies:
    // "sandboxed, no prompts" is the shipped default for every launch, not
    // only the unattended ones. `flags_pin_policy` (inside `policy_launch_
    // args`) scans the argv built so far, so an operator's own explicit
    // `--sandbox`/`--ask-for-approval`/`--permission-mode`/
    // `--disallowedTools` (passed after `--` on `zirv chat`) still wins.
    let sandbox_extra =
        adapters::policy_launch_args(cfg, adapter, &argv, adapters::LaunchMode::Interactive);
    // Visible, not silent: the one interactive pane a human is actually
    // watching gets the same announcement every headless seam does. `Chrome
    // events`/`--quiet` govern it identically (`cfg.chrome.events`); no
    // `quiet` parameter reaches this function, so a caller that silenced the
    // banner (`--quiet` folded into the environment before `CtxConfig::load`
    // ran) already has `cfg.chrome.events == false` here too.
    let announcer =
        super::announce::Announcer::new(cfg.chrome.events, console::colors_enabled_stderr());
    announcer.emit(&super::announce::Event::SandboxPosture {
        detail: if sandbox_extra.is_empty() {
            "not applied (operator flags or [sandbox] enabled = false)".to_string()
        } else {
            sandbox_extra.join(" ")
        },
    });
    // Issue #420: same seam as every other supervisor-start launch path --
    // heal any self-healable (`Outdated`) hook entry, then warn at most once
    // per 24h if something still drifted. Best-effort: no home directory is
    // not a reason to fail the launch.
    if let Ok(home) = crate::utils::home_dir() {
        let _ = super::hook_integrity::heal_outdated(state, &home);
        if let Some(summary) = super::hook_integrity::drift_warning_if_due(state, &home) {
            announcer.emit(&super::announce::Event::HookIntegrity { summary });
        }
    }
    argv.extend(sandbox_extra);
    argv.extend(prompt_args);
    // R1: a dashboard pane -- and only a dashboard pane -- pins the harness's
    // own conversation to zirv's session uuid, so the quit roster's stored id
    // is the id `AgentAdapter::resume_args` is later asked to resume. The
    // `wrap` fallback below deliberately does not: its relaunches expect the
    // harness to mint a fresh conversation each time. Empty for any adapter
    // with no verified pin flag (codex).
    //
    // D3: unless the operator already pinned it themselves. `zirv chat --
    // --resume <id>` is an explicit instruction about which conversation this
    // seat is, and appending a fresh `--session-id` on top of it hands the
    // harness two contradictory ids and gets the launch refused. The
    // operator's own flag wins.
    //
    // F6: the roster then does **not** record the conversation the operator
    // named. `PaneSpec::session_id` below is zirv's own `session` uuid either
    // way -- nothing reads the operator's `--resume` value back out of the
    // argv -- so for a pin-suppressed launch the id in the roster and the
    // harness's actual conversation id genuinely differ. That is inert only
    // because this pane is the orchestrator: `dash::on_quit` stamps it
    // `roster::ROLE_ORCHESTRATOR` and `dash::restorable_candidates` filters
    // that role out before `roster::restore_argv` is ever called, so no
    // `--resume <uuid zirv invented>` is ever issued from this entry. A worker
    // pane has no such escape hatch, which is why `dash::fulfill_spawn_request`
    // pins unconditionally.
    if !super::exec::pins_an_existing_conversation(&argv, adapter.name()) {
        argv.extend(adapter.session_pin_args(session));
    }

    Ok(PaneSpec {
        agent_name: launch.agent_name,
        argv,
        role: launch.role,
        verb: launch.verb,
        session_id: session.to_string(),
        title: "orch".to_string(),
    })
}

/// The extra arguments a chat launch is built with: the configured model's
/// own flags (`AgentAdapter::model_args`) ahead of whatever the operator
/// passed after `--`. Handing these to `build_launch`/`interactive_cmd` puts
/// them *after* the positional initial prompt, which is where CLI flags are
/// still perfectly valid and -- unlike splicing them into an already-built
/// argv -- is the one placement that cannot land inside a launcher prefix.
///
/// R1: this used to splice `model_args` in at `launch_prefix_len()`, which
/// deliberately counts only the argv the *operator* wrote (program plus
/// `bin_args`) and explicitly does not count the tokens `ClaudeAdapter::base`
/// prepends when it has to route an npm-installed `claude.cmd` through
/// `cmd.exe /c` (see `claude.rs`'s own `launch_prefix_len` doc comment). On
/// such a launch the real argv prefix is three tokens, not one, so the splice
/// produced `["cmd.exe", "--model", "fable", "/c", "claude.cmd", ...]` --
/// `cmd.exe` was handed the model flags and never started the agent at all.
/// Appending as trailing extras removes the prefix arithmetic entirely, the
/// same way `wrap::restart_launch_flags`'s output is carried into
/// `relaunch_command`'s `extra` rather than spliced anywhere.
///
/// An adapter with no verified model flag, or no configured model, yields the
/// operator's own extras unchanged.
fn extra_with_model(cfg: &CtxConfig, adapter: &dyn AgentAdapter, extra: &[String]) -> Vec<String> {
    let Some(model) = cfg.chat.model.as_deref() else {
        return extra.to_vec();
    };
    let mut out = adapter.model_args(model);
    out.extend_from_slice(extra);
    out
}

/// The `wrap` invocation a resolved chat launch becomes. Pure, so what does
/// and does not survive the hand-off is testable without a pty.
///
/// `allow_nested` is threaded through rather than re-derived: `wrap::run_with`
/// runs the same nesting guard again against the same environment, so an
/// override honored here but dropped here would simply be refused one layer
/// down.
pub fn wrap_args_for(args: &ChatArgs, launch: ChatLaunch, proxy_layer: Option<String>) -> WrapArgs {
    WrapArgs {
        agent: Some(launch.agent_name),
        no_supervise: false,
        command: launch.argv,
        simple: args.simple,
        allow_nested: args.allow_nested,
        force_pace: args.force_pace,
        proxy_layer,
    }
}

/// `--quiet` on the `chat` and `agent` verbs is a CLI flag, not an
/// environment variable, but `CtxConfig::load` (inside `wrap::run_with`)
/// only ever reads `chrome.events` from config layers and `ZIRV_CTX_QUIET`.
/// Folding the flag into the same env lookup both already share is simpler
/// than adding a second, parallel "quiet" parameter to every downstream
/// signature: it reuses the one config key that already means "silence the
/// announcement channel", and honors the same operator-overrides-repo
/// precedence `ZIRV_CTX_QUIET` always has.
pub(crate) fn quiet_env<'a>(
    env: EnvLookup<'a>,
    quiet: bool,
) -> impl Fn(&str) -> Option<String> + 'a {
    move |key: &str| {
        if quiet && key == "ZIRV_CTX_QUIET" {
            Some("true".to_string())
        } else {
            env(key)
        }
    }
}

/// Issue #358 (task 4): the same fold-a-flag-into-the-shared-env-closure
/// idiom `quiet_env` above already established for `--quiet`, this time for
/// `--pin-harness` -> `seat::PIN_ENV`. This is deliberately how `--pin-
/// harness` reaches `seat::pin_from_env` rather than a new field threaded
/// through `WrapArgs`/`PaneSpec`: the actual `seat::register` call for an
/// orchestrator session lives inside `wrap.rs`/`dash/pane.rs` (see `seat::
/// register`'s own doc comment for exactly where), both files this task does
/// not touch. Once task 5 wires that call in, reading `env(seat::PIN_ENV)`
/// there already sees this override, because both launch paths this
/// function's own caller feeds (`wrap::run_with`, `dash::run_dashboard`) are
/// handed this SAME closure -- no further plumbing needed on the flag's own
/// path once that call exists.
pub(crate) fn pin_env<'a>(env: EnvLookup<'a>, pin: bool) -> impl Fn(&str) -> Option<String> + 'a {
    move |key: &str| {
        if pin && key == super::seat::PIN_ENV {
            Some("true".to_string())
        } else {
            env(key)
        }
    }
}

pub fn run<W: Write>(args: &ChatArgs, w: &mut W) -> CtxResult<i32> {
    let repo = std::env::current_dir()?;
    let env = env_from_process();
    run_with(args, w, &mut std::io::stderr(), &repo, &env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::adapters::claude::ClaudeAdapter;
    use crate::commands::ctx::adapters::codex::CodexAdapter;
    use crate::commands::ctx::catalogue::Tier;
    use crate::commands::ctx::handoff::Handoff;
    use crate::commands::ctx::proxy::decision::{Decider, Seat, SeatTier};
    use crate::commands::ctx::state::StateDir;
    use crate::commands::workflow::classify::{Complexity, Intent, RiskBand};
    use crate::commands::workflow::profile::{ExecutionMode, ValidationProfile};
    use std::collections::BTreeMap;

    /// The exact defect the intake prompt was reported as: Left/Right must
    /// actually relocate the edit point, not just be ignored (which is what
    /// bare canonical-mode `read_line` effectively does with them) or always
    /// append at the end regardless of where the cursor visually is.
    #[test]
    fn arrow_keys_relocate_the_cursor_instead_of_only_ever_appending() {
        let mut line = EditLine::default();
        for c in "hllo".chars() {
            line.insert(c);
        }
        assert_eq!(line.text(), "hllo");
        assert_eq!(line.cursor, 4);

        // Move left three times to sit right after the "h".
        assert_eq!(
            apply_key(&mut line, KeyCode::Left, KeyModifiers::NONE),
            EditAction::Edited
        );
        assert_eq!(
            apply_key(&mut line, KeyCode::Left, KeyModifiers::NONE),
            EditAction::Edited
        );
        assert_eq!(
            apply_key(&mut line, KeyCode::Left, KeyModifiers::NONE),
            EditAction::Edited
        );
        assert_eq!(line.cursor, 1);

        // Insert at the relocated cursor, not at the end.
        assert_eq!(
            apply_key(&mut line, KeyCode::Char('e'), KeyModifiers::NONE),
            EditAction::Edited
        );
        assert_eq!(line.text(), "hello");
        assert_eq!(line.cursor, 2);

        // Right moves back toward the end.
        assert_eq!(
            apply_key(&mut line, KeyCode::Right, KeyModifiers::NONE),
            EditAction::Edited
        );
        assert_eq!(line.cursor, 3);
    }

    #[test]
    fn cursor_movement_is_a_no_op_and_ignored_at_either_edge() {
        let mut line = EditLine::default();
        line.insert('a');
        line.insert('b');
        line.cursor = 0;
        assert_eq!(
            apply_key(&mut line, KeyCode::Left, KeyModifiers::NONE),
            EditAction::Ignored,
            "already at column 0 -- nothing to move left into"
        );
        line.cursor = line.chars.len();
        assert_eq!(
            apply_key(&mut line, KeyCode::Right, KeyModifiers::NONE),
            EditAction::Ignored,
            "already past the last char -- nothing to move right into"
        );
        assert_eq!(
            apply_key(&mut line, KeyCode::Home, KeyModifiers::NONE),
            EditAction::Edited
        );
        assert_eq!(line.cursor, 0);
        assert_eq!(
            apply_key(&mut line, KeyCode::Home, KeyModifiers::NONE),
            EditAction::Ignored,
            "already at column 0"
        );
        assert_eq!(
            apply_key(&mut line, KeyCode::End, KeyModifiers::NONE),
            EditAction::Edited
        );
        assert_eq!(line.cursor, 2);
    }

    #[test]
    fn backspace_and_delete_act_on_the_cursor_not_the_end_of_the_line() {
        let mut line = EditLine::default();
        for c in "abcd".chars() {
            line.insert(c);
        }
        line.cursor = 2; // between 'b' and 'c'
        assert_eq!(
            apply_key(&mut line, KeyCode::Backspace, KeyModifiers::NONE),
            EditAction::Edited
        );
        assert_eq!(line.text(), "acd", "erases 'b', the char before the cursor");
        assert_eq!(line.cursor, 1);
        assert_eq!(
            apply_key(&mut line, KeyCode::Delete, KeyModifiers::NONE),
            EditAction::Edited
        );
        assert_eq!(
            line.text(),
            "ad",
            "erases 'c', the char at/after the cursor"
        );
        assert_eq!(line.cursor, 1);
    }

    /// Raw mode's own cost: `ISIG`/`ICANON` going away silently takes
    /// Ctrl+C's interrupt and Ctrl+D's end-of-input with them unless mapped
    /// back explicitly, which is what `EditAction::Cancel`/`Eof` are for.
    #[test]
    fn ctrl_c_and_ctrl_d_map_to_cancel_and_eof_not_a_literal_character() {
        let mut line = EditLine::default();
        assert_eq!(
            apply_key(&mut line, KeyCode::Char('c'), KeyModifiers::CONTROL),
            EditAction::Cancel
        );
        assert_eq!(
            apply_key(&mut line, KeyCode::Char('d'), KeyModifiers::CONTROL),
            EditAction::Eof
        );
        assert!(
            line.chars.is_empty(),
            "neither control chord ever gets typed into the line itself"
        );
    }

    /// Review finding: Ctrl+D mapped to `Eof` unconditionally, so the chord
    /// that ends input on an empty prompt silently threw away a request the
    /// operator had already typed. Canonical mode only ever sends `VEOF` on
    /// an empty line; with text present the terminal submits it instead.
    #[test]
    fn ctrl_d_on_a_typed_line_submits_it_instead_of_discarding_it() {
        let mut line = EditLine::default();
        for c in "ship it".chars() {
            line.insert(c);
        }
        assert_eq!(
            apply_key(&mut line, KeyCode::Char('d'), KeyModifiers::CONTROL),
            EditAction::Submit
        );
        assert_eq!(line.text(), "ship it", "the typed request survives intact");
    }

    #[test]
    fn enter_submits_without_touching_the_line() {
        let mut line = EditLine::default();
        line.insert('h');
        line.insert('i');
        assert_eq!(
            apply_key(&mut line, KeyCode::Enter, KeyModifiers::NONE),
            EditAction::Submit
        );
        assert_eq!(line.text(), "hi", "submitting must not mutate the buffer");
    }

    /// Review finding: the redraw put the cursor at `MoveToColumn(cursor)`
    /// using the codepoint index, so a request longer than the terminal is
    /// wide left the cursor on the wrong row entirely -- and every edit after
    /// that painted over the wrong line.
    #[test]
    fn the_cursor_wraps_onto_the_row_its_own_cell_count_puts_it_on() {
        assert_eq!(edit_line_layout(0, 20), (0, 0));
        assert_eq!(edit_line_layout(19, 20), (0, 19));
        assert_eq!(
            edit_line_layout(20, 20),
            (1, 0),
            "the cell just past the last column belongs to the next row"
        );
        assert_eq!(edit_line_layout(45, 20), (2, 5));
        assert_eq!(
            edit_line_layout(3, 0),
            (3, 0),
            "a zero width is treated as one column rather than dividing by zero"
        );
    }

    /// The other half of the same finding: cells, not codepoints. A CJK
    /// glyph takes two columns and a combining mark takes none, so counting
    /// `chars` put the cursor a whole row out on any non-ASCII request.
    #[test]
    fn cursor_position_counts_display_cells_not_codepoints() {
        let mut line = EditLine::default();
        for c in "日本".chars() {
            line.insert(c);
        }
        assert_eq!(line.chars.len(), 2);
        assert_eq!(line.cells(), 4, "each CJK glyph occupies two columns");
        assert_eq!(line.cells_upto(1), 2);
        assert_eq!(
            edit_line_layout(line.cells(), 3),
            (1, 1),
            "four cells in a three-column terminal wrap onto the second row"
        );
    }

    fn handoff() -> Handoff {
        Handoff {
            task: "Wire the payments webhook".to_string(),
            done: vec!["Added the route".to_string()],
            remaining: vec!["Signature verification".to_string()],
            next_step: "Add a failing test for an invalid signature".to_string(),
            files_modified: vec!["src/routes/webhook.rs".to_string()],
            gotchas: vec![],
            ..Handoff::default()
        }
    }

    #[test]
    fn chat_builds_the_launch_from_the_adapter_rather_than_a_user_argv() {
        // Deliberately does not recompute `expected_argv` by calling
        // `adapter.interactive_cmd` a second time and extracting it the same
        // way `build_launch` does: that would just be `build_launch`'s own
        // extraction logic compared against itself, so a bug in it would
        // never show up here. Asserting on fixed, independently-known
        // content (the adapter binary is the given constant; the prompt is
        // the given constant; extra flags land after both) is what actually
        // pins the behavior.
        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let launch = build_launch(
            &adapter,
            Some("hello"),
            &["--model".to_string(), "opus".to_string()],
        );

        assert_eq!(
            launch.argv.first().map(String::as_str),
            Some("/tmp/fake-claude"),
            "the argv's own program is the adapter's own binary: {:?}",
            launch.argv
        );
        assert!(
            launch.argv.contains(&"hello".to_string()),
            "the initial prompt reaches argv: {:?}",
            launch.argv
        );
        assert_eq!(
            &launch.argv[launch.argv.len() - 2..],
            &["--model".to_string(), "opus".to_string()],
            "extra flags land last: {:?}",
            launch.argv
        );
    }

    #[test]
    fn chat_passes_the_resolved_agent_explicitly_so_wrap_never_has_to_guess() {
        let adapter = ClaudeAdapter::new(None);
        let launch = build_launch(&adapter, None, &[]);
        assert_eq!(launch.agent_name, "claude");
    }

    #[test]
    fn extra_flags_after_the_separator_reach_the_agent_untouched() {
        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let extra = vec!["--model".to_string(), "opus".to_string()];
        let launch = build_launch(&adapter, None, &extra);
        assert_eq!(
            &launch.argv[launch.argv.len() - 2..],
            &extra[..],
            "extra flags must survive to the end of argv untouched: {:?}",
            launch.argv
        );
    }

    /// Bug B: claude always reports `system_prompt_supported`, so the
    /// composed fallback must be a no-op for it. With masking disabled (the
    /// default), the positional prompt stays byte-for-byte unchanged too.
    #[test]
    fn orchestrator_initial_prompt_is_a_no_op_for_a_supported_adapter() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));

        assert_eq!(
            orchestrator_initial_prompt(
                &adapter,
                None,
                &cfg,
                Some(&home),
                tmp.path(),
                false,
                &state,
                None,
                PromptRole::Orchestrator,
            ),
            None
        );
        assert_eq!(
            orchestrator_initial_prompt(
                &adapter,
                Some("resume this".to_string()),
                &cfg,
                Some(&home),
                tmp.path(),
                false,
                &state,
                None,
                PromptRole::Orchestrator,
            ),
            Some("resume this".to_string())
        );
    }

    #[test]
    fn orchestrator_initial_prompt_masks_a_supported_adapters_positional_prompt() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.obfuscate.mode = super::super::config::ObfuscateMode::Obfuscate;
        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz123456";

        let prompt = orchestrator_initial_prompt(
            &adapter,
            Some(format!("use {secret}")),
            &cfg,
            Some(&home),
            tmp.path(),
            false,
            &state,
            None,
            PromptRole::Orchestrator,
        )
        .expect("the masked prompt is preserved");

        assert!(!prompt.contains(secret), "{prompt}");
        assert!(prompt.contains("ZIRV_SECRET_GITHUB_TOKEN_1"), "{prompt}");
    }

    /// Bug B, the actual fix: on a Windows npm-installed `codex.cmd` shim --
    /// the shape `CodexAdapter::system_prompt_supported` narrows to
    /// unsupported -- the composed session context (the shipped default
    /// layer and, because this is an Orchestrator launch, the harness
    /// meta-teaching layer) must land on the positional initial-prompt slot,
    /// since `injection_args_for_session` never reaches this adapter at all.
    /// Before this fix a codex orchestrator on this launch shape started
    /// with no zirv context whatsoever, while a claude orchestrator always
    /// got one (see the previous test).
    #[cfg(windows)]
    #[test]
    fn orchestrator_initial_prompt_folds_composed_context_for_an_unsupported_codex_shim() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let shim_dir = tempfile::tempdir().expect("tempdir");
        let shim = shim_dir.path().join("codex.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");
        let adapter = CodexAdapter::new(Some(&shim.display().to_string()));
        assert!(
            !adapter.system_prompt_supported(&[]),
            "a .cmd shim must be the unsupported shape this test exercises"
        );

        let text = orchestrator_initial_prompt(
            &adapter,
            None,
            &cfg,
            Some(&home),
            tmp.path(),
            false,
            &state,
            None,
            PromptRole::Orchestrator,
        )
        .expect("an unsupported adapter still gets a fallback prompt");
        assert!(
            text.contains("zirv engineering standard"),
            "the shipped default layer must reach the fallback: {text}"
        );
        assert!(
            text.contains("zirv meta-harness"),
            "an Orchestrator launch must still get the harness delegation layer: {text}"
        );

        // A real resume prompt is preserved as the leading text, with the
        // composed context appended after it -- the same order `exec.rs`'s
        // own `task_prompt_with_composed_fallback` call keeps for a headless
        // worker's own task text.
        let with_resume = orchestrator_initial_prompt(
            &adapter,
            Some("continue the payments webhook".to_string()),
            &cfg,
            Some(&home),
            tmp.path(),
            false,
            &state,
            None,
            PromptRole::Orchestrator,
        )
        .expect("still folds a fallback in on top of a real prompt");
        assert!(
            with_resume.starts_with("continue the payments webhook"),
            "the caller's own prompt text must lead: {with_resume}"
        );
        assert!(
            with_resume.contains("zirv engineering standard"),
            "and the composed context must still follow it: {with_resume}"
        );
    }

    /// Issue #537 (T2a): the same unsupported-adapter fallback carries the
    /// harness proxy's own bounded layer when one is given, on top of the
    /// composed context this launch shape already folds in.
    #[cfg(windows)]
    #[test]
    fn orchestrator_initial_prompt_folds_the_proxy_layer_for_an_unsupported_codex_shim() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let shim_dir = tempfile::tempdir().expect("tempdir");
        let shim = shim_dir.path().join("codex.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");
        let adapter = CodexAdapter::new(Some(&shim.display().to_string()));

        let text = orchestrator_initial_prompt(
            &adapter,
            None,
            &cfg,
            Some(&home),
            tmp.path(),
            false,
            &state,
            Some("[zirv proxy]\nexecution: bounded"),
            PromptRole::Orchestrator,
        )
        .expect("an unsupported adapter still gets a fallback prompt");
        assert!(
            text.contains("[zirv proxy]"),
            "a given decision must reach the fallback prompt: {text}"
        );
    }

    /// `--simple` disables prompt composition entirely (`compile::compile`
    /// returns `composed: None`, mirroring `prompt::compose`'s own gate), so
    /// even an unsupported adapter must fall back to the caller's own
    /// `initial_prompt` unchanged rather than injecting anything.
    #[cfg(windows)]
    #[test]
    fn orchestrator_initial_prompt_is_a_no_op_under_simple_even_for_an_unsupported_adapter() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let shim_dir = tempfile::tempdir().expect("tempdir");
        let shim = shim_dir.path().join("codex.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");
        let adapter = CodexAdapter::new(Some(&shim.display().to_string()));

        assert_eq!(
            orchestrator_initial_prompt(
                &adapter,
                None,
                &cfg,
                Some(&home),
                tmp.path(),
                true,
                &state,
                None,
                PromptRole::Orchestrator,
            ),
            None,
            "--simple must still suppress every zirv-injected layer, fallback included"
        );
    }

    #[test]
    fn chat_is_an_orchestrator_session() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(
            build_launch(&adapter, None, &[]).role,
            PromptRole::Orchestrator,
            "a chat session is the one a human talks to directly"
        );
        // Resuming or adding extra flags must not change that.
        assert_eq!(
            build_launch(&adapter, Some("resume this"), &["--model".to_string()]).role,
            PromptRole::Orchestrator
        );
    }

    /// N1: a chat session's registry record must say "chat", not fall back to
    /// `wrap`'s own default verb -- `wrap::run_with` takes a `verb` parameter
    /// precisely so this can be threaded through explicitly rather than
    /// guessed from `role` (the two are independent: role governs prompt
    /// injection permissions, verb only names the calling verb for the
    /// registry). Unit-tested here, on the same pure `build_launch` the role
    /// assertions above already exercise, rather than through a real pty.
    #[test]
    fn chat_registers_as_chat_rather_than_wrap() {
        let adapter = ClaudeAdapter::new(None);
        assert_eq!(
            build_launch(&adapter, None, &[]).verb,
            crate::commands::ctx::sessions::Verb::Chat,
        );
        // Resuming or adding extra flags must not change that either.
        assert_eq!(
            build_launch(&adapter, Some("resume this"), &["--model".to_string()]).verb,
            crate::commands::ctx::sessions::Verb::Chat,
        );
    }

    /// F3: the dashboard's orchestrator pane must carry the same composed
    /// prompt the `wrap` fallback builds -- the shipped default layer proves
    /// injection happened at all, and the harness meta-teaching layer proves
    /// it happened as an *Orchestrator*. Before this fix the dashboard
    /// branch handed `run_dashboard` the bare adapter argv, so the one
    /// session a human talks to was the only unprompted one in the codebase.
    #[test]
    fn the_dash_orchestrator_pane_carries_the_composed_prompt() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        // A binary that does not exist and is not a cmd shim: the file-flag
        // capability probe fails and the launch is not the reparsed `cmd.exe /c`
        // form, so `injection_args_for_session` uses the inline
        // `system_prompt_args` form and the prompt text is visible in argv --
        // which is what makes this assertable without a real agent. (The shim
        // form, which forces the file form, is covered in `prompt.rs`.)
        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let launch = build_launch(&adapter, None, &[]);
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
            None,
        )
        .expect("pane");

        let argv = pane.argv.join(" ");
        assert!(
            argv.contains("zirv engineering standard"),
            "the shipped default layer proves injection happened: {argv}"
        );
        assert!(
            argv.contains("zirv meta-harness"),
            "an orchestrator session gets the harness delegation layer: {argv}"
        );
        assert_eq!(pane.role, PromptRole::Orchestrator);
        assert_eq!(pane.verb, crate::commands::ctx::sessions::Verb::Chat);
        assert_eq!(pane.title, "orch");
        assert_eq!(
            pane.argv.first().map(String::as_str),
            Some("/nonexistent/fake-claude"),
            "the launch program is still the adapter's own binary: {argv}"
        );
    }

    #[test]
    fn task_selected_skill_descriptions_change_late_launch_bytes_but_not_discovery() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let skills = tmp.path().join(".zirv/skills");
        std::fs::create_dir_all(&skills).expect("skills");
        let descriptions = [
            (
                "aa-database-helper",
                "database migration schema alpha ".repeat(28),
            ),
            (
                "ab-security-helper",
                "security credential audit beta ".repeat(28),
            ),
            (
                "ac-database-helper",
                "database migration schema gamma ".repeat(28),
            ),
            (
                "ad-security-helper",
                "security credential audit delta ".repeat(28),
            ),
        ];
        for (id, description) in &descriptions {
            std::fs::write(
                skills.join(format!("{id}.yaml")),
                format!(
                    "schema_version: 1\nid: {id}\nversion: 1\nname: {id}\n\
                     description: {description}\nimplicit_activation: true\n\
                     context_budget_bytes: 64\nphases: [implement]\ninstructions: use safely\n"
                ),
            )
            .expect("skill fixture");
        }
        let entries = super::super::prompt::skill_index_entries(tmp.path(), Some(&home))
            .expect("skill entries");
        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let task = "Fix the CSS frontend layout";
        let credential_env = "CHAT_TEST_JEV_SKILL_DESCRIPTIONS_737";
        // SAFETY (test-only): this test owns a unique environment variable.
        unsafe { std::env::set_var(credential_env, "secret") };
        let mut delivered = Vec::new();
        for (case, kept_skill) in [
            ("data", "aa-database-helper"),
            ("security", "ab-security-helper"),
        ] {
            let answers = descriptions
                .iter()
                .map(|(id, _)| {
                    let index = entries
                        .iter()
                        .position(|(entry_id, _, _)| entry_id == id)
                        .expect("fixture skill");
                    (
                        format!("s{index}"),
                        serde_json::json!({"type":"noul","noul": if *id == kept_skill {0.98} else {0.02}}),
                    )
                })
                .collect::<serde_json::Map<_, _>>();
            let body = serde_json::json!({"model":"jev-latest","answers":answers,"usage":{"input_tokens":8,"output_tokens":1}}).to_string();
            let (url, handle) =
                super::super::jev::tests::one_shot_server(200, Box::leak(body.into_boxed_str()));
            let mut cfg = CtxConfig::default();
            cfg.jev.context = true;
            cfg.proxy.typesafe.base_url = url;
            cfg.proxy.typesafe.credential_env = credential_env.into();
            let state = StateDir::from_root(tmp.path().join(format!("state-{case}")));
            let pane = dash_orchestrator_pane_with_task(
                &adapter,
                build_launch(&adapter, Some(task), &[]),
                &cfg,
                &state,
                tmp.path(),
                "11111111-2222-4333-8444-555555555555",
                false,
                None,
                Some(task),
            )
            .expect("pane");
            handle.join().expect("Jev fixture");
            delivered.push(pane.argv.join(" "));
        }
        let header = super::super::prompt::SKILL_DESCRIPTIONS_HEADER;
        let (first_prefix, first_late) = delivered[0].split_once(header).expect("late layer");
        let (second_prefix, second_late) = delivered[1].split_once(header).expect("late layer");
        assert_eq!(
            first_prefix, second_prefix,
            "task-independent launch prefix"
        );
        let index = first_prefix
            .split_once(super::super::prompt::SKILL_INDEX_HEADER)
            .expect("skill index")
            .1
            .split("\n\n---")
            .next()
            .expect("index body");
        for (id, _, _) in &entries {
            assert!(index.contains(&format!("- {id}")), "missing skill ID {id}");
        }
        assert!(first_prefix.contains("zirv skill load <id>"));
        for (_, description) in &descriptions {
            assert!(!index.contains(description));
        }
        assert!(first_late.contains(&descriptions[0].1));
        assert!(!first_late.contains(&descriptions[1].1));
        assert!(!second_late.contains(&descriptions[0].1));
        assert!(second_late.contains(&descriptions[1].1));

        let mut baseline_cfg = CtxConfig::default();
        baseline_cfg.proxy.typesafe.credential_env = credential_env.into();
        let baseline_state = StateDir::from_root(tmp.path().join("state-baseline"));
        let baseline = dash_orchestrator_pane_with_task(
            &adapter,
            build_launch(&adapter, Some(task), &[]),
            &baseline_cfg,
            &baseline_state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
            None,
            Some(task),
        )
        .expect("baseline pane")
        .argv
        .join(" ");
        assert!(delivered.iter().all(|prompt| prompt.len() < baseline.len()));
        assert!(
            !baseline.contains(header),
            "inactive gate keeps old prompt bytes"
        );

        let mut explicit_cfg = baseline_cfg;
        explicit_cfg.jev.context = true;
        explicit_cfg.proxy.typesafe.base_url = "http://127.0.0.1:0".into();
        let explicit_state = StateDir::from_root(tmp.path().join("state-explicit"));
        let explicit_task = "Use aa-database-helper for the frontend layout";
        let explicit = dash_orchestrator_pane_with_task(
            &adapter,
            build_launch(&adapter, Some(explicit_task), &[]),
            &explicit_cfg,
            &explicit_state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
            None,
            Some(explicit_task),
        )
        .expect("explicit pane")
        .argv
        .join(" ");
        unsafe { std::env::remove_var(credential_env) };
        assert!(explicit.contains(&descriptions[0].1));
        assert!(!explicit_state.root().join("jev-decisions.jsonl").exists());
    }

    /// Issue #537 (T2a): the dashboard orchestrator pane folds the harness
    /// proxy's own bounded layer onto its compiled context when it is given
    /// one, and (the companion assertion) never does when it is not --
    /// same shape as the test above, but with `proxy_layer` set.
    #[test]
    fn the_dash_orchestrator_pane_carries_the_proxy_layer_only_when_given_one() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));

        let without = dash_orchestrator_pane(
            &adapter,
            build_launch(&adapter, None, &[]),
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
            None,
        )
        .expect("pane");
        assert!(
            !without.argv.join(" ").contains("[zirv proxy]"),
            "no decision given, no proxy layer: {:?}",
            without.argv
        );

        let with = dash_orchestrator_pane(
            &adapter,
            build_launch(&adapter, None, &[]),
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
            Some("[zirv proxy]\nexecution: bounded"),
        )
        .expect("pane");
        assert!(
            with.argv.join(" ").contains("[zirv proxy]"),
            "a given decision must reach the pane's own argv: {:?}",
            with.argv
        );
    }

    /// Bug B (harness/model parity, 2026-08-22): the dashboard's own
    /// orchestrator pane is the interactive session a human actually
    /// watches -- previously the one path a codex operator saw zero
    /// zirv-applied argv restriction on. It now carries the shipped-default
    /// sandbox posture too, and an operator's own explicit pin still wins.
    #[test]
    fn the_dash_orchestrator_pane_carries_the_shipped_sandbox_posture_by_default() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let adapter = CodexAdapter::new(Some("/nonexistent/fake-codex"));
        let launch = build_launch(&adapter, None, &[]);
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
            None,
        )
        .expect("pane");
        assert!(
            pane.argv
                .windows(2)
                .any(|w| w == ["--sandbox", "workspace-write"]),
            "got {:?}",
            pane.argv
        );
        assert!(
            pane.argv
                .windows(2)
                .any(|w| w == ["--ask-for-approval", "never"]),
            "got {:?}",
            pane.argv
        );
    }

    /// An operator's own explicit `--sandbox`/`--ask-for-approval` (passed
    /// after `--` on `zirv chat`) suppresses the zirv-computed prefix
    /// entirely, the same `flags_pin_policy` contract every other seam
    /// honours.
    #[test]
    fn the_dash_orchestrator_pane_lets_an_operators_own_sandbox_flag_win() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let adapter = CodexAdapter::new(Some("/nonexistent/fake-codex"));
        let extra = vec!["--sandbox".to_string(), "danger-full-access".to_string()];
        let launch = build_launch(&adapter, None, &extra);
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
            None,
        )
        .expect("pane");
        assert_eq!(
            pane.argv
                .iter()
                .filter(|a| a.as_str() == "--sandbox")
                .count(),
            1,
            "the operator's own --sandbox must appear exactly once, not augmented: {:?}",
            pane.argv
        );
        assert!(pane.argv.contains(&"danger-full-access".to_string()));
    }

    /// `[sandbox] enabled = false` restores the pre-2026-08-22 behaviour: no
    /// posture argv from this seam at all.
    #[test]
    fn the_dash_orchestrator_pane_carries_nothing_when_the_sandbox_posture_is_opted_out() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig {
            sandbox: crate::commands::ctx::config::SandboxConfig {
                enabled: false,
                ..Default::default()
            },
            ..CtxConfig::default()
        };

        let adapter = CodexAdapter::new(Some("/nonexistent/fake-codex"));
        let launch = build_launch(&adapter, None, &[]);
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
            None,
        )
        .expect("pane");
        assert!(
            !pane.argv.contains(&"--sandbox".to_string()),
            "got {:?}",
            pane.argv
        );
    }

    /// Issue #34 seam coverage (memory review, fix round): the dashboard
    /// orchestrator pane's composed prompt must actually carry the memory
    /// core layer, bounded by the CONFIGURED `cfg.memory.core_max_bytes` --
    /// not a hardcoded default. A tiny cap forces `prompt::with_memory_layer`
    /// to truncate, which only happens if the seam really threads the
    /// configured value through (see `with_memory_layer`'s own truncation
    /// note).
    #[test]
    fn the_dash_orchestrator_pane_carries_the_memory_layer_under_its_configured_cap() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.memory.core_max_bytes = 40;
        // Issue #155: the merged memory layer is capped by the SUM of the two
        // budgets now, not `core_max_bytes` alone -- zero the retrieval half
        // out so this test's tiny budget still actually bounds what gets
        // delivered.
        cfg.memory.retrieval_max_bytes = 0;
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());

        crate::commands::ctx::memory::remember(
            &state,
            &slug,
            &crate::commands::ctx::memory::Entry {
                key: "seam-fact".to_string(),
                written_by: "test".to_string(),
                written: 1,
                verified: 1,
                source: "explicit".to_string(),
                body: format!("{}TAIL_MARKER_NOT_TRUNCATED", "z".repeat(200)),
                importance: None,
                confidence: None,
                tags: Vec::new(),
                paths: Vec::new(),
            },
            &cfg,
        )
        .expect("remember");

        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let launch = build_launch(&adapter, None, &[]);
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
            None,
        )
        .expect("pane");

        let argv = pane.argv.join(" ");
        assert!(
            argv.contains("seam-fact"),
            "the memory core layer must reach the composed prompt: {argv}"
        );
        assert!(
            !argv.contains("TAIL_MARKER_NOT_TRUNCATED"),
            "a tiny core_max_bytes must actually bound the delivered memory layer: {argv}"
        );
        assert!(
            argv.contains("[memory truncated:"),
            "the truncation must be visible, not silent: {argv}"
        );
    }

    /// The orchestrator is never body-delivered mail -- it gets the header's
    /// one-line unread-count advisory instead. Same trust split `wrap`'s own
    /// orchestrator path holds (it never calls `with_mail_layer` either);
    /// only a headless Worker session is handed message bodies.
    #[test]
    fn the_dash_orchestrator_pane_is_never_given_mail_bodies() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "s1".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "SECRET-MAIL-BODY-MARKER".to_string(),
            },
            &cfg,
        )
        .expect("store");

        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let launch = build_launch(&adapter, None, &[]);
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
            None,
        )
        .expect("pane");

        let argv = pane.argv.join(" ");
        assert!(
            !argv.contains("SECRET-MAIL-BODY-MARKER"),
            "an interactive orchestrator never receives message bodies: {argv}"
        );
        assert!(
            crate::commands::ctx::mail::list(&state, &slug, None, None)
                .expect("list")
                .len()
                == 1,
            "and nothing was consumed on its behalf either"
        );
    }

    /// `--simple` promises no zirv-*injected instruction* -- the composed
    /// prompt layer -- at all. It also makes the terminal dashboard-
    /// ineligible, so this path is unreachable in practice today -- pinned
    /// anyway, because the flag's meaning must not depend on which launch
    /// path happens to be taken.
    ///
    /// 2026-08-22 revision: the shipped-default sandbox posture
    /// (`adapters::policy_launch_args`) is a *safety* flag layer, not
    /// injected instruction text, so `--simple` does not withhold it --
    /// otherwise `--simple` would double as an accidental way to disable
    /// the default sandboxing, which is not what "skip zirv's injected
    /// text" asks for. This test now pins that the session pin *and* the
    /// sandbox prefix survive `--simple`, and nothing else does.
    #[test]
    fn a_simple_dash_orchestrator_pane_still_carries_the_sandbox_posture_but_no_injected_prompt() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let launch = build_launch(&adapter, None, &[]);
        let mut expected = launch.argv.clone();
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            true,
            None,
        )
        .expect("pane");
        // R1: the session pin is launch plumbing, not injected instruction --
        // `--simple` promises the agent no zirv-authored text, and a pane that
        // cannot be resumed after a quit is not what it is asking for.
        expected.extend(adapter.default_sandbox_args(
            &Default::default(),
            &Default::default(),
            &[],
            super::super::adapters::LaunchMode::Interactive,
        ));
        expected.extend(adapter.session_pin_args("11111111-2222-4333-8444-555555555555"));
        assert_eq!(
            pane.argv, expected,
            "--simple leaves the adapter's own argv untouched apart from the sandbox posture \
             and the session pin"
        );
    }

    /// R1: the roster stores zirv's own uuid, so a dashboard pane has to make
    /// the harness adopt it as the conversation id -- otherwise the next
    /// launch's restore runs `claude --resume <uuid zirv invented>` and the
    /// restored pane dies with "no conversation found" before it draws a
    /// frame.
    #[test]
    fn the_dash_orchestrator_pane_pins_the_harness_session_to_zirvs_own_uuid() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let session = "11111111-2222-4333-8444-555555555555";
        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let launch = build_launch(&adapter, None, &[]);
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            session,
            false,
            None,
        )
        .expect("pane");

        let pin = pane
            .argv
            .iter()
            .position(|a| a == "--session-id")
            .unwrap_or_else(|| panic!("no --session-id in {:?}", pane.argv));
        assert_eq!(
            pane.argv.get(pin + 1).map(String::as_str),
            Some(session),
            "the pinned id is the pane's own registry session id: {:?}",
            pane.argv
        );
    }

    /// D3: an operator who passed their own resume flag has already said which
    /// conversation this seat is. Appending a fresh `--session-id` on top of it
    /// hands the harness two contradictory ids and gets the launch refused
    /// outright -- and inside a dashboard the pane then died on the spot and was
    /// reaped, so the failure was invisible.
    #[test]
    fn an_operators_own_resume_flag_suppresses_the_session_pin() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let session = "11111111-2222-4333-8444-555555555555";
        let existing = "99999999-8888-4777-8666-555555555555";
        for extra in [
            vec!["--resume".to_string(), existing.to_string()],
            vec![format!("--resume={existing}")],
            vec!["--session-id".to_string(), existing.to_string()],
            vec![format!("--session-id={existing}")],
            vec!["-c".to_string()],
            vec!["--continue".to_string()],
            vec!["--fork-session".to_string()],
        ] {
            let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
            let launch = build_launch(&adapter, None, &extra);
            let pane = dash_orchestrator_pane(
                &adapter,
                launch,
                &cfg,
                &state,
                tmp.path(),
                session,
                false,
                None,
            )
            .expect("pane");

            assert!(
                !pane.argv.iter().any(|a| a == session),
                "no fresh pin may be appended alongside {extra:?}: {:?}",
                pane.argv
            );
            assert!(
                pane.argv.iter().any(|a| a.contains(existing)
                    || a == "-c"
                    || a == "--continue"
                    || a == "--fork-session"),
                "and the operator's own flag still reaches the harness: {:?}",
                pane.argv
            );
        }
    }

    /// F6: what the roster actually records when the pin is suppressed. The
    /// `PaneSpec` keeps zirv's own uuid whatever the operator pinned, so the
    /// stored id and the harness's real conversation id differ -- and the only
    /// thing that makes that inert is the orchestrator being excluded from
    /// restore (`dash::restorable_candidates`, pinned by its own test). This
    /// test is the other end of that pair: it states the mismatch plainly, so
    /// a future change that starts restoring orchestrators has to face it.
    #[test]
    fn a_pin_suppressed_orchestrator_pane_still_carries_zirvs_own_session_id() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();

        let session = "11111111-2222-4333-8444-555555555555";
        let existing = "99999999-8888-4777-8666-555555555555";
        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let launch = build_launch(
            &adapter,
            None,
            &["--resume".to_string(), existing.to_string()],
        );
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            session,
            false,
            None,
        )
        .expect("pane");

        assert_eq!(
            pane.session_id, session,
            "the pane -- and so the roster entry built from it -- keeps zirv's own uuid"
        );
        assert!(
            pane.argv.iter().any(|a| a == existing),
            "while the harness is actually resuming the operator's conversation: {:?}",
            pane.argv
        );
        assert_eq!(
            pane.verb,
            crate::commands::ctx::sessions::Verb::Chat,
            "and it is the verb `on_quit` stamps ROLE_ORCHESTRATOR from, which is what keeps \
             the mismatch out of any restore"
        );
    }

    // The `chat.model` disclosure. `chat.model` is repo-settable because the
    // choice is supposed to be visible; `chrome.banner` is not
    // `REPO_FORBIDDEN`, so the banner alone could be turned off by the same
    // repo that chose the model. `chrome.events` is.

    // `cfg_with_model` is the shared helper defined further down with the
    // Task 6 model-splice tests.

    #[test]
    fn a_configured_chat_model_is_disclosed_on_the_events_channel() {
        let mut err = Vec::new();
        announce_model_choice(&mut err, &cfg_with_model(Some("fable")), false);
        let text = String::from_utf8(err).expect("utf8");
        assert!(
            text.contains("chat model 'fable' (from config)"),
            "got {text:?}"
        );
    }

    #[test]
    fn no_configured_model_discloses_nothing() {
        let mut err = Vec::new();
        announce_model_choice(&mut err, &cfg_with_model(None), false);
        assert!(err.is_empty(), "got {err:?}");
    }

    /// The operator may silence it; a repo may not. `--quiet` reaches this as
    /// the flag (config was loaded before it was folded into the environment),
    /// `ZIRV_CTX_QUIET`/`[chrome] events = false` reach it as
    /// `cfg.chrome.events` -- both are operator-controlled surfaces.
    #[test]
    fn the_operator_can_silence_the_model_disclosure_but_a_repo_cannot() {
        let mut err = Vec::new();
        announce_model_choice(&mut err, &cfg_with_model(Some("fable")), true);
        assert!(err.is_empty(), "--quiet silences it: {err:?}");

        let mut quiet_cfg = cfg_with_model(Some("fable"));
        quiet_cfg.chrome.events = false;
        let mut err = Vec::new();
        announce_model_choice(&mut err, &quiet_cfg, false);
        assert!(
            err.is_empty(),
            "ZIRV_CTX_QUIET / [chrome] events = false silences it too: {err:?}"
        );

        // And the repo's own lever does not: `chrome.banner` is not
        // `REPO_FORBIDDEN`, so a repo can turn the banner off -- the events
        // line is emitted regardless of it.
        let mut bannerless = cfg_with_model(Some("fable"));
        bannerless.chrome.banner = false;
        let mut err = Vec::new();
        announce_model_choice(&mut err, &bannerless, false);
        assert!(
            String::from_utf8(err).expect("utf8").contains("fable"),
            "a repo-disabled banner must not take the disclosure with it"
        );
    }

    /// The exact scenario the finding describes, end to end through the real
    /// config loader: a repo turns its banner off and picks a model. It may do
    /// both -- neither key is repo-forbidden -- and the events line is what
    /// discloses the choice anyway. A repo that tries to silence *that* channel
    /// does not get a quiet session, it gets a refusal.
    #[test]
    fn a_repo_can_hide_the_banner_and_pick_a_model_but_cannot_hide_the_disclosure() {
        let repo = crate::commands::ctx::testenv::repo();
        let dir = repo.path().join(".zirv");
        std::fs::create_dir_all(&dir).expect("mkdir .zirv");
        std::fs::write(
            dir.join("ctx.toml"),
            "[chrome]\nbanner = false\n\n[chat]\nmodel = \"sneaky\"\n",
        )
        .expect("write repo ctx.toml");
        let env: std::collections::HashMap<String, String> = Default::default();
        let cfg = CtxConfig::load(repo.path(), &|k| env.get(k).cloned()).expect("load");

        assert!(!cfg.chrome.banner, "a repo may turn the banner off");
        assert_eq!(
            cfg.chat.model.as_deref(),
            Some("sneaky"),
            "and it may still choose the model"
        );
        assert!(
            cfg.chrome.events,
            "but the announcement channel is still on"
        );

        let mut err = Vec::new();
        announce_model_choice(&mut err, &cfg, false);
        assert!(
            String::from_utf8(err).expect("utf8").contains("sneaky"),
            "so the choice is disclosed anyway"
        );

        // And the channel itself is `REPO_FORBIDDEN`: a repo reaching for it
        // fails the load outright rather than quietly winning.
        std::fs::write(
            dir.join("ctx.toml"),
            "[chrome]\nevents = false\n\n[chat]\nmodel = \"sneaky\"\n",
        )
        .expect("write repo ctx.toml");
        let err = CtxConfig::load(repo.path(), &|k| env.get(k).cloned())
            .expect_err("a repo may not set chrome.events");
        assert!(
            err.to_string().contains("chrome.events"),
            "the refusal names the key: {err}"
        );
    }

    /// End to end through `run_with`'s own stderr writer, on the `wrap`
    /// fallback path (the only one reachable under `cargo test`'s piped
    /// stdio). The dashboard branch cannot be driven from a test, which is
    /// precisely why the emit sits *before* the branch: one call site, both
    /// paths.
    #[test]
    fn run_with_discloses_the_model_before_it_picks_a_launch_path() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let state_tmp = tempfile::tempdir().expect("tempdir");
        let env: std::collections::HashMap<String, String> = [
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                "Z:/nonexistent/agent-bin".to_string(),
            ),
            ("ZIRV_CTX_CHAT_MODEL".to_string(), "fable".to_string()),
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state_tmp.path().display().to_string(),
            ),
        ]
        .into();

        let mut out = Vec::new();
        let mut err_out = Vec::new();
        // The launch itself fails (the configured binary does not exist),
        // which is fine and is what the neighbouring tests already pin: the
        // disclosure happens before the launch either way.
        let _ = run_with(
            &chat_args(false),
            &mut out,
            &mut err_out,
            repo.path(),
            &|k| env.get(k).cloned(),
        );

        let text = String::from_utf8(err_out).expect("utf8");
        assert!(
            text.contains("chat model 'fable' (from config)"),
            "the disclosure must reach stderr on the wrap fallback path: {text:?}"
        );
    }

    /// R1, the other half: `wrap` is untouched. Its relaunch path expects the
    /// harness to mint a fresh conversation on every restart, so the pin lives
    /// at the dashboard-pane seam and never inside `interactive_cmd`/
    /// `build_launch`.
    #[test]
    fn the_wrap_fallback_launch_is_never_session_pinned() {
        let adapter = ClaudeAdapter::new(None);
        let launch = build_launch(&adapter, Some("do the thing"), &["--model".to_string()]);
        assert!(
            !launch.argv.iter().any(|a| a == "--session-id"),
            "the plain chat/wrap launch carries no pin: {:?}",
            launch.argv
        );
    }

    #[test]
    fn resume_folds_the_latest_handoff_into_the_first_prompt() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        crate::commands::ctx::handoff::store(&state, tmp.path(), "sess", &handoff())
            .expect("store");

        let mut out = Vec::new();
        let prompt = resolve_initial_prompt(
            true,
            &state,
            tmp.path(),
            &mut out,
            &super::super::screen::Thresholds::default(),
        )
        .expect("resolves")
        .expect("a handoff was stored");

        assert!(prompt.contains("Wire the payments webhook"), "got {prompt}");
        assert_eq!(
            prompt,
            resume::resume_prompt(
                &state,
                tmp.path(),
                "",
                &handoff(),
                &super::super::screen::Thresholds::default(),
            ),
            "chat must fold the handoff the same way `zirv ctx resume` does"
        );
        assert!(
            out.is_empty(),
            "no note needed when a handoff was actually found"
        );
    }

    #[test]
    fn resume_without_a_stored_handoff_starts_a_fresh_session_and_says_so() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));

        let mut out = Vec::new();
        let prompt = resolve_initial_prompt(
            true,
            &state,
            tmp.path(),
            &mut out,
            &super::super::screen::Thresholds::default(),
        )
        .expect("resolves");

        assert_eq!(prompt, None, "nothing to fold in, so a fresh session");
        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            printed.contains("--resume") && printed.to_lowercase().contains("fresh"),
            "must say why it started fresh: {printed}"
        );
    }

    #[test]
    fn no_resume_requested_never_touches_the_handoff_store() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut out = Vec::new();
        let prompt = resolve_initial_prompt(
            false,
            &state,
            tmp.path(),
            &mut out,
            &super::super::screen::Thresholds::default(),
        )
        .expect("resolves");
        assert_eq!(prompt, None);
        assert!(out.is_empty());
    }

    /// Issue #690: the banner's rule has to carry what the origin carries,
    /// or the one surface an operator reads at launch says "auto" where the
    /// truth is "the harness you configured nothing about is the only one
    /// you have". Injected rather than read off `PATH`: on a runner with no
    /// harness installed at all, an ambient probe would make this assert
    /// about the runner instead of about the mapping.
    #[test]
    fn the_harness_rule_carries_the_missing_harness_the_origin_named() {
        let cfg = CtxConfig::default();

        let (adapter, rule) =
            resolve_adapter_with_presence(&cfg, None, &adapters::only_installed(&["codex"]))
                .expect("codex is installed, so there is an answer");
        assert_eq!(adapter.name(), "codex");
        assert_eq!(
            rule,
            HarnessRule::FirstInstalledReady {
                not_found: "claude"
            }
        );

        let (adapter, rule) =
            resolve_adapter_with_presence(&cfg, None, &adapters::everything_installed())
                .expect("a default exists");
        assert_eq!(adapter.name(), "claude");
        assert_eq!(
            rule,
            HarnessRule::FirstEnabledReady,
            "with nothing missing the banner reads exactly as it always did"
        );

        // An explicitly requested harness bypasses presence entirely, even
        // when this machine is the one that does not have it.
        let (adapter, rule) = resolve_adapter_with_presence(
            &cfg,
            Some("claude"),
            &adapters::only_installed(&["codex"]),
        )
        .expect("an explicit --agent is never second-guessed");
        assert_eq!(adapter.name(), "claude");
        assert_eq!(rule, HarnessRule::Explicit);
    }

    /// The registry's own aggregated error (naming every candidate and why it
    /// was skipped) is the message shown when nothing is both enabled and
    /// ready -- the same one `adapters::resolve_default` produces on its own.
    /// Printed to `stderr` (not `w`/stdout: `zirv chat > log` must still show
    /// the operator something on the terminal, matching `output::error`'s own
    /// stream) and reported via exit code 1 rather than a returned `Err`:
    /// propagating it would have `zirv ctx`'s own dispatch print the same
    /// text a second time, unstyled, through `output::error`.
    #[test]
    fn chat_with_no_enabled_and_ready_adapter_names_each_candidate_and_its_reason() {
        let repo = crate::commands::ctx::testenv::repo();
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            crate::commands::ctx::adapters::ADAPTERS
                .iter()
                .map(|(name, _)| format!("[agents.{name}]\nenabled = false\n"))
                .collect::<String>(),
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let args = ChatArgs {
            agent: None,
            resume: false,
            simple: false,
            quiet: false,
            allow_nested: false,
            force_pace: false,
            pin_harness: false,
            no_session: false,
            runtime: None,
            proxy: false,
            no_proxy: false,
            extra: Vec::new(),
        };
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = run_with(&args, &mut out, &mut err_out, repo.path(), &|k| {
            empty.get(k).cloned()
        })
        .expect("prints and exits 1 rather than propagating an Err");
        assert_eq!(code, 1, "nothing is both enabled and ready");
        assert!(out.is_empty(), "nothing prints to stdout on this path");
        let msg = String::from_utf8(err_out).expect("utf8");
        assert!(msg.contains("claude"), "must name claude: {msg}");
        assert!(msg.contains("codex"), "must name codex: {msg}");
        assert!(msg.contains("opencode"), "must name opencode: {msg}");
        assert!(msg.contains("disabled"), "must say why: {msg}");
    }

    /// The gate is checked, and refuses, before any terminal or pty work:
    /// this runs synchronously, with no pty ever opened, to a printed
    /// message and exit code 1 (not a returned `Err` -- see the comment on
    /// `chat_with_no_enabled_and_ready_adapter_names_each_candidate_and_its_
    /// reason` for why). `wrap` performs the identical gate check on its
    /// own before touching a terminal, so the refusal holds even by that
    /// second, independent path.
    #[test]
    fn an_explicitly_named_disabled_agent_is_refused_before_the_terminal_is_touched() {
        let repo = crate::commands::ctx::testenv::repo();
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let args = ChatArgs {
            agent: Some("claude".to_string()),
            resume: false,
            simple: false,
            quiet: false,
            allow_nested: false,
            force_pace: false,
            pin_harness: false,
            no_session: false,
            runtime: None,
            proxy: false,
            no_proxy: false,
            extra: Vec::new(),
        };
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = run_with(&args, &mut out, &mut err_out, repo.path(), &|k| {
            empty.get(k).cloned()
        })
        .expect("prints and exits 1");
        assert_eq!(code, 1, "claude is disabled");
        let msg = String::from_utf8(err_out).expect("utf8");
        assert!(msg.contains("claude"), "got {msg}");
        assert!(msg.contains("disabled"), "got {msg}");
    }

    /// PR #531 review finding 3: `--runtime` used to reimplement the
    /// harness/native decision inline instead of calling
    /// `runtime::selected()`, the one place that decision is supposed to be
    /// made. An unrecognised value must be refused with THAT function's own
    /// wording, not a bespoke message this module drifted from it.
    #[test]
    fn an_unknown_runtime_value_is_refused_with_runtime_selected_s_own_error() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let args = ChatArgs {
            agent: None,
            resume: false,
            simple: false,
            quiet: false,
            allow_nested: false,
            force_pace: false,
            pin_harness: false,
            no_session: false,
            runtime: Some("bogus".to_string()),
            proxy: false,
            no_proxy: false,
            extra: Vec::new(),
        };
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = run_with(&args, &mut out, &mut err_out, repo.path(), &|k| {
            empty.get(k).cloned()
        })
        .expect("prints and exits 1 rather than propagating an Err");
        assert_eq!(code, 1);
        assert!(out.is_empty());
        let msg = String::from_utf8(err_out).expect("utf8");
        let expected = runtime_kind::selected("bogus")
            .expect_err("bogus is not a known runtime")
            .to_string();
        assert_eq!(msg.trim_end(), expected);
    }

    /// Issue #593 (roadmap N22): an explicit `--runtime harness` must
    /// override a configured `[runtime] default = "native"` and launch the
    /// normal wrapped chat -- not the native dashboard pane. Every agent is
    /// disabled so `resolve_adapter` fails deterministically, the same setup
    /// `chat_with_no_enabled_and_ready_adapter_names_each_candidate_and_its_
    /// reason` uses: reaching THAT message (naming every harness candidate)
    /// rather than `run_native_chat`'s own refusal text is the proof the
    /// wrapped path, not the native one, was taken.
    #[test]
    fn chat_runtime_harness_overrides_configured_native_default() {
        let repo = crate::commands::ctx::testenv::repo();
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            crate::commands::ctx::adapters::ADAPTERS
                .iter()
                .map(|(name, _)| format!("[agents.{name}]\nenabled = false\n"))
                .collect::<String>(),
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());
        std::fs::create_dir_all(home.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            home.path().join(".zirv").join("ctx.toml"),
            "[runtime]\ndefault = 'native'\n",
        )
        .expect("write ctx.toml");

        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let args = ChatArgs {
            agent: None,
            resume: false,
            simple: false,
            quiet: false,
            allow_nested: false,
            force_pace: false,
            pin_harness: false,
            no_session: false,
            runtime: Some("harness".to_string()),
            proxy: false,
            no_proxy: false,
            extra: Vec::new(),
        };
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = run_with(&args, &mut out, &mut err_out, repo.path(), &|k| {
            empty.get(k).cloned()
        })
        .expect("prints and exits 1 rather than propagating an Err");
        assert_eq!(code, 1, "nothing is both enabled and ready");
        assert!(out.is_empty(), "no dashboard/banner is ever built here");
        let msg = String::from_utf8(err_out).expect("utf8");
        assert!(
            msg.contains("claude") && msg.contains("codex") && msg.contains("opencode"),
            "reaching resolve_adapter's every-candidate message proves the wrapped harness \
             path was taken, not run_native_chat: {msg}"
        );
        assert!(
            !msg.contains("needs an interactive terminal"),
            "run_native_chat's own refusal text must never appear: {msg}"
        );
    }

    /// Issue #540: `run_native_chat` prints the one-time experimental banner
    /// when launched through the `zirv native` alias -- signalled by
    /// `NATIVE_ALIAS_ENV`, exactly the flag `main.rs`'s alias rewrite sets --
    /// and prints it exactly once, before any of its own refusals (proven
    /// here by asserting it appears even though a non-terminal test process
    /// makes this call reach the TTY refusal too).
    #[test]
    fn native_alias_env_prints_the_banner_once() {
        let repo = crate::commands::ctx::testenv::repo();
        let cfg = CtxConfig::default();
        let env_map: std::collections::HashMap<String, String> =
            [(NATIVE_ALIAS_ENV.to_string(), "true".to_string())]
                .into_iter()
                .collect();
        let args = ChatArgs {
            agent: None,
            resume: false,
            simple: false,
            quiet: false,
            allow_nested: false,
            force_pace: false,
            pin_harness: false,
            no_session: false,
            runtime: Some("native".to_string()),
            proxy: false,
            no_proxy: false,
            extra: Vec::new(),
        };
        let mut err_out = Vec::new();
        let _ = run_native_chat(
            "native",
            &cfg,
            repo.path(),
            &|k| env_map.get(k).cloned(),
            &mut err_out,
            &args,
            false,
            false,
            false,
        );
        let msg = String::from_utf8(err_out).expect("utf8");
        assert_eq!(
            msg.matches(NATIVE_ALIAS_BANNER).count(),
            1,
            "the banner must print exactly once: {msg}"
        );
    }

    /// The mirror of the test above: an explicit `zirv chat --runtime
    /// native` never sets `NATIVE_ALIAS_ENV`, so it must never print the
    /// alias banner even though it launches through this exact same
    /// function.
    #[test]
    fn plain_runtime_native_never_prints_the_alias_banner() {
        let repo = crate::commands::ctx::testenv::repo();
        let cfg = CtxConfig::default();
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let args = ChatArgs {
            agent: None,
            resume: false,
            simple: false,
            quiet: false,
            allow_nested: false,
            force_pace: false,
            pin_harness: false,
            no_session: false,
            runtime: Some("native".to_string()),
            proxy: false,
            no_proxy: false,
            extra: Vec::new(),
        };
        let mut err_out = Vec::new();
        let _ = run_native_chat(
            "native",
            &cfg,
            repo.path(),
            &|k| empty.get(k).cloned(),
            &mut err_out,
            &args,
            false,
            false,
            false,
        );
        let msg = String::from_utf8(err_out).expect("utf8");
        assert!(
            !msg.contains(NATIVE_ALIAS_BANNER),
            "an explicit `--runtime native` (no alias env) must never print the alias banner: \
             {msg}"
        );
    }

    /// Review finding (issue #540): `NATIVE_ALIAS_ENV` is main.rs's own
    /// internal signal, meant to be read exactly once. Left set, every child
    /// process this session later spawns (`wrap.rs`'s harness PTY,
    /// `dash/pane.rs`'s worker panes) would inherit it, since neither
    /// clears the environment before spawning. This proves `run_native_chat`
    /// clears the REAL process environment (not just its own local `env`
    /// closure argument) immediately after its one read, so a "nested" read
    /// afterward -- standing in for such a child reading its own inherited
    /// environment -- sees it unset.
    #[test]
    fn native_alias_env_is_cleared_from_the_real_process_environment_after_one_read() {
        let repo = crate::commands::ctx::testenv::repo();
        let cfg = CtxConfig::default();
        // SAFETY: nextest isolates each test in its own process (this
        // repo's own convention, documented in CLAUDE.md), so no other test
        // can be reading or writing this key concurrently.
        unsafe {
            std::env::set_var(NATIVE_ALIAS_ENV, "true");
        }
        let args = ChatArgs {
            agent: None,
            resume: false,
            simple: false,
            quiet: false,
            allow_nested: false,
            force_pace: false,
            pin_harness: false,
            no_session: false,
            runtime: Some("native".to_string()),
            proxy: false,
            no_proxy: false,
            extra: Vec::new(),
        };
        let real_env = env_from_process();
        let mut err_out = Vec::new();
        let _ = run_native_chat(
            "native",
            &cfg,
            repo.path(),
            &real_env,
            &mut err_out,
            &args,
            false,
            false,
            false,
        );
        // The one read happened -- the banner proves it.
        let msg = String::from_utf8(err_out).expect("utf8");
        assert!(msg.contains(NATIVE_ALIAS_BANNER), "got: {msg}");
        // A nested read afterward, through the identical closure, must see
        // it unset -- proving the real process environment was cleared, not
        // just some local copy.
        assert_eq!(
            real_env(NATIVE_ALIAS_ENV),
            None,
            "NATIVE_ALIAS_ENV must be cleared from the real process environment \
             immediately after run_native_chat's one read"
        );
        assert!(std::env::var(NATIVE_ALIAS_ENV).is_err());
    }

    #[test]
    fn native_help_text_reports_coming_soon_without_setup_instructions() {
        let text = native_help_text();
        assert!(text.contains("coming soon"), "{text}");
        assert!(text.contains("cannot be enabled"), "{text}");
        assert!(!text.contains("provider init"), "{text}");
    }

    #[test]
    fn codex_is_a_valid_launch_target_regardless_of_readiness() {
        // Sanity: build_launch itself does not care about readiness, only
        // resolve_adapter (exercised above) does -- true whether or not
        // codex's own ready() happens to succeed on the machine running this.
        let adapter = CodexAdapter::new(None);
        let launch = build_launch(&adapter, None, &[]);
        assert_eq!(launch.agent_name, "codex");
        assert_eq!(launch.role, PromptRole::Orchestrator);
    }

    /// The chrome probe (`std::io::stdout().is_terminal()`, `term::window_
    /// size`, `term::enable_vt_output`) runs unconditionally at the top of
    /// `run_with`, before adapter resolution -- under cargo test's own piped
    /// stdio (never a terminal) it must degrade cleanly rather than panic,
    /// and a disabled agent must still be refused, with no banner printed
    /// (there is nothing to show a banner for once resolution has failed).
    #[test]
    fn resolving_a_disabled_agent_under_non_terminal_stdio_does_not_panic_and_prints_no_banner() {
        // The agent is disabled explicitly (the same setup `chat_with_no_
        // enabled_and_ready_adapter_names_each_candidate_and_its_reason`
        // uses) so this test never depends on whatever agent binaries
        // happen to be on this machine's PATH: `resolve_adapter` fails
        // deterministically before `wrap::run_with` -- and therefore before
        // any pty or subprocess -- is ever reached.
        let repo = crate::commands::ctx::testenv::repo();
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let args = ChatArgs {
            agent: Some("claude".to_string()),
            resume: false,
            simple: false,
            quiet: false,
            allow_nested: false,
            force_pace: false,
            pin_harness: false,
            no_session: false,
            runtime: None,
            proxy: false,
            no_proxy: false,
            extra: Vec::new(),
        };
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        // Reaching this line at all -- rather than a panic from the probe --
        // is the main thing this test pins.
        let code = run_with(&args, &mut out, &mut err_out, repo.path(), &|k| {
            empty.get(k).cloned()
        })
        .expect("prints and exits 1, no panic");
        assert_eq!(code, 1);
        assert!(
            String::from_utf8(out).expect("utf8").is_empty(),
            "no terminal means no banner, and resolution failed before the banner code anyway"
        );
        let printed = String::from_utf8(err_out).expect("utf8");
        assert!(printed.contains("disabled"), "got {printed}");
    }

    /// Issue #352: the escape hatch exists on the command line and is OFF
    /// unless it is typed. A `zirv chat` that quietly opted into an
    /// experimental runtime would be the opposite of staging it behind a
    /// flag.
    #[test]
    fn no_session_is_an_explicit_opt_out_that_defaults_to_off() {
        use clap::Parser;
        let cli = crate::commands::ctx::CtxCli::try_parse_from(["zirv ctx", "chat"])
            .expect("plain chat parses");
        let crate::commands::ctx::CtxVerb::Chat(args) = cli.verb else {
            panic!("expected chat");
        };
        assert!(!args.no_session);

        let cli =
            crate::commands::ctx::CtxCli::try_parse_from(["zirv ctx", "chat", "--no-session"])
                .expect("--no-session parses");
        let crate::commands::ctx::CtxVerb::Chat(args) = cli.verb else {
            panic!("expected chat");
        };
        assert!(args.no_session);
    }

    // F2: the nesting guard, checked before anything touches the terminal.

    fn chat_args(allow_nested: bool) -> ChatArgs {
        ChatArgs {
            agent: None,
            resume: false,
            simple: false,
            quiet: false,
            allow_nested,
            force_pace: false,
            pin_harness: false,
            no_session: false,
            runtime: None,
            proxy: false,
            no_proxy: false,
            extra: Vec::new(),
        }
    }

    /// The refusal comes out on `stderr` as exit code 1, the same shape every
    /// other `chat` refusal uses (a returned `Err` would be printed a second
    /// time by `ctx`'s own dispatch), and it names the outer session so the
    /// operator can see *which* one they were about to endanger.
    #[test]
    fn chat_refuses_to_start_inside_a_supervised_session_and_names_the_evidence() {
        let repo = crate::commands::ctx::testenv::repo();
        // The `ZIRV_CTX_AGENT_BIN` entry is a safety belt, not a fixture
        // detail: `adapters::select`/`resolve_default` call `ready()`, so an
        // agent_bin that cannot exist makes a launch structurally
        // impossible. If the guard under test ever regresses, this fails on
        // a missing binary rather than spawning a real nested agent.
        let env: std::collections::HashMap<String, String> = [
            (
                crate::commands::ctx::adapters::SESSION_ENV.to_string(),
                "abcdef12-3456-4789-8abc-def012345678".to_string(),
            ),
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                "/nonexistent/agent-must-never-launch".to_string(),
            ),
        ]
        .into();

        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = run_with(
            &chat_args(false),
            &mut out,
            &mut err_out,
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("refuses by printing and exiting 1, not by propagating an Err");

        assert_eq!(code, 1);
        assert!(
            String::from_utf8(out).expect("utf8").is_empty(),
            "nothing goes to stdout on this path -- not even a banner"
        );
        let msg = String::from_utf8(err_out).expect("utf8");
        assert!(
            msg.contains("refusing to start inside an existing agent session"),
            "got {msg}"
        );
        assert!(msg.contains("abcdef12"), "names the outer session: {msg}");
        assert!(
            msg.contains("--allow-nested"),
            "says how to override: {msg}"
        );
    }

    /// With the override on, the guard is out of the way and resolution
    /// proceeds -- reaching the disabled-agent refusal instead. That specific
    /// later message is the evidence the guard was passed, without this test
    /// ever launching an agent.
    #[test]
    fn allow_nested_overrides_the_guard() {
        let repo = crate::commands::ctx::testenv::repo();
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.claude]\nenabled = false\n[agents.codex]\nenabled = false\n",
        )
        .expect("write");
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::adapters::SESSION_ENV.to_string(),
            "abcdef12-3456-4789-8abc-def012345678".to_string(),
        )]
        .into();

        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = run_with(
            &chat_args(true),
            &mut out,
            &mut err_out,
            repo.path(),
            &|k| env.get(k).cloned(),
        )
        .expect("past the guard, onto adapter resolution");
        assert_eq!(code, 1);
        let msg = String::from_utf8(err_out).expect("utf8");
        assert!(
            !msg.contains("refusing to start inside"),
            "the guard was overridden: {msg}"
        );
        assert!(msg.contains("disabled"), "got {msg}");
    }

    /// `--allow-nested` has to reach `wrap` too: `wrap::run_with` runs the
    /// identical guard against the identical environment, so an override that
    /// stopped here would simply be refused one layer down.
    #[test]
    fn the_override_is_threaded_through_to_the_wrap_arguments() {
        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let launch = build_launch(&adapter, None, &[]);
        for allow_nested in [false, true] {
            let wrap_args = wrap_args_for(&chat_args(allow_nested), launch.clone(), None);
            assert_eq!(
                wrap_args.allow_nested, allow_nested,
                "chat's own override has to reach wrap's identical guard"
            );
            assert_eq!(wrap_args.agent.as_deref(), Some("claude"));
            assert!(
                !wrap_args.no_supervise,
                "a chat session is always supervised"
            );
        }
    }

    #[test]
    fn quiet_folds_into_the_env_lookup_as_zirv_ctx_quiet() {
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let base = |k: &str| empty.get(k).cloned();
        let looked_up = quiet_env(&base, true);
        assert_eq!(looked_up("ZIRV_CTX_QUIET"), Some("true".to_string()));
        assert_eq!(looked_up("ZIRV_CTX_AGENT"), None, "other keys pass through");

        let not_quiet = quiet_env(&base, false);
        assert_eq!(
            not_quiet("ZIRV_CTX_QUIET"),
            None,
            "without --quiet the underlying lookup is untouched"
        );
    }

    #[test]
    fn an_interactive_quiet_flag_overrides_the_operators_stored_zirv_ctx_quiet_false() {
        // An operator who explicitly set ZIRV_CTX_QUIET=false is still
        // overridden by an interactive --quiet flag: the flag is this
        // invocation's own request, layered on top like any other override.
        let set: std::collections::HashMap<String, String> =
            [("ZIRV_CTX_QUIET".to_string(), "false".to_string())].into();
        let base = |k: &str| set.get(k).cloned();
        let looked_up = quiet_env(&base, true);
        assert_eq!(looked_up("ZIRV_CTX_QUIET"), Some("true".to_string()));
    }

    // Task 6: dashboard wiring -- model splice and the wrap fallback.

    fn cfg_with_model(model: Option<&str>) -> CtxConfig {
        let mut cfg = CtxConfig::default();
        cfg.chat.model = model.map(str::to_string);
        cfg
    }

    /// The configured model's flags land after the positional prompt and
    /// ahead of the operator's own `--` extras; no configured model leaves the
    /// argv byte-for-byte unchanged.
    #[test]
    fn orchestrator_argv_carries_the_configured_model() {
        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));

        let extra = extra_with_model(
            &cfg_with_model(Some("opus")),
            &adapter,
            &["--continue".to_string()],
        );
        let with_model = build_launch(&adapter, Some("hello"), &extra);
        assert_eq!(
            with_model.argv,
            vec![
                "/tmp/fake-claude".to_string(),
                "hello".to_string(),
                "--model".to_string(),
                "opus".to_string(),
                "--continue".to_string(),
            ],
            "model flags follow the prompt, the operator's extras still land last"
        );

        let plain = extra_with_model(&cfg_with_model(None), &adapter, &["--continue".to_string()]);
        let without_model = build_launch(&adapter, Some("hello"), &plain);
        assert_eq!(
            without_model.argv,
            build_launch(&adapter, Some("hello"), &["--continue".to_string()]).argv,
            "no configured model means the argv is untouched"
        );
    }

    /// R1, the shape the old splice broke on: a program whose real argv
    /// prefix is more than one token. `launch_prefix_len()` counts only what
    /// the operator wrote, so splicing at it dropped the model flags *inside*
    /// the launcher's own arguments. Appending them as trailing extras cannot:
    /// whatever the prefix turns out to be, the flags land after the prompt.
    #[test]
    fn model_flags_never_land_inside_a_multi_token_launch_prefix() {
        // `bin_args`: "sh /tmp/stub.sh" is program + one leading argument.
        let adapter = ClaudeAdapter::new(Some("sh /tmp/stub.sh"));
        let extra = extra_with_model(&cfg_with_model(Some("fable")), &adapter, &[]);
        let launch = build_launch(&adapter, Some("do the work"), &extra);
        assert_eq!(
            launch.argv,
            vec![
                "sh".to_string(),
                "/tmp/stub.sh".to_string(),
                "do the work".to_string(),
                "--model".to_string(),
                "fable".to_string(),
            ]
        );
    }

    /// The Windows launcher rewrite specifically: an npm-installed
    /// `claude.cmd` is spawned as `cmd.exe /c <shim> ...`, a three-token
    /// prefix against a `launch_prefix_len()` of 1. The old splice put
    /// `--model fable` between `cmd.exe` and `/c`, so `cmd.exe` was handed the
    /// model flags and the agent never started.
    #[cfg(windows)]
    #[test]
    fn model_flags_land_after_the_prompt_behind_the_windows_cmd_launcher() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shim = dir.path().join("claude.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");

        let adapter = ClaudeAdapter::new(Some(&shim.display().to_string()));
        let extra = extra_with_model(&cfg_with_model(Some("fable")), &adapter, &[]);
        let launch = build_launch(&adapter, Some("do the work"), &extra);

        assert_eq!(
            launch.argv,
            vec![
                launch.argv[0].clone(),
                "/c".to_string(),
                shim.display().to_string(),
                "do the work".to_string(),
                "--model".to_string(),
                "fable".to_string(),
            ],
            "the launcher prefix stays intact and the model flags trail the prompt"
        );
        assert!(
            launch.argv[0].to_lowercase().contains("cmd"),
            "the shim is routed through cmd.exe: {:?}",
            launch.argv[0]
        );
    }

    /// The dashboard eligibility gate is real terminal I/O
    /// (`std::io::stdout()`/`stdin().is_terminal()`), and `cargo test`'s own
    /// stdio is never a real terminal either way -- so under test, `zirv
    /// chat` always falls through to the `wrap` path regardless of
    /// `--simple`. This pins that the fallback is actually reached (not
    /// short-circuited by some other refusal) by letting adapter resolution
    /// succeed and following it all the way into `wrap::run_with`'s own
    /// spawn attempt, which fails fast because the binary does not exist --
    /// the fake-agent-bin pattern, never a real agent.
    #[test]
    fn simple_flag_still_reaches_the_wrap_fallback_path() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let state_tmp = tempfile::tempdir().expect("tempdir");
        let env: std::collections::HashMap<String, String> = [
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                "Z:/nonexistent/agent-bin".to_string(),
            ),
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state_tmp.path().display().to_string(),
            ),
        ]
        .into();

        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let result = run_with(
            &chat_args(false),
            &mut out,
            &mut err_out,
            repo.path(),
            &|k| env.get(k).cloned(),
        );
        // The dashboard is never reachable under `cargo test`'s own piped
        // stdio (`dash_eligible` requires a real terminal on both streams),
        // so this pins the wrap path specifically: it got far enough to
        // actually attempt the configured, nonexistent binary -- proof the
        // model splice and the dashboard branch above it did not divert or
        // corrupt the launch -- and failed there rather than anywhere
        // earlier (a disabled agent, the nesting guard, or a config error).
        let failure =
            result.expect_err("the configured binary does not exist, so the spawn must fail");
        let msg = failure.to_string();
        assert!(
            msg.contains("agent-bin") || msg.contains("Z:"),
            "expected the failure to name the configured (nonexistent) binary: {msg}"
        );
    }

    /// `chat_args(false).simple` is `false` above deliberately: the point is
    /// that even the default (non-`--simple`) path reaches `wrap` under
    /// non-terminal stdio. This companion pins that `--simple` explicitly
    /// set behaves the same way -- neither flag value changes which path a
    /// non-terminal `cargo test` run reaches.
    #[test]
    fn explicit_simple_also_reaches_the_wrap_fallback_path() {
        let repo = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let mut simple_args = chat_args(false);
        simple_args.simple = true;

        let state_tmp = tempfile::tempdir().expect("tempdir");
        let env: std::collections::HashMap<String, String> = [
            (
                "ZIRV_CTX_AGENT_BIN".to_string(),
                "Z:/nonexistent/agent-bin".to_string(),
            ),
            (
                crate::commands::ctx::state::STATE_ENV.to_string(),
                state_tmp.path().display().to_string(),
            ),
        ]
        .into();

        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let result = run_with(&simple_args, &mut out, &mut err_out, repo.path(), &|k| {
            env.get(k).cloned()
        });
        let failure =
            result.expect_err("the configured binary does not exist, so the spawn must fail");
        let msg = failure.to_string();
        assert!(
            msg.contains("agent-bin") || msg.contains("Z:"),
            "expected the failure to name the configured (nonexistent) binary: {msg}"
        );
    }

    /// Bug: the welcome banner used to mark every registered adapter live
    /// off `cfg.agents.is_enabled` alone, with no check that the binary
    /// exists -- on a machine with only a couple of harnesses installed,
    /// every other adapter still rendered a green `\u{25cf}`. `harness_list`
    /// now reuses `adapters::adapter_liveness` (the same issue #298 probe
    /// the injected roster gates on), so an enabled adapter confirmed absent
    /// is omitted entirely, while a disabled adapter still gets its
    /// `(name, false)` entry -- the banner keeps showing operators what they
    /// turned off, just not what they never installed.
    #[test]
    fn harness_list_omits_a_confirmed_absent_adapter_but_keeps_a_disabled_one() {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join(".zirv")).expect("mkdir");
        std::fs::write(
            repo.path().join(".zirv/.settings.toml"),
            "[agents.droid]\nenabled = false\n",
        )
        .expect("write settings");
        let settings_home = tempfile::tempdir().expect("tempdir");
        let cfg = {
            let _home = crate::commands::ctx::testenv::HomeGuard::set(settings_home.path());
            let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
            CtxConfig {
                agents: crate::settings::AgentGate::load(repo.path(), &|k| empty.get(k).cloned())
                    .expect("load"),
                ..CtxConfig::default()
            }
        };

        // Every adapter except codex gets a stub on `PATH`, and `HOME` moves
        // to a fresh temp dir so `adapters::known_install_roots`'s widened
        // codex search (see that function's own doc comment) cannot find a
        // real binary either -- codex is confirmed absent by construction.
        let path_dir = tempfile::tempdir().expect("tempdir");
        for (name, _) in adapters::ADAPTERS {
            if *name != "codex" {
                std::fs::write(path_dir.path().join(name), "").expect("write stub");
            }
        }
        let _path_guard = crate::commands::ctx::testenv::VarGuard::set(&[(
            "PATH",
            Some(path_dir.path().to_str().expect("utf8 tempdir path")),
        )]);
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let harnesses = harness_list(&cfg);

        assert!(
            !harnesses.iter().any(|(name, _)| name == "codex"),
            "codex is confirmed absent, so it must be omitted entirely: {harnesses:?}"
        );
        assert!(
            harnesses.contains(&("droid".to_string(), false)),
            "droid is disabled (not absent), so it must still be listed as off: {harnesses:?}"
        );
        assert!(
            harnesses.contains(&("claude".to_string(), true)),
            "claude is enabled and present, so it must still be listed as live: {harnesses:?}"
        );
    }

    // Issue #537 (T2a): the harness proxy's launch wiring.

    fn sample_decision(
        repo: &Path,
        harness: &str,
        model: &str,
        workflow: Option<&str>,
    ) -> ProxyDecision {
        ProxyDecision {
            request_sha256: "deadbeef".to_string(),
            repo: repo.to_path_buf(),
            intent: Intent::Feature,
            complexity: Complexity::Bounded,
            risk: RiskBand::Medium,
            execution: ExecutionMode::Bounded,
            // `Bounded` -> `SeatRole::Single`/`SeatTier::Standard`, mirroring
            // `decision::SeatRole::from_execution`/
            // `SeatTier::from_execution_complexity_risk` (both private to
            // that module) rather than re-deriving them.
            seat_role: SeatRole::Single,
            seat_tier: SeatTier::Standard,
            validation: ValidationProfile::default(),
            workflow: workflow.map(str::to_string),
            orchestrator: Seat {
                harness: harness.to_string(),
                model: model.to_string(),
            },
            worker_tier: Tier::Standard,
            needs_clarification: 0.0,
            needs_clarification_decisive: false,
            clarification_category: None,
            domains: Vec::new(),
            decider: Decider::Deterministic,
            confidence: BTreeMap::new(),
            reasons: Vec::new(),
            fallbacks: Vec::new(),
            elapsed_ms: 0,
            usage: None,
            created_at: 0,
        }
    }

    /// The overwhelmingly common case (the proxy never configured at all):
    /// `activation` refuses on `[proxy] enabled = false`, and `proxy_intake`
    /// folds that reason into `Inactive` rather than reading stdin at all.
    #[test]
    fn proxy_disabled_by_default_is_silently_inactive() {
        let cfg = CtxConfig::default();
        let repo = crate::commands::ctx::testenv::repo();
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let args = chat_args(false);
        let mut stderr = Vec::new();

        let outcome = proxy_intake(
            &cfg,
            &state,
            repo.path(),
            &args,
            false,
            &mut &b""[..],
            &mut stderr,
        )
        .expect("never errors");

        assert_eq!(
            outcome,
            ProxyIntakeOutcome::Inactive { advisory: None },
            "the disabled default must be byte-identical to today, announcements included"
        );
        assert!(
            stderr.is_empty(),
            "proxy_intake itself never prints the advisory -- that is the caller's job \
             (through the announce channel), so it must not touch stderr here"
        );
    }

    /// The mirror of the test above: `[proxy] enabled = true` (config or
    /// `--proxy`) but not actually usable (here, the deterministic decider,
    /// which `activation` always refuses) still gets the advisory -- an
    /// operator who turned the proxy on deserves to know why it never took
    /// over, unlike the silent, never-asked-for default.
    #[test]
    fn proxy_enabled_but_unusable_still_names_why() {
        let mut cfg = CtxConfig::default();
        cfg.proxy.enabled = true;
        cfg.proxy.decider = crate::commands::ctx::config::ProxyDecider::Deterministic;
        let repo = crate::commands::ctx::testenv::repo();
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let args = chat_args(false);
        let mut stderr = Vec::new();

        let outcome = proxy_intake(
            &cfg,
            &state,
            repo.path(),
            &args,
            false,
            &mut &b""[..],
            &mut stderr,
        )
        .expect("never errors");

        match outcome {
            ProxyIntakeOutcome::Inactive {
                advisory: Some(reason),
            } => assert!(reason.contains("deterministic"), "got {reason}"),
            other => panic!("expected Inactive with a reason, got {other:?}"),
        }
    }

    /// `--resume`'s own first prompt is the stored handoff; the proxy must
    /// never insert its own intake step ahead of it. Naming the reason is
    /// conditional on `--proxy` -- proven by the companion test below.
    #[test]
    fn resume_with_an_explicit_proxy_request_is_skipped_with_a_named_reason() {
        let cfg = CtxConfig::default();
        let repo = crate::commands::ctx::testenv::repo();
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let mut args = chat_args(false);
        args.resume = true;
        args.proxy = true;
        let mut stderr = Vec::new();

        let outcome = proxy_intake(
            &cfg,
            &state,
            repo.path(),
            &args,
            false,
            &mut &b""[..],
            &mut stderr,
        )
        .expect("never errors");

        match outcome {
            ProxyIntakeOutcome::Inactive {
                advisory: Some(reason),
            } => assert!(reason.contains("--resume"), "got {reason}"),
            other => panic!("expected Inactive naming --resume, got {other:?}"),
        }
    }

    /// The mirror of the test above: `--simple` alone (no explicit
    /// `--proxy`) must skip silently -- an operator who never asked for the
    /// proxy should not see it mentioned at all.
    #[test]
    fn simple_alone_skips_silently_with_no_explicit_proxy_request() {
        let cfg = CtxConfig::default();
        let repo = crate::commands::ctx::testenv::repo();
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let mut args = chat_args(false);
        args.simple = true;
        let mut stderr = Vec::new();

        let outcome = proxy_intake(
            &cfg,
            &state,
            repo.path(),
            &args,
            false,
            &mut &b""[..],
            &mut stderr,
        )
        .expect("never errors");

        assert_eq!(
            outcome,
            ProxyIntakeOutcome::Inactive { advisory: None },
            "no explicit --proxy means no advisory at all"
        );
    }

    /// `--proxy` and `--no-proxy` name the same underlying `enabled`
    /// override in opposite directions; clap must refuse both together
    /// rather than silently letting one win.
    #[test]
    fn proxy_and_no_proxy_together_is_a_clap_conflict() {
        use clap::Parser;
        let result = crate::commands::ctx::CtxCli::try_parse_from([
            "zirv ctx",
            "chat",
            "--proxy",
            "--no-proxy",
        ]);
        assert!(
            result.is_err(),
            "clap must refuse --proxy together with --no-proxy"
        );
    }

    /// Activation succeeding (a usable Typesafe model configured, credential
    /// present) but stdin not a terminal leaves nowhere to read the task
    /// description from: `run_with` must refuse the whole launch rather than
    /// silently falling back, which would look like the proxy was never
    /// asked for at all.
    #[test]
    fn activation_ok_but_non_tty_stdin_refuses_the_launch() {
        let mut cfg = CtxConfig::default();
        cfg.proxy.enabled = true;
        let _cred = crate::commands::ctx::testenv::VarGuard::set(&[(
            cfg.proxy.typesafe.credential_env.as_str(),
            Some("a-test-key"),
        )]);
        let repo = crate::commands::ctx::testenv::repo();
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let args = chat_args(false);
        let mut stderr = Vec::new();

        let outcome = proxy_intake(
            &cfg,
            &state,
            repo.path(),
            &args,
            false,
            &mut &b""[..],
            &mut stderr,
        )
        .expect("never errors");

        match outcome {
            ProxyIntakeOutcome::Refuse { message } => {
                assert!(message.contains("interactive terminal"), "got {message}");
            }
            other => panic!("expected Refuse, got {other:?}"),
        }
    }

    /// Issue #537 review (operator field report): nothing on screen showed
    /// that a request was actually sent to the decider, so a slow or
    /// falling-back `decide()` looked identical to a hung session.
    /// `proxy_intake` must print `proxy::asking_line` on stderr immediately
    /// before calling `decide()`, even when every model decider falls
    /// through to the deterministic floor. A connection-refused loopback
    /// address keeps `decide()`'s own Typesafe attempt instant rather than
    /// waiting out `timeout_secs`, so this stays fast and network-free; the
    /// "never when inactive" half is already proven by the disabled/resume/
    /// simple/non-tty tests above, all of which assert `stderr` is empty or
    /// carries only their own named reason.
    #[test]
    fn the_asking_line_is_announced_before_decide_runs() {
        let mut cfg = CtxConfig::default();
        cfg.proxy.enabled = true;
        cfg.proxy.typesafe.base_url = "http://127.0.0.1:1".to_string();
        cfg.proxy.typesafe.timeout_secs = 1;
        let _cred = crate::commands::ctx::testenv::VarGuard::set(&[(
            cfg.proxy.typesafe.credential_env.as_str(),
            Some("a-test-key"),
        )]);
        let repo = crate::commands::ctx::testenv::repo();
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let args = chat_args(false);
        let mut stderr = Vec::new();

        let outcome = proxy_intake(
            &cfg,
            &state,
            repo.path(),
            &args,
            true,
            &mut &b"fix the flaky retry test\n"[..],
            &mut stderr,
        )
        .expect("never errors");

        assert!(
            matches!(outcome, ProxyIntakeOutcome::Decided { .. }),
            "the deterministic floor never fails: {outcome:?}"
        );
        let printed = String::from_utf8(stderr).expect("utf8");
        assert!(
            printed.contains(&proxy::asking_line(&cfg)),
            "the asking line must reach the operator before decide() runs: {printed}"
        );
    }

    /// Issue #701 (operator field report): the launch looked like the proxy
    /// had never run at all. A blank FIRST line -- the reflexive Enter at an
    /// unexpected prompt, or a stray newline a console line editor left in
    /// the input buffer -- used to skip the proxy outright, and the one-line
    /// advisory saying so is wiped by the harness's alternate screen a
    /// moment later. `proxy_intake` must re-prompt once and decide on the
    /// request that follows.
    #[test]
    fn a_blank_first_line_reprompts_instead_of_skipping_the_proxy() {
        let mut cfg = CtxConfig::default();
        cfg.proxy.enabled = true;
        cfg.proxy.typesafe.base_url = "http://127.0.0.1:1".to_string();
        cfg.proxy.typesafe.timeout_secs = 1;
        let _cred = crate::commands::ctx::testenv::VarGuard::set(&[(
            cfg.proxy.typesafe.credential_env.as_str(),
            Some("a-test-key"),
        )]);
        let repo = crate::commands::ctx::testenv::repo();
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let args = chat_args(false);
        let mut stderr = Vec::new();

        let outcome = proxy_intake(
            &cfg,
            &state,
            repo.path(),
            &args,
            true,
            &mut &b"
fix the flaky retry test
"[..],
            &mut stderr,
        )
        .expect("never errors");

        match outcome {
            ProxyIntakeOutcome::Decided { request, .. } => {
                assert_eq!(request, "fix the flaky retry test");
            }
            other => panic!("expected Decided after the re-prompt, got {other:?}"),
        }
        let printed = String::from_utf8(stderr).expect("utf8");
        assert!(
            printed.contains("nothing typed"),
            "the re-prompt must say why it is asking again: {printed}"
        );
    }

    /// The other half of the test above: a deliberate skip is still one
    /// keypress away -- a SECOND blank line (or EOF) falls through to the
    /// ordinary harness launch with the same named advisory as before.
    #[test]
    fn a_second_blank_line_still_skips_the_proxy() {
        let mut cfg = CtxConfig::default();
        cfg.proxy.enabled = true;
        cfg.proxy.typesafe.base_url = "http://127.0.0.1:1".to_string();
        cfg.proxy.typesafe.timeout_secs = 1;
        let _cred = crate::commands::ctx::testenv::VarGuard::set(&[(
            cfg.proxy.typesafe.credential_env.as_str(),
            Some("a-test-key"),
        )]);
        let repo = crate::commands::ctx::testenv::repo();
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let args = chat_args(false);
        let mut stderr = Vec::new();

        let outcome = proxy_intake(
            &cfg,
            &state,
            repo.path(),
            &args,
            true,
            &mut &b"

"[..],
            &mut stderr,
        )
        .expect("never errors");

        match outcome {
            ProxyIntakeOutcome::Inactive {
                advisory: Some(reason),
            } => {
                assert!(reason.contains("no request given"), "got {reason}");
            }
            other => panic!("expected an Inactive skip, got {other:?}"),
        }
    }

    /// Issue #537 (A2): below `proxy::CLARIFY_THRESHOLD`, `maybe_clarify` is
    /// a complete no-op -- no prompt, `decision`/`request` unchanged --
    /// regardless of what stdin holds; at or above it with an empty answer
    /// (just Enter), the prompt still prints but the outcome is the same
    /// no-op, since an operator with nothing to add must not be forced to
    /// add something.
    #[test]
    fn maybe_clarify_is_a_no_op_below_the_threshold_or_on_an_empty_answer() {
        let repo = crate::commands::ctx::testenv::repo();
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let cfg = CtxConfig::default();

        let mut decision = sample_decision(repo.path(), "claude", "fable", None);
        decision.needs_clarification = 0.1;
        let mut stderr = Vec::new();
        let (unchanged, request) = maybe_clarify(
            &cfg,
            &state,
            repo.path(),
            decision.clone(),
            "original request".to_string(),
            &mut &b"ignored, never read below the threshold\n"[..],
            &mut stderr,
        )
        .expect("never errors");
        assert_eq!(unchanged, decision);
        assert_eq!(request, "original request");
        assert!(stderr.is_empty(), "below the threshold, no prompt at all");

        let mut ambiguous = decision;
        ambiguous.needs_clarification = 0.9;
        ambiguous.needs_clarification_decisive = true;
        let mut stderr = Vec::new();
        let (unchanged, request) = maybe_clarify(
            &cfg,
            &state,
            repo.path(),
            ambiguous.clone(),
            "original request".to_string(),
            &mut &b"\n"[..],
            &mut stderr,
        )
        .expect("never errors");
        assert_eq!(unchanged, ambiguous);
        assert_eq!(request, "original request");
        assert!(
            !stderr.is_empty(),
            "the prompt itself must still print even when the answer is empty"
        );
    }

    /// Jev determinism fix: at or above `proxy::CLARIFY_THRESHOLD`, a
    /// `needs_clarification` answer that was NOT decisive at merge time
    /// (thin margin between "ambiguous" and "clear enough") must never fire
    /// the interactive round -- the raw value alone is not enough once
    /// `needs_clarification_decisive` is `false`.
    #[test]
    fn maybe_clarify_is_a_no_op_above_the_threshold_when_not_decisive() {
        let repo = crate::commands::ctx::testenv::repo();
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let cfg = CtxConfig::default();

        let mut decision = sample_decision(repo.path(), "claude", "fable", None);
        decision.needs_clarification = 0.9;
        decision.needs_clarification_decisive = false;
        let mut stderr = Vec::new();
        let (unchanged, request) = maybe_clarify(
            &cfg,
            &state,
            repo.path(),
            decision.clone(),
            "original request".to_string(),
            &mut &b"ignored, never read when not decisive\n"[..],
            &mut stderr,
        )
        .expect("never errors");
        assert_eq!(unchanged, decision);
        assert_eq!(request, "original request");
        assert!(
            stderr.is_empty(),
            "a non-decisive answer must never prompt, however high its raw value"
        );
    }

    #[test]
    fn material_ambiguity_category_asks_a_fixed_question_before_launch() {
        let repo = crate::commands::ctx::testenv::repo();
        let state_tmp = tempfile::tempdir().expect("state");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let mut cfg = CtxConfig::default();
        cfg.jev.intake_savings = true;
        cfg.proxy.typesafe.credential_env = "JEV_TEST_KEY_CHAT_CLARIFY_EMPTY".to_string();
        unsafe { std::env::set_var("JEV_TEST_KEY_CHAT_CLARIFY_EMPTY", "test-key") };
        let mut decision = sample_decision(repo.path(), "claude", "fable", None);
        decision.decider = proxy::decision::Decider::Typesafe;
        decision.needs_clarification = 0.9;
        decision.needs_clarification_decisive = true;
        decision.clarification_category = Some("target".to_string());
        let mut stderr = Vec::new();
        let (unchanged, request) = maybe_clarify(
            &cfg,
            &state,
            repo.path(),
            decision.clone(),
            "change the service".to_string(),
            &mut &b"\n"[..],
            &mut stderr,
        )
        .expect("clarification prompt");
        assert_eq!(unchanged, decision);
        assert_eq!(request, "change the service");
        assert!(
            String::from_utf8(stderr)
                .expect("utf8")
                .contains("Which service or files")
        );
        let effects = std::fs::read_to_string(state.root().join("jev-effects.jsonl"))
            .expect("clarification effects");
        assert!(effects.contains("\"action\":\"requested\""));
        assert!(effects.contains("\"action\":\"unanswered\""));
        unsafe { std::env::remove_var("JEV_TEST_KEY_CHAT_CLARIFY_EMPTY") };
    }

    #[test]
    fn nonempty_clarification_records_answer_before_redeciding() {
        let repo = crate::commands::ctx::testenv::repo();
        let state_tmp = tempfile::tempdir().expect("state");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let mut cfg = CtxConfig::default();
        cfg.jev.intake_savings = true;
        cfg.proxy.decider = crate::commands::ctx::config::ProxyDecider::Deterministic;
        cfg.proxy.typesafe.credential_env = "JEV_TEST_KEY_CHAT_CLARIFY_ANSWER".to_string();
        unsafe { std::env::set_var("JEV_TEST_KEY_CHAT_CLARIFY_ANSWER", "test-key") };
        let mut decision = sample_decision(repo.path(), "claude", "fable", None);
        decision.decider = proxy::decision::Decider::Typesafe;
        decision.needs_clarification = 0.9;
        decision.needs_clarification_decisive = true;
        let (_, request) = maybe_clarify(
            &cfg,
            &state,
            repo.path(),
            decision,
            "change service".to_string(),
            &mut &b"service A; preserve API B\n"[..],
            &mut Vec::new(),
        )
        .expect("clarify");
        unsafe { std::env::remove_var("JEV_TEST_KEY_CHAT_CLARIFY_ANSWER") };
        assert_eq!(request, "change service\n\nservice A; preserve API B");
        let effects = std::fs::read_to_string(state.root().join("jev-effects.jsonl"))
            .expect("clarification effects");
        assert!(effects.contains("\"action\":\"requested\""));
        assert!(effects.contains("\"action\":\"answered\""));
        assert!(!effects.contains("service A"));
    }

    /// Issue #537 (A2): at or above the threshold, a non-empty answer is
    /// appended to the request (separated by a blank line) and `decide` runs
    /// exactly once more against the combined text -- never a second
    /// clarification round, however ambiguous the new decision still looks.
    #[test]
    fn maybe_clarify_appends_a_non_empty_answer_and_redecides_once() {
        let repo = crate::commands::ctx::testenv::repo();
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let mut cfg = CtxConfig::default();
        // Deterministic only: this test must never depend on network access
        // or an ambient credential, and must be reproducible.
        cfg.proxy.decider = crate::commands::ctx::config::ProxyDecider::Deterministic;

        let mut decision = sample_decision(repo.path(), "claude", "fable", None);
        decision.needs_clarification = 0.9;
        decision.needs_clarification_decisive = true;
        let mut stderr = Vec::new();

        let (new_decision, new_request) = maybe_clarify(
            &cfg,
            &state,
            repo.path(),
            decision,
            "original request".to_string(),
            &mut &b"more detail here\n"[..],
            &mut stderr,
        )
        .expect("never errors");

        assert_eq!(new_request, "original request\n\nmore detail here");
        // `elapsed_ms`/`created_at` legitimately differ between two separate
        // `decide()` calls; every other field must match exactly.
        let mut new_decision = new_decision;
        let mut expected = proxy::decide(&cfg, state.root(), repo.path(), &new_request);
        new_decision.elapsed_ms = 0;
        expected.elapsed_ms = 0;
        new_decision.created_at = 0;
        expected.created_at = 0;
        assert_eq!(new_decision, expected, "must redecide on the combined text");
        let printed = String::from_utf8(stderr).expect("utf8");
        assert!(printed.contains("ambiguous (0.90)"), "{printed}");
    }

    /// The wiring `run_with` applies once the proxy actually decided this
    /// launch: the decided model replaces `cfg.chat.model` and the decided
    /// harness is returned for `resolve_adapter`, so `extra_with_model`/
    /// `build_launch` (already covered by their own tests) put the decided
    /// `--model` in argv and the request text becomes the initial prompt.
    #[test]
    fn an_injected_proxy_decision_lands_the_decided_model_and_request_in_the_built_launch() {
        let repo = crate::commands::ctx::testenv::repo();
        let decision = sample_decision(repo.path(), "claude", "fable", Some("bugfix"));
        let mut cfg = CtxConfig::default();

        let requested_agent = apply_proxy_decision(&mut cfg, &decision);

        assert_eq!(requested_agent, "claude");
        assert_eq!(cfg.chat.model.as_deref(), Some("fable"));

        let adapter = ClaudeAdapter::new(Some("/tmp/fake-claude"));
        let extra = extra_with_model(&cfg, &adapter, &[]);
        let launch = build_launch(&adapter, Some("fix the flaky retry test"), &extra);

        assert!(
            launch
                .argv
                .windows(2)
                .any(|pair| pair == ["--model", "fable"]),
            "the decided model must land in argv: {:?}",
            launch.argv
        );
        assert!(
            launch
                .argv
                .contains(&"fix the flaky retry test".to_string()),
            "the request text must become the initial prompt: {:?}",
            launch.argv
        );
    }

    /// `apply_proxy_decision` is a no-op on `cfg.chat.model` for any field
    /// it does not touch: the proxy only ever replaces the model, never
    /// anything else on `cfg`.
    #[test]
    fn apply_proxy_decision_only_touches_the_chat_model() {
        let repo = crate::commands::ctx::testenv::repo();
        let decision = sample_decision(repo.path(), "codex", "o-fast", None);
        let mut cfg = CtxConfig::default();
        cfg.chat.model = Some("stale".to_string());

        let requested_agent = apply_proxy_decision(&mut cfg, &decision);

        assert_eq!(requested_agent, "codex");
        assert_eq!(cfg.chat.model.as_deref(), Some("o-fast"));
    }

    /// Issue #537 (T3): `run_with` reads the seat this launch runs as
    /// straight off `proxy_prompt_role` -- `SeatRole::Single` maps to
    /// `PromptRole::Single`, `SeatRole::Orchestrator` keeps today's
    /// `Orchestrator`, and an intake that never decided this launch at all
    /// (the disabled default, or every other `Inactive`/`Refuse` outcome)
    /// also keeps `Orchestrator`, unchanged.
    #[test]
    fn proxy_prompt_role_maps_seat_role_and_leaves_an_undecided_launch_alone() {
        let repo = crate::commands::ctx::testenv::repo();
        let mut decision = sample_decision(repo.path(), "claude", "fable", None);
        assert_eq!(
            decision.seat_role,
            SeatRole::Single,
            "sample_decision's Bounded execution is a Single seat"
        );

        let single = ProxyIntakeOutcome::Decided {
            decision: Box::new(decision.clone()),
            request: "fix a typo".to_string(),
        };
        assert_eq!(proxy_prompt_role(&single), PromptRole::Single);

        decision.seat_role = SeatRole::Orchestrator;
        let orchestrated = ProxyIntakeOutcome::Decided {
            decision: Box::new(decision),
            request: "redesign the billing pipeline".to_string(),
        };
        assert_eq!(proxy_prompt_role(&orchestrated), PromptRole::Orchestrator);

        assert_eq!(
            proxy_prompt_role(&ProxyIntakeOutcome::Inactive { advisory: None }),
            PromptRole::Orchestrator
        );
    }

    /// Issue #537 (T3, operator field report): the actual bug -- a direct/
    /// bounded decision started "the full orchestrator setup" -- reproduced
    /// and fixed at the launch level. A `SeatRole::Single` decision must
    /// launch with no harness meta-teaching (`HARNESS_PROMPT`, "zirv meta-
    /// harness"), no adapter orchestrator-conventions layer (`ORCHESTRATOR_
    /// PROMPT`, "it does not implement"), and its role must reach
    /// `adapters::seat_role_env` as `"single"` -- the same env pair the
    /// write guard and the subagent guard key off (see `hook.rs`'s `run_
    /// pretool_stays_silent_for_a_single_seat_editing_a_repo_file`). Same
    /// recipe `the_dash_orchestrator_pane_carries_the_composed_prompt`
    /// already proves for an `Orchestrator` decision, which this test's
    /// companion assertions confirm is still unaffected.
    #[test]
    fn a_decided_single_seat_launches_with_no_orchestrator_conventions() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));

        let decision = sample_decision(tmp.path(), "claude", "fable", None);
        let outcome = ProxyIntakeOutcome::Decided {
            decision: Box::new(decision),
            request: "fix a typo in README".to_string(),
        };
        let role = proxy_prompt_role(&outcome);
        assert_eq!(role, PromptRole::Single);

        let mut launch = build_launch(&adapter, None, &[]);
        launch.role = role;
        let pane = dash_orchestrator_pane(
            &adapter,
            launch,
            &cfg,
            &state,
            tmp.path(),
            "11111111-2222-4333-8444-555555555555",
            false,
            None,
        )
        .expect("pane");

        let argv = pane.argv.join(" ");
        assert!(
            argv.contains("zirv engineering standard"),
            "the shipped default layer must still apply to a single seat: {argv}"
        );
        assert!(
            !argv.contains("zirv meta-harness"),
            "a single seat must not get the harness delegation layer: {argv}"
        );
        assert!(
            !argv.contains("it does not implement"),
            "a single seat must not get the orchestrator's own conventions layer: {argv}"
        );
        assert_eq!(pane.role, PromptRole::Single);
        assert_eq!(
            adapters::seat_role_env(pane.role),
            vec![(adapters::SEAT_ROLE_ENV.to_string(), "single".to_string())],
            "the hook write guard and the subagent guard both key off this env pair"
        );
    }

    /// Issue #537 (T3, native seam): the actual bug this fixes -- a native
    /// launch's `PaneSpec`/`NativeDashboardSpec` used to hardcode
    /// `Orchestrator` no matter what the proxy decided, because `proxy_
    /// intake` never even ran on that path. Reproduced at the seam that
    /// actually builds those two structs (`native_pane_spec`), fed the SAME
    /// `proxy_prompt_role(&Decided{SeatRole::Single, ..})` the wrapped
    /// path's own `a_decided_single_seat_launches_with_no_orchestrator_
    /// conventions` test above proves for `dash_orchestrator_pane`.
    #[test]
    fn native_pane_spec_uses_the_single_seat_role_for_a_decided_single_seat() {
        let repo = crate::commands::ctx::testenv::repo();
        let decision = sample_decision(repo.path(), "claude", "fable", None);
        let outcome = ProxyIntakeOutcome::Decided {
            decision: Box::new(decision),
            request: "fix a typo in README".to_string(),
        };
        let seat_role = proxy_prompt_role(&outcome);
        assert_eq!(seat_role, PromptRole::Single);

        let (pane, native) =
            native_pane_spec(repo.path(), "session-1".to_string(), seat_role, None);

        assert_eq!(pane.role, PromptRole::Single);
        assert_eq!(
            native.role, "single",
            "NativeDashboardSpec.role must carry PromptRole::Single's own \
             label, not a hardcoded string"
        );
    }

    /// The mirror of the test above: an intake that never decided this
    /// launch at all (the disabled-by-default case, exercised end to end by
    /// `proxy_disabled_by_default_is_silently_inactive`) must still produce
    /// today's `Orchestrator` seat on the native pane -- not just leave
    /// `proxy_prompt_role` unchanged (already proven by `proxy_prompt_role_
    /// maps_seat_role_and_leaves_an_undecided_launch_alone`), but actually
    /// carry that role through into both fields `run_native_chat` builds.
    #[test]
    fn native_pane_spec_keeps_the_orchestrator_role_when_the_proxy_never_decided() {
        let repo = crate::commands::ctx::testenv::repo();
        let seat_role = proxy_prompt_role(&ProxyIntakeOutcome::Inactive { advisory: None });
        assert_eq!(seat_role, PromptRole::Orchestrator);

        let (pane, native) =
            native_pane_spec(repo.path(), "session-2".to_string(), seat_role, None);

        assert_eq!(pane.role, PromptRole::Orchestrator);
        assert_eq!(native.role, "orchestrator");
    }

    /// Issue #703 (follow-up to #702): the actual bug this fixes -- a native
    /// launch's `NativeDashboardSpec` used to hardcode `route: None` no
    /// matter what model the proxy decided, because `native_pane_spec` never
    /// read `ProxyDecision.orchestrator.model` at all. `proxy_decided_model`
    /// is the seam `run_native_chat` now feeds `native_pane_spec` from, the
    /// same way `proxy_prompt_role` already feeds it the seat role.
    #[test]
    fn proxy_decided_model_reaches_the_native_pane_spec_as_a_route_candidate() {
        let repo = crate::commands::ctx::testenv::repo();
        let decision = sample_decision(repo.path(), "claude", "fable", None);
        let outcome = ProxyIntakeOutcome::Decided {
            decision: Box::new(decision),
            request: "fix a typo in README".to_string(),
        };

        let model = proxy_decided_model(&outcome);
        assert_eq!(model.as_deref(), Some("fable"));

        let (_, native) = native_pane_spec(
            repo.path(),
            "session-3".to_string(),
            PromptRole::Single,
            model,
        );

        assert_eq!(
            native.route.as_deref(),
            Some("fable"),
            "the decided model must reach NativeDashboardSpec::route as the \
             candidate NativePaneRuntime::spawn validates"
        );
    }

    /// The mirror of the test above: an intake that never decided this
    /// launch leaves `proxy_decided_model` (and therefore the pane's route)
    /// `None` -- the role's own configured default route, unchanged.
    #[test]
    fn proxy_decided_model_is_none_when_the_proxy_never_decided() {
        assert_eq!(
            proxy_decided_model(&ProxyIntakeOutcome::Inactive { advisory: None }),
            None
        );
    }

    fn git_init_with_commit(repo: &Path) {
        let git = |cmd_args: &[&str]| {
            let status = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(cmd_args)
                .current_dir(repo)
                .status()
                .expect("run git");
            assert!(status.success(), "git {cmd_args:?} failed");
        };
        git(&["init", "-q"]);
        std::fs::write(repo.join("README.md"), "hello\n").expect("write");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "base"]);
    }

    /// `start_proxy_workflow` is a plain no-op when the intake never decided
    /// this launch: no workflow is ever touched, and `close_proxy_workflow_
    /// on_failure` (given the resulting `None`) is a plain pass-through to
    /// `spawn`.
    #[test]
    fn start_proxy_workflow_is_a_no_op_when_inactive() {
        let repo = tempfile::tempdir().expect("tempdir");
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let outcome = ProxyIntakeOutcome::Inactive { advisory: None };
        let mut announced: Vec<String> = Vec::new();

        let started_id =
            start_proxy_workflow(&outcome, &state, repo.path(), |text| announced.push(text));
        assert_eq!(started_id, None);

        let result = close_proxy_workflow_on_failure(
            started_id.as_deref(),
            &state,
            repo.path(),
            |text| announced.push(text),
            || Ok(42),
        );

        assert_eq!(result.expect("passthrough"), 42);
        assert!(
            announced.is_empty(),
            "inactive never announces anything: {announced:?}"
        );
    }

    /// A failed spawn must close the workflow this launch started -- an
    /// orphaned "active" workflow left behind by a launch that never
    /// actually started would otherwise block every later `zirv chat`
    /// (proxy or not) that reaches the same repo, since `engine::
    /// start_workflow` never overwrites an existing active pointer.
    #[test]
    fn close_proxy_workflow_on_failure_closes_a_workflow_it_started_when_the_spawn_fails() {
        let repo = tempfile::tempdir().expect("tempdir");
        git_init_with_commit(repo.path());
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        // Trivial/Low keeps the built-in `bugfix` pack's conditional,
        // approval-gated `intent` step out of the materialized plan, so
        // this starts `Running` -- `engine::close` refuses to close a
        // workflow still `AwaitingApproval`.
        let mut decision = sample_decision(repo.path(), "claude", "fable", Some("bugfix"));
        decision.complexity = Complexity::Trivial;
        decision.risk = RiskBand::Low;
        let outcome = ProxyIntakeOutcome::Decided {
            decision: Box::new(decision),
            request: "fix a database retry bug".to_string(),
        };

        let mut announced: Vec<String> = Vec::new();
        let started_id =
            start_proxy_workflow(&outcome, &state, repo.path(), |text| announced.push(text));
        assert!(started_id.is_some(), "expected a started workflow id");

        let result: CtxResult<i32> = close_proxy_workflow_on_failure(
            started_id.as_deref(),
            &state,
            repo.path(),
            |text| announced.push(text),
            || Err("spawn failed".into()),
        );
        assert!(result.is_err());

        let active =
            crate::commands::workflow::engine::load_active(&state, repo.path()).expect("readable");
        assert!(
            active.is_none(),
            "a failed spawn must clear the active pointer via close_started"
        );
        assert!(
            announced.is_empty(),
            "close_started succeeded, so nothing needs announcing: {announced:?}"
        );
    }

    /// The mirror of the test above: a successful spawn leaves the started
    /// workflow running -- `close_proxy_workflow_on_failure` must never
    /// close a launch that actually succeeded.
    #[test]
    fn close_proxy_workflow_on_failure_leaves_a_successful_launch_s_workflow_running() {
        let repo = tempfile::tempdir().expect("tempdir");
        git_init_with_commit(repo.path());
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());
        let mut decision = sample_decision(repo.path(), "claude", "fable", Some("bugfix"));
        decision.complexity = Complexity::Trivial;
        decision.risk = RiskBand::Low;
        let outcome = ProxyIntakeOutcome::Decided {
            decision: Box::new(decision),
            request: "fix a database retry bug".to_string(),
        };

        let mut announced: Vec<String> = Vec::new();
        let started_id =
            start_proxy_workflow(&outcome, &state, repo.path(), |text| announced.push(text));
        assert!(started_id.is_some(), "expected a started workflow id");

        let result: CtxResult<i32> = close_proxy_workflow_on_failure(
            started_id.as_deref(),
            &state,
            repo.path(),
            |text| announced.push(text),
            || Ok(0),
        );
        assert_eq!(result.expect("spawn succeeded"), 0);

        let active = crate::commands::workflow::engine::load_active(&state, repo.path())
            .expect("readable")
            .expect("the started workflow is still active");
        assert_eq!(
            active.status,
            crate::commands::workflow::engine::WorkflowStatus::Running
        );
        assert!(
            announced.is_empty(),
            "started + successful spawn must stay silent: {announced:?}"
        );
    }

    /// Review fix (findings 1 & 2): `run_with`'s persistent-runtime attempt
    /// is never the last launch shape tried, so its own failure must not
    /// close the workflow -- proven here by simulating that failure as a
    /// plain no-op (exactly what the fixed `run_with` does: it calls
    /// `chat_via_runtime` directly, with no `close_proxy_workflow_on_failure`
    /// wrapper) and asserting the workflow is still `Running` afterwards.
    /// The dashboard branch IS the last shape once it is reached, so building
    /// its pane is folded into the SAME `close_proxy_workflow_on_failure`
    /// call as `dash::run_dashboard`, via [`run_dash_branch`] -- the exact
    /// function `run_with` itself calls -- proven by forcing `dash_
    /// orchestrator_pane` itself to fail (`protect_composed` refuses when the
    /// operator's own obfuscation config failed to load, `cfg.obfuscate.
    /// operator_load_failed`, a deterministic failure with no filesystem
    /// trickery needed) and asserting that THIS closes the still-running
    /// workflow. Before the fix, a `dash_orchestrator_pane` failure sat
    /// outside the wrapper's own `?` and left the workflow orphaned forever
    /// (the engine never overwrites an active pointer).
    ///
    /// `run_with` itself is not driven end to end here: `chat_route`/
    /// `dash_eligible` both require a real interactive terminal on both
    /// streams, which `cargo test`'s piped stdio never provides (see
    /// `run_with_discloses_the_model_before_it_picks_a_launch_path`'s own
    /// doc comment for the same limitation) -- but [`run_dash_branch`] is a
    /// free function with no TTY dependency of its own, so it is driven
    /// directly, exercising the real composition rather than a re-
    /// implementation of it.
    #[test]
    fn a_failed_runtime_attempt_leaves_the_workflow_live_for_the_dash_branch_that_actually_runs() {
        let repo = crate::commands::ctx::testenv::repo();
        git_init_with_commit(repo.path());
        let home = repo.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(repo.path().join("state"));
        let mut decision = sample_decision(repo.path(), "claude", "fable", Some("bugfix"));
        decision.complexity = Complexity::Trivial;
        decision.risk = RiskBand::Low;
        let outcome = ProxyIntakeOutcome::Decided {
            decision: Box::new(decision),
            request: "fix a database retry bug".to_string(),
        };

        let mut announced: Vec<String> = Vec::new();
        let started_id =
            start_proxy_workflow(&outcome, &state, repo.path(), |text| announced.push(text));
        assert!(started_id.is_some(), "expected a started workflow id");

        // The runtime attempt "fails" -- nothing here closes anything, which
        // is the fix: `close_proxy_workflow_on_failure` is never called
        // around it any more.
        let running_after_runtime_failure =
            crate::commands::workflow::engine::load_active(&state, repo.path())
                .expect("readable")
                .expect("still active: the runtime attempt's own failure must not touch it");
        assert_eq!(
            running_after_runtime_failure.status,
            crate::commands::workflow::engine::WorkflowStatus::Running
        );

        // The dash branch, driven through the real `run_dash_branch` -- pane
        // build folded into the same wrapped closure as `dash::run_
        // dashboard`, exactly as `run_with` now composes it.
        let adapter = ClaudeAdapter::new(Some("/nonexistent/fake-claude"));
        let launch = build_launch(&adapter, None, &[]);
        let mut cfg = CtxConfig::default();
        cfg.obfuscate.mode = super::super::config::ObfuscateMode::Flag;
        cfg.obfuscate.operator_load_failed = true;
        let env: std::collections::HashMap<String, String> = Default::default();

        let result = run_dash_branch(
            &adapter,
            launch,
            &cfg,
            &state,
            repo.path(),
            &|k| env.get(k).cloned(),
            "11111111-2222-4333-8444-555555555555",
            false,
            None,
            None,
            started_id.as_deref(),
            false,
            |text| announced.push(text),
        );
        assert!(
            result.is_err(),
            "the forced pane-build failure must propagate: {result:?}"
        );

        let active =
            crate::commands::workflow::engine::load_active(&state, repo.path()).expect("readable");
        assert!(
            active.is_none(),
            "a pane-build failure in the launch shape that actually runs must close the \
             workflow, not orphan it"
        );
    }

    /// Issue #537 review: a `Skipped` workflow start (an active workflow
    /// already on the repo) must not vanish silently -- the operator has no
    /// other way to learn the proxy's own decision never actually started a
    /// workflow, unlike `runtime/native.rs`, which already announces this
    /// case.
    #[test]
    fn start_proxy_workflow_announces_a_skipped_start() {
        let repo = tempfile::tempdir().expect("tempdir");
        git_init_with_commit(repo.path());
        let state_tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(state_tmp.path().to_path_buf());

        // Seed an existing active workflow so `start_workflow_for` skips.
        let existing = crate::commands::workflow::engine::start_workflow(
            &state,
            &crate::commands::workflow::engine::StartArgs {
                id: Some("bugfix".to_string()),
                task: "an earlier launch's workflow".to_string(),
                agent: None,
                built_in_only: true,
                repo: Some(repo.path().to_path_buf()),
                paths: vec![std::path::PathBuf::from("README.md")],
                changed_lines: Some(1),
                tests_changed: false,
                complexity: None,
                risk: None,
                branch: None,
                frontend_root: None,
                brainstorm: false,
                no_brainstorm: false,
                profile: None,
                json: false,
            },
        )
        .expect("seed an active workflow");

        let mut decision = sample_decision(repo.path(), "claude", "fable", Some("feature"));
        decision.complexity = Complexity::Trivial;
        decision.risk = RiskBand::Low;
        let outcome = ProxyIntakeOutcome::Decided {
            decision: Box::new(decision),
            request: "do more work".to_string(),
        };
        let mut announced: Vec<String> = Vec::new();

        let started_id =
            start_proxy_workflow(&outcome, &state, repo.path(), |text| announced.push(text));
        assert_eq!(started_id, None, "a skipped start never names an id");

        let result: CtxResult<i32> = close_proxy_workflow_on_failure(
            started_id.as_deref(),
            &state,
            repo.path(),
            |text| announced.push(text),
            || Ok(0),
        );
        assert_eq!(result.expect("spawn still runs"), 0);

        assert_eq!(announced.len(), 1, "got {announced:?}");
        assert!(
            announced[0].contains("proxy: workflow not started")
                && announced[0].contains(&existing.state.id),
            "must name why and which workflow is already active: {}",
            announced[0]
        );
    }
}
