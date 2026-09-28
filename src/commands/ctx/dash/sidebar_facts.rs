//! Sidebar row assembly and header/footer fact snapshots for the frame.
use super::*;

/// One dashboard-owned pane's row inputs, decoupled from `Pane` itself so
/// `assemble_sidebar` stays pure and testable without a real spawn.
pub(super) struct PaneRowMeta {
    /// `PromptRole::label`'s own spelling; `display_role` shortens it for the
    /// row's 8-column role field.
    pub(super) role: String,
    /// Issue #354: `Pane::launch_model` -- what the child was actually
    /// launched with, `None` when the argv pinned nothing.
    pub(super) model: Option<String>,
    /// `Pane::work_group_id`, the only source of group membership.
    pub(super) group_id: Option<String>,
    /// `Pane::parent_session`, for the `group` disclosure line.
    pub(super) parent: Option<String>,
    /// The `budget` disclosure line, pre-rendered from the pane's ceiling and
    /// its last measured usage -- both already cached, neither re-read here.
    pub(super) budget: String,
    /// The `writer` disclosure line: whether this pane holds the write
    /// permit, and for which checkout.
    pub(super) writer: String,
    pub(super) short: String,
    pub(super) harness: String,
    pub(super) state: ui::RowState,
    /// Issue #209/v3 codex review finding 5: `Pane::reachable()`, threaded
    /// through so the footer's supervision segment can render the truth
    /// instead of an assumed `supervised`.
    pub(super) supervised: bool,
    /// Issue #354 phase 2: `Some` for a **retained ended row** -- a completed
    /// pane whose `Pane` `reap_ended_panes` has already dropped but whose row
    /// the roster keeps. `None` for every live pane.
    pub(super) ended: Option<EndedMeta>,
}

/// Issue #354 phase 2: what a retained ended row knows that a live pane's row
/// does not. Frozen at the moment of the reap: the session's registry record
/// is released there, so nothing can be re-derived from it afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct EndedMeta {
    /// The child's own exit code. A nonzero one is `✗`; a clean one is `◆`
    /// until seen and `●` afterwards -- see `ui::glyph_for`.
    pub(super) exit_code: i32,
    /// When the pane exited, in epoch seconds -- what the `since` disclosure
    /// counts up from.
    pub(super) exited_at: u64,
    /// The row's age at the instant it exited, frozen: a finished worker's
    /// "how long did it run" must not keep ticking up after it stopped
    /// running. `None` when no registry record was found for it at reap time.
    pub(super) age_secs: Option<u64>,
}

/// Issue #354 phase 2: one completed pane's retained sidebar row.
///
/// A reaped pane used to vanish from the roster in the same tick its child
/// exited, which is exactly when an operator wants to see what happened to it.
/// The row survives the `Pane` -- glyph from the exit code, age frozen, role
/// and model retained -- until the cap below drops it or the dashboard exits.
/// It is deliberately *not* attached: it is selectable (so its disclosure can
/// be read) and never focusable, since there is no child left to type into.
#[derive(Debug, Clone)]
pub(super) struct EndedRow {
    pub(super) short: String,
    pub(super) role: String,
    pub(super) model: Option<String>,
    pub(super) harness: String,
    pub(super) group_id: Option<String>,
    pub(super) parent: Option<String>,
    /// The pane's last `budget`/`writer` disclosure values, captured before it
    /// was dropped rather than re-derived from a `Pane` that no longer exists.
    pub(super) budget: String,
    pub(super) writer: String,
    /// The checkout the pane ran in, captured for the same reason -- the
    /// inspector's `cwd` line and the menu's `open worktree` entry both read
    /// it, and there is no `Pane` left to ask.
    pub(super) cwd: String,
    /// Issue #354 phase 3: the very `spawnreq::SpawnRequest` that created
    /// this pane, kept so `restore`/`retry` can relaunch it VERBATIM through
    /// the existing `fulfill_spawn_request` machinery -- never a
    /// reconstructed argv (`Command Safety`). `None` for a pane this
    /// dashboard did not spawn from a request (the orchestrator itself, and
    /// a startup-restored pane), which is exactly what disables the two
    /// entries with `no spawn request kept`.
    pub(super) request: Option<spawnreq::SpawnRequest>,
    /// Who asked for that request, so a relaunch is attributed to the same
    /// lineage the original spawn was rather than to the dashboard itself.
    pub(super) requested_by: Option<String>,
    pub(super) meta: EndedMeta,
}

/// How many retained ended rows the roster keeps at once, oldest dropped
/// first. A long orchestration session can reap dozens of workers, and an
/// unbounded list would push every live pane off the bottom of the sidebar --
/// the exact failure the pre-#354 reap was introduced to avoid (see
/// `reap_fixup`'s own doc comment on R2). Eight is one screenful's worth of
/// recent history on the smallest dashboard-eligible terminal.
pub(super) const MAX_RETAINED_ENDED_ROWS: usize = 8;

/// Pure: the `budget` disclosure text for a pane -- `used / ceiling`, with
/// the shared placeholder for whichever half nothing has measured.
///
/// Review of 5c1b6c3, finding 2: shared by [`build_pane_rows`] (a live pane)
/// and [`reap_ended_panes`] (the retained row frozen at the reap), so the two
/// can never disagree about what a pane's budget line says.
pub(super) fn budget_text(used: Option<u64>, ceiling: Option<u64>) -> String {
    format!(
        "{} / {}",
        used.map(|v| v.to_string())
            .unwrap_or_else(|| style::PLACEHOLDER.into()),
        ceiling
            .map(|v| v.to_string())
            .unwrap_or_else(|| style::PLACEHOLDER.into())
    )
}

/// Pure: the `writer` disclosure text for a pane -- whether it holds the
/// write permit, and for which checkout. Shared for the same reason
/// [`budget_text`] is.
pub(super) fn writer_text(holds_permit: bool, cwd: &Path) -> String {
    format!(
        "{} \u{b7} {}",
        if holds_permit {
            "held"
        } else {
            style::PLACEHOLDER
        },
        cwd.display()
    )
}

/// Pure: the attention observations a reap owes the session it is retiring,
/// in the order they must be recorded.
///
/// Review of 5c1b6c3, finding 1: a CLEAN exit gets a `Settled` observation
/// FIRST whenever nothing had already settled the session, because
/// `attention::compose` latches `Visibility::Unseen` only on a genuine
/// `Working -> Settled` transition -- and a worker with no Stop hook that
/// works right up to a fast, clean exit never spends a tick `Settled` for the
/// quiet heuristic to observe (`PaneState` reports `Ended` the instant
/// `child_exit` is set, so the idle debounce never elapses). Without it the
/// retained row rendered `●` immediately and the operator was never told the
/// worker had finished. A nonzero exit is `✗` regardless of visibility, so it
/// gets the exit observation alone.
///
/// Review round 2, finding 2: the exit observation also asserts
/// `Attention::None`, which is what actually CLEARS a latch on the attention
/// axis. Every other variant of `Attention` is a latch that survives until
/// something positively says otherwise (`attention::compose` clears only
/// `Compacting`, and only implicitly), so a pane that `report_stalled_
/// compaction` latched `Stalled` and that then exited kept projecting
/// `Blocked(Stalled)` forever -- `zirv ctx status` never showed the exit, and
/// `zirv ctx wait` resolved for no target at all. A process that is gone is
/// blocked on nothing, whatever it was blocked on while it lived, so exit is
/// exactly the authority that may say so.
pub(super) fn reap_observations(
    prior: super::attention::Lifecycle,
    code: i32,
    at: u64,
    tail: &str,
) -> Vec<super::attention::Observation> {
    let mut observations = Vec::new();
    if code == 0 && prior != super::attention::Lifecycle::Settled {
        observations.push(
            super::attention::Observation::new(
                super::attention::Authority::Supervisor,
                "pane finished its work and exited cleanly",
                90,
                at,
            )
            .with_lifecycle(super::attention::Lifecycle::Settled),
        );
    }
    observations.push(
        super::attention::Observation::new(
            super::attention::Authority::Supervisor,
            if code == 0 {
                format!("pane exited with code {code}")
            } else {
                format!("pane exited with code {code}: {tail}")
            },
            90,
            at,
        )
        .with_lifecycle(super::attention::Lifecycle::Exited)
        .with_attention(super::attention::Attention::None),
    );
    observations
}

/// Pure: appends `row` to the retained list, dropping the oldest once the list
/// is over [`MAX_RETAINED_ENDED_ROWS`]. Split out so the cap is testable
/// without reaping a real pane.
pub(super) fn push_retained_ended(retained: &mut VecDeque<EndedRow>, row: EndedRow, cap: usize) {
    retained.push_back(row);
    while retained.len() > cap {
        retained.pop_front();
    }
}

/// Combines this dashboard's own panes (attached, in pane order) with every
/// OTHER live session in the registry that THIS SAME dashboard process
/// itself spawned (view-only, `attached: false`) -- so the sidebar shows
/// every session this dashboard is responsible for, not only the ones
/// currently attached as panes. A registry record whose `owner_pid` does not
/// match `dashboard_pid` (another, concurrently running dashboard's session)
/// or is `None` (a pre-ownership record, or a session registered outside any
/// dashboard -- `wrap`/`exec`/`loop`/`chat`) is excluded outright: this
/// dashboard has no more business showing it than it does attaching to it.
/// Deduped by short id -- a pane's own registry record is never listed a
/// second time as a view-only row. Dead/stale registry entries
/// (`Liveness::Stale`) are excluded outright: `sessions::list` already swept
/// them from disk, and a dashboard has nothing useful to attach to or nudge
/// there. `selected` indexes into the combined list this returns; `focused`
/// indexes into `panes` alone (see `ui::SidebarRow`'s own doc comment for
/// why the two are separate), and is simply not marked when it is out of
/// range -- an empty dashboard has nothing to focus. Pure: no I/O of its own
/// -- `registry` is whatever the caller already read via `sessions::list`,
/// and `dashboard_pid` is passed in rather than read via
/// `std::process::id()` here so tests can exercise foreign vs. own owners.
///
/// `now_secs` (`super::state::now_secs()`) is likewise passed in rather than
/// read here: every row's age is `now_secs - record.started_at` for the
/// registry record matching its own short id -- a pane's own record is
/// always in `registry` too (only the dedup above keeps it from being listed
/// a second time), so this is the one age source both row kinds share. `None`
/// only when no matching record exists at all, a race between a fresh spawn
/// and its own registration.
pub(super) fn assemble_sidebar(
    panes: &[PaneRowMeta],
    registry: &[(sessions::Record, sessions::Liveness)],
    scores: &ScoreMap,
    selected: usize,
    focused: usize,
    dashboard_pid: u32,
    now_secs: u64,
) -> Vec<ui::SidebarRow> {
    let own_shorts: HashSet<&str> = panes.iter().map(|p| p.short.as_str()).collect();
    let started_at: HashMap<&str, u64> = registry
        .iter()
        .map(|(record, _)| (record.short.as_str(), record.started_at))
        .collect();
    let age_of = |short: &str| started_at.get(short).map(|at| now_secs.saturating_sub(*at));

    // Issue #354 phase 2: the retained ended rows sit at the very END of the
    // roster -- after the view-only registry rows, not immediately after the
    // live panes. That keeps `reap_fixup`'s index arithmetic exactly right:
    // reaping still removes one row from the middle and shifts everything
    // after it down by one, and the row retained in its place is appended
    // where no existing selection points. A retained row that still carries a
    // work group is drawn under that group's header regardless
    // (`ui::roster_frame` gathers a group's members from the whole row list),
    // so only an ungrouped one actually sits at the bottom.
    let (live_panes, ended_panes): (Vec<&PaneRowMeta>, Vec<&PaneRowMeta>) =
        panes.iter().partition(|p| p.ended.is_none());
    let row_of = |p: &PaneRowMeta| ui::SidebarRow {
        role: display_role(&p.role).into(),
        model: p.model.clone(),
        group: p.group_id.as_ref().map(|id| ui::GroupRef {
            id: id.clone(),
            scope: style::PLACEHOLDER.into(),
            lead_short: panes
                .iter()
                .find(|lead| lead.group_id.as_ref() == Some(id) && lead.role == "sub-orchestrator")
                .or_else(|| panes.iter().find(|lead| lead.group_id.as_ref() == Some(id)))
                .map(|lead| lead.short.clone())
                .unwrap_or_default(),
        }),
        tree: ui::TreePos::Flat,
        disclosure: Vec::new(),
        short: p.short.clone(),
        harness: p.harness.clone(),
        // Issue #354 phase 2: a retained ended row's age is frozen at the
        // instant it exited. Its registry record was released by the same
        // reap that retained it, so `age_of` would report the placeholder
        // here anyway -- but frozen is the honest answer either way.
        age_secs: match &p.ended {
            Some(ended) => ended.age_secs,
            None => age_of(&p.short),
        },
        score: scores.get(&p.short).copied(),
        state: p.state,
        status: None,
        exit_code: p.ended.map(|e| e.exit_code),
        // A retained ended row is selectable but never focusable: there is
        // no child left to receive a keystroke.
        attached: p.ended.is_none(),
        selected: false,
        focused: false,
        supervised: p.supervised,
        // Phase 1 placeholders; `enrich_sidebar` refines `fact_state`/
        // `fact_since_secs` from the cached attention status and fills
        // `workflow`/`unread_mail` from this same throttled tick's disk
        // reads. A retained ended row's own fact is final already -- frozen
        // at the reap, the same way its `since` disclosure line is.
        fact_state: row_state_label(p.state).to_string(),
        fact_since_secs: p.ended.map(|e| now_secs.saturating_sub(e.exited_at)),
        workflow: None,
        unread_mail: 0,
        // Dash refresh PR2 placeholders, same convention as `workflow`/
        // `unread_mail` above: `enrich_sidebar` fills these from this same
        // throttled tick's seat/rollover-runtime reads and the flash
        // tracker.
        rollover_badge: None,
        flash: None,
    };
    let mut rows: Vec<ui::SidebarRow> = live_panes.iter().copied().map(row_of).collect();

    if let Some(row) = rows.get_mut(focused) {
        row.focused = true;
    }

    for (record, liveness) in registry {
        if *liveness != sessions::Liveness::Live {
            continue;
        }
        if record.owner_pid != Some(dashboard_pid) {
            continue;
        }
        if own_shorts.contains(record.short.as_str()) {
            continue;
        }
        rows.push(ui::SidebarRow {
            role: record
                .role
                .as_deref()
                .map(display_role)
                .unwrap_or(style::PLACEHOLDER)
                .into(),
            model: None,
            group: None,
            tree: ui::TreePos::Flat,
            disclosure: Vec::new(),
            short: record.short.clone(),
            harness: record.agent.clone(),
            age_secs: Some(now_secs.saturating_sub(record.started_at)),
            score: scores.get(&record.short).copied(),
            state: ui::RowState::Unknown,
            status: None,
            exit_code: None,
            attached: false,
            selected: false,
            focused: false,
            // No `Pane` to ask, and never `focused` -- see `SidebarRow::
            // supervised`'s own doc comment.
            supervised: true,
            fact_state: row_state_label(ui::RowState::Unknown).to_string(),
            fact_since_secs: None,
            workflow: None,
            unread_mail: 0,
            rollover_badge: None,
            flash: None,
        });
    }

    rows.extend(ended_panes.into_iter().map(row_of));

    if let Some(row) = rows.get_mut(selected) {
        row.selected = true;
        // Issue #354: the selected row's disclosure, in the spec's own key
        // order. Every value comes from something already in hand -- the
        // pane's own fields, the registry record, or a placeholder that
        // `enrich_sidebar` fills in from the throttled facts cache a moment
        // later. Nothing here reads the disk and nothing shells out: `branch`
        // in particular stays the placeholder rather than running git, which
        // would put a subprocess on the render path.
        let pane = panes.iter().find(|p| p.short == row.short);
        let state = row_state_label(row.state);
        // Dash refresh PR1: the fact block's own line 1 -- phase 1's plain
        // `RowState` word and elapsed time, the same facts the old `reason`/
        // `since` disclosure lines led with before `enrich_sidebar` composes
        // a richer word from the cached attention status a moment later. A
        // retained ended row's fact is already final: frozen at the reap,
        // exactly like its old `since` disclosure line was.
        match pane.and_then(|p| p.ended) {
            Some(ended) => {
                row.fact_state = "ended".to_string();
                row.fact_since_secs = Some(now_secs.saturating_sub(ended.exited_at));
            }
            None => {
                row.fact_state = state.to_string();
                row.fact_since_secs = row.age_secs;
            }
        }
        row.disclosure = vec![
            (
                "group".into(),
                format!(
                    "{} · parent {}",
                    row.group
                        .as_ref()
                        .map(|g| g.scope.as_str())
                        .unwrap_or(style::PLACEHOLDER),
                    pane.and_then(|p| p.parent.as_deref())
                        .unwrap_or(style::PLACEHOLDER)
                ),
            ),
            (
                "budget".into(),
                pane.map(|p| p.budget.clone())
                    .unwrap_or_else(|| style::PLACEHOLDER.into()),
            ),
            ("branch".into(), style::PLACEHOLDER.into()),
            (
                "writer".into(),
                pane.map(|p| p.writer.clone())
                    .unwrap_or_else(|| style::PLACEHOLDER.into()),
            ),
            (
                "since".into(),
                // Issue #354 phase 2: a retained ended row says how long ago
                // it exited and with what -- the two facts that are actually
                // still true about it. A live row keeps the placeholder
                // `enrich_sidebar` fills in from the cached attention status
                // (or, with none, from `DiskFacts::state_since`).
                match pane.and_then(|p| p.ended) {
                    Some(ended) => format!(
                        "exited {} \u{b7} exit {}",
                        style::format_age(now_secs.saturating_sub(ended.exited_at)),
                        ended.exit_code
                    ),
                    None => format!(
                        "{state} {} · started {} ago",
                        style::PLACEHOLDER,
                        row.age_secs
                            .map(style::format_age)
                            .unwrap_or_else(|| style::PLACEHOLDER.into())
                    ),
                },
            ),
            (
                "signal".into(),
                if row.supervised {
                    "socket bound"
                } else {
                    "unreachable"
                }
                .into(),
            ),
        ];
    }
    rows
}

