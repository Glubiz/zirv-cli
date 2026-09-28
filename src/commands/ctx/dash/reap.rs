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

/// Which way [`fold_group`] moves a work group.
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

/// Issue #354 phase 2: the done-unread (`◆`) acknowledgement gate.
///
/// `Visibility::Unseen` is latched by a `Working -> Settled` transition and
/// only [`super::attention::mark_seen`] ever clears it, so whatever calls it
/// is asserting "an operator has actually looked at this session". Phase 1
/// called it on every focus change, which asserts something weaker and often
/// false: arrowing past a pane, or clicking it while a modal covers the whole
/// grid, cleared a badge nobody read.
///
/// The rule now is a *render* rule, not an input rule: the pane must be the
/// focused one, no overlay may be covering it, and it must be at its live
/// scroll position (a pane scrolled back into history is showing something
/// else entirely). [`ack_candidate`] decides that against one drawn frame;
/// this remembers the `(short, revision)` it qualified at and hands it back
/// exactly once, on the next tick, so the write happens off the render path.
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
    /// Pure: records what the frame just drawn qualifies for.
    pub(super) fn observe(&mut self, candidate: Option<(String, u64)>) {
        self.pending = candidate.filter(|key| !self.acked.contains(key));
    }

    /// Pure: latches an acknowledgement earned OFF the render path, returning
    /// the short id whose `mark_seen_io` is owed -- at most once per
    /// `(short, revision)`, exactly like [`Self::take_due`].
    ///
    /// Issue #354 phase 3: the render path's own rule requires the pane to be
    /// FOCUSED, which a retained ended row can never be -- there is no child
    /// left to type into. Opening the inspector on such a row is the operator
    /// reading it, and is the only way its `◆` can ever clear.
    pub(super) fn acknowledge(&mut self, candidate: Option<(String, u64)>) -> Option<String> {
        let (short, revision) = candidate?;
        self.acked
            .insert((short.clone(), revision))
            .then_some(short)
    }

    /// Pure: the short id whose `mark_seen_io` is now due, at most once per
    /// `(short, revision)`.
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

/// Pure: the `(short, revision)` opening the inspector on `row` qualifies for.
///
/// Keyed off the glyph the roster actually drew rather than the projection
/// alone, because a retained ended row's `◆` comes from `glyph_for`'s own
/// exit-code rule (a clean exit with `Visibility::Unseen`), which
/// `attention::project` maps to `Failed` and so would never match here.
///
/// Review of 9314156 (finding 1, HIGH): restricted to rows the render path
/// can NEVER acknowledge on its own -- an ended row (`exit_code`), or one
/// this dashboard owns no pane for. Done-unread clears only after the
/// operator actually views a pane, which means focus plus one unoccluded
/// render at live scroll; a live attached pane that is merely *selected* has
/// not been viewed, and opening the inspector on it used to clear its `◆`
/// anyway. Those rows are left to the render path's own rule.
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

/// Pure: `(focused, selected)` after the pane at `removed` has been taken out
/// of `panes`. An index past the removed one shifts down by one; `focused`
/// landing exactly on it goes to the first pane (the keyboard has to point
/// *somewhere*, and the pane that shifted into the slot is a session the
/// operator never asked to type into); `selected` landing on it stays put,
/// since it addresses the combined sidebar (panes plus view-only rows) and the
/// row that shifted up is the natural next thing to have the cursor on.
///
/// R2: reaping supersedes the earlier keep-every-pane-forever choice, which
/// bought index stability at the price of unbounded growth -- registry corpses
/// listed as `Live` by `zirv ctx sessions` (a `SessionGuard` was released only
/// at quit, so `send`/`nudge` "succeeded" against dead workers), leaked
/// sockets and `vt100` buffers, and live panes pushed past `Ctrl+A <digit>`
/// reach. Index stability is now maintained by this explicit fixup instead.
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

/// Review round 1 (R5): resolved against the PANE's own cwd, not the
/// dashboard's `repo`. Both adapters key a transcript on the directory the
/// session runs in -- claude by project slug, codex by the `cwd` its rollout's
/// `session_meta` records -- so a worktree-hosted pane priced off the root
/// repo read another pane's transcript, or none.
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

