//! Turn-signal decisions and verified in-place compaction.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalAction {
    Ignore,
    Compact,
    Stop,
}

pub(crate) fn action_for_verdict(
    adapter: &dyn adapters::AgentAdapter,
    verdict: Verdict,
) -> SignalAction {
    match verdict {
        Verdict::Compact if adapter.supports_headless_compact() => SignalAction::Compact,
        Verdict::Compact | Verdict::Restart => SignalAction::Stop,
        Verdict::Healthy | Verdict::Advise => SignalAction::Ignore,
    }
}

/// Reject stale turn signals before deciding any action; socket names alone
/// do not prove session ownership.
pub fn action_for_signal(
    adapter: &dyn adapters::AgentAdapter,
    signal: &TurnSignal,
    session: &str,
) -> SignalAction {
    if signal.session_id != session {
        return SignalAction::Ignore;
    }
    action_for_verdict(adapter, signal.verdict)
}

/// One bounded attempt per cooldown, followed by reported progress before
/// another attempt.
#[derive(Debug, Default)]
pub(crate) struct CompactBudget {
    attempted_at: Option<Instant>,
    progressed_after_attempt: bool,
}

impl CompactBudget {
    pub(crate) fn ready(&self, now: Instant, window: Duration) -> bool {
        let Some(attempted_at) = self.attempted_at else {
            return true;
        };
        self.progressed_after_attempt && now.saturating_duration_since(attempted_at) >= window
    }

    pub(crate) fn arm(&mut self, now: Instant) {
        self.attempted_at = Some(now);
        self.progressed_after_attempt = false;
    }

    pub(crate) fn observe_progress(&mut self) {
        if self.attempted_at.is_some() {
            self.progressed_after_attempt = true;
        }
    }
}

/// A final-drain provider limit outranks compaction; continuing would
/// spend the exhausted provider again.
pub(crate) fn should_attempt_compact(compact_requested: bool, limit_hit: bool) -> bool {
    compact_requested && !limit_hit
}

#[derive(Debug)]
struct CompactPlan<'a> {
    prompt: String,
    transcript: &'a Path,
}

fn compact_plan<'a>(
    adapter: &dyn adapters::AgentAdapter,
    transcript: Option<&'a Path>,
    focus: &str,
) -> Result<CompactPlan<'a>, String> {
    let command = adapter.compact_command().ok_or_else(|| {
        format!(
            "adapter '{}' does not support in-place compaction",
            adapter.name()
        )
    })?;
    let transcript = transcript
        .filter(|path| path.is_file())
        .ok_or_else(|| "no transcript reported, compaction unverifiable".to_string())?;
    Ok(CompactPlan {
        prompt: supervise::compact_prompt(command, focus),
        transcript,
    })
}

pub(super) fn protect_compaction_continuation(
    state: &StateDir,
    repo: &Path,
    cfg: &CtxConfig,
    prompt: &str,
) -> CtxResult<String> {
    let continuation = format!(
        "{prompt}\n\nContinue the same task after the verified in-place \
         compaction without redoing completed work."
    );
    Ok(super::obfuscate_store::protect_text(
        state,
        repo,
        cfg,
        &continuation,
        "exec_compaction_continuation",
    )?
    .0)
}

