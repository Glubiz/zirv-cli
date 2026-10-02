//! Overlay reducers: mail, spawn, memory, restore, errors, menu, inspector, palette, quit, handover.
use super::*;

/// Exactly the quit path's own first half (`shutdown_all`): ask the harness to quit and let this
/// tick's `reap_ended_panes` do the rest, so the row is retained, the spend accounted and the group
/// closed by the one code path that knows how. The context menu's `stop` and the agent tree's `x`
/// both come here.
pub(super) fn stop_pane(
    target: &str,
    panes: &mut [Pane],
    cfg: &CtxConfig,
    errors: &mut ErrorLog,
    notices: &mut Vec<Notice>,
    now: Instant,
) {
    let Some(pane) = panes.iter_mut().find(|p| p.short() == target) else {
        return push_notice(notices, now, format!("{target} is no longer running"));
    };
    if pane.is_native() {
        match pane.stop_now(0) {
            Ok(()) => push_notice(notices, now, format!("asked {target} to stop")),
            Err(error) => push_error(errors, format!("could not stop {target}: {error}")),
        }
        return;
    }
    let quit_sequence = adapters::select(Some(pane.agent()), &[], cfg)
        .map(|adapter| adapter.quit_sequence())
        .unwrap_or("");
    pane.request_quit(quit_sequence);
    push_notice(notices, now, format!("asked {target} to quit"));
}

/// The nudge dialog for one session: its pane when this dashboard owns one, else the view-only
/// registry session.
pub(super) fn nudge_draft(target: &str, panes: &[Pane]) -> ui::NudgeDraft {
    let attached = panes.iter().any(|p| p.short() == target);
    ui::NudgeDraft {
        target: if attached {
            ui::NudgeTarget::AttachedPane(target.to_string())
        } else {
            ui::NudgeTarget::ViewOnlySession(target.to_string())
        },
        input: String::new(),
    }
}

/// Relaunch a retained row from its original request through the normal spawn gate (#354).
#[allow(clippy::too_many_arguments)]
pub(super) fn restore_ended_row(
    short: &str,
    panes: &mut Vec<Pane>,
    nudge_queues: &mut Vec<VecDeque<String>>,
    retained: &mut VecDeque<EndedRow>,
    kept_requests: &mut HashMap<String, (spawnreq::SpawnRequest, Option<String>)>,
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
    size: (u16, u16),
    requests_dir: &Path,
    errors: &mut ErrorLog,
    notices: &mut Vec<Notice>,
    now: Instant,
    rows: &[ui::SidebarRow],
    selected: &mut usize,
) {
    let Some(index) = retained.iter().position(|row| row.short == short) else {
        push_notice(notices, now, format!("restore: no ended row named {short}"));
        return;
    };
    let Some(request) = retained[index].request.clone() else {
        push_notice(notices, now, format!("restore {short}: {MENU_NO_REQUEST}"));
        return;
    };
    let requested_by = retained[index].requested_by.clone();
    // Capture both indices before restore changes pane and retained-row positions.
    let old_pane_count = panes.len();
    let restored_row = rows.iter().position(|row| row.short == short);
    match fulfill_spawn_request(
        &request,
        FILE_DROP_TRUSTED_INTERACTIVE,
        requested_by.as_deref(),
        panes,
        nudge_queues,
        cfg,
        state,
        repo,
        size,
        requests_dir,
        errors,
    ) {
        Ok((new_short, _, advisory)) => {
            retained.remove(index);
            kept_requests.insert(new_short.clone(), (request, requested_by));
            if let Some(restored_row) = restored_row {
                *selected = restore_fixup(old_pane_count, panes.len(), restored_row, *selected);
            }
            push_notice(notices, now, format!("restored {short} as {new_short}"));
            // Report restore success as a transient notice, not a sticky error (#399).
            if let Some(text) = advisory {
                push_notice(notices, now, text);
            }
        }
        Err(refusal) => push_error(errors, format!("restore {short}: {}", refusal.reason)),
    }
}

/// Clamps a cursor into `0..len` (or `0` on an empty list) -- shared by every
/// browsing-mode reducer below so "move past the last row" and "the list
/// just shrank out from under the cursor" (an item consumed/forgotten while
/// selected) both land on a valid index.
pub(super) fn clamp_cursor(cursor: usize, len: usize) -> usize {
    if len == 0 { 0 } else { cursor.min(len - 1) }
}

/// Move a list cursor by one row in either direction without leaving the list.
pub(super) fn move_cursor(cursor: usize, len: usize, delta: isize) -> usize {
    if delta >= 0 {
        clamp_cursor(cursor.saturating_add(delta as usize), len)
    } else {
        cursor.saturating_sub(delta.unsigned_abs())
    }
}

/// Insert a newline at the end of compose drafts using the same Enter convention as the pane.
pub(super) fn insert_compose_newline(input: &mut String, modifiers: KeyModifiers) -> bool {
    if modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) {
        input.push('\n');
        return true;
    }
    if !input.ends_with('\\') {
        return false;
    }
    input.pop();
    input.push('\n');
    true
}

/// Pure: one keystroke against the mail overlay's current state. Returns the
/// overlay's next state (`None` closes it -- Esc while browsing) alongside
/// any effect the caller must execute against real storage. Esc while
/// composing cancels only the compose draft, not the whole overlay.
pub fn mail_overlay_reduce(
    mut view: ui::MailView,
    key: KeyEvent,
) -> (Option<ui::MailView>, Option<ui::MailEffect>) {
    if let Some(draft) = view.compose.as_mut() {
        return match key.code {
            KeyCode::Esc => {
                view.compose = None;
                (Some(view), None)
            }
            KeyCode::Enter if insert_compose_newline(&mut draft.body, key.modifiers) => {
                (Some(view), None)
            }
            KeyCode::Enter => {
                if draft.body.trim().is_empty() {
                    return (Some(view), None);
                }
                let to = if draft.to.trim().is_empty() {
                    "any".to_string()
                } else {
                    draft.to.clone()
                };
                let body = draft.body.clone();
                view.compose = None;
                let msg = mail::Message {
                    // Fill sender identity and timestamp only when applying the mail effect to storage.
                    from_session: String::new(),
                    from_agent: String::new(),
                    to,
                    to_session: None,
                    sent: 0,
                    body,
                };
                (Some(view), Some(ui::MailEffect::Send(msg)))
            }
            KeyCode::Backspace => {
                draft.body.pop();
                (Some(view), None)
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                draft.body.push(c);
                (Some(view), None)
            }
            _ => (Some(view), None),
        };
    }

    match key.code {
        KeyCode::Esc => (None, None),
        KeyCode::Down | KeyCode::Char('j') => {
            view.cursor = clamp_cursor(view.cursor + 1, view.items.len());
            (Some(view), None)
        }
        KeyCode::Up | KeyCode::Char('k') => {
            view.cursor = view.cursor.saturating_sub(1);
            (Some(view), None)
        }
        KeyCode::Char('c') => {
            view.compose = Some(ui::ComposeDraft::default());
            (Some(view), None)
        }
        KeyCode::Enter => {
            if view.items.is_empty() {
                return (Some(view), None);
            }
            let (path, _, _) = view.items.remove(view.cursor);
            view.cursor = clamp_cursor(view.cursor, view.items.len());
            (Some(view), Some(ui::MailEffect::Consume(path)))
        }
        _ => (Some(view), None),
    }
}

/// What confirming the spawn dialog asks the caller to do. `Submit` has
/// already been split into its two required halves; `Notice` is a message for
/// the header, with the dialog left open so the operator can fix what they
/// typed rather than losing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnEffect {
    Submit { agent: String, prompt: String },
    Notice(String),
}

/// What a spawn line that cannot be used says. Both halves are required: an
/// agent with no task is not a request, and a task with no agent has nowhere
/// to go.
pub(crate) const SPAWN_USAGE_NOTICE: &str =
    "spawn: type <agent> <prompt>, e.g. `claude fix the failing tests`";

/// Pure: one keystroke against the spawn dialog (`Ctrl+A s`), the same
/// reducer shape every other overlay in this module already uses. `Enter`
/// splits the typed line at its first run of whitespace -- first token is the
/// agent name, the whole remainder is the prompt -- and closes the dialog;
/// anything missing a half keeps the dialog open with a notice. `Esc`
/// cancels outright.
///
/// Deliberately does **not** re-implement the argv guard, the pane cap or the
/// agent gate: a submitted draft is routed through the exact same
/// `fulfill_spawn_request` path a pane's own `zirv ctx agent` request takes,
/// so there is one place those rules live and one place they can be wrong.
pub fn spawn_overlay_reduce(
    mut draft: ui::SpawnDraft,
    key: KeyEvent,
) -> (Option<ui::SpawnDraft>, Option<SpawnEffect>) {
    match key.code {
        KeyCode::Esc => (None, None),
        KeyCode::Enter if insert_compose_newline(&mut draft.input, key.modifiers) => {
            (Some(draft), None)
        }
        KeyCode::Enter => {
            let line = draft.input.trim();
            let Some((agent, prompt)) = line.split_once(char::is_whitespace) else {
                return (
                    Some(draft),
                    Some(SpawnEffect::Notice(SPAWN_USAGE_NOTICE.to_string())),
                );
            };
            let prompt = prompt.trim();
            if agent.is_empty() || prompt.is_empty() {
                return (
                    Some(draft),
                    Some(SpawnEffect::Notice(SPAWN_USAGE_NOTICE.to_string())),
                );
            }
            (
                None,
                Some(SpawnEffect::Submit {
                    agent: agent.to_string(),
                    prompt: prompt.to_string(),
                }),
            )
        }
        KeyCode::Backspace => {
            draft.input.pop();
            (Some(draft), None)
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            draft.input.push(c);
            (Some(draft), None)
        }
        _ => (Some(draft), None),
    }
}

/// Pure: the same shape as `mail_overlay_reduce`, for the memory bank. `r`
/// (remember) seeds the edit buffer with the selected entry's current body
/// so the operator edits rather than retypes; `d`/`v` (forget/verify) act on
/// the selected entry immediately, no confirmation dialog.
pub fn memory_overlay_reduce(
    mut view: ui::MemoryView,
    key: KeyEvent,
) -> (Option<ui::MemoryView>, Option<ui::MemoryEffect>) {
    if let Some(input) = view.input.as_mut() {
        return match key.code {
            KeyCode::Esc => {
                view.input = None;
                (Some(view), None)
            }
            KeyCode::Enter if insert_compose_newline(input, key.modifiers) => (Some(view), None),
            KeyCode::Enter => {
                if input.trim().is_empty() {
                    return (Some(view), None);
                }
                let Some((key_name, _, _)) = view.entries.get(view.cursor).cloned() else {
                    view.input = None;
                    return (Some(view), None);
                };
                let body = input.clone();
                view.input = None;
                (
                    Some(view),
                    Some(ui::MemoryEffect::Remember {
                        key: key_name,
                        body,
                    }),
                )
            }
            KeyCode::Backspace => {
                input.pop();
                (Some(view), None)
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                input.push(c);
                (Some(view), None)
            }
            _ => (Some(view), None),
        };
    }

    match key.code {
        KeyCode::Esc => (None, None),
        KeyCode::Down | KeyCode::Char('j') => {
            view.cursor = clamp_cursor(view.cursor + 1, view.entries.len());
            (Some(view), None)
        }
        KeyCode::Up | KeyCode::Char('k') => {
            view.cursor = view.cursor.saturating_sub(1);
            (Some(view), None)
        }
        KeyCode::Char('r') => {
            if let Some((_, _, body)) = view.entries.get(view.cursor) {
                view.input = Some(body.clone());
            }
            (Some(view), None)
        }
        KeyCode::Char('d') => {
            if view.entries.is_empty() {
                return (Some(view), None);
            }
            let (key_name, _, _) = view.entries.remove(view.cursor);
            view.cursor = clamp_cursor(view.cursor, view.entries.len());
            (Some(view), Some(ui::MemoryEffect::Forget(key_name)))
        }
        KeyCode::Char('v') => match view.entries.get(view.cursor) {
            Some((key_name, _, _)) => {
                let effect = ui::MemoryEffect::Verify(key_name.clone());
                (Some(view), Some(effect))
            }
            None => (Some(view), None),
        },
        _ => (Some(view), None),
    }
}

/// Apply mail effects to storage with the dashboard's own sender identity.
pub(super) fn apply_mail_effect(
    effect: ui::MailEffect,
    state: &StateDir,
    repo: &Path,
    cfg: &CtxConfig,
    from_session: &str,
    from_agent: &str,
    errors: &mut ErrorLog,
) {
    let slug = super::state::repo_slug(repo);
    match effect {
        ui::MailEffect::Consume(path) => {
            // Send overlay mail as the orchestrator pane, not as a process reading inbox (#30).
            if let Err(e) =
                mail::consume_and_log(state, &slug, &path, from_session, "dash", "dash:overlay")
            {
                push_error(errors, format!("mail consume: {e}"));
            }
        }
        ui::MailEffect::Send(mut msg) => {
            msg.from_session = from_session.to_string();
            msg.from_agent = from_agent.to_string();
            msg.sent = super::state::now_secs();
            // Same-repo store: the dashboard composes into its own repo's
            // mailbox, so sender and destination slug are one and the same
            // and the sender's own mail limits legitimately apply.
            if let Err(e) = mail::store_to(state, &slug, &slug, &msg, cfg) {
                push_error(errors, format!("mail send: {e}"));
            }
        }
    }
}

/// Executes a `MemoryEffect` against real storage. `written_by` is this
/// dashboard's own agent name, the same convention `run_remember_with` uses
/// for `AGENT_ENV`.
pub(super) fn apply_memory_effect(
    effect: ui::MemoryEffect,
    state: &StateDir,
    repo: &Path,
    cfg: &CtxConfig,
    written_by: &str,
    errors: &mut ErrorLog,
) {
    let slug = super::state::repo_slug(repo);
    match effect {
        ui::MemoryEffect::Remember { key, body } => {
            let now = super::state::now_secs();
            let entry = memory::Entry {
                key,
                written_by: written_by.to_string(),
                written: now,
                verified: now,
                source: "explicit".to_string(),
                body,
                importance: None,
                confidence: None,
                tags: Vec::new(),
                paths: Vec::new(),
            };
            if let Err(e) = memory::remember(state, &slug, &entry, cfg) {
                push_error(errors, format!("memory remember: {e}"));
            }
        }
        ui::MemoryEffect::Forget(key) => {
            if let Err(e) = memory::forget(state, &slug, &key) {
                push_error(errors, format!("memory forget: {e}"));
            }
        }
        ui::MemoryEffect::Verify(key) => {
            if let Err(e) = memory::verify(state, &slug, &key) {
                push_error(errors, format!("memory verify: {e}"));
            }
        }
    }
}

/// Preview only the first line within the dialog's width.
pub(super) fn mail_preview(body: &str) -> String {
    body.lines().next().unwrap_or("").chars().take(60).collect()
}

/// Builds a freshly-populated `MailView` from every message currently
/// visible to the dashboard operator -- `for_agent`/`for_session` both
/// `None`, the same broad "everything in this repo's mailbox" view
/// `zirv ctx inbox` gives a human, not the narrow per-session filter a
/// delivery seam applies. A read error degrades to an empty view rather than
/// failing the overlay open.
pub(super) fn build_mail_view(state: &StateDir, repo: &Path) -> ui::MailView {
    let slug = super::state::repo_slug(repo);
    let items = mail::list(state, &slug, None, None)
        .unwrap_or_default()
        .into_iter()
        .map(|(path, msg)| (path, msg.from_agent, mail_preview(&msg.body)))
        .collect();
    ui::MailView {
        items,
        cursor: 0,
        offset: 0,
        compose: None,
    }
}

/// Builds a freshly-populated `MemoryView` from this repo's whole memory
/// bank. The age wording matches `memory::render_for_prompt`'s own
/// convention ("written Nd ago, verified Nd ago") so it reads the same
/// everywhere it appears.
pub(super) fn build_memory_view(state: &StateDir, repo: &Path) -> ui::MemoryView {
    let slug = super::state::repo_slug(repo);
    let now = super::state::now_secs();
    let entries = memory::list(state, &slug)
        .unwrap_or_default()
        .into_iter()
        .map(|(_, entry)| {
            let age = format!(
                "written {}d ago, verified {}d ago",
                now.saturating_sub(entry.written) / 86_400,
                now.saturating_sub(entry.verified) / 86_400,
            );
            (entry.key, age, entry.body)
        })
        .collect();
    ui::MemoryView {
        entries,
        cursor: 0,
        offset: 0,
        input: None,
    }
}

