//! pump support for the interactive supervisor.

use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn pump(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    // P2/P3/P5: both are swapped over to the fresh child on every relaunch,
    // so the replaced child's tree can never outlive the supervisor that
    // replaced it and the registry record never points at a dead pid.
    child_guard: &mut super::supervise::ChildGuard,
    session_guard: &mut super::sessions::SessionGuard,
    rx: &mpsc::Receiver<PumpEvent>,
    pair: &mut portable_pty::PtyPair,
    supervision: &mut InjectionState,
    server: Option<&super::signal::SignalServer>,
    // T84: `&mut Box<dyn AgentAdapter>`, not `&dyn AgentAdapter` -- a live
    // `zirv ctx handover` swap replaces the boxed trait object in place
    // (`*adapter = new_adapter`) once the swap has actually happened, so
    // every later tick's `adapter.<method>()` call (unchanged syntax, thanks
    // to auto-deref through `&mut Box<dyn _>`) resolves against the new
    // harness. See the handover request check near the top of this loop.
    adapter: &mut Box<dyn AgentAdapter>,
    writer: &std::sync::Arc<std::sync::Mutex<Box<dyn Write + Send>>>,
    transcript: &mut TranscriptSource,
    state_dir: &super::state::StateDir,
    session: &super::event::SessionId,
    debounce: Duration,
    inject_timeout: Duration,
    repo: &Path,
    // Issue #780: needed for `LiveAutoRollover`'s own fresh, layered
    // `CtxConfig::load` -- see the seat-rollover-enabled gate below.
    env: EnvLookup<'_>,
    tail_items: usize,
    // T84: `&mut String`, not `&str` -- a handover swap recomputes this for
    // the new adapter's own distiller default, so a rot-triggered restart
    // *after* a handover does not keep quoting the predecessor's model name.
    distiller_model: &mut String,
    distiller_timeout: Duration,
    // N6: read alongside `distiller_model`/`distiller_timeout` at the one
    // restart site below (`Action::Restart`), never anywhere else in the
    // pump -- the harvest call is gated on `cfg.memory.harvest` internally.
    cfg: &CtxConfig,
    memory_slug: &str,
    grace: Duration,
    tx: mpsc::Sender<PumpEvent>,
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    extra: &[String],
    // T84: `&mut Vec<_>`, not `&[_]` -- a handover swap rebuilds this for the
    // new adapter/model (fresh `AGENT_ENV`/`SEAT_MODEL_ENV`/turn-signal env),
    // so a *later* rot-triggered restart relaunches with the right identity.
    turn_env: &mut Vec<(String, String)>,
    cpr_filter: &std::sync::Arc<std::sync::Mutex<CprFilter>>,
    announcer: &Announcer,
    bar: &mut BarRuntime,
    // T84: needed to recompute `SEAT_MODEL_ENV` on a handover swap
    // (`adapters::seat_model_env`), the same role this session's own first
    // launch was composed for.
    role: super::prompt::PromptRole,
    // Issue #249: this session's own supervising session, if any -- resolved
    // once by `run_with` from `env` (`agent::parent_identity`) and passed
    // straight through, never re-derived per poll.
    parent_short: Option<&str>,
    native_successor: &mut Option<super::dash::Pane>,
) -> CtxResult<i32> {
    let mut last_size = window_size(STDIN_FD).unwrap_or(DEFAULT_SIZE);
    // T13: the live mail wake-up. See `mail_polling_enabled` for the gates: a
    // session that cannot receive mail at all never reads the mailbox once.
    let mut mail_watch = MailWatch::default();
    // Issue #785: the `[jev] inject` gate's pump-local state; off => every
    // `check` proceeds at once and the paths below run as before.
    let mut inject_gate = super::inject_gate::AsyncGate::default();
    // Finding #7: `handover::take_request`'s own polling cadence, tracked
    // independently of `mail_watch` -- see the check site's own doc comment.
    let mut last_handover_poll: Option<Instant> = None;
    // Issue #358 (task 5): the automatic rollover's own cadence, seeded with
    // this pump's start so no usage I/O runs during session startup, and the
    // one rollover transaction that may be open at a time.
    let mut last_rollover_eval: Option<Instant> = Some(Instant::now());
    let seat_short = bar.session_short.clone();
    let mut reactive_pending = super::seat::load(state_dir, &seat_short)
        .and_then(|seat| seat.pending)
        .is_some_and(|pending| matches!(pending.cause, super::seat::Cause::Reactive { .. }));
    let mut pending_rollover: Option<PendingRollover> = None;
    let is_orchestrator = role == PromptRole::Orchestrator;
    // Issue #780: `cfg` above is loaded once at this session's launch and
    // held for the whole (potentially very long) wrapped session, so a gate
    // reading `cfg.auto_orchestrator_rollover()` never sees a later `zirv ctx
    // config set fallback.auto_orchestrator_rollover false` -- see
    // `rollover::LiveAutoRollover`'s own doc comment. Seeded from `cfg`'s own
    // value so the very first tick (before either `ctx.toml` could possibly
    // have changed) matches what launch already decided. The readiness watch
    // for an already-open transaction (below, gated only on `pending_rollover
    // .is_some()`) is deliberately NOT behind this switch either -- a live
    // disable must stop a NEW rollover from being prepared, but a
    // transaction already open must still reach commit or abort.
    let mut auto_rollover =
        super::rollover::LiveAutoRollover::new(repo, env, cfg.auto_orchestrator_rollover());

    loop {
        if let Some(status) = child.try_wait()? {
            // Issue #358: the successor of an automatic rollover never came
            // up (or died before it ever answered), so the transaction that
            // launched it is a failure, not a commit.
            if let Some(pending) = pending_rollover.take() {
                let _ = super::rollover::fail(
                    state_dir,
                    "wrap",
                    &seat_short,
                    pending.generation,
                    "the successor exited before it answered",
                    super::state::now_secs(),
                );
            }
            // Let the reader thread flush whatever is still buffered.
            while rx.recv_timeout(Duration::from_millis(50)).is_ok() {}
            let code = status.exit_code() as i32;
            // Item 6 audit: the wrapped session just ended, whether the
            // agent quit cleanly or crashed. Previously silent -- nothing
            // printed at all, so the session appeared to just stop.
            announcer.emit(&Event::SessionEnded {
                agent: adapter.name().to_string(),
                code,
            });
            harvest_at_clean_exit(
                adapter.as_ref(),
                transcript,
                tail_items,
                distiller_model.as_str(),
                distiller_timeout,
                repo,
                state_dir,
                memory_slug,
                cfg,
            );
            return Ok(code);
        }

        while let Ok(event) = rx.try_recv() {
            if event == PumpEvent::PtyClosed {
                let status = child.wait()?;
                let code = status.exit_code() as i32;
                announcer.emit(&Event::SessionEnded {
                    agent: adapter.name().to_string(),
                    code,
                });
                harvest_at_clean_exit(
                    adapter.as_ref(),
                    transcript,
                    tail_items,
                    distiller_model.as_str(),
                    distiller_timeout,
                    repo,
                    state_dir,
                    memory_slug,
                    cfg,
                );
                return Ok(code);
            }
            // Issue #281: the operator's own keystroke reaching this pty is
            // the one edge that reliably means a fresh turn is starting for
            // every turn after the first (the first is stamped once, above
            // this loop, since it never goes through `Input` at all -- see
            // that call site's own comment). `supervision.last_turn` is
            // still the PREVIOUS completed turn's number here (`on_event`
            // below runs after this), so `+ 1` names the turn this input is
            // about to start.
            if matches!(event, PumpEvent::Input(_)) {
                let verb = session_guard.record().verb.as_str();
                session_guard.stamp_in_flight(verb, supervision.last_turn + 1);
            }
            supervision.on_event(event, Instant::now());
        }

        if let Some(server) = server
            && let Some(signal) = server.try_recv()
        {
            transcript.adopt(signal.transcript_path.as_deref());
            let previous_verdict = supervision.verdict;
            supervision.on_turn(&signal);
            // Issue #281: the turn just reported by `signal` has reached its
            // clean boundary -- clear the marker `stamp_in_flight` set for
            // it, so a crash from this point until the NEXT turn's own start
            // (the next `Input`, or a fresh spawn on restart) is correctly
            // read as "idle between turns", not "interrupted mid-turn".
            session_guard.clear_in_flight();
            if let Some(event) =
                super::announce::verdict_change(previous_verdict, supervision.verdict, signal.score)
            {
                announcer.emit(&event);
            }

            // T13: mail is no longer read here. The poll arm below owns it
            // end to end -- it runs on its own `MAIL_POLL` cadence rather
            // than only when the agent happens to report a turn, so a
            // message that arrives mid-turn no longer waits for one.

            // N4: an interactive session is only ever *advised* of a nudge:
            // never restarted, and never handed the nudge's own message
            // body. Claiming the marker here (rather than on a byte-pump or
            // per-tick path) keeps this arm as cheap as it always was.
            // C4: `from` is the *sender*, read out of the marker file, and
            // the disposition is `Advisory` -- an interactive session never
            // receives message bodies, so the line has to point the operator
            // at `zirv ctx inbox` rather than promise the guidance will
            // "be picked up as mail".
            if let Some(from) = super::sessions::claim_nudge_marker(state_dir, &bar.session_short) {
                announcer.emit(&Event::Nudge {
                    from,
                    disposition: super::announce::NudgeDisposition::Advisory,
                });
            }
        }

        // T84: `zirv ctx handover`. `take_request` is a real file read +
        // remove, and used to run on *every* ~100ms pump tick for the
        // session's entire lifetime -- the overwhelming majority of which
        // find nothing there (finding #7, issue-close review). Gated on the
        // same `MAIL_POLL` (2s) cadence the mail poll arm below already
        // uses, tracked independently (`last_handover_poll`) so a session
        // with mail polling disabled/degraded still gets its own cadence,
        // and vice versa -- a handover request still answers within one
        // cadence tick either way, since the request sits in a state-dir
        // file until this loop notices it regardless of how often it looks.
        // `may_inject` is exactly the "verified-idle turn boundary" check
        // every other injection already gates on -- reusing it here is what
        // "quiesce" means for this feature, per the module's own doc
        // comment on `handover.rs`.
        let now = Instant::now();
        let handover_poll_due = handover_poll_due(last_handover_poll, now);
        if handover_poll_due {
            last_handover_poll = Some(now);
        }
        // Issue #358 (task 5): the automatic rollover shares this exact
        // seam, so an automatically decided swap and an operator's own are
        // executed by the same code. A manual request always wins: it is
        // claimed first, and its presence skips the automatic evaluation for
        // this tick entirely (and clears whatever `pending` cause the seat
        // was still carrying -- the operator just answered the question).
        let manual_req = handover_poll_due
            .then(|| super::handover::take_request(state_dir, &bar.session_short))
            .flatten();
        let swap_req = match manual_req {
            Some(req) => {
                // Finding #1 (issue #358 review): a manual request must not
                // let an already-open automatic rollover transaction linger.
                // If it did, the readiness watch below would later commit
                // that transaction's generation against whatever session id
                // this manual swap put in the seat -- the wrong agent for
                // that generation. Close the open transaction first (the
                // successor it named already lost the pty to this manual
                // request, so it never got to prove itself ready) before
                // honouring the manual request.
                if let Some(pending) = pending_rollover.take() {
                    let _ = super::rollover::fail(
                        state_dir,
                        "wrap",
                        &seat_short,
                        pending.generation,
                        "superseded by a manual handover request",
                        super::state::now_secs(),
                    );
                }
                let _ =
                    super::seat::clear_pending(state_dir, &seat_short, super::state::now_secs());
                reactive_pending = false;
                Some(req)
            }
            // Issue #780: `pending_rollover.is_none()` and `is_orchestrator`
            // come first so nothing below runs when there is nothing to
            // prepare or this is not the orchestrator seat at all. The
            // cadence check (`rollover_eval_due`, a cheap `Instant`
            // comparison) runs BEFORE `auto_rollover.is_enabled()` (two
            // `stat`s), so the live reload only ever costs a syscall once
            // per interval, not on every tick. `last_rollover_eval` advances
            // whenever the cadence comes due, whether or not the switch is
            // enabled: otherwise a disabled switch would leave `due()`
            // permanently true and `is_enabled()` would run every tick again
            // anyway. `auto_rollover.is_enabled()` re-derives the switch from
            // a fresh layered load rather than this session's stale start-up
            // `cfg`, so a live operator disable takes effect on the very next
            // check -- no restart required. A failed reload never enables it
            // (see `LiveAutoRollover`'s own doc comment), and a disable
            // always wins over an eval that came due.
            None if pending_rollover.is_none() && is_orchestrator => {
                let eval_due = rollover_eval_due_advancing(
                    &mut last_rollover_eval,
                    now,
                    cfg,
                    reactive_pending,
                );
                if eval_due && auto_rollover.is_enabled() {
                    let live_cfg = auto_rollover.patched(cfg);
                    let request = automatic_rollover_request(
                        state_dir,
                        &live_cfg,
                        session.as_str(),
                        &seat_short,
                        adapter.provider_for_model(seat_model_from_turn_env(turn_env)),
                        supervision,
                        debounce,
                        interactive_from_turn_env(turn_env),
                    );
                    reactive_pending = super::seat::load(state_dir, &seat_short)
                        .and_then(|seat| seat.pending)
                        .is_some_and(|pending| {
                            matches!(pending.cause, super::seat::Cause::Reactive { .. })
                        });
                    request
                } else {
                    None
                }
            }
            None => None,
        };
        if let Some(req) = swap_req {
            let may_act = handover_may_act(supervision, Instant::now(), debounce, req.force);
            if !may_act {
                let reason = "mid-turn; retry once idle, or pass --force".to_string();
                // An automatic request has no waiting requester to ack, and
                // its seat transaction is already open -- close it here so
                // the seat is not left `Prepared` for a swap that never ran.
                if let Some(generation) = req.generation {
                    let _ = super::rollover::fail(
                        state_dir,
                        "wrap",
                        &seat_short,
                        generation,
                        &reason,
                        super::state::now_secs(),
                    );
                } else {
                    super::handover::write_ack(
                        state_dir,
                        &bar.session_short,
                        &super::handover::HandoverAck {
                            ok: false,
                            reason: Some(reason.clone()),
                            ..Default::default()
                        },
                    );
                }
                announcer.emit(&Event::HandoverRefused {
                    reason: reason.clone(),
                });
                let _ = super::log::append(
                    state_dir,
                    &super::log::Decision {
                        ts: super::state::now_secs(),
                        session: session.as_str(),
                        verb: "wrap",
                        verdict: "handover",
                        score: supervision.score,
                        action: "handover-refused",
                        detail: &reason,
                        observed_at: None,
                    },
                );
            } else {
                supervision.cooldown_at_signal = Some(supervision.signals_seen);
                // Same P5 reasoning as a rot-triggered restart: park the
                // record on zirv's own (unquestionably alive) pid for the
                // duration of the swap, since a concurrent `sessions::list`
                // sweep must never delete this very much live session's
                // record while its old child is being killed and no new one
                // exists yet.
                session_guard.adopt_child_pid(std::process::id());
                match perform_handover_swap(
                    child,
                    child_guard,
                    session_guard,
                    pair,
                    writer,
                    &mut mail_watch,
                    &generation,
                    &tx,
                    cpr_filter,
                    bar,
                    adapter,
                    distiller_model,
                    turn_env,
                    transcript,
                    server,
                    session,
                    repo,
                    cfg,
                    state_dir,
                    role,
                    memory_slug,
                    grace,
                    tail_items,
                    distiller_timeout,
                    last_size,
                    announcer,
                    &req,
                ) {
                    Ok(mut outcome) => {
                        let stored_text = match &outcome.stored {
                            Ok(path) => path.display().to_string(),
                            Err(e) => format!("not stored: {e}"),
                        };
                        // Issue #358: the pty now runs the successor, but the
                        // seat is only committed once that successor has
                        // actually answered -- watched from the tick below,
                        // never blocked on here (a blocking probe would
                        // freeze the operator's own terminal).
                        // T4 (C-3): only a manual request has a `zirv ctx
                        // handover` process waiting on an ack -- writing one
                        // for an automatic rollover leaves a stale `ok: true`
                        // on disk that the operator's NEXT handover would
                        // read as an answer to its own request. Same
                        // manual/automatic discriminator the refusal arm
                        // above already uses.
                        if let Some(generation) = req.generation {
                            pending_rollover = Some(PendingRollover {
                                generation,
                                signals_at_swap: supervision.signals_seen,
                                started: Instant::now(),
                            });
                        } else {
                            super::handover::write_ack(
                                state_dir,
                                &bar.session_short,
                                &super::handover::HandoverAck {
                                    ok: true,
                                    reason: None,
                                    from_agent: Some(outcome.from_agent.clone()),
                                    from_model: Some(outcome.from_model.clone()),
                                    to_agent: Some(outcome.to_agent.clone()),
                                    to_model: Some(outcome.to_model.clone()),
                                    stored: Some(stored_text.clone()),
                                },
                            );
                        }
                        announcer.emit(&Event::Handover {
                            from_agent: outcome.from_agent.clone(),
                            from_model: outcome.from_model.clone(),
                            to_agent: outcome.to_agent.clone(),
                            to_model: outcome.to_model.clone(),
                            stored: stored_text.clone(),
                        });
                        let _ = super::log::append(
                            state_dir,
                            &super::log::Decision {
                                ts: super::state::now_secs(),
                                session: session.as_str(),
                                verb: "wrap",
                                verdict: "handover",
                                score: supervision.score,
                                action: "handover",
                                detail: &format!(
                                    "{}/{} -> {}/{} ({} handoff at {})",
                                    outcome.from_agent,
                                    outcome.from_model,
                                    outcome.to_agent,
                                    outcome.to_model,
                                    outcome.source,
                                    stored_text
                                ),
                                observed_at: None,
                            },
                        );
                        if outcome.native.is_some() {
                            *native_successor = outcome.native.take();
                            return Ok(0);
                        }
                    }
                    Err(e) => {
                        let reason = e.to_string();
                        // Issue #358: the transaction is closed against the
                        // successor that failed, so `seat::abort`'s own visit
                        // record keeps the next evaluation at this same epoch
                        // from picking it again.
                        // T4 (C-3): as in the refusal arm above -- an automatic
                        // rollover closes its own transaction and has nobody
                        // waiting on an ack; only a manual request writes one.
                        if let Some(generation) = req.generation {
                            let _ = super::rollover::fail(
                                state_dir,
                                "wrap",
                                &seat_short,
                                generation,
                                &reason,
                                super::state::now_secs(),
                            );
                        } else {
                            super::handover::write_ack(
                                state_dir,
                                &bar.session_short,
                                &super::handover::HandoverAck {
                                    ok: false,
                                    reason: Some(reason.clone()),
                                    ..Default::default()
                                },
                            );
                        }
                        note_failure(
                            supervision,
                            Some((state_dir, session.as_str())),
                            &reason,
                            announcer,
                        );
                        let _ = super::log::append(
                            state_dir,
                            &super::log::Decision {
                                ts: super::state::now_secs(),
                                session: session.as_str(),
                                verb: "wrap",
                                verdict: "handover",
                                score: supervision.score,
                                action: "handover-failed",
                                detail: &reason,
                                observed_at: None,
                            },
                        );
                        let status = child.wait()?;
                        let code = status.exit_code() as i32;
                        announcer.emit(&Event::SessionEnded {
                            agent: adapter.name().to_string(),
                            code,
                        });
                        return Ok(code);
                    }
                }
            }
        }

        // Issue #358 (task 5): the open rollover transaction's readiness
        // watch. Purely local state (a signal count, two instants), so it is
        // cheap enough for the ordinary tick and never blocks the pty pump.
        // Reaching this line at all means the child is alive: the loop's own
        // `try_wait` arm above returns before it otherwise.
        if let Some(pending) = pending_rollover.as_ref() {
            let readiness = super::rollover::successor_readiness(
                true,
                adapter.capabilities().turn_signal,
                supervision.signals_seen > pending.signals_at_swap,
                signal_less_mail_ready(
                    supervision,
                    Instant::now(),
                    Duration::from_millis(cfg.dash.idle_quiet_ms),
                ),
                Instant::now().saturating_duration_since(pending.started),
                Duration::from_secs(cfg.handoff.timeout_secs),
            );
            match readiness {
                super::rollover::Readiness::Ready => {
                    let _ = super::rollover::commit(
                        state_dir,
                        "wrap",
                        &seat_short,
                        pending.generation,
                        session.as_str(),
                        super::state::now_secs(),
                    );
                    pending_rollover = None;
                }
                super::rollover::Readiness::TimedOut => {
                    // DEVIATION (documented): wrap quits the predecessor
                    // before it can launch the successor, so unlike
                    // `Pane::handover` there is no source left to restore --
                    // and killing a live-but-silent successor here would end
                    // the operator's session outright, which `wrap` may never
                    // do. The transaction is aborted (recording the visit, so
                    // the next evaluation tries the NEXT candidate) and the
                    // seat is re-registered onto the successor that is in
                    // fact running, so the record still describes reality.
                    let now_secs = super::state::now_secs();
                    let _ = super::rollover::fail(
                        state_dir,
                        "wrap",
                        &seat_short,
                        pending.generation,
                        "the successor did not answer within handoff.timeout_secs",
                        now_secs,
                    );
                    let model = turn_env
                        .iter()
                        .find(|(key, _)| key == adapters::SEAT_MODEL_ENV)
                        .map(|(_, value)| value.clone());
                    let _ = super::seat::register(
                        state_dir,
                        &seat_short,
                        session.as_str(),
                        adapter.name(),
                        model.as_deref(),
                        adapter.provider_for_model(model.as_deref()),
                        role.label(),
                        false,
                        now_secs,
                    );
                    pending_rollover = None;
                }
                super::rollover::Readiness::Waiting | super::rollover::Readiness::Dead => {}
            }
        }

        // T12b: ticks on the ordinary ~100ms poll, so the bar still
        // refreshes (usage, mail, a still-degrading session) both right
        // after a turn signal and during a long turn with none at all.
        // `redraw_bar_if_due` is what actually enforces the 1s throttle and
        // the no-op-when-unchanged check; this call is cheap otherwise.
        redraw_bar_if_due(bar, supervision, state_dir, repo, Instant::now());

        let action = match action_for(supervision, Instant::now(), debounce) {
            action @ (Action::Compact | Action::Restart) => {
                let now = Instant::now();
                let kind = match action {
                    Action::Compact => super::inject_gate::InjectKind::Compact,
                    _ => super::inject_gate::InjectKind::Restart,
                };
                let facts = super::inject_gate::InjectFacts {
                    rot_score: Some(supervision.score),
                    restart_at: cfg.score.restart_at,
                    output_idle_ms: Some(
                        now.saturating_duration_since(supervision.last_output)
                            .as_millis()
                            .min(u128::from(u64::MAX)) as u64,
                    ),
                    ..Default::default()
                };
                match inject_gate.check(cfg, state_dir, kind, supervision.signals_seen, facts, now)
                {
                    super::inject_gate::Gate::Proceed => action,
                    super::inject_gate::Gate::Hold => Action::None,
                }
            }
            action => action,
        };
        match action {
            Action::None => {}
            Action::Advise => {
                announcer.emit(&Event::RotAdvisory {
                    score: supervision.score,
                    tokens: 0,
                });
                // Advise once per turn.
                supervision.cooldown_at_signal = Some(supervision.signals_seen);
            }
            Action::Compact => {
                inject_gate.injected(
                    super::inject_gate::InjectKind::Compact,
                    supervision.signals_seen,
                );
                let defer = adapter.capabilities().defer_injection_submit;
                // Issue #798 (`[jev] compaction_select`): best-effort, off by
                // default -- `compaction_focus_for_transcript` checks the
                // gate and credential BEFORE touching the transcript at all
                // (review of 6bdd7675, defect #1), so with the gate off
                // (today's default) this never reads or parses the
                // transcript inline in the pump loop, and with the gate on
                // it bounds the Jev call so it cannot stall the pump for the
                // full configured typesafe timeout.
                let compact_focus = handoff::compaction_focus_for_transcript(
                    cfg,
                    state_dir,
                    adapter.as_ref(),
                    transcript.path(),
                    tail_items,
                    super::supervise::COMPACT_FOCUS,
                );
                let injected = writer
                    .lock()
                    .map_err(|_| "pty writer poisoned".to_string())
                    .and_then(|mut sink| {
                        let command = adapter.compact_command().unwrap_or("/compact");
                        inject_compact(&mut *sink, command, &compact_focus, defer)
                            .map_err(|e| e.to_string())
                    });

                // Arm the cooldown before verifying so a failed verification
                // cannot turn into a retry loop.
                supervision.cooldown_at_signal = Some(supervision.signals_seen);

                // No transcript means no verification is possible, and a
                // deadline spent polling a file nobody writes would block the
                // pump for nothing.
                let failure = match (injected, transcript.path()) {
                    (Err(_), _) => Some("compact injection failed"),
                    (Ok(()), None) => Some("no transcript reported, compaction unverifiable"),
                    (Ok(()), Some(path)) => {
                        let seen = verify_compaction(
                            &mut Watcher::new(path.to_path_buf()),
                            adapter.as_ref(),
                            Instant::now() + inject_timeout,
                        )
                        .unwrap_or(false);
                        if seen {
                            None
                        } else {
                            Some("compaction not verified")
                        }
                    }
                };
                let verified = failure.is_none();
                announcer.emit(&Event::Compact { verified });

                if let Some(reason) = failure {
                    note_failure(
                        supervision,
                        Some((state_dir, session.as_str())),
                        reason,
                        announcer,
                    );
                }
                let _ = super::log::append(
                    state_dir,
                    &super::log::Decision {
                        ts: super::state::now_secs(),
                        session: session.as_str(),
                        verb: "wrap",
                        verdict: "compact",
                        score: supervision.score,
                        action: if verified {
                            "inject"
                        } else {
                            "inject-unverified"
                        },
                        detail: &transcript
                            .path()
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "no transcript reported".to_string()),
                        observed_at: None,
                    },
                );
            }
            Action::Restart => 'restart: {
                supervision.cooldown_at_signal = Some(supervision.signals_seen);
                inject_gate.injected(
                    super::inject_gate::InjectKind::Restart,
                    supervision.signals_seen,
                );

                // Before anything is torn down: a restart chain that has
                // already tripped means this session is in a loop that
                // relaunching will not break, so `wrap` stands down to plain
                // passthrough (the child keeps running, untouched) rather than
                // spending another distiller call and another pty on it. Same
                // breaker `exec` gates on -- see `tripped_restart_chain`.
                if let Some(boots) =
                    tripped_restart_chain(state_dir, repo, cfg, super::state::now_secs())
                {
                    let _ = super::log::append(
                        state_dir,
                        &super::log::Decision {
                            ts: super::state::now_secs(),
                            session: session.as_str(),
                            verb: "wrap",
                            verdict: "restart",
                            score: supervision.score,
                            action: "chain-tripped",
                            detail: &format!(
                                "{boots} unplanned restarts within the configured gap; not \
                                 relaunching"
                            ),
                            observed_at: None,
                        },
                    );
                    note_failure(
                        supervision,
                        Some((state_dir, session.as_str())),
                        &format!(
                            "restart-chain breaker tripped ({boots} restarts within the \
                             configured gap); supervising no further -- run `zirv ctx status`"
                        ),
                        announcer,
                    );
                    break 'restart;
                }

                // P5: park the record on zirv's own (unquestionably alive) pid
                // for the duration of the restart. Everything from here to the
                // respawn below -- the distiller call, `quit_child`'s grace
                // ladder, the fresh pty -- happens while the *old* child is
                // being killed, and `sessions::list` sweeps any record whose
                // pid is dead. That listing runs on other processes' schedules
                // (`zirv ctx status`, `nudge`, `send --to-session`, a
                // dashboard's own ~1s registry refresh), so leaving the record
                // pointing at the child being killed meant a concurrent reader
                // could delete this very much live session's record mid-restart
                // and strand every message addressed to it. The real child pid
                // is adopted again the moment there is one.
                session_guard.adopt_child_pid(std::process::id());

                let jsonl = transcript
                    .path()
                    .map(|path| std::fs::read_to_string(path).unwrap_or_default())
                    .unwrap_or_default();
                let ctx = adapter.structural_context(&jsonl, tail_items);
                let previous = handoff::latest_for_repo(state_dir, repo)
                    .ok()
                    .flatten()
                    .map(|(_, h)| h);
                let (note, source) = handoff::distill_or_structural_with_jev(
                    cfg,
                    state_dir,
                    adapter.as_ref(),
                    distiller_model.as_str(),
                    &ctx,
                    distiller_timeout,
                    announcer.enabled,
                    previous.as_ref(),
                );
                let stored = handoff::store(state_dir, repo, session.as_str(), &note);
                // N6: opt-in (`cfg.memory.harvest`, default off) and only
                // from a genuinely distilled handoff -- never the mechanical
                // structural fallback. Best-effort: a harvest failure must
                // never turn a successful restart into a failed one.
                if source == "distilled" {
                    let _ = super::memory::harvest_durable(
                        adapter.as_ref(),
                        distiller_model.as_str(),
                        &note,
                        repo,
                        state_dir,
                        memory_slug,
                        cfg,
                    );
                }

                // The writer is taken first, and the generation is bumped only
                // once this restart is genuinely under way. The two used to be
                // the other way round, which meant a poisoned writer -- the
                // one way `quit` can fail -- left the generation bumped over a
                // child that had never even been asked to quit: `relaunched`
                // stayed false, the pump fell through to `child.wait()`, and
                // the old reader thread's `still_current` was already false, so
                // that very much alive TUI painted to nobody for the rest of
                // the run.
                let (new_generation, quit) = match writer.lock() {
                    Ok(mut sink) => {
                        // Bumped before the old child is even asked to quit:
                        // its reader thread's own EOF can land at any point
                        // from here on (quit_child alone may take up to
                        // `grace`), and once bumped that thread's
                        // `still_current` check is already false, so a pty
                        // closing on its way out can never be mistaken for the
                        // fresh one that is about to replace it.
                        let bumped =
                            generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        let quit = quit_child(&mut *sink, child, adapter.quit_sequence(), grace)
                            .map_err(|e| e.to_string());
                        (Some(bumped), quit)
                    }
                    Err(_) => (None, Err("pty writer poisoned".to_string())),
                };

                // That session is over. Whatever it was writing is now a dead
                // file, and the replacement reports its own on its first turn.
                transcript.forget();

                // Item 6 audit: captured so `note_failure`'s own announcement
                // can name *why* the restart failed -- quit/writer-lock
                // trouble, or the fresh pty/spawn itself -- rather than the
                // same generic "relaunch failed" either way. Neither error
                // used to be kept past this match at all.
                let mut relaunch_error = quit.as_ref().err().cloned();
                let relaunched = match (new_generation, quit.is_ok()) {
                    (Some(new_generation), true) => {
                        match relaunch(
                            adapter.as_ref(),
                            repo,
                            &note,
                            extra,
                            turn_env.as_slice(),
                            relaunch_size(bar, last_size),
                            &cfg.screen.thresholds(),
                            state_dir,
                            session.as_str(),
                        ) {
                            Ok((fresh_pair, fresh_child, fresh_reader, fresh_writer)) => {
                                spawn_output_thread(
                                    fresh_reader,
                                    tx.clone(),
                                    generation.clone(),
                                    new_generation,
                                    bar.stdout_lock.clone(),
                                );
                                if let Ok(mut sink) = writer.lock() {
                                    *sink = fresh_writer;
                                }
                                // A CR still owed to the replaced child must
                                // not be typed into the fresh one (same rule
                                // as the handover swap above).
                                mail_watch.clear_pending_submit();
                                // The fresh pty ran its own console-host probe,
                                // so the terminal is about to answer that one
                                // too; see `CprFilter`.
                                if let Ok(mut filter) = cpr_filter.lock() {
                                    filter.arm(Instant::now());
                                }
                                *pair = fresh_pair;
                                *child = fresh_child;
                                // P1/P2/P3: the old child was tree-killed by
                                // `quit_child` above, so releasing its guard
                                // now only takes it out of the console-close
                                // registry and closes a job with nothing left
                                // in it. Released *before* the new adoption
                                // so a pid the OS has already recycled cannot
                                // be deregistered out from under the fresh
                                // child.
                                child_guard.release();
                                *child_guard =
                                    super::supervise::ChildGuard::adopt(child.process_id());
                                // P5: and the registry record follows the
                                // child it names. Left pointing at the
                                // replaced child's dead pid, `sessions::list`
                                // would sweep the record and this very much
                                // live session would disappear from `zirv ctx
                                // status`.
                                if let Some(child_pid) = child.process_id() {
                                    session_guard.adopt_child_pid(child_pid);
                                }
                                true
                            }
                            Err(e) => {
                                relaunch_error = Some(e.to_string());
                                false
                            }
                        }
                    }
                    _ => false,
                };

                if relaunched {
                    announcer.emit(&Event::Restart {
                        style: source.to_string(),
                        stored: match &stored {
                            Ok(path) => path.display().to_string(),
                            Err(e) => format!("not stored: {e}"),
                        },
                    });
                } else {
                    let reason = relaunch_error.unwrap_or_else(|| "relaunch failed".to_string());
                    note_failure(
                        supervision,
                        Some((state_dir, session.as_str())),
                        &reason,
                        announcer,
                    );
                }
                let _ = super::log::append(
                    state_dir,
                    &super::log::Decision {
                        ts: super::state::now_secs(),
                        session: session.as_str(),
                        verb: "wrap",
                        verdict: "restart",
                        score: supervision.score,
                        action: if relaunched {
                            "restart"
                        } else {
                            "restart-failed"
                        },
                        detail: &match stored {
                            Ok(path) => format!("{source} handoff at {}", path.display()),
                            Err(e) => format!("{source} handoff not stored: {e}"),
                        },
                        observed_at: None,
                    },
                );
                if !relaunched {
                    let status = child.wait()?;
                    let code = status.exit_code() as i32;
                    // The `Degraded` announcement just above already said
                    // *why* (the relaunch failure reason); this says the
                    // session is over, the same as every other exit point.
                    announcer.emit(&Event::SessionEnded {
                        agent: adapter.name().to_string(),
                        code,
                    });
                    return Ok(code);
                }
            }
        }

        // T13: the live mail wake-up.
        //
        // Deliberately *after* the escalation ladder above: a compaction or
        // restart that just fired armed `cooldown_at_signal`, which makes
        // `may_inject` false here, so this arm falls back to the
        // announcement channel rather than typing a second line into a child
        // that was just handed a `/compact`. The existing cooldown is the
        // mutual exclusion; nothing extra is needed for it.
        //
        // Every failure here is a no-op by construction: an unreadable
        // mailbox yields `None` and nothing happens, a poisoned writer
        // degrades to the announcement channel, and neither ever calls
        // `note_failure` -- mail is advisory, and a wrapped session must
        // never be made worse by it.
        let now = Instant::now();
        if mail_polling_enabled(bar.mail_enabled, &bar.session_short, supervision.degraded)
            && mail_watch.due(now)
        {
            mail_watch.polled(now);
            if let Some(unread) = unread_mail_for_session(
                state_dir,
                repo,
                adapter.name(),
                &bar.session_short,
                bar.mail_enabled,
            ) {
                let facts = mail_facts(&unread);
                mail_watch.forget_missing(&facts);
                // A signal-less adapter (codex today) never satisfies
                // `may_inject`'s own `signals_seen > 0` precondition -- see
                // `signal_less_mail_ready`'s own doc comment. `cfg.dash.
                // idle_quiet_ms` is reused rather than a new wrap-only knob:
                // it already means exactly "how long a signal-less session's
                // pty must be quiet before zirv treats it as idle", the same
                // question this is, and it is already an operator-only,
                // non-`REPO_FORBIDDEN` timing knob over a session the
                // operator chose to run interactively.
                let caps = adapter.capabilities();
                let ready = mail_inject_ready(
                    caps.turn_signal,
                    supervision,
                    now,
                    debounce,
                    Duration::from_millis(cfg.dash.idle_quiet_ms),
                );
                let mut action = mail_watch.decide(&facts, ready);
                if matches!(action, MailAction::Inject { .. }) {
                    let now_secs = super::state::now_secs();
                    let gate_facts = super::inject_gate::InjectFacts {
                        unread: Some(unread.len() as u64),
                        oldest_unread_age_secs: unread
                            .iter()
                            .map(|(_, message)| now_secs.saturating_sub(message.sent))
                            .max(),
                        sender: facts
                            .iter()
                            .map(|fact| {
                                super::inject_gate::sender_class(
                                    &fact.from_agent,
                                    &fact.from_short,
                                    parent_short,
                                )
                            })
                            .max()
                            .unwrap_or_default(),
                        output_idle_ms: Some(
                            now.saturating_duration_since(supervision.last_output)
                                .as_millis()
                                .min(u128::from(u64::MAX)) as u64,
                        ),
                        ..Default::default()
                    };
                    // Held exactly as an unready child would be: announced
                    // on the `zirv ▸` channel, injection still owed.
                    if inject_gate.check(
                        cfg,
                        state_dir,
                        super::inject_gate::InjectKind::MailPty,
                        supervision.signals_seen,
                        gate_facts,
                        now,
                    ) == super::inject_gate::Gate::Hold
                    {
                        action = mail_watch.decide(&facts, false);
                    }
                }
                match action {
                    MailAction::None => {}
                    MailAction::Announce { count, ids } => {
                        // R5: `try_emit`, not `emit` -- an advisory the
                        // channel swallowed must stay unannounced so the next
                        // poll retries it. See `MailWatch::note_announcement`.
                        let landed = announcer.try_emit(&Event::MailWaiting { count });
                        mail_watch.note_announcement(&ids, landed);
                    }
                    // Issue #118: single-burst for a turn-signal-capable
                    // adapter (claude); `inject_compact` above now shares
                    // this same capability-gated two-phase shape, for the
                    // same reason -- see that function's own doc comment.
                    // For a `defer_injection_submit` adapter (codex)
                    // `write_mail_advisory` splits this into a phase-1 write
                    // and a deadline the drain just below this match
                    // submits later; either way `commit_injected` fires on
                    // `wrote` alone, i.e. at phase 1, the same "advised"
                    // moment `dash::pane::inject_visible` already commits
                    // its own state at.
                    MailAction::Inject {
                        count,
                        from_agent,
                        from_short,
                        ids,
                    } => {
                        // Issue #249: `from_short` is already `sessions::
                        // short_id`'s own vocabulary (`mail_facts`), the
                        // same one `parent_short` is in -- a direct compare.
                        let is_parent = parent_short.is_some_and(|p| p == from_short);
                        let wrote = write_mail_advisory(
                            &mut mail_watch,
                            writer,
                            caps.defer_injection_submit,
                            count,
                            &from_agent,
                            &from_short,
                            is_parent,
                        );
                        if wrote {
                            mail_watch.commit_injected(&ids);
                            inject_gate.injected(
                                super::inject_gate::InjectKind::MailPty,
                                supervision.signals_seen,
                            );
                        } else {
                            // Same R5 rule on the degrade path: a poisoned
                            // writer plus a swallowed announcement must leave
                            // the advisory owed, not quietly discharged.
                            let landed = announcer.try_emit(&Event::MailWaiting { count });
                            mail_watch.note_announcement(&ids, landed);
                        }
                    }
                }
            }
        }

        // Issue #118: drains a still-owed mail-advisory `\r`
        // (`MailWatch::pending_submit`) once its deadline has passed --
        // unconditioned by `mail_polling_enabled`/`mail_watch.due` above,
        // because this is finishing a write an earlier tick already
        // committed (`commit_injected` already fired at phase 1), not a new
        // poll, so it must not wait on either gate. A failed write here is
        // safe to simply retry on a later tick -- see
        // `dash::pane::write_submit_cr`'s own doc comment.
        if mail_watch.pending_submit_due(Instant::now())
            && let Ok(mut sink) = writer.lock()
            && super::dash::pane::write_submit_cr(&mut *sink).is_ok()
        {
            mail_watch.clear_pending_submit();
        }

        if let Ok(size) = window_size(STDIN_FD)
            && size != last_size
        {
            last_size = size;
            // B1: `chrome::resize_decision` is the single source of truth for
            // what a resize does to the bar and the pty -- see its own tests
            // for the shrink-below-floor and widen-after-degrade cases this
            // used to get wrong (the child pinned at a stale reserved size
            // forever, even once the terminal widened back out).
            let decision = super::chrome::resize_decision(bar.active(), size);
            if decision.disables_bar {
                disable_bar_at_current_size(bar, size);
            } else if decision.set_scroll_region {
                bar.cols = size.0;
                bar.rows = size.1;
                let region = super::chrome::scroll_region_sequence(bar.rows);
                let region_ok = match bar.stdout_lock.lock() {
                    Ok(_guard) => {
                        let mut stdout = std::io::stdout();
                        stdout
                            .write_all(region.as_bytes())
                            .and_then(|()| stdout.flush())
                            .is_ok()
                    }
                    Err(_) => false,
                };
                bar.disabled = super::chrome::after_redraw_attempt(bar.disabled, region_ok);
                // C5: set on success, never *cleared* on failure. A resize
                // whose region write failed leaves whatever region was
                // already in effect still fencing the console, so clearing
                // the flag here would tell the emergency handler it owes the
                // terminal nothing while the terminal was still fenced.
                // `BAR_ACTIVE` means "we have set a region that is still
                // outstanding", and only a successful `reset_bar` retires it.
                if region_ok {
                    super::term::set_bar_active(true);
                }
                if !bar.disabled {
                    // The bar's own row moved; the next throttle tick must
                    // redraw it even if the text is unchanged.
                    bar.last_text = None;
                }
            }
            let _ = pair.master.resize(PtySize {
                rows: decision.pty_size.1,
                cols: decision.pty_size.0,
                pixel_width: 0,
                pixel_height: 0,
            });
        }

        // B1: catches a disable that just happened above (this tick's
        // resize shrank below the floor) *and* one that happened earlier
        // with no resize event at all (a redraw or lock failure) -- either
        // way, `bar_needs_recovery` makes this a one-time, idempotent
        // reaction, so calling it unconditionally every tick is cheap and
        // correct in both cases.
        recover_bar_to_full_size(bar, pair, last_size);

        std::thread::sleep(PUMP_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::RecordingWriter;
    use super::*;
    use crate::commands::ctx::adapters::claude::ClaudeAdapter;
    use crate::commands::ctx::supervise::Watcher;
    #[test]
    fn the_injected_command_carries_focus_instructions_and_ends_with_a_carriage_return() {
        let mut sink: Vec<u8> = Vec::new();
        inject_compact(&mut sink, "/compact", COMPACT_FOCUS, false).expect("inject");
        let text = String::from_utf8(sink).expect("utf8");
        assert!(text.starts_with("/compact "), "got {text:?}");
        assert!(text.contains(COMPACT_FOCUS));
        assert!(
            text.ends_with('\r'),
            "a TUI submits on carriage return: {text:?}"
        );
        assert_eq!(text.matches('\r').count(), 1, "exactly one submit");
        assert!(!text.contains('\n'), "no stray newline: {text:?}");
    }

    /// Issue #118 follow-up: `inject_compact` is reachable for a
    /// `defer_injection_submit` adapter (codex) too, once a live handover
    /// swaps the adapter mid-pump-loop -- see this function's own doc
    /// comment. Non-deferring stays exactly the pre-existing single burst:
    /// one write, command text and the submitting CR together.
    #[test]
    fn inject_compact_stays_single_burst_for_a_non_deferring_adapter() {
        let mut writer = RecordingWriter::default();
        inject_compact(&mut writer, "/compact", COMPACT_FOCUS, false).expect("inject");
        let chunks = writer.chunks.lock().expect("lock");
        assert_eq!(chunks.len(), 1, "one write, not two: {chunks:?}");
        assert_eq!(
            String::from_utf8_lossy(&chunks[0]),
            format!("/compact {COMPACT_FOCUS}\r")
        );
    }

    /// For a deferring adapter the command text lands first with no
    /// trailing CR, then the lone CR lands as its own write once
    /// `INJECTION_SUBMIT_DELAY` has passed -- mirroring
    /// `write_mail_advisory`'s deferred shape, but blocking inline rather
    /// than arming `MailWatch::pending_submit`: the pump loop calls
    /// `verify_compaction` immediately after this and blocks there anyway,
    /// so there is no responsiveness cost to blocking here first.
    #[test]
    fn inject_compact_defers_the_submitting_cr_for_a_deferring_adapter() {
        let mut writer = RecordingWriter::default();
        let started = std::time::Instant::now();
        inject_compact(&mut writer, "/compact", COMPACT_FOCUS, true).expect("inject");
        assert!(
            started.elapsed() >= INJECTION_SUBMIT_DELAY,
            "the CR must not land before the submit delay has passed"
        );
        let chunks = writer.chunks.lock().expect("lock");
        assert_eq!(
            chunks.len(),
            2,
            "phase 1 text, then the lone CR: {chunks:?}"
        );
        assert_eq!(
            String::from_utf8_lossy(&chunks[0]),
            format!("/compact {COMPACT_FOCUS}")
        );
        assert_eq!(chunks[1], b"\r".to_vec());
    }

    #[test]
    fn the_focus_text_names_what_to_preserve() {
        for needle in [
            "task",
            "constraint",
            "file",
            "decision",
            "reasoning",
            "error",
            "next step",
        ] {
            assert!(
                COMPACT_FOCUS.to_lowercase().contains(needle),
                "focus text should mention {needle}: {COMPACT_FOCUS}"
            );
        }
        assert!(!COMPACT_FOCUS.contains('\u{2014}'));
        assert!(
            !COMPACT_FOCUS.contains('\n'),
            "typed into a single TUI line: {COMPACT_FOCUS}"
        );
    }

    #[test]
    fn verification_succeeds_when_a_compaction_event_appears() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n",
        )
        .expect("write");

        let mut watcher = Watcher::new(path.clone());
        let _ = watcher.read_appended();

        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open");
            use std::io::Write as _;
            writeln!(
                file,
                "{{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"content\":\"x\"}}"
            )
            .expect("append");
        });

        let adapter = ClaudeAdapter::new(None);
        let verified = verify_compaction(
            &mut watcher,
            &adapter,
            Instant::now() + Duration::from_secs(5),
        )
        .expect("verify");
        writer.join().expect("writer thread");
        assert!(verified);
    }

    #[test]
    fn verification_gives_up_at_the_deadline_instead_of_retrying() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"user\",\"message\":{\"content\":\"go\"}}\n",
        )
        .expect("write");
        let mut watcher = Watcher::new(path);
        let adapter = ClaudeAdapter::new(None);

        let started = Instant::now();
        let verified = verify_compaction(
            &mut watcher,
            &adapter,
            Instant::now() + Duration::from_millis(300),
        )
        .expect("verify");
        assert!(!verified);
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
