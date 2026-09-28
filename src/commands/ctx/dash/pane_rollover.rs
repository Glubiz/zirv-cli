//! Error log plus the pane rollover/handover sweep.
use super::*;

/// Issue #354 phase 5: one kept error, collapsed over identical CONSECUTIVE
/// repeats. A supervisor that fails the same way once a second used to evict
/// the whole five-entry buffer in five seconds, taking every other error with
/// it and telling the operator nothing they did not already know; one entry
/// with a count says strictly more in one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ErrorEntry {
    /// Monotonic within one dashboard run. A1-3: `Ctrl+A e` acknowledges the
    /// entries its own snapshot covered, and an id is the only stable way to
    /// name them -- an index moves the moment the buffer cap drops an entry
    /// off the front while the dialog is open.
    id: u64,
    pub(super) text: String,
    /// How many times in a row this exact message was pushed. `1` is the
    /// ordinary case and renders no count at all.
    pub(super) count: usize,
    /// When the most recent repeat arrived -- the age the dialog shows.
    pub(super) last: Instant,
    /// Acknowledged by the operator (`Esc`/`a` on the errors dialog). An
    /// acknowledged entry is never deleted: it stays listed, dimmed, until
    /// the buffer cap drops it, and only stops holding the sticky header
    /// line.
    pub(super) acked: bool,
}

/// The dashboard's kept-errors buffer: `push_error`'s storage, the sticky
/// `\u{26a0}` header line's source, and what `Ctrl+A e` lists.
///
/// The whole decision half is pure and clock-injected ([`ErrorLog::record`]);
/// `push_error` is the thin impure wrapper the hot path calls, so the collapse
/// and acknowledgement rules are testable without a terminal.
#[derive(Debug, Default)]
pub(crate) struct ErrorLog {
    pub(super) entries: Vec<ErrorEntry>,
    next_id: u64,
}

impl ErrorLog {
    /// Pure: records one error as of `now`.
    ///
    /// Collapses onto the newest entry when it is the same message AND has
    /// not been acknowledged. The acknowledgement check is what makes "the
    /// same failure, again, after I said I had seen it" news again rather
    /// than a silent `\u{d7}n` bump nothing draws the operator's eye to.
    pub(super) fn record(&mut self, message: String, now: Instant) {
        if let Some(last) = self.entries.last_mut()
            && !last.acked
            && last.text == message
        {
            last.count += 1;
            last.last = now;
            return;
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.entries.push(ErrorEntry {
            id,
            text: message,
            count: 1,
            last: now,
            acked: false,
        });
        if self.entries.len() > MAX_KEPT_ERRORS {
            let drop = self.entries.len() - MAX_KEPT_ERRORS;
            self.entries.drain(0..drop);
        }
    }

    /// The id the NEXT recorded error will take -- what a dialog snapshot
    /// stores so acknowledging it later covers exactly the entries it showed.
    pub(super) fn mark(&self) -> u64 {
        self.next_id
    }

    /// Pure: marks acknowledged every entry the snapshot taken at `mark`
    /// covered. Never deletes anything.
    ///
    /// A1-3: this used to acknowledge the whole buffer, including errors
    /// pushed AFTER the dialog took its snapshot -- clearing the sticky
    /// `\u{26a0}` for failures that were never on screen. An entry that
    /// arrived after the snapshot keeps holding the line.
    pub(super) fn acknowledge(&mut self, mark: u64) {
        for entry in &mut self.entries {
            if entry.id < mark {
                entry.acked = true;
            }
        }
    }

    /// Pure: how many unacknowledged entries the sticky header line counts.
    pub(super) fn sticky_count(&self) -> usize {
        self.entries.iter().filter(|e| !e.acked).count()
    }

    /// Pure: the sticky header line's own text -- the newest unacknowledged
    /// message, with its repeat count when it has one. `None` once everything
    /// has been acknowledged, which is exactly how the line clears.
    pub(super) fn sticky_line(&self) -> Option<String> {
        self.entries
            .iter()
            .rev()
            .find(|e| !e.acked)
            .map(|e| ui::error_line(&e.text, e.count))
    }

    /// Every kept message, oldest first, with its repeat count folded in --
    /// what the all-panes-ended scrollback dump prints.
    pub(super) fn messages(&self) -> impl Iterator<Item = String> + '_ {
        self.entries
            .iter()
            .map(|e| ui::error_line(&e.text, e.count))
    }

    /// The raw messages, oldest first, without their counts -- what a caller
    /// filtering for one session's own lines matches against.
    pub(super) fn iter(&self) -> impl DoubleEndedIterator<Item = &str> {
        self.entries.iter().map(|e| e.text.as_str())
    }
}

/// The shorthands the tests read the buffer back with -- the production code
/// only ever pushes, acknowledges and renders, so these live behind
/// `cfg(test)` rather than as dead public surface.
#[cfg(test)]
impl ErrorLog {
    /// How many distinct messages are kept (repeats of one message are one).
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(super) fn first(&self) -> Option<&str> {
        self.entries.first().map(|e| e.text.as_str())
    }

    pub(super) fn last(&self) -> Option<&str> {
        self.entries.last().map(|e| e.text.as_str())
    }
}

#[cfg(test)]
impl std::ops::Index<usize> for ErrorLog {
    type Output = str;

    fn index(&self, index: usize) -> &str {
        &self.entries[index].text
    }
}

/// Impure only in its clock: the hot path's own entry point, kept at the
/// exact signature every call site already uses.
pub(super) fn push_error(errors: &mut ErrorLog, message: String) {
    errors.record(message, Instant::now());
}

/// The dashboard's own successor backend (issue #552).
///
/// Two backends, one per successor runtime:
///
/// * a HARNESS successor is the in-place pty swap `Pane::handover` has always
///   performed -- one child replaced under one pane, the pane's own identity
///   untouched;
/// * a NATIVE successor cannot be an in-place child replacement, because a
///   native pane has no child: it is an in-process session with its own
///   journal. So it is opened first (`Pane::spawn_native`, on THIS seat's
///   short id and the committed generation) and the source pane is retired
///   only once the successor exists. Exactly one of the two is ever live: the
///   successor is built before anything is taken away, and a failure to build
///   it leaves the source untouched and holding the seat.
///
/// Nothing here is `#[cfg(unix)]`; both halves compile and run on every
/// platform CI covers.
pub(super) struct PaneSuccessorLauncher<'a> {
    pub(super) pane: &'a mut Pane,
    pub(super) cfg: &'a CtxConfig,
    pub(super) req: &'a handover::HandoverRequest,
    pub(super) note: &'a handoff::Handoff,
    pub(super) role: prompt::PromptRole,
    pub(super) repo: &'a Path,
    pub(super) size: (u16, u16),
    /// The two `NativeDashboardSpec` fields a successor cannot infer.
    /// `Default` is what every production call site passes; a deterministic
    /// test opens a REAL successor pane against `fixture::FixtureProvider`
    /// in a bare temp repo, the same escape `NativeDashboardSpec::provider`
    /// already documents for itself.
    pub(super) native: super::rollover_runtime::NativeSuccessorSpec,
}

impl super::rollover_runtime::SuccessorLauncher for PaneSuccessorLauncher<'_> {
    fn defers_settlement(&self) -> bool {
        self.req.generation.is_some()
            && !self.pane.is_native()
            && self.req.successor_runtime() == super::runtime::RuntimeKind::Harness
    }

    fn launch(
        &mut self,
        plan: &super::rollover_runtime::SuccessorPlan,
    ) -> Result<String, super::rollover_runtime::SuccessorRefusal> {
        use super::rollover_runtime::SuccessorRefusal;

        if plan.to != super::runtime::RuntimeKind::Native {
            // A WRAPPED source swaps its child in place: one pane, one
            // identity, a new harness underneath it.
            if !self.pane.is_native() {
                return self
                    .pane
                    .handover(
                        self.cfg, self.req, self.note, self.role, self.repo, self.size,
                    )
                    .map(|()| self.pane.session_id().to_string())
                    .map_err(|e| SuccessorRefusal::LaunchFailed(e.to_string()));
            }
            // A NATIVE source has no child to swap, so the harness successor
            // is opened beside it and the source retired afterwards -- the
            // same open-then-retire shape the native branch below uses, over
            // the same `Pane::build_swap_launch` derivation an in-place swap
            // runs on.
            let state = self.pane.state_dir().clone();
            let session_id = uuid::Uuid::new_v4().to_string();
            let launch = self
                .pane
                .build_swap_launch(
                    self.cfg,
                    self.req,
                    self.note,
                    self.role,
                    self.repo,
                    &session_id,
                    // The socket `Pane::spawn_on_seat` goes on to bind for
                    // this identity, so the child is told where to report
                    // before the pane exists to bind it.
                    Some(&state.socket_for(&session_id)),
                    self.pane.title().to_string(),
                )
                .map_err(|e| SuccessorRefusal::LaunchFailed(e.to_string()))?;
            let successor = Pane::spawn_on_seat(
                launch.spec,
                &state,
                self.pane.cwd(),
                self.repo,
                self.size,
                &launch.turn_env,
                launch.turn_signal_capable,
                launch.idle_quiet,
                Some(&plan.short),
            )
            .map_err(|e| SuccessorRefusal::LaunchFailed(e.to_string()))?;
            return Ok(self.retire_source_for(successor));
        }

        // Built BEFORE anything is taken away: a failure here leaves the
        // source pane exactly as it was, still holding the seat with all of
        // its durable state (`rollover_runtime`'s own item 7).
        let state = self.pane.state_dir().clone();
        let successor = super::rollover_runtime::launch_native_pane(
            self.cfg,
            &state,
            self.repo,
            self.pane.cwd(),
            self.pane.verb(),
            self.pane.title().to_string(),
            self.size,
            self.role,
            plan,
            self.note,
            &self.native,
        )?;

        Ok(self.retire_source_for(successor))
    }
}

