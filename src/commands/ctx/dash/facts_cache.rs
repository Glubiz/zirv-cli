//! Disk-backed facts cache and its background refresher.
use super::*;

/// The disk-backed part of the header's facts: everything `FactsCache::
/// refresh_if_due` re-reads on the throttle. Kept separate from
/// `ui::HeaderFacts` itself because the harness/error line and the live
/// session count are cheap, in-memory state recomputed fresh on every frame
/// regardless -- only these fields, plus the registry listing below, cost an
/// actual read.
#[derive(Default)]
pub(super) struct DiskFacts {
    /// Rot scores for every row the sidebar can draw -- this dashboard's own
    /// panes and every live registry session it owns (see `assemble_sidebar`'s
    /// own `owner_pid` filter; a foreign or unowned record is never displayed,
    /// so scoring it here would be wasted work). `score::cached_score` is
    /// cheap in the steady state but still costs one `metadata` call per
    /// session, which is one per pane per frame if it is not cached here.
    pub(super) scores: ScoreMap,
    pub(super) mail: Option<(usize, usize)>,
    pub(super) memory_count: usize,
    /// Per-harness usage snapshot for the header row, one entry per enabled
    /// harness (`cfg.agents`) in registry order. Filled from whatever
    /// `window::load_for` already has on disk -- a file read only, never a
    /// rollout scan or a poll -- and then run through `window::available` so
    /// a reading whose window has provably reset never renders as a live
    /// percentage. Those scans/polls live in the sessions that actually gate
    /// on pacing (Tasks 4-6's `PaceGate` call sites) and, for a wrapped codex
    /// session with no statusline tee, in wrap's own throttled passive scan
    /// (`wrap::redraw_bar_if_due`); this dashboard's event loop must never do
    /// either itself, or a redraw could stall on a stale rollout file or the
    /// network.
    pub(super) usage: Vec<ui::HarnessUsage>,
    /// Issue #209/v3 §D: the dashboard's own repo's active `zirv workflow`,
    /// as much as the footer's workflow segment needs. `workflow::
    /// active_workflow_summary` is the same plain-file read `zirv workflow
    /// status` itself uses with no `--id` -- no subprocess, no scan --
    /// so it costs nothing more than the scores/usage reads right above it
    /// to fold into this same throttled tick. `None` covers "no active
    /// workflow" and "failed to load" alike; the footer renders the same
    /// dim `▸ –` either way.
    pub(super) workflow: Option<workflow::ActiveWorkflowSummary>,
    /// Issue #209/v3 codex review finding 2: per-session unread mail, for
    /// the footer's own `✉` segment -- see [`MailMap`]'s own doc comment
    /// for why `mail` above cannot answer this.
    pub(super) mail_by_session: MailMap,
    /// Issue #310: session short ids with an armed stall latch
    /// (`sessions::stall_marker`), read on the same throttled tick as
    /// `mail_by_session` and populated exactly the same way -- every
    /// attached pane by its own `short()`, plus every live registry row this
    /// dashboard owns. Absence means "not stalled" (never armed, or already
    /// cleared by observed progress), matching the marker's own
    /// once-cleared-on-progress contract; there is no separate "unknown"
    /// state to represent here.
    pub(super) stalled: HashSet<String>,
    /// Dash refresh PR1: each pane's own bound workflow, resolved by id from
    /// its own session record -- see `resolve_session_workflow`'s own doc
    /// comment. Read on the same throttled tick and populated the same way
    /// as `mail_by_session`/`stalled` right above; absence means "no bound
    /// workflow, or its run could not be resolved," never a guess.
    pub(super) workflow_by_session: HashMap<String, ui::SessionWorkflowFact>,
    /// Issue #264: the aggregate row's own `failed`/`cost` cells, read once
    /// per throttled tick alongside `usage`/`mail` above -- `delegations.
    /// jsonl` is a plain file read, the same no-scan/no-network discipline
    /// `usage`'s own doc comment holds. `None` when the ledger has no rows
    /// at all yet (a fresh state dir, or a dashboard that has never spawned
    /// a delegated worker): [`ui::render_aggregate_row`] renders `--` for
    /// both cells rather than a phantom `0`/`$0.00` that would be
    /// indistinguishable from "checked and found none".
    pub(super) spend: Option<AggregateSpendFacts>,
    /// Issue #358 (task T6a): one [`ui::HarnessStrip`] per harness `cfg.
    /// fallback.order` names, for the aggregate row's own pool strip -- off
    /// the identical `fallback::capacity_snapshot` the fallback/status
    /// surfaces already build. Empty when the repo configures no fallback
    /// order at all. Composed by the background refresher and swapped in
    /// whole (see [`FactsSnapshot::pool_harnesses`]): that call lists the
    /// session registry itself, so unlike `usage` right above it is not a
    /// plain file read and has no business on the tick.
    pub(super) pool_harnesses: Vec<ui::HarnessStrip>,
    /// This dashboard's own orchestrator seat's `"gen N"` label (`seat::
    /// load`, keyed by `FactsOwner::session_short`), `None` until a seat is
    /// registered for it.
    pub(super) pool_seat: Option<String>,
    /// Dash refresh PR2: this dashboard's own orchestrator seat, in full --
    /// `pool_seat` above only ever kept the formatted generation string.
    /// Read on the same throttled tick, the same `<short>.seat.json` file
    /// `pool_seat` already opens. `None` until a seat is registered.
    pub(super) seat_full: Option<super::seat::Seat>,
    /// Dash refresh PR2: that same seat's own rollover-runtime record
    /// (`<short>.rollover.json`), read alongside `seat_full` above. `None`
    /// with no rollover history at all for this seat.
    pub(super) rollover_record: Option<super::rollover::runtime::Record>,
    /// Dash refresh PR2: the JEV sidebar section's facts, refreshed on its
    /// OWN (much coarser, 10s) cadence -- see `jev_due`/its own call site.
    /// `None` with every `[jev]` gate off, which is also how the section
    /// hides itself entirely.
    pub(super) jev: Option<ui::JevSectionFact>,
    /// Issue #354: every live pane's work group, by id -- the sidebar's group
    /// headers name a scope, and `group::load` is a disk read that must never
    /// happen per frame. Read by the background [`FactsRefresher`] and swapped
    /// in whole (see [`FactsSnapshot`]).
    pub(super) groups: HashMap<String, super::group::WorkGroup>,
    /// Issue #354: when each pane last changed [`ui::RowState`], for the
    /// `since` disclosure line. Kept here rather than on `Pane` because it is
    /// a property of what the *dashboard* has observed across ticks, and it
    /// is pruned to the live panes on every refresh.
    pub(super) state_since: HashMap<String, (ui::RowState, u64)>,
    /// Issue #354 phase 2: the composed `attention::SessionStatus` behind
    /// every row's glyph, its rollups and its `reason` line -- one
    /// `attention::load` (a single small JSON read) per drawable row, on this
    /// same throttled tick and NEVER per frame. Filled by
    /// [`FactsCache::refresh_attention`], which the event loop calls only on a
    /// tick where [`FactsCache::refresh_if_due`] actually re-read. A missing
    /// or corrupt file loads back as `SessionStatus::default()`, which
    /// projects `Unknown` -- and `ui::glyph_for` treats that exactly like no
    /// entry at all, so a dashboard with no issue #349 writers renders the
    /// phase 1 sidebar unchanged.
    pub(super) attention: HashMap<String, super::attention::SessionStatus>,
}

