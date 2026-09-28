//! Budget enforcement and reaping ended panes out of the dashboard.
use super::*;

/// Pure: every session short id the sidebar can draw a glyph for, deduped and
/// in row order -- this dashboard's own panes, its retained ended rows, and
/// every live registry session it owns. Exactly the rows `assemble_sidebar`
/// builds, so no status is ever read for a row that will not be drawn.
pub(super) fn attention_row_shorts(
    pane_shorts: &[String],
    retained: &VecDeque<EndedRow>,
    registry: &[(sessions::Record, sessions::Liveness)],
    dashboard_pid: u32,
) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut shorts = Vec::new();
    let push = |short: &str, seen: &mut HashSet<String>, shorts: &mut Vec<String>| {
        if seen.insert(short.to_string()) {
            shorts.push(short.to_string());
        }
    };
    for short in pane_shorts {
        push(short, &mut seen, &mut shorts);
    }
    for row in retained {
        push(&row.short, &mut seen, &mut shorts);
    }
    for (record, liveness) in registry {
        if *liveness == sessions::Liveness::Live && record.owner_pid == Some(dashboard_pid) {
            push(&record.short, &mut seen, &mut shorts);
        }
    }
    shorts
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GroupFold {
    Collapse,
    Expand,
    Toggle,
}

/// Pure: folds a work group shut or open again, returning whether it is
/// collapsed afterwards.
///
/// The single reducer both `^A Left`/`^A Right` and a click on a group
/// header's disclosure triangle go through, so the keyboard and the pointer
/// can never disagree about what "collapsed" means. Collapsing never touches
/// focus -- the group's children stop being drawn, but whichever pane owns the
/// keyboard keeps it.
pub(super) fn fold_group(collapsed: &mut HashSet<String>, id: &str, fold: GroupFold) -> bool {
    match fold {
        GroupFold::Collapse => {
            collapsed.insert(id.to_string());
            true
        }
        GroupFold::Expand => {
            collapsed.remove(id);
            false
        }
        GroupFold::Toggle => {
            if collapsed.remove(id) {
                false
            } else {
                collapsed.insert(id.to_string());
                true
            }
        }
    }
}

/// Pure: which work group `^A Left`/`^A Right` acts on -- the one the cursor
/// is parked on when it sits on a group header, else the one the selected
/// session belongs to. `None` on a flat (ungrouped) row or the summary line,
/// which is what makes both keys no-ops there rather than folding whichever
/// group happened to be nearby.
pub(super) fn group_under_cursor(
    chrome: Option<&Hit>,
    rows: &[ui::SidebarRow],
    selected: usize,
) -> Option<String> {
    match chrome {
        Some(Hit::GroupToggle(id)) => Some(id.clone()),
        Some(_) => None,
        None => rows
            .get(selected)
            .and_then(|row| row.group.as_ref())
            .map(|group| group.id.clone()),
    }
}

/// Latch done-unread until a completed, unobscured frame shows the focused pane (#354).
#[derive(Debug, Default)]
pub(super) struct DoneUnreadAck {
    /// The `(short, revision)` a qualifying render observed, waiting for the
    /// next tick to act on.
    pending: Option<(String, u64)>,
    /// Every `(short, revision)` already acknowledged. Keyed by revision, so a
    /// session that settles again later latches `Unseen` again at a NEW
    /// revision and is acknowledged again -- while the same revision is never
    /// written twice, however many frames it survives.
    acked: HashSet<(String, u64)>,
}

impl DoneUnreadAck {
    pub(super) fn observe(&mut self, candidate: Option<(String, u64)>) {
        self.pending = candidate.filter(|key| !self.acked.contains(key));
    }

    /// Queue one acknowledgement per revision after visibility is earned;
    /// rendering must not write attention state itself.
    pub(super) fn acknowledge(&mut self, candidate: Option<(String, u64)>) -> Option<String> {
        let (short, revision) = candidate?;
        self.acked
            .insert((short.clone(), revision))
            .then_some(short)
    }

    /// Take each due acknowledgement at most once per session revision.
    pub(super) fn take_due(&mut self) -> Option<String> {
        let (short, revision) = self.pending.take()?;
        self.acked.insert((short.clone(), revision));
        Some(short)
    }
}

/// Pure: the `(short, revision)` one completed, unoccluded render of the
/// focused pane qualifies for acknowledgement -- `None` for every render that
/// does not.
///
/// `focused` is `(short, scrollback)` for the pane the frame actually drew;
/// `status` is that pane's cached status. Note what is deliberately absent:
/// there is no path here from *selection*. A row the cursor merely walked onto
/// is not a row anybody read.
pub(super) fn ack_candidate(
    overlay_open: bool,
    focused: Option<(&str, usize)>,
    status: Option<&super::attention::SessionStatus>,
) -> Option<(String, u64)> {
    if overlay_open {
        return None;
    }
    let (short, scrollback) = focused?;
    if scrollback != 0 {
        return None;
    }
    let status = status?;
    if super::attention::project(status) != super::attention::Projection::DoneUnread {
        return None;
    }
    Some((short.to_string(), status.revision))
}

/// Use the glyph actually drawn for the row when deciding whether its inspector acknowledges it.
pub(super) fn inspect_ack_candidate(row: &ui::SidebarRow) -> Option<(String, u64)> {
    let status = row.status.as_ref()?;
    if row.exit_code.is_none() && row.attached {
        return None;
    }
    if ui::glyph_for(row) != ui::Glyph::DoneUnread {
        return None;
    }
    Some((row.short.clone(), status.revision))
}

/// Shift indices after removal; focus falls back to the first pane if its pane was reaped.
pub(super) fn reap_fixup(removed: usize, focused: usize, selected: usize) -> (usize, usize) {
    let focused = match focused.cmp(&removed) {
        std::cmp::Ordering::Greater => focused - 1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Less => focused,
    };
    let selected = if selected > removed {
        selected - 1
    } else {
        selected
    };
    (focused, selected)
}

/// A failed pane launch includes the last visible output line. Reaping
/// removes the pane, so the header receives this message only once.
pub(super) fn early_pane_failure(
    agent: &str,
    short: &str,
    code: i32,
    elapsed: Duration,
    tail: &str,
) -> Option<String> {
    if code == 0 || elapsed > Duration::from_secs(10) {
        return None;
    }
    let tail: String = tail.trim().chars().take(160).collect();
    Some(format!(
        "{agent} pane {short} exited with code {code} {}s after launch: {tail}",
        elapsed.as_secs()
    ))
}

/// Resolve transcripts from the pane's own cwd because adapters key them by launch directory.
pub(super) fn pane_transcript_usage(
    pane: &Pane,
    cfg: &CtxConfig,
) -> Option<super::super::event::TranscriptUsage> {
    let adapter = adapters::select(Some(pane.agent()), &[], cfg).ok()?;
    let transcript = adapter.transcript_path(&SessionRef {
        id: SessionId::parse(pane.session_id()),
        cwd: pane.cwd().to_path_buf(),
    });
    let body = std::fs::read_to_string(transcript).ok()?;
    adapter.transcript_usage(&body)
}

pub(super) fn enforce_pane_token_budgets(
    panes: &mut [Pane],
    cfg: &CtxConfig,
    errors: &mut ErrorLog,
    last_sweep: &mut Instant,
    now: Instant,
) {
    enforce_pane_token_budgets_with(panes, cfg, errors, last_sweep, now, |pane| {
        pane_transcript_usage(pane, cfg)
    });
}

/// Throttle transcript usage reads with other disk work; each read parses the whole pane transcript.
pub(super) fn enforce_pane_token_budgets_with<F>(
    panes: &mut [Pane],
    cfg: &CtxConfig,
    errors: &mut ErrorLog,
    last_sweep: &mut Instant,
    now: Instant,
    mut usage_of: F,
) where
    F: FnMut(&Pane) -> Option<super::super::event::TranscriptUsage>,
{
    if !due(*last_sweep, now, FACTS_THROTTLE) {
        return;
    }
    *last_sweep = now;
    for pane in panes {
        if pane.budget_tokens().is_none() {
            continue;
        }
        let Some(usage) = usage_of(pane) else {
            continue;
        };
        let quit_sequence = adapters::select(Some(pane.agent()), &[], cfg)
            .map(|adapter| adapter.quit_sequence().to_string())
            .unwrap_or_default();
        match pane.enforce_token_budget(&usage, &quit_sequence) {
            Ok(Some(PaneBudgetNotice::SoftWarn { used, limit })) => push_error(
                errors,
                format!(
                    "pane '{}' ({}) has spent {used}/{limit} tokens; wrap up and checkpoint soon",
                    pane.title(),
                    pane.short()
                ),
            ),
            Ok(Some(PaneBudgetNotice::HardStop { used, limit })) => push_error(
                errors,
                format!(
                    "pane '{}' ({}) token budget exhausted ({used}/{limit}); stopped with exit {}",
                    pane.title(),
                    pane.short(),
                    super::exec::EXIT_BUDGET_EXHAUSTED
                ),
            ),
            Ok(None) => {}
            Err(e) => push_error(
                errors,
                format!("pane '{}' budget enforcement failed: {e}", pane.short()),
            ),
        }
    }
}

/// Apply wall-clock timeout to dashboard worker panes as well as headless delegations.
pub(super) fn enforce_pane_deadlines(
    panes: &mut [Pane],
    cfg: &CtxConfig,
    errors: &mut ErrorLog,
    last_sweep: &mut Instant,
    now: Instant,
) {
    if !due(*last_sweep, now, FACTS_THROTTLE) {
        return;
    }
    *last_sweep = now;
    for pane in panes {
        if pane.deadline().is_none() {
            continue;
        }
        let quit_sequence = adapters::select(Some(pane.agent()), &[], cfg)
            .map(|adapter| adapter.quit_sequence().to_string())
            .unwrap_or_default();
        match pane.enforce_deadline(now, &quit_sequence) {
            Ok(true) => push_error(
                errors,
                format!(
                    "pane '{}' ({}) outran its --timeout-secs; stopped with exit {}",
                    pane.title(),
                    pane.short(),
                    super::exec::EXIT_TIMEOUT
                ),
            ),
            Ok(false) => {}
            Err(e) => push_error(
                errors,
                format!("pane '{}' timeout enforcement failed: {e}", pane.short()),
            ),
        }
    }
}