impl PaneSuccessorLauncher<'_> {
    /// Swaps `successor` into the roster slot the source occupies and retires
    /// the source, returning the successor's own session identity.
    ///
    /// One live successor from here on. The source is retired WITHOUT
    /// releasing the registry record or the seat, both of which the successor
    /// has just adopted under the same short id -- see
    /// `Pane::retire_for_successor`.
    pub(super) fn retire_source_for(&mut self, successor: Pane) -> String {
        let session = successor.session_id().to_string();
        let mut source = std::mem::replace(self.pane, successor);
        // The SOURCE's own harness quit sequence -- a native source has no
        // child to ask politely and ignores it.
        let quit_sequence = adapters::select(Some(source.agent()), &[], self.cfg)
            .map(|adapter| adapter.quit_sequence().to_string())
            .unwrap_or_default();
        source.retire_for_successor(&quit_sequence);
        session
    }
}

/// Issue #84: the `Ctrl+A o` picker's confirm action. Distills a handoff
/// packet through the exact same machinery `wrap::perform_handover_swap`
/// uses (`handoff::distill_or_structural` against the pane's own current
/// adapter/transcript, never a parallel format), then hands it to `Pane::
/// handover`, which resolves the new adapter/argv/turn-env and performs the
/// actual pty swap. The pane keeps its registry short id throughout (`Pane::
/// handover` never re-registers), which is what keeps mail and `zirv ctx
/// nudge` addressed to it valid across the swap.
///
/// Issue #358 (task 5): takes an already-built `HandoverRequest` rather than
/// a target agent/model pair, so an automatically decided rollover
/// (`rollover::evaluate`) and the operator's own picker are executed by the
/// exact same code -- including the seat transaction `req.generation` names.
/// Returns whether the swap actually happened.
#[allow(clippy::too_many_arguments)]
pub(super) fn handover_pane(
    pane: &mut Pane,
    req: &handover::HandoverRequest,
    cfg: &CtxConfig,
    repo: &Path,
    state: &StateDir,
    errors: &mut ErrorLog,
    // Issue #440: whose transcript the outgoing context is read from, as
    // `(agent, session id)`. `None` -- every ordinary swap -- means this
    // pane's own current agent and session, which is what is leaving. A
    // SOURCE recovery is the exception: the pane is running the dead
    // successor by then, so reading `pane.agent()` there would build the
    // packet from the successor's own (usually empty) transcript rather than
    // from the source whose context is the thing actually being carried.
    context: Option<(&str, &str)>,
) -> bool {
    let (old_agent_name, context_session) = match context {
        Some((agent, session)) => (agent.to_string(), session.to_string()),
        None => {
            let conversation = sessions::native_conversation(
                state,
                pane.short(),
                pane.agent(),
                pane.session_id(),
                runtime::RuntimeKind::Harness,
            )
            .unwrap_or_else(|| pane.session_id().to_string());
            (pane.agent().to_string(), conversation)
        }
    };
    let Ok(old_adapter) = adapters::select(Some(&old_agent_name), &[], cfg) else {
        let reason = format!("could not resolve the outgoing agent '{old_agent_name}'");
        if let Some(generation) = req.generation {
            let _ = super::rollover::fail(
                state,
                "dash",
                pane.short(),
                generation,
                &reason,
                super::state::now_secs(),
            );
        }
        push_error(errors, format!("handover: {reason}"));
        return false;
    };
    let transcript_path = old_adapter.transcript_path(&SessionRef {
        id: SessionId::parse(&context_session),
        cwd: repo.to_path_buf(),
    });
    let jsonl = std::fs::read_to_string(&transcript_path).unwrap_or_default();
    let ctx = old_adapter.structural_context(&jsonl, cfg.handoff.tail_items);
    if let Some(generation) = req.generation
        && ctx.user_messages.is_empty()
        && !pane.is_native()
    {
        let reason = "no user task found in the source transcript; original session retained";
        super::rollover::fail(
            state,
            "dash",
            pane.short(),
            generation,
            reason,
            super::state::now_secs(),
        );
        push_error(errors, format!("handover: {reason}"));
        return false;
    }
    let distiller_model =
        handoff::resolve_distiller_model(cfg.handoff.model.as_deref(), old_adapter.as_ref());
    let previous = handoff::latest_for_repo(state, repo)
        .ok()
        .flatten()
        .map(|(_, h)| h);
    // Issue #358: a reactive rollover fires because this provider stopped
    // answering; the distiller call would run against that same provider.
    let (note, _source) = if req.structural_only {
        (handoff::structural(&ctx), "structural")
    } else {
        handoff::distill_or_structural(
            old_adapter.as_ref(),
            &distiller_model,
            &ctx,
            Duration::from_secs(cfg.handoff.timeout_secs),
            cfg.chrome.events,
            previous.as_ref(),
        )
    };

    let role = if pane.verb() == sessions::Verb::Chat {
        prompt::PromptRole::Orchestrator
    } else {
        prompt::PromptRole::Worker
    };
    let size = pane.screen().size();
    // Issue #552: every live swap starts its successor through the ONE
    // production seam, `rollover_runtime::launch_successor` -- so the
    // direction (harness->harness, harness->native, native->harness,
    // native->native) decides which backend runs, this seat's subagents are
    // settled before anything takes the seat, and an ambiguous tool effect
    // halts the successor instead of being replayed by it.
    let from = if pane.is_native() {
        super::runtime::RuntimeKind::Native
    } else {
        super::runtime::RuntimeKind::Harness
    };
    let plan = super::rollover_runtime::plan_successor(
        from,
        req.successor_runtime(),
        pane.short(),
        req.generation
            .or_else(|| super::seat::load(state, pane.short()).map(|seat| seat.generation))
            .unwrap_or(1),
        Some(&req.target_agent),
        req.target_model.as_deref(),
        req.target_route.as_deref(),
        req.resume_session.as_deref(),
        super::rollover_runtime::load(state, pane.short())
            .and_then(|record| record.boundary)
            .as_ref(),
    );
    let parent_session = pane.session_id().to_string();
    let mut launcher = PaneSuccessorLauncher {
        pane,
        cfg,
        req,
        note: &note,
        role,
        repo,
        size: (size.1, size.0),
        native: super::rollover_runtime::NativeSuccessorSpec::default(),
    };
    match super::rollover_runtime::launch_successor(
        state,
        repo,
        &mut launcher,
        &plan,
        Some(&parent_session),
        if req.structural_only {
            super::rollover_runtime::Drain::Forced
        } else {
            super::rollover_runtime::Drain::Quiesced
        },
        super::state::now_secs(),
    ) {
        Ok(_) => true,
        Err(e) => {
            // `Pane::handover` assembles the successor completely before it
            // touches the old child, so a failure here leaves the pane
            // exactly as it was -- the seat transaction is a clean abort.
            if let Some(generation) = req.generation {
                let _ = super::rollover::fail(
                    state,
                    "dash",
                    pane.short(),
                    generation,
                    &e.to_string(),
                    super::state::now_secs(),
                );
            }
            push_error(errors, format!("handover: {e}"));
            false
        }
    }
}

/// Issue #358 (task 5): one automatic rollover evaluation for the
/// dashboard's own orchestrator pane. The evaluation, the seat transaction
/// and the swap all go through the same seams a manual `Ctrl+A o` does; the
/// only difference is who decided. A parked seat is asked first, in case its
/// window has elapsed and the best harness is no longer its own.
#[allow(clippy::too_many_arguments)]
pub(super) fn rollover_sweep(
    panes: &mut [Pane],
    cfg: &CtxConfig,
    repo: &Path,
    state: &StateDir,
    pending: &mut Option<(String, u64, Instant)>,
    errors: &mut ErrorLog,
    // Coordinator follow-up: the footer's rollover distance/soon reading
    // must be the SAME `source_headroom_pct` this evaluation computed, not
    // a separate guess -- captured here (via `evaluate`'s own out-param)
    // and kept across ticks by the caller so the footer can read it on
    // frames this sweep does not itself run on. Left untouched (not
    // cleared) on any tick this function returns before calling `evaluate`
    // at all (e.g. the `on_resume`/parked-return path) -- the last real
    // reading is still the best answer until a fresh one replaces it.
    seat_headroom_pct: &mut Option<SeatHeadroom>,
) {
    let Some(idx) = panes
        .iter()
        .position(|pane| pane.role() == prompt::PromptRole::Orchestrator)
    else {
        return;
    };
    let short = panes[idx].short().to_string();
    // The seat in full: its currently-registered model (`Seat::model`,
    // stamped by the same `seat::register` call `Pane::spawn`/`Pane::
    // handover` make), for a point-in-time rollover-eligibility read, and
    // its own `generation`, which is what tags whatever headroom this tick
    // computes below (review fix: a pane's registry short id survives a
    // handover unchanged, so `short` alone cannot tell an old seat from a
    // new one at the same address -- only `generation` advances).
    let loaded_seat = super::seat::load(state, &short);
    let seat_generation = loaded_seat.as_ref().map(|seat| seat.generation);
    let seat_model = loaded_seat.and_then(|seat| seat.model);
    let provider =
        adapters::provider_for_agent_and_model(Some(panes[idx].agent()), seat_model.as_deref())
            .to_string();
    let idle = panes[idx].state() == PaneState::Idle;
    let now = super::state::now_secs();

    let req = match super::rollover::on_resume(state, cfg, "dash", &short, now, true) {
        Some(req) => req,
        None => {
            let blocked = super::rollover::confirmed_block(state, cfg, now, &provider, &short);
            let mut headroom_out: Option<f64> = None;
            let evaluation = super::rollover::evaluate(
                state,
                cfg,
                "dash",
                &short,
                now,
                idle,
                blocked,
                true,
                &mut headroom_out,
            );
            // Cache the reading -- tagged with the generation it was
            // actually read against -- regardless of what this evaluation
            // decided: Skip/Pending/Park all still computed a real,
            // current headroom worth showing. A Rollover decision is
            // cleared right below instead, before the handover itself.
            if let (Some(pct), Some(generation)) = (headroom_out, seat_generation) {
                *seat_headroom_pct = Some(SeatHeadroom {
                    short: short.clone(),
                    generation,
                    pct,
                });
            }
            match evaluation {
                super::rollover::Evaluation::Rollover { request, .. } => request,
                _ => return,
            }
        }
    };
    // Review fix: a handover is about to be attempted (from `on_resume` or
    // the fresh `Rollover` decision just above) -- `evaluate`'s own
    // `seat::prepare_onto` already advanced the on-disk seat to `Phase::
    // Prepared` under a NEW generation before this point, so whatever
    // headroom is cached for the OLD generation is stale the instant it is
    // cleared here, not merely once the identity check downstream happens
    // to notice.
    *seat_headroom_pct = None;
    // The seat transaction is already open, so a pane that is no longer at a
    // clean boundary has to close it rather than leave it prepared -- the
    // same rule `wrap`'s own refusal arm follows.
    if !idle && !req.force {
        if let Some(generation) = req.generation {
            let _ = super::rollover::fail(
                state,
                "dash",
                &short,
                generation,
                "the orchestrator pane is mid-turn",
                now,
            );
        }
        return;
    }
    if handover_pane(&mut panes[idx], &req, cfg, repo, state, errors, None)
        && let Some(generation) = req.generation
    {
        *pending = Some((short, generation, Instant::now()));
    }
}

