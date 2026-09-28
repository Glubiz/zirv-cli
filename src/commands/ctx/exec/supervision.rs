//! Live child supervision, transcript discovery, and stall handling.

use super::*;

/// The transcript path a supervision tick should switch to, or `None` to keep
/// polling the current one.
///
/// Review round 1 (R4) added the self-heal: a path derived before the child
/// was spawned can name a file the agent never writes (`codex exec` mints its
/// own rollout id once it is running), so the adapter is asked again while the
/// current path does not exist, accepting only an answer that names a
/// DIFFERENT file which does.
///
/// Review round 2 (S1) narrows it to paths zirv itself derived, hence
/// `resolve` being an `Option`. `--transcript` documents itself as the escape
/// hatch for "the agent writes somewhere the adapter cannot derive": for such
/// a run the adapter's guess is known-wrong by construction, so re-resolving
/// onto it -- which an unconditional swap does the moment the operator's file
/// has not been written yet and a stale derived one exists -- would silently
/// supervise a file the operator never named.
fn self_heal_transcript(current: &Path, resolve: Option<&dyn Fn() -> PathBuf>) -> Option<PathBuf> {
    if current.exists() {
        return None;
    }
    let candidate = resolve?();
    (candidate != current && candidate.is_file()).then_some(candidate)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn supervise_run(
    child: &mut std::process::Child,
    deadline: Instant,
    poll: Duration,
    scorer: &mut score::IncrementalScorer,
    adapter: &dyn adapters::AgentAdapter,
    score_cfg: &super::config::ScoreConfig,
    pace_cfg: &super::config::PaceConfig,
    // Issue #272 review round 1: the caller's own resolved `[screen]`
    // config, the same narrow-purpose-parameter shape as `score_cfg`/
    // `pace_cfg` right above -- so a repo-narrowed threshold reaches the
    // live supervision poll below, not just the Stop hook's own fallback.
    screen_thresholds: &super::screen::Thresholds,
    // Issue #455 (review round 1, finding 4): the route-health policy, in
    // the same narrow-purpose-parameter shape as `score_cfg`/`pace_cfg`/
    // `screen_thresholds` above. This supervisor owns its own
    // uncheckpointed scorer, so it must feed route health itself; a
    // headless codex worker dying on connection refusals otherwise left
    // its harness reading `Healthy` forever.
    health_policy: &super::health::HealthPolicy,
    state: &StateDir,
    server: Option<&signal::SignalServer>,
    session: &str,
    // C7: this run's stable registry short id, not `short_id(session)`.
    // `zirv ctx nudge` writes its wake-up marker under the address it
    // resolved from the registry, and that address does not rotate when a
    // restart mints a fresh session -- deriving it from `session` here meant
    // a nudge sent after the first restart was never claimed.
    registry_short: &str,
    // Issue #281: cleared the instant the tick loop below sees a turn signal
    // reported by THIS session -- the turn `stamp_in_flight` marked at this
    // cycle's own spawn (the call site right before `supervise_run`) has now
    // reached a clean boundary.
    session_guard: &mut super::sessions::SessionGuard,
    // Issue #243 (review round, F3): the same `Announcer` this run's other
    // events already use, plus this run's own de-duplication memory --
    // owned above the per-restart loop (like `registry_short`/`nudged_by`),
    // so a screening summary that has not changed is announced once for
    // the whole supervised run, not once per poll or once per restart.
    announcer: &super::announce::Announcer,
    screening_announced: &mut Option<String>,
    rotted: &mut bool,
    compact_requested: &mut bool,
    compact_budget: &mut CompactBudget,
    compact_window: Duration,
    // C3: set when this session reported a turn boundary of its own.
    progressed: &mut bool,
    tap: &supervise::OutputTap,
    limit_hit: &mut bool,
    capacity_pattern: &mut Option<&'static str>,
    account_pattern: &mut Option<&'static str>,
    limit_confirmation_detail: &mut Option<String>,
    nudged_by: &mut Option<String>,
    nudges_used: u32,
    max_nudges: u32,
    can_restart: bool,
    // Issue #155, Phase 5(d): a budget checkpoint, independent of rot/nudge/
    // limit above. `transcript` is read directly here rather than folded
    // into `scorer`'s own bounded fold, because a budget needs this child's
    // whole cumulative spend, which `RotState`'s windowed segments do not
    // retain once the window has moved past them.
    transcript: &mut PathBuf,
    // Review round 1 (R4): the path above is derived BEFORE the child is
    // spawned, and `codex exec` mints its own session id and writes its
    // rollout's `session_meta` only once it is running -- so the derivation
    // answers a `rollout-<zirv id>.jsonl` that never appears, and every
    // budget/rot/spend read for the whole run lands on a missing file. This
    // asks the adapter again, but only while the current path does not exist
    // and only accepting an answer that names a DIFFERENT file which does:
    // one `exists()` per tick in the steady state, and never a redirect away
    // from a transcript that is genuinely being written. Review round 2 (S1):
    // `None` for an operator's own `--transcript`, which by definition names
    // a file the adapter cannot derive -- see `self_heal_transcript`.
    resolve_transcript: Option<&dyn Fn() -> PathBuf>,
    budget: agent::WorkerBudget,
    // Issue #169.2: every prior child's own already-harvested spend this
    // invocation has superseded, folded into every check below alongside
    // `transcript`'s own current reading (`evaluate_worker_budget`).
    prior_usage: &TranscriptUsage,
    prior_tool_calls: u32,
    soft_warned: &mut bool,
    budget_exhausted: &mut bool,
    // Issue #310 (3a): progress-clock inputs and the once-only stall latch.
    // `repo`/`cfg` feed the mail-activity progress signal (`mail::unread_
    // counts`) and, since T4 (C-2), the steering nudge the latch actually
    // delivers; the three durations are `cfg.supervise.{idle_no_tool,in_tool,
    // stall_grace}_secs`, still read once by the caller and passed narrowly,
    // like `score_cfg`/`pace_cfg` above. `cfg` is the ONE whole-config
    // parameter here, and only because `mail::store_to` takes a `&CtxConfig`
    // of its own -- no decision in this function reads anything off it but
    // `mail.enabled`. `stalled` mirrors `rotted` above: set the instant the
    // grace period elapses with no observed progress, so the caller's own
    // `reason`/chain-recording logic can tell this restart apart from an
    // ordinary rot/timeout one.
    repo: &Path,
    cfg: &CtxConfig,
    idle_no_tool: Duration,
    in_tool: Duration,
    stall_grace: Duration,
    stalled: &mut bool,
    cancellation: Option<&super::provider::adapter::CancellationFlag>,
) -> CtxResult<Outcome> {
    // Issue #203: `evaluate_worker_budget` reads the transcript fresh on
    // every tick, so it can see a `HardStop` the instant the child's last
    // chunk lands on disk -- often milliseconds before the child itself
    // calls `exit()`. `supervise_child` checks `try_wait` *before* every
    // tick, including the very next one, but a `Tick::Stop` on this same
    // tick short-circuits straight to `terminate` and `Outcome::
    // StoppedByTick`, which carries no exit code -- so a child that was
    // already on its way out on its own has its real code discarded for
    // `EXIT_BUDGET_EXHAUSTED`. One tick of grace (`Tick::Continue` instead
    // of `Stop`, exactly once) gives that next `try_wait` a chance to
    // observe a natural exit first -- the same spirit as the `limit_hit`
    // path's own brief final drain/wait below, letting a child that is
    // already on its way out finish naturally instead of being overridden.
    // Only a child still alive on the SECOND consecutive `HardStop` tick is
    // actually killed for budget.
    let mut budget_grace_given = false;
    // Issue #310 (3a): the progress clock for this one boot -- fresh per
    // `supervise_run` call, the same "a restart mints a fresh child, so its
    // own clock starts over" reasoning `budget_grace_given`/`compact_budget`
    // already follow. `last_mail_activity` is the previous tick's own mail
    // reading, so a CHANGE (new mail arrived, or was consumed) is what
    // counts as the mail signal advancing, not merely mail existing.
    let mut stall_signals = super::stall::ProgressSignals::new(Instant::now());
    let mut stall_latch: Option<super::stall::StallLatch> = None;
    let mut last_mail_activity: Option<(usize, usize)> = None;
    let mut tick = || {
        if cancellation.is_some_and(super::provider::adapter::Cancellation::is_cancelled) {
            return Tick::Stop("cancelled");
        }
        if let Some(candidate) = self_heal_transcript(transcript, resolve_transcript) {
            *scorer = score::IncrementalScorer::new(candidate.clone());
            *transcript = candidate;
        }
        let lines = tap.try_lines();
        *account_pattern = account_pattern.or_else(|| pace::scan_for_account_exhausted(&lines));
        *capacity_pattern = capacity_pattern.or_else(|| pace::scan_for_capacity_error(&lines));
        if !lines.is_empty() {
            stall_signals.last_output = Some(Instant::now());
        }
        if pace::scan_for_limit(&lines, state, session, "exec", &mut std::io::stderr()) {
            let now = now_secs();
            // Left on the static `provider()`: `supervise_run` has no pinned-
            // model parameter of its own, and this deep, already-huge
            // argument list (`#[allow(clippy::too_many_arguments)]`) is not
            // the place to add one for this foundation track -- the caller
            // (`run_with_clock_inner`) already resolves `execution_model` for
            // every OTHER pacing call in this file.
            match pace::confirm_limit_hit(state, pace_cfg, now, adapter.provider()) {
                pace::LimitConfirmation::Confirmed { detail } => {
                    *limit_hit = true;
                    *limit_confirmation_detail = Some(detail);
                    return Tick::Stop("limit");
                }
                pace::LimitConfirmation::Unconfirmed { detail } => {
                    pace::note_unconfirmed_limit_text(
                        state,
                        now,
                        session,
                        "exec",
                        &detail,
                        &mut std::io::stderr(),
                    );
                }
            }
        }
        if let Some(server) = server
            && let Some(received) = server.try_recv()
        {
            // C3: a turn boundary reported by *this* session is evidence it
            // got somewhere since the last nudge relaunch, which is what
            // makes the nudge budget consecutive rather than cumulative.
            // Recorded for any verdict, including the Restart one handled
            // just below: the session still did a turn's work.
            if received.session_id == session {
                *progressed = true;
                compact_budget.observe_progress();
                // Issue #310 (3a): a turn boundary is exactly the progress
                // clock's own turn-signal channel.
                stall_signals.last_turn = Some(Instant::now());
                // Issue #281: this session's own turn just reached a clean
                // boundary -- see this parameter's own doc comment.
                session_guard.clear_in_flight();
            }
            match action_for_signal(adapter, &received, session) {
                SignalAction::Stop => {
                    *rotted = true;
                    return Tick::Stop("rot");
                }
                SignalAction::Compact if compact_budget.ready(Instant::now(), compact_window) => {
                    compact_budget.arm(Instant::now());
                    *compact_requested = true;
                    return Tick::Stop("compact");
                }
                SignalAction::Compact | SignalAction::Ignore => {}
            }
        }
        // N4: claiming the marker is atomic (`remove_file`), so exactly one
        // observer ever sees `true` -- important even within one process,
        // since a stale marker from a previous cycle must never re-fire.
        // Gracefully stops the child (same `Tick::Stop` shape rot uses) only
        // when a relaunch is actually possible and the consecutive-nudge cap
        // has not been reached; otherwise the marker is still claimed (so it
        // never re-triggers) but the child runs on untouched and the mail
        // stays unread -- `nudge-ignored` in the decision log says why.
        if let Some(from) = super::sessions::claim_nudge_marker(state, registry_short) {
            if can_restart && nudges_used < max_nudges {
                // C4: the sender's own short id, read out of the marker, so
                // the announcement can name who actually nudged us.
                *nudged_by = Some(from);
                return Tick::Stop("nudge");
            }
            let _ = log::append(
                state,
                &log::Decision {
                    ts: now_secs(),
                    session,
                    verb: "exec",
                    verdict: "n/a",
                    score: 0,
                    action: "nudge-ignored",
                    detail: if can_restart {
                        "consecutive nudge cap reached; message left unread"
                    } else {
                        "no prompt available for a nudge relaunch; message left unread"
                    },
                    observed_at: None,
                },
            );
            return Tick::Continue;
        }
        // Issue #310 (3a): mail activity is the progress clock's third
        // channel -- a CHANGE in the unread counts (new mail arrived, or was
        // just consumed by the nudge check above) counts as progress, not
        // merely mail existing.
        let mail_activity = super::mail::unread_counts(
            state,
            repo,
            adapter.name(),
            registry_short,
            cfg.mail.enabled,
        );
        if mail_activity != last_mail_activity {
            stall_signals.last_mail = Some(Instant::now());
            last_mail_activity = mail_activity;
        }
        // T4 (C-1): transcript growth is the progress clock's turn channel
        // ("Stop hook / transcript-growth signal"), and this poll is already
        // reading exactly the bytes that prove it -- `IncrementalScorer::
        // poll` answers `Some(report)` only for a poll that genuinely
        // consumed new bytes (or saw the transcript restart). Without this,
        // the only channels were stdout, a turn signal and a mail-count
        // change: a healthy `claude -p` worker prints nothing until the very
        // end and posts its Stop hook only then, so 20 minutes of real tool
        // work tripped `in_tool_secs` and was terminated after the grace.
        // Polled HERE, ahead of `stall::decide`, so this tick's own growth
        // counts toward this tick's verdict; the result is consumed by the
        // screening/verdict arms below exactly as before, and a scoring
        // failure still must never kill a healthy run.
        let poll_result = scorer.poll(adapter, score_cfg, screen_thresholds);
        if let Ok((_, Some(_))) = &poll_result {
            stall_signals.last_turn = Some(Instant::now());
        }
        match super::stall::decide(
            stall_latch,
            &stall_signals,
            // exec.rs has no live tool-call boundary tracking yet
            // (`IncrementalScorer` does not expose its parsed events), so
            // this always applies the LONGER `in_tool` threshold -- the
            // conservative direction: it only ever grows the fuse for a
            // session that might genuinely be idle-thinking, never shortens
            // it below what a legitimate long-running tool call could need.
            super::stall::ToolState::InTool,
            Instant::now(),
            idle_no_tool,
            in_tool,
            stall_grace,
        ) {
            super::stall::StallAction::Continue => {}
            super::stall::StallAction::ClearLatch => {
                stall_latch = None;
                super::sessions::clear_stall_marker(state, registry_short);
                // Issue #349: progress resumed, so the stalled attention
                // this same authority raised no longer applies. `Supervisor`
                // authority, matching the observation that raised it -- a
                // higher authority's own attention (an `AdapterHook`
                // permission prompt, say) is untouched by this, since the
                // two axes are resolved independently.
                let _ = super::attention::record(
                    state,
                    registry_short,
                    super::attention::Observation::new(
                        super::attention::Authority::Supervisor,
                        "progress resumed",
                        90,
                        now_secs(),
                    )
                    .with_attention(super::attention::Attention::None),
                    now_secs(),
                );
            }
            super::stall::StallAction::LatchAndNudge => {
                let baseline = stall_signals.baseline();
                stall_latch = Some(super::stall::StallLatch {
                    latched_at: Instant::now(),
                    baseline_at_latch: baseline,
                });
                super::sessions::write_stall_marker(state, registry_short, now_secs());
                let idle_secs = Instant::now().saturating_duration_since(baseline).as_secs();
                let _ = super::attention::record(
                    state,
                    registry_short,
                    super::attention::Observation::new(
                        super::attention::Authority::Supervisor,
                        format!("no progress observed for {idle_secs}s"),
                        90,
                        now_secs(),
                    )
                    .with_attention(super::attention::Attention::Stalled),
                    now_secs(),
                );
                // Issue #310 / T4 (C-2): the latch used to write a marker,
                // announce "sending a steering nudge" and log `stall-nudge`
                // while delivering NOTHING to the child -- the announcement
                // and the log line described a nudge that did not exist.
                // Deliver it over the one channel a headless child actually
                // consumes: a directed message in this run's own mailbox,
                // addressed to the stable registry short so the next nudge
                // relaunch (or `zirv ctx inbox`) picks it up. `nudged`
                // carries the truth of that delivery into the banner rather
                // than letting it claim something that did not happen.
                let slug = super::state::repo_slug(repo);
                let nudge = super::mail::Message {
                    from_session: "supervisor".into(),
                    from_agent: "zirv".into(),
                    to: adapter.name().into(),
                    to_session: Some(registry_short.into()),
                    sent: now_secs(),
                    body: format!(
                        "No progress has been observed for {idle_secs}s. Checkpoint your work and report any blocker before continuing."
                    ),
                };
                let (nudged, detail) = if !cfg.mail.enabled {
                    (
                        false,
                        "no progress observed; mail is disabled, so no steering nudge could be \
                         delivered"
                            .to_string(),
                    )
                } else {
                    match super::mail::store_to(state, &slug, &slug, &nudge, cfg) {
                        Ok(_) => (
                            true,
                            "no progress observed; steering nudge queued for the session"
                                .to_string(),
                        ),
                        Err(err) => (
                            false,
                            format!(
                                "no progress observed; could not queue the steering nudge: {err}"
                            ),
                        ),
                    }
                };
                announcer.emit(&super::announce::Event::Stalled { idle_secs, nudged });
                // Our own nudge is not evidence that the child progressed.
                last_mail_activity = super::mail::unread_counts(
                    state,
                    repo,
                    adapter.name(),
                    registry_short,
                    cfg.mail.enabled,
                );
                let _ = log::append(
                    state,
                    &log::Decision {
                        ts: now_secs(),
                        session,
                        verb: "exec",
                        verdict: "n/a",
                        score: 0,
                        action: "stall-nudge",
                        detail: &detail,
                        observed_at: None,
                    },
                );
            }
            super::stall::StallAction::AwaitGrace => {}
            super::stall::StallAction::Terminate => {
                *stalled = true;
                super::sessions::clear_stall_marker(state, registry_short);
                let _ = log::append(
                    state,
                    &log::Decision {
                        ts: now_secs(),
                        session,
                        verb: "exec",
                        verdict: "n/a",
                        score: 0,
                        action: "stall-terminate",
                        detail: "grace period elapsed with no observed progress",
                        observed_at: None,
                    },
                );
                return Tick::Stop("stalled");
            }
        }
        // Issue #155, Phase 5(d): `evaluate_worker_budget` itself skips the
        // transcript read entirely when no ceiling is configured (every
        // delegation before 2.35.0, and the common case even after), so a
        // run that never asked to be bounded pays nothing extra here.
        match evaluate_worker_budget(
            adapter,
            budget,
            transcript.as_path(),
            prior_usage,
            prior_tool_calls,
        ) {
            Some(agent::BudgetState::HardStop { used, limit }) => {
                // Issue #203: give a child that is about to exit on its own
                // one poll's worth of room to do so, so `try_wait` -- not
                // this kill -- is what reports its real exit code.
                if !budget_grace_given {
                    budget_grace_given = true;
                    return Tick::Continue;
                }
                eprintln!(
                    "zirv ctx exec: token/tool-call budget exhausted ({used}/{limit}); \
                     stopping now -- this run will not restart"
                );
                *budget_exhausted = true;
                return Tick::Stop("budget");
            }
            Some(agent::BudgetState::SoftWarn { used, limit }) if !*soft_warned => {
                *soft_warned = true;
                eprintln!(
                    "zirv ctx exec: {used}/{limit} of the token/tool-call budget spent -- \
                     wrap up and checkpoint your result soon"
                );
            }
            Some(agent::BudgetState::SoftWarn { .. } | agent::BudgetState::Ok) | None => {}
        }
        // Issue #243 (review round, F3/F5): consumes the screening half of
        // every poll that actually read new bytes -- persisted and, when
        // it changed, announced -- through the same shared helper the Stop
        // hook uses (`sessions::record_screening`), so a codex/wrap-
        // supervised session (no Claude Stop hook at all) still gets a
        // live-detected injection marker or credential shape surfaced, not
        // only silently dropped. `Some(report)` only when bytes were
        // genuinely consumed this poll (`IncrementalScorer::poll`'s own
        // doc comment): an IDLE poll (`None`) must never reach
        // `record_screening` at all, or its fabricated-clean default would
        // clobber an already-persisted flagged summary and reset the
        // de-dup memory, making a real finding vanish across every idle
        // gap and then re-announce.
        if let Ok((_, Some(report))) = &poll_result {
            super::sessions::record_screening(
                state,
                registry_short,
                report,
                announcer,
                screening_announced,
            );
        }
        // Finding 4: drained every poll, before the limit short-circuit
        // below can return -- a poll whose transcript carried BOTH a rate
        // limit and a transport failure must still record the transport
        // one. Swallows its own I/O failures; nothing here can reach the
        // supervised child.
        score::observe_route_health(state, adapter, scorer, health_policy, session, "exec");
        if scorer.provider_limit_hit() {
            *limit_hit = true;
            return Tick::Stop("limit");
        }
        match poll_result {
            Ok((Some(score), _)) => match action_for_verdict(adapter, score.verdict) {
                SignalAction::Stop => {
                    *rotted = true;
                    Tick::Stop("rot")
                }
                SignalAction::Compact if compact_budget.ready(Instant::now(), compact_window) => {
                    compact_budget.arm(Instant::now());
                    *compact_requested = true;
                    Tick::Stop("compact")
                }
                SignalAction::Compact => Tick::Continue,
                SignalAction::Ignore => {
                    compact_budget.observe_progress();
                    Tick::Continue
                }
            },
            _ => Tick::Continue,
        }
    };
    let outcome = supervise::supervise_child(child, deadline, poll, &mut tick)?;

    // A fast API-error exit can land its provider event after the final live
    // tick, just like the tapped-output race closed by the caller's final
    // drain. Give the transcript one last incremental read before deciding
    // whether this was an ordinary exit.
    if !*limit_hit {
        let _ = scorer.poll(adapter, score_cfg, screen_thresholds);
        score::observe_route_health(state, adapter, scorer, health_policy, session, "exec");
        *limit_hit = scorer.provider_limit_hit();
    }

    // C1 (issue #155 review finding): `supervise_child` checks `try_wait`
    // for a completed child *before* ever calling the tick above, so a
    // child that writes an over-budget final transcript and then exits
    // between two polls can race past the very last tick that would have
    // caught it -- and report its own clean exit code instead of the
    // budget stop its transcript actually earned. Caught here as a final
    // check on the transcript the child left behind, gated on
    // `Exited(0)` specifically: a child that exited with its own failure
    // code keeps that code untouched, since overriding it with a budget
    // verdict here would erase a real failure that may have nothing to do
    // with the budget at all.
    if !*budget_exhausted
        && matches!(outcome, Outcome::Exited(0))
        && let Some(agent::BudgetState::HardStop { used, limit }) = evaluate_worker_budget(
            adapter,
            budget,
            transcript.as_path(),
            prior_usage,
            prior_tool_calls,
        )
    {
        eprintln!(
            "zirv ctx exec: token/tool-call budget exhausted ({used}/{limit}) in the child's \
             final transcript; stopping now -- this run will not restart"
        );
        *budget_exhausted = true;
    }

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;

    #[test]
    fn a_headless_exec_is_not_subject_to_the_nesting_guard() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tempfile::tempdir().expect("tempdir");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(home.path());

        let mut env = base_env(&tmp.path().join("state"));
        for (key, value) in [
            (
                adapters::SESSION_ENV,
                "abcdef12-3456-4789-8abc-def012345678",
            ),
            (adapters::SOCKET_ENV, "/tmp/outer.sock"),
            ("CLAUDE_PID", "4242"),
            ("CLAUDECODE", "1"),
        ] {
            env.insert(key.to_string(), value.to_string());
        }

        let command: Vec<String> = if cfg!(windows) {
            ["cmd", "/c", "exit", "0"]
        } else {
            ["sh", "-c", "exit 0", "--"]
        }
        .iter()
        .map(|s| (*s).to_string())
        .collect();

        let args = ExecArgs {
            agent: None,
            session_id: None,
            transcript: None,
            prompt: None,
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: true,
            reservation_id: None,
            command,
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned())
            .expect("a headless worker legitimately runs inside a session");
        assert_eq!(
            code,
            0,
            "exec ran the child rather than refusing: {}",
            String::from_utf8_lossy(&out)
        );
    }

    #[test]
    fn transcript_growth_keeps_a_silent_worker_alive() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = tmp.path().join("state");
        let transcript = tmp.path().join("transcript.jsonl");
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_POLL_MS".into(), "100".into());
        env.insert("ZIRV_CTX_SUPERVISE_IN_TOOL_SECS".into(), "1".into());
        env.insert("ZIRV_CTX_SUPERVISE_STALL_GRACE_SECS".into(), "1".into());
        let args = ExecArgs {
            agent: Some("claude".into()),
            session_id: Some("11111111-2222-4333-8444-555555555555".into()),
            transcript: Some(transcript.clone()),
            prompt: Some("do the work".into()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(15),
            simple: true,
            reservation_id: None,
            command: vec![
                "sh".into(), "-c".into(),
                "for i in 1 2 3 4 5 6 7 8 9 10 11 12; do printf '{}\\n' >> \"$1\"; /bin/sleep 0.25; done".into(),
                "worker".into(), transcript.display().to_string(),
            ],
            ..Default::default()
        };
        let code = run_with(&args, &mut Vec::new(), tmp.path(), &|k| env.get(k).cloned());
        assert_eq!(
            code.expect("runs"),
            0,
            "transcript growth must prevent a stall"
        );
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(
            !log.contains("stall-nudge"),
            "healthy work must never latch: {log}"
        );
    }

    #[test]
    fn a_stall_delivers_one_steering_mail() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = tmp.path().join("state");
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_POLL_MS".into(), "100".into());
        env.insert("ZIRV_CTX_SUPERVISE_IN_TOOL_SECS".into(), "1".into());
        env.insert("ZIRV_CTX_SUPERVISE_STALL_GRACE_SECS".into(), "1".into());
        let args = ExecArgs {
            agent: Some("claude".into()),
            session_id: Some("11111111-2222-4333-8444-555555555555".into()),
            transcript: Some(tmp.path().join("transcript.jsonl")),
            prompt: Some("do the work".into()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(15),
            simple: true,
            reservation_id: None,
            command: vec!["sh".into(), "-c".into(), "/bin/sleep 5".into()],
            ..Default::default()
        };
        let code =
            run_with(&args, &mut Vec::new(), tmp.path(), &|k| env.get(k).cloned()).expect("runs");
        assert_eq!(code, EXIT_STALLED);
        let state = StateDir::from_root(state);
        let unread = super::super::mail::list(
            &state,
            &super::super::state::repo_slug(tmp.path()),
            Some("claude"),
            Some("11111111"),
        )
        .expect("mail");
        assert_eq!(
            unread.len(),
            1,
            "a stall must deliver exactly one steering mail"
        );
        assert_eq!(unread[0].1.to_session.as_deref(), Some("11111111"));
    }

    #[test]
    fn live_capacity_text_survives_until_exit() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = tmp.path().join("state");
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_POLL_MS".into(), "100".into());

        let args = ExecArgs {
            agent: Some("claude".into()),
            session_id: Some("11111111-2222-4333-8444-555555555555".into()),
            transcript: Some(tmp.path().join("transcript.jsonl")),
            prompt: Some("do the work".into()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(15),
            simple: true,
            reservation_id: None,
            command: vec![
                "sh".into(),
                "-c".into(),
                "printf 'Selected model is at capacity\\n'; /bin/sleep 3; exit 2".into(),
            ],
            ..Default::default()
        };
        let code =
            run_with(&args, &mut Vec::new(), tmp.path(), &|k| env.get(k).cloned()).expect("runs");
        assert_eq!(
            code, EXIT_CAPACITY_EXHAUSTED,
            "live capacity text must survive the final drain"
        );
    }

    #[test]
    fn live_account_text_survives_until_exit() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = tmp.path().join("state");
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_POLL_MS".into(), "100".into());

        let args = ExecArgs {
            agent: Some("claude".into()),
            session_id: Some("11111111-2222-4333-8444-555555555555".into()),
            transcript: Some(tmp.path().join("transcript.jsonl")),
            prompt: Some("do the work".into()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(15),
            simple: true,
            reservation_id: None,
            command: vec![
                "sh".into(),
                "-c".into(),
                "printf 'insufficient_quota\\n'; /bin/sleep 3; exit 2".into(),
            ],
            ..Default::default()
        };
        let code =
            run_with(&args, &mut Vec::new(), tmp.path(), &|k| env.get(k).cloned()).expect("runs");
        assert_eq!(
            code, EXIT_ACCOUNT_EXHAUSTED,
            "live account text must survive the final drain"
        );
    }

    #[test]
    fn a_hanging_child_is_killed_at_the_deadline() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "55555555-2222-4333-8444-555555555555";
        let env = base_env(&state);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "hang");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(1),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session),
            ..Default::default()
        };
        let started = std::time::Instant::now();
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), EXIT_TIMEOUT);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "the deadline must not wait for the child"
        );
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"verdict\":\"timeout\""), "got {log}");
    }

    /// Review round 1 (R4): `exec` derives the transcript path ONCE, before
    /// the child is spawned. For `codex exec` the rollout does not exist yet
    /// at that moment (codex mints its own id and writes `session_meta` only
    /// after it starts), so `pinned_rollout` answers `None` and the whole run
    /// would poll a `rollout-<zirv id>.jsonl` that never appears -- budgets,
    /// rot and spend all blind. A derived path that is still missing must
    /// therefore follow the adapter's later, better answer.
    #[test]
    fn a_derived_transcript_missing_at_spawn_is_re_resolved_once_the_child_writes_one() {
        let tmp = crate::commands::ctx::testenv::repo();
        let written = tmp.path().join("rollout-real.jsonl");
        std::fs::write(&written, "{}\n").expect("write");
        let derived = tmp.path().join("rollout-guessed.jsonl");

        assert_eq!(
            self_heal_transcript(&derived, Some(&|| written.clone())),
            Some(written.clone()),
            "a derived path that never appeared must follow the adapter's later answer"
        );
    }

    /// The self-heal only ever moves off a path nothing is writing, and only
    /// onto a file that actually exists: a transcript being written stays
    /// pinned, and an answer that names another missing file is ignored
    /// rather than swapped in.
    #[test]
    fn the_transcript_self_heal_never_leaves_a_live_file_or_lands_on_a_missing_one() {
        let tmp = crate::commands::ctx::testenv::repo();
        let live = tmp.path().join("live.jsonl");
        std::fs::write(&live, "{}\n").expect("write");
        let missing = tmp.path().join("missing.jsonl");
        let other_missing = tmp.path().join("other-missing.jsonl");

        assert_eq!(
            self_heal_transcript(&live, Some(&|| other_missing.clone())),
            None,
            "a transcript that is genuinely being written is never redirected"
        );
        assert_eq!(
            self_heal_transcript(&missing, Some(&|| other_missing.clone())),
            None,
            "a candidate that does not exist either is no improvement"
        );
    }

    /// Review round 2 (S1), at the unit level: with no resolver at all --
    /// what an operator's `--transcript` passes -- nothing is ever swapped
    /// in, however tempting the adapter's own guess looks.
    #[test]
    fn the_transcript_self_heal_is_inert_without_a_resolver() {
        let tmp = crate::commands::ctx::testenv::repo();
        let operator = tmp.path().join("operator-mirror.jsonl");

        assert_eq!(self_heal_transcript(&operator, None), None);
    }

    /// Review round 2 (S1): the self-heal above must stay confined to paths
    /// zirv itself derived. `--transcript` exists precisely for "the agent
    /// writes somewhere the adapter cannot derive", so redirecting it onto
    /// the adapter's own guess -- which is exactly what an unconditional
    /// re-resolution does while the operator's file has not been written yet
    /// -- silently supervises the wrong file. Here the operator names a file
    /// that never appears while the child fills the derived one with 12
    /// over-budget turns: the run must ride out its deadline blind rather
    /// than report a budget stop earned by a transcript it was never told to
    /// watch.
    #[test]
    fn an_operators_explicit_transcript_is_never_redirected_by_re_resolution() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "99999999-3333-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "hang");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(tmp.path().join("operator-mirror.jsonl")),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: Some(10_000),
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(3),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(
            code.expect("runs"),
            EXIT_TIMEOUT,
            "an explicit --transcript must never be swapped for the adapter's derived guess"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_child_is_told_where_the_socket_is() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "66666666-2222-4333-8444-555555555555";
        let env = base_env(&state);
        let marker = tmp.path().join("socket-env.txt");

        // A child that records the socket env it inherited, then exits.
        let command = vec![
            "sh".to_string(),
            "-c".to_string(),
            format!(
                "printf '%s' \"$ZIRV_CTX_SOCKET\" > {}; exit 0",
                marker.display()
            ),
        ];

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(30),
            simple: false,
            reservation_id: None,
            command,
            ..Default::default()
        };
        let mut out = Vec::new();
        run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned()).expect("runs");

        let seen = std::fs::read_to_string(&marker).expect("marker written");
        assert!(seen.ends_with(".sock"), "socket path exported: {seen}");
        assert!(seen.contains("66666666"), "per-session socket: {seen}");
    }

    #[test]
    fn an_unbindable_socket_does_not_stop_the_run() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "77777777-2222-4333-8444-555555555555";
        // A state dir path long enough that the socket path exceeds the limit.
        let long_state = tmp.path().join("x".repeat(120));
        let mut env = base_env(&long_state);
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(30),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }
        assert_eq!(code.expect("runs"), 0, "polling still supervises the run");
    }
}