/// What confirming the restore dialog (Enter) reports back: the indices,
/// into whatever candidate list the caller built the view's entries from in
/// the same order, that were checked at the moment of confirmation. `Esc`
/// (skip everything) yields no effect at all -- see `restore_overlay_reduce`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreEffect {
    Confirm(Vec<usize>),
}

/// Pure: one keystroke against the restore dialog's current state. `Space`
/// toggles the entry under the cursor; `Enter` closes the dialog and reports
/// every currently-checked index as a `Confirm` effect (an empty roster, or
/// everything unchecked, is still a valid confirm -- it simply restores
/// nothing); `Esc` closes the dialog with no effect, skipping the restore
/// entirely. Arrow keys and `j`/`k` move the cursor, clamped the same way
/// every other browsing-mode reducer in this module already is.
pub fn restore_overlay_reduce(
    mut view: ui::RestoreView,
    key: KeyEvent,
) -> (Option<ui::RestoreView>, Option<RestoreEffect>) {
    match key.code {
        KeyCode::Esc => (None, None),
        KeyCode::Enter => {
            let checked = view
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.checked)
                .map(|(i, _)| i)
                .collect();
            (None, Some(RestoreEffect::Confirm(checked)))
        }
        KeyCode::Down | KeyCode::Char('j') => {
            view.cursor = move_cursor(view.cursor, view.entries.len(), 1);
            (Some(view), None)
        }
        KeyCode::Up | KeyCode::Char('k') => {
            view.cursor = move_cursor(view.cursor, view.entries.len(), -1);
            (Some(view), None)
        }
        KeyCode::Char(' ') => {
            if let Some(entry) = view.entries.get_mut(view.cursor) {
                entry.checked = !entry.checked;
            }
            (Some(view), None)
        }
        _ => (Some(view), None),
    }
}

/// Acknowledgement applies to the errors visible when the dialog opened (#354).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorsAck {
    /// The snapshot's own [`ui::ErrorsView::mark`] -- what the caller passes
    /// to [`ErrorLog::acknowledge`] so only the entries this dialog actually
    /// showed are acknowledged.
    pub mark: u64,
}

/// Closing the error list acknowledges its snapshot because the operator has seen it (#354).
pub fn errors_overlay_reduce(
    mut view: ui::ErrorsView,
    key: KeyEvent,
) -> (Option<ui::ErrorsView>, Option<ErrorsAck>) {
    match key.code {
        // Enter closes a read-only list because it has no activation action (#354).
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
            (None, Some(ErrorsAck { mark: view.mark }))
        }
        KeyCode::Char('a') => {
            let mark = view.mark;
            for item in &mut view.items {
                item.acked = true;
            }
            (Some(view), Some(ErrorsAck { mark }))
        }
        KeyCode::Down | KeyCode::Char('j') => {
            view.cursor = move_cursor(view.cursor, view.items.len(), 1);
            (Some(view), None)
        }
        KeyCode::Up | KeyCode::Char('k') => {
            view.cursor = move_cursor(view.cursor, view.items.len(), -1);
            (Some(view), None)
        }
        _ => (Some(view), None),
    }
}

/// Snapshot retained errors at dialog open so acknowledgement does not include later arrivals.
pub(super) fn build_errors_view(errors: &ErrorLog, now: Instant) -> ui::ErrorsView {
    ui::ErrorsView {
        items: errors
            .entries
            .iter()
            .rev()
            .map(|e| ui::ErrorItem {
                text: e.text.clone(),
                count: e.count,
                age_secs: now.saturating_duration_since(e.last).as_secs(),
                acked: e.acked,
            })
            .collect(),
        cursor: 0,
        offset: 0,
        mark: errors.mark(),
    }
}

/// Builds `Overlay::JevErrors`' own view (click affordance follow-up)
/// straight from the JEV sidebar's own cached fact -- never a disk read on
/// click, only whatever `jev_section_fact` last cached on the 10s JEV
/// refresh cadence. `None` (the gate off, or `NoKey`) and zero cached
/// errors both give an empty view; [`ui::list_spec_for`]'s own
/// `empty_message` is what the operator actually sees for either.
pub(super) fn build_jev_errors_view(fact: &Option<ui::JevSectionFact>) -> ui::JevErrorsView {
    let items = match fact {
        Some(ui::JevSectionFact::Active { errors_detail, .. }) => errors_detail
            .iter()
            .map(|row| ui::ErrorItem {
                text: format!("{} \u{b7} {}", row.site, row.reason),
                count: 1,
                age_secs: row.age_secs,
                acked: false,
            })
            .collect(),
        _ => Vec::new(),
    };
    ui::JevErrorsView {
        items,
        cursor: 0,
        offset: 0,
    }
}

/// Pure: one keystroke against the JEV errors dialog (click affordance
/// follow-up) -- read-only history, so browsing is all there is: `j/k`/
/// arrows move the cursor, `Esc`/`Enter`/`q` close it. No acknowledgement,
/// unlike `Ctrl+A e`'s own `errors_overlay_reduce`: these rows are a rollup
/// snapshot, not the dashboard's own live error buffer, so there is nothing
/// to acknowledge and no `Ack` payload to hand back. Mirrors `inspector_
/// overlay_reduce`'s own read-only shape.
pub(super) fn jev_errors_overlay_reduce(
    mut view: ui::JevErrorsView,
    key: KeyEvent,
) -> Option<ui::JevErrorsView> {
    match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => None,
        KeyCode::Down | KeyCode::Char('j') => {
            view.cursor = move_cursor(view.cursor, view.items.len(), 1);
            Some(view)
        }
        KeyCode::Up | KeyCode::Char('k') => {
            view.cursor = move_cursor(view.cursor, view.items.len(), -1);
            Some(view)
        }
        _ => Some(view),
    }
}

/// Everything the context menu decides ONE row's entries from. Assembled at
/// the moment the menu opens, from values already in hand -- the row the
/// roster built, the pane behind it (if any), and whether a spawn request was
/// kept for it -- so the entry matrix itself stays a pure function.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct MenuFacts {
    short: String,
    role: String,
    /// This dashboard owns a live `Pane` for the row.
    attached: bool,
    /// The row is a live session (attached, or a view-only registry row).
    alive: bool,
    /// The row is an ended pane -- a retained completed worker.
    ended: bool,
    /// The exit code of an ended row, when one was recorded.
    exit_code: Option<i32>,
    /// The row is one of the retained ended rows the roster keeps, so it can
    /// be dropped from that list.
    retained: bool,
    /// The dashboard still holds the `spawnreq::SpawnRequest` that created
    /// this row, so it can be relaunched verbatim.
    has_request: bool,
    /// The checkout this row runs (or ran) in, when it is known.
    cwd: Option<String>,
}

impl MenuFacts {
    /// The availability slice of these facts, in the shape the one
    /// action-descriptor table decides against.
    pub(super) fn action_context(&self) -> actions::ActionContext {
        actions::ActionContext {
            selected: true,
            attached: self.attached,
            alive: self.alive,
            ended: self.ended,
            // The menu never chooses its own entries by the glyph -- every
            // entry is always present -- so this stays false here.
            needs_action: false,
            retained: self.retained,
            has_request: self.has_request,
            clean_exit: self.exit_code.unwrap_or(0) == 0,
            has_cwd: self.cwd.is_some(),
            // A `MenuFacts` is always a real session row; the summary line
            // never goes through here (see `selected_action_context`).
            summary: false,
        }
    }
}

/// Derive menu entries and disabled reasons from the shared action table (#354).
pub(super) fn menu_entries(facts: &MenuFacts) -> Vec<ui::MenuEntry> {
    let available = actions::menu_actions(&facts.action_context());
    let order: Vec<ui::MenuAction> = available.iter().map(|(action, _)| *action).collect();
    let letters = ui::menu_letters(&order);
    available
        .into_iter()
        .zip(letters)
        .map(|((action, availability), letter)| ui::MenuEntry {
            action,
            disabled: availability.reason().map(str::to_string),
            letter,
        })
        .collect()
}

/// Builds the context menu for one row. `subject` is what the dialog title
/// names, so a right-click menu is never mistaken for the selected row's.
pub(super) fn build_menu_view(facts: &MenuFacts) -> ui::MenuView {
    ui::MenuView {
        target: facts.short.clone(),
        subject: format!("{} \u{b7} {}", facts.short, facts.role),
        entries: menu_entries(facts),
        cursor: 0,
        offset: 0,
        confirm: None,
    }
}

/// Keep the summary-line menu's session actions visible with reasons, though the target is the dashboard (#354).
pub(super) fn build_summary_menu_view() -> ui::MenuView {
    let ctx = actions::ActionContext {
        summary: true,
        ..actions::ActionContext::default()
    };
    let available = actions::menu_actions(&ctx);
    let order: Vec<ui::MenuAction> = available.iter().map(|(action, _)| *action).collect();
    let letters = ui::menu_letters(&order);
    ui::MenuView {
        target: DASHBOARD_TARGET.to_string(),
        subject: "the dashboard".to_string(),
        entries: available
            .into_iter()
            .zip(letters)
            .map(|((action, availability), letter)| ui::MenuEntry {
                action,
                disabled: availability.reason().map(str::to_string),
                letter,
            })
            .collect(),
        cursor: 0,
        offset: 0,
        confirm: None,
    }
}

/// The checkout one row runs (or ran) in: the live pane's own `cwd`, else
/// the one frozen onto its retained ended row at the reap. `None` for a
/// view-only registry row, which this dashboard never spawned and whose
/// working directory it has no record of.
pub(super) fn row_cwd(
    short: &str,
    panes: &[Pane],
    retained: &VecDeque<EndedRow>,
) -> Option<String> {
    panes
        .iter()
        .find(|p| p.short() == short)
        .map(|p| p.cwd().display().to_string())
        .or_else(|| {
            retained
                .iter()
                .find(|e| e.short == short)
                .map(|e| e.cwd.clone())
        })
}

/// Assembles the context menu's facts for one already-built roster row.
pub(super) fn menu_facts_for(
    row: &ui::SidebarRow,
    panes: &[Pane],
    retained: &VecDeque<EndedRow>,
) -> MenuFacts {
    let ended_row = retained.iter().find(|e| e.short == row.short);
    MenuFacts {
        short: row.short.clone(),
        role: row.role.clone(),
        attached: row.attached,
        alive: row.state != ui::RowState::Dead && row.exit_code.is_none(),
        ended: row.state == ui::RowState::Dead || row.exit_code.is_some(),
        exit_code: row.exit_code,
        retained: ended_row.is_some(),
        has_request: ended_row.is_some_and(|e| e.request.is_some()),
        cwd: row_cwd(&row.short, panes, retained),
    }
}

/// What activating a context-menu entry means. The menu itself stays pure:
/// looking a pane up, opening another overlay, quitting a child or
/// relaunching a request all happen at the call site, which is the only place
/// with the panes, the state directory and the kept requests in hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuEffect {
    pub target: String,
    pub action: ui::MenuAction,
}

/// Pure: one keystroke against the context menu.
///
/// `Enter` activates the entry under the caret -- except `stop`, the one
/// destructive entry, which first raises an inline confirmation on its own
/// row and only acts on `y` (or a second `Enter`). A disabled entry does
/// nothing at all: its reason is already on screen. `Esc` closes the menu
/// without touching focus, and a letter jumps straight to its entry.
pub fn menu_overlay_reduce(
    mut view: ui::MenuView,
    key: KeyEvent,
) -> (Option<ui::MenuView>, Option<MenuEffect>) {
    let len = view.entries.len();
    let activate = |view: ui::MenuView| -> (Option<ui::MenuView>, Option<MenuEffect>) {
        match view.entries.get(view.cursor) {
            Some(entry) if entry.enabled() => {
                let effect = MenuEffect {
                    target: view.target.clone(),
                    action: entry.action,
                };
                (None, Some(effect))
            }
            // A disabled entry is inert: the row already says why.
            _ => (Some(view), None),
        }
    };
    // The inline stop confirmation owns the keyboard while it is up, so a
    // stray `j` cannot walk the caret off the entry that is being confirmed
    // and leave the confirmation pointing at a different row.
    if let Some(index) = view.confirm {
        return match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                view.cursor = index;
                view.confirm = None;
                activate(view)
            }
            _ => {
                view.confirm = None;
                (Some(view), None)
            }
        };
    }
    match key.code {
        KeyCode::Esc => (None, None),
        KeyCode::Up | KeyCode::Char('k') => {
            view.cursor = move_cursor(view.cursor, len, -1);
            (Some(view), None)
        }
        KeyCode::Down | KeyCode::Char('j') => {
            view.cursor = move_cursor(view.cursor, len, 1);
            (Some(view), None)
        }
        KeyCode::Enter => {
            if view
                .entries
                .get(view.cursor)
                .is_some_and(|e| e.action == ui::MenuAction::Stop && e.enabled())
            {
                view.confirm = Some(view.cursor);
                return (Some(view), None);
            }
            activate(view)
        }
        KeyCode::Char(c) => match view.entries.iter().position(|e| e.letter == Some(c)) {
            Some(index) => {
                view.cursor = index;
                if view.entries[index].action == ui::MenuAction::Stop
                    && view.entries[index].enabled()
                {
                    view.confirm = Some(index);
                    return (Some(view), None);
                }
                activate(view)
            }
            None => (Some(view), None),
        },
        _ => (Some(view), None),
    }
}

/// The inspector's own section names, once, so `build_inspector_view`, the
/// `evidence` menu entry and the tests all name the same strings.
pub(super) const INSPECT_IDENTITY: &str = "identity";
pub(super) const INSPECT_STATUS: &str = "status";
pub(super) const INSPECT_EVIDENCE: &str = "evidence";
pub(super) const INSPECT_BUDGET: &str = "budget";
pub(super) const INSPECT_WRITER: &str = "writer";
pub(super) const INSPECT_SIGNAL: &str = "signal";
pub(super) const INSPECT_ERRORS: &str = "errors";

/// Use the same inspector sections for the dashboard target (#354).
pub(super) const INSPECT_DASH_HARNESS: &str = "harness";
pub(super) const INSPECT_DASH_SESSIONS: &str = "sessions";
pub(super) const INSPECT_DASH_DELEGATION: &str = "delegation";
pub(super) const INSPECT_DASH_USAGE: &str = "usage";
pub(super) const INSPECT_DASH_REPO: &str = "repo";
pub(super) const INSPECT_DASH_DASHBOARD: &str = "dashboard";

/// The `target` a dashboard-level inspector or action menu carries.
///
/// Session short ids are exactly eight characters (`sessions::short_id`), so
/// a nine-character literal can never collide with one -- which is what lets
/// the menu effect and the inspector both say "this is the dashboard, not a
/// row" without a second `Option` threaded through every arm.
pub(super) const DASHBOARD_TARGET: &str = "dashboard";

/// One `key  value` inspector line, with the shared placeholder standing in
/// for a fact nothing has recorded yet -- never a fabricated value.
pub(super) fn inspect_line(key: &str, value: Option<String>) -> String {
    format!(
        "{key:<12}{}",
        value
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| style::PLACEHOLDER.to_string())
    )
}