/// Pure: `PromptRole::label`'s persisted spelling shortened to fit the row
/// contract's 8-column role field. Anything else -- a role written by a
/// future build, or one already short enough -- passes through untouched
/// rather than being truncated into something that reads as a different role.
pub(super) fn display_role(role: &str) -> &str {
    match role {
        "orchestrator" => "orch",
        "sub-orchestrator" => "sub-orch",
        other => other,
    }
}

/// Pure: `cwd`, `~`-shortened against `home` when `cwd` starts with it --
/// the pane header's own convention for its own `cwd` segment
/// (` {harness} ▸ {role} · {model} · {cwd}`). Passes `cwd` through
/// untouched when `home` is `None` or does not prefix it (a checkout
/// outside the operator's home, or a build that could not resolve one).
pub(super) fn shorten_home(cwd: &str, home: Option<&str>) -> String {
    match home {
        Some(home) if !home.is_empty() && cwd.starts_with(home) => {
            format!("~{}", &cwd[home.len()..])
        }
        _ => cwd.to_string(),
    }
}

/// Pure: the word a disclosure line uses for a row's state when the composed
/// attention model has nothing to say about it. Phase 2's [`lifecycle_word`]
/// is the richer answer whenever a `SessionStatus` exists; this stays the
/// fallback for a row that has never been observed by an issue #349 writer.
pub(super) fn row_state_label(state: ui::RowState) -> &'static str {
    match state {
        ui::RowState::Working => "working",
        ui::RowState::Idle => "idle",
        ui::RowState::Dead => "ended",
        ui::RowState::Unknown => "unknown",
    }
}

/// Pure: the word the `since` disclosure line leads with, from the composed
/// model's own lifecycle axis -- `waiting 1m · started 9m ago` in the approved
/// frame. Deliberately the LIFECYCLE, not the projection: "waiting" and
/// "working" are what an operator reads as elapsed-time-in-state, whereas the
/// projection (which folds attention in) is what the `reason` line says.
pub(super) fn lifecycle_word(lifecycle: super::attention::Lifecycle) -> &'static str {
    use super::attention::Lifecycle;
    match lifecycle {
        Lifecycle::Starting => "starting",
        Lifecycle::Working => "working",
        Lifecycle::Waiting => "waiting",
        Lifecycle::Settled => "idle",
        Lifecycle::Exited => "exited",
        Lifecycle::Unknown => "unknown",
    }
}

/// Pure: the word the `reason` disclosure line leads with -- the projection's
/// own name, with `Blocked`'s payload spelled out (that IS the reason, and
/// `blocked` on its own says nothing an operator can act on).
pub(super) fn projection_word(projection: super::attention::Projection) -> String {
    use super::attention::{Attention, Projection};
    match projection {
        Projection::Blocked(Attention::None) => "waiting".to_string(),
        Projection::Blocked(attention) => spaced_lowercase(&format!("{attention:?}")),
        other => other.label().to_string(),
    }
}

/// Pure: `WorkflowGate` -> `workflow gate`. The composed model's enums are
/// `CamelCase` on the wire; a sidebar reads in words.
pub(super) fn spaced_lowercase(camel: &str) -> String {
    let mut out = String::with_capacity(camel.len() + 2);
    for (i, ch) in camel.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            out.push(' ');
        }
        out.extend(ch.to_lowercase());
    }
    out
}

/// Dash refresh PR1: resolves ONE session's own bound workflow (`workflow_
/// id`) into the fact the sidebar/pane-header renders, replacing the old
/// repo-wide `active_workflow_summary` pointer every pane used to share.
///
/// Three outcomes, matching the bug this replaces (`WorkflowState::current()`
/// returns `None` once `current_step` is out of range -- previously papered
/// over as an empty step string):
/// - `Completed`: the fact reads `done` for 10 minutes after the run's own
///   state file was last written (`engine::state_mtime_secs`), then `None`.
///   Round 2 coordinator review, CONFIRMED: a state file this build could
///   not stat (`state_mtime_secs` returns `None`) is treated as NOT fresh
///   -- `None` right away -- never as "just written" (an `unwrap_or(now)`
///   would make `now.saturating_sub(now) == 0`, always inside the 10-minute
///   window, so a completed workflow's own "done" could never expire).
/// - Any other status with a valid current step (`WorkflowState::current()`
///   is `Some`): the step, its 1-based position, and whether the run is
///   `AwaitingApproval`.
/// - Anything else -- `current_step` out of range for a non-`Completed`
///   status, or the run could not be loaded at all (purged, malformed,
///   unknown id) -- `None`. Never a guessed or empty step.
pub(super) fn resolve_session_workflow(
    state: &StateDir,
    repo: &Path,
    workflow_id: &str,
    now: u64,
) -> Option<ui::SessionWorkflowFact> {
    let wf = workflow::engine::load(state, repo, workflow_id).ok()?;
    let total = wf.steps.len();
    if wf.status == workflow::engine::WorkflowStatus::Completed {
        let mtime = workflow::engine::state_mtime_secs(state, repo, workflow_id);
        if !completed_workflow_is_fresh(mtime, now) {
            return None;
        }
        return Some(ui::SessionWorkflowFact {
            kind: wf.kind.as_str().to_string(),
            step: "done".to_string(),
            index: total,
            total,
            awaiting_approval: false,
            completed: true,
        });
    }
    let step = wf.current()?;
    Some(ui::SessionWorkflowFact {
        kind: wf.kind.as_str().to_string(),
        step: step.id.clone(),
        index: wf.current_step + 1,
        total,
        awaiting_approval: wf.status == workflow::engine::WorkflowStatus::AwaitingApproval,
        completed: false,
    })
}

/// Pure: whether a `Completed` run's own "done" fact is still fresh, given
/// its state file's own last-write time (`engine::state_mtime_secs`) and
/// `now`.
///
/// Round 2 coordinator review, CONFIRMED: `None` (no state file, or one
/// this build could not stat) must NEVER be fresh. The bug this replaces
/// was `mtime.unwrap_or(now)`, which made `now.saturating_sub(now) == 0` --
/// always inside the 10-minute window -- so a completed workflow whose
/// state file had since been purged, or was simply unreadable, showed
/// "done" forever instead of fading like every other completed run.
pub(super) fn completed_workflow_is_fresh(mtime: Option<u64>, now: u64) -> bool {
    match mtime {
        Some(mtime) => now.saturating_sub(mtime) <= 600,
        None => false,
    }
}

/// Pure: one navigation action's effect on the `(selected, focused)` pair.
///
/// The split is the whole of F7. `selected` is the sidebar cursor over the
/// *combined* row list (panes plus view-only registry rows) and is what a
/// nudge is aimed at; `focused` is the pane whose grid is drawn and whose
/// child gets every un-prefixed keystroke, so it may only ever name a pane.
/// `prefix,Tab` and `prefix,<digit>` address panes, so they move both.
///
/// `prefix,Up`/`prefix,Down` move `selected` and then let `focused` **follow
/// it onto any row that is a pane** (see [`follow_focus`]): arrow navigation
/// that highlighted another session but could not switch to it was reported
/// as a bug, and switching panes is what the arrows are for. Walking onto a
/// view-only registry row still leaves the focused pane exactly where it was
/// -- that session is not attached to this dashboard and cannot receive the
/// keyboard -- rather than blanking the grid and swallowing all input the way
/// a single shared index did. The sidebar dims those rows so the difference
/// is visible (`ui::render_sidebar`).
///
/// Every index stays clamped to something addressable: an empty dashboard
/// (no panes at all) leaves both untouched.
pub(super) fn apply_navigation(
    action: DashAction,
    selected: usize,
    focused: usize,
    pane_count: usize,
    total_rows: usize,
) -> (usize, usize) {
    match action {
        // N2: a digit beyond the pane count is a no-op, not a jump to the
        // last pane. `Ctrl+A 7` on a two-pane dashboard is a mistyped `1`
        // far more often than it is a request for "whatever is last", and
        // silently retargeting it moved the keyboard out from under the
        // operator.
        DashAction::Switch(i) => {
            if i >= pane_count {
                (selected, focused)
            } else {
                (i, i)
            }
        }
        DashAction::NextPane => {
            if pane_count == 0 {
                (selected, focused)
            } else {
                let target = (focused + 1) % pane_count;
                (target, target)
            }
        }
        DashAction::SelectUp => {
            let next = selected.saturating_sub(1);
            (next, follow_focus(next, focused, pane_count))
        }
        DashAction::SelectDown => {
            if total_rows == 0 {
                (selected, focused)
            } else {
                let next = (selected + 1).min(total_rows - 1);
                (next, follow_focus(next, focused, pane_count))
            }
        }
        _ => (selected, focused),
    }
}

/// Pure: where `focused` ends up after the sidebar cursor moved to `selected`.
///
/// The combined sidebar puts this dashboard's own panes first and the
/// view-only registry rows after them, so `selected < pane_count` is exactly
/// "this row is an attached pane". A pane can take the keyboard, so focus
/// follows the cursor onto it; a view-only row cannot, so focus stays put and
/// the operator keeps typing into whatever pane they were already in.
pub(super) const fn follow_focus(selected: usize, focused: usize, pane_count: usize) -> usize {
    if selected < pane_count {
        selected
    } else {
        focused
    }
}

/// Pure: assembles `ui::HeaderFacts` from already-computed ingredients. Kept
/// separate from `FactsCache::refresh_if_due` (the impure disk-reading half)
/// and from the transient `errors`/`notices` channels' own storage, so the
/// header's own precedence rule -- a fresh notice shows over a sticky error,
/// never both at once -- is exercised without a state dir. Mirrors `ui`'s own
/// `HeaderFacts` field order.
pub(super) fn assemble_header_facts(
    sessions: usize,
    working: usize,
    needs_you: usize,
    error_count: usize,
    latest_error: Option<String>,
    notice: Option<String>,
) -> ui::HeaderFacts {
    ui::HeaderFacts {
        hints: ui::HintContext::default(),
        sessions,
        working,
        needs_you,
        error_count,
        latest_error,
        notice,
        // Issue #354 phase 4: set by the event loop right after this, the
        // same way the hint context is -- it is session state, not a fact
        // this assembly step has any way to know.
        tip: None,
    }
}