/// The other half: an open rollover transaction is committed only once the
/// successor pane has actually answered, and aborted when it never does.
/// `Pane::handover` spawns the successor before it touches the old child, so
/// a pane that has already ENDED here is the only genuine "it never came up"
/// case; a timeout leaves the (live but silent) successor alone rather than
/// killing the operator's own orchestrator pane.
pub(super) fn settle_pending_rollover(
    panes: &mut [Pane],
    cfg: &CtxConfig,
    repo: &Path,
    state: &StateDir,
    pending: &mut Option<(String, u64, Instant)>,
    errors: &mut ErrorLog,
) {
    let Some((short, generation, started)) = pending.clone() else {
        return;
    };
    let Some(pane) = panes.iter_mut().find(|pane| pane.short() == short) else {
        let _ = super::rollover::fail(
            state,
            "dash",
            &short,
            generation,
            "the successor pane is gone",
            super::state::now_secs(),
        );
        *pending = None;
        return;
    };
    if pane.has_pending_handover() {
        let (readiness, reason) = pane.poll_handover(Duration::from_secs(cfg.handoff.timeout_secs));
        let now = super::state::now_secs();
        match readiness {
            super::rollover::Readiness::Waiting => return,
            super::rollover::Readiness::Ready => {
                if let Err(error) = pane.commit_handover(repo, generation) {
                    push_error(errors, format!("rollover failed: {error}"));
                }
            }
            super::rollover::Readiness::Dead | super::rollover::Readiness::TimedOut => {
                pane.cancel_handover();
                super::rollover::fail(state, "dash", &short, generation, &reason, now);
                push_error(
                    errors,
                    format!("rollover failed -- original session kept running: {reason}"),
                );
            }
        }
        *pending = None;
        return;
    }
    let pane_state = pane.state();
    let readiness = super::rollover::successor_readiness(
        !matches!(pane_state, PaneState::Ended(_)),
        true,
        pane_state == PaneState::Idle,
        pane_state == PaneState::Idle,
        Instant::now().saturating_duration_since(started),
        Duration::from_secs(cfg.handoff.timeout_secs),
    );
    let now = super::state::now_secs();
    match readiness {
        super::rollover::Readiness::Ready => {
            let _ =
                super::rollover::commit(state, "dash", &short, generation, pane.session_id(), now);
            *pending = None;
        }
        super::rollover::Readiness::Dead => {
            // Issue #440: `Pane::handover` has already quit the source child
            // by the time a successor can die, so without this arm the
            // dashboard simply reaped the pane and the operator's
            // orchestrator was gone -- the incident this whole fix exists
            // for. The seat still names the SOURCE (a failed rollover never
            // commits), so relaunching it in this same pane, through the very
            // seam the swap itself used, gives the seat its own harness back
            // at the same short id. Structural handoff only: the source is
            // the harness that just proved it cannot answer a distiller call.
            let source = super::seat::load(state, &short);
            let reactive = source.as_ref().is_some_and(|seat| {
                matches!(
                    &seat.phase,
                    super::seat::Phase::Prepared {
                        cause: super::seat::Cause::Reactive { .. },
                        ..
                    }
                )
            });
            // Issue #462: preserve WHY the successor exited. Recovery below
            // can fail in its own right -- in the incident the restored pane
            // died immediately on a resume of an id its harness had never
            // heard of -- and a reason string that only ever said "the
            // successor exited before it answered" left the initial failure
            // unexplained in the retained evidence.
            let mut reason = "the successor exited before it answered".to_string();
            if let PaneState::Ended(code) = pane_state {
                reason.push_str(&format!(" (exit {code})"));
            }
            let tail = pane.last_line().trim().to_string();
            if !tail.is_empty() {
                reason.push_str(": ");
                reason.extend(tail.chars().take(160));
            }
            super::rollover::fail(state, "dash", &short, generation, &reason, now);
            *pending = None;
            // Issue #440: resume the source's OWN conversation where the
            // adapter has a verified mechanism for it. A cold relaunch
            // carrying a structural packet cannot carry unsaved in-flight
            // state, and that state is exactly what the incident lost.
            //
            // Issue #462: the id resumed here is the one the HARNESS knows,
            // which equals zirv's seat uuid only when the launch was pinned
            // (`AgentAdapter::session_pin_args`). A `zirv chat -- --resume
            // <id>` launch deliberately suppresses that pin, so the seat's
            // uuid is a zirv-side handle no `--resume` can resolve: resuming
            // it blind is what killed the restored pane and closed the
            // orchestrator outright. Preference order is therefore the
            // conversation a lifecycle hook actually OBSERVED for this seat
            // (`sessions::native_conversation`), then the seat uuid -- and
            // either one only when the adapter cannot prove that
            // conversation is absent.
            let resume = source.as_ref().and_then(|seat| {
                let adapter = adapters::select(Some(&seat.agent), &[], cfg).ok()?;
                match super::sessions::native_conversation(
                    state,
                    &short,
                    &seat.agent,
                    &seat.session,
                    runtime::RuntimeKind::Harness,
                ) {
                    // A lifecycle hook OBSERVED this conversation for this
                    // exact seat and zirv session: the strongest evidence
                    // there is, and it needs no probe.
                    Some(observed) => adapter.resume_args(&observed).map(|_| observed),
                    // Nothing observed (no turn boundary reached this seat
                    // yet, or an unsupervised launch). The seat uuid is only
                    // the conversation id when the launch was PINNED, so it
                    // is used only where the adapter cannot prove that
                    // conversation is absent -- `None` from
                    // `conversation_exists` means "cannot tell", which keeps
                    // the previous behaviour, while a proven-absent
                    // conversation downgrades to a cold structural relaunch
                    // instead of a resume the harness would reject.
                    None => {
                        adapter.resume_args(&seat.session)?;
                        let exists = adapter.conversation_exists(&SessionRef {
                            id: SessionId::parse(&seat.session),
                            cwd: repo.to_path_buf(),
                        });
                        (exists != Some(false)).then(|| seat.session.clone())
                    }
                }
            });
            let restored = source
                .filter(|seat| !seat.agent.eq_ignore_ascii_case(pane.agent()))
                .is_some_and(|seat| {
                    // Issue #462: the outgoing context is read from the
                    // conversation actually being resumed, not from zirv's
                    // seat uuid -- `handover_pane` resolves the transcript
                    // from this value, and for an unpinned launch the seat
                    // uuid names no transcript at all.
                    let context_session = resume.clone().unwrap_or_else(|| seat.session.clone());
                    handover_pane(
                        pane,
                        &handover::HandoverRequest {
                            target_agent: seat.agent.clone(),
                            target_model: seat.model.clone(),
                            force: true,
                            requested_at: now,
                            interactive: true,
                            automatic: true,
                            // No seat transaction: the seat already names this
                            // agent, so there is nothing to prepare or commit.
                            generation: None,
                            structural_only: true,
                            resume_session: resume.clone(),
                            target_runtime: None,
                            target_route: None,
                        },
                        cfg,
                        repo,
                        state,
                        errors,
                        Some((seat.agent.as_str(), context_session.as_str())),
                    )
                });
            // A source that is hard-blocked waits out its own window rather
            // than being re-rolled on the next sweep. A proactive rollover is
            // left alone: it still has candidates worth trying, and parking
            // would strand the seat until a reset it never needed.
            let parked = reactive
                .then(|| {
                    super::rollover::park_source(
                        state,
                        cfg,
                        "dash",
                        &short,
                        now,
                        "the rollover successor exited before it answered",
                    )
                })
                .flatten();
            let outcome = match (restored, resume.is_some()) {
                (true, true) => "the previous session is resumed here",
                (true, false) => "the previous harness is relaunched here",
                (false, _) => "the previous harness could not be brought back",
            };
            let seat_state = match parked {
                Some(until) => format!(
                    "parked until the limit resets in {}",
                    crate::style::format_age(until.saturating_sub(now))
                ),
                None => "the seat is free to try another harness".to_string(),
            };
            push_error(
                errors,
                format!("rollover failed -- {outcome}; {seat_state}"),
            );
        }
        super::rollover::Readiness::TimedOut => {
            // Finding #13 (issue #358 review): unlike `wrap`'s identical
            // arm, this used to leave the on-disk seat naming the OLD
            // predecessor after the transaction aborted -- the pane itself
            // is kept alive running the SUCCESSOR (killing a live-but-
            // silent successor here would end the operator's own dashboard
            // pane outright, the same DEVIATION `wrap`'s own timeout arm
            // documents), so the seat record and the actually-running pane
            // disagreed about which agent was answering at this address
            // until the next rollover happened to overwrite it. Peek the
            // model `seat::prepare` recorded before `rollover::fail`/`seat::
            // abort` discards the `Prepared` phase, then re-register onto
            // the successor that is, in fact, running -- mirroring wrap's
            // own re-registration exactly.
            let successor_model =
                super::seat::load(state, &short).and_then(|seat| match seat.phase {
                    super::seat::Phase::Prepared {
                        successor_model, ..
                    } => successor_model,
                    _ => None,
                });
            let _ = super::rollover::fail(
                state,
                "dash",
                &short,
                generation,
                "the successor did not answer within handoff.timeout_secs",
                now,
            );
            let _ = super::seat::register(
                state,
                &short,
                pane.session_id(),
                pane.agent(),
                successor_model.as_deref(),
                adapters::provider_for_agent_and_model(
                    Some(pane.agent()),
                    successor_model.as_deref(),
                ),
                pane.role().label(),
                false,
                now,
            );
            *pending = None;
        }
        super::rollover::Readiness::Waiting => {}
    }
}