/// Issue #264: [`DiskFacts::spend`]'s own shape. Issue #457: `cost_micros`
/// now folds in the seat's own transcript and its native subagent
/// transcripts too, not only `delegations.jsonl` -- see
/// `session_spend::fold_session_spend`, the one function this and
/// `status::spend_status_line` both call.
#[derive(Debug, Clone, Copy)]
pub(super) struct AggregateSpendFacts {
    pub(super) failed: u64,
    pub(super) cost_micros: u64,
    /// Issue #457: how many deduplicated assistant messages/delegation rows
    /// contributed nothing to `cost_micros` because their model priced as
    /// unknown -- surfaced on the dashboard inspector (`^A i`) so "$0.00"
    /// never looks indistinguishable from "nothing happened".
    pub(super) skipped_messages: u64,
}

/// Who the dashboard is, for the reads that are scoped to it: the repo it
/// runs in, its launch agent, and its own registry short id (D2 -- deliberately
/// the dashboard's own identity, never `panes.first()`'s). Grouped rather than
/// passed as three more parameters: all three are fixed for a session's whole
/// life, and `refresh_if_due` already carries the ones that are not.
#[derive(Clone, Copy)]
pub(super) struct FactsOwner<'a> {
    pub(super) repo: &'a Path,
    pub(super) agent_name: &'a str,
    pub(super) session_short: &'a str,
}

/// The disk-derived facts that are read OFF the UI thread, published whole by
/// [`FactsRefresher`] and swapped into [`FactsCache`] by the tick.
///
/// Exactly the reads whose cost is a function of machine-wide history rather
/// than of this dashboard's own panes -- above all `sessions::list`, which
/// reads and parses every `sessions/*.json` on the machine and then sweeps the
/// state directory, probing each orphan socket synchronously (`signal::probe`,
/// a named-pipe open on Windows). On the UI thread that is keystroke latency:
/// the tick reaches `event::poll` only after it finishes, once a second,
/// forever. Everything else `FactsCache::refresh_if_due` reads is keyed to
/// this dashboard's own panes and stays on the tick.
#[derive(Default)]
pub(super) struct FactsSnapshot {
    pub(super) mail: Option<(usize, usize)>,
    pub(super) memory_count: usize,
    pub(super) registry: Vec<(sessions::Record, sessions::Liveness)>,
    pub(super) groups: HashMap<String, super::group::WorkGroup>,
    /// Issue #358 (task T6a): the aggregate row's pool strip. Here rather
    /// than on the tick because `fallback::capacity_snapshot` calls
    /// `sessions::list` itself (`fallback.rs`), so leaving it behind would
    /// have kept the very sweep this snapshot exists to move -- and its
    /// `refresh_ranked_providers` can walk a codex rollout tree on top of
    /// that. Composed all the way into [`ui::HarnessStrip`]s on the
    /// refresher's thread: the mapping is pure, so nothing is gained by
    /// carrying the raw snapshot back to the loop.
    pub(super) pool_harnesses: Vec<ui::HarnessStrip>,
}

/// Everything [`collect_facts_snapshot`] needs and a background thread cannot
/// borrow from the event loop. All of it is fixed for a dashboard's whole life
/// (the same reasoning [`FactsOwner`] documents for its own three fields), so
/// it is cloned once at spawn; the one input that does move -- the live panes'
/// work-group ids -- travels through [`FactsRefresher::group_ids`] instead.
#[derive(Clone)]
pub(super) struct FactsInputs {
    pub(super) state: StateDir,
    pub(super) repo: PathBuf,
    pub(super) agent_name: String,
    pub(super) session_short: String,
    pub(super) mail_enabled: bool,
    /// The dashboard's own config, cloned rather than borrowed for the same
    /// reason the rest of this struct is: it is loaded once at launch and
    /// never reassigned for the life of the loop.
    pub(super) cfg: CtxConfig,
}

/// The reads behind one [`FactsSnapshot`], in one place so the background
/// thread, the inline fallback and the tests all run identical code.
pub(super) fn collect_facts_snapshot(inputs: &FactsInputs, group_ids: &[String]) -> FactsSnapshot {
    let FactsInputs {
        state,
        repo,
        agent_name,
        session_short,
        mail_enabled,
        cfg,
    } = inputs;
    let mail = mail::unread_counts(state, repo, agent_name, session_short, *mail_enabled);
    let slug = super::state::repo_slug(repo);
    let memory_count = memory::list(state, &slug).map(|v| v.len()).unwrap_or(0);
    let registry = sessions::list_with_retention(state, cfg.dash.roster_max_age_secs);
    // Issue #354: the sidebar's group headers name a scope -- one `group::
    // load` per distinct live group, never one per frame.
    let groups = group_ids
        .iter()
        .filter_map(|id| {
            super::group::load(state, id)
                .ok()
                .flatten()
                .map(|g| (id.clone(), g))
        })
        .collect();
    FactsSnapshot {
        mail,
        memory_count,
        registry,
        groups,
        // The thread's own clock: `now_secs` is wall time, and this snapshot
        // is only ever read against wall time (`window::available`'s own
        // freshness checks live inside the call).
        pool_harnesses: pool_strips(state, cfg, super::state::now_secs()),
    }
}