/// Settle reservations and write the delegation row at reap, when actual spend
/// is known; the requester can return while the pane is still running.
pub(super) fn account_reaped_pane_spend(
    pane: &Pane,
    cfg: &CtxConfig,
    state: &StateDir,
    exit_code: i32,
) {
    let usage = pane_transcript_usage(pane, cfg).unwrap_or_default();
    let exit_code = super::agent::recorded_contract_exit(state, pane.short(), exit_code);
    if let Some(facts) = pane.delegation() {
        let _ = super::log::append_delegation(
            state,
            &super::log::Delegation {
                ts: super::state::now_secs(),
                session: pane.session_id(),
                parent_session: &facts.requester,
                work_group_id: pane.work_group_id(),
                agent: pane.agent(),
                model: pane.launch_model(),
                input_tokens: usage.input_tokens,
                cache_creation_input_tokens: usage.cache_creation_input_tokens,
                cache_read_input_tokens: usage.cache_read_input_tokens,
                output_tokens: usage.output_tokens,
                wall_ms: u64::try_from(facts.started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                exit_code,
                outcome: super::agent::delegation_outcome(exit_code),
                mode: Some(facts.mode),
                // A `SpawnRequest` carries no `--task-class`; the field is
                // honestly unknown for a pane rather than guessed at.
                task_class: None,
                principal: &facts.principal,
                envelope_sha256: facts.envelope_sha256.as_deref(),
            },
        );
    }
    let actual = super::agent::token_spend(&usage);
    if let Some(group_id) = pane.work_group_id() {
        // Settle the exact token ceiling reserved for this pane at admission (#301).
        let reserved = pane.budget_tokens().unwrap_or(0);
        let _ = super::group::settle_reservation(state, group_id, reserved, actual);
    }
    // Settle the provider reservation taken for this pane, with or without a work group (#358).
    if let Some(reservation_id) = pane.reservation_id() {
        // Must match `fulfill_spawn_request`'s own reserve exactly -- see
        // that function's Track C (#383) note for why this stays name-only.
        let provider = adapters::provider_for_agent_name(Some(pane.agent()));
        let _ = super::reservation::settle(state, provider, reservation_id, actual);
    }
}

/// Shift selected view-only row indices when panes are inserted, preserving the same logical target.
pub(super) fn insert_fixup(old_pane_count: usize, new_pane_count: usize, selected: usize) -> usize {
    let added = new_pane_count.saturating_sub(old_pane_count);
    if selected >= old_pane_count {
        selected + added
    } else {
        selected
    }
}

/// Adjust selection after restore grows live panes and removes a retained row.
pub(super) fn restore_fixup(
    old_pane_count: usize,
    new_pane_count: usize,
    restored_row: usize,
    selected: usize,
) -> usize {
    if selected == restored_row {
        return new_pane_count.saturating_sub(1);
    }
    let shifted = insert_fixup(old_pane_count, new_pane_count, selected);
    if selected > restored_row {
        shifted.saturating_sub(1)
    } else {
        shifted
    }
}

/// Keep a retained ended row available after its pane is reaped so unread output can be inspected (#209).
pub(super) struct LastExited {
    pub(super) harness: String,
    pub(super) exited_at: Instant,
}

/// Finish shutdown and release registry, permit and socket before retaining the ended row.
#[allow(clippy::too_many_arguments)]
pub(super) fn reap_ended_panes(
    panes: &mut Vec<Pane>,
    queues: &mut Vec<VecDeque<String>>,
    cfg: &CtxConfig,
    state: &StateDir,
    repo: &Path,
    focused: &mut usize,
    selected: &mut usize,
    errors: &mut ErrorLog,
    reaped_codes: &mut Vec<i32>,
    reaped_recent: &mut HashSet<String>,
    last_exited: &mut Option<LastExited>,
    retained: &mut VecDeque<EndedRow>,
    kept_requests: &mut HashMap<String, (spawnreq::SpawnRequest, Option<String>)>,
) -> Vec<String> {
    // Route clean completion to a transient notice; reserve the sticky error ring for failures.
    let mut confirmations = Vec::new();
    let mut index = 0;
    while index < panes.len() {
        let PaneState::Ended(code) = panes[index].state() else {
            index += 1;
            continue;
        };
        // Keep an exited pane until its reader channel drains, since the shared vt100 budget may defer output (#330).
        if panes[index].has_pending_output() {
            index += 1;
            continue;
        }
        if panes[index].delivery_sender.is_some() {
            let mut notices = Vec::new();
            report_unconfirmed_submission(
                &mut panes[index],
                state,
                cfg,
                errors,
                &mut notices,
                Instant::now(),
            );
            confirmations.extend(notices.into_iter().map(|notice| notice.text));
        }
        // Capture retained-row facts before shutdown releases the pane's registry record (#354).
        let now_secs = super::state::now_secs();
        let ended_meta = EndedMeta {
            exit_code: code,
            exited_at: now_secs,
            age_secs: sessions::load_record(state, panes[index].short())
                .map(|record| now_secs.saturating_sub(record.started_at)),
        };
        // Capture budget and writer disclosure from the live pane before it is dropped.
        let retained_budget = budget_text(
            panes[index]
                .measured_usage()
                .map(|u| u.context_total().saturating_add(u.output_tokens)),
            panes[index].budget_tokens(),
        );
        let retained_writer = writer_text(panes[index].holds_writer_permit(), panes[index].cwd());
        let early_failure = panes[index].exited_after().and_then(|elapsed| {
            early_pane_failure(
                panes[index].agent(),
                panes[index].short(),
                code,
                elapsed,
                &panes[index].last_line(),
            )
        });
        if let Err(e) = panes[index].finish_shutdown() {
            push_error(errors, format!("reap {}: {e}", panes[index].short()));
        }
        let short = panes[index].short().to_string();
        let (request, requested_by) = match kept_requests.remove(&short) {
            Some((req, by)) => (Some(req), by),
            None => (None, None),
        };
        let retained_row = EndedRow {
            role: panes[index].role().label().to_string(),
            model: panes[index].launch_model().map(str::to_string),
            harness: panes[index].agent().to_string(),
            group_id: panes[index].work_group_id().map(str::to_string),
            parent: panes[index].parent_session().map(str::to_string),
            budget: retained_budget,
            writer: retained_writer,
            cwd: panes[index].cwd().display().to_string(),
            request,
            requested_by,
            short,
            meta: ended_meta,
        };
        // Record the child's exit at supervisor authority before removing its pane (#349).
        let prior_lifecycle = super::attention::load(state, &retained_row.short).lifecycle;
        let tail = panes[index].screen_tail();
        for observation in reap_observations(prior_lifecycle, code, ended_meta.exited_at, &tail) {
            let _ = super::attention::record(
                state,
                &retained_row.short,
                observation,
                ended_meta.exited_at,
            );
        }
        report_settled_pane(&mut panes[index], state, cfg, errors);
        push_retained_ended(retained, retained_row, MAX_RETAINED_ENDED_ROWS);
        let pane = panes.remove(index);
        // Capture owned worktree identity before consuming the pane for shutdown.
        let pane_cwd = pane.cwd().to_path_buf();
        let pane_owns_cwd = pane.owns_cwd();
        let pane_short = pane.short().to_string();
        account_reaped_pane_spend(&pane, cfg, state, code);
        close_claimed_group(&pane, state);
        if index < queues.len() {
            queues.remove(index);
        }
        // Exclude a just-reaped session from the up-to-one-second stale registry view until refresh drops it.
        reaped_recent.insert(pane.short().to_string());
        reaped_codes.push(code);
        let ended_line = format!(
            "pane '{}' ({}) ended (exit {code})",
            pane.title(),
            pane.short()
        );
        if code == 0 {
            confirmations.push(ended_line);
        } else {
            push_error(errors, early_failure.unwrap_or(ended_line));
        }
        if panes.is_empty() {
            *last_exited = Some(LastExited {
                harness: pane.agent().to_string(),
                exited_at: Instant::now(),
            });
        }
        (*focused, *selected) = reap_fixup(index, *focused, *selected);
        // Reclaim worktrees for dashboard-hosted panes on reap; the headless path cannot reclaim them.
        if let Some(outcome) = reclaim_pane_worktree(
            state,
            repo,
            &pane_cwd,
            pane_owns_cwd,
            cfg.worktree.idle_pool_max,
        ) {
            push_error(
                errors,
                describe_pane_worktree_reclaim(&pane_short, &pane_cwd, outcome),
            );
        }
        // Deliberately no `index += 1`: the next pane has shifted into this
        // slot and has not been looked at yet.
    }
    confirmations
}

/// Reclaim only a workdir explicitly owned by this pane's request, never one merely under a worktree path.
pub(super) fn reclaim_pane_worktree(
    state: &StateDir,
    repo: &Path,
    cwd: &Path,
    owns_cwd: bool,
    idle_pool_max: u32,
) -> Option<super::agent::ReclaimOutcome> {
    if !owns_cwd || !super::agent::is_agent_managed_worktree(repo, cwd) {
        return None;
    }
    // Use the caller's configured idle-pool cap when reclaiming a worktree (#718).
    Some(super::agent::reclaim_worktree(
        state,
        repo,
        cwd,
        idle_pool_max,
    ))
}

/// One stderr-bound line describing [`reclaim_pane_worktree`]'s own outcome
/// for `pane_short`'s worktree at `path` -- routed through `push_error`
/// (the dashboard's own notice channel) rather than `eprintln!`, since a raw
/// stderr write would corrupt the alt-screen TUI `agent::run_with`'s own
/// headless equivalent (`reclaim_worktree_and_report`) never has to worry
/// about.
pub(super) fn describe_pane_worktree_reclaim(
    pane_short: &str,
    path: &Path,
    outcome: super::agent::ReclaimOutcome,
) -> String {
    match outcome {
        super::agent::ReclaimOutcome::Removed => format!(
            "pane '{pane_short}' worktree {} reclaimed (clean)",
            path.display()
        ),
        super::agent::ReclaimOutcome::Archived(dest) => format!(
            "pane '{pane_short}' worktree {} untracked content archived to {}, then reclaimed",
            path.display(),
            dest.display()
        ),
        super::agent::ReclaimOutcome::InspectionFailed { probe, note } => format!(
            "pane '{pane_short}' worktree {} left in place ({probe}: {note})",
            path.display()
        ),
        super::agent::ReclaimOutcome::Failed(reason) => format!(
            "pane '{pane_short}' worktree {} left in place ({reason})",
            path.display()
        ),
        super::agent::ReclaimOutcome::Idled => format!(
            "pane '{pane_short}' worktree {} idled; kept warm for the next `--worktree-reuse` \
             allocation with a matching base commit",
            path.display()
        ),
    }
}

/// Close a coordinator's work group when its child exits, regardless of exit code.
pub(super) fn close_claimed_group(pane: &Pane, state: &StateDir) {
    if !matches!(pane.role(), prompt::PromptRole::SubOrchestrator) {
        return;
    }
    let Some(group_id) = pane.work_group_id() else {
        return;
    };
    let Ok(Some(group)) = super::group::load(state, group_id) else {
        return;
    };
    if group.sub_orchestrator_session.as_deref() != Some(pane.short()) {
        return;
    }
    let _ = super::group::close(state, group_id, super::state::now_secs());
}

/// Snapshot live panes for roster before any shutdown releases their records.
pub(super) fn on_quit(
    panes: &[Pane],
    unoffered: &[roster::RosterPane],
    deferred_restore: &[roster::RosterPane],
    requests_dir: &Path,
    state: &StateDir,
    repo: &Path,
) {
    let live: Vec<roster::RosterPane> = panes
        .iter()
        // Skip already-ended pane candidates when writing the next restore roster.
        .filter(|pane| !matches!(pane.state(), PaneState::Ended(_)))
        .map(|pane| roster::RosterPane {
            agent: pane.agent().to_string(),
            session_id: pane.session_id().to_string(),
            // Persist each pane's actual role so restoration cannot demote a coordinator (#169).
            role: pane.role().label().to_string(),
            short: pane.short().to_string(),
            title: pane.title().to_string(),
            // Persist report target and one-shot reminder state across dashboard restart (#116).
            report_to: pane.report_to().map(str::to_string),
            report_reminder_sent: pane.report_reminder_sent(),
            settled_mail_sent: pane.settled_mail_sent,
            // Persist the work-group binding for restoration.
            work_group_id: pane.work_group_id().map(str::to_string),
            budget_tokens: pane.budget_tokens(),
            // Persist the actual launch mode so restore cannot grant interactive posture to a headless pane (#160).
            interactive: pane.launch_mode() == adapters::LaunchMode::Interactive,
            // Persist the server-verified parent session for restored steering (#249, #250).
            parent_session: pane.parent_session().map(str::to_string),
            // Persist pane kind so native sessions reattach through the runtime (#490).
            native: pane.is_native(),
            native_generation: pane.native().map(|native| native.generation()).unwrap_or(0),
        })
        .collect();
    let panes_for_roster = merge_unoffered(live, unoffered);
    let panes_for_roster = merge_unoffered(panes_for_roster, deferred_restore);
    let roster = roster::Roster {
        written: super::state::now_secs(),
        panes: panes_for_roster,
    };
    let slug = super::state::repo_slug(repo);
    let _ = roster::write_roster(state, &slug, &roster);

    remove_request_dir(requests_dir);
}

/// Carry live and skipped prior candidates into the new roster, deduplicated by session ID.
pub(super) fn merge_unoffered(
    mut live: Vec<roster::RosterPane>,
    unoffered: &[roster::RosterPane],
) -> Vec<roster::RosterPane> {
    for candidate in unoffered {
        if !live
            .iter()
            .any(|pane| pane.session_id == candidate.session_id)
        {
            live.push(candidate.clone());
        }
    }
    live
}

/// The restore candidates a quit still owes the next launch: everything, while
/// the startup restore dialog is still open and unanswered, and nothing once
/// the operator has answered it (`Enter` restored what they chose, `Esc` said
/// no). See `on_quit`'s own `unoffered` parameter (F5).
pub(super) fn unoffered_candidates<'a>(
    overlay: &ui::Overlay,
    candidates: &'a [roster::RosterPane],
) -> &'a [roster::RosterPane] {
    if matches!(overlay, ui::Overlay::Restore(_)) {
        candidates
    } else {
        &[]
    }
}