/// Pure: assembles `ui::FooterFacts` (issue #209/v3 §D) for the **focused**
/// pane (Q1) from already-computed ingredients, the same separation-of-
/// concerns `assemble_header_facts` keeps: the impure disk reads happen in
/// `FactsCache::refresh_if_due`, this only shapes what they already found.
///
/// `focused_row` is the sidebar row already marked `focused` in this tick's
/// `assemble_sidebar` output, reused rather than re-derived: it already
/// carries the harness, cached score, age and dead/alive state the footer
/// needs, and reusing it means the sidebar and the footer can never disagree
/// about which pane is focused or what its own facts are.
///
/// `None` means there is no attached pane at all right now -- an empty
/// dashboard, or (codex review finding 1) the tick right after the last one
/// exited: `reap_ended_panes` removes an `Ended` pane from `panes` in the
/// same tick it detects the exit, so `focused_row` can never actually carry
/// `RowState::Dead` in the live loop the way the sidebar's own glyph styling
/// still accounts for. `last_exited` (`(harness, age since it exited)`,
/// from `reap_ended_panes`'s own `LastExited`, only ever set when that
/// reap left `panes` empty) is what makes the dead-pane footer variant
/// reachable for exactly that case; with nothing focused and no exit to
/// report either, this is `ui::FooterFacts::None` and nothing draws.
///
/// `mail` (codex review finding 2) is the FOCUSED pane's own unread count
/// (`FactsCache::disk.mail_by_session`, looked up by its short id) -- never
/// the dashboard's own fixed launch identity's, which answers a different
/// question (see `MailMap`'s own doc comment).
/// Dash refresh PR2: `cfg.dash.motion` (`config::DashMotion`) as `ui::
/// Motion` -- `dash::ui` takes no config dependency of its own (see that
/// module's own doc comment), so this thin mapping is where the two meet.
pub(super) fn dash_motion_of(cfg: &CtxConfig) -> ui::Motion {
    match cfg.dash.motion {
        super::config::DashMotion::Full => ui::Motion::Full,
        super::config::DashMotion::Reduced => ui::Motion::Reduced,
    }
}

/// Review fix: `cached`'s own `pct`, but ONLY when it names the exact seat
/// (`short` + `generation`) `current` reads as live right now -- a pane's
/// registry short id survives a handover unchanged (`Pane::handover` never
/// re-registers), so `short` alone cannot tell an old seat from the new one
/// a rollover just put in its place; only `generation` advances. Pulled out
/// of the render loop as its own pure function so the identity check has a
/// test independent of the whole loop.
pub(super) fn seat_headroom_for_current(
    cached: Option<&SeatHeadroom>,
    current: Option<&seat::Seat>,
) -> Option<f64> {
    let cached = cached?;
    let current = current?;
    (current.short == cached.short && current.generation == cached.generation).then_some(cached.pct)
}

/// Review fix: which of `after`'s own sessions should flash for newly
/// arrived mail -- `None` (never flashes anything) on the first observation
/// (`seen_before: false`), since `before` is then `FactsCache`'s still-empty
/// starting map and every already-unread row would otherwise read as "just
/// arrived" (the same false-transition mistake the DoneUnread path avoids
/// for free, since ITS OWN `previous: Option<Projection>` genuinely means
/// "never sampled" when absent -- a plain `MailMap` has no such marker, so
/// this flag stands in for one).
pub(super) fn mail_flash_targets(
    before: &MailMap,
    after: &MailMap,
    seen_before: bool,
) -> Vec<String> {
    if !seen_before {
        return Vec::new();
    }
    after
        .iter()
        .filter(|(short, (broadcast, direct))| {
            let prior = before.get(*short).map(|(b, d)| b + d).unwrap_or(0);
            *broadcast + *direct > prior
        })
        .map(|(short, _)| short.clone())
        .collect()
}

/// Review fix: the "rolled over" toast text, if this observation earns one
/// -- `None` on the first observation (`seen_before: false`) even when
/// `current` is already `Committed`, since that settlement may predate this
/// dashboard process entirely (a prior session's rollover); the toast is
/// for a commit that happens WHILE this dashboard is watching, never one it
/// merely discovers on its first read.
pub(super) fn rollover_committed_toast(
    current: &Option<super::rollover_runtime::Settlement>,
    previous: &Option<super::rollover_runtime::Settlement>,
    seen_before: bool,
    source_agent: &str,
) -> Option<String> {
    if !seen_before || current == previous {
        return None;
    }
    let super::rollover_runtime::Settlement::Committed { generation, .. } = current.as_ref()?
    else {
        return None;
    };
    Some(format!(
        "\u{2913} rolled over from {source_agent} \u{b7} gen {generation}"
    ))
}

/// Dash refresh PR2: the JEV sidebar section's facts, off `jev::
/// usage_rollup`'s own 24h-windowed read (`JEV_SECTION_WINDOW_SECS`) --
/// `None` with every `[jev]` gate off, which is what hides the section
/// entirely. Gates enabled but no credential is the one-line `NoKey` state;
/// otherwise the top 3 sites by calls, bars relative to the busiest.
///
/// Session-scoped total follow-up: `jev-decisions.jsonl`/`jev-effects.jsonl`
/// are a SINGLE machine-wide file, written by every zirv process on the
/// machine across every repo -- without `sessions`, this used to fold every
/// OTHER session's rows in too, which is why the section could look like it
/// was not moving even though the calling session's own calls were landing:
/// a handful of new rows barely shift a total already carrying a whole
/// machine's unrelated history. `sessions` is `jev_session_snapshot`'s own
/// result -- every pane this dashboard has ever hosted, this run -- so the
/// section now reads as this session's own total.
pub(super) fn jev_section_fact(
    cfg: &CtxConfig,
    state: &StateDir,
    sessions: &BTreeSet<String>,
) -> Option<ui::JevSectionFact> {
    if !super::jev::any_gate_enabled(&cfg.jev) {
        return None;
    }
    if !super::jev::credential_present(cfg) {
        return Some(ui::JevSectionFact::NoKey {
            credential_env: super::jev::credential_env_name(cfg),
        });
    }
    let rollup = super::jev::usage_rollup(state, JEV_SECTION_WINDOW_SECS, Some(sessions));
    let now = super::state::now_secs();
    let total_calls: u64 = rollup.sites.values().map(|u| u.calls).sum();
    let total_errors: u64 = rollup.sites.values().map(|u| u.errors).sum();
    let cache_hit_rate = if total_calls > 0 {
        let hits: f64 = rollup
            .sites
            .values()
            .filter_map(|u| u.cache_hit_rate.map(|rate| rate * u.calls as f64))
            .sum();
        Some(hits / total_calls as f64)
    } else {
        None
    };
    let wait_p95_ms = rollup.sites.values().filter_map(|u| u.wall_ms_p95).max();
    let mut sites: Vec<(&String, u64)> = rollup
        .sites
        .iter()
        .map(|(site, usage)| (site, usage.calls))
        .collect();
    sites.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let busiest = sites.first().map(|(_, calls)| *calls).unwrap_or(0).max(1);
    let site_bars: Vec<ui::JevSiteBar> = sites
        .into_iter()
        .take(3)
        .map(|(name, calls)| {
            let target = ((calls as f64 / busiest as f64) * 6.0).clamp(0.0, 6.0);
            ui::JevSiteBar {
                name: name.clone(),
                calls,
                filled: target.round() as usize,
                eased_filled: target,
            }
        })
        .collect();
    // Click affordance follow-up: the errors dialog's own cached rows,
    // built in the SAME pass as everything else here (the 10s JEV refresh
    // cadence, never per frame or on click) -- see `ui::JevErrorRow`'s own
    // doc comment for why a click must never read `recent_errors` off disk
    // itself.
    let errors_detail: Vec<ui::JevErrorRow> = rollup
        .recent_errors
        .iter()
        .map(|e| ui::JevErrorRow {
            age_secs: now.saturating_sub(e.ts),
            site: e.site.clone(),
            reason: e.reason.clone(),
        })
        .collect();
    Some(ui::JevSectionFact::Active {
        calls: total_calls,
        cache_hit_rate,
        wait_p95_ms,
        errors: total_errors,
        latest_error_reason: rollup.latest_error_reason,
        last: rollup.last_call.map(|call| ui::JevLastLine {
            site: call.site,
            age_secs: now.saturating_sub(call.ts),
        }),
        sites: site_bars,
        errors_detail,
    })
}

/// Session-scoped total, review round: folds this tick's live pane session
/// ids into `sessions` (the dashboard's own grow-only running set -- see its
/// own doc comment where it is declared) and hands back a snapshot for
/// `jev::usage_rollup`'s own filter. Read once per JEV refresh
/// (`JEV_THROTTLE`, 10s) -- never per frame -- alongside `jev_section_
/// fact`'s own read.
///
/// A prior shape of this closed the set over `log::read_delegations`' own
/// parent chain (a `zirv agent` worker spawned by a worker, and so on) --
/// review round found that this could never actually grow the set past a
/// dashboard's own panes: a pane's `session_id()` is the FULL id
/// `jev::session_and_principal` reads back off `ZIRV_CTX_SESSION`
/// (confirmed identical for both adapters -- `claude.rs`'s own `register_
/// turn_signal` sets it from `session.id.to_string()`, and `build_turn_env`'s
/// own signal-less fallback pushes the same `session_id` string verbatim),
/// while `DelegationRow::parent_session` is stamped from `mail::
/// session_identity` (`sessions::short_id`, an 8-character prefix) -- so the
/// two could never match, and every worker this dashboard itself spawns is
/// already one of its own panes regardless, which is exactly what makes the
/// simpler grow-only set below sufficient on its own. Reading the
/// never-rotated delegation ledger in full every 10s was also needless
/// cost this removes.
///
/// A reaped/ended pane's session id, once added, is never removed -- its
/// own JEV rows keep counting toward the total after it finishes. What is
/// NOT covered: a quit/restore round trip starts a fresh, empty set (a
/// prior launch's session is not carried forward), and a headless worker
/// that never became a pane of THIS dashboard is never added at all.
pub(super) fn jev_session_snapshot(
    sessions: &mut BTreeSet<String>,
    panes: &[Pane],
) -> BTreeSet<String> {
    sessions.extend(panes.iter().map(|p| p.session_id().to_string()));
    sessions.clone()
}

/// One step of the footer rot track's own eased fill -- `prev` is `None`
/// exactly when there was nothing to ease FROM (no focused row last tick,
/// or it carried no score), in which case the track starts AT `target`
/// rather than easing up from zero.
pub(super) fn ease_toward_score(
    prev: Option<f64>,
    target: f64,
    dt_ms: u64,
    motion: ui::Motion,
) -> f64 {
    ui::ease_toward(prev.unwrap_or(target), target, dt_ms, motion)
}

/// Coordinator follow-up: one step of a keyed bar's own eased value --
/// LIMITS bars and JEV site bars share this cache-and-step pattern with
/// the rot track's own [`ease_toward_score`], just keyed by name since
/// several bars are ever on screen together. Starts AT `target` (no
/// climb-from-zero) the first time a given key is ever seen, the same
/// "nothing to ease FROM yet" rule `ease_toward_score` follows.
pub(super) fn ease_bar(
    cache: &mut HashMap<String, f64>,
    key: &str,
    target: f64,
    dt_ms: u64,
    motion: ui::Motion,
) -> f64 {
    let current = cache.get(key).copied().unwrap_or(target);
    let eased = ui::ease_toward(current, target, dt_ms, motion);
    cache.insert(key.to_string(), eased);
    eased
}

/// Coordinator follow-up: `fact`'s own site bars, eased the same way LIMITS
/// bars are -- a rendering-only clone. `facts_cache.disk.jev` itself is
/// NEVER mutated in place: it is the raw target from the last 10s refresh,
/// and easing it here would corrupt what the NEXT frame eases FROM.
pub(super) fn eased_jev_fact(
    fact: &ui::JevSectionFact,
    cache: &mut HashMap<String, f64>,
    dt_ms: u64,
    motion: ui::Motion,
    touched: &mut HashSet<String>,
) -> ui::JevSectionFact {
    let mut fact = fact.clone();
    if let ui::JevSectionFact::Active { sites, .. } = &mut fact {
        for site in sites.iter_mut() {
            let key = format!("jev:{}", site.name);
            // `site.eased_filled` still carries the RAW continuous target
            // (`jev_section_fact` seeds it that way, unrounded) -- the
            // cache eases toward that, never toward the already-rounded
            // `filled`.
            let target = site.eased_filled;
            site.eased_filled = ease_bar(cache, &key, target, dt_ms, motion);
            touched.insert(key);
        }
    }
    fact
}

/// Review fix: `rollover_sweep`'s own captured headroom, tagged with the
/// seat it was computed for -- `short` alone is not enough, since a pane's
/// registry short id survives a handover unchanged (`Pane::handover` never
/// re-registers); only `generation` actually advances when the seat swaps.
/// The render loop compares this against `DiskFacts::seat_full`'s own
/// current `(short, generation)` before ever handing the `pct` to
/// [`rollover_state`], so a reading computed for the seat BEFORE a rollover
/// never gets shown against the seat AFTER one.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct SeatHeadroom {
    pub(super) short: String,
    pub(super) generation: u64,
    pub(super) pct: f64,
}

/// Dash refresh PR2: this dashboard's own orchestrator seat's rollover
/// state, from the same two small JSON files (`seat`/`record`) read on the
/// facts-refresh cadence -- shared by the footer's `RolloverFooterFact` and
/// the sidebar's `RolloverBadge` (see [`rollover_footer_fact_of`]/
/// [`rollover_badge_of`]). `None` (nothing shown anywhere) when cross-
/// harness fallback or automatic orchestrator rollover is off, or this
/// dashboard has no seat at all yet.
///
/// Priority, most urgent first: a `Parked` settlement/phase (nothing could
/// take the seat) outranks a merely-`Pending` one (a candidate is already
/// lined up), which outranks the plain distance/soon reading -- and THAT
/// reading is `seat_headroom_pct`, exactly the `source_headroom_pct`
/// `rollover::evaluate` itself last computed for this seat (`rollover_
/// sweep`'s own out-parameter capture), never a separately estimated
/// value. Coordinator follow-up: the operator does not want the dashboard
/// guessing at a number the real trigger does not use -- with no
/// evaluation having produced one yet (`None`), the distance/soon segment
/// is hidden entirely rather than approximated.
pub(super) fn rollover_state(
    cfg: &CtxConfig,
    seat: Option<&seat::Seat>,
    record: Option<&super::rollover_runtime::Record>,
    seat_headroom_pct: Option<f64>,
) -> Option<RolloverState> {
    if !cfg.fallback.enabled || !cfg.auto_orchestrator_rollover() {
        return None;
    }
    let seat = seat?;
    if let Some(super::rollover_runtime::Settlement::Parked { until, .. }) =
        record.and_then(|r| r.settlement.as_ref())
    {
        return Some(RolloverState::Parked {
            harness: seat.agent.clone(),
            resets_at: *until,
        });
    }
    if let seat::Phase::Parked { until, .. } = seat.phase {
        return Some(RolloverState::Parked {
            harness: seat.agent.clone(),
            resets_at: until,
        });
    }
    if seat.pending.is_some() {
        return Some(RolloverState::Pending);
    }
    let floor = cfg.fallback.rollover_headroom_pct();
    let headroom = seat_headroom_pct?;
    if headroom <= floor + 10.0 {
        Some(RolloverState::Soon(floor, headroom))
    } else {
        Some(RolloverState::Distance(floor, headroom))
    }
}

