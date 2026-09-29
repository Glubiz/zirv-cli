//! Live child supervision, transcript discovery, and stall handling.

use super::*;

/// Self-heal only a zirv-derived missing transcript path; an explicit
/// operator path must never be replaced by an adapter guess.
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
    // Use this run's resolved screen thresholds in live polling. (#272)
    screen_thresholds: &super::screen::Thresholds,
    // Feed route health from this supervisor's own scorer. (#455)
    health_policy: &super::health::HealthPolicy,
    state: &StateDir,
    server: Option<&signal::SignalServer>,
    session: &str,
    // Use the stable registry address for nudges across session remints.
    registry_short: &str,
    // Clear in-flight state only on this session's turn boundary. (#281)
    session_guard: &mut super::sessions::SessionGuard,
    // Keep screening deduplication across restarts, not per poll. (#243)
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
    // Budget reads full cumulative spend, beyond the scorer's rolling
    // window. (#155)
    transcript: &mut PathBuf,
    // Retry adapter discovery only for missing derived paths; explicit
    // transcript overrides are authoritative.
    resolve_transcript: Option<&dyn Fn() -> PathBuf>,
    budget: agent::WorkerBudget,
    // Include every prior child's harvested spend in this budget. (#169.2)
    prior_usage: &TranscriptUsage,
    prior_tool_calls: u32,
    soft_warned: &mut bool,
    budget_exhausted: &mut bool,
    // Keep progress and stall state for this boot; only mailbox configuration
    // is read from the whole config. (#310)
    repo: &Path,
    cfg: &CtxConfig,
    idle_no_tool: Duration,
    in_tool: Duration,
    stall_grace: Duration,
    stalled: &mut bool,
    cancellation: Option<&super::provider::adapter::CancellationFlag>,
) -> CtxResult<Outcome> {
    // Give an over-budget child one tick to exit naturally so its own exit
    // code is preserved; kill only on a second hard-stop tick. (#203)
    let mut budget_grace_given = false;
    // Restart gets a fresh progress clock; only changes in unread mail count
    // advance its mail channel. (#310)
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
            // This scorer uses the adapter's static provider; pinned model
            // bucketing is handled by the caller.
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
            // Any completed turn restores the consecutive-nudge budget.
            if received.session_id == session {
                *progressed = true;
                compact_budget.observe_progress();
                // This session's turn boundary advances progress and clears
                // its in-flight marker. (#281, #310)
                stall_signals.last_turn = Some(Instant::now());
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
        // Claim a nudge once, then stop only if relaunch is possible; otherwise
        // leave the child and unread mail untouched.
        if let Some(from) = super::sessions::claim_nudge_marker(state, registry_short) {
            if can_restart && nudges_used < max_nudges {
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
        // Mail counts advance progress only when they change. (#310)
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
        // Count transcript growth before the stall verdict; a quiet stdout
        // can still accompany active tool work. Scoring failure cannot kill
        // a healthy run.
        let poll_result = scorer.poll(adapter, score_cfg, screen_thresholds);
        if let Ok((_, Some(_))) = &poll_result {
            stall_signals.last_turn = Some(Instant::now());
        }
        match super::stall::decide(
            stall_latch,
            &stall_signals,
            // Without live tool boundaries, use the longer in-tool timeout
            // so legitimate work is not stopped early.
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
                // Clear only Supervisor attention on resumed progress; higher
                // authorities remain untouched. (#349)
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
                // Deliver a stall nudge into this run's directed mailbox;
                // do not announce delivery before it succeeds. (#310)
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
        // No configured ceiling means no budget transcript read. (#155)
        match evaluate_worker_budget(
            adapter,
            budget,
            transcript.as_path(),
            prior_usage,
            prior_tool_calls,
        ) {
            Some(agent::BudgetState::HardStop { used, limit }) => {
                // Preserve a natural exit code before forcing a budget stop. (#203)
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
        // Record screening only after new transcript bytes; an idle poll
        // must not erase or repeat a flagged summary. (#243)
        if let Ok((_, Some(report))) = &poll_result {
            super::sessions::record_screening(
                state,
                registry_short,
                report,
                announcer,
                screening_announced,
            );
        }
        // Drain route events before a limit short-circuit so simultaneous
        // transport failure is recorded too.
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

    // Read the final transcript before classifying a fast provider error.
    if !*limit_hit {
        let _ = scorer.poll(adapter, score_cfg, screen_thresholds);
        score::observe_route_health(state, adapter, scorer, health_policy, session, "exec");
        *limit_hit = scorer.provider_limit_hit();
    }

    // Check final spend after a clean exit, but preserve any nonzero child
    // failure code instead of overwriting it. (#155)
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