/// Builds the inspector over one row, from facts that are already cached.
///
/// Nothing here reads the disk, runs git or shells out: `row` is what the
/// roster already assembled this frame, `status` is the composed
/// `attention::SessionStatus` the `FactsCache` cadence loaded, `cwd` comes
/// from the pane (or the retained row) the caller already has, and `errors`
/// is `push_error`'s own kept buffer.
pub(super) fn build_inspector_view(
    row: &ui::SidebarRow,
    cwd: Option<&str>,
    errors: &ErrorLog,
) -> ui::InspectorView {
    let disclosure = |key: &str| {
        row.disclosure
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    let status = row.status.as_ref();
    let identity = ui::InspectorSection {
        name: INSPECT_IDENTITY.to_string(),
        lines: vec![
            inspect_line("short", Some(row.short.clone())),
            inspect_line("harness", Some(row.harness.clone())),
            inspect_line("model", row.model.clone()),
            inspect_line("role", Some(row.role.clone())),
            inspect_line(
                "group",
                row.group.as_ref().map(|g| {
                    format!(
                        "{} ({})",
                        g.scope,
                        if g.lead_short.is_empty() {
                            style::PLACEHOLDER
                        } else {
                            g.lead_short.as_str()
                        }
                    )
                }),
            ),
            inspect_line("parent", disclosure("group")),
            inspect_line("cwd", cwd.map(str::to_string)),
            // Never run: `branch` stays whatever the roster cached, which is
            // the placeholder until something else fills it in. Shelling out
            // to git here would put a subprocess behind a keystroke.
            inspect_line("branch", disclosure("branch")),
        ],
    };
    let status_section = ui::InspectorSection {
        name: INSPECT_STATUS.to_string(),
        lines: vec![
            inspect_line(
                "lifecycle",
                status.map(|s| spaced_lowercase(&format!("{:?}", s.lifecycle))),
            ),
            inspect_line(
                "attention",
                status.map(|s| spaced_lowercase(&format!("{:?}", s.attention))),
            ),
            inspect_line(
                "projection",
                status.map(|s| projection_word(super::attention::project(s))),
            ),
            inspect_line(
                "authority",
                status.map(|s| spaced_lowercase(&format!("{:?}", s.authority))),
            ),
            inspect_line("confidence", status.map(|s| s.confidence.to_string())),
            inspect_line("since", disclosure("since")),
            inspect_line("revision", status.map(|s| s.revision.to_string())),
            inspect_line("exit", row.exit_code.map(|code| format!("exit {code}"))),
        ],
    };
    // The whole point of the inspector: WHY the dashboard believes what it
    // shows. Both the winning observation's evidence and every authority that
    // lost this tick's vote, with its own reason.
    let mut evidence_lines = Vec::new();
    if let Some(status) = status {
        if !status.evidence.trim().is_empty() {
            evidence_lines.push(inspect_line("evidence", Some(status.evidence.clone())));
        }
        for skipped in &status.skipped {
            evidence_lines.push(format!(
                "skipped     {} \u{b7} {}",
                spaced_lowercase(&format!("{:?}", skipped.authority)),
                skipped.reason
            ));
        }
    }
    let evidence = ui::InspectorSection {
        name: INSPECT_EVIDENCE.to_string(),
        lines: evidence_lines,
    };
    let budget = ui::InspectorSection {
        name: INSPECT_BUDGET.to_string(),
        lines: vec![inspect_line("usage", disclosure("budget"))],
    };
    let writer = ui::InspectorSection {
        name: INSPECT_WRITER.to_string(),
        lines: vec![inspect_line("permit", disclosure("writer"))],
    };
    let signal = ui::InspectorSection {
        name: INSPECT_SIGNAL.to_string(),
        lines: vec![
            inspect_line("transport", disclosure("signal")),
            inspect_line(
                "attached",
                Some(if row.attached { "yes" } else { "no" }.to_string()),
            ),
        ],
    };
    // Only this pane's own kept errors: the buffer is shared, and every line
    // `push_error` writes for a pane names its short id.
    let pane_errors = ui::InspectorSection {
        name: INSPECT_ERRORS.to_string(),
        lines: errors
            .iter()
            .rev()
            .filter(|e| e.contains(&row.short))
            .take(MAX_KEPT_ERRORS)
            .map(str::to_string)
            .collect(),
    };
    ui::InspectorView {
        target: row.short.clone(),
        subject: format!("{} \u{b7} {}", row.short, row.role),
        sections: vec![
            identity,
            status_section,
            evidence,
            budget,
            writer,
            signal,
            pane_errors,
        ],
        cursor: 0,
        offset: 0,
    }
}

/// Build the dashboard inspector only from cached facts; rendering must not read disk or run commands (#354).
pub(super) struct DashboardFacts<'a> {
    harness: &'a str,
    short: &'a str,
    /// This dashboard's own orchestrator seat label (`gen N`).
    seat: Option<&'a str>,
    state_dir: String,
    uptime_secs: u64,
    /// Mouse reporting is on (the pointer drives the chrome and zirv's own
    /// click-drag selection); `false` is the operator's own `dash.mouse`
    /// config turned off entirely, handing the pointer to the terminal.
    mouse: bool,
    sidebar_cols: u16,
    /// How stale the throttled disk facts below are.
    facts_age_secs: u64,
    rows: &'a [ui::SidebarRow],
    spend: Option<AggregateSpendFacts>,
    usage: &'a [ui::HarnessUsage],
    pool: &'a [ui::HarnessStrip],
    /// `(broadcast, direct)` unread for the dashboard's own identity.
    mail: Option<(usize, usize)>,
    workflow: Option<&'a workflow::ActiveWorkflowSummary>,
    /// `(panes whose turn-signal socket bound, attached panes)`.
    supervised: (usize, usize),
}

/// Open the dashboard inspector when the summary line is selected (#354).
pub(super) fn build_dashboard_inspector(facts: &DashboardFacts<'_>) -> ui::InspectorView {
    let age = |secs: u64| format!("read {} ago", style::format_age(secs));
    let harness = ui::InspectorSection {
        name: INSPECT_DASH_HARNESS.to_string(),
        lines: vec![
            inspect_line("harness", Some(facts.harness.to_string())),
            inspect_line("short", Some(facts.short.to_string())),
            inspect_line("seat", facts.seat.map(str::to_string)),
            inspect_line("uptime", Some(style::format_age(facts.uptime_secs))),
        ],
    };
    let live = facts
        .rows
        .iter()
        .filter(|r| r.state != ui::RowState::Dead)
        .count();
    let mut session_lines = vec![
        inspect_line("live", Some(live.to_string())),
        inspect_line("ended", Some((facts.rows.len() - live).to_string())),
    ];
    // One line per glyph, always all six: a zero here is a fact (nothing is
    // waiting), not a missing reading.
    for glyph in ui::ALL_GLYPHS {
        let count = facts
            .rows
            .iter()
            .filter(|r| ui::glyph_for(r) == glyph)
            .count();
        session_lines.push(inspect_line(
            glyph.name(),
            Some(format!("{} {count}", glyph.symbol())),
        ));
    }
    let sessions = ui::InspectorSection {
        name: INSPECT_DASH_SESSIONS.to_string(),
        lines: session_lines,
    };
    let mut spend_lines = vec![inspect_line(
        "delegated",
        facts.spend.map(|s| format!("{} failed", s.failed)),
    )];
    for strip in facts.pool {
        spend_lines.push(inspect_line(
            &strip.name,
            Some(match strip.headroom_pct {
                Some(pct) => format!("{} \u{b7} headroom {pct:.0}%", strip.state),
                None => strip.state.clone(),
            }),
        ));
    }
    if facts.spend.is_some() || !facts.pool.is_empty() {
        spend_lines.push(inspect_line("as of", Some(age(facts.facts_age_secs))));
    }
    let spend = ui::InspectorSection {
        name: INSPECT_DASH_DELEGATION.to_string(),
        lines: spend_lines,
    };
    let usage = ui::InspectorSection {
        name: INSPECT_DASH_USAGE.to_string(),
        lines: facts
            .usage
            .iter()
            .map(|u| {
                let pct = |v: Option<f64>| match v {
                    Some(v) => format!("{v:.0}%"),
                    None => style::PLACEHOLDER.to_string(),
                };
                inspect_line(
                    u.name,
                    Some(format!(
                        "5h {} \u{b7} 7d {}",
                        pct(u.five_hour),
                        pct(u.seven_day)
                    )),
                )
            })
            .collect(),
    };
    let repo = ui::InspectorSection {
        name: INSPECT_DASH_REPO.to_string(),
        lines: vec![
            inspect_line(
                "mail",
                facts.mail.map(|(broadcast, direct)| {
                    format!("{broadcast} broadcast \u{b7} {direct} direct")
                }),
            ),
            inspect_line(
                "workflow",
                facts.workflow.map(|w| {
                    let gate = if w.awaiting_approval {
                        " \u{b7} awaits approval"
                    } else {
                        ""
                    };
                    format!("{} \u{b7} {}{gate}", w.kind, w.step)
                }),
            ),
            inspect_line(
                "supervision",
                Some(format!(
                    "{} of {} panes",
                    facts.supervised.0, facts.supervised.1
                )),
            ),
        ],
    };
    let dashboard = ui::InspectorSection {
        name: INSPECT_DASH_DASHBOARD.to_string(),
        lines: vec![
            inspect_line("state dir", Some(facts.state_dir.clone())),
            inspect_line(
                "mouse",
                Some(if facts.mouse { "on" } else { "off" }.to_string()),
            ),
            inspect_line("sidebar", Some(format!("{} cols", facts.sidebar_cols))),
            inspect_line("facts", Some(age(facts.facts_age_secs))),
        ],
    };
    ui::InspectorView {
        target: DASHBOARD_TARGET.to_string(),
        subject: format!("dashboard \u{b7} {}", facts.short),
        sections: vec![harness, sessions, spend, usage, repo, dashboard],
        cursor: 0,
        offset: 0,
    }
}

/// Gathers the dashboard-level inspector's facts out of what is already in
/// hand: this tick's roster, the throttled [`FactsCache`], and the loop's own
/// in-memory state. No read of any kind happens here.
#[allow(clippy::too_many_arguments)]
pub(super) fn dashboard_facts<'a>(
    harness: &'a str,
    short: &'a str,
    rows: &'a [ui::SidebarRow],
    panes: &[Pane],
    cache: &'a FactsCache,
    state: &StateDir,
    launched_at: Instant,
    mouse: bool,
    sidebar_cols: u16,
    now: Instant,
) -> DashboardFacts<'a> {
    DashboardFacts {
        harness,
        short,
        seat: cache.disk.pool_seat.as_deref(),
        state_dir: state.root().display().to_string(),
        uptime_secs: now.saturating_duration_since(launched_at).as_secs(),
        mouse,
        sidebar_cols,
        facts_age_secs: now.saturating_duration_since(cache.last_refresh).as_secs(),
        rows,
        spend: cache.disk.spend,
        usage: &cache.disk.usage,
        pool: &cache.disk.pool_harnesses,
        mail: cache.disk.mail,
        workflow: cache.disk.workflow.as_ref(),
        supervised: (panes.iter().filter(|p| p.reachable()).count(), panes.len()),
    }
}

/// Close a read-only inspector on Enter or Esc; it has no action to activate (#354).
pub fn inspector_overlay_reduce(
    mut view: ui::InspectorView,
    key: KeyEvent,
) -> Option<ui::InspectorView> {
    let len = view.rows().len();
    match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => None,
        KeyCode::Down | KeyCode::Char('j') => {
            view.cursor = move_cursor(view.cursor, len, 1);
            Some(view)
        }
        KeyCode::Up | KeyCode::Char('k') => {
            view.cursor = move_cursor(view.cursor, len, -1);
            Some(view)
        }
        _ => Some(view),
    }
}

/// What activating a palette row asks the caller to do: run the descriptor
/// under the caret. The palette itself stays pure -- turning an
/// [`actions::ActionId`] into either a `DashAction` (a global chord, replayed
/// through the exact dispatch the keyboard uses) or a `ui::MenuAction`
/// (a row action, replayed through the exact path the context menu uses)
/// happens at the call site, which is the only place with the roster, the
/// panes and the state directory in hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaletteEffect(pub actions::ActionId);

/// Pure: one keystroke against the palette (`^A p`) or the help screen
/// (`^A ?`), which is the same dialog in read-only mode.
///
/// `Esc` closes without running anything. `Enter` runs the caret's own action
/// -- and in help mode simply closes, since a key reference must never fire
/// an action an operator only meant to read about. Up/Down move the caret,
/// skipping section headings; Backspace edits the query and every other
/// printable character extends it. Nothing typed here is ever forwarded to
/// the child: an open overlay owns every keystroke, and the palette's query
/// is the clearest case of why that rule exists.
pub fn palette_overlay_reduce(
    mut view: ui::PaletteView,
    key: KeyEvent,
) -> (Option<ui::PaletteView>, Option<PaletteEffect>) {
    let refresh = |view: &mut ui::PaletteView| {
        let rows = view.rows();
        // A query that no longer matches what the caret was on puts the
        // caret back on the first row that does -- never off the end of the
        // list, and never parked on a heading.
        if !rows
            .get(view.cursor)
            .is_some_and(actions::PaletteRow::selectable)
        {
            view.cursor = actions::palette_first(&rows);
        }
        // A new query is a new list: back to the top of it.
        view.offset = 0;
    };
    match key.code {
        KeyCode::Esc => (None, None),
        KeyCode::Enter => match view.activated() {
            Some(id) => (None, Some(PaletteEffect(id))),
            // Help mode, a section heading, a disabled row, or an empty
            // result: Enter closes rather than doing nothing at all.
            None => (None, None),
        },
        KeyCode::Up => {
            let rows = view.rows();
            view.cursor = actions::palette_step(&rows, view.cursor, -1);
            (Some(view), None)
        }
        KeyCode::Down => {
            let rows = view.rows();
            view.cursor = actions::palette_step(&rows, view.cursor, 1);
            (Some(view), None)
        }
        KeyCode::Backspace => {
            view.query.pop();
            refresh(&mut view);
            (Some(view), None)
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            view.query.push(c);
            refresh(&mut view);
            (Some(view), None)
        }
        _ => (Some(view), None),
    }
}

/// Use the same availability snapshot for palette and context menu.
pub(super) fn selected_action_context(
    rows: &[ui::SidebarRow],
    selected: usize,
    panes: &[Pane],
    retained: &VecDeque<EndedRow>,
    summary: bool,
) -> actions::ActionContext {
    if summary {
        return actions::ActionContext {
            summary: true,
            ..actions::ActionContext::default()
        };
    }
    match rows.get(selected) {
        Some(row) => menu_facts_for(row, panes, retained).action_context(),
        None => actions::ActionContext::default(),
    }
}

/// Snapshot row context when opening the palette, matching other overlays.
pub(super) fn build_palette_view(
    mode: ui::PaletteMode,
    ctx: actions::ActionContext,
) -> ui::PaletteView {
    let mut view = ui::PaletteView {
        mode,
        query: String::new(),
        ctx,
        cursor: 0,
        offset: 0,
    };
    view.cursor = actions::palette_first(&view.rows());
    view
}

/// Reduce quit confirmation keys without side effects (#202).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitConfirmEffect {
    Confirm,
}

pub fn quit_confirm_reduce(
    working: Vec<String>,
    key: KeyEvent,
) -> (Option<Vec<String>>, Option<QuitConfirmEffect>) {
    match key.code {
        KeyCode::Enter => (None, Some(QuitConfirmEffect::Confirm)),
        KeyCode::Esc => (None, None),
        _ => (Some(working), None),
    }
}

/// Return the chosen handover action for the event loop to apply (#202).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoverEffect {
    Swap {
        target_short: String,
        target_agent: String,
        target_model: String,
    },
}

pub fn handover_overlay_reduce(
    mut draft: ui::HandoverDraft,
    key: KeyEvent,
) -> (Option<ui::HandoverDraft>, Option<HandoverEffect>) {
    match key.code {
        KeyCode::Esc => (None, None),
        KeyCode::Up => {
            draft.cursor = move_cursor(draft.cursor, draft.items.len(), -1);
            (Some(draft), None)
        }
        KeyCode::Down => {
            draft.cursor = move_cursor(draft.cursor, draft.items.len(), 1);
            (Some(draft), None)
        }
        KeyCode::Enter => match draft.items.get(draft.cursor).cloned() {
            Some((target_agent, _tier, target_model)) => (
                None,
                Some(HandoverEffect::Swap {
                    target_short: draft.target_short.clone(),
                    target_agent,
                    target_model,
                }),
            ),
            None => (Some(draft), None),
        },
        _ => (Some(draft), None),
    }
}

/// Offer only worker restore candidates, since startup already creates the orchestrator.
pub(super) fn build_restore_view(candidates: &[roster::RosterPane]) -> ui::RestoreView {
    ui::RestoreView {
        entries: candidates
            .iter()
            .map(|pane| ui::RestoreEntry {
                label: format!("{} {} ({})", pane.title, pane.agent, pane.short),
                checked: true,
            })
            .collect(),
        cursor: 0,
        offset: 0,
    }
}