/// [`rollover_state`]'s own verdict -- one type shared by the footer and
/// sidebar-badge conversions right below it, so the two can never disagree
/// about which state the seat is in.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum RolloverState {
    /// `(floor_pct, headroom_pct)`, comfortably clear of the floor.
    Distance(f64, f64),
    /// `(floor_pct, headroom_pct)`, within 10 points of the floor.
    Soon(f64, f64),
    Pending,
    Parked {
        harness: String,
        resets_at: u64,
    },
}

/// [`RolloverState`] as the footer's own [`ui::RolloverFooterFact`].
pub(super) fn rollover_footer_fact_of(state: &RolloverState) -> ui::RolloverFooterFact {
    match state {
        RolloverState::Distance(floor_pct, headroom_pct) => ui::RolloverFooterFact::Distance {
            floor_pct: *floor_pct,
            headroom_pct: *headroom_pct,
        },
        RolloverState::Soon(floor_pct, headroom_pct) => ui::RolloverFooterFact::Soon {
            floor_pct: *floor_pct,
            headroom_pct: *headroom_pct,
        },
        RolloverState::Pending => ui::RolloverFooterFact::Pending,
        RolloverState::Parked { harness, resets_at } => ui::RolloverFooterFact::Parked {
            harness: harness.clone(),
            resets_at: *resets_at,
        },
    }
}

/// [`RolloverState`] as the sidebar's own [`ui::RolloverBadge`] -- only the
/// two states a badge shows at all (spec's own words: "⤓ pending, ⏸
/// parked"); distance/soon have no badge of their own.
pub(super) fn rollover_badge_of(state: &RolloverState) -> Option<ui::RolloverBadge> {
    match state {
        RolloverState::Pending => Some(ui::RolloverBadge::Pending),
        RolloverState::Parked { .. } => Some(ui::RolloverBadge::Parked),
        RolloverState::Distance(..) | RolloverState::Soon(..) => None,
    }
}

pub(super) fn assemble_footer_facts(
    focused_row: Option<&ui::SidebarRow>,
    mail: Option<(usize, usize)>,
    // Dash refresh PR1: only the DEAD-pane variant still carries a workflow
    // segment (the alive footer's own workflow segment moved to the pane
    // header, which has nowhere to draw for a pane that no longer exists).
    workflow: Option<&workflow::ActiveWorkflowSummary>,
    last_exited: Option<(&str, Option<u64>)>,
    // Issue #310: whether the focused pane's stall latch is currently armed
    // (`DiskFacts::stalled`, looked up by the caller the same way `mail`
    // above is) -- see `FooterAliveFacts::stalled`'s own doc comment for how
    // this overrides the supervision segment.
    stalled: bool,
    // Dash refresh PR2: the rot track's own eased fill value, kept across
    // frames by the caller (`ease_toward`) -- `None` in lockstep with
    // `focused_row`'s own score (there is nothing to ease toward without a
    // cached score).
    eased_score: Option<f64>,
    // Dash refresh PR2: the orchestrator seat's own rollover facts, built by
    // the caller from the same seat/rollover-runtime reads the sidebar
    // badge uses -- `None` unless the focused pane IS the orchestrator seat
    // and rollover has something to say (see `RolloverFooterFact`'s own doc
    // comment for every hidden case).
    rollover: Option<ui::RolloverFooterFact>,
) -> ui::FooterFacts {
    let footer_workflow = match workflow {
        Some(wf) => ui::FooterWorkflow::Active {
            kind: wf.kind.to_string(),
            step: wf.step.clone(),
            gated: wf.awaiting_approval,
        },
        None => ui::FooterWorkflow::None,
    };

    let Some(row) = focused_row else {
        return match last_exited {
            Some((harness, exited_age_secs)) => ui::FooterFacts::Dead(ui::FooterDeadFacts {
                harness: harness.to_string(),
                exited_age_secs,
                workflow: footer_workflow,
            }),
            None => ui::FooterFacts::None,
        };
    };

    if row.state == ui::RowState::Dead {
        return ui::FooterFacts::Dead(ui::FooterDeadFacts {
            harness: row.harness.clone(),
            exited_age_secs: row.age_secs,
            workflow: footer_workflow,
        });
    }

    // N7's own broadcast/direct split collapses into one total here: the
    // mock's footer shows a single unlabeled number, unlike the wrap bar's
    // own richer `+`-suffixed segment (`chrome::status_bar`'s own `mail`).
    let unread_mail = mail
        .map(|(broadcast, direct)| broadcast + direct)
        .unwrap_or(0);

    ui::FooterFacts::Alive(ui::FooterAliveFacts {
        score: row.score,
        eased_score,
        unread_mail,
        // Issue #209/v3 codex review finding 5: `Pane::reachable()`, via
        // `SidebarRow::supervised` -- a pane whose turn-signal socket
        // failed to bind at spawn runs genuinely unsupervised, and the
        // footer now says so instead of assuming every alive pane is fine.
        supervised: row.supervised,
        stalled,
        rollover,
    })
}

/// Cached rot scores, keyed by session short id. An **absent** key is the
/// unknown case (`score::cached_score` returned `None`: no transcript yet, an
/// unreadable one, an unresolvable agent), which the sidebar renders as
/// `rot --`. Nothing here ever stores a placeholder zero.
pub(super) type ScoreMap = HashMap<String, u32>;

/// `mail::unread_counts`'s own `(broadcast, direct)`, keyed by session short
/// id -- issue #209/v3 codex review finding 2. `mail::unread_counts`'s
/// `direct` count is relative to a *particular session's own identity*
/// (`msg.to_session == session_short`), not the repo as a whole, so a single
/// `Option<(usize, usize)>` scoped to the dashboard's own launch identity
/// (`DiskFacts`'s old `mail` field, kept for whatever else eventually reads
/// it) cannot answer "how much mail is addressed to the *focused* pane" --
/// it can only ever answer that for the dashboard's own fixed identity.
/// Populated exactly like `ScoreMap`: every attached pane, by its own
/// `agent()`/`short()`, plus every live registry row this dashboard owns.
/// An absent key means mail is disabled, never a fabricated `(0, 0)`.
pub(super) type MailMap = HashMap<String, (usize, usize)>;

/// How often the header's own disk-backed facts (rot scores, mail,
/// memory-bank size) -- and the session registry the sidebar's view-only rows come
/// from -- are re-read. Mirrors wrap's own `BAR_THROTTLE`/`BarRuntime::
/// last_draw` pattern (`wrap.rs:1362`): the render loop polls every 50ms,
/// but nothing here needs a disk hit that often.
pub(super) const FACTS_THROTTLE: Duration = Duration::from_secs(1);

/// Dash refresh PR2: the JEV sidebar section's own cadence -- coarser than
/// [`FACTS_THROTTLE`] because the section rolls up a 24h window; nothing
/// about it needs second-level freshness.
pub(super) const JEV_THROTTLE: Duration = Duration::from_secs(10);

/// Dash refresh PR2: how far back the JEV sidebar section's own
/// `jev::usage_rollup` looks -- 24h (mock §03's own words), narrower than
/// `zirv ctx jev status`'s 7-day window.
pub(super) const JEV_SECTION_WINDOW_SECS: u64 = 24 * 60 * 60;

/// Pure: whether an action last performed at `last` is due again as of `now`,
/// given how often it may run (`interval`). Shared by the header facts refresh
/// pattern (`FactsCache::refresh_if_due`) and the mail sweep throttle (H3):
/// both are disk-backed housekeeping that must not run on the render loop's
/// own 50ms cadence.
pub(super) fn due(last: Instant, now: Instant, interval: Duration) -> bool {
    now.duration_since(last) >= interval
}

/// Issue #780: [`due`], but also advances `*last` to `now` whenever the
/// cadence comes due -- regardless of what the caller does next. Used where a
/// cheap cadence check gates a second, expensive check (e.g. `auto_rollover.
/// is_enabled()`'s two `stat`s): without advancing `*last` unconditionally, a
/// negative outcome of that second check (the switch found disabled) would
/// leave `*last` stale forever, so `due` alone would keep reporting "due" on
/// every subsequent tick and the expensive check would run every tick again.
pub(super) fn due_advancing(last: &mut Instant, now: Instant, interval: Duration) -> bool {
    let is_due = due(*last, now, interval);
    if is_due {
        *last = now;
    }
    is_due
}

/// How often [`handle_spawn_requests`] actually reads its intake directories.
/// One `read_dir` for the dashboard's shared channel plus one per live pane,
/// on a tick rate that reaches 100/s while the operator is typing, is a
/// per-keystroke directory scan per worker; a queued request is data sitting
/// on disk (see `handle_spawn_requests`' own doc comment), so a quarter of a
/// second of latency on picking it up is invisible next to the round trip the
/// requester is already waiting on.
pub(super) const SPAWN_REQUEST_POLL: Duration = Duration::from_millis(250);

/// Pure: whether this tick reads the spawn-request channels.
///
/// L17: forced when there are no panes left, whatever the throttle says. The
/// empty-exit decision runs immediately after the intake, and a request that
/// arrives on the very tick the last pane ends must still be seen -- otherwise
/// the dashboard exits first and the requester burns its ack timeout against a
/// channel nobody will ever poll again.
pub(super) fn spawn_intake_due(last: Instant, now: Instant, panes_empty: bool) -> bool {
    panes_empty || due(last, now, SPAWN_REQUEST_POLL)
}

/// Pure: the order one tick drains its panes in -- the focused pane first,
/// then every other pane round-robin from `start`.
///
/// Issue #330: the focused pane is the one the operator is looking at and
/// typing into, so it gets first call on the tick's parsing budget; `start`
/// rotates each tick so that whichever unfocused pane gets what is left over
/// changes, and none of them starves behind a noisier neighbour.
pub(super) fn drain_order(count: usize, focused: usize, start: usize) -> Vec<usize> {
    if count == 0 {
        return Vec::new();
    }
    let mut order = Vec::with_capacity(count);
    // `focused` can be one past the end for the length of a tick: a pane may
    // have ended since the last one, and the index is re-clamped further down
    // the loop, after this.
    if focused < count {
        order.push(focused);
    }
    let start = start % count;
    for step in 0..count {
        let idx = (start + step) % count;
        if idx != focused {
            order.push(idx);
        }
    }
    order
}