/// Issue #358 (task T6a): the aggregate row's own pool strip.
/// `fallback::capacity_snapshot` is a read of already-stored usage windows
/// plus the session registry -- never a poller and never an outbound request
/// -- but it is a `sessions::list` and, through `refresh_ranked_providers`, a
/// possible rollout scan, so it runs on the refresher's thread with the rest
/// of the machine-wide reads. `requester`/`requested` are both `None`: this is
/// a repo-wide overview, not a placement decision for one particular unit of
/// work, so nothing needs excluding from the live `active` count and no
/// harness outside `cfg.fallback.order` needs to be forced in.
pub(super) fn pool_strips(
    state: &StateDir,
    cfg: &CtxConfig,
    now_secs: u64,
) -> Vec<ui::HarnessStrip> {
    let snapshot = fallback::capacity_snapshot(state, cfg, now_secs, None, None);
    snapshot
        .harnesses
        .iter()
        .map(|harness| {
            // Audit finding G2: the strip reports the reading the allocator
            // ranks on, so an idle codex whose rollout snapshot has aged past
            // `collector_max_age_secs` reads `stale 53%` rather than the old
            // `unknown --`. `harness.state` itself is unchanged -- `classify`
            // still refuses an unbinding reading as a hard-gate authority.
            let ranking = snapshot
                .provider(&harness.provider)
                .and_then(super::allocator::ranking_window);
            let state =
                if harness.state == super::allocator::HarnessState::Unknown && ranking.is_some() {
                    "stale".to_string()
                } else {
                    harness.state.as_str().to_string()
                };
            ui::HarnessStrip {
                name: harness.name.clone(),
                state,
                headroom_pct: ranking.map(|w| w.headroom_pct),
            }
        })
        .collect()
}

/// How many slices the refresher's sleep between cycles is cut into, so a quit
/// is noticed within a fraction of `FACTS_THROTTLE` rather than after a whole
/// one. The thread is otherwise entirely passive.
pub(super) const FACTS_SLEEP_SLICES: u32 = 10;

/// The background half of [`FactsCache`]: one thread that runs
/// [`collect_facts_snapshot`] on the `FACTS_THROTTLE` cadence and publishes
/// each result down an `mpsc` channel the tick drains with `try_recv`, so a
/// slow state directory costs the operator nothing but staleness -- never a
/// keystroke.
pub(super) struct FactsRefresher {
    /// The live panes' work-group ids, republished by the tick on its own
    /// cadence. Locked only to clone in or out, never across a disk read, so
    /// the tick can never wait on a `sessions::list` in progress.
    group_ids: Arc<Mutex<Vec<String>>>,
    rx: mpsc::Receiver<FactsSnapshot>,
    stop: Arc<AtomicBool>,
    /// `Some` only when the thread could not be spawned at all: the snapshot
    /// is then computed inline, exactly as it was before this loop had a
    /// background half. A dashboard that cannot start a thread still shows a
    /// correct sidebar -- it just pays the old latency for it.
    inline: Option<FactsInputs>,
    /// When the inline fallback last collected. Review finding 3: the tick
    /// asks on EVERY iteration now (that is the point of the swap being
    /// decoupled from the throttle), so without a clock of its own the
    /// fallback would read the whole state directory up to 100 times a
    /// second -- far worse than the once-a-second it replaced. Inert while
    /// the thread is running, which is the only case that is not a
    /// pathology.
    last_inline: std::cell::Cell<Instant>,
}

impl FactsRefresher {
    pub(super) fn spawn(state: &StateDir, owner: FactsOwner<'_>, cfg: &CtxConfig) -> Self {
        let inputs = FactsInputs {
            state: state.clone(),
            repo: owner.repo.to_path_buf(),
            agent_name: owner.agent_name.to_string(),
            session_short: owner.session_short.to_string(),
            mail_enabled: cfg.mail.enabled,
            cfg: cfg.clone(),
        };
        let (tx, rx) = mpsc::channel();
        let group_ids = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_groups = Arc::clone(&group_ids);
        let thread_stop = Arc::clone(&stop);
        let thread_inputs = inputs.clone();
        let spawned = std::thread::Builder::new()
            .name("zirv-dash-facts".to_string())
            .spawn(move || {
                while !thread_stop.load(Ordering::Relaxed) {
                    let ids = lock_group_ids(&thread_groups);
                    if tx
                        .send(collect_facts_snapshot(&thread_inputs, &ids))
                        .is_err()
                    {
                        // The dashboard dropped its receiver: nothing will
                        // ever read another snapshot.
                        return;
                    }
                    for _ in 0..FACTS_SLEEP_SLICES {
                        if thread_stop.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(FACTS_THROTTLE / FACTS_SLEEP_SLICES);
                    }
                }
            })
            .is_ok();
        Self {
            group_ids,
            rx,
            stop,
            inline: (!spawned).then_some(inputs),
            // Seeded a full interval in the past so the first ask collects
            // immediately, the same reasoning `FactsCache::new` documents.
            last_inline: std::cell::Cell::new(
                Instant::now()
                    .checked_sub(FACTS_THROTTLE)
                    .unwrap_or_else(Instant::now),
            ),
        }
    }

    /// The panes' current work-group ids, for the refresher's next cycle. A
    /// group that appears on this tick therefore lands in the snapshot after
    /// next -- the same order-of-a-second staleness every other fact on this
    /// cadence already carries.
    pub(super) fn publish_group_ids(&self, ids: Vec<String>) {
        match self.group_ids.lock() {
            Ok(mut guard) => *guard = ids,
            Err(poisoned) => *poisoned.into_inner() = ids,
        }
    }

    /// The newest snapshot the refresher has published, or `None` when it has
    /// not finished a cycle since the last call. Never blocks: `try_recv` in a
    /// loop keeping only the last, so a tick costs one channel probe however
    /// far behind a busy machine has left the thread.
    ///
    /// `now` is the tick's own clock, and is used only by the inline
    /// fallback, which stands in for the thread's cadence with a throttle of
    /// its own (review finding 3).
    pub(super) fn take_latest(&self, now: Instant) -> Option<FactsSnapshot> {
        if let Some(inputs) = self.inline.as_ref() {
            if !due(self.last_inline.get(), now, FACTS_THROTTLE) {
                return None;
            }
            self.last_inline.set(now);
            let ids = lock_group_ids(&self.group_ids);
            return Some(collect_facts_snapshot(inputs, &ids));
        }
        let mut latest = None;
        while let Ok(snapshot) = self.rx.try_recv() {
            latest = Some(snapshot);
        }
        latest
    }
}

