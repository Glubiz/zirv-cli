//! pump support for the interactive supervisor.

use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn pump(
    child: &mut Box<dyn portable_pty::Child + Send + Sync>,
    // Swap both guards on relaunch so the old child cannot outlive this
    // supervisor and the registry follows the new pid.
    child_guard: &mut super::supervise::ChildGuard,
    session_guard: &mut super::sessions::SessionGuard,
    rx: &mpsc::Receiver<PumpEvent>,
    pair: &mut portable_pty::PtyPair,
    supervision: &mut InjectionState,
    server: Option<&super::signal::SignalServer>,
    // `&mut Box<dyn AgentAdapter>`, not `&dyn AgentAdapter`: a handover swap
    // replaces the boxed adapter in place, so every later pump tick must see
    // the new one.
    adapter: &mut Box<dyn AgentAdapter>,
    writer: &std::sync::Arc<std::sync::Mutex<Box<dyn Write + Send>>>,
    transcript: &mut TranscriptSource,
    state_dir: &super::state::StateDir,
    session: &super::event::SessionId,
    debounce: Duration,
    inject_timeout: Duration,
    repo: &Path,
    env: EnvLookup<'_>,
    tail_items: usize,
    // `&mut String`, not `&str`: a handover swap recomputes this for the new
    // adapter's own distiller default, so a later rot restart does not keep
    // quoting the predecessor's model name.
    distiller_model: &mut String,
    distiller_timeout: Duration,
    cfg: &CtxConfig,
    memory_slug: &str,
    grace: Duration,
    tx: mpsc::Sender<PumpEvent>,
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    // `&mut Vec<_>`, not `&[_]`: a handover swap rebuilds this for the new
    // adapter/model, so a later rot restart relaunches with the right identity.
    turn_env: &mut Vec<(String, String)>,
    cpr_filter: &std::sync::Arc<std::sync::Mutex<CprFilter>>,
    announcer: &Announcer,
    bar: &mut BarRuntime,
    role: super::prompt::PromptRole,
    // Resolve the supervising session once at launch. (#249)
    parent_short: Option<&str>,
    native_successor: &mut Option<super::dash::Pane>,
) -> CtxResult<i32> {
    let mut last_size = window_size(STDIN_FD).unwrap_or(DEFAULT_SIZE);
    let mut mail_watch = MailWatch::default();
    let mut inject_gate = super::inject_gate::AsyncGate::default();
    // Handover file reads have their own cadence, independent of mail.
    let mut last_handover_poll: Option<Instant> = None;
    // Delay rollover I/O until after startup; only one transaction may be open. (#358)
    let mut last_rollover_eval: Option<Instant> = Some(Instant::now());
    let seat_short = bar.session_short.clone();
    let mut reactive_pending = super::seat::load(state_dir, &seat_short)
        .and_then(|seat| seat.pending)
        .is_some_and(|pending| matches!(pending.cause, super::seat::Cause::Reactive { .. }));
    let mut pending_rollover: Option<PendingRollover> = None;
    let is_orchestrator = role == PromptRole::Orchestrator;
    // Reload the rollover switch on cadence because this session may be
    // long-lived. A live disable prevents new swaps, but an open transaction
    // must still commit or abort. (#780)
    let mut auto_rollover =
        super::rollover::LiveAutoRollover::new(repo, env, cfg.auto_orchestrator_rollover());

    loop {
        if let Some(status) = child.try_wait()? {
            // A successor that never answered cannot commit its transaction. (#358)
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
            // Announce the child exit even when no supervisory action fired.
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
            // Input starts the next turn; the first turn is stamped at spawn.
            // The last completed turn number is still current here. (#281)
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
            // Clear in-flight state at the turn boundary so a later crash
            // between turns is not marked as interrupted. (#281)
            session_guard.clear_in_flight();
            if let Some(event) =
                super::announce::verdict_change(previous_verdict, supervision.verdict, signal.score)
            {
                announcer.emit(&event);
            }

            // Interactive nudges are advisory only: never restart or deliver
            // message bodies through the pty.
            if let Some(from) = super::sessions::claim_nudge_marker(state_dir, &bar.session_short) {
                announcer.emit(&Event::Nudge {
                    from,
                    disposition: super::announce::NudgeDisposition::Advisory,
                });
            }
        }

        // Poll handover files on their own cadence and act only at verified
        // idle; this read/remove must stay off the 100ms pump path. (#84)
        let now = Instant::now();
        let handover_poll_due = handover_poll_due(last_handover_poll, now);
        if handover_poll_due {
            last_handover_poll = Some(now);
        }
        // Manual handover wins over automatic rollover on the same tick. (#358)
        let manual_req = handover_poll_due
            .then(|| super::handover::take_request(state_dir, &bar.session_short))
            .flatten();
        let swap_req = match manual_req {
            Some(req) => {
                // Abort an open automatic transaction before manual swap, or
                // its generation could commit against the wrong successor. (#358)
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
            // Check cadence before reloading config so disabled rollover
            // does not stat files every tick; advance cadence even while off.
            // Failed reload must not enable rollover. (#780)
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
                // Abort a transaction when its automatic swap never ran.
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
                // Park the registry on zirv during the swap so a concurrent
                // liveness sweep cannot delete this live session.
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
                        // Commit only after the successor answers, without
                        // blocking the terminal. Only manual requests get a
                        // waiting-process acknowledgment. (#358)
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
                        // Abort against the failed successor to avoid choosing
                        // it again this epoch; only manual requests need ack. (#358)
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

        // Watch successor readiness using local state on the ordinary tick;
        // never block the pty pump. (#358)
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
                    // The predecessor is gone, so a silent but live successor
                    // cannot be killed without ending the operator session.
                    // Abort the transaction and register the running successor. (#358)
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

        // Tick frequently; redraw itself throttles disk reads to one second.
        redraw_bar_if_due(bar, supervision, state_dir, repo, Instant::now());

        let action = match action_for(supervision, Instant::now(), debounce) {
            action @ Action::Compact => {
                let now = Instant::now();
                let kind = super::inject_gate::InjectKind::Compact;
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
                supervision.cooldown_at_signal = Some(supervision.signals_seen);
            }
            Action::Compact => {
                inject_gate.injected(
                    super::inject_gate::InjectKind::Compact,
                    supervision.signals_seen,
                );
                let defer = adapter.capabilities().defer_injection_submit;
                // Gate and bound Jev selection before reading a transcript
                // on the pty pump path. (#798)
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

                // Arm cooldown before verification to prevent retry loops.
                supervision.cooldown_at_signal = Some(supervision.signals_seen);

                // Without a transcript, verification cannot succeed.
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
            // Zirv never restarts a session because of rot: the operator does.
            Action::SuggestRestart => {
                announcer.emit(&Event::RestartSuggested {
                    score: supervision.score,
                });
                supervision.cooldown_at_signal = Some(supervision.signals_seen);
                let _ = super::log::append(
                    state_dir,
                    &super::log::Decision {
                        ts: super::state::now_secs(),
                        session: session.as_str(),
                        verb: "wrap",
                        verdict: "restart",
                        score: supervision.score,
                        action: "suggest-restart",
                        detail: "restart suggested to the operator; zirv does not restart sessions",
                        observed_at: None,
                    },
                );
            }
        }

        // Run after escalation: cooldown prevents an advisory from being
        // typed into a child just given compact or restart. Mail failures
        // remain advisory and never degrade the session.
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
                // Signal-less adapters use the configured idle quiet interval
                // as their injection gate.
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
                        // Only a landed announcement can clear its retry.
                        let landed = announcer.try_emit(&Event::MailWaiting { count });
                        mail_watch.note_announcement(&ids, landed);
                    }
                    // Commit a split advisory when text lands; its submit
                    // follows on a later pump tick. (#118)
                    MailAction::Inject {
                        count,
                        from_agent,
                        from_short,
                        ids,
                    } => {
                        // Both ids use the registry short-id vocabulary. (#249)
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
                            // A failed writer and announcement leave the
                            // advisory owed.
                            let landed = announcer.try_emit(&Event::MailWaiting { count });
                            mail_watch.note_announcement(&ids, landed);
                        }
                    }
                }
            }
        }

        // Finish an owed submit independently of new mail polling; failed
        // writes remain retryable on later ticks. (#118)
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
            // One resize decision governs both bar and pty dimensions,
            // including recovery after degradation.
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
                // A failed resize may leave the old scroll region active;
                // only successful reset clears the emergency-reset flag.
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

        // Recover once whether resize or redraw disabled the bar.
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