/// Cap restored panes against live panes; return the number that cannot fit.
pub(super) fn restore_budget(live: usize, max_panes: usize, wanted: usize) -> (usize, usize) {
    let room = max_panes.saturating_sub(live);
    let take = wanted.min(room);
    (take, wanted - take)
}

/// Take only candidates within the pane cap and keep the skipped remainder.
pub(super) fn partition_restore_selection(
    indices: Vec<usize>,
    restore_candidates: &[roster::RosterPane],
    take: usize,
) -> (Vec<roster::RosterPane>, Vec<roster::RosterPane>) {
    let mut to_spawn = Vec::new();
    let mut deferred = Vec::new();
    for (position, idx) in indices.into_iter().enumerate() {
        let Some(candidate) = restore_candidates.get(idx) else {
            continue;
        };
        if position < take {
            to_spawn.push(candidate.clone());
        } else {
            deferred.push(candidate.clone());
        }
    }
    (to_spawn, deferred)
}

/// Rebuild restored turn environment from trusted roster state, including launch mode and lineage.
pub(super) fn restored_pane_turn_env(
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
    candidate: &roster::RosterPane,
    requests_dir: &Path,
    errors: &mut ErrorLog,
) -> (Vec<(String, String)>, PathBuf) {
    let mode = if candidate.interactive {
        adapters::LaunchMode::Interactive
    } else {
        adapters::LaunchMode::Headless
    };
    let (mut turn_env, turn_env_err) = build_turn_env(
        cfg,
        state,
        repo,
        &candidate.agent,
        &candidate.session_id,
        mode,
    );
    if let Some(e) = turn_env_err {
        push_error(errors, e);
    }
    // Give a restored pane a fresh private request channel; its old token belonged to the prior dashboard.
    let pane_channel = mint_pane_channel(requests_dir, errors);
    turn_env.push((
        spawnreq::DASH_REQUESTS_ENV.to_string(),
        pane_channel.display().to_string(),
    ));
    // Restore work-group binding so descendants and coordinator ownership keep their lineage.
    if let Some(group_id) = &candidate.work_group_id {
        turn_env.push((super::agent::WORK_GROUP_ENV.to_string(), group_id.clone()));
    }
    // Restore server-verified parent lineage in the child environment (#249, #250).
    if let Some(parent) = &candidate.parent_session {
        turn_env.push((super::agent::PARENT_SESSION_ENV.to_string(), parent.clone()));
    }
    (turn_env, pane_channel)
}