impl Drop for FactsRefresher {
    /// The dashboard is leaving: the thread stops at its next slice rather
    /// than outliving the terminal it was reading for.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A poisoned lock is not worth failing a dashboard over -- the payload is a
/// list of group ids, and the worst a stale one costs is a missing sidebar
/// header for one cycle.
pub(super) fn lock_group_ids(ids: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    match ids.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// Caches every disk read the header and sidebar need -- rot scores, mail,
/// memory-bank size, and the session registry itself -- refreshed at most
/// once per `FACTS_THROTTLE` rather than on the render loop's own 50ms poll.
pub(super) struct FactsCache {
    pub(super) disk: DiskFacts,
    pub(super) registry: Vec<(sessions::Record, sessions::Liveness)>,
    pub(super) last_refresh: Instant,
    /// A1-4/#457: every file `disk.spend` was last folded from -- the
    /// delegation ledger, the seat's own transcript, and its subagent
    /// directory (`session_spend::SpendFingerprint`) -- as of the last fold.
    /// `None` until the first fold. Every one of those files only ever
    /// grows or is appended to for a live session, so an unchanged
    /// fingerprint means unchanged content -- and re-reading and re-pricing
    /// megabytes of transcript once a second, forever, buys nothing.
    spend_key: Option<super::session_spend::SpendFingerprint>,
}

impl FactsCache {
    /// Never refreshed yet, so the very first check always reads through.
    /// `checked_sub` (rather than a bare subtraction) degrades to "refresh
    /// immediately" on a process uptime under a second, the same reasoning
    /// `BarRuntime::new` documents for its own `last_draw`.
    pub(super) fn new(now: Instant) -> Self {
        Self {
            disk: DiskFacts::default(),
            registry: Vec::new(),
            last_refresh: now.checked_sub(FACTS_THROTTLE).unwrap_or(now),
            spend_key: None,
        }
    }

    /// A1-4/#457: folds `disk.spend` from THIS session's own delegation rows
    /// AND its own transcript sources (seat + native subagents), but only
    /// when the combined fingerprint moved since the last fold. `read` is
    /// injected so the skip is testable without any of those on disk, and so
    /// this stays the one place both inputs are actually read -- pricing
    /// itself happens in `session_spend::fold_session_spend`, the identical
    /// function `status::spend_status_line` calls, so the footer and `zirv
    /// ctx status` can no longer disagree about what "this session" means.
    pub(super) fn refresh_spend_with<F>(
        &mut self,
        fingerprint: super::session_spend::SpendFingerprint,
        cfg: &CtxConfig,
        session_short: &str,
        read: F,
    ) where
        F: FnOnce() -> (
            Vec<super::log::DelegationRow>,
            super::session_spend::TranscriptFold,
        ),
    {
        if self.spend_key == Some(fingerprint) {
            return;
        }
        self.spend_key = Some(fingerprint);
        let (delegation_rows, transcript) = read();
        let table = super::price::resolve_table(cfg);
        // 2026-09-06: the footer renders this as "$<cost> this session", so
        // delegation rows are filtered by the same predicate `status::
        // spend_status_line` uses for its own "this session" figure --
        // `parent_session`, the session that DELEGATED the row. Unfiltered,
        // the footer summed the whole machine-wide ledger and disagreed with
        // `zirv ctx status` on the same dashboard.
        let spend = super::session_spend::fold_session_spend(
            &delegation_rows,
            Some(session_short),
            None,
            &transcript,
            &table,
        );
        // Issue #457 item 3: `--` only when NEITHER source exists at all --
        // `spend.cost_micros` is already `None` in exactly that case (see
        // `fold_session_spend`'s own doc comment), never re-hidden here.
        self.disk.spend = spend.cost_micros.map(|cost_micros| AggregateSpendFacts {
            failed: spend.delegation_failed,
            cost_micros,
            skipped_messages: spend.skipped_messages,
        });
    }

    /// Pure: swaps in whatever the background [`FactsRefresher`] last
    /// published, reporting whether anything actually arrived.
    ///
    /// Split out from [`FactsCache::refresh_if_due`] so the swap is testable
    /// with no thread, no state directory and no terminal -- and so the one
    /// property that matters here is stated in one place: a tick that finds
    /// nothing waiting keeps the facts it already had, at whatever age they
    /// are, rather than blanking the sidebar while the refresher catches up.
    pub(super) fn apply_snapshot(&mut self, latest: Option<FactsSnapshot>) -> bool {
        let Some(snapshot) = latest else {
            return false;
        };
        self.disk.mail = snapshot.mail;
        self.disk.memory_count = snapshot.memory_count;
        self.disk.groups = snapshot.groups;
        self.disk.pool_harnesses = snapshot.pool_harnesses;
        self.registry = snapshot.registry;
        true
    }

    /// Every disk read the header and sidebar need, at most once per
    /// `FACTS_THROTTLE`. `panes` is only walked when a refresh is actually
    /// due, so a throttled tick costs the `due` comparison and nothing else.
    ///
    /// `take_snapshot` is the non-blocking hand-off from the background
    /// [`FactsRefresher`] -- the machine-wide reads (mail, memory bank, the
    /// session registry, the work groups, the pool strip) happen on its
    /// thread and are only swapped in here.
    ///
    /// Deliberately claimed on EVERY tick, ahead of and independently of the
    /// throttle (review finding 1). Tying the swap to `last_refresh` meant a
    /// due tick that found nothing waiting still consumed the window: the
    /// refresher starts after `FactsCache::new` has already seeded itself
    /// due, so the very first due tick almost always missed and the sidebar
    /// stayed empty for a second window -- and any later cycle that slipped
    /// past a due tick cost another whole one. The throttled block below
    /// keeps its own clock, so a tick that only swaps stays free of disk.
    ///
    /// Returns whether the tick refreshed anything: the throttled block ran,
    /// a snapshot landed, or both. Issue #354 phase 2 hangs
    /// [`FactsCache::refresh_attention`] off that answer rather than off a
    /// second throttle of its own: the attention statuses have to be exactly
    /// as fresh as the registry listing they are keyed against, and a second
    /// clock could only ever drift them apart. Reporting a swap-only tick as
    /// a refresh is what keeps that true now that the two can happen apart.
    pub(super) fn refresh_if_due<F>(
        &mut self,
        cfg: &CtxConfig,
        state: &StateDir,
        owner: FactsOwner<'_>,
        panes: &[Pane],
        now: Instant,
        take_snapshot: F,
    ) -> bool
    where
        F: FnOnce() -> Option<FactsSnapshot>,
    {
        let swapped = self.apply_snapshot(take_snapshot());
        if !due(self.last_refresh, now, FACTS_THROTTLE) {
            return swapped;
        }
        self.last_refresh = now;

        let FactsOwner {
            repo,
            agent_name: _,
            session_short,
        } = owner;

        // Issue #354: the sidebar's `since` line, on this same throttled
        // cadence -- a state-change clock that only ever moves when the state
        // actually changed, pruned to the live panes so a reaped pane leaves
        // nothing behind. In-memory and keyed to the panes this tick is
        // holding, so unlike the group headers it stays on this thread.
        self.disk
            .state_since
            .retain(|short, _| panes.iter().any(|p| p.short() == short));
        for pane in panes {
            let current = ui::row_state_for(&pane.state());
            let entry = self
                .disk
                .state_since
                .entry(pane.short().into())
                .or_insert((current, super::state::now_secs()));
            if entry.0 != current {
                *entry = (current, super::state::now_secs());
            }
        }
        // Issue #209/v3 §D: same throttled tick as the reads above it, same
        // no-subprocess/no-scan discipline -- see `DiskFacts::workflow`'s
        // own doc comment. Deliberately the dashboard's own `repo`, not a
        // per-pane one (codex review finding 3, refuted): a workflow is a
        // repo-level singleton with no session dimension at all
        // (`engine::WorkflowState`/`load_active` take a repo, never a
        // session id), and every other per-session disk read in this loop
        // -- scores, mail, memory -- is already keyed off this same shared
        // `repo` by the identical, deliberate convention `Pane::spawn`'s own
        // doc comment documents for `cwd` vs. `repo` (issue #119): a
        // worktree-hosted pane's *argv* runs in its own working tree, but
        // its identity for every disk read stays the dashboard's repo,
        // because the session/state store is shared across every pane this
        // dashboard hosts.
        self.disk.workflow = workflow::active_workflow_summary(state, repo);

        // Task 7: one usage entry per enabled harness, read straight off
        // disk. `window::load_for` is a file read, never a scan/poll -- see
        // `DiskFacts::usage`'s own doc comment for why this loop must stay
        // that way. `window::available` is a pure in-memory filter over what
        // was just read, so it costs nothing extra here.
        let now_secs = super::state::now_secs();
        self.disk.usage = adapters::ADAPTERS
            .iter()
            .filter(|(name, _)| cfg.agents.is_enabled(name))
            .map(|(name, _)| {
                // No model in hand, and none applies: this is one row per
                // registered HARNESS name, not a specific launch, so there is
                // no pinned model to resolve `provider_for_model` against.
                let provider = adapters::provider_for_agent_name(Some(name));
                let windows = window::load_for(state, provider)
                    .map(|w| window::available(&w, now_secs))
                    .unwrap_or_default();
                let detail_of = |w: &window::Window| ui::WindowDetail {
                    resets_at: w.resets_at,
                    limit_reached: w.limit_reached,
                    overage_covered: w.overage_covered,
                };
                ui::HarnessUsage {
                    name,
                    five_hour: windows.five_hour.as_ref().map(|w| w.used_percentage),
                    seven_day: windows.seven_day.as_ref().map(|w| w.used_percentage),
                    five_hour_detail: windows.five_hour.as_ref().map(detail_of),
                    seven_day_detail: windows.seven_day.as_ref().map(detail_of),
                }
            })
            .collect();

        // The dashboard's own orchestrator seat -- `FactsOwner::session_
        // short` is this dashboard's own registry short id (D2's own
        // "deliberately the dashboard's own identity" convention, the same
        // field every other per-dashboard disk read on this tick keys off).
        let loaded_seat = seat::load(state, session_short);
        self.disk.pool_seat = loaded_seat
            .as_ref()
            .map(|s| format!("gen {}", s.generation));
        // Dash refresh PR2: the same seat record in full, plus its own
        // rollover-runtime settlement -- two small JSON files, both already
        // being read (or immediately adjacent) on this same throttled tick,
        // never per frame.
        self.disk.rollover_record = super::rollover::runtime::load(state, session_short);
        self.disk.seat_full = loaded_seat;

        // Issue #264/#457: the aggregate row's own `failed`/`cost` cells --
        // delegations, the seat's own transcript, and its native subagent
        // transcripts, folded through the one shared function `status::
        // spend_status_line` also calls. `None` when NONE of those sources
        // exist at all, so the aggregate row renders `--` rather than a
        // phantom `0`/`$0.00`. A1-4: a handful of `stat` calls, not a full
        // read-and-re-price of every source, on the overwhelmingly common
        // tick where nothing has moved since the last one.
        let transcript = super::session_spend::resolve_transcript(state, session_short);
        self.refresh_spend_with(
            super::session_spend::fingerprint(state, transcript.as_deref()),
            cfg,
            session_short,
            || {
                let delegation_rows = super::log::read_delegations(state, usize::MAX);
                let transcript_fold =
                    super::session_spend::session_transcript_usage(transcript.as_deref(), None);
                (delegation_rows, transcript_fold)
            },
        );

        // Rebuilt rather than updated in place: a reaped pane or a released
        // registry record must drop out of the map, not linger as a stale
        // score attached to whatever short id lands there next. Every
        // sidebar row is scored -- a view-only session's transcript is
        // readable by short id and repo just like a pane's.
        self.disk.scores.clear();
        for pane in panes {
            if let Some(score) = score::cached_score(state, repo, pane.session_id(), pane.agent()) {
                self.disk.scores.insert(pane.short().to_string(), score);
            }
        }
        for (record, liveness) in &self.registry {
            if *liveness != sessions::Liveness::Live
                || self.disk.scores.contains_key(&record.short)
                // Undisplayable: `assemble_sidebar` will drop this row for
                // the same reason (a foreign dashboard's session, or an
                // unowned pre-upgrade record), so scoring it is wasted work.
                || record.owner_pid != Some(std::process::id())
            {
                continue;
            }
            if let Some(score) =
                score::cached_score(state, &record.repo, &record.session, &record.agent)
            {
                self.disk.scores.insert(record.short.clone(), score);
            }
        }

        // Issue #209/v3 codex review finding 2: `mail_by_session`, mirroring
        // the `scores` loop right above -- every attached pane by its own
        // agent/short, then every live registry row this dashboard owns.
        // Rebuilt rather than updated in place for the identical reason
        // `scores` is: a reaped pane's short must not linger with a stale
        // count once something else reuses it.
        self.disk.mail_by_session.clear();
        if cfg.mail.enabled {
            for pane in panes {
                if let Some(counts) =
                    mail::unread_counts(state, repo, pane.agent(), pane.short(), true)
                {
                    self.disk
                        .mail_by_session
                        .insert(pane.short().to_string(), counts);
                }
            }
            for (record, liveness) in &self.registry {
                if *liveness != sessions::Liveness::Live
                    || self.disk.mail_by_session.contains_key(&record.short)
                    || record.owner_pid != Some(std::process::id())
                {
                    continue;
                }
                if let Some(counts) =
                    mail::unread_counts(state, &record.repo, &record.agent, &record.short, true)
                {
                    self.disk
                        .mail_by_session
                        .insert(record.short.clone(), counts);
                }
            }
        }

        // Issue #310: `stalled`, mirroring `mail_by_session` right above --
        // every attached pane by its own `short()`, then every live registry
        // row this dashboard owns. Rebuilt rather than updated in place for
        // the identical reason: a cleared latch (or a reaped pane) must not
        // linger as a stale badge once something else reuses the short.
        self.disk.stalled.clear();
        for pane in panes {
            if sessions::stall_marker(state, pane.short()).is_some() {
                self.disk.stalled.insert(pane.short().to_string());
            }
        }
        for (record, liveness) in &self.registry {
            if *liveness != sessions::Liveness::Live
                || self.disk.stalled.contains(&record.short)
                || record.owner_pid != Some(std::process::id())
            {
                continue;
            }
            if sessions::stall_marker(state, &record.short).is_some() {
                self.disk.stalled.insert(record.short.clone());
            }
        }

        // Dash refresh PR1: each pane's OWN bound workflow, resolved from its
        // own session record by id -- mirroring `mail_by_session`/`stalled`
        // right above, the same throttled per-session disk read. Never the
        // repo-wide `active_workflow_summary` pointer above: that pointer
        // moves on any `zirv workflow start` anywhere in the repo and can
        // point at a completed run's now out-of-range step, which is exactly
        // the bug this per-session resolution replaces (see
        // `resolve_session_workflow`'s own doc comment).
        let now = super::state::now_secs();
        self.disk.workflow_by_session.clear();
        for pane in panes {
            if let Some(fact) = sessions::workflow_id_for(state, pane.short())
                .and_then(|id| resolve_session_workflow(state, repo, &id, now))
            {
                self.disk
                    .workflow_by_session
                    .insert(pane.short().to_string(), fact);
            }
        }
        for (record, liveness) in &self.registry {
            if *liveness != sessions::Liveness::Live
                || self.disk.workflow_by_session.contains_key(&record.short)
                || record.owner_pid != Some(std::process::id())
            {
                continue;
            }
            if let Some(fact) = sessions::workflow_id_for(state, &record.short)
                .and_then(|id| resolve_session_workflow(state, repo, &id, now))
            {
                self.disk
                    .workflow_by_session
                    .insert(record.short.clone(), fact);
            }
        }
        true
    }

    /// Issue #354 phase 2: re-reads the composed attention status for exactly
    /// the rows the sidebar can draw. Called only on a tick where
    /// [`FactsCache::refresh_if_due`] returned `true`, so a frame never costs
    /// a read; `load` is a seam purely so a test can count how often that
    /// actually happens.
    ///
    /// Rebuilt rather than updated in place, for the same reason `scores` and
    /// `mail_by_session` are: a reaped, un-retained pane's status must drop
    /// out of the map rather than linger against a short id something else may
    /// reuse.
    /// Issue #354 phase 5: returns the map it replaced, which is exactly the
    /// "previous projection" half the notice reducer needs -- handed over
    /// rather than cloned, so watching for transitions costs nothing.
    pub(super) fn refresh_attention(
        &mut self,
        shorts: &[String],
        load: &dyn Fn(&str) -> super::attention::SessionStatus,
    ) -> HashMap<String, super::attention::SessionStatus> {
        let previous = std::mem::take(&mut self.disk.attention);
        for short in shorts {
            self.disk.attention.insert(short.clone(), load(short));
        }
        previous
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;

    /// The glyph column must never put a file read on a frame: the statuses
    /// are loaded exactly as often as the rest of the throttled facts are,
    /// however many frames the dashboard draws in between.
    #[test]
    fn attention_statuses_are_read_on_the_facts_cadence_never_per_frame() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let cfg = CtxConfig::default();
        let shorts = vec!["aaa11111".to_string()];
        let loads = std::cell::Cell::new(0usize);
        let counting = |_: &str| -> super::super::attention::SessionStatus {
            loads.set(loads.get() + 1);
            super::super::attention::SessionStatus::default()
        };

        let start = Instant::now();
        let refresher = FakeRefresher::new(&state, &repo, &cfg);
        let mut cache = FactsCache::new(start);
        // 40 frames inside one throttle window -- the dashboard's own poll is
        // 10-50ms, so this is well under a second of real time.
        for _ in 0..40 {
            if cache.refresh_if_due(&cfg, &state, owner(&repo), &[], start, || refresher.take()) {
                cache.refresh_attention(&shorts, &counting);
            }
        }
        assert_eq!(
            loads.get(),
            1,
            "one read per throttle window, not per frame"
        );

        // The next window -- with the refresher's next cycle landing in it --
        // reads again, and only once more.
        refresher.arm();
        let later = start + FACTS_THROTTLE + Duration::from_millis(1);
        for _ in 0..40 {
            if cache.refresh_if_due(&cfg, &state, owner(&repo), &[], later, || refresher.take()) {
                cache.refresh_attention(&shorts, &counting);
            }
        }
        assert_eq!(loads.get(), 2);
        assert!(cache.disk.attention.contains_key("aaa11111"));
    }

    /// Task 7: `refresh_if_due` fills `disk.usage` with one entry per enabled
    /// harness, read straight off `window::load_for` -- a file already
    /// stored, never a rollout scan or a poll.
    #[test]
    fn refresh_if_due_reads_per_harness_usage_off_disk_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let cfg = CtxConfig::default();
        let now = Instant::now();

        super::super::window::store_for(
            &state,
            "anthropic",
            &super::super::window::UsageWindows {
                five_hour: Some(super::super::window::Window {
                    used_percentage: 55.0,
                    resets_at: 0,
                    observed_at: super::super::state::now_secs(),
                    overage_covered: false,
                    limit_reached: false,
                }),
                seven_day: None,
            },
        )
        .expect("store claude's reading");

        let mut cache = FactsCache::new(now);
        // An empty snapshot: this test is about the throttled block's own
        // `window::load_for` reads, and a real one would have the refresher's
        // pool read (`fallback::capacity_snapshot` -> `pace::refresh_sources`)
        // store a provider window of its own into this very state dir first.
        cache.refresh_if_due(&cfg, &state, owner(&repo), &[], now, || {
            Some(FactsSnapshot::default())
        });

        let claude = cache
            .disk
            .usage
            .iter()
            .find(|u| u.name == "claude")
            .expect("claude is enabled by default");
        assert_eq!(claude.five_hour, Some(55.0));
        assert_eq!(claude.seven_day, None);

        let codex = cache
            .disk
            .usage
            .iter()
            .find(|u| u.name == "codex")
            .expect("codex is enabled by default");
        assert_eq!(
            codex.five_hour, None,
            "nothing was ever stored for codex's own provider"
        );
        // Dash refresh PR1: the LIMITS block's own reset/limit/overage facts
        // (`window::Window`'s fields beyond `used_percentage`) ride along
        // too, not just the bare percentage.
        assert_eq!(
            claude.five_hour_detail,
            Some(ui::WindowDetail {
                resets_at: 0,
                limit_reached: false,
                overage_covered: false,
            })
        );
    }

    /// Coordinator scope addition: a harness the operator (or repo) has
    /// disabled -- `crate::settings::AgentGate::is_enabled` false -- must
    /// never reach `disk.usage` at all, even with a stale usage file still
    /// sitting on disk for it from before it was disabled. The LIMITS
    /// block (`ui::limits_blocks_from_usage`) only ever sees `disk.usage`,
    /// so this is also what keeps a disabled harness out of LIMITS.
    #[test]
    fn refresh_if_due_hides_a_disabled_harnesss_usage_even_with_a_stale_file_on_disk() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let now = Instant::now();

        // Codex's own stale usage file, written as if from before the
        // operator disabled it -- it must not resurrect once disabled.
        super::super::window::store_for(
            &state,
            "chatgpt",
            &super::super::window::UsageWindows {
                five_hour: Some(super::super::window::Window {
                    used_percentage: 92.0,
                    resets_at: 0,
                    observed_at: super::super::state::now_secs(),
                    overage_covered: false,
                    limit_reached: false,
                }),
                seven_day: None,
            },
        )
        .expect("store codex's stale reading");

        let env = HashMap::from([("ZIRV_AGENT_CODEX_ENABLED".to_string(), "false".to_string())]);
        let cfg = CtxConfig::load(&repo, &|k| env.get(k).cloned()).expect("load");
        assert!(
            !cfg.agents.is_enabled("codex"),
            "sanity: the env override actually disabled codex"
        );

        let mut cache = FactsCache::new(now);
        cache.refresh_if_due(&cfg, &state, owner(&repo), &[], now, || {
            Some(FactsSnapshot::default())
        });

        assert!(
            cache.disk.usage.iter().all(|u| u.name != "codex"),
            "a disabled harness's usage must not appear at all, stale file or not: {:?}",
            cache.disk.usage.iter().map(|u| u.name).collect::<Vec<_>>()
        );
        assert!(
            cache.disk.usage.iter().any(|u| u.name == "claude"),
            "an enabled harness is unaffected"
        );
        assert!(
            ui::limits_blocks_from_usage(&cache.disk.usage)
                .iter()
                .all(|b| b.harness != "codex"),
            "and therefore never reaches a LIMITS block either"
        );
    }