/// The budget sweep with its transcript read injected, so the throttle guarding
/// it is testable without a multi-megabyte transcript on disk.
///
/// A1-1: `usage_of` is a full `read_to_string` + parse of one pane's whole
/// transcript, plus an `adapters::select` on either side of it. That is disk
/// work, and disk work in this loop runs on [`FACTS_THROTTLE`] -- the same
/// ~1s cadence `DiskFacts` and the mail sweep use -- not on the render
/// loop's own 20-100 ticks a second.
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

/// 2026-09-06: the pane-side mirror of `exec::run_with`'s wall clock. A
/// delegation that asked for `--timeout-secs` used to hard-error rather than
/// spawn a pane at all; it spawns one now, and this is what makes the ceiling
/// real. Runs on the same [`FACTS_THROTTLE`] cadence as the token-budget
/// sweep beside it -- a wall clock measured to the second does not need the
/// render loop's tick rate -- and reports each stop exactly once, because
/// `Pane::enforce_deadline` disarms the deadline in the same step.
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

/// Settles this pane's reservations against what it actually spent, and --
/// 2026-09-06 -- writes the `log::Delegation` row for it.
///
/// The row belongs here rather than on the requesting side: `agent::run_with`
/// returns at `Dispatch::Answered` as soon as the dashboard acknowledges the
/// spawn, which is before the pane has run a single turn, so the requester
/// never learns this delegation's usage, exit code or model at all. Since
/// headless spawns were removed, a delegation made while any dashboard is
/// live is ALWAYS a pane -- so with nothing appended here,
/// `logs/delegations.jsonl` simply stopped growing and every cost line read
/// `$0.00`. One row per completed pane delegation, attributed to the
/// requester, carrying the same fields the inline supervised path writes.
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
        // Issue #301: `pane.budget_tokens()` is exactly the ceiling
        // `admit_child` reserved for this pane at spawn time
        // (`fulfill_spawn_request` sets both from the same `admit_child`
        // result), so settling here always releases exactly what was
        // reserved.
        let reserved = pane.budget_tokens().unwrap_or(0);
        let _ = super::group::settle_reservation(state, group_id, reserved, actual);
    }
    // Issue #358 (task T3): the provider-level reservation `fulfill_spawn_
    // request` took for this pane, regardless of whether it also belonged
    // to a work group -- settled with the same actual spend just computed
    // above, mirroring `agent::run_with`'s own headless completion path.
    if let Some(reservation_id) = pane.reservation_id() {
        // Must match `fulfill_spawn_request`'s own reserve exactly -- see
        // that function's Track C (#383) note for why this stays name-only.
        let provider = adapters::provider_for_agent_name(Some(pane.agent()));
        let _ = super::reservation::settle(state, provider, reservation_id, actual);
    }
}

/// Pure: `selected` after `new_pane_count - old_pane_count` panes were
/// appended to `panes`. `selected` indexes the combined sidebar (panes first,
/// then view-only registry rows), so appending a pane pushes every view-only
/// row -- and any selection sitting on one -- down by the number appended.
///
/// M4: the mirror of [`reap_fixup`] for insertion. Removal was fixed up;
/// insertion was not, so `fulfill_spawn_request`/`spawn_restored_pane` pushing
/// onto `panes` silently re-aimed a view-only selection (e.g. `Ctrl+A n`) at a
/// different session. A selection already on a pane (index below the old pane
/// count) keeps naming that same pane.
pub(super) fn insert_fixup(old_pane_count: usize, new_pane_count: usize, selected: usize) -> usize {
    let added = new_pane_count.saturating_sub(old_pane_count);
    if selected >= old_pane_count {
        selected + added
    } else {
        selected
    }
}

/// Pure: `selected` after the retained ended row at combined-roster index
/// `restored_row` was relaunched into a pane.
///
/// A1-1 review finding A1-2: restoring grows `panes` (every view-only and
/// retained row below shifts DOWN by the number of panes appended) and
/// shrinks `retained` (every row after the restored one shifts back UP by
/// one) in a single step, and `restore_ended_row` applied neither, so the
/// sidebar cursor silently re-aimed at a different session. The restored row
/// itself becomes the newest pane, so a cursor that was on it follows it
/// there rather than landing on whatever slid into its old slot.
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