/// Spends one tick's shared vt100 budget ([`pane::DRAIN_BUDGET_BYTES`]) over
/// `count` panes in [`drain_order`], returning the indices that actually
/// produced output.
///
/// `drain_one(index, share)` returns `(any, used)`. Every pane is visited --
/// a visit is also how a pane's child exit is noticed and how a signal-less
/// pane's turn flags retire -- and nothing is ever dropped: what a pane could
/// not parse this tick stays queued for the next one.
///
/// Review finding 1: the focused pane gets first call on the budget, but a
/// capped one. Uncapped, a focused pane streaming faster than the whole
/// budget took all of it every tick, and the unfocused panes behind it never
/// drained at all -- their channels grew without bound and their quiescence
/// and turn-signal logic, which only ever runs off a drain, never saw
/// another byte. So every other pane keeps a reserved floor of
/// `budget / (2 * count)`, which is what the focused pane's share is reduced
/// by; a share an earlier pane leaves unspent flows to the ones behind it,
/// and anything still unspent at the end flows back to the focused pane, so a
/// firehose next to quiet neighbours still gets the whole tick's budget.
///
/// `drain_one` is a closure so the sharing itself is testable with plain byte
/// counters, without a pty child or a terminal.
pub(super) fn drain_shared_budget<F>(
    count: usize,
    focused: usize,
    start: usize,
    budget: usize,
    mut drain_one: F,
) -> Vec<usize>
where
    F: FnMut(usize, usize) -> (bool, usize),
{
    let order = drain_order(count, focused, start);
    if order.is_empty() {
        return Vec::new();
    }
    // Half the budget, split evenly, is what the unfocused panes are
    // guaranteed between them; the focused pane may spend the rest.
    let reserve = budget / (2 * count);
    let mut produced = Vec::new();
    let mut remaining = budget;
    for (position, idx) in order.iter().copied().enumerate() {
        // Still owed to the panes queued behind this one, and therefore not
        // this pane's to spend.
        let owed = reserve.saturating_mul(order.len() - position - 1);
        let (any, used) = drain_one(idx, remaining.saturating_sub(owed));
        if any {
            produced.push(idx);
        }
        remaining = remaining.saturating_sub(used);
    }
    // What the unfocused panes did not need goes back to the pane the
    // operator is actually watching.
    if remaining > 0 && focused < count {
        let (any, _used) = drain_one(focused, remaining);
        if any && !produced.contains(&focused) {
            produced.push(focused);
        }
    }
    produced
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;

    #[test]
    fn assemble_sidebar_marks_the_focused_pane_separately_from_the_selection() {
        let panes = vec![
            pane_row("aaa11111", "claude"),
            pane_row("bbb22222", "claude"),
        ];
        let registry = vec![(
            registry_record("ccc33333", "codex", Some(DASHBOARD_PID)),
            sessions::Liveness::Live,
        )];
        // Selection has walked onto the view-only row; focus is still pane 1.
        let rows = assemble_sidebar(&panes, &registry, &HashMap::new(), 2, 1, DASHBOARD_PID, 0);
        assert!(
            rows[2].selected,
            "the sidebar cursor is on the view-only row"
        );
        assert!(!rows[2].focused, "a view-only row can never be focused");
        assert!(rows[1].focused, "the focused pane is still marked as such");
        assert!(!rows[1].selected);
        assert!(!rows[0].focused && !rows[0].selected);
    }

    #[test]
    fn assemble_sidebar_lists_dashboard_panes_first_in_pane_order() {
        let panes = vec![
            pane_row("aaa11111", "claude"),
            pane_row("bbb22222", "claude"),
        ];
        let rows = assemble_sidebar(&panes, &[], &HashMap::new(), 0, 0, DASHBOARD_PID, 0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].short, "aaa11111");
        assert!(rows[0].attached);
        assert_eq!(rows[1].short, "bbb22222");
        assert!(rows[1].attached);
    }

    #[test]
    fn assemble_sidebar_appends_view_only_registry_rows_owned_by_this_dashboard() {
        let panes = vec![pane_row("aaa11111", "claude")];
        let registry = vec![(
            registry_record("ccc33333", "codex", Some(DASHBOARD_PID)),
            sessions::Liveness::Live,
        )];
        let rows = assemble_sidebar(&panes, &registry, &HashMap::new(), 0, 0, DASHBOARD_PID, 0);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].short, "ccc33333");
        assert!(!rows[1].attached, "a registry-only row is never attached");
        assert_eq!(rows[1].state, ui::RowState::Unknown);
    }

    #[test]
    fn assemble_sidebar_excludes_a_registry_record_owned_by_a_different_dashboard() {
        let panes = vec![pane_row("aaa11111", "claude")];
        let registry = vec![(
            registry_record("ccc33333", "codex", Some(DASHBOARD_PID + 1)),
            sessions::Liveness::Live,
        )];
        let rows = assemble_sidebar(&panes, &registry, &HashMap::new(), 0, 0, DASHBOARD_PID, 0);
        assert_eq!(
            rows.len(),
            1,
            "a session another dashboard spawned must not appear in this one's sidebar"
        );
    }

    #[test]
    fn assemble_sidebar_excludes_a_registry_record_with_no_owner() {
        let panes = vec![pane_row("aaa11111", "claude")];
        let registry = vec![(
            registry_record("ccc33333", "codex", None),
            sessions::Liveness::Live,
        )];
        let rows = assemble_sidebar(&panes, &registry, &HashMap::new(), 0, 0, DASHBOARD_PID, 0);
        assert_eq!(
            rows.len(),
            1,
            "a record with no owner_pid (pre-ownership build, or a session \
             registered outside any dashboard) must not appear"
        );
    }

    #[test]
    fn assemble_sidebar_dedupes_a_panes_own_registry_record() {
        let panes = vec![pane_row("aaa11111", "claude")];
        let registry = vec![(
            registry_record("aaa11111", "claude", Some(DASHBOARD_PID)),
            sessions::Liveness::Live,
        )];
        let rows = assemble_sidebar(&panes, &registry, &HashMap::new(), 0, 0, DASHBOARD_PID, 0);
        assert_eq!(
            rows.len(),
            1,
            "the pane's own registry record must not appear a second time"
        );
        assert!(rows[0].attached);
    }

    #[test]
    fn assemble_sidebar_excludes_stale_registry_entries() {
        let registry = vec![(
            registry_record("ddd44444", "codex", Some(DASHBOARD_PID)),
            sessions::Liveness::Stale,
        )];
        let rows = assemble_sidebar(&[], &registry, &HashMap::new(), 0, 0, DASHBOARD_PID, 0);
        assert!(
            rows.is_empty(),
            "a dead session must not appear as a view-only row"
        );
    }

    #[test]
    fn assemble_sidebar_marks_the_selected_index_in_the_combined_list() {
        let panes = vec![pane_row("aaa11111", "claude")];
        let registry = vec![(
            registry_record("ccc33333", "codex", Some(DASHBOARD_PID)),
            sessions::Liveness::Live,
        )];
        let rows = assemble_sidebar(&panes, &registry, &HashMap::new(), 1, 0, DASHBOARD_PID, 0);
        assert!(!rows[0].selected);
        assert!(rows[1].selected);
    }

    /// Every row's age is `now_secs - started_at` off the matching registry
    /// record -- the one age source both an attached pane and a view-only
    /// row share.
    #[test]
    fn assemble_sidebar_computes_each_rows_age_from_the_matching_registry_record() {
        let panes = vec![pane_row("aaa11111", "claude")];
        let mut record = registry_record("aaa11111", "claude", Some(DASHBOARD_PID));
        record.started_at = 100;
        let registry = vec![(record, sessions::Liveness::Live)];
        let rows = assemble_sidebar(&panes, &registry, &HashMap::new(), 0, 0, DASHBOARD_PID, 160);
        assert_eq!(rows[0].age_secs, Some(60));
    }

    /// No matching registry record at all (a race between a fresh spawn and
    /// its own registration) leaves the age unknown, never a fabricated 0.
    #[test]
    fn assemble_sidebar_leaves_age_unknown_with_no_matching_registry_record() {
        let panes = vec![pane_row("aaa11111", "claude")];
        let rows = assemble_sidebar(&panes, &[], &HashMap::new(), 0, 0, DASHBOARD_PID, 160);
        assert_eq!(rows[0].age_secs, None);
    }

    /// Issue #209/v3 §C: a pane's own cached score threads through by short
    /// id, both for this dashboard's own panes and for a view-only registry
    /// row it owns; a short id with no entry at all leaves it `None`, never
    /// a fabricated `0`.
    #[test]
    fn assemble_sidebar_threads_the_scores_map_through_by_short_id() {
        let panes = vec![
            pane_row("aaa11111", "claude"),
            pane_row("bbb22222", "codex"),
        ];
        let mut record = registry_record("ccc33333", "claude", Some(DASHBOARD_PID));
        record.started_at = 100;
        let registry = vec![(record, sessions::Liveness::Live)];
        let mut scores: ScoreMap = HashMap::new();
        scores.insert("aaa11111".to_string(), 47);
        scores.insert("ccc33333".to_string(), 12);

        let rows = assemble_sidebar(&panes, &registry, &scores, 0, 0, DASHBOARD_PID, 160);

        assert_eq!(rows[0].score, Some(47), "own pane, scored");
        assert_eq!(rows[1].score, None, "own pane, no cached score");
        assert_eq!(rows[2].score, Some(12), "view-only registry row, scored");
    }

    #[test]
    fn the_lifecycle_word_is_the_axis_the_since_line_counts_from() {
        use super::super::attention::Lifecycle;
        assert_eq!(lifecycle_word(Lifecycle::Waiting), "waiting");
        assert_eq!(lifecycle_word(Lifecycle::Working), "working");
        assert_eq!(lifecycle_word(Lifecycle::Settled), "idle");
        assert_eq!(lifecycle_word(Lifecycle::Exited), "exited");
        assert_eq!(spaced_lowercase("WriterConflict"), "writer conflict");
        assert_eq!(spaced_lowercase("Approval"), "approval");
    }

    /// Retained rows go LAST, after the view-only registry rows: a reap then
    /// still removes one row from the middle and shifts everything after it
    /// down by exactly one (`reap_fixup`), and the row retained in its place
    /// lands where no existing selection points.
    #[test]
    fn retained_ended_rows_sit_after_the_view_only_rows_so_reap_fixup_still_holds() {
        let panes = vec![
            pane_row("aaa11111", "claude"),
            ended_pane_row("bbb22222", 0, 600),
        ];
        let registry = vec![(
            registry_record("ccc33333", "claude", Some(DASHBOARD_PID)),
            sessions::Liveness::Live,
        )];
        let rows = assemble_sidebar(&panes, &registry, &HashMap::new(), 0, 0, DASHBOARD_PID, 900);
        let order: Vec<&str> = rows.iter().map(|r| r.short.as_str()).collect();
        assert_eq!(order, vec!["aaa11111", "ccc33333", "bbb22222"]);
        assert!(rows[0].focused, "focused still indexes the live pane block");
        assert!(!rows[2].attached);
        // And the retained row's own short is never re-listed as a view-only
        // registry row.
        let registry = vec![(
            registry_record("bbb22222", "claude", Some(DASHBOARD_PID)),
            sessions::Liveness::Live,
        )];
        let rows = assemble_sidebar(&panes, &registry, &HashMap::new(), 0, 0, DASHBOARD_PID, 900);
        assert_eq!(rows.len(), 2);
    }

    /// A clean exit is `◆` until it has been seen and `●` afterwards, and its
    /// `since` line is never overwritten by the cached status's own clock.
    #[test]
    fn a_cleanly_ended_retained_row_reads_done_unread_then_idle() {
        let panes = vec![ended_pane_row("bbb22222", 0, 600)];
        let mut rows = assemble_sidebar(&panes, &[], &HashMap::new(), 0, 0, DASHBOARD_PID, 900);
        let mut disk = DiskFacts::default();
        // What `reap_ended_panes` files: an `Exited` lifecycle, still unseen.
        let mut exited = done_unread_status(4);
        exited.lifecycle = super::super::attention::Lifecycle::Exited;
        disk.attention.insert("bbb22222".into(), exited.clone());
        enrich_sidebar(&mut rows, &disk, 900);
        assert_eq!(ui::glyph_for(&rows[0]), ui::Glyph::DoneUnread);
        let since = |rows: &[ui::SidebarRow]| {
            rows[0]
                .disclosure
                .iter()
                .find(|(k, _)| k == "since")
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(since(&rows), "exited 5m \u{b7} exit 0");

        let mut seen = exited;
        seen.visibility = super::super::attention::Visibility::Seen;
        disk.attention.insert("bbb22222".into(), seen);
        let mut rows = assemble_sidebar(&panes, &[], &HashMap::new(), 0, 0, DASHBOARD_PID, 900);
        enrich_sidebar(&mut rows, &disk, 900);
        assert_eq!(ui::glyph_for(&rows[0]), ui::Glyph::Idle);
    }

    /// Review of 5c1b6c3, finding 1: a worker that is `Working` right up to a
    /// fast, clean exit -- no Stop hook, no other authority, no intervening
    /// `Settled` tick for the quiet heuristic to observe -- must still latch
    /// `Unseen`, so its retained row reads `◆` and the operator is told it
    /// finished. Before the fix `reap_ended_panes` recorded only `Exited`,
    /// which is not a `Working -> Settled` transition, and the row went
    /// straight to `●`.
    #[test]
    fn a_fast_clean_exit_from_working_still_latches_done_unread() {
        use super::super::attention::{Lifecycle, Observation, Visibility, compose};
        // Exactly what the quiet heuristic files for a busy pane, and nothing
        // else -- the reported failure case.
        let working = compose(
            None,
            &[Observation::new(
                super::super::attention::Authority::QuietHeuristic,
                "pane is producing output",
                40,
                100,
            )
            .with_lifecycle(Lifecycle::Working)],
            100,
        );
        assert_eq!(working.lifecycle, Lifecycle::Working);

        let mut status = working.clone();
        for observation in reap_observations(status.lifecycle, 0, 200, "") {
            status = compose(Some(&status), std::slice::from_ref(&observation), 200);
        }
        assert_eq!(status.lifecycle, Lifecycle::Exited);
        assert_eq!(
            status.visibility,
            Visibility::Unseen,
            "a clean exit out of Working has to latch Unseen"
        );

        // ... and that is what the roster actually draws for the retained row.
        let panes = vec![ended_pane_row("bbb22222", 0, 190)];
        let mut rows = assemble_sidebar(&panes, &[], &HashMap::new(), 0, 0, DASHBOARD_PID, 900);
        let mut disk = DiskFacts::default();
        disk.attention.insert("bbb22222".into(), status.clone());
        enrich_sidebar(&mut rows, &disk, 900);
        assert_eq!(ui::glyph_for(&rows[0]), ui::Glyph::DoneUnread);

        // It stays `◆` until it is acknowledged -- which, for a row that can
        // never be focused, only opening the inspector on it can do, once per
        // revision.
        let mut ack = DoneUnreadAck::default();
        let candidate = inspect_ack_candidate(&rows[0]);
        assert_eq!(
            candidate,
            Some(("bbb22222".to_string(), status.revision)),
            "the inspector's own acknowledgement path must see this row"
        );
        assert_eq!(
            ack.acknowledge(candidate.clone()),
            Some("bbb22222".to_string())
        );
        assert_eq!(
            ack.acknowledge(candidate),
            None,
            "the same revision is never acknowledged twice"
        );
        disk.attention.insert(
            "bbb22222".into(),
            super::super::attention::mark_seen(status),
        );
        let mut rows = assemble_sidebar(&panes, &[], &HashMap::new(), 0, 0, DASHBOARD_PID, 900);
        enrich_sidebar(&mut rows, &disk, 900);
        assert_eq!(ui::glyph_for(&rows[0]), ui::Glyph::Idle);
    }

    /// A nonzero exit is `✗` whatever its visibility, so it gets the exit
    /// observation alone -- and a session something already settled is not
    /// re-settled behind that authority's back.
    #[test]
    fn a_reap_only_settles_a_clean_exit_that_nothing_settled_already() {
        use super::super::attention::Lifecycle;
        let clean = reap_observations(Lifecycle::Working, 0, 5, "");
        assert_eq!(
            clean.iter().map(|o| o.lifecycle).collect::<Vec<_>>(),
            vec![Some(Lifecycle::Settled), Some(Lifecycle::Exited)]
        );
        for (prior, code) in [
            (Lifecycle::Working, 1),
            (Lifecycle::Settled, 0),
            (Lifecycle::Settled, 3),
        ] {
            let observations = reap_observations(prior, code, 5, "fatal: terminal disconnected");
            if code != 0 {
                assert!(
                    observations[0]
                        .evidence
                        .contains("fatal: terminal disconnected")
                );
            }
            assert_eq!(
                observations.iter().map(|o| o.lifecycle).collect::<Vec<_>>(),
                vec![Some(Lifecycle::Exited)],
                "{prior:?}/{code}"
            );
        }
    }

    /// Review of 5c1b6c3, finding 2: a retained row keeps the budget text and
    /// writer state its pane had, not a pair of placeholders -- through the
    /// very same helpers the live row uses, so the two can never drift.
    #[test]
    fn a_retained_row_keeps_its_panes_last_budget_and_writer_state() {
        assert_eq!(budget_text(Some(40_000), Some(200_000)), "40000 / 200000");
        assert_eq!(
            budget_text(None, Some(200_000)),
            format!("{} / 200000", style::PLACEHOLDER)
        );
        assert_eq!(
            budget_text(Some(40_000), None),
            format!("40000 / {}", style::PLACEHOLDER)
        );
        let cwd = Path::new("D:/repo");
        assert_eq!(writer_text(true, cwd), "held \u{b7} D:/repo");
        assert_eq!(
            writer_text(false, cwd),
            format!("{} \u{b7} D:/repo", style::PLACEHOLDER)
        );

        // And the retained row carries them into its own disclosure rather
        // than reporting "nothing was ever known about this worker".
        let retained = EndedRow {
            short: "bbb22222".into(),
            role: "worker".into(),
            model: None,
            harness: "claude".into(),
            group_id: None,
            parent: None,
            budget: budget_text(Some(40_000), Some(200_000)),
            writer: writer_text(true, cwd),
            cwd: cwd.display().to_string(),
            request: None,
            requested_by: None,
            meta: EndedMeta {
                exit_code: 0,
                exited_at: 600,
                age_secs: Some(300),
            },
        };
        let mut queue: VecDeque<EndedRow> = VecDeque::new();
        push_retained_ended(&mut queue, retained, MAX_RETAINED_ENDED_ROWS);
        let metas = build_pane_rows(&[], &queue);
        let rows = assemble_sidebar(&metas, &[], &HashMap::new(), 0, 0, DASHBOARD_PID, 900);
        let value = |key: &str| {
            rows[0]
                .disclosure
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        assert_eq!(value("budget"), "40000 / 200000");
        assert_eq!(value("writer"), "held \u{b7} D:/repo");
    }

    /// The retained list is capped, oldest dropped first, so a long session
    /// cannot push every live pane off the bottom of the sidebar.
    #[test]
    fn retained_ended_rows_are_capped_oldest_first() {
        let mut retained: VecDeque<EndedRow> = VecDeque::new();
        for i in 0..MAX_RETAINED_ENDED_ROWS + 4 {
            push_retained_ended(
                &mut retained,
                EndedRow {
                    short: format!("sess{i:04}"),
                    role: "worker".into(),
                    model: None,
                    harness: "claude".into(),
                    group_id: None,
                    parent: None,
                    budget: style::PLACEHOLDER.into(),
                    writer: style::PLACEHOLDER.into(),
                    cwd: String::new(),
                    request: None,
                    requested_by: None,
                    meta: EndedMeta {
                        exit_code: 0,
                        exited_at: i as u64,
                        age_secs: Some(10),
                    },
                },
                MAX_RETAINED_ENDED_ROWS,
            );
        }
        assert_eq!(retained.len(), MAX_RETAINED_ENDED_ROWS);
        assert_eq!(retained.front().unwrap().short, "sess0004");
        assert_eq!(retained.back().unwrap().short, "sess0011");
        // And they are rows, not panes: the final-pane auto-exit reads the
        // live pane vector alone, so a roster full of retained rows still
        // closes the dashboard when the last real pane goes.
        assert_eq!(
            build_pane_rows(&[], &retained).len(),
            MAX_RETAINED_ENDED_ROWS
        );
        assert!(should_exit_empty(0, false));
    }

    // -- attention reads stay on the facts cadence -------------------------

    #[test]
    fn attention_row_shorts_covers_every_drawable_row_once() {
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
                cwd: String::new(),
                request: None,
                requested_by: None,
                meta: EndedMeta {
                    exit_code: 0,
                    exited_at: 1,
                    age_secs: None,
                },
            },
            MAX_RETAINED_ENDED_ROWS,
        );
        let registry = vec![
            // Already a pane: never listed twice.
            (
                registry_record("aaa11111", "claude", Some(DASHBOARD_PID)),
                sessions::Liveness::Live,
            ),
            (
                registry_record("ccc33333", "claude", Some(DASHBOARD_PID)),
                sessions::Liveness::Live,
            ),
            // Another dashboard's session, and a stale one: neither is drawn,
            // so neither is read.
            (
                registry_record("ddd44444", "claude", Some(DASHBOARD_PID + 1)),
                sessions::Liveness::Live,
            ),
            (
                registry_record("eee55555", "claude", Some(DASHBOARD_PID)),
                sessions::Liveness::Stale,
            ),
        ];
        let shorts = attention_row_shorts(
            &["aaa11111".to_string()],
            &retained,
            &registry,
            DASHBOARD_PID,
        );
        assert_eq!(shorts, vec!["aaa11111", "bbb22222", "ccc33333"]);
    }

    #[test]
    fn assemble_header_facts_carries_the_session_counts_through() {
        let facts = assemble_header_facts(5, 2, 1, 0, None, None);
        assert_eq!(facts.sessions, 5);
        assert_eq!(facts.working, 2);
        assert_eq!(facts.needs_you, 1);
        assert_eq!(facts.error_count, 0);
        assert_eq!(facts.latest_error, None);
        assert_eq!(facts.notice, None);

        let facts =
            assemble_header_facts(1, 0, 0, 3, Some("mail send: disk full".to_string()), None);
        assert_eq!(facts.error_count, 3);
        assert_eq!(facts.latest_error.as_deref(), Some("mail send: disk full"));
    }

    #[test]
    fn assemble_header_facts_carries_the_notice_through() {
        let facts = assemble_header_facts(
            1,
            0,
            0,
            0,
            None,
            Some("spawned claude as wrk-2".to_string()),
        );
        assert_eq!(facts.notice.as_deref(), Some("spawned claude as wrk-2"));
    }

    // Issue #209/v3 §D: `assemble_footer_facts`.

    fn focused_alive_row(score: Option<u32>) -> ui::SidebarRow {
        focused_alive_row_supervised(score, true)
    }

    fn focused_alive_row_supervised(score: Option<u32>, supervised: bool) -> ui::SidebarRow {
        ui::SidebarRow {
            role: "worker".into(),
            model: None,
            group: None,
            tree: ui::TreePos::Flat,
            disclosure: Vec::new(),
            short: "aaa11111".to_string(),
            harness: "claude".to_string(),
            age_secs: Some(90),
            score,
            state: ui::RowState::Idle,
            status: None,
            exit_code: None,
            attached: true,
            selected: false,
            focused: true,
            supervised,
            fact_state: "idle".into(),
            fact_since_secs: Some(90),
            workflow: None,
            unread_mail: 0,
            rollover_badge: None,
            flash: None,
        }
    }

    // Coordinator follow-up: `rollover_state` must show the exact
    // `source_headroom_pct` `rollover::evaluate` computed, never a separate
    // estimate, and must hide the distance/soon segment entirely (not
    // approximate one) when no evaluation has produced a reading yet.

    fn test_seat(agent: &str, phase: seat::Phase, pending: Option<seat::Pending>) -> seat::Seat {
        seat::Seat {
            short: "orch0001".to_string(),
            session: "s".to_string(),
            generation: 1,
            agent: agent.to_string(),
            model: None,
            provider: "anthropic".to_string(),
            role: "orchestrator".to_string(),
            pinned: false,
            phase,
            visited: Vec::new(),
            last_rollover_at: None,
            rollover_failures: 0,
            failed_rollover_observed_at: None,
            pending,
            displaced: None,
            created_at: 0,
            updated_at: 0,
            runtime: Default::default(),
        }
    }

    fn rollover_test_cfg() -> CtxConfig {
        let mut cfg = CtxConfig::default();
        cfg.fallback.enabled = true;
        cfg.fallback.auto_orchestrator_rollover = Some(true);
        cfg.fallback.order = vec!["claude".to_string(), "codex".to_string()];
        cfg
    }

    #[test]
    fn rollover_state_hides_the_distance_segment_with_no_evaluation_yet() {
        let cfg = rollover_test_cfg();
        let seat = test_seat("claude", seat::Phase::Idle, None);
        assert_eq!(
            rollover_state(&cfg, Some(&seat), None, None),
            None,
            "no rollover::evaluate has run yet -- never fabricate a headroom reading"
        );
    }

    #[test]
    fn rollover_state_distance_and_soon_use_the_evaluations_own_headroom() {
        let cfg = rollover_test_cfg();
        let seat = test_seat("claude", seat::Phase::Idle, None);
        // floor defaults to `predictive_headroom_pct` (20.0).
        assert_eq!(
            rollover_state(&cfg, Some(&seat), None, Some(59.0)),
            Some(RolloverState::Distance(20.0, 59.0))
        );
        assert_eq!(
            rollover_state(&cfg, Some(&seat), None, Some(24.0)),
            Some(RolloverState::Soon(20.0, 24.0)),
            "within 10 points of the floor is soon, not distance"
        );
    }

    #[test]
    fn rollover_state_pending_and_parked_never_read_the_headroom_at_all() {
        let cfg = rollover_test_cfg();
        let pending_seat = test_seat(
            "claude",
            seat::Phase::Idle,
            Some(seat::Pending {
                cause: seat::Cause::Manual,
                since: 0,
            }),
        );
        // A `None` headroom would hide a plain distance reading, but
        // pending/parked never consult it in the first place.
        assert_eq!(
            rollover_state(&cfg, Some(&pending_seat), None, None),
            Some(RolloverState::Pending)
        );
        let parked_seat = test_seat(
            "claude",
            seat::Phase::Parked {
                until: 100,
                window: "5h".to_string(),
                reason: "no headroom anywhere".to_string(),
                since: 0,
            },
            None,
        );
        assert_eq!(
            rollover_state(&cfg, Some(&parked_seat), None, None),
            Some(RolloverState::Parked {
                harness: "claude".to_string(),
                resets_at: 100,
            })
        );
    }

    #[test]
    fn rollover_state_hidden_when_fallback_or_auto_rollover_is_off() {
        let seat = test_seat("claude", seat::Phase::Idle, None);
        let mut cfg = rollover_test_cfg();
        cfg.fallback.enabled = false;
        assert_eq!(rollover_state(&cfg, Some(&seat), None, Some(59.0)), None);

        let mut cfg = rollover_test_cfg();
        cfg.fallback.auto_orchestrator_rollover = Some(false);
        assert_eq!(rollover_state(&cfg, Some(&seat), None, Some(59.0)), None);
    }

    // Coordinator follow-up: LIMITS/JEV bar easing (`ease_bar`).

    #[test]
    fn ease_bar_starts_at_target_for_a_never_seen_key_then_converges() {
        let mut cache: HashMap<String, f64> = HashMap::new();
        let first = ease_bar(&mut cache, "claude:5h", 80.0, 0, ui::Motion::Full);
        assert_eq!(
            first, 80.0,
            "nothing to ease FROM yet -- starts at the target"
        );

        // The target drops (a fresh, lower reading); repeated small steps
        // converge toward it rather than jumping.
        let mut value = first;
        for _ in 0..30 {
            value = ease_bar(&mut cache, "claude:5h", 20.0, 20, ui::Motion::Full);
        }
        assert!(
            (value - 20.0).abs() < 1.0,
            "must have converged after 30 steps of 20ms: {value}"
        );
    }

    #[test]
    fn ease_bar_reduced_motion_snaps_and_keys_never_cross_talk() {
        let mut cache: HashMap<String, f64> = HashMap::new();
        ease_bar(&mut cache, "claude:5h", 80.0, 0, ui::Motion::Full);
        assert_eq!(
            ease_bar(&mut cache, "claude:5h", 20.0, 20, ui::Motion::Reduced),
            20.0,
            "reduced motion snaps to the target outright"
        );
        // A different key never inherits "claude:5h"'s own cached value.
        assert_eq!(
            ease_bar(&mut cache, "codex:wk", 50.0, 0, ui::Motion::Full),
            50.0
        );
    }

    // Review fix: stale headroom after a handover (`seat_headroom_for_current`).

    #[test]
    fn seat_headroom_for_current_matches_only_the_exact_seat_identity() {
        let cached = SeatHeadroom {
            short: "orch0001".to_string(),
            generation: 2,
            pct: 41.0,
        };
        let same_seat = test_seat("claude", seat::Phase::Idle, None);
        assert_eq!(
            seat_headroom_for_current(Some(&cached), Some(&same_seat)),
            None,
            "test_seat's own generation (1) does not match the cached one (2)"
        );

        let mut matching_seat = test_seat("codex", seat::Phase::Idle, None);
        matching_seat.short = "orch0001".to_string();
        matching_seat.generation = 2;
        assert_eq!(
            seat_headroom_for_current(Some(&cached), Some(&matching_seat)),
            Some(41.0)
        );

        // A rollover bumped the generation at the SAME short address -- the
        // whole point of keying by generation, not short alone.
        let mut new_generation = matching_seat.clone();
        new_generation.generation = 3;
        assert_eq!(
            seat_headroom_for_current(Some(&cached), Some(&new_generation)),
            None,
            "same short, new generation -- must not show the old seat's reading"
        );

        assert_eq!(seat_headroom_for_current(None, Some(&matching_seat)), None);
        assert_eq!(seat_headroom_for_current(Some(&cached), None), None);
    }

    // Review fix: false toast/flash on the dashboard's first observation.

    #[test]
    fn mail_flash_targets_never_flashes_on_the_first_observation() {
        let mut after: MailMap = HashMap::new();
        after.insert("aaa11111".to_string(), (1, 0));
        assert_eq!(
            mail_flash_targets(&MailMap::new(), &after, false),
            Vec::<String>::new(),
            "pre-existing unread mail must not flash just because this is the first read"
        );
    }

    #[test]
    fn mail_flash_targets_flashes_only_a_count_that_rose_since_the_last_observation() {
        let mut before: MailMap = HashMap::new();
        before.insert("aaa11111".to_string(), (1, 0));
        before.insert("bbb22222".to_string(), (2, 0));
        let mut after: MailMap = HashMap::new();
        after.insert("aaa11111".to_string(), (2, 0)); // rose 1 -> 2
        after.insert("bbb22222".to_string(), (2, 0)); // unchanged
        after.insert("ccc33333".to_string(), (1, 0)); // brand new short
        let mut flashed = mail_flash_targets(&before, &after, true);
        flashed.sort();
        assert_eq!(
            flashed,
            vec!["aaa11111".to_string(), "ccc33333".to_string()]
        );
    }

    #[test]
    fn rollover_committed_toast_never_fires_on_the_first_observation() {
        let committed = Some(super::super::rollover_runtime::Settlement::Committed {
            route: "codex".to_string(),
            generation: 2,
        });
        assert_eq!(
            rollover_committed_toast(&committed, &None, false, "claude"),
            None,
            "a settlement that predates this dashboard process must not toast on discovery"
        );
    }

    #[test]
    fn rollover_committed_toast_fires_only_for_a_genuinely_new_commit() {
        let committed = Some(super::super::rollover_runtime::Settlement::Committed {
            route: "codex".to_string(),
            generation: 2,
        });
        // A transition INTO Committed, seen after at least one observation.
        let text = rollover_committed_toast(&committed, &None, true, "claude")
            .expect("a fresh commit must toast");
        assert!(text.contains("claude"), "got {text:?}");
        assert!(text.contains("gen 2"), "got {text:?}");

        // Unchanged from the last observation: no repeat toast.
        assert_eq!(
            rollover_committed_toast(&committed, &committed, true, "claude"),
            None
        );

        // Not a Committed settlement at all: no toast.
        let parked = Some(super::super::rollover_runtime::Settlement::Parked {
            until: 100,
            reason: "no headroom anywhere".to_string(),
        });
        assert_eq!(
            rollover_committed_toast(&parked, &None, true, "claude"),
            None
        );
    }

    #[test]
    fn assemble_footer_facts_is_none_with_nothing_focused_and_no_exit_to_report() {
        let facts = assemble_footer_facts(None, None, None, None, false, None, None);
        assert!(matches!(facts, ui::FooterFacts::None));
    }

    /// Codex review finding 1: with nothing focused (the dashboard's last
    /// pane was just reaped) but a `last_exited` snapshot, the footer shows
    /// the dead-pane variant instead of drawing nothing.
    #[test]
    fn assemble_footer_facts_is_dead_when_nothing_is_focused_but_something_just_exited() {
        let facts = assemble_footer_facts(
            None,
            None,
            None,
            Some(("codex", Some(720))),
            false,
            None,
            None,
        );
        match facts {
            ui::FooterFacts::Dead(dead) => {
                assert_eq!(dead.harness, "codex");
                assert_eq!(dead.exited_age_secs, Some(720));
            }
            _ => panic!("expected FooterFacts::Dead"),
        }
    }

    /// Dash refresh PR1: the alive footer carries only the score/mail/
    /// supervision facts now -- harness, usage and workflow all moved
    /// elsewhere (the pane header, or PR2's forecast track).
    #[test]
    fn assemble_footer_facts_carries_score_and_mail_for_the_focused_row() {
        let row = focused_alive_row(Some(47));
        let facts = assemble_footer_facts(Some(&row), Some((2, 1)), None, None, false, None, None);
        match facts {
            ui::FooterFacts::Alive(alive) => {
                assert_eq!(alive.score, Some(47));
                // N7's broadcast/direct split collapses into one total.
                assert_eq!(alive.unread_mail, 3);
                assert!(alive.supervised);
                assert!(!alive.stalled);
            }
            _ => panic!("expected FooterFacts::Alive"),
        }
    }

    /// Codex review finding 5: an unsupervised focused pane (a turn-signal
    /// bind failure at spawn) must render as such, not as `supervised`.
    #[test]
    fn assemble_footer_facts_carries_unsupervised_through() {
        let row = focused_alive_row_supervised(None, false);
        let facts = assemble_footer_facts(Some(&row), None, None, None, false, None, None);
        match facts {
            ui::FooterFacts::Alive(alive) => assert!(!alive.supervised),
            _ => panic!("expected FooterFacts::Alive"),
        }
    }

    /// Issue #310: the caller's own `stalled` lookup reaches the footer
    /// facts unchanged -- `footer_alive_spans`'s own test proves this then
    /// overrides the supervision segment.
    #[test]
    fn assemble_footer_facts_carries_stalled_through() {
        let row = focused_alive_row(Some(47));
        let facts = assemble_footer_facts(Some(&row), None, None, None, true, None, None);
        match facts {
            ui::FooterFacts::Alive(alive) => assert!(alive.stalled),
            _ => panic!("expected FooterFacts::Alive"),
        }
    }

    /// A dead focused row produces `FooterFacts::Dead`, never `Alive` --
    /// there is no verdict/mail to show for an exited pane.
    #[test]
    fn assemble_footer_facts_is_dead_for_a_dead_focused_row() {
        let mut row = focused_alive_row(Some(12));
        row.state = ui::RowState::Dead;
        row.age_secs = Some(720);
        let facts = assemble_footer_facts(Some(&row), None, None, None, false, None, None);
        match facts {
            ui::FooterFacts::Dead(dead) => {
                assert_eq!(dead.harness, "claude");
                assert_eq!(dead.exited_age_secs, Some(720));
            }
            _ => panic!("expected FooterFacts::Dead"),
        }
    }

    fn wf_test_classification() -> crate::commands::workflow::classify::Classification {
        crate::commands::workflow::classify::Classification {
            intent: crate::commands::workflow::classify::Intent::Feature,
            complexity: crate::commands::workflow::classify::Complexity::Bounded,
            risk: crate::commands::workflow::classify::RiskBand::Low,
            risk_score: 0,
            changed_files: 1,
            changed_lines: 10,
            changed_paths: Vec::new(),
            declared_scope: true,
            work_domain: crate::commands::workflow::classify::DomainClassification::default(),
            risk_measurement: crate::commands::workflow::classify::RiskMeasurement::default(),
            reasons: Vec::new(),
        }
    }

    /// Dash refresh PR1: a running workflow resolves to its own current step
    /// and 1-based position, never the repo-wide pointer.
    #[test]
    fn resolve_session_workflow_reports_the_current_step() {
        let root = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(root.path().to_path_buf());
        let wf = workflow::engine::WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            workflow::engine::WorkflowKind::Feature,
            None,
            true,
            wf_test_classification(),
        );
        workflow::engine::save(&state, &wf, true).expect("save workflow");

        let fact =
            resolve_session_workflow(&state, repo.path(), &wf.id, super::super::state::now_secs())
                .expect("a saved, running workflow resolves");
        assert_eq!(fact.kind, "feature");
        assert_eq!(fact.step, wf.current().unwrap().id);
        assert_eq!(fact.index, 1);
        assert_eq!(
            fact.awaiting_approval,
            wf.status == workflow::engine::WorkflowStatus::AwaitingApproval
        );
        assert!(!fact.completed);
    }

    /// A completed run reads `done` immediately after finishing.
    #[test]
    fn resolve_session_workflow_reports_done_right_after_completion() {
        let root = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(root.path().to_path_buf());
        let mut wf = workflow::engine::WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            workflow::engine::WorkflowKind::Feature,
            None,
            true,
            wf_test_classification(),
        );
        wf.status = workflow::engine::WorkflowStatus::Completed;
        workflow::engine::save(&state, &wf, true).expect("save workflow");

        let fact =
            resolve_session_workflow(&state, repo.path(), &wf.id, super::super::state::now_secs())
                .expect("a just-completed workflow still resolves");
        assert_eq!(fact.step, "done");
        assert!(fact.completed);
    }

    /// The same completed run stops resolving at all once its own state
    /// file's last write is more than 10 minutes in the past -- the fact
    /// block's own "10 minutes, then nothing" rule.
    #[test]
    fn resolve_session_workflow_hides_a_completed_run_after_ten_minutes() {
        let root = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(root.path().to_path_buf());
        let mut wf = workflow::engine::WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            workflow::engine::WorkflowKind::Feature,
            None,
            true,
            wf_test_classification(),
        );
        wf.status = workflow::engine::WorkflowStatus::Completed;
        workflow::engine::save(&state, &wf, true).expect("save workflow");

        let far_future = super::super::state::now_secs() + 700;
        assert_eq!(
            resolve_session_workflow(&state, repo.path(), &wf.id, far_future),
            None,
            "a completed run more than 10 minutes stale must render nothing"
        );
    }

    /// The bug this replaces: `current_step` past the end of `steps` (with a
    /// non-`Completed` status) must resolve to `None`, never an empty step.
    #[test]
    fn resolve_session_workflow_is_none_when_the_current_step_is_out_of_range() {
        let root = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(root.path().to_path_buf());
        let mut wf = workflow::engine::WorkflowState::start(
            repo.path().to_path_buf(),
            "small feature".into(),
            workflow::engine::WorkflowKind::Feature,
            None,
            true,
            wf_test_classification(),
        );
        wf.current_step = wf.steps.len() + 5;
        workflow::engine::save(&state, &wf, true).expect("save workflow");

        assert_eq!(
            resolve_session_workflow(&state, repo.path(), &wf.id, super::super::state::now_secs()),
            None
        );
    }

    /// Round 2 coordinator review, CONFIRMED: a missing/unreadable state
    /// file (`state_mtime_secs` -> `None`) must never be treated as fresh --
    /// the old `unwrap_or(now)` made `done` never expire for exactly this
    /// case.
    #[test]
    fn completed_workflow_is_fresh_treats_a_missing_mtime_as_not_fresh() {
        assert!(!completed_workflow_is_fresh(None, 1_000));
        assert!(!completed_workflow_is_fresh(None, 0));
    }

    #[test]
    fn completed_workflow_is_fresh_is_the_ten_minute_window_on_a_real_mtime() {
        assert!(completed_workflow_is_fresh(Some(1_000), 1_000));
        assert!(completed_workflow_is_fresh(Some(1_000), 1_000 + 600));
        assert!(!completed_workflow_is_fresh(Some(1_000), 1_000 + 601));
    }

    /// No such id on disk at all (never bound, or purged) resolves to `None`.
    #[test]
    fn resolve_session_workflow_is_none_for_an_unknown_id() {
        let root = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(root.path().to_path_buf());
        assert_eq!(
            resolve_session_workflow(
                &state,
                repo.path(),
                "w-doesnotexist",
                super::super::state::now_secs()
            ),
            None
        );
    }

    /// Dash refresh PR1: the workflow segment moved to the pane header --
    /// an alive row's own footer facts carry no workflow field at all any
    /// more, whatever the repo-wide summary says.
    #[test]
    fn assemble_footer_facts_alive_variant_has_no_workflow_field() {
        let row = focused_alive_row(None);
        let summary = workflow::ActiveWorkflowSummary {
            kind: "feature",
            step: "design".to_string(),
            awaiting_approval: false,
        };
        let facts =
            assemble_footer_facts(Some(&row), None, Some(&summary), None, false, None, None);
        match facts {
            ui::FooterFacts::Alive(_) => {}
            _ => panic!("expected FooterFacts::Alive"),
        }
    }

    /// The dead-pane variant is the one exception (dash refresh PR1): it
    /// still carries the workflow summary, since a pane that no longer
    /// exists has no pane header left to show it in.
    #[test]
    fn assemble_footer_facts_dead_variant_still_carries_the_workflow_summary() {
        let mut row = focused_alive_row(None);
        row.state = ui::RowState::Dead;
        row.age_secs = Some(720);
        let summary = workflow::ActiveWorkflowSummary {
            kind: "feature",
            step: "spec".to_string(),
            awaiting_approval: true,
        };
        let facts =
            assemble_footer_facts(Some(&row), None, Some(&summary), None, false, None, None);
        match facts {
            ui::FooterFacts::Dead(dead) => match dead.workflow {
                ui::FooterWorkflow::Active { kind, step, gated } => {
                    assert_eq!(kind, "feature");
                    assert_eq!(step, "spec");
                    assert!(gated);
                }
                ui::FooterWorkflow::None => panic!("expected an active workflow segment"),
            },
            _ => panic!("expected FooterFacts::Dead"),
        }
    }

    /// Review finding 1: the swap is claimed on every tick, not once per
    /// `FACTS_THROTTLE`. Tied to the throttle, a due tick that found nothing
    /// waiting -- which the very first one almost always is, since the
    /// refresher only starts after `FactsCache::new` has seeded itself due --
    /// consumed the window, and the sidebar stayed empty for a second one.
    #[test]
    fn a_snapshot_landing_between_due_ticks_is_swapped_in_by_the_very_next_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let cfg = CtxConfig::default();
        let now = Instant::now();
        let mut cache = FactsCache::new(now);

        // The first due tick, with the refresher still on its first cycle.
        assert!(
            cache.refresh_if_due(&cfg, &state, owner(&repo), &[], now, || None),
            "the throttled block still ran"
        );
        assert_eq!(cache.disk.memory_count, 0, "nothing was published yet");

        // 10ms later -- nowhere near due -- the refresher's first snapshot
        // finally lands.
        let soon = now + Duration::from_millis(10);
        assert!(
            cache.refresh_if_due(&cfg, &state, owner(&repo), &[], soon, || Some(
                FactsSnapshot {
                    memory_count: 9,
                    ..FactsSnapshot::default()
                }
            )),
            "a swapped snapshot is a refreshed tick: the attention statuses \
             have to be re-keyed to the registry that came with it"
        );
        assert_eq!(
            cache.disk.memory_count, 9,
            "swapped in on the very next tick, not held back for the next window"
        );

        // ...and a tick with neither a snapshot nor a due window is not a
        // refresh at all, so the throttled reads stay throttled.
        assert!(!cache.refresh_if_due(&cfg, &state, owner(&repo), &[], soon, || None));
    }

    /// Issue #330: the spawn-request channels are read on their own cadence,
    /// not once per (up to 100/s) tick -- except with no panes left, where
    /// L17's empty-exit decision has to see a request that arrived on the very
    /// tick the last pane ended.
    #[test]
    fn the_spawn_request_intake_is_throttled_but_forced_once_the_panes_are_gone() {
        let start = Instant::now();
        assert!(
            spawn_intake_due(
                start.checked_sub(SPAWN_REQUEST_POLL).unwrap_or(start),
                start,
                false
            ),
            "seeded an interval in the past, the first tick reads"
        );
        assert!(
            !spawn_intake_due(start, start + SPAWN_REQUEST_POLL / 2, false),
            "a tick inside the window does no directory reads at all"
        );
        assert!(spawn_intake_due(start, start + SPAWN_REQUEST_POLL, false));
        assert!(
            spawn_intake_due(start, start + Duration::from_millis(1), true),
            "L17: with no panes left the intake runs whatever the throttle says"
        );
    }

    /// Issue #330: one vt100 budget for the tick, not one per pane. The
    /// focused pane -- the one the operator is watching and typing into --
    /// spends it first; the rest take what is left in a rotation, and whatever
    /// nobody could parse stays queued for the next tick rather than being
    /// dropped.
    #[test]
    fn the_shared_drain_budget_feeds_the_focused_pane_first_and_loses_nothing() {
        assert_eq!(drain_order(3, 1, 0), vec![1, 0, 2]);
        assert_eq!(
            drain_order(3, 1, 1),
            vec![1, 2, 0],
            "the rotation moves the start index, so no unfocused pane starves"
        );

        let mut queued = [100usize, 100, 100];
        let tick = |focused: usize, start: usize, queued: &mut [usize; 3]| {
            drain_shared_budget(3, focused, start, 150, |idx, remaining| {
                let used = queued[idx].min(remaining);
                queued[idx] -= used;
                (used > 0, used)
            })
        };

        // Focused pane 1 may spend 150 - 2 * (150 / 6) = 100 of the 150, and
        // each unfocused pane keeps its floor of 25.
        assert_eq!(tick(1, 0, &mut queued), vec![1, 0, 2]);
        assert_eq!(
            queued,
            [75, 0, 75],
            "the focused pane drains in full first, and the two behind it \
             still get their reserved floor"
        );

        assert_eq!(tick(1, 1, &mut queued), vec![2, 0]);
        assert_eq!(
            queued,
            [0, 0, 0],
            "two ticks parse every byte that was queued: nothing is ever dropped"
        );
    }

    /// Review finding 1: a focused pane streaming faster than the whole tick
    /// budget must not starve the panes behind it -- uncapped it took all of
    /// it every tick, and an unfocused pane's channel then grew without bound
    /// while its quiescence and turn-signal logic, which only ever runs off a
    /// drain, never saw another byte.
    #[test]
    fn a_focused_firehose_cannot_starve_the_panes_behind_it() {
        let mut queued = [100_000usize, 100, 100];
        let mut drained = [0usize; 3];
        // Scoped so the borrow of `drained` ends before it is read back.
        {
            let mut tick = |focused: usize, start: usize, queued: &mut [usize; 3]| {
                drain_shared_budget(3, focused, start, 150, |idx, share| {
                    let used = queued[idx].min(share);
                    queued[idx] -= used;
                    drained[idx] += used;
                    (used > 0, used)
                })
            };
            tick(0, 0, &mut queued);
            tick(0, 1, &mut queued);
        }

        assert!(
            drained[1] > 0 && drained[2] > 0,
            "every unfocused pane with queued bytes drained something over \
             two ticks: {drained:?}"
        );
        assert_eq!(
            drained[0], 200,
            "the focused pane still goes first, up to its cap of \
             150 - 2 * 25 per tick"
        );

        // ...and with the neighbours quiet, the share they did not need flows
        // back, so a focused firehose alone still spends the whole tick.
        let mut alone = [100_000usize, 0, 0];
        let mut spent = 0usize;
        drain_shared_budget(3, 0, 0, 150, |idx, share| {
            let used = alone[idx].min(share);
            alone[idx] -= used;
            spent += used;
            (used > 0, used)
        });
        assert_eq!(
            spent, 150,
            "an unused reserve flows back to the focused pane rather than \
             being left on the table"
        );
    }

    /// Issue #330: the header's mail counts come from the background
    /// refresher and from nowhere else. This is the stronger successor to the
    /// old "refreshes immediately, then honors the throttle" test: the tick
    /// used to re-read `mail::unread_counts` itself once a window, so the
    /// throttle was the only thing standing between the sidebar and a
    /// per-frame read. Now the tick never reads mail at all, and whatever the
    /// refresher publishes lands on the very next tick that asks.
    #[test]
    fn facts_cache_takes_its_mail_counts_from_the_refresher_and_never_reads_them_itself() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let cfg = CtxConfig::default();
        let now = Instant::now();

        let refresher = FakeRefresher::new(&state, &repo, &cfg);
        let mut cache = FactsCache::new(now);
        cache.refresh_if_due(&cfg, &state, owner(&repo), &[], now, || refresher.take());
        assert_eq!(
            cache.disk.mail,
            Some((0, 0)),
            "the refresher's first cycle lands on the first tick that asks"
        );

        // A message stored right after that first cycle is invisible until
        // the refresher publishes again -- however many ticks go by, and
        // whether or not the throttled block runs on them.
        let slug = super::super::state::repo_slug(&repo);
        mail::store(
            &state,
            &slug,
            &mail::Message {
                from_session: "other".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "note".to_string(),
            },
            &cfg,
        )
        .expect("store");

        cache.refresh_if_due(&cfg, &state, owner(&repo), &[], now, || refresher.take());
        assert_eq!(
            cache.disk.mail,
            Some((0, 0)),
            "with no new cycle published, the cached counts stand"
        );

        let later = now + FACTS_THROTTLE;
        cache.refresh_if_due(&cfg, &state, owner(&repo), &[], later, || refresher.take());
        assert_eq!(
            cache.disk.mail,
            Some((0, 0)),
            "and a due tick changes nothing either: the tick has no mail read \
             of its own left to make"
        );

        // The refresher's next cycle carries it -- on a tick that is NOT due,
        // proving the swap is independent of the throttled block.
        refresher.arm();
        cache.refresh_if_due(&cfg, &state, owner(&repo), &[], later, || refresher.take());
        assert_eq!(
            cache.disk.mail,
            Some((1, 0)),
            "the published counts land on the next tick that asks"
        );
    }

    /// The session registry and the rot scores are disk-backed, and both are
    /// rebuilt every ~20fps frame if they are not folded into this cache. An
    /// earlier round of this dashboard shipped exactly that regression, so
    /// where each one comes from is pinned here rather than assumed: the
    /// registry listing only ever arrives from the background refresher
    /// (issue #330 -- `sessions::list` is the machine-wide sweep that must
    /// never sit on the tick), while the scores stay on the throttled block
    /// and are keyed against whatever listing was swapped in first.
    #[test]
    fn the_registry_comes_from_the_refresher_and_the_scores_off_the_throttled_tick() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let cfg = CtxConfig::default();
        let now = Instant::now();

        let refresher = FakeRefresher::new(&state, &repo, &cfg);
        let mut cache = FactsCache::new(now);
        cache.refresh_if_due(&cfg, &state, owner(&repo), &[], now, || refresher.take());
        assert!(cache.registry.is_empty(), "nothing is registered yet");
        assert!(
            cache.disk.scores.is_empty(),
            "an unscored session is absent from the map, never a placeholder zero"
        );

        let _guard = sessions::SessionGuard::register(
            &state,
            registry_record("aaa11111", "claude", Some(DASHBOARD_PID)),
        );

        let later = now + FACTS_THROTTLE;
        cache.refresh_if_due(&cfg, &state, owner(&repo), &[], later, || refresher.take());
        assert!(
            cache.registry.is_empty(),
            "a due tick lists no sessions of its own: the record is invisible \
             until the refresher publishes it"
        );

        refresher.arm();
        cache.refresh_if_due(&cfg, &state, owner(&repo), &[], later, || refresher.take());
        assert_eq!(
            cache.registry.len(),
            1,
            "the refresher's next cycle carries it, throttle or no throttle"
        );
        assert!(
            cache.disk.scores.is_empty(),
            "a session with no readable transcript stays unscored: `rot --`, not `rot 0`"
        );
    }

    /// Finding 5: `refresh_if_due` used to score every live registry record
    /// regardless of ownership, even though `assemble_sidebar` was about to
    /// discard any record this dashboard process does not own. A record with
    /// a real, scorable transcript must still never reach `score::
    /// cached_score` at all when a foreign pid owns it.
    #[test]
    fn refresh_if_due_scores_only_registry_records_this_dashboard_owns() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let _agent =
            crate::commands::ctx::testenv::VarGuard::set(&[("ZIRV_CTX_AGENT", Some("claude"))]);
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path();
        let cfg = CtxConfig::default();

        let owned_session = "5c0d0002-2222-4222-8333-555555555555";
        let foreign_session = "5c0d0003-3333-4222-8333-555555555555";

        let transcript_dir = home
            .join(".claude")
            .join("projects")
            .join(super::super::state::repo_slug(repo));
        std::fs::create_dir_all(&transcript_dir).expect("mkdir");
        let body = "{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n\
             {\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"[zirv] done\"}],\"usage\":{\"input_tokens\":170000}}}\n";
        for session in [owned_session, foreign_session] {
            std::fs::write(transcript_dir.join(format!("{session}.jsonl")), body).expect("write");
        }

        let owned = sessions::Record::new(owned_session, "claude", repo, sessions::Verb::Wrap);
        let owned_short = owned.short.clone();
        let _owned_guard = sessions::SessionGuard::register(&state, owned);

        let mut foreign =
            sessions::Record::new(foreign_session, "claude", repo, sessions::Verb::Wrap);
        let foreign_short = foreign.short.clone();
        foreign.owner_pid = Some(std::process::id().wrapping_add(1));
        let _foreign_guard = sessions::SessionGuard::register(&state, foreign);

        let mut cache = FactsCache::new(Instant::now());
        cache.refresh_if_due(&cfg, &state, owner(repo), &[], Instant::now(), || {
            snapshot(&state, repo, &cfg)
        });

        assert_eq!(cache.registry.len(), 2, "both records are on disk");
        assert!(
            cache.disk.scores.contains_key(&owned_short),
            "an owned record with a real transcript is scored: {:?}",
            cache.disk.scores
        );
        assert!(
            !cache.disk.scores.contains_key(&foreign_short),
            "a foreign-owned record must never be scored, undisplayable as it is: {:?}",
            cache.disk.scores
        );
    }

    /// Session-scoped total, review round: `jev_session_snapshot`'s own set
    /// is grow-only -- a pane's session id, once seen, stays in the JEV
    /// filter even after that pane is reaped and no longer in `panes` at
    /// all, so its own JEV rows keep counting toward the dashboard's total.
    #[test]
    fn a_panes_session_id_stays_in_the_jev_set_after_it_is_reaped() {
        use super::pane::tests::long_lived_argv;

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "88888888-3333-4444-8888-555555555555";
        let spec = PaneSpec {
            agent_name: "codex".to_string(),
            argv: long_lived_argv(),
            role: prompt::PromptRole::Worker,
            verb: sessions::Verb::Dash,
            session_id: session_id.to_string(),
            title: "wrk".to_string(),
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

        let mut sessions: BTreeSet<String> = BTreeSet::new();
        let seen_while_live = jev_session_snapshot(&mut sessions, std::slice::from_ref(&pane));
        assert!(
            seen_while_live.contains(session_id),
            "the live pane's own session id is in the set"
        );

        // Reaped: the pane no longer exists in the slice the caller passes
        // (the same shape a real `panes.retain`/removal leaves behind), but
        // the running set it already grew into must not shrink back.
        let seen_after_reap = jev_session_snapshot(&mut sessions, &[]);
        assert!(
            seen_after_reap.contains(session_id),
            "a reaped pane's session id must still count: {seen_after_reap:?}"
        );

        pane.finish_shutdown().expect("shutdown");
    }

    /// `^A r` on a row that cannot be restored says so and changes nothing --
    /// no pane is spawned, and the row stays on the roster to be inspected.
    /// (The hint is not even drawn for such a row; see
    /// `header_hints_follow_the_selected_rows_state`.)
    #[test]
    fn restoring_a_row_with_no_kept_request_is_a_notice_and_no_spawn() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = std::env::current_dir().expect("cwd");
        let cfg = CtxConfig::default();
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
                request: None,
                requested_by: None,
                meta: EndedMeta {
                    exit_code: 0,
                    exited_at: 600,
                    age_secs: None,
                },
            },
            MAX_RETAINED_ENDED_ROWS,
        );
        let mut panes: Vec<Pane> = Vec::new();
        let mut queues: Vec<VecDeque<String>> = Vec::new();
        let mut kept: HashMap<String, (spawnreq::SpawnRequest, Option<String>)> = HashMap::new();
        let mut errors = ErrorLog::default();
        let mut notices = Vec::new();
        let now = Instant::now();
        for short in ["bbb22222", "not-a-row"] {
            restore_ended_row(
                short,
                &mut panes,
                &mut queues,
                &mut retained,
                &mut kept,
                &cfg,
                &state,
                &repo,
                (80, 24),
                &tmp.path().join("requests"),
                &mut errors,
                &mut notices,
                now,
                &[],
                &mut 0,
            );
        }
        assert!(panes.is_empty(), "nothing may be spawned");
        assert_eq!(retained.len(), 1, "the row stays on the roster");
        assert!(errors.is_empty(), "this is a notice, not an error");
        assert!(
            notices.iter().any(|n| n.text.contains(MENU_NO_REQUEST)),
            "{:?}",
            notices.iter().map(|n| n.text.clone()).collect::<Vec<_>>()
        );
        assert!(
            notices
                .iter()
                .any(|n| n.text.contains("no ended row named")),
            "{:?}",
            notices.iter().map(|n| n.text.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn due_fires_only_once_the_interval_has_elapsed() {
        let now = Instant::now();
        assert!(!due(now, now, Duration::from_secs(1)));
        assert!(due(
            now,
            now + Duration::from_secs(1),
            Duration::from_secs(1)
        ));
        assert!(due(
            now,
            now + Duration::from_secs(5),
            Duration::from_secs(1)
        ));
    }

    #[test]
    fn due_advancing_advances_last_only_when_due_regardless_of_what_the_caller_does_next() {
        // Issue #780: a disabled `auto_orchestrator_rollover` must not leave
        // `last` stale -- otherwise the cheap cadence check alone keeps
        // reporting "due" every tick, forcing the caller's expensive
        // `is_enabled()` (two `stat`s) to run every tick too.
        let start = Instant::now();
        let mut last = start;
        let interval = Duration::from_secs(1);

        assert!(!due_advancing(&mut last, start, interval));
        assert_eq!(last, start);

        let tick = start + Duration::from_secs(1);
        assert!(due_advancing(&mut last, tick, interval));
        assert_eq!(last, tick);

        // Immediately after, the cadence is not due again -- the caller's
        // expensive check is not run again on the very next tick, whether or
        // not that check ends up finding the switch disabled.
        assert!(!due_advancing(
            &mut last,
            tick + Duration::from_millis(100),
            interval
        ));
        assert_eq!(last, tick);
    }
}