/// Recheck a roster candidate against live adapter and configuration gates:
/// stored spawn requests are data, never authority to bypass current policy.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_restored_pane(
    candidate: &roster::RosterPane,
    panes: &mut Vec<Pane>,
    nudge_queues: &mut Vec<VecDeque<String>>,
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
    size: (u16, u16),
    requests_dir: &Path,
    errors: &mut ErrorLog,
    deferred_restore: &mut Vec<roster::RosterPane>,
) {
    // Open native restores through resolve_attach so an existing runtime session is reattached, not duplicated (#490).
    if candidate.native {
        match Pane::spawn_native(
            cfg,
            state,
            &super::config::env_from_process(),
            repo,
            sessions::Verb::Dash,
            candidate.title.clone(),
            size,
            native_pane::NativeDashboardSpec {
                repo: repo.to_path_buf(),
                role: candidate.role.clone(),
                route: None,
                writing: true,
                provider: None,
                seat: None,
                initial_input: None,
            },
        ) {
            Ok(mut pane) => {
                pane.set_report_to(candidate.report_to.clone());
                if candidate.report_reminder_sent {
                    pane.mark_report_reminder_sent();
                }
                pane.settled_mail_sent = candidate.settled_mail_sent;
                pane.set_work_group_id(candidate.work_group_id.clone());
                pane.set_budget_tokens(candidate.budget_tokens);
                pane.set_parent_session(candidate.parent_session.clone());
                panes.push(pane);
                nudge_queues.push(VecDeque::new());
            }
            Err(e) => {
                push_error(errors, format!("restore {}: {e}", candidate.short));
                deferred_restore.push(candidate.clone());
            }
        }
        return;
    }
    let adapter = match adapters::select(Some(&candidate.agent), &[], cfg) {
        Ok(adapter) => adapter,
        Err(e) => {
            push_error(errors, format!("restore {}: {e}", candidate.short));
            deferred_restore.push(candidate.clone());
            return;
        }
    };
    let argv = roster::restore_argv(adapter.as_ref(), candidate);
    let spec = PaneSpec {
        agent_name: candidate.agent.clone(),
        argv,
        // Restore the role recorded in the roster; a coordinator must retain its delegation scope.
        role: prompt::PromptRole::from_label(&candidate.role).unwrap_or(prompt::PromptRole::Worker),
        verb: sessions::Verb::Dash,
        session_id: candidate.session_id.clone(),
        title: candidate.title.clone(),
    };

    let (turn_env, pane_channel) =
        restored_pane_turn_env(cfg, state, repo, candidate, requests_dir, errors);

    match Pane::spawn(
        spec,
        state,
        repo,
        repo,
        size,
        &turn_env,
        adapter.capabilities().turn_signal,
        Duration::from_millis(cfg.dash.idle_quiet_ms),
    ) {
        Ok(mut pane) => {
            // Restore report target and reminder state without sending a second one-shot reminder (#116).
            pane.set_report_to(candidate.report_to.clone());
            if candidate.report_reminder_sent {
                pane.mark_report_reminder_sent();
            }
            pane.settled_mail_sent = candidate.settled_mail_sent;
            pane.set_intake_dir(pane_channel);
            pane.set_work_group_id(candidate.work_group_id.clone());
            pane.set_budget_tokens(candidate.budget_tokens);
            // Restore the pane's server-verified parent for dashboard-side steering checks (#249, #250).
            pane.set_parent_session(candidate.parent_session.clone());
            panes.push(pane);
            nudge_queues.push(VecDeque::new());
        }
        Err(e) => {
            push_error(errors, format!("restore {}: {e}", candidate.short));
            deferred_restore.push(candidate.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::actions::{
        MENU_ENDED, MENU_EXITED_CLEAN, MENU_NOT_ATTACHED, MENU_NOT_RETAINED, MENU_STILL_RUNNING,
    };
    use super::*;

    /// `Esc`, `Enter`, `q` and `a` all acknowledge; only `a` keeps the dialog
    /// open, and the caret keys acknowledge nothing.
    #[test]
    fn the_errors_dialog_acknowledges_on_close_and_on_the_a_hint() {
        let view = ui::ErrorsView {
            items: vec![err_item("boom"), err_item("bang")],
            cursor: 0,
            offset: 0,
            mark: 0,
        };
        for code in [KeyCode::Esc, KeyCode::Enter, KeyCode::Char('q')] {
            let (next, ack) = errors_overlay_reduce(view.clone(), key(code, KeyModifiers::NONE));
            assert!(next.is_none(), "{code:?} closes");
            assert!(ack.is_some(), "{code:?} acknowledges");
        }
        let (next, ack) =
            errors_overlay_reduce(view.clone(), key(KeyCode::Char('a'), KeyModifiers::NONE));
        let next = next.expect("`a` acknowledges in place, without closing");
        assert!(ack.is_some());
        assert!(
            next.items.iter().all(|i| i.acked),
            "the rows go dim where they are"
        );
        let (next, ack) = errors_overlay_reduce(view, key(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(next.expect("still open").cursor, 1);
        assert!(ack.is_none(), "moving the caret is not reading them");
    }

    #[test]
    fn mail_overlay_esc_while_browsing_closes_the_overlay() {
        let (next, effect) = mail_overlay_reduce(
            ui::MailView::default(),
            key(KeyCode::Esc, KeyModifiers::NONE),
        );
        assert!(next.is_none());
        assert!(effect.is_none());
    }

    #[test]
    fn mail_overlay_cursor_clamps_within_bounds() {
        let view = ui::MailView {
            items: vec![
                (PathBuf::from("/a"), "claude".to_string(), "one".to_string()),
                (PathBuf::from("/b"), "codex".to_string(), "two".to_string()),
            ],
            cursor: 0,
            offset: 0,
            compose: None,
        };

        let (next, _) = mail_overlay_reduce(view.clone(), key(KeyCode::Down, KeyModifiers::NONE));
        let next = next.expect("stays open");
        assert_eq!(next.cursor, 1);

        // Past the last row: clamps rather than overflowing.
        let (next, _) = mail_overlay_reduce(next, key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(next.expect("stays open").cursor, 1);

        // Up from row 0 saturates at 0.
        let (next, _) = mail_overlay_reduce(view, key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(next.expect("stays open").cursor, 0);
    }

    #[test]
    fn mail_overlay_c_opens_compose_and_typing_accumulates_the_draft() {
        let (next, effect) = mail_overlay_reduce(ui::MailView::default(), press('c'));
        let next = next.expect("stays open");
        assert!(next.compose.is_some(), "c opens the compose draft");
        assert!(effect.is_none());

        let (next, _) = mail_overlay_reduce(next, press('h'));
        let (next, _) = mail_overlay_reduce(next.expect("stays open"), press('i'));
        let draft = next.expect("stays open").compose.expect("still composing");
        assert_eq!(draft.body, "hi");
    }

    #[test]
    fn mail_overlay_backspace_edits_the_compose_draft() {
        let view = ui::MailView {
            compose: Some(ui::ComposeDraft {
                to: String::new(),
                body: "hix".to_string(),
            }),
            ..ui::MailView::default()
        };
        let (next, _) = mail_overlay_reduce(view, key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(
            next.expect("stays open")
                .compose
                .expect("still composing")
                .body,
            "hi"
        );
    }

    #[test]
    fn mail_overlay_esc_while_composing_cancels_only_the_draft() {
        let view = ui::MailView {
            compose: Some(ui::ComposeDraft {
                to: String::new(),
                body: "half-written".to_string(),
            }),
            ..ui::MailView::default()
        };
        let (next, effect) = mail_overlay_reduce(view, key(KeyCode::Esc, KeyModifiers::NONE));
        let next = next.expect("overlay stays open; only the draft is cancelled");
        assert!(next.compose.is_none());
        assert!(effect.is_none());
    }

    #[test]
    fn mail_overlay_enter_on_an_empty_compose_body_is_a_noop() {
        let view = ui::MailView {
            compose: Some(ui::ComposeDraft::default()),
            ..ui::MailView::default()
        };
        let (next, effect) = mail_overlay_reduce(view, key(KeyCode::Enter, KeyModifiers::NONE));
        let next = next.expect("stays open");
        assert!(next.compose.is_some(), "still composing, nothing was sent");
        assert!(effect.is_none());
    }

    #[test]
    fn mail_overlay_enter_while_composing_emits_a_send_effect() {
        let view = ui::MailView {
            compose: Some(ui::ComposeDraft {
                to: String::new(),
                body: "heads up".to_string(),
            }),
            ..ui::MailView::default()
        };
        let (next, effect) = mail_overlay_reduce(view, key(KeyCode::Enter, KeyModifiers::NONE));
        let next = next.expect("overlay stays open");
        assert!(next.compose.is_none(), "compose closes on submit");
        match effect {
            Some(ui::MailEffect::Send(msg)) => {
                assert_eq!(msg.to, "any");
                assert_eq!(msg.body, "heads up");
            }
            other => panic!("expected a Send effect, got {other:?}"),
        }
    }

    #[test]
    fn mail_overlay_shift_enter_inserts_a_newline_and_does_not_submit() {
        let view = ui::MailView {
            compose: Some(ui::ComposeDraft {
                to: String::new(),
                body: "line one".to_string(),
            }),
            ..ui::MailView::default()
        };
        let (next, effect) = mail_overlay_reduce(view, key(KeyCode::Enter, KeyModifiers::SHIFT));
        let next = next.expect("stays open");
        assert_eq!(next.compose.expect("still composing").body, "line one\n");
        assert!(effect.is_none(), "shift+enter must not submit");
    }

    #[test]
    fn mail_overlay_alt_enter_inserts_a_newline_and_does_not_submit() {
        let view = ui::MailView {
            compose: Some(ui::ComposeDraft {
                to: String::new(),
                body: "line one".to_string(),
            }),
            ..ui::MailView::default()
        };
        let (next, effect) = mail_overlay_reduce(view, key(KeyCode::Enter, KeyModifiers::ALT));
        let next = next.expect("stays open");
        assert_eq!(next.compose.expect("still composing").body, "line one\n");
        assert!(effect.is_none(), "alt+enter must not submit");
    }

    #[test]
    fn mail_overlay_backslash_enter_replaces_the_backslash_with_a_newline() {
        let view = ui::MailView {
            compose: Some(ui::ComposeDraft {
                to: String::new(),
                body: "line one\\".to_string(),
            }),
            ..ui::MailView::default()
        };
        let (next, effect) = mail_overlay_reduce(view, key(KeyCode::Enter, KeyModifiers::NONE));
        let next = next.expect("stays open");
        assert_eq!(next.compose.expect("still composing").body, "line one\n");
        assert!(effect.is_none(), "backslash+enter must not submit");
    }

    #[test]
    fn mail_overlay_enter_on_an_item_emits_consume_and_removes_it_from_the_list() {
        let view = ui::MailView {
            items: vec![(PathBuf::from("/a"), "claude".to_string(), "one".to_string())],
            cursor: 0,
            offset: 0,
            compose: None,
        };
        let (next, effect) = mail_overlay_reduce(view, key(KeyCode::Enter, KeyModifiers::NONE));
        let next = next.expect("stays open");
        assert!(
            next.items.is_empty(),
            "the read item is removed from the view"
        );
        assert_eq!(effect, Some(ui::MailEffect::Consume(PathBuf::from("/a"))));
    }

    #[test]
    fn mail_overlay_enter_on_an_empty_list_is_a_noop() {
        let (next, effect) = mail_overlay_reduce(
            ui::MailView::default(),
            key(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert!(next.is_some());
        assert!(effect.is_none());
    }

    fn memory_view(entries: Vec<(&str, &str, &str)>) -> ui::MemoryView {
        ui::MemoryView {
            entries: entries
                .into_iter()
                .map(|(k, a, b)| (k.to_string(), a.to_string(), b.to_string()))
                .collect(),
            cursor: 0,
            offset: 0,
            input: None,
        }
    }

    #[test]
    fn memory_overlay_esc_while_browsing_closes_the_overlay() {
        let (next, effect) = memory_overlay_reduce(
            ui::MemoryView::default(),
            key(KeyCode::Esc, KeyModifiers::NONE),
        );
        assert!(next.is_none());
        assert!(effect.is_none());
    }

    #[test]
    fn memory_overlay_cursor_clamps_on_an_empty_list() {
        let (next, _) = memory_overlay_reduce(
            ui::MemoryView::default(),
            key(KeyCode::Down, KeyModifiers::NONE),
        );
        assert_eq!(next.expect("stays open").cursor, 0);
    }

    #[test]
    fn memory_overlay_r_prefills_input_from_the_selected_entrys_body() {
        let view = memory_view(vec![(
            "build-cmd",
            "written 1d ago, verified 1d ago",
            "cargo build",
        )]);
        let (next, effect) = memory_overlay_reduce(view, press('r'));
        let next = next.expect("stays open");
        assert_eq!(next.input, Some("cargo build".to_string()));
        assert!(effect.is_none());
    }

    #[test]
    fn memory_overlay_esc_while_editing_cancels_only_the_edit() {
        let mut view = memory_view(vec![("build-cmd", "age", "old body")]);
        view.input = Some("half-typed".to_string());
        let (next, effect) = memory_overlay_reduce(view, key(KeyCode::Esc, KeyModifiers::NONE));
        let next = next.expect("overlay stays open");
        assert!(next.input.is_none());
        assert!(effect.is_none());
    }

    #[test]
    fn memory_overlay_enter_while_editing_emits_remember_and_exits_edit_mode() {
        let mut view = memory_view(vec![("build-cmd", "age", "old body")]);
        view.input = Some("new body".to_string());
        let (next, effect) = memory_overlay_reduce(view, key(KeyCode::Enter, KeyModifiers::NONE));
        let next = next.expect("stays open");
        assert!(next.input.is_none());
        assert_eq!(
            effect,
            Some(ui::MemoryEffect::Remember {
                key: "build-cmd".to_string(),
                body: "new body".to_string(),
            })
        );
    }

    #[test]
    fn memory_overlay_shift_enter_inserts_a_newline_and_does_not_submit() {
        let mut view = memory_view(vec![("build-cmd", "age", "old body")]);
        view.input = Some("new body".to_string());
        let (next, effect) = memory_overlay_reduce(view, key(KeyCode::Enter, KeyModifiers::SHIFT));
        let next = next.expect("stays open");
        assert_eq!(next.input, Some("new body\n".to_string()));
        assert!(effect.is_none(), "shift+enter must not submit");
    }

    #[test]
    fn memory_overlay_alt_enter_inserts_a_newline_and_does_not_submit() {
        let mut view = memory_view(vec![("build-cmd", "age", "old body")]);
        view.input = Some("new body".to_string());
        let (next, effect) = memory_overlay_reduce(view, key(KeyCode::Enter, KeyModifiers::ALT));
        let next = next.expect("stays open");
        assert_eq!(next.input, Some("new body\n".to_string()));
        assert!(effect.is_none(), "alt+enter must not submit");
    }

    #[test]
    fn memory_overlay_backslash_enter_replaces_the_backslash_with_a_newline() {
        let mut view = memory_view(vec![("build-cmd", "age", "old body")]);
        view.input = Some("new body\\".to_string());
        let (next, effect) = memory_overlay_reduce(view, key(KeyCode::Enter, KeyModifiers::NONE));
        let next = next.expect("stays open");
        assert_eq!(next.input, Some("new body\n".to_string()));
        assert!(effect.is_none(), "backslash+enter must not submit");
    }

    #[test]
    fn memory_overlay_d_emits_forget_and_removes_the_entry_locally() {
        let view = memory_view(vec![("drop-me", "age", "body")]);
        let (next, effect) = memory_overlay_reduce(view, press('d'));
        let next = next.expect("stays open");
        assert!(next.entries.is_empty());
        assert_eq!(
            effect,
            Some(ui::MemoryEffect::Forget("drop-me".to_string()))
        );
    }

    #[test]
    fn memory_overlay_v_emits_verify_without_changing_the_list() {
        let view = memory_view(vec![("build-cmd", "age", "body")]);
        let (next, effect) = memory_overlay_reduce(view, press('v'));
        let next = next.expect("stays open");
        assert_eq!(next.entries.len(), 1, "verify does not remove the entry");
        assert_eq!(
            effect,
            Some(ui::MemoryEffect::Verify("build-cmd".to_string()))
        );
    }

    // The disk-reading half: `build_mail_view`/`build_memory_view`.

    #[test]
    fn build_mail_view_lists_every_message_visible_to_the_operator() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let cfg = CtxConfig::default();
        let slug = super::super::state::repo_slug(&repo);
        mail::store(
            &state,
            &slug,
            &mail::Message {
                from_session: "s1".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "hello world".to_string(),
            },
            &cfg,
        )
        .expect("store");

        let view = build_mail_view(&state, &repo);
        assert_eq!(view.items.len(), 1);
        assert_eq!(view.items[0].1, "claude");
        assert_eq!(view.items[0].2, "hello world");
    }

    #[test]
    fn build_memory_view_lists_every_entry_with_its_age() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let cfg = CtxConfig::default();
        let slug = super::super::state::repo_slug(&repo);
        let now = super::super::state::now_secs();
        memory::remember(
            &state,
            &slug,
            &memory::Entry {
                key: "build-cmd".to_string(),
                written_by: "claude".to_string(),
                written: now,
                verified: now,
                source: "explicit".to_string(),
                body: "cargo build".to_string(),
                importance: None,
                confidence: None,
                tags: Vec::new(),
                paths: Vec::new(),
            },
            &cfg,
        )
        .expect("remember");

        let view = build_memory_view(&state, &repo);
        assert_eq!(view.entries.len(), 1);
        assert_eq!(view.entries[0].0, "build-cmd");
        assert_eq!(view.entries[0].2, "cargo build");
        assert!(view.entries[0].1.contains("written"));
    }

    // The executor half: `apply_mail_effect`/`apply_memory_effect`.

    /// D2: the identity is derived exactly as `run_dashboard` derives it --
    /// `sessions::short_id` of the dashboard's own session id -- rather than
    /// handed in as a literal, so this test would notice the derivation moving
    /// back to anything pane-dependent.
    #[test]
    fn apply_mail_effect_send_stamps_identity_and_stores_the_message() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let cfg = CtxConfig::default();
        let mut errors = ErrorLog::default();

        let session_id = "77777777-2222-4333-8444-555555555555";
        let dashboard_short = sessions::short_id(session_id);
        let msg = mail::Message {
            from_session: String::new(),
            from_agent: String::new(),
            to: "any".to_string(),
            to_session: None,
            sent: 0,
            body: "heads up".to_string(),
        };
        apply_mail_effect(
            ui::MailEffect::Send(msg),
            &state,
            &repo,
            &cfg,
            &dashboard_short,
            "claude",
            &mut errors,
        );
        assert!(errors.is_empty(), "got errors: {errors:?}");

        let slug = super::super::state::repo_slug(&repo);
        let listed = mail::list(&state, &slug, None, None).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].1.from_session, dashboard_short);
        assert_eq!(listed[0].1.from_agent, "claude");
    }

    /// A1-3: `Ctrl+A e` acknowledges the errors the operator actually saw.
    /// An error that arrived AFTER the dialog took its snapshot was never on
    /// screen, so closing the dialog must leave the sticky `⚠` up for it.
    #[test]
    fn acknowledging_the_errors_dialog_never_clears_an_error_it_never_showed() {
        let now = Instant::now();
        let mut errors = ErrorLog::default();
        errors.record("supervisor a failed".to_string(), now);
        let view = build_errors_view(&errors, now);
        errors.record("supervisor b failed".to_string(), now);

        let (next, ack) = errors_overlay_reduce(view, key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(next.is_none(), "Esc closes the dialog");
        errors.acknowledge(ack.expect("Esc acknowledges on the way out").mark);

        assert_eq!(
            errors.sticky_line().as_deref(),
            Some("supervisor b failed"),
            "the error that arrived after the snapshot was never seen: {errors:?}"
        );
    }

    // F11: the spawn dialog's own reducer.

    fn type_line(line: &str) -> ui::SpawnDraft {
        let mut draft = ui::SpawnDraft::default();
        for c in line.chars() {
            let (next, effect) = spawn_overlay_reduce(draft, press(c));
            assert!(effect.is_none(), "typing emits no effect");
            draft = next.expect("typing keeps the dialog open");
        }
        draft
    }

    #[test]
    fn spawn_dialog_enter_splits_the_agent_from_the_prompt() {
        let draft = type_line("claude fix the failing tests");
        let (next, effect) = spawn_overlay_reduce(draft, key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(next.is_none(), "a submitted dialog closes");
        assert_eq!(
            effect,
            Some(SpawnEffect::Submit {
                agent: "claude".to_string(),
                prompt: "fix the failing tests".to_string(),
            })
        );
    }

    #[test]
    fn spawn_dialog_shift_enter_inserts_a_newline_and_does_not_submit() {
        let draft = type_line("claude");
        let (next, effect) = spawn_overlay_reduce(draft, key(KeyCode::Enter, KeyModifiers::SHIFT));
        let next = next.expect("stays open");
        assert_eq!(next.input, "claude\n");
        assert!(effect.is_none(), "shift+enter must not submit");
    }

    #[test]
    fn spawn_dialog_alt_enter_inserts_a_newline_and_does_not_submit() {
        let draft = type_line("claude");
        let (next, effect) = spawn_overlay_reduce(draft, key(KeyCode::Enter, KeyModifiers::ALT));
        let next = next.expect("stays open");
        assert_eq!(next.input, "claude\n");
        assert!(effect.is_none(), "alt+enter must not submit");
    }

    #[test]
    fn spawn_dialog_backslash_enter_replaces_the_backslash_with_a_newline() {
        let draft = type_line("claude line one\\");
        let (next, effect) = spawn_overlay_reduce(draft, key(KeyCode::Enter, KeyModifiers::NONE));
        let next = next.expect("stays open");
        assert_eq!(next.input, "claude line one\n");
        assert!(effect.is_none(), "backslash+enter must not submit");
    }

    #[test]
    fn spawn_dialog_needs_both_an_agent_and_a_prompt() {
        for line in ["", "   ", "claude", "claude   "] {
            let draft = type_line(line);
            let (next, effect) =
                spawn_overlay_reduce(draft, key(KeyCode::Enter, KeyModifiers::NONE));
            assert!(
                next.is_some(),
                "the dialog stays open so the typed text is not lost: {line:?}"
            );
            assert_eq!(
                effect,
                Some(SpawnEffect::Notice(SPAWN_USAGE_NOTICE.to_string())),
                "got no notice for {line:?}"
            );
        }
    }

    #[test]
    fn spawn_dialog_backspace_edits_and_esc_cancels() {
        let draft = type_line("claudex");
        let (next, _) = spawn_overlay_reduce(draft, key(KeyCode::Backspace, KeyModifiers::NONE));
        let draft = next.expect("stays open");
        assert_eq!(draft.input, "claude");

        let (next, effect) = spawn_overlay_reduce(draft, key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(next.is_none(), "Esc closes the dialog");
        assert!(effect.is_none(), "and asks for nothing");
    }

    /// The dialog does not re-implement the argv guard, the pane cap or the
    /// agent gate: it submits, and the shared `fulfill_spawn_request` path
    /// refuses. This pins that a flag-shaped prompt does reach that path
    /// intact (rather than being silently mangled or split into flags here).
    #[test]
    fn spawn_dialog_submits_a_flag_shaped_prompt_for_the_shared_guard_to_refuse() {
        let draft = type_line("claude --dangerously-skip-permissions");
        let (_, effect) = spawn_overlay_reduce(draft, key(KeyCode::Enter, KeyModifiers::NONE));
        match effect {
            Some(SpawnEffect::Submit { prompt, .. }) => {
                assert_eq!(prompt, "--dangerously-skip-permissions");
                assert!(
                    argv_unsafe_prompt(&prompt),
                    "and the shared guard is what refuses it"
                );
            }
            other => panic!("expected a Submit, got {other:?}"),
        }
    }

    // Task 12: the startup restore dialog's pure reducer, `build_restore_view`,
    // and `on_quit`'s own roster write.

    fn restore_entry(label: &str, checked: bool) -> ui::RestoreEntry {
        ui::RestoreEntry {
            label: label.to_string(),
            checked,
        }
    }

    #[test]
    fn restore_overlay_esc_skips_with_no_effect() {
        let view = ui::RestoreView {
            entries: vec![restore_entry("a", true)],
            cursor: 0,
            offset: 0,
        };
        let (next, effect) = restore_overlay_reduce(view, key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(next.is_none());
        assert!(effect.is_none());
    }

    #[test]
    fn restore_overlay_space_toggles_the_entry_under_the_cursor() {
        let view = ui::RestoreView {
            entries: vec![restore_entry("a", true), restore_entry("b", true)],
            cursor: 1,
            offset: 0,
        };
        let (next, effect) = restore_overlay_reduce(view, press(' '));
        let next = next.expect("stays open");
        assert!(next.entries[0].checked, "untouched entry stays checked");
        assert!(
            !next.entries[1].checked,
            "entry under the cursor toggles off"
        );
        assert!(effect.is_none());
    }

    #[test]
    fn restore_overlay_enter_confirms_only_the_checked_indices() {
        let view = ui::RestoreView {
            entries: vec![
                restore_entry("a", true),
                restore_entry("b", false),
                restore_entry("c", true),
            ],
            cursor: 0,
            offset: 0,
        };
        let (next, effect) = restore_overlay_reduce(view, key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(next.is_none(), "confirming closes the dialog");
        assert_eq!(effect, Some(RestoreEffect::Confirm(vec![0, 2])));
    }

    #[test]
    fn restore_overlay_enter_with_nothing_checked_still_confirms_an_empty_set() {
        let view = ui::RestoreView {
            entries: vec![restore_entry("a", false)],
            cursor: 0,
            offset: 0,
        };
        let (next, effect) = restore_overlay_reduce(view, key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(next.is_none());
        assert_eq!(effect, Some(RestoreEffect::Confirm(Vec::new())));
    }

    #[test]
    fn restore_overlay_cursor_clamps_within_bounds() {
        let view = ui::RestoreView {
            entries: vec![restore_entry("a", true), restore_entry("b", true)],
            cursor: 0,
            offset: 0,
        };
        let (next, _) = restore_overlay_reduce(view, key(KeyCode::Down, KeyModifiers::NONE));
        let next = next.expect("stays open");
        assert_eq!(next.cursor, 1);

        let (next, _) = restore_overlay_reduce(next, key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            next.expect("stays open").cursor,
            1,
            "past the last row clamps rather than overflowing"
        );
    }

    // -- issue #354 phase 3: the context menu ------------------------------

    fn menu_facts(short: &str) -> MenuFacts {
        MenuFacts {
            short: short.to_string(),
            role: "worker".to_string(),
            attached: true,
            alive: true,
            ended: false,
            exit_code: None,
            retained: false,
            has_request: false,
            cwd: Some("D:/repo".to_string()),
        }
    }

    /// The enabled/disabled matrix, per row state. Every entry is always
    /// present -- an operator who cannot see that `restore` exists cannot
    /// learn why it is unavailable -- and every unavailable one carries a
    /// reason.
    #[test]
    fn the_context_menu_offers_every_entry_and_says_why_each_is_unavailable() {
        use ui::MenuAction as A;

        // An alive, attached pane: everything that acts on a running child.
        let alive = menu_entries(&menu_facts("aaaa1111"));
        for action in [
            A::Inspect,
            A::Focus,
            A::Nudge,
            A::Mail,
            A::Handover,
            A::Stop,
            A::OpenWorktree,
            A::Evidence,
        ] {
            assert!(entry(&alive, action).enabled(), "{action:?}");
        }
        assert_eq!(
            entry(&alive, A::Restore).disabled.as_deref(),
            Some(MENU_STILL_RUNNING)
        );
        assert_eq!(
            entry(&alive, A::Retry).disabled.as_deref(),
            Some(MENU_STILL_RUNNING)
        );
        assert_eq!(
            entry(&alive, A::Dismiss).disabled.as_deref(),
            Some(MENU_NOT_RETAINED)
        );

        // A view-only registry row: no pane to focus, stop or swap, but a
        // nudge still reaches it through the headless marker path.
        let view_only = MenuFacts {
            attached: false,
            cwd: None,
            ..menu_facts("cccc3333")
        };
        let entries = menu_entries(&view_only);
        for action in [A::Focus, A::Handover, A::Stop] {
            assert_eq!(
                entry(&entries, action).disabled.as_deref(),
                Some(MENU_NOT_ATTACHED),
                "{action:?}"
            );
        }
        assert!(entry(&entries, A::Nudge).enabled());
        assert_eq!(
            entry(&entries, A::OpenWorktree).disabled.as_deref(),
            Some(MENU_NO_CWD)
        );

        // An ended row with no kept request: restore and retry both say so.
        let ended_no_request = MenuFacts {
            attached: false,
            alive: false,
            ended: true,
            exit_code: Some(0),
            retained: true,
            has_request: false,
            ..menu_facts("bbbb2222")
        };
        let entries = menu_entries(&ended_no_request);
        assert_eq!(
            entry(&entries, A::Restore).disabled.as_deref(),
            Some(MENU_NO_REQUEST)
        );
        assert_eq!(
            entry(&entries, A::Retry).disabled.as_deref(),
            Some(MENU_NO_REQUEST)
        );
        assert_eq!(
            entry(&entries, A::Nudge).disabled.as_deref(),
            Some(MENU_ENDED)
        );
        assert_eq!(
            entry(&entries, A::Stop).disabled.as_deref(),
            Some(MENU_ENDED)
        );
        assert!(entry(&entries, A::Dismiss).enabled());
        // The inspector and its evidence section describe any row at all.
        assert!(entry(&entries, A::Inspect).enabled());
        assert!(entry(&entries, A::Evidence).enabled());

        // An ended row with a kept request: restore is on, retry is not --
        // it exited cleanly, so there is no failure to retry.
        let ended_clean = MenuFacts {
            has_request: true,
            ..ended_no_request
        };
        let entries = menu_entries(&ended_clean);
        assert!(entry(&entries, A::Restore).enabled());
        assert_eq!(
            entry(&entries, A::Retry).disabled.as_deref(),
            Some(MENU_EXITED_CLEAN)
        );

        // An ended row that failed: both are on.
        let ended_failed = MenuFacts {
            exit_code: Some(1),
            ..ended_clean.clone()
        };
        let entries = menu_entries(&ended_failed);
        assert!(entry(&entries, A::Restore).enabled());
        assert!(entry(&entries, A::Retry).enabled());

        // Whatever the row, the menu is the same shape in the same order.
        for facts in [alive_facts(), ended_clean.clone(), view_only.clone()] {
            let entries = menu_entries(&facts);
            assert_eq!(entries.len(), 11);
            assert_eq!(
                entries.iter().map(|e| e.action).collect::<Vec<_>>(),
                vec![
                    A::Inspect,
                    A::Focus,
                    A::Nudge,
                    A::Mail,
                    A::Handover,
                    A::Stop,
                    A::Restore,
                    A::OpenWorktree,
                    A::Evidence,
                    A::Retry,
                    A::Dismiss,
                ]
            );
            for e in &entries {
                assert!(
                    e.enabled() || e.disabled.as_ref().is_some_and(|r| !r.trim().is_empty()),
                    "{:?} is disabled with no reason",
                    e.action
                );
            }
        }
    }

    fn alive_facts() -> MenuFacts {
        menu_facts("aaaa1111")
    }

    /// The menu is opened for whatever row the gesture named, which is not
    /// necessarily the selected one -- and the title says which.
    #[test]
    fn a_right_click_menu_targets_its_own_row_not_the_selection() {
        let panes = vec![
            pane_row("aaaa1111", "claude"),
            pane_row("bbbb2222", "codex"),
        ];
        // Row 0 is selected; the menu is raised on row 1.
        let rows = assemble_sidebar(&panes, &[], &HashMap::new(), 0, 0, DASHBOARD_PID, 0);
        let target = menu_facts_for(&rows[1], &[], &VecDeque::new());
        let view = build_menu_view(&target);
        assert_eq!(view.target, "bbbb2222");
        assert!(
            view.subject.contains("bbbb2222"),
            "the menu title names its target: {}",
            view.subject
        );
        assert_eq!(view.cursor, 0);
        assert!(view.confirm.is_none());
        // And `^A c` on the selection raises the menu for THAT row instead.
        let selected = menu_facts_for(&rows[0], &[], &VecDeque::new());
        assert_eq!(build_menu_view(&selected).target, "aaaa1111");
    }

    /// Enter activates, a letter jumps AND activates, a disabled entry is
    /// inert, and Esc closes with no effect at all.
    #[test]
    fn the_menu_activates_on_enter_and_on_its_own_letters() {
        let view = build_menu_view(&alive_facts());
        // Enter on the first entry (`inspect`).
        let (next, effect) = menu_overlay_reduce(view.clone(), press('\0'));
        assert!(next.is_some(), "an unknown key leaves the menu open");
        assert!(effect.is_none());

        let (next, effect) =
            menu_overlay_reduce(view.clone(), key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(next.is_none(), "activating closes the menu");
        assert_eq!(
            effect,
            Some(MenuEffect {
                target: "aaaa1111".to_string(),
                action: ui::MenuAction::Inspect,
            })
        );

        // `e` jumps to `evidence` and fires it in one keystroke.
        let (next, effect) = menu_overlay_reduce(view.clone(), press('e'));
        assert!(next.is_none());
        assert_eq!(effect.map(|e| e.action), Some(ui::MenuAction::Evidence));

        // `r` is `restore`, which is disabled for a live pane: the menu stays
        // open on it and nothing happens.
        let (next, effect) = menu_overlay_reduce(view.clone(), press('r'));
        let next = next.expect("a disabled entry leaves the menu open");
        assert_eq!(next.entries[next.cursor].action, ui::MenuAction::Restore);
        assert!(effect.is_none());

        // A letter no entry claims moves nothing.
        let (next, effect) = menu_overlay_reduce(view.clone(), press('x'));
        assert_eq!(next.expect("stays open").cursor, 0);
        assert!(effect.is_none());

        // Esc closes with no effect -- and the caller never touches focus.
        let (next, effect) =
            menu_overlay_reduce(view.clone(), key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(next.is_none());
        assert!(effect.is_none());

        // j/k walk the menu, clamped at both ends.
        let (down, _) = menu_overlay_reduce(view.clone(), press('j'));
        assert_eq!(down.expect("stays open").cursor, 1);
        let (up, _) = menu_overlay_reduce(view, press('k'));
        assert_eq!(up.expect("stays open").cursor, 0, "clamps at the top");
    }

    /// `stop` is the one destructive entry, so it confirms inline first --
    /// and backing out of the confirmation kills nothing.
    #[test]
    fn stop_confirms_inline_before_anything_is_killed() {
        let view = build_menu_view(&alive_facts());
        let (armed, effect) = menu_overlay_reduce(view, press('s'));
        let armed = armed.expect("the confirmation keeps the menu open");
        assert!(effect.is_none(), "nothing is stopped by arming it");
        let index = armed.confirm.expect("the confirmation is armed");
        assert_eq!(armed.entries[index].action, ui::MenuAction::Stop);

        // `n` backs out: still open, still nothing stopped.
        let (backed_out, effect) = menu_overlay_reduce(armed.clone(), press('n'));
        let backed_out = backed_out.expect("stays open");
        assert!(backed_out.confirm.is_none());
        assert!(effect.is_none());
        // ... and so does Esc.
        let (escaped, effect) =
            menu_overlay_reduce(armed.clone(), key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(escaped.expect("stays open").confirm.is_none());
        assert!(effect.is_none());

        // `y` confirms.
        let (next, effect) = menu_overlay_reduce(armed.clone(), press('y'));
        assert!(next.is_none());
        assert_eq!(effect.map(|e| e.action), Some(ui::MenuAction::Stop));
        // A second Enter confirms too.
        let (next, effect) = menu_overlay_reduce(armed, key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(next.is_none());
        assert_eq!(effect.map(|e| e.action), Some(ui::MenuAction::Stop));
    }

    // -- issue #354 phase 3: restore -----------------------------------------

    /// A retained row that kept its spawn request is restorable and its cwd
    /// is known, so `restore`, `retry` (it failed) and `open worktree` are
    /// all on; one that kept nothing says exactly why.
    #[test]
    fn a_retained_row_is_restorable_only_while_its_spawn_request_is_kept() {
        let request = spawn_request("do the work", Path::new("D:/repo"));
        let mut retained: VecDeque<EndedRow> = VecDeque::new();
        push_retained_ended(
            &mut retained,
            EndedRow {
                short: "bbb22222".into(),
                role: "worker".into(),
                model: None,
                harness: "claude".into(),
                group_id: None,
                parent: None,
                budget: style::PLACEHOLDER.into(),
                writer: style::PLACEHOLDER.into(),
                cwd: "D:/repo".into(),
                request: Some(request.clone()),
                requested_by: Some("aaa11111".into()),
                meta: EndedMeta {
                    exit_code: 1,
                    exited_at: 600,
                    age_secs: Some(300),
                },
            },
            MAX_RETAINED_ENDED_ROWS,
        );
        let metas = build_pane_rows(&[], &retained);
        let rows = assemble_sidebar(&metas, &[], &HashMap::new(), 0, 0, DASHBOARD_PID, 900);
        assert_eq!(
            row_cwd("bbb22222", &[], &retained).as_deref(),
            Some("D:/repo")
        );
        assert_eq!(row_cwd("nobody", &[], &retained), None);

        let facts = menu_facts_for(&rows[0], &[], &retained);
        assert!(facts.ended && facts.retained && facts.has_request);
        let entries = menu_entries(&facts);
        assert!(entry(&entries, ui::MenuAction::Restore).enabled());
        assert!(entry(&entries, ui::MenuAction::Retry).enabled());
        assert!(entry(&entries, ui::MenuAction::Dismiss).enabled());
        assert!(entry(&entries, ui::MenuAction::OpenWorktree).enabled());

        // Drop the kept request and the two relaunch entries say so, while
        // the row itself is still there to be inspected and dismissed.
        retained[0].request = None;
        let facts = menu_facts_for(&rows[0], &[], &retained);
        let entries = menu_entries(&facts);
        assert_eq!(
            entry(&entries, ui::MenuAction::Restore).disabled.as_deref(),
            Some(MENU_NO_REQUEST)
        );
        assert_eq!(
            entry(&entries, ui::MenuAction::Retry).disabled.as_deref(),
            Some(MENU_NO_REQUEST)
        );
        assert!(entry(&entries, ui::MenuAction::Dismiss).enabled());
    }

    /// Renders one overlay into a `width x height` frame and returns what
    /// landed on the cells -- the same technique `ui`'s own render tests use,
    /// reached from here so the dashboard inspector can be checked end to end.
    fn render_overlay_text(width: u16, height: u16, overlay: &ui::Overlay) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut term = Terminal::new(backend).expect("terminal");
        term.draw(|f| ui::render_overlay(f, f.area(), overlay, 0))
            .expect("draw");
        let buf = term.backend().buffer().clone();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Issue #354 phase 5: the DASHBOARD-level inspector -- every section the
    /// approved design names, filled from cached facts only.
    #[test]
    fn the_dashboard_inspector_reports_every_section_from_cached_facts() {
        let panes = vec![pane_row("aaaa1111", "claude")];
        let rows = assemble_sidebar(&panes, &[], &HashMap::new(), 0, 0, DASHBOARD_PID, 0);
        let usage = vec![ui::HarnessUsage {
            name: "claude",
            five_hour: Some(61.0),
            seven_day: Some(18.0),
            five_hour_detail: None,
            seven_day_detail: None,
        }];
        let pool = vec![ui::HarnessStrip {
            name: "claude".to_string(),
            state: "ready".to_string(),
            headroom_pct: Some(64.0),
        }];
        let workflow = workflow::ActiveWorkflowSummary {
            kind: "feature",
            step: "design".to_string(),
            awaiting_approval: true,
            pack: String::new(),
            steps: Vec::new(),
            title: String::new(),
            started_at: 0,
            next_gate: None,
        };
        let facts = DashboardFacts {
            harness: "claude \u{b7} fable",
            short: "a0000001",
            seat: Some("gen 3"),
            state_dir: "D:/state".to_string(),
            uptime_secs: 840,
            mouse: true,
            sidebar_cols: 44,
            facts_age_secs: 1,
            rows: &rows,
            spend: Some(AggregateSpendFacts { failed: 2 }),
            usage: &usage,
            pool: &pool,
            mail: Some((1, 0)),
            workflow: Some(&workflow),
            supervised: (1, 1),
        };
        let view = build_dashboard_inspector(&facts);
        assert_eq!(view.target, DASHBOARD_TARGET);
        assert_eq!(view.subject, "dashboard \u{b7} a0000001");
        let names: Vec<&str> = view.sections.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                INSPECT_DASH_HARNESS,
                INSPECT_DASH_SESSIONS,
                INSPECT_DASH_DELEGATION,
                INSPECT_DASH_USAGE,
                INSPECT_DASH_REPO,
                INSPECT_DASH_DASHBOARD,
            ]
        );
        let all = view.rows().join("\n");
        assert!(all.contains("gen 3"), "{all}");
        assert!(all.contains("14m"), "uptime reads as an age: {all}");
        // Live/ended plus one line per glyph, every one of the six.
        for glyph in ui::ALL_GLYPHS {
            assert!(
                all.contains(glyph.name()),
                "missing {}: {all}",
                glyph.name()
            );
        }
        assert!(all.contains("headroom 64%"), "{all}");
        assert!(!all.contains('$'), "no cost in the inspector: {all}");
        assert!(all.contains("5h 61%"), "{all}");
        assert!(all.contains("1 broadcast"), "{all}");
        assert!(all.contains("awaits approval"), "{all}");
        assert!(all.contains("1 of 1 panes"), "{all}");
        assert!(all.contains("D:/state"), "{all}");
        assert!(all.contains("44 cols"), "{all}");
    }

    /// Nothing read yet: every unknown cell is the shared placeholder, never
    /// a fabricated zero, and the dialog still draws at all three approved
    /// frame sizes.
    #[test]
    fn the_dashboard_inspector_is_all_placeholders_and_renders_at_every_frame_size() {
        let facts = DashboardFacts {
            harness: "claude",
            short: "a0000001",
            seat: None,
            state_dir: "D:/state".to_string(),
            uptime_secs: 0,
            mouse: false,
            sidebar_cols: 44,
            facts_age_secs: 0,
            rows: &[],
            spend: None,
            usage: &[],
            pool: &[],
            mail: None,
            workflow: None,
            supervised: (0, 0),
        };
        let view = build_dashboard_inspector(&facts);
        let all = view.rows().join("\n");
        assert!(all.contains(style::PLACEHOLDER), "{all}");
        assert!(
            all.contains(&inspect_line("mouse", Some("off".to_string()))),
            "mouse off says so: {all}"
        );
        // The usage section has no harnesses at all, so it draws the shared
        // empty-section placeholder rather than vanishing.
        assert!(
            view.sections
                .iter()
                .any(|s| s.name == INSPECT_DASH_USAGE && s.lines.is_empty())
        );
        let overlay = ui::Overlay::Inspector(view);
        for (w, h) in [(80u16, 20u16), (120, 40), (200, 50)] {
            let text = render_overlay_text(w, h, &overlay);
            // A2-3: the second operand used to repeat the first verbatim, so
            // the subject the dialog is opened ON was never asserted at all.
            assert!(text.contains("dashboard"), "{w}x{h}: {text}");
            assert!(text.contains("a0000001"), "{w}x{h}: {text}");
            assert!(text.contains("harness"), "{w}x{h}: {text}");
        }
    }

    /// Both entry points the approved design names: `^A i` while the summary
    /// line holds the cursor, and the summary line's own action menu.
    #[test]
    fn the_summary_line_opens_the_dashboard_inspector_by_key_and_by_menu() {
        // The key: `^A i` is the same chord, and the availability table says
        // it applies with the summary selected.
        let ctx = selected_action_context(&[], 0, &[], &VecDeque::new(), true);
        assert!(ctx.summary);
        assert!(
            actions::descriptor(actions::ActionId::Inspect)
                .map(|d| (d.availability)(&ctx))
                .is_some_and(|a| a.is_enabled())
        );
        assert_eq!(
            actions::descriptor(actions::ActionId::Nudge)
                .map(|d| (d.availability)(&ctx))
                .and_then(|a| a.reason()),
            Some(actions::MENU_SUMMARY_LINE),
            "a per-session action stays listed, with its reason"
        );
        // The menu: every entry is present, `inspect` is the only live one,
        // and activating it names the dashboard rather than a row.
        let menu = build_summary_menu_view();
        assert_eq!(menu.target, DASHBOARD_TARGET);
        assert_eq!(menu.subject, "the dashboard");
        assert_eq!(menu.entries.len(), 11);
        let live: Vec<ui::MenuAction> = menu
            .entries
            .iter()
            .filter(|e| e.enabled())
            .map(|e| e.action)
            .collect();
        // `mail` is dashboard-wide, so it stays live here; every entry that
        // needs a session is inert with its reason.
        assert_eq!(live, vec![ui::MenuAction::Inspect, ui::MenuAction::Mail]);
        let (next, effect) = menu_overlay_reduce(menu, key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(next.is_none());
        assert_eq!(
            effect,
            Some(MenuEffect {
                target: DASHBOARD_TARGET.to_string(),
                action: ui::MenuAction::Inspect,
            })
        );
    }

    /// A row with no cached status at all still inspects: every section is
    /// there, filled with the placeholder rather than missing.
    #[test]
    fn the_inspector_on_a_row_with_no_facts_yet_is_all_placeholders() {
        let panes = vec![pane_row("aaaa1111", "claude")];
        let rows = assemble_sidebar(&panes, &[], &HashMap::new(), 0, 0, DASHBOARD_PID, 0);
        let view = build_inspector_view(&rows[0], None, &ErrorLog::default());
        assert_eq!(view.sections.len(), 7);
        let evidence = view
            .sections
            .iter()
            .find(|s| s.name == INSPECT_EVIDENCE)
            .expect("evidence section is never dropped");
        assert!(evidence.lines.is_empty());
        // An empty section still draws one placeholder row, so the dialog
        // never silently loses a heading.
        let drawn = view.rows();
        let start = view.section_start(INSPECT_EVIDENCE);
        assert_eq!(drawn[start], format!("{INSPECT_EVIDENCE}:"));
        assert_eq!(drawn[start + 1], format!("  {}", style::PLACEHOLDER));
        // Read-only: Esc closes, j/k scroll, nothing else does anything.
        let (moved, _) = (
            inspector_overlay_reduce(view.clone(), press('j')).expect("stays open"),
            (),
        );
        assert_eq!(moved.cursor, 1);
        assert!(inspector_overlay_reduce(view, key(KeyCode::Esc, KeyModifiers::NONE)).is_none());
    }

    #[test]
    fn build_restore_view_defaults_every_entry_to_checked_and_labels_it() {
        let candidates = vec![roster::RosterPane {
            agent: "claude".to_string(),
            session_id: "sess-1".to_string(),
            role: prompt::PromptRole::Worker.label().to_string(),
            short: "aaaa1111".to_string(),
            title: "wrk claude".to_string(),
            ..Default::default()
        }];
        let view = build_restore_view(&candidates);
        assert_eq!(view.entries.len(), 1);
        assert!(view.entries[0].checked);
        assert!(
            view.entries[0].label.contains("aaaa1111"),
            "got {}",
            view.entries[0].label
        );
    }

    /// F6: an orchestrator roster entry never reaches `build_restore_view` or
    /// `roster::restore_argv`. Its stored `session_id` is zirv's own uuid even
    /// when the operator pinned the conversation themselves (see
    /// `chat::dash_orchestrator_pane`), so resuming from it would ask the
    /// harness for a conversation that never existed under that id -- and the
    /// fresh launch has already spawned its own orchestrator anyway.
    #[test]
    fn the_orchestrator_is_never_offered_for_restore() {
        let orchestrator = roster::RosterPane {
            agent: "claude".to_string(),
            session_id: "11111111-2222-4333-8444-555555555555".to_string(),
            role: roster::ROLE_ORCHESTRATOR.to_string(),
            short: "aaaa1111".to_string(),
            title: "orch".to_string(),
            ..Default::default()
        };
        let worker = roster::RosterPane {
            agent: "codex".to_string(),
            session_id: "22222222-2222-4333-8444-555555555555".to_string(),
            role: prompt::PromptRole::Worker.label().to_string(),
            short: "bbbb2222".to_string(),
            title: "wrk codex".to_string(),
            ..Default::default()
        };
        let taken = roster::Roster {
            written: 1_000,
            panes: vec![orchestrator, worker.clone()],
        };

        let candidates = restorable_candidates(taken);
        assert_eq!(
            candidates,
            vec![worker],
            "only workers survive the filter, so only workers ever reach restore_argv"
        );
        let view = build_restore_view(&candidates);
        assert_eq!(view.entries.len(), 1);
        assert!(
            !view.entries[0].label.contains("orch"),
            "and the dialog never offers one either: {:?}",
            view.entries[0].label
        );
    }

    // R7: restoring is creating panes, so it answers to the same cap.

    #[test]
    fn restore_budget_stops_at_the_pane_cap() {
        assert_eq!(
            restore_budget(0, 2, 3),
            (2, 1),
            "a roster of three under a cap of two restores two and reports one skipped"
        );
        assert_eq!(
            restore_budget(1, 2, 3),
            (1, 2),
            "the orchestrator already occupies a slot"
        );
        assert_eq!(
            restore_budget(2, 2, 3),
            (0, 3),
            "a full dashboard restores nothing"
        );
        assert_eq!(
            restore_budget(5, 2, 3),
            (0, 3),
            "and saturates rather than wrapping"
        );
        assert_eq!(
            restore_budget(0, 9, 3),
            (3, 0),
            "room for everything skips nothing"
        );
    }

    /// G3: a confirmed selection under the pane cap is split into what gets
    /// spawned (the first `take`, per `restore_budget`) and what the cap
    /// forced this launch to defer -- and the deferred half must still be the
    /// original `RosterPane`s, not merely dropped indices.
    #[test]
    fn partition_restore_selection_defers_what_the_cap_skips() {
        let candidates = vec![
            restore_pane("aaaa1111", "11111111-2222-4333-8444-555555555555"),
            restore_pane("bbbb2222", "22222222-2222-4333-8444-555555555555"),
            restore_pane("cccc3333", "33333333-2222-4333-8444-555555555555"),
        ];

        // cap 2, roster 3, confirm all -> 2 to spawn, the third deferred.
        let (take, _skipped) = restore_budget(0, 2, 3);
        let (to_spawn, deferred) = partition_restore_selection(vec![0, 1, 2], &candidates, take);

        assert_eq!(to_spawn, vec![candidates[0].clone(), candidates[1].clone()]);
        assert_eq!(deferred, vec![candidates[2].clone()]);
    }

    #[test]
    fn partition_restore_selection_defers_nothing_under_budget() {
        let candidates = vec![restore_pane(
            "aaaa1111",
            "11111111-2222-4333-8444-555555555555",
        )];
        let (take, _skipped) = restore_budget(0, 9, 1);
        let (to_spawn, deferred) = partition_restore_selection(vec![0], &candidates, take);

        assert_eq!(to_spawn, candidates);
        assert!(deferred.is_empty());
    }

    #[test]
    fn partition_restore_selection_ignores_a_stale_index() {
        let candidates = vec![restore_pane(
            "aaaa1111",
            "11111111-2222-4333-8444-555555555555",
        )];
        let (to_spawn, deferred) = partition_restore_selection(vec![5], &candidates, 1);

        assert!(to_spawn.is_empty());
        assert!(deferred.is_empty());
    }

    /// Issue #160 finding 1, review round (2026-08-28): a restore must
    /// relaunch a pane "on the same terms as a freshly spawned one" -- a
    /// worker pane that WAS interactive-pinned at its original spawn
    /// (`RosterPane::interactive == true`, recorded from `Pane::launch_mode`
    /// at quit time) gets the pin back on restore. Also FINDING 3: asserts
    /// all three env pairs `restored_pane_turn_env`'s own doc comment claims
    /// are pinned directly -- `DASH_REQUESTS_ENV` and `WORK_GROUP_ENV` had
    /// zero coverage before this round even though the doc comment claimed
    /// otherwise.
    #[test]
    fn restored_pane_turn_env_pins_interactive_when_the_original_pane_was_interactive() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let requests_dir = state.dash().join("aaaa1111-token").join("requests");
        std::fs::create_dir_all(&requests_dir).expect("mkdir requests");

        let mut candidate = restore_pane("cccc3333", "33333333-2222-4333-8444-555555555555");
        candidate.interactive = true;
        candidate.work_group_id = Some("wg-42".to_string());
        let cfg = CtxConfig::default();
        let mut errors = ErrorLog::default();

        let (turn_env, pane_channel) =
            restored_pane_turn_env(&cfg, &state, &repo, &candidate, &requests_dir, &mut errors);

        assert!(
            turn_env.contains(&(
                super::super::adapters::LAUNCH_MODE_ENV.to_string(),
                super::super::adapters::LAUNCH_MODE_INTERACTIVE_VALUE.to_string()
            )),
            "a restored pane that was originally interactive-pinned must carry the durable \
             interactive-launch pin again: {turn_env:?}"
        );
        assert!(
            turn_env.contains(&(
                spawnreq::DASH_REQUESTS_ENV.to_string(),
                pane_channel.display().to_string()
            )),
            "a restored pane gets its own fresh spawn-request channel: {turn_env:?}"
        );
        assert!(
            turn_env.contains(&(
                super::super::agent::WORK_GROUP_ENV.to_string(),
                "wg-42".to_string()
            )),
            "the roster's group binding must travel back with the restored pane: {turn_env:?}"
        );
    }

    /// Fix 4 (issue #249/#250 review): the roster's own recorded parent
    /// lineage travels back with a restored pane too, the same way the
    /// group binding above does -- without this, a quit/restore round-trip
    /// silently downgraded a genuine worker's steering mail to peer, since
    /// the restored child's own real process env carried no `PARENT_
    /// SESSION_ENV` at all.
    #[test]
    fn restored_pane_turn_env_carries_the_roster_parent_session_forward() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let requests_dir = state.dash().join("aaaa1111-token").join("requests");
        std::fs::create_dir_all(&requests_dir).expect("mkdir requests");

        let mut candidate = restore_pane("cccc3333", "33333333-2222-4333-8444-555555555555");
        candidate.parent_session = Some("orch0001".to_string());
        let cfg = CtxConfig::default();
        let mut errors = ErrorLog::default();

        let (turn_env, _pane_channel) =
            restored_pane_turn_env(&cfg, &state, &repo, &candidate, &requests_dir, &mut errors);

        assert!(
            turn_env.contains(&(
                super::super::agent::PARENT_SESSION_ENV.to_string(),
                "orch0001".to_string()
            )),
            "the roster's own parent lineage must travel back with the restored pane: \
             {turn_env:?}"
        );
    }

    /// The other half of Fix 4: a roster entry with no recorded parent (an
    /// old-format entry, or a pane that genuinely never had one) must not
    /// fabricate one -- no `PARENT_SESSION_ENV` pair at all, the same
    /// fail-safe shape `restored_pane_turn_env_restores_without_the_pin_for_
    /// a_non_interactive_worker_pane` proves for the interactive pin.
    #[test]
    fn restored_pane_turn_env_carries_no_parent_session_when_the_roster_had_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let requests_dir = state.dash().join("aaaa1111-token").join("requests");
        std::fs::create_dir_all(&requests_dir).expect("mkdir requests");

        let candidate = restore_pane("dddd4444", "44444444-2222-4333-8444-555555555555");
        assert_eq!(candidate.parent_session, None, "sanity: no recorded parent");
        let cfg = CtxConfig::default();
        let mut errors = ErrorLog::default();

        let (turn_env, _pane_channel) =
            restored_pane_turn_env(&cfg, &state, &repo, &candidate, &requests_dir, &mut errors);

        assert!(
            !turn_env
                .iter()
                .any(|(k, _)| k == super::super::agent::PARENT_SESSION_ENV),
            "a roster entry with no recorded parent must not fabricate one: {turn_env:?}"
        );
    }

    /// The other half of issue #160 finding 1: a worker pane that was
    /// spawned `Headless` (every file-dropped spawn request --
    /// `FILE_DROP_TRUSTED_INTERACTIVE` -- is always `Headless`, regardless
    /// of what a forged `SpawnRequest.interactive` claims) must NOT gain the
    /// interactive pin just by surviving a dashboard quit+restore cycle.
    /// Before this fix `spawn_restored_pane` unconditionally pinned
    /// `LaunchMode::Interactive`, which would have handed every ordinary
    /// delegated worker an interactive posture it was explicitly refused at
    /// spawn time.
    #[test]
    fn restored_pane_turn_env_restores_without_the_pin_for_a_non_interactive_worker_pane() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let requests_dir = state.dash().join("aaaa1111-token").join("requests");
        std::fs::create_dir_all(&requests_dir).expect("mkdir requests");

        // `restore_pane` leaves `interactive` at its `Default` (`false`) --
        // an ordinary delegated worker pane, never trusted-interactive.
        let candidate = restore_pane("dddd4444", "44444444-2222-4333-8444-555555555555");
        assert!(
            !candidate.interactive,
            "sanity: the fixture is non-interactive"
        );
        let cfg = CtxConfig::default();
        let mut errors = ErrorLog::default();

        let (turn_env, pane_channel) =
            restored_pane_turn_env(&cfg, &state, &repo, &candidate, &requests_dir, &mut errors);

        assert!(
            !turn_env
                .iter()
                .any(|(k, _)| k == super::super::adapters::LAUNCH_MODE_ENV),
            "a worker pane that was never interactive-pinned must not gain the pin on \
             restore: {turn_env:?}"
        );
        assert!(
            turn_env.contains(&(
                spawnreq::DASH_REQUESTS_ENV.to_string(),
                pane_channel.display().to_string()
            )),
            "the fresh spawn-request channel is still pushed regardless of launch mode: \
             {turn_env:?}"
        );
    }

    /// H3: a restore candidate whose spawn fails must not simply vanish. It
    /// was already taken out of the on-disk roster by `roster::take_roster`
    /// before this launch ever tried to spawn it, so if `spawn_restored_pane`
    /// only reports an error and does not push the candidate into
    /// `deferred_restore`, `on_quit` never sees it again and the session is
    /// lost for good -- the same failure mode G3 fixed for cap-skipped
    /// candidates, but for spawn-failed ones instead.
    ///
    /// Forces the failure through `adapters::select` (an agent name the
    /// permissive test `CtxConfig` does not recognise) rather than a real
    /// `Pane::spawn` failure, since that is the deterministic, no-process
    /// path through the same function -- both of `spawn_restored_pane`'s
    /// error arms push into `deferred_restore` identically.
    #[test]
    fn spawn_restored_pane_writes_a_failed_candidate_back_for_next_launch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let requests_dir = state.dash().join("aaaa1111-token").join("requests");
        std::fs::create_dir_all(&requests_dir).expect("mkdir requests");

        let mut candidate = restore_pane("cccc3333", "33333333-2222-4333-8444-555555555555");
        candidate.agent = "not-a-real-agent".to_string();
        let cfg = CtxConfig::default();

        let mut panes = Vec::new();
        let mut nudge_queues = Vec::new();
        let mut errors = ErrorLog::default();
        let mut deferred_restore = Vec::new();

        spawn_restored_pane(
            &candidate,
            &mut panes,
            &mut nudge_queues,
            &cfg,
            &state,
            &repo,
            (80, 24),
            &requests_dir,
            &mut errors,
            &mut deferred_restore,
        );

        assert!(panes.is_empty(), "the failed candidate spawned no pane");
        assert!(
            errors.iter().any(|e| e.contains("cccc3333")),
            "the operator is told the restore failed: {errors:?}"
        );
        assert_eq!(
            deferred_restore,
            vec![candidate],
            "the failed candidate is carried forward for the next launch's roster"
        );

        // And it actually round-trips through `on_quit`, same as the
        // cap-skipped case above.
        on_quit(&panes, &[], &deferred_restore, &requests_dir, &state, &repo);
        let slug = super::super::state::repo_slug(&repo);
        let written = roster::take_roster(&state, &slug, super::super::state::now_secs(), 999_999)
            .expect("a roster is still written");
        assert_eq!(
            written.panes, deferred_restore,
            "the spawn-failed candidate is offered again next launch"
        );
    }

    /// F3 (review, PR #116): a successfully restored worker pane gets its
    /// `report_to`/`report_reminder_sent` back from the roster entry that
    /// named them -- before this fix the roster carried no such fields at
    /// all, so `spawn_restored_pane` never set `report_to` on the pane it
    /// spawned and a restored worker's requester silently lost its
    /// completion reminder for good.
    ///
    /// The candidate's own argv is deliberately not a real agent (`ping`
    /// with extra positional args it will reject and exit on almost
    /// immediately) -- only the pty spawn itself has to succeed here, the
    /// same ABSOLUTE rule every other test in this module already follows.
    #[test]
    fn spawn_restored_pane_restores_report_to_and_reminder_sent_from_the_roster() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let requests_dir = state.dash().join("aaaa1111-token").join("requests");
        std::fs::create_dir_all(&requests_dir).expect("mkdir requests");

        let mut candidate = restore_pane("cccc3333", "33333333-2222-4333-8444-555555555555");
        candidate.report_to = Some("aaaa1111".to_string());
        candidate.report_reminder_sent = true;
        candidate.settled_mail_sent = true;
        let cfg = CtxConfig {
            #[cfg(windows)]
            agent_bin: Some("ping -n 3 127.0.0.1".to_string()),
            #[cfg(unix)]
            agent_bin: Some("sleep 3".to_string()),
            ..Default::default()
        };

        let mut panes = Vec::new();
        let mut nudge_queues = Vec::new();
        let mut errors = ErrorLog::default();
        let mut deferred_restore = Vec::new();

        spawn_restored_pane(
            &candidate,
            &mut panes,
            &mut nudge_queues,
            &cfg,
            &state,
            &repo,
            (80, 24),
            &requests_dir,
            &mut errors,
            &mut deferred_restore,
        );

        assert!(
            errors.is_empty(),
            "a trivially spawnable program must restore cleanly: {errors:?}"
        );
        assert_eq!(panes.len(), 1, "the candidate spawned exactly one pane");
        assert_eq!(
            panes[0].report_to(),
            Some("aaaa1111"),
            "the roster's report_to must reach the restored pane"
        );
        assert!(
            panes[0].report_reminder_sent(),
            "a restore resurrects the SAME logical session, so an \
             already-reminded worker must not be reminded again"
        );

        assert!(
            panes[0].settled_mail_sent,
            "the restored session must not send a second settled report"
        );

        on_quit(&panes, &[], &[], &requests_dir, &state, &repo);
        let saved = roster::take_roster(
            &state,
            &super::super::state::repo_slug(&repo),
            super::super::state::now_secs(),
            999_999,
        )
        .expect("saved roster");
        assert!(saved.panes[0].report_reminder_sent);
        assert!(saved.panes[0].settled_mail_sent);

        panes[0].finish_shutdown().expect("shutdown");
    }

    /// Security review Finding 6 (2026-08-28): a restore used to hardcode
    /// `role: Worker` and push no group binding at all, so a coordinator pane
    /// came back from a dashboard restart demoted (refused its own onward
    /// delegation by the depth cap, and no longer able to close the group it
    /// still owned) and outside the batch it was launched under. Round trip
    /// here: quit snapshot -> roster -> restore.
    #[cfg(unix)]
    #[test]
    fn a_coordinator_pane_survives_a_snapshot_and_restore_as_a_coordinator() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let requests_dir = state.dash().join("aaaa1111-token").join("requests");
        std::fs::create_dir_all(&requests_dir).expect("mkdir requests");

        let session_id = "77777777-2222-4333-8444-555555555555";
        let mut pane = Pane::spawn(
            PaneSpec {
                // A real adapter NAME (the restore path resolves it again),
                // never a real agent binary -- `cfg.agent_bin` below is what
                // the restored pane actually launches.
                agent_name: "claude".to_string(),
                argv: trivial_argv(),
                role: prompt::PromptRole::SubOrchestrator,
                verb: sessions::Verb::Dash,
                session_id: session_id.to_string(),
                title: "sub codex".to_string(),
            },
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            pane::DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");
        pane.set_work_group_id(Some("wg-1".to_string()));
        let panes = vec![pane];

        on_quit(&panes, &[], &[], &requests_dir, &state, &repo);
        let slug = super::super::state::repo_slug(&repo);
        let written = roster::take_roster(&state, &slug, super::super::state::now_secs(), 999_999)
            .expect("a roster is written");
        assert_eq!(written.panes.len(), 1);
        assert_eq!(
            written.panes[0].role,
            prompt::PromptRole::SubOrchestrator.label(),
            "the quit snapshot records the role the pane was spawned with"
        );
        assert_eq!(
            written.panes[0].work_group_id.as_deref(),
            Some("wg-1"),
            "and the group it belongs to"
        );
        assert!(
            !restorable_candidates(written.clone()).is_empty(),
            "a coordinator is still offered for restore -- only the orchestrator seat is filtered"
        );

        let cfg = CtxConfig {
            agent_bin: Some("sleep 3".to_string()),
            ..Default::default()
        };
        let mut restored = Vec::new();
        let mut nudge_queues = Vec::new();
        let mut errors = ErrorLog::default();
        let mut deferred_restore = Vec::new();
        spawn_restored_pane(
            &written.panes[0],
            &mut restored,
            &mut nudge_queues,
            &cfg,
            &state,
            &repo,
            (80, 24),
            &requests_dir,
            &mut errors,
            &mut deferred_restore,
        );

        assert_eq!(restored.len(), 1, "the candidate restored: {errors:?}");
        assert_eq!(
            restored[0].role(),
            prompt::PromptRole::SubOrchestrator,
            "a restored coordinator is still a coordinator"
        );
        assert_eq!(
            restored[0].work_group_id(),
            Some("wg-1"),
            "and is still bound to its own group"
        );

        for pane in &mut restored {
            let _ = pane.finish_shutdown();
        }
    }

    /// Fix 4 (issue #249/#250 review): the full quit -> roster -> restore
    /// round trip preserves a worker pane's own parent lineage. Mirrors
    /// `a_coordinator_pane_survives_a_snapshot_and_restore_as_a_coordinator`'s
    /// own recipe for `work_group_id`, but for `Pane::parent_session`.
    #[cfg(unix)]
    #[test]
    fn a_worker_panes_parent_session_survives_a_snapshot_and_restore_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let requests_dir = state.dash().join("aaaa1111-token").join("requests");
        std::fs::create_dir_all(&requests_dir).expect("mkdir requests");

        let session_id = "99999999-2222-4333-8444-555555555555";
        let mut pane = Pane::spawn(
            PaneSpec {
                agent_name: "claude".to_string(),
                argv: trivial_argv(),
                role: prompt::PromptRole::Worker,
                verb: sessions::Verb::Dash,
                session_id: session_id.to_string(),
                title: "wrk claude".to_string(),
            },
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            pane::DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");
        pane.set_parent_session(Some("orch0001".to_string()));
        let panes = vec![pane];

        on_quit(&panes, &[], &[], &requests_dir, &state, &repo);
        let slug = super::super::state::repo_slug(&repo);
        let written = roster::take_roster(&state, &slug, super::super::state::now_secs(), 999_999)
            .expect("a roster is written");
        assert_eq!(written.panes.len(), 1);
        assert_eq!(
            written.panes[0].parent_session.as_deref(),
            Some("orch0001"),
            "the quit snapshot records the pane's own parent session"
        );

        let cfg = CtxConfig {
            agent_bin: Some("sleep 3".to_string()),
            ..Default::default()
        };
        let mut restored = Vec::new();
        let mut nudge_queues = Vec::new();
        let mut errors = ErrorLog::default();
        let mut deferred_restore = Vec::new();
        spawn_restored_pane(
            &written.panes[0],
            &mut restored,
            &mut nudge_queues,
            &cfg,
            &state,
            &repo,
            (80, 24),
            &requests_dir,
            &mut errors,
            &mut deferred_restore,
        );

        assert_eq!(restored.len(), 1, "the candidate restored: {errors:?}");
        assert_eq!(
            restored[0].parent_session(),
            Some("orch0001"),
            "a restored worker pane's own parent lineage must survive the round trip"
        );

        for pane in &mut restored {
            let _ = pane.finish_shutdown();
        }
    }

    /// A row action with no chord of its own comes back as a menu action --
    /// the same effect the context menu produces for the same entry.
    #[test]
    fn a_chordless_palette_row_runs_through_the_context_menu_path() {
        let view = typed(
            open_palette(ui::PaletteMode::Run),
            "give this row the keyboard",
        );
        let (_, effect) = palette_overlay_reduce(view, key(KeyCode::Enter, KeyModifiers::NONE));
        let descriptor = actions::descriptor(effect.expect("an effect").0).expect("descriptor");
        assert_eq!(descriptor.dash_action(), None);
        assert_eq!(descriptor.menu, Some(ui::MenuAction::Focus));
    }

    /// A disabled row is inert: Enter on it closes the palette with nothing
    /// to run, exactly as a disabled context-menu entry does nothing.
    #[test]
    fn enter_on_a_disabled_palette_row_runs_nothing() {
        let view = typed(open_palette(ui::PaletteMode::Run), "relaunch an ended row");
        assert!(
            view.rows().iter().any(|r| matches!(
                r,
                actions::PaletteRow::Action {
                    disabled: Some(_),
                    ..
                }
            )),
            "a live row cannot be restored, so the row must be disabled"
        );
        assert_eq!(view.activated(), None);
        let (next, effect) = palette_overlay_reduce(view, key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(next.is_none());
        assert!(effect.is_none());
    }

    /// Esc closes with nothing run. Focus is never touched by any overlay --
    /// the palette owns no pane index at all, which is what makes "returns
    /// focus to the previously focused pane" structurally true.
    #[test]
    fn esc_closes_the_palette_without_running_anything() {
        let view = typed(open_palette(ui::PaletteMode::Run), "quit");
        let (next, effect) = palette_overlay_reduce(view, key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(next.is_none());
        assert!(effect.is_none());
    }

    /// Backspace edits the query, and a query that no longer matches the
    /// caret's row moves the caret rather than leaving it off the list.
    #[test]
    fn backspace_edits_the_query_and_keeps_the_caret_on_a_real_row() {
        let mut view = typed(open_palette(ui::PaletteMode::Run), "spawn");
        assert_eq!(view.query, "spawn");
        for _ in 0..3 {
            let (next, _) =
                palette_overlay_reduce(view, key(KeyCode::Backspace, KeyModifiers::NONE));
            view = next.expect("backspace never closes the palette");
        }
        assert_eq!(view.query, "sp");
        let rows = view.rows();
        assert!(rows[view.cursor].selectable(), "{rows:?}");
    }

    /// Up/Down walk only real rows, never the section headings an empty
    /// query draws.
    #[test]
    fn the_palette_caret_never_lands_on_a_section_heading() {
        let mut view = open_palette(ui::PaletteMode::Run);
        for _ in 0..40 {
            let (next, _) = palette_overlay_reduce(view, key(KeyCode::Down, KeyModifiers::NONE));
            view = next.expect("still open");
            assert!(view.rows()[view.cursor].selectable());
        }
        for _ in 0..60 {
            let (next, _) = palette_overlay_reduce(view, key(KeyCode::Up, KeyModifiers::NONE));
            view = next.expect("still open");
            assert!(view.rows()[view.cursor].selectable());
        }
    }

    /// Deliverable D: the Esc/Enter matrix, one assertion pair per overlay.
    ///
    /// Esc always closes the topmost layer (or cancels an inline confirm or
    /// compose buffer); Enter always confirms or activates. Deliberately
    /// left as they are, and asserted here as such: mail/memory compose
    /// buffers and the menu's inline stop confirmation, where Esc cancels
    /// that inner layer rather than the whole dialog, and Restore, whose Esc
    /// is labelled `skip` because closing it IS skipping the restore.
    #[test]
    fn every_overlay_closes_on_esc_and_confirms_on_enter() {
        let esc = key(KeyCode::Esc, KeyModifiers::NONE);
        let enter = key(KeyCode::Enter, KeyModifiers::NONE);

        // QuitConfirm.
        assert!(quit_confirm_reduce(vec!["w".into()], esc).0.is_none());
        assert_eq!(
            quit_confirm_reduce(vec!["w".into()], enter).1,
            Some(QuitConfirmEffect::Confirm)
        );

        // Spawn.
        let draft = ui::SpawnDraft {
            input: "claude do the thing".into(),
            items: Vec::new(),
            cursor: 0,
        };
        assert!(spawn_overlay_reduce(draft.clone(), esc).0.is_none());
        assert!(matches!(
            spawn_overlay_reduce(draft, enter).1,
            Some(SpawnEffect::Submit { .. })
        ));

        // Nudge.
        let nudge = ui::NudgeDraft {
            target: ui::NudgeTarget::AttachedPane("aaaa1111".into()),
            input: "go".into(),
        };
        assert!(nudge_overlay_reduce(nudge.clone(), esc).0.is_none());
        assert!(nudge_overlay_reduce(nudge, enter).1.is_some());

        // Mail: browsing, then its compose buffer (Esc cancels the buffer,
        // not the overlay -- deliberate, and asserted).
        let mail = ui::MailView {
            items: vec![(PathBuf::from("/mail/1.md"), "claude".into(), "body".into())],
            cursor: 0,
            offset: 0,
            compose: None,
        };
        assert!(mail_overlay_reduce(mail.clone(), esc).0.is_none());
        assert!(matches!(
            mail_overlay_reduce(mail.clone(), enter).1,
            Some(ui::MailEffect::Consume(_))
        ));
        let composing = ui::MailView {
            compose: Some(ui::ComposeDraft {
                to: "any".into(),
                body: "hi".into(),
            }),
            ..mail
        };
        let (back, _) = mail_overlay_reduce(composing.clone(), esc);
        assert!(
            back.is_some_and(|v| v.compose.is_none()),
            "Esc cancels the compose buffer, not the whole dialog"
        );
        assert!(matches!(
            mail_overlay_reduce(composing, enter).1,
            Some(ui::MailEffect::Send(_))
        ));

        // Memory: the same shape, including its edit buffer.
        let memory = ui::MemoryView {
            entries: vec![("k".into(), "1m".into(), "body".into())],
            cursor: 0,
            offset: 0,
            input: None,
        };
        assert!(memory_overlay_reduce(memory.clone(), esc).0.is_none());
        let editing = ui::MemoryView {
            input: Some("new".into()),
            ..memory
        };
        let (back, _) = memory_overlay_reduce(editing.clone(), esc);
        assert!(back.is_some_and(|v| v.input.is_none()));
        assert!(matches!(
            memory_overlay_reduce(editing, enter).1,
            Some(ui::MemoryEffect::Remember { .. })
        ));

        // Restore: Esc closes with no effect (that is what `skip` means),
        // Enter confirms whatever is checked.
        let restore = ui::RestoreView {
            entries: vec![ui::RestoreEntry {
                label: "w".into(),
                checked: true,
            }],
            cursor: 0,
            offset: 0,
        };
        let (next, effect) = restore_overlay_reduce(restore.clone(), esc);
        assert!(next.is_none() && effect.is_none());
        assert_eq!(
            restore_overlay_reduce(restore, enter).1,
            Some(RestoreEffect::Confirm(vec![0]))
        );

        // Handover.
        let handover = ui::HandoverDraft {
            items: vec![("claude".into(), "worker".into(), "sonnet".into())],
            cursor: 0,
            offset: 0,
            target_short: "aaaa1111".into(),
        };
        assert!(handover_overlay_reduce(handover.clone(), esc).0.is_none());
        assert!(handover_overlay_reduce(handover, enter).1.is_some());

        // Errors: read-only, so Enter closes rather than doing nothing.
        let errors = ui::ErrorsView {
            items: vec![err_item("boom")],
            cursor: 0,
            offset: 0,
            mark: 0,
        };
        assert!(errors_overlay_reduce(errors.clone(), esc).0.is_none());
        assert!(errors_overlay_reduce(errors, enter).0.is_none());

        // Inspector: likewise.
        let inspector = ui::InspectorView {
            target: "aaaa1111".into(),
            subject: "aaaa1111 \u{b7} worker".into(),
            sections: vec![ui::InspectorSection {
                name: "identity".into(),
                lines: vec!["short  aaaa1111".into()],
            }],
            cursor: 0,
            offset: 0,
        };
        assert!(inspector_overlay_reduce(inspector.clone(), esc).is_none());
        assert!(inspector_overlay_reduce(inspector, enter).is_none());

        // Menu: Esc backs out, Enter activates -- and Esc on the inline stop
        // confirmation cancels only that confirmation.
        let menu = ui::MenuView {
            target: "aaaa1111".into(),
            subject: "aaaa1111 \u{b7} worker".into(),
            entries: vec![
                ui::MenuEntry {
                    action: ui::MenuAction::Inspect,
                    disabled: None,
                    letter: Some('i'),
                },
                ui::MenuEntry {
                    action: ui::MenuAction::Stop,
                    disabled: None,
                    letter: Some('s'),
                },
            ],
            cursor: 0,
            offset: 0,
            confirm: None,
        };
        assert!(menu_overlay_reduce(menu.clone(), esc).0.is_none());
        assert_eq!(
            menu_overlay_reduce(menu.clone(), enter).1,
            Some(MenuEffect {
                target: "aaaa1111".into(),
                action: ui::MenuAction::Inspect,
            })
        );
        let armed = ui::MenuView {
            confirm: Some(1),
            cursor: 1,
            ..menu
        };
        let (back, effect) = menu_overlay_reduce(armed, esc);
        assert!(
            back.is_some_and(|v| v.confirm.is_none()) && effect.is_none(),
            "Esc cancels the inline stop confirmation, not the menu"
        );

        // Palette and help.
        assert!(
            palette_overlay_reduce(open_palette(ui::PaletteMode::Run), esc)
                .0
                .is_none()
        );
        assert!(
            palette_overlay_reduce(open_palette(ui::PaletteMode::Run), enter)
                .1
                .is_some()
        );
        let (next, effect) = palette_overlay_reduce(open_palette(ui::PaletteMode::Help), enter);
        assert!(
            next.is_none() && effect.is_none(),
            "help confirms by closing, and never runs anything"
        );
    }
}