/// How long a transient header notice stays on screen before it expires (L13).
/// Issue #354 phase 3: how close together two clicks on the same dialog row
/// have to be to count as a double-click (activate) rather than two
/// selections. The usual desktop default; a slow second click simply
/// re-selects the row it is already on, which is a no-op.
pub(super) const DOUBLE_CLICK: Duration = Duration::from_millis(400);

pub(super) const NOTICE_TTL: Duration = Duration::from_secs(4);

/// Issue #354 phase 5: how wide an attention notice may be built.
///
/// The header's middle slot is whatever is left after the fixed chrome and
/// the hint cluster, which `header_layout` truncates to anyway -- this is the
/// reducer's own clamp so a notice is never *composed* longer than the middle
/// can ever be at the dashboard's minimum eligible width. Anything wider
/// would only be ellipsised twice.
pub(super) const NOTICE_MAX_COLS: usize = 48;

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;

    #[test]
    fn push_error_keeps_only_the_most_recent_handful() {
        let mut errors = ErrorLog::default();
        for i in 0..10 {
            push_error(&mut errors, format!("err{i}"));
        }
        assert_eq!(errors.len(), MAX_KEPT_ERRORS);
        assert_eq!(errors.last().unwrap(), "err9");
        assert_eq!(errors.first().unwrap(), "err5");
    }

    /// Issue #354 phase 5: identical CONSECUTIVE messages collapse into one
    /// entry with a count and the latest timestamp -- so a failure repeating
    /// once a second can no longer evict every other kept error, and the
    /// sticky line says how often it happened.
    #[test]
    fn identical_consecutive_errors_collapse_into_one_counted_entry() {
        let t0 = Instant::now();
        let mut errors = ErrorLog::default();
        errors.record("mail send: disk full".into(), t0);
        errors.record("mail send: disk full".into(), t0 + Duration::from_secs(1));
        errors.record("mail send: disk full".into(), t0 + Duration::from_secs(2));
        assert_eq!(errors.len(), 1, "one entry, not three");
        assert_eq!(errors.entries[0].count, 3);
        assert_eq!(errors.entries[0].last, t0 + Duration::from_secs(2));
        assert_eq!(errors.sticky_count(), 1);
        assert_eq!(
            errors.sticky_line().as_deref(),
            Some("mail send: disk full \u{d7}3")
        );
        // A different message starts its own entry; the older one survives.
        errors.record("handover: timed out".into(), t0 + Duration::from_secs(3));
        assert_eq!(errors.len(), 2);
        assert_eq!(errors.sticky_count(), 2);
        assert_eq!(errors.sticky_line().as_deref(), Some("handover: timed out"));
        // And the cap still counts entries, not repeats.
        for i in 0..10 {
            errors.record(format!("err{i}"), t0 + Duration::from_secs(10 + i));
        }
        assert_eq!(errors.len(), MAX_KEPT_ERRORS);
    }

    /// Acknowledgement clears the sticky line without deleting anything, and
    /// the SAME message arriving again afterwards raises it back.
    #[test]
    fn acknowledgement_clears_the_sticky_line_until_a_new_error_arrives() {
        let t0 = Instant::now();
        let mut errors = ErrorLog::default();
        errors.record("mail send: disk full".into(), t0);
        errors.record("mail send: disk full".into(), t0 + Duration::from_secs(1));
        assert!(errors.sticky_line().is_some());

        errors.acknowledge(errors.mark());
        assert_eq!(errors.sticky_count(), 0);
        assert_eq!(errors.sticky_line(), None, "the header line clears");
        assert_eq!(errors.len(), 1, "acknowledgement is never a delete");
        assert!(errors.entries.iter().all(|e| e.acked));

        // The same text again is news, not a silent count bump.
        errors.record("mail send: disk full".into(), t0 + Duration::from_secs(9));
        assert_eq!(errors.len(), 2);
        assert_eq!(errors.entries[1].count, 1);
        assert_eq!(errors.sticky_count(), 1);
        assert_eq!(
            errors.sticky_line().as_deref(),
            Some("mail send: disk full")
        );

        // The dialog still lists the acknowledged one, dimmed, with its age.
        let view = build_errors_view(&errors, t0 + Duration::from_secs(69));
        assert_eq!(view.items.len(), 2);
        assert_eq!(view.items[0].text, "mail send: disk full");
        assert!(!view.items[0].acked, "newest first: the unacked repeat");
        assert_eq!(view.items[0].age_secs, 60);
        assert!(view.items[1].acked);
        assert_eq!(view.items[1].count, 2);
    }

    /// D2 on a real reap: the dashboard's identity is its own, for the whole
    /// run. It used to be re-derived from `panes.first()` on every tick, so
    /// once the orchestrator exited and was reaped the dashboard adopted a
    /// *worker's* short id -- stamping it on operator-composed mail, on its own
    /// spawn requests and on the header's per-session counts -- or, with no
    /// panes left at all, an empty string.
    #[test]
    fn the_dashboards_identity_survives_its_first_pane_being_reaped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let cfg = CtxConfig::default();

        // Exactly `run_dashboard`'s own derivation, from the session id it was
        // called with -- before any pane exists, and unchanged by any of them.
        let session_id = "88888888-2222-4333-8444-555555555555";
        let dashboard_short = sessions::short_id(session_id);

        let spec = PaneSpec {
            agent_name: "test-agent".to_string(),
            argv: trivial_argv(),
            role: prompt::PromptRole::Orchestrator,
            verb: sessions::Verb::Chat,
            session_id: session_id.to_string(),
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
                &mut None,
                &mut VecDeque::new(),
                &mut HashMap::new(),
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(panes.is_empty(), "the orchestrator pane has been reaped");
        assert!(
            panes
                .first()
                .map(|p: &Pane| p.short().to_string())
                .unwrap_or_default()
                .is_empty(),
            "the old pane-derived identity is empty here -- which is the bug"
        );

        let mut errors = ErrorLog::default();
        apply_mail_effect(
            ui::MailEffect::Send(mail::Message {
                from_session: String::new(),
                from_agent: String::new(),
                to: "any".to_string(),
                to_session: None,
                sent: 0,
                body: "heads up".to_string(),
            }),
            &state,
            &repo,
            &cfg,
            &dashboard_short,
            "test-agent",
            &mut errors,
        );
        assert!(errors.is_empty(), "got errors: {errors:?}");

        let slug = super::super::state::repo_slug(&repo);
        let listed = mail::list(&state, &slug, None, None).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].1.from_session, dashboard_short,
            "composed mail still carries the dashboard's own short"
        );
    }

    /// Finding #13 (issue #358 review): on a readiness TIMEOUT, `wrap`'s own
    /// identical arm re-registers the seat onto the successor that is, in
    /// fact, running (the pane is kept alive rather than killed -- see this
    /// module's own `settle_pending_rollover` doc comment on that
    /// DEVIATION). `settle_pending_rollover` used to skip that
    /// re-registration entirely, leaving the on-disk seat naming the OLD
    /// predecessor even though the pane it describes is genuinely running
    /// the NEW successor from here on.
    #[test]
    fn a_readiness_timeout_reregisters_the_seat_onto_the_running_successor() {
        use super::pane::tests::long_lived_argv;

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "77777777-3333-4444-8888-555555555555";
        let short = sessions::short_id(session_id);
        let spec = PaneSpec {
            agent_name: "codex".to_string(),
            argv: long_lived_argv(),
            role: prompt::PromptRole::Orchestrator,
            verb: sessions::Verb::Chat,
            session_id: session_id.to_string(),
            title: "orch".to_string(),
        };
        let pane = Pane::spawn(
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

        // The seat as it stood right after `handover_pane` swapped this
        // pane's pty: still naming the OLD predecessor ("claude"), with an
        // open transaction naming "codex" as the successor.
        super::seat::register(
            &state,
            &short,
            "old-session-id",
            "claude",
            Some("sonnet"),
            "anthropic",
            prompt::PromptRole::Orchestrator.label(),
            false,
            1_700_000_000,
        )
        .expect("register seat");
        let generation = super::seat::prepare(
            &state,
            &short,
            "codex",
            Some("gpt5"),
            super::seat::Cause::Manual,
            1_700_000_000,
        )
        .expect("prepare rollover");

        let mut cfg = CtxConfig::default();
        cfg.handoff.timeout_secs = 0;
        let mut panes = vec![pane];
        let mut pending = Some((short.clone(), generation, Instant::now()));

        settle_pending_rollover(
            &mut panes,
            &cfg,
            &repo,
            &state,
            &mut pending,
            &mut ErrorLog::default(),
        );

        assert!(pending.is_none(), "the timed-out transaction must close");
        let seat = super::seat::load(&state, &short).expect("seat still exists");
        assert_eq!(seat.phase, super::seat::Phase::Idle);
        assert_eq!(
            seat.agent, "codex",
            "the seat must name the successor that is actually running, not the old \
             predecessor: {seat:?}"
        );
        assert_eq!(seat.model.as_deref(), Some("gpt5"));
        assert_eq!(seat.provider, "openai");
        assert_eq!(seat.session, session_id);

        panes[0].finish_shutdown().expect("shutdown");
    }

    /// Issue #440: the incident itself. A rollover's successor dies right
    /// after the swap, so `Pane::handover` has already quit the source child
    /// and the dashboard used to reap the pane -- leaving the operator with
    /// no orchestrator at all and, before the terminal-row fix, no
    /// explanation either. The seat still names the SOURCE (a failed rollover
    /// never commits), so the source harness is relaunched in this same pane
    /// at the same short id, and a seat whose cause was a hard block waits
    /// out its own window instead of being re-rolled on the next sweep.
    #[test]
    fn a_dead_successor_relaunches_the_source_and_parks_a_hard_blocked_seat() {
        use crate::commands::ctx::window;

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        // The successor that never answered: a child that exits immediately.
        let session_id = "66666666-3333-4444-8888-555555555555";
        let short = sessions::short_id(session_id);
        let spec = PaneSpec {
            agent_name: "codex".to_string(),
            argv: trivial_argv(),
            role: prompt::PromptRole::Orchestrator,
            verb: sessions::Verb::Chat,
            session_id: session_id.to_string(),
            title: "orch".to_string(),
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

        let now = super::super::state::now_secs();
        window::store_for(
            &state,
            "anthropic",
            &window::UsageWindows {
                five_hour: Some(window::Window {
                    used_percentage: 100.0,
                    resets_at: now + 3_600,
                    observed_at: now,
                    overage_covered: false,
                    limit_reached: true,
                }),
                seven_day: None,
            },
        )
        .expect("store the exhausted source window");

        // The seat as `handover_pane` left it: still naming the source, with
        // an open reactive transaction naming the successor above.
        super::seat::register(
            &state,
            &short,
            session_id,
            "claude",
            None,
            "anthropic",
            prompt::PromptRole::Orchestrator.label(),
            false,
            now,
        )
        .expect("register seat");
        let generation = super::seat::prepare(
            &state,
            &short,
            "codex",
            None,
            super::seat::Cause::Reactive {
                detail: "provider=anthropic, five_hour reached=true".to_string(),
                observed_at: now,
            },
            now,
        )
        .expect("prepare rollover");

        // Issue #462: a PINNED launch is the case where zirv's uuid and the
        // harness's own conversation id coincide, which is what makes a
        // resume of the seat's own session id correct here. Recorded the way
        // a real turn boundary records it, so this test states that premise
        // rather than assuming it -- the sibling test below is the case
        // where the two ids differ.
        sessions::record_native_conversation(&state, &short, "claude", session_id, session_id);

        let relaunched = tmp.path().join("relaunched.sh");
        std::fs::write(&relaunched, "#!/bin/sh\nsleep 30\n").expect("write relaunch script");
        let mut cfg = CtxConfig {
            agent_bin: Some(format!("sh {}", relaunched.display())),
            ..CtxConfig::default()
        };
        cfg.pace.estimator = false;

        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && !matches!(pane.state(), PaneState::Ended(_)) {
            pane.drain();
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            matches!(pane.state(), PaneState::Ended(_)),
            "sanity: the successor child must have exited"
        );

        let mut panes = vec![pane];
        let mut pending = Some((short.clone(), generation, Instant::now()));
        let mut errors = ErrorLog::default();

        settle_pending_rollover(&mut panes, &cfg, &repo, &state, &mut pending, &mut errors);

        assert!(pending.is_none(), "the dead transaction must close");
        assert_eq!(
            panes[0].agent(),
            "claude",
            "the source harness must be running in this pane again, not left dead"
        );
        assert!(
            !matches!(panes[0].state(), PaneState::Ended(_)),
            "the relaunched source is a live child, so the pane is not reaped"
        );

        let seat = super::seat::load(&state, &short).expect("seat still exists");
        let super::seat::Phase::Parked { until, .. } = seat.phase else {
            panic!("a hard-blocked source must park until its window resets, got {seat:?}");
        };
        assert_eq!(until, now + 3_600);
        assert_eq!(seat.agent, "claude", "the successor never took the seat");

        let logged =
            std::fs::read_to_string(state.logs().join(super::super::log::LOG_FILE)).expect("log");
        assert_eq!(
            logged.matches(super::super::rollover::FAILED).count(),
            1,
            "the transaction ends in exactly one terminal row: {logged}"
        );
        assert!(
            errors
                .entries
                .iter()
                .any(|entry| entry.text.contains("rollover failed")
                    && entry.text.contains("resumed")
                    && entry.text.contains("parked until")),
            "the pane says the source was resumed and the seat parked: {errors:?}"
        );

        panes[0].finish_shutdown().expect("shutdown");
    }

    /// Issue #462: the incident. A seat whose zirv session uuid is NOT the
    /// harness's own conversation id (any launch that suppressed
    /// `session_pin_args` -- `zirv chat -- --resume <id>` -- or a harness
    /// that mints its own id) used to be recovered with `--resume <zirv
    /// uuid>`. Claude answered "No conversation found with session ID:
    /// <uuid>", the restored pane exited 1, and the operator's orchestrator
    /// was gone for good. Recovery must resume the conversation a lifecycle
    /// hook actually OBSERVED for this seat, and the failure evidence must
    /// still say why the successor itself died.
    #[test]
    fn a_dead_successor_resumes_the_harnesss_own_conversation_not_the_seat_uuid() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        // The successor that never answered: a child that exits immediately.
        let session_id = "6c967beb-0b72-46e9-9d3e-504a03f741b3";
        let native_id = "49195b07-217f-4401-8681-c857fcea294e";
        let short = sessions::short_id(session_id);
        let spec = PaneSpec {
            agent_name: "codex".to_string(),
            argv: trivial_argv(),
            role: prompt::PromptRole::Orchestrator,
            verb: sessions::Verb::Chat,
            session_id: session_id.to_string(),
            title: "orch".to_string(),
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

        let now = super::super::state::now_secs();
        // The seat as `handover_pane` left it: still naming the source claude
        // seat, with an open transaction naming the codex successor above.
        super::seat::register(
            &state,
            &short,
            session_id,
            "claude",
            None,
            "anthropic",
            prompt::PromptRole::Orchestrator.label(),
            false,
            now,
        )
        .expect("register seat");
        let generation = super::seat::prepare(
            &state,
            &short,
            "codex",
            None,
            super::seat::Cause::Manual,
            now,
        )
        .expect("prepare rollover");
        // What the harness itself reported at its own turn boundaries: a
        // conversation id that is NOT zirv's session uuid.
        sessions::record_native_conversation(&state, &short, "claude", session_id, native_id);

        // Issue #450: the relaunch records the argv it was actually given.
        // A relative filename here used to rely on the pane's own cwd being
        // `repo`, but a liveness/capability probe of this same `agent_bin`
        // shim spawns it with no `current_dir` set, so a relative path could
        // land in the real process cwd instead. An absolute tempdir path,
        // quoted, is immune to that. The probe itself exits unlogged, or its
        // `--help` would land in the log first.
        let argv_log = tmp.path().join("argv.txt");
        let relaunched = tmp.path().join("relaunched.sh");
        std::fs::write(
            &relaunched,
            format!(
                "#!/bin/sh\n[ \"$1\" = --help ] && exit 0\nprintf '%s\\n' \"$@\" > \"{}\"\nsleep 30\n",
                argv_log.display()
            ),
        )
        .expect("write relaunch script");
        let mut cfg = CtxConfig {
            agent_bin: Some(format!("sh {}", relaunched.display())),
            ..CtxConfig::default()
        };
        cfg.pace.estimator = false;

        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && !matches!(pane.state(), PaneState::Ended(_)) {
            pane.drain();
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            matches!(pane.state(), PaneState::Ended(_)),
            "sanity: the successor child must have exited"
        );

        let mut panes = vec![pane];
        let mut pending = Some((short.clone(), generation, Instant::now()));
        let mut errors = ErrorLog::default();

        settle_pending_rollover(&mut panes, &cfg, &repo, &state, &mut pending, &mut errors);

        assert!(pending.is_none(), "the dead transaction must close");
        assert_eq!(
            panes[0].agent(),
            "claude",
            "the source harness must be running in this pane again"
        );

        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && !argv_log.is_file() {
            panes[0].drain();
            std::thread::sleep(Duration::from_millis(25));
        }
        let argv = std::fs::read_to_string(&argv_log).expect("the relaunch recorded its argv");
        assert!(
            argv.contains(native_id),
            "the relaunch must resume the harness's OWN conversation: {argv}"
        );
        assert!(
            !argv.contains(session_id),
            "zirv's seat uuid is not a conversation any harness can resume: {argv}"
        );

        let logged =
            std::fs::read_to_string(state.logs().join(super::super::log::LOG_FILE)).expect("log");
        assert!(
            logged.contains("the successor exited before it answered (exit"),
            "the successor's own exit must survive its failed recovery: {logged}"
        );

        panes[0].finish_shutdown().expect("shutdown");
    }

    /// Issue #440 (codex review, finding 1): the relaunch is best-effort, so
    /// it can fail -- and when it does the pane stays `Ended` and the reap
    /// behind it runs `finish_shutdown` -> `rollover::forget`. The park is
    /// the record of a rollover that failed and a source owed a window, so
    /// `forget` must leave a PARKED seat alone; a restore reuses the same
    /// session id, and `seat::register` preserves `phase`.
    #[test]
    fn a_failed_source_relaunch_keeps_the_parked_seat_through_the_reap() {
        use crate::commands::ctx::window;

        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");

        let session_id = "55555555-3333-4444-8888-555555555555";
        let short = sessions::short_id(session_id);
        let spec = PaneSpec {
            agent_name: "codex".to_string(),
            argv: trivial_argv(),
            role: prompt::PromptRole::Orchestrator,
            verb: sessions::Verb::Chat,
            session_id: session_id.to_string(),
            title: "orch".to_string(),
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

        let now = super::super::state::now_secs();
        window::store_for(
            &state,
            "anthropic",
            &window::UsageWindows {
                five_hour: Some(window::Window {
                    used_percentage: 100.0,
                    resets_at: now + 3_600,
                    observed_at: now,
                    overage_covered: false,
                    limit_reached: true,
                }),
                seven_day: None,
            },
        )
        .expect("store the exhausted source window");
        super::seat::register(
            &state,
            &short,
            session_id,
            "claude",
            None,
            "anthropic",
            prompt::PromptRole::Orchestrator.label(),
            false,
            now,
        )
        .expect("register seat");
        let generation = super::seat::prepare(
            &state,
            &short,
            "codex",
            None,
            super::seat::Cause::Reactive {
                detail: "provider=anthropic, five_hour reached=true".to_string(),
                observed_at: now,
            },
            now,
        )
        .expect("prepare rollover");

        // A binary that cannot be spawned: the relaunch fails inside
        // `Pane::handover`, leaving the pane exactly as dead as it was.
        let mut cfg = CtxConfig {
            agent_bin: Some(tmp.path().join("no-such-harness").display().to_string()),
            ..CtxConfig::default()
        };
        cfg.pace.estimator = false;

        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && !matches!(pane.state(), PaneState::Ended(_)) {
            pane.drain();
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(matches!(pane.state(), PaneState::Ended(_)), "sanity");

        let mut panes = vec![pane];
        let mut pending = Some((short.clone(), generation, Instant::now()));
        let mut errors = ErrorLog::default();
        settle_pending_rollover(&mut panes, &cfg, &repo, &state, &mut pending, &mut errors);

        assert_eq!(
            panes[0].agent(),
            "codex",
            "sanity: the relaunch must have failed, leaving the dead successor named"
        );
        let logged =
            std::fs::read_to_string(state.logs().join(super::super::log::LOG_FILE)).expect("log");
        assert_eq!(
            logged.matches(super::super::rollover::FAILED).count(),
            1,
            "{logged}"
        );
        assert!(
            matches!(
                super::seat::load(&state, &short).map(|seat| seat.phase),
                Some(super::seat::Phase::Parked { .. })
            ),
            "the blocked source is parked even though the relaunch failed"
        );

        // The reap's own teardown, which is what used to delete the park.
        panes[0].finish_shutdown().expect("shutdown");
        let seat = super::seat::load(&state, &short)
            .expect("a parked seat outlives the session that was sitting in it");
        assert!(
            matches!(seat.phase, super::seat::Phase::Parked { .. }),
            "got {seat:?}"
        );
        assert_eq!(
            std::fs::read_to_string(state.logs().join(super::super::log::LOG_FILE))
                .expect("log")
                .matches(super::super::rollover::FAILED)
                .count(),
            1,
            "the reap neither drops the terminal row nor adds a second one"
        );
    }

    #[test]
    fn apply_mail_effect_consume_moves_the_message_to_read() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let cfg = CtxConfig::default();
        let slug = super::super::state::repo_slug(&repo);
        let path = mail::store(
            &state,
            &slug,
            &mail::Message {
                from_session: "s1".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "note".to_string(),
            },
            &cfg,
        )
        .expect("store");

        let mut errors = ErrorLog::default();
        apply_mail_effect(
            ui::MailEffect::Consume(path.clone()),
            &state,
            &repo,
            &cfg,
            "orch1234",
            "claude",
            &mut errors,
        );
        assert!(errors.is_empty(), "got errors: {errors:?}");
        assert!(!path.exists());
        assert!(
            mail::list(&state, &slug, None, None)
                .expect("list")
                .is_empty()
        );
    }

    #[test]
    fn apply_memory_effect_remember_writes_an_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        let cfg = CtxConfig::default();
        let mut errors = ErrorLog::default();

        apply_memory_effect(
            ui::MemoryEffect::Remember {
                key: "build-cmd".to_string(),
                body: "cargo build".to_string(),
            },
            &state,
            &repo,
            &cfg,
            "claude",
            &mut errors,
        );
        assert!(errors.is_empty(), "got errors: {errors:?}");

        let slug = super::super::state::repo_slug(&repo);
        let listed = memory::list(&state, &slug).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].1.key, "build-cmd");
        assert_eq!(listed[0].1.written_by, "claude");
    }

    /// Issue #552: the two directions with a NATIVE target actually open a
    /// live native pane, through the production launcher
    /// (`PaneSuccessorLauncher`, which is what `handover_pane` drives) --
    /// not a `NoBackend` refusal.
    ///
    /// What is asserted is what a successor owes: it exists, it is native, it
    /// sits in the roster slot the source occupied (so exactly one seat
    /// holder), it is a NEW conversation, it answers to the seat's own short
    /// id, it runs under the committed generation, and retiring the source
    /// did not delete the registry record the successor just wrote.
    #[test]
    fn a_native_successor_actually_opens_as_a_live_pane_on_the_same_seat() {
        use super::super::rollover_runtime::{SuccessorLauncher, plan_successor};
        use super::super::runtime::RuntimeKind;

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state_root = tmp.path().join("state");
        let state = StateDir::from_root(state_root.clone());
        let env: HashMap<String, String> = [(
            super::super::state::STATE_ENV.to_string(),
            state_root.to_str().expect("utf8").to_string(),
        )]
        .into();
        let lookup = |key: &str| env.get(key).cloned();
        let cfg = CtxConfig::default();

        for source_runtime in [RuntimeKind::Harness, RuntimeKind::Native] {
            let mut source = if source_runtime == RuntimeKind::Native {
                Pane::spawn_native(
                    &cfg,
                    &state,
                    &lookup,
                    repo.path(),
                    sessions::Verb::Chat,
                    "seat".to_string(),
                    (80, 24),
                    native_pane::NativeDashboardSpec {
                        repo: repo.path().to_path_buf(),
                        role: "orchestrator".to_string(),
                        route: None,
                        writing: false,
                        provider: Some(fixture_provider()),
                        seat: None,
                        initial_input: None,
                    },
                )
                .expect("a native source pane opens")
            } else {
                Pane::spawn(
                    PaneSpec {
                        agent_name: "test-agent".to_string(),
                        argv: trivial_argv(),
                        role: prompt::PromptRole::Orchestrator,
                        verb: sessions::Verb::Chat,
                        session_id: "51111111-2222-4333-8444-555555555555".to_string(),
                        title: "seat".to_string(),
                    },
                    &state,
                    repo.path(),
                    repo.path(),
                    (80, 24),
                    &[],
                    true,
                    pane::DEFAULT_IDLE_QUIET,
                )
                .expect("a wrapped source pane opens")
            };

            let seat_short = source.short().to_string();
            let source_session = source.session_id().to_string();
            let plan = super::super::rollover_runtime::SuccessorPlan {
                acknowledged_input: vec!["finish the migration note".to_string()],
                ..plan_successor(
                    source_runtime,
                    RuntimeKind::Native,
                    &seat_short,
                    9,
                    Some("claude"),
                    None,
                    None,
                    None,
                    None,
                )
            };
            let note = handoff::Handoff {
                task: "carry the seat across".to_string(),
                ..handoff::Handoff::default()
            };
            let req = handover::HandoverRequest {
                target_agent: "claude".to_string(),
                target_model: None,
                force: false,
                requested_at: 0,
                interactive: false,
                automatic: true,
                generation: Some(9),
                structural_only: true,
                resume_session: None,
                target_runtime: Some(RuntimeKind::Native.as_str().to_string()),
                target_route: None,
            };
            let successor_session = {
                let mut launcher = PaneSuccessorLauncher {
                    pane: &mut source,
                    cfg: &cfg,
                    req: &req,
                    note: &note,
                    role: prompt::PromptRole::Orchestrator,
                    repo: repo.path(),
                    size: (24, 80),
                    native: super::super::rollover_runtime::NativeSuccessorSpec {
                        // A bare temp repo cannot take a writer lease, and
                        // this test is about the successor existing at all.
                        writing: false,
                        provider: Some(fixture_provider()),
                    },
                };
                launcher.launch(&plan).unwrap_or_else(|e| {
                    panic!("{source_runtime:?} -> Native must open a pane: {e}")
                })
            };

            assert!(
                source.is_native(),
                "{source_runtime:?} -> Native must leave a LIVE native pane, not a refusal"
            );
            assert!(source.native().is_some(), "with its own native driver");
            assert_ne!(
                successor_session, source_session,
                "a successor is a new conversation, never the source's own"
            );
            assert_eq!(
                source.session_id(),
                successor_session,
                "and the pane in the roster slot IS that successor"
            );
            assert_eq!(
                source.short(),
                seat_short,
                "the seat's short id is its address and does not move across a rollover"
            );
            let seat =
                super::super::seat::load(&state, &seat_short).expect("the seat still exists");
            assert_eq!(
                seat.generation, 9,
                "the successor runs under the committed generation"
            );
            assert_eq!(seat.runtime, RuntimeKind::Native);
            assert!(
                sessions::list(&state)
                    .iter()
                    .any(|(record, _)| record.short == seat_short),
                "retiring the source must not delete the record the successor registered"
            );
            let _ = source.shutdown("");
        }
    }

    /// Issue #552, the fourth direction: a NATIVE source hands its seat to a
    /// WRAPPED successor.
    ///
    /// A native pane has no child for `Pane::handover` to swap in place, so
    /// this runs the same open-then-retire shape the native target uses, over
    /// the same `Pane::build_swap_launch` derivation an in-place swap runs
    /// on. The assertions are the seat's: a live wrapped pane, on the seat's
    /// own short id, under the committed generation, with the record the
    /// successor registered still present.
    #[test]
    fn a_harness_successor_takes_the_seat_from_a_native_source() {
        use super::super::rollover_runtime::{SuccessorLauncher, plan_successor};
        use super::super::runtime::RuntimeKind;

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tempfile::tempdir().expect("repo");
        let state_root = tmp.path().join("state");
        let state = StateDir::from_root(state_root.clone());
        let env: HashMap<String, String> = [(
            super::super::state::STATE_ENV.to_string(),
            state_root.to_str().expect("utf8").to_string(),
        )]
        .into();
        let lookup = |key: &str| env.get(key).cloned();

        // A successor that really spawns, without needing to survive -- the
        // same stub the pane module's own handover tests use.
        #[cfg(windows)]
        let successor_bin = "ping";
        #[cfg(not(windows))]
        let successor_bin = "sleep";
        let cfg = CtxConfig {
            agent_bin: Some(successor_bin.to_string()),
            ..CtxConfig::default()
        };

        let mut source = Pane::spawn_native(
            &CtxConfig::default(),
            &state,
            &lookup,
            repo.path(),
            sessions::Verb::Chat,
            "seat".to_string(),
            (80, 24),
            native_pane::NativeDashboardSpec {
                repo: repo.path().to_path_buf(),
                role: "orchestrator".to_string(),
                route: None,
                writing: false,
                provider: Some(fixture_provider()),
                seat: None,
                initial_input: None,
            },
        )
        .expect("a native source pane opens");
        let seat_short = source.short().to_string();
        let source_session = source.session_id().to_string();
        super::super::seat::register(
            &state,
            &seat_short,
            &source_session,
            "native",
            None,
            "anthropic",
            "orchestrator",
            false,
            0,
        )
        .expect("seat");

        let plan = plan_successor(
            RuntimeKind::Native,
            RuntimeKind::Harness,
            &seat_short,
            11,
            Some("claude"),
            None,
            None,
            None,
            None,
        );
        let note = handoff::Handoff {
            task: "carry the seat back onto a harness".to_string(),
            ..handoff::Handoff::default()
        };
        let req = handover::HandoverRequest {
            target_agent: "claude".to_string(),
            target_model: None,
            force: true,
            requested_at: 0,
            interactive: false,
            automatic: true,
            generation: Some(11),
            structural_only: true,
            resume_session: None,
            target_runtime: Some(RuntimeKind::Harness.as_str().to_string()),
            target_route: None,
        };
        // The successor's own launch, off the SAME builder an in-place swap
        // runs on: fenced on the committed generation, and told the socket
        // its own identity will bind.
        let probe = source
            .build_swap_launch(
                &cfg,
                &req,
                &note,
                prompt::PromptRole::Orchestrator,
                repo.path(),
                "probe-session",
                None,
                "seat".to_string(),
            )
            .expect("the swap launch derives");
        assert!(
            probe
                .turn_env
                .iter()
                .any(|(key, value)| key == super::super::seat::GENERATION_ENV && value == "11"),
            "the successor child is fenced on the committed generation: {:?}",
            probe.turn_env
        );

        let successor_session = {
            let mut launcher = PaneSuccessorLauncher {
                pane: &mut source,
                cfg: &cfg,
                req: &req,
                note: &note,
                role: prompt::PromptRole::Orchestrator,
                repo: repo.path(),
                size: (24, 80),
                native: super::super::rollover_runtime::NativeSuccessorSpec::default(),
            };
            launcher
                .launch(&plan)
                .expect("Native -> Harness must open a wrapped pane")
        };

        assert!(
            !source.is_native(),
            "the roster slot must now hold a LIVE wrapped pane, not a refusal"
        );
        assert_eq!(source.agent(), "claude");
        assert_ne!(
            successor_session, source_session,
            "a successor is a new conversation, never the source's own"
        );
        assert_eq!(source.session_id(), successor_session);
        assert_eq!(
            source.short(),
            seat_short,
            "the seat's short id is its address and does not move across a rollover"
        );
        let record = sessions::list(&state)
            .into_iter()
            .find(|(record, _)| record.short == seat_short);
        assert!(
            record.is_some(),
            "retiring the source must not delete the record the successor registered"
        );
        let _ = source.shutdown("");
    }

    fn fixture_provider() -> String {
        format!(
            "fixture:{}",
            super::super::runtime::fixture::fixture_root()
                .join("helper-answer.json")
                .display()
        )
    }

    // -- issue #354 phase 3: the inspector ---------------------------------

    fn inspected_row() -> ui::SidebarRow {
        let panes = vec![pane_row("aaaa1111", "claude")];
        let mut rows = assemble_sidebar(&panes, &[], &HashMap::new(), 0, 0, DASHBOARD_PID, 900);
        let mut status = super::super::attention::SessionStatus {
            lifecycle: super::super::attention::Lifecycle::Waiting,
            attention: super::super::attention::Attention::Approval,
            evidence: "workflow gate is waiting on approval".to_string(),
            confidence: 90,
            revision: 7,
            last_transition: 800,
            ..Default::default()
        };
        status.skipped = vec![
            super::super::attention::Skipped {
                authority: super::super::attention::Authority::QuietHeuristic,
                reason: "lifecycle: outranked by Harness (stop hook)".to_string(),
            },
            super::super::attention::Skipped {
                authority: super::super::attention::Authority::Supervisor,
                reason: "attention: outranked by Harness (stop hook)".to_string(),
            },
        ];
        rows[0].status = Some(status);
        rows.remove(0)
    }

    /// Every section is present, missing facts read as the shared placeholder
    /// rather than a fabricated value, and the evidence section lists both
    /// the winning evidence and every skipped authority's reason.
    #[test]
    fn the_inspector_reports_every_section_with_evidence_and_skipped_reasons() {
        let row = inspected_row();
        let mut kept = ErrorLog::default();
        push_error(&mut kept, "aaaa1111 write_input: EPIPE".to_string());
        let view = build_inspector_view(&row, Some("D:/repo"), &kept);
        let names: Vec<&str> = view.sections.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                INSPECT_IDENTITY,
                INSPECT_STATUS,
                INSPECT_EVIDENCE,
                INSPECT_BUDGET,
                INSPECT_WRITER,
                INSPECT_SIGNAL,
                INSPECT_ERRORS,
            ]
        );
        let section = |name: &str| {
            view.sections
                .iter()
                .find(|s| s.name == name)
                .expect("section")
                .lines
                .join("\n")
        };
        assert!(section(INSPECT_IDENTITY).contains("aaaa1111"));
        assert!(section(INSPECT_IDENTITY).contains("claude"));
        assert!(section(INSPECT_IDENTITY).contains("D:/repo"));
        // `model` was never pinned and `branch` is never resolved on the
        // render path: both read as the placeholder, not as a guess.
        assert!(
            section(INSPECT_IDENTITY).contains(style::PLACEHOLDER),
            "a missing fact must read as the placeholder"
        );
        assert!(section(INSPECT_STATUS).contains("waiting"));
        assert!(section(INSPECT_STATUS).contains('7'), "revision");
        let evidence = section(INSPECT_EVIDENCE);
        assert!(evidence.contains("workflow gate is waiting on approval"));
        assert!(evidence.contains("quiet heuristic"), "got {evidence}");
        assert!(evidence.contains("outranked by Harness"), "got {evidence}");
        assert!(section(INSPECT_ERRORS).contains("EPIPE"));

        // Opening it "at evidence" is a caret position in the same flattened
        // row list the dialog draws -- never a second layout.
        let rows = view.rows();
        let start = view.section_start(INSPECT_EVIDENCE);
        assert_eq!(rows[start], format!("{INSPECT_EVIDENCE}:"));
        assert!(rows[start + 1].contains("workflow gate"));
        assert_eq!(view.section_start("nothing-like-this"), 0);
    }

    // D1: a nudge names its target by short id and is resolved against the
    // live pane list at Enter time, so panes coming and going while the
    // dialog is open cannot re-aim it.

    /// Two live panes, spawned with long-lived children so neither is reaped
    /// out from under the test, returned with their shorts.
    fn two_live_panes(state: &StateDir, repo: &Path) -> (Vec<Pane>, String, String) {
        use super::pane::tests::long_lived_argv;
        let mut panes = Vec::new();
        for (i, session_id) in [
            "aaaaaaaa-2222-4333-8444-555555555555",
            "bbbbbbbb-2222-4333-8444-555555555555",
        ]
        .into_iter()
        .enumerate()
        {
            let spec = PaneSpec {
                agent_name: "test-agent".to_string(),
                argv: long_lived_argv(),
                role: prompt::PromptRole::Worker,
                verb: sessions::Verb::Dash,
                session_id: session_id.to_string(),
                title: format!("wrk {i}"),
            };
            panes.push(
                Pane::spawn(
                    spec,
                    state,
                    repo,
                    repo,
                    (80, 24),
                    &[],
                    true,
                    pane::DEFAULT_IDLE_QUIET,
                )
                .expect("spawn"),
            );
        }
        let a = panes[0].short().to_string();
        let b = panes[1].short().to_string();
        (panes, a, b)
    }

    /// The dialog was opened on pane A; A ended and was reaped before the
    /// operator pressed Enter. The nudge must be reported undeliverable and
    /// land nowhere -- least of all in whichever pane took A's place.
    #[test]
    fn a_nudge_aimed_at_a_reaped_pane_is_reported_and_delivered_nowhere() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let (mut panes, a, _b) = two_live_panes(&state, &repo);

        // A is reaped while the dialog is open: it leaves the vector, and B
        // slides into index 0 -- the index the dialog used to hold.
        let mut reaped = panes.remove(0);
        // finish_shutdown: immediate, no QUIT_GRACE wait -- these panes'
        // `long_lived_argv` child never reads its pty input, so the polite
        // `shutdown` ask-then-wait always burns the full grace for nothing.
        let _ = reaped.finish_shutdown();
        let mut queues: Vec<VecDeque<String>> = vec![VecDeque::new()];
        let mut errors = ErrorLog::default();
        let env = |_: &str| None;

        submit_nudge(
            ui::NudgeTarget::AttachedPane(a),
            "restart the build",
            &mut panes,
            &mut queues,
            &repo,
            &env,
            &mut errors,
            &mut Vec::new(),
            Instant::now(),
        );

        assert!(
            errors.iter().any(|e| e.contains("target ended")),
            "the operator is told the target is gone: {errors:?}"
        );
        assert!(
            queues[0].is_empty(),
            "and the surviving pane -- now at the reaped one's index -- got nothing"
        );

        for pane in panes.iter_mut() {
            let _ = pane.finish_shutdown();
        }
    }

    /// The dialog was opened on pane B, and pane A was reaped before Enter, so
    /// B's index shifted. The nudge must still reach B.
    #[test]
    fn a_nudge_follows_its_target_when_an_earlier_pane_is_reaped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let (mut panes, _a, b) = two_live_panes(&state, &repo);

        let mut reaped = panes.remove(0);
        // finish_shutdown: see the identical comment on
        // `a_nudge_aimed_at_a_reaped_pane_is_reported_and_delivered_nowhere`.
        let _ = reaped.finish_shutdown();
        assert_eq!(panes[0].short(), b, "B is at index 0 now, not index 1");

        // B has reported no turn boundary, so a nudge for it queues rather
        // than injecting -- which is exactly the observable this needs.
        let mut queues: Vec<VecDeque<String>> = vec![VecDeque::new()];
        let mut errors = ErrorLog::default();
        let env = |_: &str| None;

        submit_nudge(
            ui::NudgeTarget::AttachedPane(b),
            "restart the build",
            &mut panes,
            &mut queues,
            &repo,
            &env,
            &mut errors,
            &mut Vec::new(),
            Instant::now(),
        );

        assert_eq!(
            queues[0].front().map(String::as_str),
            Some("restart the build"),
            "the nudge followed its target across the index shift: {errors:?}"
        );

        for pane in panes.iter_mut() {
            let _ = pane.finish_shutdown();
        }
    }

    #[test]
    fn settled_worker_without_requester_sends_no_mail() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut pane = spawn_idle_signal_less_worker_pane(
            &state,
            tmp.path(),
            "dddddddd-2222-4333-8444-555555555555",
        );
        report_settled_pane(
            &mut pane,
            &state,
            &CtxConfig::default(),
            &mut ErrorLog::default(),
        );
        assert!(
            mail::list(
                &state,
                &super::super::state::repo_slug(tmp.path()),
                None,
                None
            )
            .expect("list")
            .is_empty()
        );
        pane.finish_shutdown().expect("shutdown");
    }

    /// Review round 2: `push_error`'s own message must not vary with the
    /// refused kill's `target`, or a same-uid process naming a different
    /// (even nonexistent) short id on every forged kill would defeat
    /// `ErrorLog::record`'s adjacent-message dedup and evict every genuine
    /// error out of the ring (`MAX_KEPT_ERRORS`, 5 slots) one forged kill at
    /// a time. Two refusals in a row, naming two DIFFERENT targets, still
    /// collapse onto the one slot identical adjacent text already gets.
    #[test]
    fn two_shared_channel_kill_refusals_in_a_row_leave_one_error_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let dir = tmp.path().join("requests");

        spawnreq::write_request(&dir, &kill_request("deadbeef")).expect("write");
        spawnreq::write_request(&dir, &kill_request("feedface")).expect("write");

        let mut panes: Vec<Pane> = Vec::new();
        let mut queues: Vec<VecDeque<String>> = Vec::new();
        let mut errors = ErrorLog::default();
        handle_spawn_requests(
            &dir,
            &mut panes,
            &mut queues,
            &CtxConfig::default(),
            &state,
            &repo,
            (80, 24),
            &mut errors,
            &mut Vec::new(),
            &mut HashMap::new(),
        );

        let messages: Vec<String> = errors.messages().collect();
        assert_eq!(
            errors.len(),
            1,
            "two forged kills naming different targets must not evict each other's slot: \
             {messages:?}"
        );
        assert!(
            messages[0].ends_with("\u{d7}2"),
            "both refusals are still counted, just onto the same slot: {messages:?}"
        );
    }

    /// SECURITY (issue #435 item 1, was review round 2 finding 1): a `kill`
    /// arriving on a pane's own channel is honoured only for that pane
    /// itself or a pane it spawned -- naming an unrelated sibling (no
    /// `requested_by` chain connects them) is refused, even though the
    /// channel itself identifies the requester (issue #179: that
    /// identification is not authentication).
    #[test]
    fn a_kill_request_on_a_panes_own_channel_targeting_an_unrelated_pane_is_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let requests_dir = tmp
            .path()
            .join("dash")
            .join("aaaa2222-token")
            .join("requests");

        let mut errors = ErrorLog::default();
        let mut panes: Vec<Pane> = Vec::new();
        for session_id in [
            "aaaaaaaa-3333-4444-8555-666666666666",
            "bbbbbbbb-3333-4444-8555-666666666666",
        ] {
            let mut pane = Pane::spawn(
                PaneSpec {
                    agent_name: "test-agent".to_string(),
                    argv: silent_long_lived_argv(),
                    role: prompt::PromptRole::Worker,
                    verb: sessions::Verb::Dash,
                    session_id: session_id.to_string(),
                    title: "wrk test".to_string(),
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
            pane.set_intake_dir(mint_pane_channel(&requests_dir, &mut errors));
            panes.push(pane);
        }
        let victim = panes[0].short().to_string();
        let attacker_channel = panes[1]
            .intake_dir()
            .expect("the second pane has its own channel")
            .to_path_buf();

        let path =
            spawnreq::write_request(&attacker_channel, &kill_request(&victim)).expect("write");
        let stem = spawnreq::request_stem(&path).expect("stem");
        let mut queues: Vec<VecDeque<String>> = vec![VecDeque::new(); panes.len()];
        handle_spawn_requests(
            &requests_dir,
            &mut panes,
            &mut queues,
            &CtxConfig::default(),
            &state,
            &repo,
            (80, 24),
            &mut errors,
            &mut Vec::new(),
            &mut HashMap::new(),
        );

        let ack = spawnreq::wait_for_ack(&attacker_channel, &stem, Duration::from_millis(50))
            .expect("the refusal is acked on the channel it arrived on");
        assert!(!ack.ok);
        assert!(
            !ack.retryable,
            "and it is final -- no channel this request may be re-sent on exists: {ack:?}"
        );
        assert_eq!(ack.reason.as_deref(), Some(KILL_UNRELATED_PANE_REFUSAL));
        assert!(
            !matches!(panes[0].state(), PaneState::Ended(_)),
            "the named pane is untouched"
        );
        assert!(
            sessions::list(&state)
                .iter()
                .any(|(r, _)| r.short == victim),
            "and still registered"
        );

        for pane in panes.iter_mut() {
            let _ = pane.shutdown("");
        }
    }

    /// M6: applying a resize resizes every pane's screen to the new effective
    /// main geometry and updates the stored terminal size.
    #[test]
    fn apply_terminal_resize_reconciles_pane_geometry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(tmp.path().join("state"));
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let spec = PaneSpec {
            agent_name: "test-agent".to_string(),
            argv: super::pane::tests::long_lived_argv(),
            role: prompt::PromptRole::Worker,
            verb: sessions::Verb::Dash,
            session_id: "eeeeeeee-2222-4333-8444-555555555555".to_string(),
            title: "wrk resize".to_string(),
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

        let mut term_cols = 80u16;
        let mut term_rows = 24u16;
        let mut full = Rect::new(0, 0, 80, 24);
        let mut errors = ErrorLog::default();
        // sidebar 20, not zoomed: main width = 100 - 20 - 1 = 79, height =
        // 40 - 6 (issue #209/v3 §A4/§D: one header row, one top rule, one
        // bottom rule, one footer row -- `ui::chrome_rows` -- plus dash
        // refresh PR1's own pane-header row and the rule below it).
        apply_terminal_resize(
            100,
            40,
            20,
            false,
            &mut term_cols,
            &mut term_rows,
            &mut full,
            &mut panes,
            &mut errors,
            &mut None,
        );
        assert_eq!((term_cols, term_rows), (100, 40), "stored size updated");
        assert_eq!(full, Rect::new(0, 0, 100, 40));
        // vt100 `size()` returns (rows, cols).
        assert_eq!(
            panes[0].screen().size(),
            (34, 79),
            "the pane's screen was resized to the new inner geometry"
        );

        // finish_shutdown: see the identical comment on
        // `a_nudge_aimed_at_a_reaped_pane_is_reported_and_delivered_nowhere`.
        for pane in panes.iter_mut() {
            let _ = pane.finish_shutdown();
        }
    }
}