    /// The same rule wrap's status bar now applies: a reading whose window
    /// has provably reset must not render as a live percentage just because
    /// it is the newest thing `window::load_for` finds on disk.
    #[test]
    fn refresh_if_due_filters_out_an_expired_window_before_it_reaches_the_header() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let cfg = CtxConfig::default();
        let now = Instant::now();

        super::super::window::store_for(
            &state,
            "anthropic",
            &super::super::window::UsageWindows {
                five_hour: Some(super::super::window::Window {
                    used_percentage: 14.0,
                    resets_at: 1, // long past any real wall clock
                    observed_at: 1,
                    overage_covered: false,
                    limit_reached: false,
                }),
                seven_day: None,
            },
        )
        .expect("store an expired reading");

        let mut cache = FactsCache::new(now);
        // An empty snapshot: this test is about the throttled block's own
        // `window::load_for` reads, and a real one would have the refresher's
        // pool read (`fallback::capacity_snapshot` -> `pace::refresh_sources`)
        // store a provider window of its own into this very state dir first.
        cache.refresh_if_due(&cfg, &state, owner(&repo), &[], now, || {
            Some(FactsSnapshot::default())
        });

        let claude = cache
            .disk
            .usage
            .iter()
            .find(|u| u.name == "claude")
            .expect("claude is enabled by default");
        assert_eq!(
            claude.five_hour, None,
            "an expired window must not render as a current percent"
        );
        assert_eq!(claude.seven_day, None);
    }

    /// Issue #330: the tick's half of the facts refresh never waits on the
    /// background refresher. A refresher still mid-cycle -- the state
    /// directory it is listing is exactly what made this expensive -- leaves
    /// the facts the dashboard already had; whatever it publishes in the
    /// meantime is swapped in by the next tick that asks, newest only.
    ///
    /// `take_latest` returning at all is half the assertion: a hand-off that
    /// blocked on the refresher would hang this test rather than fail it.
    #[test]
    fn a_blocked_facts_refresher_leaves_the_tick_alone_and_lands_on_the_next_one() {
        let (tx, rx) = mpsc::channel();
        let refresher = FactsRefresher {
            group_ids: Arc::new(Mutex::new(Vec::new())),
            rx,
            stop: Arc::new(AtomicBool::new(false)),
            inline: None,
            last_inline: std::cell::Cell::new(Instant::now()),
        };
        let release = Arc::new(AtomicBool::new(false));
        let thread_release = Arc::clone(&release);
        let publisher = std::thread::spawn(move || {
            while !thread_release.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(1));
            }
            for (memory_count, unread) in [(4usize, 1usize), (7, 3)] {
                let _ = tx.send(FactsSnapshot {
                    mail: Some((unread, 0)),
                    memory_count,
                    ..FactsSnapshot::default()
                });
            }
        });

        let mut cache = FactsCache::new(Instant::now());
        cache.disk.mail = Some((0, 0));
        cache.disk.memory_count = 2;
        assert!(
            !cache.apply_snapshot(refresher.take_latest(Instant::now())),
            "a refresher mid-cycle has published nothing to swap in"
        );
        assert_eq!(
            (cache.disk.mail, cache.disk.memory_count),
            (Some((0, 0)), 2),
            "the tick keeps the facts it already had rather than blanking them"
        );

        release.store(true, Ordering::Relaxed);
        publisher.join().expect("the publisher finishes");
        assert!(cache.apply_snapshot(refresher.take_latest(Instant::now())));
        assert_eq!(
            (cache.disk.mail, cache.disk.memory_count),
            (Some((3, 0)), 7),
            "the newest snapshot wins; a tick never replays a stale backlog"
        );
    }

    /// Review finding 3: the tick asks for a snapshot on EVERY iteration
    /// now, so the inline fallback -- the arm that stands in when the
    /// refresher thread could not be spawned at all -- needs a cadence of its
    /// own. Without one it would read the whole state directory up to a
    /// hundred times a second, far worse than the once-a-second it replaced.
    #[test]
    fn the_inline_facts_fallback_collects_at_most_once_per_throttle() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let cfg = CtxConfig::default();
        let now = Instant::now();
        let (_tx, rx) = mpsc::channel();
        let refresher = FactsRefresher {
            group_ids: Arc::new(Mutex::new(Vec::new())),
            rx,
            stop: Arc::new(AtomicBool::new(false)),
            inline: Some(FactsInputs {
                state: state.clone(),
                repo: repo.clone(),
                agent_name: "claude".to_string(),
                session_short: "sess0000".to_string(),
                mail_enabled: cfg.mail.enabled,
                cfg: cfg.clone(),
            }),
            last_inline: std::cell::Cell::new(now.checked_sub(FACTS_THROTTLE).unwrap_or(now)),
        };

        assert!(
            refresher.take_latest(now).is_some(),
            "seeded a full interval in the past, the first ask collects"
        );
        assert!(
            refresher.take_latest(now + FACTS_THROTTLE / 2).is_none(),
            "every tick inside the window costs nothing at all"
        );
        assert!(
            refresher.take_latest(now + FACTS_THROTTLE).is_some(),
            "and the next window collects again"
        );
    }

    /// Codex review finding 2: `mail_by_session` reads each owned session's
    /// OWN unread mail, not the dashboard's fixed launch identity's --
    /// mirrors `refresh_if_due_scores_only_registry_records_this_dashboard_
    /// owns` above, but for mail's direct/broadcast split.
    #[test]
    fn refresh_if_due_reads_mail_by_session_for_a_registry_row_this_dashboard_owns() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let cfg = CtxConfig::default();

        let session = "5c0d0004-4444-4222-8333-555555555555";
        let record = sessions::Record::new(session, "claude", &repo, sessions::Verb::Dash);
        let short = record.short.clone();
        let _guard = sessions::SessionGuard::register(&state, record);

        // Addressed directly to `short`, not "any" -- must land in that
        // session's own `direct` count, never the dashboard's own identity
        // (`owner(&repo)` below uses `"sess0000"`, a different session
        // entirely).
        let slug = super::super::state::repo_slug(&repo);
        mail::store(
            &state,
            &slug,
            &mail::Message {
                from_session: "other".to_string(),
                from_agent: "codex".to_string(),
                to: "any".to_string(),
                to_session: Some(short.clone()),
                sent: 1,
                body: "hi".to_string(),
            },
            &cfg,
        )
        .expect("store");

        let mut cache = FactsCache::new(Instant::now());
        cache.refresh_if_due(&cfg, &state, owner(&repo), &[], Instant::now(), || {
            snapshot(&state, &repo, &cfg)
        });

        assert_eq!(
            cache.disk.mail_by_session.get(&short).copied(),
            Some((0, 1)),
            "the owned session's own direct mail: {:?}",
            cache.disk.mail_by_session
        );
        // The dashboard's own fixed identity (`sess0000`) has no mail of
        // its own here -- confirming the two are genuinely independent.
        assert!(!cache.disk.mail_by_session.contains_key("sess0000"));
    }

    /// A1-4/#457: every spend source is append-only (or entirely rewritten
    /// wholesale, for a transcript), so an unchanged fingerprint means
    /// unchanged content. Re-reading and re-pricing every source once a
    /// second, forever, bought nothing.
    #[test]
    fn the_delegation_ledger_is_re_priced_only_when_it_actually_changed() {
        use super::super::session_spend::SpendFingerprint;

        let cfg = CtxConfig::default();
        let mut cache = FactsCache::new(Instant::now());
        let mut reads = 0usize;
        let unchanged = SpendFingerprint {
            delegations: (128, 42),
            ..Default::default()
        };
        let grown = SpendFingerprint {
            delegations: (256, 43),
            ..Default::default()
        };

        for _ in 0..5 {
            cache.refresh_spend_with(unchanged, &cfg, "orch0001", || {
                reads += 1;
                (Vec::new(), Default::default())
            });
        }
        assert_eq!(
            reads, 1,
            "an unchanged fingerprint is folded once, not once per throttled tick"
        );

        cache.refresh_spend_with(grown, &cfg, "orch0001", || {
            reads += 1;
            (Vec::new(), Default::default())
        });
        assert_eq!(reads, 2, "a grown ledger is re-read and re-priced");
    }

    /// The footer says "$<spend> this session", so it has to mean what
    /// `status::spend_status_line` means by it -- the rows THIS dashboard's
    /// own session delegated -- rather than every row any session on this
    /// machine ever logged. The two disagreeing is what made `zirv ctx
    /// status` and the dashboard footer report different money.
    #[test]
    fn the_footer_spend_counts_only_this_sessions_own_delegations() {
        let cfg = CtxConfig::default();
        let mut cache = FactsCache::new(Instant::now());
        let row = |parent: &str, outcome: &str| -> super::super::log::DelegationRow {
            serde_json::from_value(serde_json::json!({
                "ts": 1_700_000_000u64,
                "session": "child001",
                "parent_session": parent,
                "agent": "claude",
                "model": "sonnet",
                "input_tokens": 0u64,
                "cache_creation_input_tokens": 0u64,
                "cache_read_input_tokens": 0u64,
                "output_tokens": 1_000_000u64,
                "wall_ms": 1000u64,
                "exit_code": 0,
                "outcome": outcome,
            }))
            .expect("row")
        };

        cache.refresh_spend_with(
            super::super::session_spend::SpendFingerprint::default(),
            &cfg,
            "orch0001",
            || {
                (
                    vec![row("orch0001", "ok"), row("other999", "failed")],
                    Default::default(),
                )
            },
        );

        let spend = cache.disk.spend.expect("the owner's own row is not empty");
        assert_eq!(
            spend.cost_micros, 15_000_000,
            "only the owner's own 1M sonnet output tokens ($15) may be counted"
        );
        assert_eq!(
            spend.failed, 0,
            "another session's failed delegation is not this footer's failure"
        );
    }
}