/// Remove the entire capability-token tree, including pane-specific request channels.
pub(super) fn remove_request_dir(requests_dir: &Path) {
    let dir = requests_dir.parent().unwrap_or(requests_dir);
    let _ = std::fs::remove_dir_all(dir);
}

/// Caps how many hot-path error strings the header keeps around: the header
/// is one line, so anything beyond the most recent handful is never going
/// to be shown anyway.
pub(super) const MAX_KEPT_ERRORS: usize = 5;

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;

    // -- done-unread acknowledgement --------------------------------------

    #[test]
    fn done_unread_is_acknowledged_only_by_an_unoccluded_render_of_the_focused_pane() {
        let status = done_unread_status(5);
        // The qualifying case: focused, no overlay, live scroll.
        assert_eq!(
            ack_candidate(false, Some(("aaa11111", 0)), Some(&status)),
            Some(("aaa11111".to_string(), 5))
        );
        // An open dialog covers the grid: nothing was read.
        assert_eq!(
            ack_candidate(true, Some(("aaa11111", 0)), Some(&status)),
            None
        );
        // Scrolled back into history: the pane is showing something else.
        assert_eq!(
            ack_candidate(false, Some(("aaa11111", 12)), Some(&status)),
            None
        );
        // Nothing focused at all, and a pane that is not done-unread.
        assert_eq!(ack_candidate(false, None, Some(&status)), None);
        assert_eq!(
            ack_candidate(false, Some(("aaa11111", 0)), Some(&blocked_status(5))),
            None
        );
        assert_eq!(ack_candidate(false, Some(("aaa11111", 0)), None), None);
    }

    /// One write per revision, however many frames the pane stays focused --
    /// and a fresh `Unseen` latch at a NEW revision is acknowledged again.
    #[test]
    fn done_unread_acknowledgement_fires_once_per_revision() {
        let mut ack = DoneUnreadAck::default();
        let focused = Some(("aaa11111", 0usize));
        let status = done_unread_status(5);

        for _ in 0..10 {
            ack.observe(ack_candidate(false, focused, Some(&status)));
        }
        assert_eq!(ack.take_due().as_deref(), Some("aaa11111"));
        // Every later frame at the same revision is a no-op.
        for _ in 0..10 {
            ack.observe(ack_candidate(false, focused, Some(&status)));
            assert_eq!(ack.take_due(), None);
        }
        // The session works and settles again: a new revision, acknowledged
        // on its own.
        let again = done_unread_status(6);
        ack.observe(ack_candidate(false, focused, Some(&again)));
        assert_eq!(ack.take_due().as_deref(), Some("aaa11111"));
    }

    /// Selecting a row is not reading it: only the pane the frame actually
    /// drew can be acknowledged, and a frame drawn under an overlay
    /// acknowledges nothing at all.
    #[test]
    fn selecting_a_row_never_acknowledges_it() {
        let mut ack = DoneUnreadAck::default();
        let status = done_unread_status(5);
        // The cursor is on `bbb22222` while `aaa11111` is focused: the render
        // can only ever qualify the focused pane.
        ack.observe(ack_candidate(false, Some(("aaa11111", 0)), Some(&status)));
        assert_eq!(ack.take_due().as_deref(), Some("aaa11111"));
        // And with a dialog open, nothing qualifies however long it is up.
        let mut ack = DoneUnreadAck::default();
        for _ in 0..5 {
            ack.observe(ack_candidate(true, Some(("bbb22222", 0)), Some(&status)));
        }
        assert_eq!(ack.take_due(), None);
    }

    // -- group collapse ----------------------------------------------------

    #[test]
    fn fold_group_is_one_reducer_for_the_keyboard_and_the_pointer() {
        let mut collapsed = HashSet::new();
        assert!(fold_group(&mut collapsed, "g", GroupFold::Collapse));
        assert!(collapsed.contains("g"));
        // Collapsing an already-collapsed group is idempotent, not a toggle.
        assert!(fold_group(&mut collapsed, "g", GroupFold::Collapse));
        assert!(!fold_group(&mut collapsed, "g", GroupFold::Expand));
        assert!(!collapsed.contains("g"));
        assert!(!fold_group(&mut collapsed, "g", GroupFold::Expand));
        // The header click's own toggle shares the same state.
        assert!(fold_group(&mut collapsed, "g", GroupFold::Toggle));
        assert!(!fold_group(&mut collapsed, "g", GroupFold::Toggle));
    }

    /// `^A Left`/`^A Right` act on the group the cursor is in -- the header it
    /// is parked on, else the selected session's own group -- and are no-ops
    /// on a flat row or the summary line.
    #[test]
    fn the_collapse_keys_resolve_the_group_under_the_cursor_and_no_ops_elsewhere() {
        let mut grouped = pane_row("aaa11111", "claude");
        grouped.group_id = Some("g".into());
        let rows = assemble_sidebar(
            &[grouped, pane_row("bbb22222", "claude")],
            &[],
            &HashMap::new(),
            0,
            0,
            DASHBOARD_PID,
            0,
        );
        assert_eq!(
            group_under_cursor(None, &rows, 0).as_deref(),
            Some("g"),
            "a grouped row folds its own group"
        );
        assert_eq!(
            group_under_cursor(None, &rows, 1),
            None,
            "a flat row is a no-op"
        );
        assert_eq!(
            group_under_cursor(Some(&Hit::GroupToggle("g".into())), &rows, 1).as_deref(),
            Some("g"),
            "the cursor parked on a header folds that header's group"
        );
        assert_eq!(
            group_under_cursor(Some(&Hit::SidebarSummary), &rows, 0),
            None,
            "the summary line owns no group"
        );
        // A collapsed group stays addressable from its own header, so `^A
        // Right` can open it again.
        let mut collapsed = HashSet::from(["g".to_string()]);
        let id = group_under_cursor(Some(&Hit::GroupToggle("g".into())), &rows, 0).unwrap();
        assert!(!fold_group(&mut collapsed, &id, GroupFold::Expand));
    }

    /// Codex review finding 1, on the real reap path (not just the pure
    /// `assemble_footer_facts` shaping): reaping the dashboard's only pane
    /// leaves `last_exited` filled with its harness, so the footer has
    /// something to describe even though `panes` (and therefore any
    /// `SidebarRow`) no longer does.
    #[test]
    fn reap_ended_panes_records_the_last_exited_pane_when_it_leaves_panes_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let cfg = CtxConfig::default();

        let spec = PaneSpec {
            agent_name: "test-agent".to_string(),
            argv: trivial_argv(),
            role: prompt::PromptRole::Orchestrator,
            verb: sessions::Verb::Chat,
            session_id: "77777777-2222-4333-8444-555555555555".to_string(),
            title: "orch".to_string(),
        };
        let mut panes = vec![
            Pane::spawn(
                spec,
                &state,
                &repo,
                &repo,
                (80, 24),
                &[],
                true,
                pane::DEFAULT_IDLE_QUIET,
            )
            .expect("spawn"),
        ];
        let mut queues: Vec<VecDeque<String>> = vec![VecDeque::new()];
        let (mut focused, mut selected) = (0usize, 0usize);
        let mut errors = ErrorLog::default();
        let mut last_exited: Option<LastExited> = None;

        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && !panes.is_empty() {
            for pane in panes.iter_mut() {
                pane.drain();
            }
            reap_ended_panes(
                &mut panes,
                &mut queues,
                &cfg,
                &state,
                &repo,
                &mut focused,
                &mut selected,
                &mut errors,
                &mut Vec::new(),
                &mut HashSet::new(),
                &mut last_exited,
                &mut VecDeque::new(),
                &mut HashMap::new(),
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(panes.is_empty(), "sanity: the pane was reaped");
        let recorded = last_exited.expect("last_exited must be filled once panes is empty");
        assert_eq!(recorded.harness, "test-agent");
    }

    /// Review finding 2: a pane whose child has exited with its last lines
    /// still sitting in the reader channel must not be reaped on that tick.
    /// The vt100 budget is shared across panes now (issue #330), so an exited
    /// pane can easily reach the reap with output outstanding -- and reaping
    /// it there retires the row and drops the parser, taking exactly the
    /// output the operator needs to understand the exit with it.
    #[test]
    fn an_exited_pane_is_not_reaped_while_its_last_output_is_still_queued() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let cfg = CtxConfig::default();

        let mut panes = vec![
            Pane::spawn(
                PaneSpec {
                    agent_name: "test-agent".to_string(),
                    argv: chatty_argv(),
                    role: prompt::PromptRole::Worker,
                    verb: sessions::Verb::Dash,
                    session_id: "77779999-2222-4333-8444-555555555555".to_string(),
                    title: "chatty".to_string(),
                },
                &state,
                &repo,
                &repo,
                (80, 24),
                &[],
                true,
                pane::DEFAULT_IDLE_QUIET,
            )
            .expect("spawn"),
        ];
        let mut queues: Vec<VecDeque<String>> = vec![VecDeque::new()];
        let (mut focused, mut selected) = (0usize, 0usize);
        let mut errors = ErrorLog::default();
        let mut reap = |panes: &mut Vec<Pane>, queues: &mut Vec<VecDeque<String>>| {
            reap_ended_panes(
                panes,
                queues,
                &cfg,
                &state,
                &repo,
                &mut focused,
                &mut selected,
                &mut errors,
                &mut Vec::new(),
                &mut HashSet::new(),
                &mut None,
                &mut VecDeque::new(),
                &mut HashMap::new(),
            );
        };

        // Wait for the child to exit WITHOUT draining: `on_turn_signal` polls
        // the exit status and never touches the reader channel, so everything
        // the child printed is still queued when it goes `Ended`.
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && !matches!(panes[0].state(), PaneState::Ended(_)) {
            panes[0].on_turn_signal();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            matches!(panes[0].state(), PaneState::Ended(_)),
            "the trivial child must exit within the deadline, got {:?}",
            panes[0].state()
        );

        // A tick whose shared budget was spent on the panes ahead of this
        // one: the drain stops on the budget with the channel unfinished.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !panes[0].has_pending_output() {
            panes[0].drain_with_budget(1);
            if !panes[0].has_pending_output() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(
            panes[0].has_pending_output(),
            "the child's output must reach the channel for this test to mean anything"
        );

        reap(&mut panes, &mut queues);
        assert_eq!(
            panes.len(),
            1,
            "an exited pane keeps its place while its output is still queued"
        );

        // The following ticks drain it to the end, and the operator sees the
        // last lines. Loop on the hold, not on the text: a plain pty (unix)
        // delivers the text in the very first message, so the 1-byte drain
        // above already put it on screen while the hold is still set.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && panes[0].has_pending_output() {
            panes[0].drain();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            panes[0].last_line().contains("zirv330"),
            "the child's output reached the screen before the reap: {:?}",
            panes[0].last_line()
        );
        assert!(
            !panes[0].has_pending_output(),
            "a drain that reached the end of the channel clears the hold"
        );

        reap(&mut panes, &mut queues);
        assert!(
            panes.is_empty(),
            "and with nothing left queued it is reaped on the next tick"
        );
    }

    /// A1-5: the module's own rule -- failures go to the sticky `⚠` error
    /// channel, confirmations go to the auto-expiring notice channel. A pane
    /// that exited 0 finished; it did not fail, so it must not pin the
    /// warning glyph nor consume one of the five `MAX_KEPT_ERRORS` slots a
    /// real failure needs.
    #[test]
    fn reaping_a_clean_exit_is_a_notice_and_a_failure_is_still_an_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let cfg = CtxConfig::default();

        let mut panes = Vec::new();
        for (argv, session_id, title) in [
            (
                trivial_argv(),
                "77771111-2222-4333-8444-555555555555",
                "clean",
            ),
            (
                failing_argv(),
                "77772222-2222-4333-8444-555555555555",
                "failed",
            ),
        ] {
            panes.push(
                Pane::spawn(
                    PaneSpec {
                        agent_name: "test-agent".to_string(),
                        argv,
                        role: prompt::PromptRole::Worker,
                        verb: sessions::Verb::Dash,
                        session_id: session_id.to_string(),
                        title: title.to_string(),
                    },
                    &state,
                    &repo,
                    &repo,
                    (80, 24),
                    &[],
                    true,
                    pane::DEFAULT_IDLE_QUIET,
                )
                .expect("spawn"),
            );
        }
        let mut queues: Vec<VecDeque<String>> = vec![VecDeque::new(), VecDeque::new()];
        let (mut focused, mut selected) = (0usize, 0usize);
        let mut errors = ErrorLog::default();

        let mut confirmations: Vec<String> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && !panes.is_empty() {
            for pane in panes.iter_mut() {
                pane.drain();
            }
            confirmations.extend(reap_ended_panes(
                &mut panes,
                &mut queues,
                &cfg,
                &state,
                &repo,
                &mut focused,
                &mut selected,
                &mut errors,
                &mut Vec::new(),
                &mut HashSet::new(),
                &mut None,
                &mut VecDeque::new(),
                &mut HashMap::new(),
            ));
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(panes.is_empty(), "sanity: both panes were reaped");
        assert!(
            !errors.iter().any(|e| e.contains("(exit 0)")),
            "a clean exit is a confirmation, not a sticky failure: {errors:?}"
        );
        assert!(
            errors.iter().any(|e| e.contains("exited with code 1")),
            "a failing exit is still an error: {errors:?}"
        );
        assert!(
            confirmations.iter().any(|c| c.contains("(exit 0)")),
            "and the clean exit is still reported, as a notice: {confirmations:?}"
        );
        assert!(
            !confirmations.iter().any(|c| c.contains("(exit 1)")),
            "a failure never becomes a confirmation: {confirmations:?}"
        );
    }

    #[test]
    fn early_pane_failure_bounds_age_and_unicode_tail() {
        let tail = "é".repeat(180);
        let error =
            early_pane_failure("codex", "12345678", 2, Duration::from_secs(10), &tail).unwrap();
        assert_eq!(
            error,
            format!(
                "codex pane 12345678 exited with code 2 10s after launch: {}",
                "é".repeat(160)
            )
        );
        assert!(
            early_pane_failure(
                "codex",
                "12345678",
                2,
                Duration::from_millis(10_001),
                "late"
            )
            .is_none()
        );
        assert!(early_pane_failure("codex", "12345678", 0, Duration::ZERO, "done").is_none());
        assert_eq!(
            early_pane_failure("codex", "12345678", 2, Duration::ZERO, "  ").unwrap(),
            "codex pane 12345678 exited with code 2 0s after launch: "
        );
    }

    /// A failed launch reports its output once, even if the reap runs again.
    #[cfg(unix)]
    #[test]
    fn early_worker_and_orchestrator_pane_failure_reports_the_output_tail_once() {
        for (role, verb) in [
            (prompt::PromptRole::Worker, sessions::Verb::Dash),
            (prompt::PromptRole::Orchestrator, sessions::Verb::Chat),
        ] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let state = StateDir::from_root(tmp.path().join("state"));
            let cfg = CtxConfig::default();
            let mut pane = Pane::spawn(
                PaneSpec {
                    agent_name: "test-agent".to_string(),
                    argv: vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        "printf 'first\nlaunch failed\n\n'; exit 2".to_string(),
                    ],
                    role,
                    verb,
                    session_id: "77774444-2222-4333-8444-555555555555".to_string(),
                    title: "failed launch".to_string(),
                },
                &state,
                tmp.path(),
                tmp.path(),
                (200, 24),
                &[],
                true,
                pane::DEFAULT_IDLE_QUIET,
            )
            .expect("spawn");
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                pane.drain();
                if matches!(pane.state(), PaneState::Ended(2))
                    && pane.last_line() == "launch failed"
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(pane.state(), PaneState::Ended(2));
            assert_eq!(pane.last_line(), "launch failed");
            let short = pane.short().to_string();
            let mut panes = vec![pane];
            let mut queues = vec![VecDeque::new()];
            let mut errors = ErrorLog::default();
            for _ in 0..2 {
                reap_ended_panes(
                    &mut panes,
                    &mut queues,
                    &cfg,
                    &state,
                    tmp.path(),
                    &mut 0,
                    &mut 0,
                    &mut errors,
                    &mut Vec::new(),
                    &mut HashSet::new(),
                    &mut None,
                    &mut VecDeque::new(),
                    &mut HashMap::new(),
                );
            }
            assert!(panes.is_empty());
            assert_eq!(errors.len(), 1, "{errors:?}");
            let error = errors.iter().next().expect("one error");
            assert!(
                error.starts_with(&format!("test-agent pane {short} exited with code 2 ")),
                "{error}"
            );
            assert!(error.ends_with("s after launch: launch failed"), "{error}");
        }
    }

    /// A1-1: the budget sweep reads (and parses) every budgeted pane's whole
    /// transcript, so it belongs on the ~1s disk cadence every other disk
    /// fact in the loop uses -- not on the render loop's own 20-100 ticks a
    /// second.
    #[test]
    fn the_pane_token_budget_sweep_is_throttled_to_the_facts_cadence() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let cfg = CtxConfig::default();

        let mut pane = Pane::spawn(
            PaneSpec {
                agent_name: "test-agent".to_string(),
                argv: trivial_argv(),
                role: prompt::PromptRole::Worker,
                verb: sessions::Verb::Dash,
                session_id: "77773333-2222-4333-8444-555555555555".to_string(),
                title: "budgeted".to_string(),
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
        pane.set_budget_tokens(Some(1_000_000));
        let mut panes = vec![pane];
        let mut errors = ErrorLog::default();

        let start = Instant::now();
        let mut last_sweep = start.checked_sub(FACTS_THROTTLE).unwrap_or(start);
        let mut reads = 0usize;
        for tick in 0..100u64 {
            let now = start + Duration::from_millis(tick * 5);
            enforce_pane_token_budgets_with(
                &mut panes,
                &cfg,
                &mut errors,
                &mut last_sweep,
                now,
                |_| {
                    reads += 1;
                    None
                },
            );
        }
        assert_eq!(
            reads, 1,
            "100 ticks spanning under one throttle interval may read a budgeted pane's \
             transcript once, not {reads} times"
        );

        for pane in &mut panes {
            let _ = pane.finish_shutdown();
        }
    }

    /// Review round 1 (R5): `pane_transcript_usage` resolved every pane's
    /// transcript against the DASHBOARD's repo, so a worktree-hosted pane was
    /// priced off whichever transcript lives under the root repo's own
    /// project slug -- another pane's spend, or none at all. Both slugs hold a
    /// transcript here, so the answer proves which cwd was used rather than
    /// merely that something was found.
    #[test]
    fn a_worktree_panes_transcript_is_resolved_against_its_own_cwd() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        std::fs::create_dir_all(&worktree).expect("mkdir worktree");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let cfg = CtxConfig::default();
        let session = "5f5f5f5f-2222-4333-8444-555555555555";

        for (cwd, output) in [(&repo, 111u64), (&worktree, 222u64)] {
            let dir = home
                .join(".claude/projects")
                .join(crate::commands::ctx::adapters::claude::project_slug(cwd));
            std::fs::create_dir_all(&dir).expect("mkdir projects");
            std::fs::write(
                dir.join(format!("{session}.jsonl")),
                format!(
                    "{{\"type\":\"assistant\",\"message\":{{\"usage\":{{\"output_tokens\":{output}}}}}}}\n"
                ),
            )
            .expect("write transcript");
        }

        let mut pane = Pane::spawn(
            PaneSpec {
                agent_name: "claude".to_string(),
                argv: trivial_argv(),
                role: prompt::PromptRole::Worker,
                verb: sessions::Verb::Dash,
                session_id: session.to_string(),
                title: "worktree".to_string(),
            },
            &state,
            &worktree,
            &repo,
            (80, 24),
            &[],
            true,
            pane::DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");

        let usage = pane_transcript_usage(&pane, &cfg).expect("the pane's own transcript");
        assert_eq!(
            usage.output_tokens, 222,
            "a worktree pane is priced off its own cwd's transcript, not the dashboard repo's"
        );

        let _ = pane.finish_shutdown();
    }

    /// A1-2: restoring a retained ended row grows `panes` and shrinks
    /// `retained` in one step, so every index between the two moves. The
    /// cursor must keep naming the same session (and follow the restored row
    /// into its new pane when it was on that row).
    #[test]
    fn restoring_an_ended_row_keeps_the_sidebar_cursor_on_the_same_session() {
        // Roster: [p0 p1][view-only][r_a r_b r_c] -- 2 panes, 1 view-only, 3
        // retained; restoring appends one pane and removes one retained row.
        assert_eq!(restore_fixup(2, 3, 3, 0), 0, "a pane row never moves");
        assert_eq!(
            restore_fixup(2, 3, 3, 2),
            3,
            "the view-only row is pushed down by the appended pane"
        );
        assert_eq!(
            restore_fixup(2, 3, 3, 3),
            2,
            "the cursor follows the restored row into its new pane"
        );
        assert_eq!(restore_fixup(2, 3, 3, 4), 4, "r_b: pushed down, then back");
        assert_eq!(restore_fixup(2, 3, 3, 5), 5, "r_c: pushed down, then back");
        // Restoring the LAST retained row instead: nothing after it shifts
        // back, so the rows before it only take the append.
        assert_eq!(restore_fixup(2, 3, 5, 3), 4, "r_a takes only the append");
        assert_eq!(restore_fixup(2, 3, 5, 4), 5, "r_b takes only the append");
        assert_eq!(restore_fixup(2, 3, 5, 5), 2, "the cursor follows r_c");
    }

    /// A real temp git repo (`git init`, one commit) plus one linked
    /// worktree at EXACTLY the path `agent::allocate_worktree` itself would
    /// put it -- `<repo>/.zirv/worktrees/<short>` -- so `reclaim_pane_
    /// worktree`'s own tests can exercise `agent::is_agent_managed_
    /// worktree`/`agent::reclaim_worktree` against a tree those functions
    /// actually recognise, without a real `zirv ctx agent --worktree`
    /// delegation. Returns `None` (callers skip) if `git` itself is
    /// unavailable or any setup step fails -- same discipline as
    /// `git_repo_with_linked_worktree`.
    /// Issue #319: also writes a matching ownership record (as `agent::
    /// allocate_worktree` now does for real) into the returned `StateDir` --
    /// without one, `agent::reclaim_worktree` has no recorded base commit to
    /// probe against and refuses outright (`InspectionFailed { probe:
    /// "record", .. }`), which is correct but not what these tests exercise.
    fn git_repo_with_agent_managed_worktree()
    -> Option<(tempfile::TempDir, StateDir, PathBuf, PathBuf)> {
        if !git_available() {
            eprintln!("skipping: git not found on PATH");
            return None;
        }
        let root = tempfile::tempdir().ok()?;
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).ok()?;
        let worktree = repo
            .join(crate::utils::SCRIPT_DIR_NAME)
            .join("worktrees")
            .join("abcd1234");

        let run = |args: &[&str], cwd: &Path| -> bool {
            std::process::Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };

        if !run(&["init", "-q"], &repo) {
            return None;
        }
        if !run(&["config", "user.email", "test@example.com"], &repo) {
            return None;
        }
        if !run(&["config", "user.name", "test"], &repo) {
            return None;
        }
        std::fs::write(repo.join("README.md"), "hello\n").ok()?;
        if !run(&["add", "README.md"], &repo) {
            return None;
        }
        if !run(&["commit", "-q", "-m", "initial"], &repo) {
            return None;
        }
        let base_output = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()?;
        let base_commit = String::from_utf8_lossy(&base_output.stdout)
            .trim()
            .to_string();
        let worktree_str = worktree.to_string_lossy().to_string();
        if !run(
            &[
                "worktree",
                "add",
                "-b",
                "abcd1234",
                &worktree_str,
                &base_commit,
            ],
            &repo,
        ) {
            return None;
        }

        let state = StateDir::from_root(root.path().join("state"));
        let repo_slug = crate::commands::ctx::state::repo_slug(&repo);
        crate::commands::ctx::worktree::append_record(
            &state,
            &repo_slug,
            &crate::commands::ctx::worktree::WorktreeRecord {
                path: worktree.to_string_lossy().to_string(),
                branch: "abcd1234".to_string(),
                base_commit,
                owner_session: None,
                owner_pid: None,
                created_at: 1_700_000_000,
                status: crate::commands::ctx::worktree::WorktreeStatus::Active,
                note: None,
                setup_digest: None,
                idled_at: None,
            },
        )
        .ok()?;

        Some((root, state, repo, worktree))
    }

    /// Review finding (2026-09), finding 2a: `agent::run_with`'s own
    /// `--worktree` reclamation only ever covers the HEADLESS fallback path
    /// -- a dashboard-hosted worker pane's linked worktree was left with
    /// nothing reclaiming it once the pane's child exited. `reclaim_pane_
    /// worktree` is the helper `reap_ended_panes` calls for that; tested
    /// directly here (rather than through a real spawned pane) per the
    /// finding's own guidance.
    #[test]
    fn reclaim_pane_worktree_removes_a_clean_agent_managed_worktree() {
        let Some((_root, state, repo, worktree)) = git_repo_with_agent_managed_worktree() else {
            return;
        };
        let outcome = reclaim_pane_worktree(&state, &repo, &worktree, true, 4);
        assert_eq!(
            outcome,
            Some(crate::commands::ctx::agent::ReclaimOutcome::Removed)
        );
        assert!(!worktree.exists(), "the clean worktree must be removed");
    }

    /// Issue #319: a pane cwd with only untracked content is archived, then
    /// removed -- never left in place, and never force-removed without a
    /// copy first, exactly like `agent::run_with`'s own headless reclamation.
    #[test]
    fn reclaim_pane_worktree_archives_untracked_content_then_removes_it() {
        let Some((_root, state, repo, worktree)) = git_repo_with_agent_managed_worktree() else {
            return;
        };
        std::fs::write(worktree.join("scratch.txt"), "not committed\n").expect("write");

        let outcome = reclaim_pane_worktree(&state, &repo, &worktree, true, 4);
        match outcome {
            Some(crate::commands::ctx::agent::ReclaimOutcome::Archived(dest)) => {
                assert_eq!(
                    std::fs::read_to_string(dest.join("scratch.txt")).expect("read archived"),
                    "not committed\n"
                );
            }
            other => panic!("expected Some(Archived), got {other:?}"),
        }
        assert!(
            !worktree.exists(),
            "the worktree must be removed once its untracked content is archived"
        );
    }

    /// An ordinary pane cwd (not under `.zirv/worktrees/`) is never touched
    /// -- this dashboard did not allocate it, so no reclaim path may act on
    /// it.
    #[test]
    fn reclaim_pane_worktree_never_touches_an_unmanaged_cwd() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        assert_eq!(reclaim_pane_worktree(&state, &repo, &repo, true, 4), None);
    }

    /// Review round 3: ownership travels on the spawn request, never on the
    /// path. A pane whose request did not allocate its cwd with `--worktree`
    /// (an operator-named `--workdir` that merely lives under
    /// `.zirv/worktrees/`) is never reclaimed, clean or not.
    #[test]
    fn reclaim_pane_worktree_never_touches_a_cwd_the_pane_does_not_own() {
        let Some((_root, state, repo, worktree)) = git_repo_with_agent_managed_worktree() else {
            return;
        };
        assert_eq!(
            reclaim_pane_worktree(&state, &repo, &worktree, false, 4),
            None
        );
        assert!(
            worktree.exists(),
            "an operator-named worktree must survive its pane's exit"
        );
    }

    /// A trivial child that prints one recognisable line and exits -- output
    /// that must still reach the screen before the pane is reaped.
    #[cfg(windows)]
    fn chatty_argv() -> Vec<String> {
        vec![
            "cmd".to_string(),
            "/c".to_string(),
            "echo zirv330".to_string(),
        ]
    }

    #[cfg(unix)]
    fn chatty_argv() -> Vec<String> {
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo zirv330".to_string(),
        ]
    }

    /// The same trivial child, failing -- what a reap must keep routing to
    /// the sticky error channel.
    #[cfg(windows)]
    fn failing_argv() -> Vec<String> {
        vec![
            "cmd".to_string(),
            "/c".to_string(),
            "exit".to_string(),
            "1".to_string(),
        ]
    }

    #[cfg(unix)]
    fn failing_argv() -> Vec<String> {
        vec!["sh".to_string(), "-c".to_string(), "exit 1".to_string()]
    }

    /// A trivial, immediately-exiting child -- never a real agent (the
    /// ABSOLUTE rule this plan spells out) -- just enough of a process for
    /// `Pane::spawn` to have something real to supervise, matching
    /// `pane.rs`'s own test pattern.
    #[test]
    fn on_quit_writes_this_repos_roster_before_removing_the_requests_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let spec = PaneSpec {
            agent_name: "test-agent".to_string(),
            argv: trivial_argv(),
            role: prompt::PromptRole::Worker,
            verb: sessions::Verb::Dash,
            session_id: "11111111-2222-4333-8444-555555555555".to_string(),
            title: "wrk test".to_string(),
        };
        let mut panes = vec![
            Pane::spawn(
                spec,
                &state,
                &repo,
                &repo,
                (80, 24),
                &[],
                true,
                pane::DEFAULT_IDLE_QUIET,
            )
            .expect("spawn"),
        ];

        let requests_dir = state.dash().join("aaaa1111-token").join("requests");
        std::fs::create_dir_all(&requests_dir).expect("mkdir requests");

        on_quit(&panes, &[], &[], &requests_dir, &state, &repo);

        assert!(
            !requests_dir
                .parent()
                .expect("requests dir has a parent")
                .exists(),
            "the whole capability-token directory is removed on quit"
        );

        let slug = super::super::state::repo_slug(&repo);
        let written = roster::take_roster(&state, &slug, super::super::state::now_secs(), 999_999)
            .expect("on_quit must have written a roster");
        assert_eq!(written.panes.len(), 1);
        assert_eq!(written.panes[0].agent, "test-agent");
        assert_eq!(written.panes[0].short, panes[0].short());
        assert_eq!(written.panes[0].role, prompt::PromptRole::Worker.label());

        for pane in panes.iter_mut() {
            let _ = pane.shutdown("");
        }
    }

    // R2: an ended pane is reaped out of the dashboard entirely -- vector,
    // nudge queue, registry record and socket -- rather than kept forever.

    #[test]
    fn reap_fixup_shifts_indices_that_pointed_past_the_removed_pane() {
        assert_eq!(
            reap_fixup(1, 3, 4),
            (2, 3),
            "both indices pointed past the removed pane and shift down"
        );
        assert_eq!(
            reap_fixup(2, 1, 0),
            (1, 0),
            "indices before the removed pane are untouched"
        );
        assert_eq!(
            reap_fixup(2, 2, 2),
            (0, 2),
            "focus lands on the first pane; the sidebar cursor stays where it is"
        );
        assert_eq!(
            reap_fixup(0, 0, 0),
            (0, 0),
            "reaping the only pane leaves both at zero"
        );
        assert_eq!(
            reap_fixup(0, 1, 1),
            (0, 0),
            "everything after the first pane shifts down one"
        );
    }

    #[test]
    fn ended_worker_reports_without_becoming_injectable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let spec = PaneSpec {
            agent_name: "test-agent".to_string(),
            argv: trivial_argv(),
            role: prompt::PromptRole::Worker,
            verb: sessions::Verb::Dash,
            session_id: "dddddddd-2222-4333-8444-555555555555".to_string(),
            title: "worker".to_string(),
        };
        let mut pane = Pane::spawn(
            spec,
            &state,
            tmp.path(),
            tmp.path(),
            (80, 24),
            &[],
            true,
            pane::DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");
        pane.set_report_to(Some("aaaa1111".to_string()));
        let mut panes = vec![pane];
        let mut errors = ErrorLog::default();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && !matches!(panes[0].state(), PaneState::Ended(_)) {
            panes[0].drain();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(matches!(panes[0].state(), PaneState::Ended(_)));
        assert!(!panes[0].injectable());
        reap_ended_panes(
            &mut panes,
            &mut vec![VecDeque::new()],
            &CtxConfig::default(),
            &state,
            tmp.path(),
            &mut 0,
            &mut 0,
            &mut errors,
            &mut Vec::new(),
            &mut HashSet::new(),
            &mut None,
            &mut VecDeque::new(),
            &mut HashMap::new(),
        );
        assert!(panes.is_empty());
        let messages = mail::list(
            &state,
            &super::super::state::repo_slug(tmp.path()),
            None,
            Some("aaaa1111"),
        )
        .expect("list");
        assert_eq!(messages.len(), 1);
        assert!(
            messages[0]
                .1
                .body
                .contains("ended with exit code 0 with unread output")
        );
    }

    #[cfg(unix)]
    #[test]
    fn early_failed_worker_mails_exit_tail_and_ledgers_without_a_transcript() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let cfg = CtxConfig::default();
        let spec = PaneSpec {
            agent_name: "codex".to_string(),
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo 'error: unexpected argument' >&2; exit 2".into(),
            ],
            role: prompt::PromptRole::Worker,
            verb: sessions::Verb::Dash,
            session_id: uuid::Uuid::new_v4().to_string(),
            title: "worker".to_string(),
        };
        let mut pane = Pane::spawn(
            spec,
            &state,
            tmp.path(),
            tmp.path(),
            (80, 24),
            &[],
            false,
            pane::DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");
        pane.set_report_to(Some("aaaa1111".into()));
        pane.set_delegation(pane::DelegationFacts {
            requester: "aaaa1111".into(),
            mode: super::super::permit::WorkerMode::ReadOnly,
            principal: "root/aaaa1111".into(),
            envelope_sha256: None,
            started_at: Instant::now(),
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            pane.drain();
            if matches!(pane.state(), PaneState::Ended(2))
                && pane.last_line().contains("unexpected argument")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(matches!(pane.state(), PaneState::Ended(2)));
        let short = pane.short().to_string();
        let mut panes = vec![pane];
        let mut errors = ErrorLog::default();
        reap_ended_panes(
            &mut panes,
            &mut vec![VecDeque::new()],
            &cfg,
            &state,
            tmp.path(),
            &mut 0,
            &mut 0,
            &mut errors,
            &mut Vec::new(),
            &mut HashSet::new(),
            &mut None,
            &mut VecDeque::new(),
            &mut HashMap::new(),
        );
        assert!(panes.is_empty());
        let messages = mail::list(
            &state,
            &super::super::state::repo_slug(tmp.path()),
            None,
            Some("aaaa1111"),
        )
        .expect("list");
        assert_eq!(messages.len(), 1);
        assert!(messages[0].1.body.contains("exit code 2"));
        assert!(messages[0].1.body.contains("error: unexpected argument"));
        let rows = super::super::log::read_delegations(&state, usize::MAX);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].exit_code, 2);
        assert_eq!(rows[0].outcome, "failed");
        assert_eq!(rows[0].input_tokens, 0);
        assert!(
            super::super::attention::load(&state, &short)
                .evidence
                .contains("error: unexpected argument")
        );
        assert!(sessions::load_record(&state, &short).is_none());
    }

    /// F5: a candidate this launch took but never offered goes back into the
    /// roster on the way out, deduped against whatever is still live.
    #[test]
    fn merge_unoffered_adds_back_only_what_is_not_already_there() {
        let live = roster::RosterPane {
            agent: "claude".to_string(),
            session_id: "11111111-2222-4333-8444-555555555555".to_string(),
            role: prompt::PromptRole::Worker.label().to_string(),
            short: "aaaa1111".to_string(),
            title: "wrk claude".to_string(),
            ..Default::default()
        };
        let unoffered = roster::RosterPane {
            agent: "codex".to_string(),
            session_id: "22222222-2222-4333-8444-555555555555".to_string(),
            role: prompt::PromptRole::Worker.label().to_string(),
            short: "bbbb2222".to_string(),
            title: "wrk codex".to_string(),
            ..Default::default()
        };

        assert_eq!(
            merge_unoffered(vec![live.clone()], std::slice::from_ref(&unoffered)),
            vec![live.clone(), unoffered.clone()]
        );
        assert_eq!(
            merge_unoffered(vec![live.clone()], std::slice::from_ref(&live)),
            vec![live.clone()],
            "a candidate that is live again is written once, as the live pane"
        );
        assert_eq!(merge_unoffered(Vec::new(), &[]), Vec::new());
    }

    /// F5, end to end through the file: a dashboard that exits with the
    /// restore dialog still unanswered must leave the offer where the next
    /// launch will find it, rather than overwriting it with its own (here
    /// empty) set of live panes.
    #[test]
    fn an_unanswered_restore_dialog_round_trips_through_the_roster() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let requests_dir = state.dash().join("aaaa1111-token").join("requests");
        std::fs::create_dir_all(&requests_dir).expect("mkdir requests");

        let candidate = roster::RosterPane {
            agent: "codex".to_string(),
            session_id: "22222222-2222-4333-8444-555555555555".to_string(),
            role: prompt::PromptRole::Worker.label().to_string(),
            short: "bbbb2222".to_string(),
            title: "wrk codex".to_string(),
            ..Default::default()
        };
        let pending = ui::Overlay::Restore(build_restore_view(std::slice::from_ref(&candidate)));
        let answered = ui::Overlay::None;
        let candidates = vec![candidate.clone()];

        // No panes at all: exactly the early-total-death shape that lost the
        // roster before F5.
        on_quit(
            &[],
            unoffered_candidates(&pending, &candidates),
            &[],
            &requests_dir,
            &state,
            &repo,
        );

        let slug = super::super::state::repo_slug(&repo);
        let written = roster::take_roster(&state, &slug, super::super::state::now_secs(), 999_999)
            .expect("a roster is still written");
        assert_eq!(
            written.panes,
            vec![candidate],
            "the unoffered candidate is offered again next launch"
        );

        assert!(
            unoffered_candidates(&answered, &candidates).is_empty(),
            "an answered dialog owes the next launch nothing"
        );
    }

    /// 2026-09-06: `agent::run_with` returns at `Dispatch::Answered` the
    /// moment a dashboard acknowledges the spawn -- 600-odd lines before the
    /// only `log::append_delegation` call it has -- so a delegation the
    /// dashboard accepted as a pane was never written to the ledger at all.
    /// With headless spawns gone that is every delegation made while a
    /// dashboard is live, which is why `logs/delegations.jsonl` stopped
    /// growing and `zirv ctx status` reported `$0.00 this session`.
    #[test]
    fn reaping_a_pane_delegation_writes_exactly_one_ledger_row_for_its_requester() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().to_path_buf();

        let session_id = "5a5a5a5a-2222-4333-8444-555555555555";
        let spec = PaneSpec {
            agent_name: "claude".to_string(),
            argv: trivial_argv(),
            role: prompt::PromptRole::Worker,
            verb: sessions::Verb::Dash,
            session_id: session_id.to_string(),
            title: "wrk claude".to_string(),
        };
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            pane::DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");
        pane.set_work_group_id(Some("wg-ledger".to_string()));
        pane.set_delegation(pane::DelegationFacts {
            requester: "orch0001".to_string(),
            mode: crate::commands::ctx::permit::WorkerMode::Writing,
            principal: "root/aaaa1111".to_string(),
            envelope_sha256: Some("deadbeef".to_string()),
            started_at: Instant::now(),
        });

        let cfg = CtxConfig::default();
        let adapter = adapters::select(Some("claude"), &[], &cfg).expect("adapter");
        let transcript = adapter.transcript_path(&SessionRef {
            id: SessionId::parse(session_id),
            cwd: repo.clone(),
        });
        std::fs::create_dir_all(transcript.parent().expect("transcript parent"))
            .expect("create transcript dir");
        std::fs::write(
            &transcript,
            r#"{"type":"assistant","message":{"usage":{"input_tokens":10,"cache_creation_input_tokens":20,"cache_read_input_tokens":30,"output_tokens":5}}}"#,
        )
        .expect("write transcript");

        account_reaped_pane_spend(&pane, &cfg, &state, 0);
        let _ = pane.finish_shutdown();

        let rows = crate::commands::ctx::log::read_delegations(&state, usize::MAX);
        assert_eq!(rows.len(), 1, "exactly one row per completed delegation");
        let row = &rows[0];
        assert_eq!(
            row.parent_session, "orch0001",
            "the row is attributed to the session that delegated it"
        );
        assert_eq!(row.session, session_id);
        assert_eq!(row.work_group_id.as_deref(), Some("wg-ledger"));
        assert_eq!(row.input_tokens, 10);
        assert_eq!(row.cache_creation_input_tokens, 20);
        assert_eq!(row.cache_read_input_tokens, 30);
        assert_eq!(row.output_tokens, 5);
        assert_eq!(row.outcome, "ok");
    }

    #[cfg(unix)]
    #[test]
    fn reaping_a_grouped_pane_rolls_its_transcript_spend_into_the_group() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().to_path_buf();
        let worker_cwd = tmp.path().join("worker-cwd");
        std::fs::create_dir_all(&worker_cwd).expect("create worker cwd");
        let group = crate::commands::ctx::group::WorkGroup {
            work_group_id: "wg-reap-spend".to_string(),
            parent_session_id: String::new(),
            scope: "account a pane".to_string(),
            child_limit: 3,
            token_budget: Some(1_000),
            spent_tokens: 10,
            reserved_tokens: 0,
            deadline_secs: None,
            completion_contract: String::new(),
            created_at: crate::commands::ctx::state::now_secs(),
            closed_at: None,
            admitted_children: 1,
            sub_orchestrator_session: None,
        };
        crate::commands::ctx::group::create(&state, &group).expect("create group");

        let session_id = "45454545-2222-4333-8444-555555555555";
        let spec = PaneSpec {
            agent_name: "claude".to_string(),
            argv: trivial_argv(),
            role: prompt::PromptRole::Worker,
            verb: sessions::Verb::Dash,
            session_id: session_id.to_string(),
            title: "wrk claude".to_string(),
        };
        let mut pane = Pane::spawn(
            spec,
            &state,
            &worker_cwd,
            &repo,
            (80, 24),
            &[],
            true,
            pane::DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");
        pane.set_work_group_id(Some("wg-reap-spend".to_string()));

        let cfg = CtxConfig::default();
        let adapter = adapters::select(Some("claude"), &[], &cfg).expect("adapter");
        let transcript = adapter.transcript_path(&SessionRef {
            id: SessionId::parse(session_id),
            cwd: worker_cwd,
        });
        std::fs::create_dir_all(transcript.parent().expect("transcript parent"))
            .expect("create transcript dir");
        std::fs::write(
            &transcript,
            r#"{"type":"assistant","message":{"usage":{"input_tokens":10,"cache_creation_input_tokens":20,"cache_read_input_tokens":30,"output_tokens":5}}}"#,
        )
        .expect("write transcript");

        let mut panes = vec![pane];
        let mut queues = vec![VecDeque::new()];
        let mut errors = ErrorLog::default();
        let mut focused = 0;
        let mut selected = 0;
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && !panes.is_empty() {
            for pane in &mut panes {
                pane.drain();
            }
            reap_ended_panes(
                &mut panes,
                &mut queues,
                &cfg,
                &state,
                &repo,
                &mut focused,
                &mut selected,
                &mut errors,
                &mut Vec::new(),
                &mut HashSet::new(),
                &mut None,
                &mut VecDeque::new(),
                &mut HashMap::new(),
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        assert!(panes.is_empty(), "pane was reaped: {errors:?}");
        assert_eq!(
            crate::commands::ctx::group::load(&state, "wg-reap-spend")
                .expect("load")
                .expect("group")
                .spent_tokens,
            75,
            "10 existing + 10 input + 20 cache-create + 30 cache-read + 5 output"
        );
    }

    /// Issue #301: reaping a pane that carries a token ceiling (set at
    /// admission via `fulfill_spawn_request`'s own `pane.set_budget_tokens`)
    /// releases exactly that reservation and rolls the pane's ACTUAL spend
    /// in -- not the ceiling it was reserved under, which is very rarely the
    /// same number.
    #[cfg(unix)]
    #[test]
    fn reaping_a_grouped_pane_settles_its_reservation_exactly_once() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().to_path_buf();
        let worker_cwd = tmp.path().join("worker-cwd");
        std::fs::create_dir_all(&worker_cwd).expect("create worker cwd");
        let group = crate::commands::ctx::group::WorkGroup {
            work_group_id: "wg-settle".to_string(),
            parent_session_id: String::new(),
            scope: "settle a reservation".to_string(),
            child_limit: 3,
            token_budget: Some(1_000),
            spent_tokens: 0,
            // What `admit_child` would have reserved for this pane's own
            // ceiling below (500) -- set up directly, rather than through a
            // real admission, so this test isolates settlement alone.
            reserved_tokens: 500,
            deadline_secs: None,
            completion_contract: String::new(),
            created_at: crate::commands::ctx::state::now_secs(),
            closed_at: None,
            admitted_children: 1,
            sub_orchestrator_session: None,
        };
        crate::commands::ctx::group::create(&state, &group).expect("create group");

        let session_id = "46464646-2222-4333-8444-555555555555";
        let spec = PaneSpec {
            agent_name: "claude".to_string(),
            argv: trivial_argv(),
            role: prompt::PromptRole::Worker,
            verb: sessions::Verb::Dash,
            session_id: session_id.to_string(),
            title: "wrk claude".to_string(),
        };
        let mut pane = Pane::spawn(
            spec,
            &state,
            &worker_cwd,
            &repo,
            (80, 24),
            &[],
            true,
            pane::DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");
        pane.set_work_group_id(Some("wg-settle".to_string()));
        pane.set_budget_tokens(Some(500));

        let cfg = CtxConfig::default();
        let adapter = adapters::select(Some("claude"), &[], &cfg).expect("adapter");
        let transcript = adapter.transcript_path(&SessionRef {
            id: SessionId::parse(session_id),
            cwd: worker_cwd,
        });
        std::fs::create_dir_all(transcript.parent().expect("transcript parent"))
            .expect("create transcript dir");
        std::fs::write(
            &transcript,
            r#"{"type":"assistant","message":{"usage":{"input_tokens":40,"output_tokens":5}}}"#,
        )
        .expect("write transcript");

        let mut panes = vec![pane];
        let mut queues = vec![VecDeque::new()];
        let mut errors = ErrorLog::default();
        let mut focused = 0;
        let mut selected = 0;
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && !panes.is_empty() {
            for pane in &mut panes {
                pane.drain();
            }
            reap_ended_panes(
                &mut panes,
                &mut queues,
                &cfg,
                &state,
                &repo,
                &mut focused,
                &mut selected,
                &mut errors,
                &mut Vec::new(),
                &mut HashSet::new(),
                &mut None,
                &mut VecDeque::new(),
                &mut HashMap::new(),
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        assert!(panes.is_empty(), "pane was reaped: {errors:?}");
        let settled = crate::commands::ctx::group::load(&state, "wg-settle")
            .expect("load")
            .expect("group");
        assert_eq!(
            settled.reserved_tokens, 0,
            "settlement must release the full reservation, not just what was actually spent"
        );
        assert_eq!(
            settled.spent_tokens, 45,
            "spend must reflect ACTUAL usage (40 input + 5 output), never the reserved ceiling"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dashboard_stops_a_pane_that_exhausts_its_token_budget() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().to_path_buf();
        let session_id = "56565656-2222-4333-8444-555555555555";
        let spec = PaneSpec {
            agent_name: "claude".to_string(),
            argv: vec!["sleep".to_string(), "30".to_string()],
            role: prompt::PromptRole::Worker,
            verb: sessions::Verb::Dash,
            session_id: session_id.to_string(),
            title: "wrk claude".to_string(),
        };
        let mut pane = Pane::spawn(
            spec,
            &state,
            &repo,
            &repo,
            (80, 24),
            &[],
            true,
            pane::DEFAULT_IDLE_QUIET,
        )
        .expect("spawn");
        pane.set_budget_tokens(Some(50));

        let cfg = CtxConfig::default();
        let adapter = adapters::select(Some("claude"), &[], &cfg).expect("adapter");
        let transcript = adapter.transcript_path(&SessionRef {
            id: SessionId::parse(session_id),
            cwd: repo.clone(),
        });
        std::fs::create_dir_all(transcript.parent().expect("transcript parent"))
            .expect("create transcript dir");
        std::fs::write(
            &transcript,
            r#"{"type":"assistant","message":{"usage":{"input_tokens":30,"cache_creation_input_tokens":10,"cache_read_input_tokens":5,"output_tokens":10}}}"#,
        )
        .expect("write transcript");

        let mut panes = vec![pane];
        let mut errors = ErrorLog::default();
        let now = Instant::now();
        let mut last_sweep = now.checked_sub(FACTS_THROTTLE).unwrap_or(now);
        enforce_pane_token_budgets(&mut panes, &cfg, &mut errors, &mut last_sweep, now);
        assert!(
            !matches!(panes[0].state(), PaneState::Ended(_)),
            "the first hard-stop observation gives the same one-tick grace as exec"
        );
        enforce_pane_token_budgets(
            &mut panes,
            &cfg,
            &mut errors,
            &mut last_sweep,
            now + FACTS_THROTTLE,
        );

        assert!(
            matches!(
                panes[0].state(),
                PaneState::Ended(super::super::exec::EXIT_BUDGET_EXHAUSTED)
            ),
            "the pane must stop with the shared budget-exhausted exit"
        );
        assert!(
            errors.iter().any(|e| e.contains("budget exhausted")),
            "the dashboard should explain the stop: {errors:?}"
        );
        let _ = panes[0].finish_shutdown();
    }

    /// G3, end to end through `on_quit`: a restore candidate the pane cap
    /// deferred this session must still be in the roster `on_quit` writes,
    /// even though the restore dialog that offered it is long since closed
    /// (`unoffered` here is empty -- exactly the state a closed dialog leaves
    /// it in) and even though two *other* candidates from the same roster are
    /// already live, spawned panes.
    #[test]
    fn on_quit_writes_back_restore_candidates_the_pane_cap_deferred() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let requests_dir = state.dash().join("aaaa1111-token").join("requests");
        std::fs::create_dir_all(&requests_dir).expect("mkdir requests");

        let deferred = restore_pane("cccc3333", "33333333-2222-4333-8444-555555555555");

        // No live panes needed to prove the point: `deferred_restore` must
        // round-trip through the roster on its own, the same as `unoffered`
        // does in `an_unanswered_restore_dialog_round_trips_through_the_roster`.
        on_quit(
            &[],
            &[],
            std::slice::from_ref(&deferred),
            &requests_dir,
            &state,
            &repo,
        );

        let slug = super::super::state::repo_slug(&repo);
        let written = roster::take_roster(&state, &slug, super::super::state::now_secs(), 999_999)
            .expect("a roster is still written");
        assert_eq!(
            written.panes,
            vec![deferred],
            "the cap-deferred candidate is offered again next launch"
        );
    }

    /// P4, end to end: a candidate the liveness gate holds back must survive
    /// the launch that declined to offer it.
    ///
    /// `take_roster` claims the roster by rename *before* it reads, so by the
    /// time `partition_live` runs the candidate has already been consumed off
    /// disk. Simply dropping it would make a *wrong* liveness verdict --
    /// entirely possible, since the probe is a pid lookup and a roster may be
    /// days old while the OS recycles pids -- permanently destroy the pane.
    /// So the skipped half is seeded straight into `deferred_restore` (the
    /// same pool G3 and H3 already use) and merged back by `on_quit`.
    ///
    /// This pins the wiring `run_dashboard` performs inline: partition, then
    /// hand the skipped half to `on_quit` as deferred.
    #[test]
    fn a_candidate_held_back_because_its_session_is_live_is_written_back_to_the_roster() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let requests_dir = state.dash().join("aaaa1111-token").join("requests");
        std::fs::create_dir_all(&requests_dir).expect("mkdir requests");

        let live_one = restore_pane("dddd4444", "44444444-2222-4333-8444-555555555555");
        let dead_one = restore_pane("eeee5555", "55555555-2222-4333-8444-555555555555");

        // Exactly what `run_dashboard` does with `take_roster`'s output.
        let (offerable, still_live) =
            roster::partition_live(vec![live_one.clone(), dead_one.clone()], &|short| {
                short == "dddd4444"
            });
        assert_eq!(offerable, vec![dead_one], "only the dead one is offered");
        let deferred_restore = still_live;

        on_quit(&[], &[], &deferred_restore, &requests_dir, &state, &repo);

        let slug = super::super::state::repo_slug(&repo);
        let written = roster::take_roster(&state, &slug, super::super::state::now_secs(), 999_999)
            .expect("a roster is still written");
        assert_eq!(
            written.panes,
            vec![live_one],
            "a held-back candidate is offered again next launch, not destroyed"
        );
    }

    /// M4: appending panes shifts a view-only selection down by the number
    /// appended; a selection on a pane keeps naming it.
    #[test]
    fn insert_fixup_shifts_a_view_only_selection_past_appended_panes() {
        // 2 panes; selection on the first view-only row (index 2); append 1.
        assert_eq!(insert_fixup(2, 3, 2), 3);
        // A selection on a pane (index 0 or 1) is unchanged.
        assert_eq!(insert_fixup(2, 3, 0), 0);
        assert_eq!(insert_fixup(2, 3, 1), 1);
        // Two appended shifts a view-only selection by two.
        assert_eq!(insert_fixup(2, 4, 3), 5);
        // Nothing appended is a no-op.
        assert_eq!(insert_fixup(2, 2, 3), 3);
    }
}