/// Issue #209/v3 codex review finding 1: `reap_ended_panes` removes an ended
/// pane from `panes` (and reindexes `focused`/`selected`) in the same tick it
/// detects the exit -- well before `assemble_sidebar`/`assemble_footer_facts`
/// ever run downstream that tick. A `SidebarRow` with `RowState::Dead` is
/// therefore never actually observed by either: `panes` never contains an
/// `Ended` pane by the time rows are built from it. `LastExited` is the
/// dashboard's own record of the pane it just lost, kept only for as long as
/// there is nothing else to focus instead (`panes` is empty) -- once a new or
/// restored pane takes focus, `assemble_footer_facts` finds a real focused
/// row again and this becomes irrelevant until the next full reap, so it
/// never needs an explicit clear.
pub(super) struct LastExited {
    pub(super) harness: String,
    pub(super) exited_at: Instant,
}

/// Removes every pane whose child has exited, in place: each one is shut down
/// first (`Pane::finish_shutdown` releases the registry record, writer permit
/// and socket), announced into the header's notice channel, then
/// dropped along with its nudge queue, with `focused`/`selected` fixed up by
/// [`reap_fixup`].
///
/// Called once per tick, right after every pane has been drained and polled,
/// so `state()` is as fresh as it gets.
///
/// F4: every reaped pane's own exit code is recorded in `reaped_codes`, in
/// reap order. The dashboard's own exit status is a fold over that list
/// (`empty_exit_code`) once the last pane is gone: a dashboard whose sessions
/// all died badly used to exit 0 regardless, which is the same dishonest exit
/// `exec`/`wrap` are careful never to report.
///
/// `last_exited` records whichever pane this call reaps last, but only when
/// it leaves `panes` empty -- see [`LastExited`]'s own doc comment for why
/// that is exactly the condition under which the footer would otherwise have
/// nothing to describe.
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
    // A1-5: the module's own rule (`Notice`'s doc comment) is failures ->
    // the sticky `\u{26a0}` error channel, confirmations -> the transient
    // notice channel. A pane that exited 0 finished; routing it through
    // `push_error` pinned the warning glyph and burned one of five
    // `MAX_KEPT_ERRORS` slots a real failure needs. Returned rather than
    // pushed here so the reap path keeps its existing parameter list and the
    // one caller that owns a notice channel does the pushing.
    let mut confirmations = Vec::new();
    let mut index = 0;
    while index < panes.len() {
        let PaneState::Ended(code) = panes[index].state() else {
            index += 1;
            continue;
        };
        // Issue #330 (review finding 2): an exited pane whose reader channel
        // is not drained yet keeps its place for another tick. The vt100
        // budget is shared across panes now, so a pane can genuinely reach
        // its exit with its last lines still queued -- and reaping it here
        // would retire the row, drop the parser and take exactly the output
        // the operator needs to understand the exit with it. `drain_with_
        // budget` reports `more` only when it stopped on the budget rather
        // than on an empty channel, so this can hold a pane back for a tick
        // but never forever: the next drain that reaches the end of the
        // channel clears it.
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
        // Issue #354 phase 2: the row survives the pane, so everything it will
        // ever need is captured HERE -- before `finish_shutdown` below releases the
        // registry record the age comes from, and before the `Pane` itself is
        // dropped. Nothing about a finished worker can be re-derived a tick
        // later.
        let now_secs = super::state::now_secs();
        let ended_meta = EndedMeta {
            exit_code: code,
            exited_at: now_secs,
            age_secs: sessions::load_record(state, panes[index].short())
                .map(|record| now_secs.saturating_sub(record.started_at)),
        };
        // Review of 5c1b6c3, finding 2: `budget`/`writer` are read off the
        // LIVE pane here, through the very same helpers `build_pane_rows`
        // uses for a running one. They used to be hardcoded placeholders, so
        // a retained row's disclosure claimed nothing was ever known about a
        // worker's token usage or its write permit -- while the doc comment
        // on `EndedRow` promised the opposite.
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
        // Issue #349: the dashboard's own quiet-heuristic sync never sees this
        // pane again (it is about to leave `panes`), so the one authority that
        // can say the child is gone files it here instead -- a `Supervisor`
        // observation, the same rank `exec`/`wrap` use for a process exit.
        // Without it the retained row's cached status would still claim the
        // session was working. Review of 5c1b6c3, finding 1: a clean exit is
        // preceded by the `Settled` observation that latches `Unseen` -- see
        // [`reap_observations`].
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
        // Review finding (2026-09), finding 2a: captured before `pane` is
        // consumed below, so the worktree-reclaim check after this pane is
        // fully torn down still has its own cwd and label to work with.
        let pane_cwd = pane.cwd().to_path_buf();
        let pane_owns_cwd = pane.owns_cwd();
        let pane_short = pane.short().to_string();
        account_reaped_pane_spend(&pane, cfg, state, code);
        close_claimed_group(&pane, state);
        if index < queues.len() {
            queues.remove(index);
        }
        // L19: `finish_shutdown` above released the registry record immediately, but
        // `facts_cache.registry` is up to ~1s stale, so the dead session would
        // re-list as a view-only (nudge-targetable) row until the next refresh.
        // Remember its short and exclude it from the view-only rows until the
        // registry snapshot no longer carries it.
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
        // Review finding (2026-09), finding 2a: `agent::run_with`'s own
        // `--worktree` reclamation only ever runs for the HEADLESS fallback
        // path -- a dashboard-hosted worker pane's linked worktree is
        // handed off entirely (its own allocating process disarms its own
        // reclaim guard) and nothing else reclaimed it once the pane's
        // child exited. `pane` (and, via its own `Drop`, any writer permit
        // it held) is already gone by this point.
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

/// Review finding (2026-09), finding 2a: reclaims `cwd` if (and only if) the
/// pane OWNED it -- its spawn request carried `owns_workdir` because
/// `zirv agent --worktree` allocated it (review round 3: ownership travels
/// on the request, never inferred from the path, so an operator-named
/// `--workdir` that happens to live under `.zirv/worktrees/` is never
/// touched) -- and it is one of THIS repo's own agent-managed worktrees
/// (`agent::is_agent_managed_worktree`, the second guard). `None` for an
/// ordinary pane. Split out of [`reap_ended_panes`] so the checks and the
/// reclaim call are directly testable without spawning a real pane.
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
    // Issue #718 review finding (2026-09): threaded from the caller's own
    // resolved `cfg.worktree.idle_pool_max`, the same way `run_dashboard_
    // inner` threads `cfg.worktree.idle_ttl_secs` into `worktree::gc` --
    // a dashboard-hosted pane's own worktree reclaim now honors a repo/
    // operator override exactly like the headless `zirv ctx agent
    // --worktree --worktree-reuse` path (`agent::run_with`) already does,
    // instead of silently falling back to the built-in default.
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

/// Security review Finding 2 (2026-08-28): a coordinator pane's scope is
/// done the moment its own child exits -- successfully or not -- exactly as
/// `agent::run_with`'s completion path already treats a headless
/// coordinator's, and with the same two guards: only a `SubOrchestrator`
/// pane, and only for a group THIS pane actually claimed
/// (`group::claim_sub_orchestrator` is first-claim-wins, so a group some
/// other session owns must never be closed out from under it). Totals
/// survive: `group::close` only stamps `closed_at`, leaving
/// `admitted_children` and the terms a reviewer reads with `zirv ctx group
/// status` exactly as they were.
///
/// Best-effort throughout: a pane is being reaped either way, and a group
/// record that cannot be read or written is not a reason to fail that.
/// Deliberately NOT called from `on_quit`: a dashboard quitting kills its
/// panes mid-work rather than watching them finish, and such a group is
/// genuinely still open -- `group::is_abandoned` (claimed, unclosed, claimant
/// gone) is what surfaces it then, which is what the claim at spawn now makes
/// possible for a dash-spawned coordinator at all.
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

/// Called on every quit path, before any pane is torn down (shutdown --
/// quit-sequence, registry release, socket unpublish -- happens in the
/// caller right after this returns). Two things happen here, both
/// best-effort (the dashboard is exiting either way, and there is nothing
/// left to report a failure to):
///
/// 1. Writes this repo's own restore roster (`roster::write_roster`) from
///    every pane still alive, orchestrator included -- `RosterPane::role`
///    records which is which (`Pane::role`'s own `label()`), so
///    a later startup restore can filter the orchestrator back out itself
///    rather than this write having to guess which pane index is safe to
///    keep. A pane whose child has already exited is left out entirely: there
///    is nothing there to restore (R2).
/// 2. Removes the whole spawn-request directory this dashboard created at
///    startup (`requests_dir`'s own parent, `<dash_short>-<token>`, not just
///    the `requests` leaf, so no empty shell is left under `<state>/dash/`):
///    once this dashboard is gone, nothing should still be able to reach a
///    channel that nobody is polling any more.
///
/// F5: `unoffered` is whatever this launch took out of the previous roster and
/// never actually put to the operator -- the restore dialog still sitting
/// unanswered when the dashboard exited. `roster::take_roster` consumes on
/// read, so without writing those candidates back this quit's fresh roster
/// overwrote them and the sessions were lost for good, unoffered twice over.
///
/// G3: `deferred_restore` is the other pool of candidates a quit still owes
/// the next launch -- every restore candidate the pane cap forced this
/// session to skip when the operator confirmed the restore dialog
/// (`partition_restore_selection`'s own `deferred` half), independent of
/// whether that dialog is still open now. Merged in the same way and for the
/// same reason as `unoffered`: both are offers this launch consumed without
/// ever actually spawning them.
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
        // R2: a pane whose child already exited has nothing to restore.
        // Offering it back would spawn a fresh session for something the
        // operator watched finish, and would spend the next launch's pane
        // budget doing it.
        .filter(|pane| !matches!(pane.state(), PaneState::Ended(_)))
        .map(|pane| roster::RosterPane {
            agent: pane.agent().to_string(),
            session_id: pane.session_id().to_string(),
            // Security review Finding 6: the role this pane was actually
            // spawned with (`Pane::role`, issue #169), not a guess re-derived
            // from its verb. The verb form collapsed every non-chat pane to
            // `roster::ROLE_WORKER`, so a coordinator pane came back from a restore
            // demoted -- refused its own onward delegation by the depth cap,
            // and unable to close the group it still owned.
            role: pane.role().label().to_string(),
            short: pane.short().to_string(),
            title: pane.title().to_string(),
            // F3 (review, PR #116): persisted so a restore
            // (`spawn_restored_pane`) can hand a worker pane back its
            // report-back target and reminder-sent state -- without this,
            // every restored worker pane lost `report_to` for good, so
            // `report_back_reminder_sweep` could never remind it again.
            report_to: pane.report_to().map(str::to_string),
            report_reminder_sent: pane.report_reminder_sent(),
            settled_mail_sent: pane.settled_mail_sent,
            // Finding 6: and the group it belongs to, so the restore can put
            // it back inside the same one.
            work_group_id: pane.work_group_id().map(str::to_string),
            budget_tokens: pane.budget_tokens(),
            // Issue #160 finding 1, review round (2026-08-28): the launch
            // mode this pane was ACTUALLY spawned with (`Pane::launch_mode`),
            // so a restore can relaunch it on the same terms rather than
            // unconditionally pinning `Interactive` -- see `restored_pane_
            // turn_env`'s own doc comment.
            interactive: pane.launch_mode() == adapters::LaunchMode::Interactive,
            // Issue #249/#250 review (Fix 4): this pane's own server-verified
            // parent (`Pane::parent_session`), so a restore can hand it back
            // to `Pane::set_parent_session` and re-export it as `PARENT_
            // SESSION_ENV` -- without this, a quit/restore round-trip
            // silently downgraded a genuine worker's steering mail to peer.
            parent_session: pane.parent_session().map(str::to_string),
            // Issue #490 (roadmap N21 item A): which KIND of pane this was, so
            // the restore reopens it through `open_native_pane`/
            // `resolve_attach` rather than trying to relaunch an argv a native
            // pane never had. The generation is recorded beside it so a
            // restore that comes back on a different one is visible.
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

/// Pure: this quit's own live panes, plus every candidate this launch took out
/// of the previous roster and never offered, minus any duplicate.
///
/// Deduped on `session_id` because that is the identity a restore actually
/// resumes (`roster::restore_argv` feeds it to `resume_args`): a candidate that
/// somehow *is* live again must be written once, as the live pane, not twice.
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

/// Removes the whole capability-token directory this dashboard created for its
/// spawn-request channel -- `requests_dir`'s own parent
/// (`<state>/dash/<short>-<token>`), not just the `requests` leaf, so no empty
/// shell is left behind under `<state>/dash/`.
///
/// O7: shared by every path that leaves `run_dashboard` -- the quit path
/// (`on_quit`), the terminal-setup failures (`abort_setup`) and the very first
/// pane's own spawn failure. Only the first of the three used to clean up, so
/// a dashboard that failed to start leaked a directory per attempt, each still
/// holding a live capability token's name.
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