pub(crate) fn compact_in_place<F>(
    adapter: &dyn adapters::AgentAdapter,
    transcript: Option<&Path>,
    hard_timeout: Duration,
    poll: Duration,
    focus: &str,
    build: F,
) -> Result<(), String>
where
    F: FnOnce(&str) -> Option<(Command, Option<String>)>,
{
    let plan = compact_plan(adapter, transcript, focus)?;
    let mut watcher = supervise::Watcher::new(plan.transcript.to_path_buf());
    watcher
        .read_appended()
        .map_err(|error| format!("compaction verification failed: {error}"))?;

    let (command, stdin_prompt) = build(&plan.prompt).ok_or_else(|| {
        format!(
            "adapter '{}' cannot resume a headless session in place",
            adapter.name()
        )
    })?;
    let (mut child, tap, _child_guard) = supervise::spawn_tapped(command, stdin_prompt)
        .map_err(|error| format!("compact command failed to start: {error}"))?;
    let poll = poll.max(Duration::from_millis(10));
    // A headless compaction may emit no transcript bytes until completion,
    // so only one hard deadline may bound child exit and verification.
    // Early silence cannot prove a stall.
    let deadline = Instant::now() + hard_timeout;
    let outcome = supervise::supervise_child(&mut child, deadline, poll, &mut || Tick::Continue)
        .map_err(|error| format!("compact command failed: {error}"))?;
    let _ = tap.drain_to_eof(supervise::FINAL_DRAIN_BUDGET);
    match outcome {
        Outcome::Exited(0) => {}
        Outcome::Exited(code) => return Err(format!("compact command exited with code {code}")),
        Outcome::TimedOut => {
            return Err(format!(
                "compact command exceeded its {}s hard timeout",
                hard_timeout.as_secs()
            ));
        }
        Outcome::StoppedByTick(reason) => {
            return Err(format!("compact command stopped unexpectedly: {reason}"));
        }
    }

    let verified = supervise::verify_compaction(&mut watcher, adapter, deadline)
        .map_err(|error| format!("compaction verification failed: {error}"))?;
    if !verified {
        return Err("compaction not verified".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;

    fn signal_with(verdict: Verdict, score: u32) -> TurnSignal {
        TurnSignal {
            session_id: "s".to_string(),
            turn: 4,
            score,
            verdict,
            transcript_path: None,
        }
    }

    #[test]
    fn signal_actions_cover_restart_compact_and_ignore() {
        let claude = crate::commands::ctx::adapters::claude::ClaudeAdapter::new(None);
        assert_eq!(
            action_for_signal(&claude, &signal_with(Verdict::Restart, 95), "s"),
            SignalAction::Stop
        );
        assert_eq!(
            action_for_signal(&claude, &signal_with(Verdict::Compact, 65), "s"),
            SignalAction::Compact
        );
        assert_eq!(
            action_for_signal(&claude, &signal_with(Verdict::Advise, 45), "s"),
            SignalAction::Ignore
        );
        assert_eq!(
            action_for_signal(&claude, &signal_with(Verdict::Healthy, 0), "s"),
            SignalAction::Ignore
        );
    }

    /// Issue #303 gave codex a real `headless_resume_cmd` (`codex exec
    /// resume`), but deliberately left `supports_headless_compact` `false`:
    /// no verified in-place compaction directive exists to pair the resume
    /// with (see `CodexAdapter::compact_command`'s own doc comment). This
    /// pins that a codex `Verdict::Compact` still restarts, unchanged by
    /// that issue.
    #[test]
    fn codex_compact_verdict_restarts_without_arming_the_compact_budget() {
        let codex = crate::commands::ctx::adapters::codex::CodexAdapter::new(None);
        let now = Instant::now();
        let window = Duration::from_secs(60);
        let mut budget = CompactBudget::default();

        let action = action_for_verdict(&codex, Verdict::Compact);
        if action == SignalAction::Compact && budget.ready(now, window) {
            budget.arm(now);
        }

        assert_eq!(action, SignalAction::Stop);
        assert!(budget.attempted_at.is_none());
    }

    /// The socket path is derived from the first eight hex characters of a
    /// session id, so a stale hook or a neighbouring run can reach it. Killing
    /// a healthy child on someone else's verdict is the failure to avoid.
    #[test]
    fn a_verdict_about_another_session_is_ignored() {
        let claude = crate::commands::ctx::adapters::claude::ClaudeAdapter::new(None);
        assert_eq!(
            action_for_signal(
                &claude,
                &signal_with(Verdict::Restart, 95),
                "a-different-session",
            ),
            SignalAction::Ignore
        );
        assert_eq!(
            action_for_signal(
                &claude,
                &signal_with(Verdict::Compact, 65),
                "a-different-session",
            ),
            SignalAction::Ignore
        );
    }

    #[test]
    fn compact_gate_rejects_an_adapter_without_a_command_and_a_missing_transcript() {
        use std::cell::Cell;

        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(&transcript, "{}\n").expect("write transcript");
        let codex = crate::commands::ctx::adapters::codex::CodexAdapter::new(None);
        let attempts = Cell::new(0);
        assert_eq!(
            compact_in_place(
                &codex,
                Some(&transcript),
                Duration::ZERO,
                Duration::ZERO,
                supervise::COMPACT_FOCUS,
                |_| {
                    attempts.set(attempts.get() + 1);
                    None
                },
            )
            .expect_err("codex cannot compact"),
            "adapter 'codex' does not support in-place compaction"
        );
        assert_eq!(attempts.get(), 0, "unsupported adapter must not launch");

        let claude = crate::commands::ctx::adapters::claude::ClaudeAdapter::new(None);
        let attempts = Cell::new(0);
        assert_eq!(
            compact_in_place(
                &claude,
                None,
                Duration::ZERO,
                Duration::ZERO,
                supervise::COMPACT_FOCUS,
                |_| {
                    attempts.set(attempts.get() + 1);
                    None
                },
            )
            .expect_err("missing transcript must fail closed"),
            "no transcript reported, compaction unverifiable"
        );
        assert_eq!(attempts.get(), 0, "missing transcript must not launch");
    }

    /// Compaction stall-detection correction: `compact_in_place` used to reset
    /// a "stall" clock on every observed byte of transcript growth and kill
    /// the child once that clock ran out with NO growth at all. But a single
    /// headless compaction turn appends nothing to the transcript until the
    /// whole turn completes, so that clock could just as easily kill a real,
    /// healthy compaction as a genuine hang -- exactly the production
    /// incident this reproduces at unit-test scale (a real ~150k-token
    /// compaction can run well past what any short stall grace would
    /// tolerate while writing nothing back). The fake compact command here
    /// appends NOTHING to the transcript for several poll intervals, then
    /// finally emits `compact_boundary` and exits 0, well inside the hard
    /// timeout. It must not be killed: `compact_in_place` no longer uses
    /// transcript growth to decide liveness at all, relying on
    /// `supervise.compact_timeout_ms`'s hard bound alone. Durations are kept
    /// short (milliseconds) so the test never sleeps anywhere near the old
    /// 60s grace.
    #[test]
    fn a_compaction_with_no_transcript_growth_at_all_finishes_inside_the_hard_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(&transcript, "{}\n").expect("seed transcript");
        let claude = crate::commands::ctx::adapters::claude::ClaudeAdapter::new(None);

        // No writes to the transcript at all until the very end -- exactly
        // what a real headless compaction turn does while still computing.
        let script = format!(
            "sleep 0.3; printf '{{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"content\":\"c\"}}\\n' >> '{path}'",
            path = transcript.display()
        );
        let build = |_: &str| -> Option<(Command, Option<String>)> {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(&script);
            Some((cmd, None))
        };

        let result = compact_in_place(
            &claude,
            Some(&transcript),
            Duration::from_secs(5),
            Duration::from_millis(20),
            supervise::COMPACT_FOCUS,
            build,
        );
        assert!(
            result.is_ok(),
            "a compaction that appends nothing until it completes must not be killed for lack \
             of transcript growth, as long as it finishes inside the hard timeout: {result:?}"
        );
    }

    /// F4 (codex review, cff7ff57 follow-up): `compact_in_place` used to hand
    /// the post-exit verification step an entirely fresh `Instant::now() +
    /// hard_timeout` deadline, independent of how long the compact child
    /// itself had already taken to exit -- so a slow compaction could block
    /// this call for close to TWO full `hard_timeout` periods instead of
    /// one. The fake compact command sleeps most of the budget away, then
    /// exits 0 without ever appending a compaction marker, so verification
    /// can never find one and is guaranteed to spin out its own full
    /// window rather than returning early -- the one scenario that actually
    /// measures whether that window is bounded by the SAME deadline as the
    /// child's own exit wait, rather than a second one.
    #[test]
    fn compact_in_place_bounds_total_wait_to_one_hard_timeout_not_two() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(&transcript, "{}\n").expect("seed transcript");
        let claude = crate::commands::ctx::adapters::claude::ClaudeAdapter::new(None);

        let script = "sleep 0.7";
        let build = |_: &str| -> Option<(Command, Option<String>)> {
            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(script);
            Some((cmd, None))
        };

        let hard_timeout = Duration::from_millis(1000);
        let started = Instant::now();
        let result = compact_in_place(
            &claude,
            Some(&transcript),
            hard_timeout,
            Duration::from_millis(20),
            supervise::COMPACT_FOCUS,
            build,
        );
        let elapsed = started.elapsed();

        assert!(
            result.is_err(),
            "the child never writes a compaction marker, so this must report unverified, not \
             success: {result:?}"
        );
        assert!(
            elapsed < hard_timeout * 3 / 2,
            "one hard_timeout must cover the child's exit AND verification together, not two \
             separate full windows: elapsed {elapsed:?}, hard_timeout {hard_timeout:?}"
        );
    }

    #[test]
    fn compact_budget_arms_before_an_attempt_and_resets_only_after_progress_and_the_window() {
        let now = Instant::now();
        let window = Duration::from_secs(60);
        let mut budget = CompactBudget::default();
        assert!(budget.ready(now, window));

        budget.arm(now);
        assert!(!budget.ready(now, window));
        budget.observe_progress();
        assert!(!budget.ready(now + Duration::from_secs(59), window));
        assert!(budget.ready(now + window, window));
    }

    #[test]
    fn a_final_drain_limit_preempts_an_already_requested_compaction() {
        assert!(!should_attempt_compact(true, true));
        assert!(should_attempt_compact(true, false));
        assert!(!should_attempt_compact(false, false));
    }

    #[test]
    fn post_compaction_continuation_masks_the_original_task() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let state = StateDir::from_root(tmp.path().join("state"));
        let mut cfg = CtxConfig::default();
        cfg.obfuscate.mode = super::super::config::ObfuscateMode::Obfuscate;
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz123456";

        let continuation = protect_compaction_continuation(
            &state,
            tmp.path(),
            &cfg,
            &format!("finish the task with {secret}"),
        )
        .expect("mask continuation");

        assert!(!continuation.contains(secret), "{continuation}");
        assert!(
            continuation.contains("ZIRV_SECRET_GITHUB_TOKEN_1"),
            "{continuation}"
        );
        assert!(
            continuation.contains("Continue the same task after the verified in-place compaction"),
            "{continuation}"
        );
    }

    #[test]
    fn a_verified_compaction_resumes_and_continues_the_same_session() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "abababab-2222-4333-8444-555555555555";
        let modes = tmp.path().join("modes.txt");
        let argv_log = tmp.path().join("argv.log");
        std::fs::write(&modes, "compact-tier\nhealthy\n").expect("write modes");
        let mut env = base_env(&state);
        env.insert(
            "ZIRV_CTX_SUPERVISE_COMPACT_TIMEOUT_MS".to_string(),
            "2000".to_string(),
        );
        env.insert("ZIRV_CTX_INTERVAL_SECS".to_string(), "0".to_string());

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_SLEEP", Some("30")),
            ("FAKE_AGENT_ARGV_LOG", argv_log.to_str()),
        ]);
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|key| env.get(key).cloned());
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv log");
        assert!(
            argv.contains(&format!(
                "/compact {} --resume {session}",
                supervise::COMPACT_FOCUS
            )),
            "the compact command must resume the existing session: {argv}"
        );
        assert!(
            argv.contains(&format!(
                "Continue the same task after the verified in-place compaction without redoing \
                 completed work. --resume {session}"
            )),
            "the continuation must resume the same session: {argv}"
        );
        assert_eq!(
            argv.matches(&format!("--resume {session}")).count(),
            2,
            "exactly the compact and continuation launches resume: {argv}"
        );
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"verdict\":\"compact\"") && log.contains("\"action\":\"compact\""));
        assert!(!log.contains("\"action\":\"restart\""), "{log}");
        assert_eq!(
            transcripts_in(&home).len(),
            1,
            "same session, same transcript"
        );
    }

    #[test]
    fn a_verified_compaction_keeps_the_sessions_headless_effort() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "abababab-3333-4333-8444-555555555555";
        let modes = tmp.path().join("modes.txt");
        let effort_log = tmp.path().join("effort.log");
        std::fs::write(
            &modes,
            "compact-tier
healthy
",
        )
        .expect("write modes");
        let mut env = base_env(&state);
        env.insert(
            "ZIRV_CTX_SUPERVISE_COMPACT_TIMEOUT_MS".to_string(),
            "2000".to_string(),
        );
        env.insert("ZIRV_CTX_INTERVAL_SECS".to_string(), "0".to_string());
        env.insert(
            "ZIRV_CTX_HEADLESS_EFFORT_TRIVIAL".to_string(),
            "low".to_string(),
        );

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_SLEEP", Some("30")),
            ("FAKE_AGENT_EFFORT_ENV_LOG", effort_log.to_str()),
        ]);
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            timeout_secs: Some(60),
            command: fake_agent_command(session),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|key| env.get(key).cloned());
        assert_eq!(code.expect("runs"), 0);

        let efforts = std::fs::read_to_string(&effort_log).expect("effort log");
        assert_eq!(
            efforts.lines().collect::<Vec<_>>(),
            ["low", "low", "low"],
            "the first, compact and continuation launches share one effort"
        );
    }

    #[test]
    fn an_unverified_compaction_falls_through_to_restart_with_the_reason() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "acacacac-2222-4333-8444-555555555555";
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "compact-tier\nhealthy\n").expect("write modes");
        let mut env = base_env(&state);
        env.insert(
            "ZIRV_CTX_SUPERVISE_COMPACT_TIMEOUT_MS".to_string(),
            "300".to_string(),
        );
        env.insert("ZIRV_CTX_INTERVAL_SECS".to_string(), "0".to_string());

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_SLEEP", Some("30")),
            ("FAKE_AGENT_COMPACTION_EVENT", Some("0")),
        ]);
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(1),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|key| env.get(key).cloned());
        assert_eq!(code.expect("runs"), 0);

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains("\"action\":\"compact-failed\"")
                && log.contains("\"detail\":\"compaction not verified\"")
        );
        assert!(log.contains("\"action\":\"restart\""), "{log}");
        assert_eq!(
            transcripts_in(&home).len(),
            2,
            "restart mints a new session"
        );
    }
}
