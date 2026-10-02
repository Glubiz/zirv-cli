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
    /// Read the repository's active workflow on the throttled tick; absence and read failure both render as unavailable (#209).
    pub(super) workflow: Option<workflow::ActiveWorkflowSummary>,
    /// Track unread mail per session because the aggregate mail count cannot drive the focused footer (#209).
    pub(super) mail_by_session: MailMap,
    /// Track stall latches for attached and registry sessions; absence means no armed stall (#310).
    pub(super) stalled: HashSet<String>,
    /// Resolve each session's bound workflow on refresh; absence never implies
    /// a repo-wide workflow belongs to it.
    pub(super) workflow_by_session: HashMap<String, ui::SessionWorkflowFact>,
    /// Read aggregate spend on the throttled tick; no ledger means unknown, not zero (#264).
    pub(super) spend: Option<AggregateSpendFacts>,
    /// Build the configured provider pool strip on the background refresher because capacity snapshots scan session state (#358).
    pub(super) pool_harnesses: Vec<ui::HarnessStrip>,
    /// This dashboard's own orchestrator seat's `"gen N"` label (`seat::
    /// load`, keyed by `FactsOwner::session_short`), `None` until a seat is
    /// registered for it.
    pub(super) pool_seat: Option<String>,
    /// Keep the full seat beside its generation label from the same throttled read.
    pub(super) seat_full: Option<super::seat::Seat>,
    /// Read rollover state beside the seat; `None` means no record for this seat.
    pub(super) rollover_record: Option<super::rollover::runtime::Record>,
    /// Refresh JEV on its own slower cadence; `None` hides it when all gates are off.
    pub(super) jev: Option<ui::JevSectionFact>,
    /// Load each live work group in the background, never on a render frame (#354).
    pub(super) groups: HashMap<String, super::group::WorkGroup>,
    /// Keep observed state-change time here, not on Pane: it describes the
    /// dashboard's observation and must disappear when the pane is reaped (#354).
    pub(super) state_since: HashMap<String, (ui::RowState, u64)>,
    /// Cache one composed attention status per drawable row on refresh, never per frame; absent or corrupt status projects Unknown (#354).
    pub(super) attention: HashMap<String, super::attention::SessionStatus>,
}

/// Aggregate spend includes the seat transcript, native subagents and delegation ledger through the shared fold (#264, #457).
#[derive(Debug, Clone, Copy)]
pub(super) struct AggregateSpendFacts {
    pub(super) failed: u64,
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
    /// Build provider strips on the refresher thread because capacity snapshots may scan the registry and rollout tree (#358).
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
    // Load each distinct live work group once for sidebar headers, never per frame (#354).
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

/// Read repo-wide provider capacity on the refresher thread; it scans stored usage and sessions but makes no outbound request (#358).
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
            // Show the allocator's stale reading in the pool strip; stale usage does not become hard-gate authority.
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
    /// Compute a snapshot inline only if the refresher thread could not start, so the sidebar still has facts.
    inline: Option<FactsInputs>,
    /// Throttle the inline fallback independently because the tick probes it on every iteration.
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

    /// Take only the latest snapshot without blocking the UI tick; the inline
    /// fallback uses its own cadence so failed thread startup cannot cause scans every tick.
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
        // Display unknown spend only when neither transcript nor delegation source exists (#457).
        self.disk.spend = spend.cost_micros.map(|_| AggregateSpendFacts {
            failed: spend.delegation_failed,
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

    /// Swap background facts on every tick, independent of the disk throttle; refresh attention whenever facts change so rows and registry stay aligned (#354).
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

        // Update state-change time only when the row changes and prune entries for reaped panes (#354).
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
        // Read the dashboard repository's singleton workflow; worktree panes share this repository's session and state store (#119, #209).
        self.disk.workflow = workflow::active_workflow_summary(state, repo);

        // Load one entry per enabled harness from files; refresh must never scan transcripts.
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
        // Read seat and rollover records on the throttled tick, never per frame.
        self.disk.rollover_record = super::rollover::runtime::load(state, session_short);
        self.disk.seat_full = loaded_seat;

        // Fold changed spend sources only; absence of every source renders unknown rather than a fabricated zero (#264, #457).
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

        // Rebuild mail counts for attached and registry sessions so reaped shorts cannot retain stale values (#209).
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

        // Rebuild stall latches for attached and registry sessions so cleared or reaped entries disappear (#310).
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

        // Resolve each pane's bound workflow by session ID; the repo-wide active pointer can change independently.
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

    /// Refresh only drawable rows on a facts tick, and return the previous map for transition notices; rebuilding prunes stale shorts (#354).
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
            spend.failed, 0,
            "another session's failed delegation is not this footer's failure"
        );
    }
}
