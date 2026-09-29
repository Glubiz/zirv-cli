//! Supervisor exit outcomes, retry backoff, and nudge restart policy.

/// The restart budget is spent and the session is still rotting. Callers apply
/// their own policy from here.
pub const EXIT_ROT_EXHAUSTED: i32 = 75;

/// Wall-clock timeout with no restarts left.
pub const EXIT_TIMEOUT: i32 = 76;

/// Budget ceiling stops permanently without restart. (#155)
pub const EXIT_BUDGET_EXHAUSTED: i32 = 77;

/// Capacity retries exhausted; distinct from a rotting session. (#227)
pub const EXIT_CAPACITY_EXHAUSTED: i32 = 78;

/// Account exhaustion is not retryable; restarting cannot restore quota. (#227)
pub const EXIT_ACCOUNT_EXHAUSTED: i32 = 79;

/// Writer permit busy before launch; retry after it frees or use a separate
/// worktree. (#267)
pub const EXIT_WRITER_BUSY: i32 = 80;

/// Progress stayed stalled after one nudge and restart budget was spent;
/// keep this failure class separate from rot and timeout. (#310)
pub const EXIT_STALLED: i32 = 81;

/// The worker exited cleanly but its final report failed the result contract or named
/// deliverables that do not exist.
pub const EXIT_CONTRACT_FAILED: i32 = 82;

/// Supervisor-owned exit codes, shared with the README completeness check.
pub(crate) const EXIT_CODES: &[(i32, &str)] = &[
    (EXIT_ROT_EXHAUSTED, "EXIT_ROT_EXHAUSTED"),
    (EXIT_TIMEOUT, "EXIT_TIMEOUT"),
    (EXIT_BUDGET_EXHAUSTED, "EXIT_BUDGET_EXHAUSTED"),
    (EXIT_CAPACITY_EXHAUSTED, "EXIT_CAPACITY_EXHAUSTED"),
    (EXIT_ACCOUNT_EXHAUSTED, "EXIT_ACCOUNT_EXHAUSTED"),
    (EXIT_WRITER_BUSY, "EXIT_WRITER_BUSY"),
    (EXIT_STALLED, "EXIT_STALLED"),
    (EXIT_CONTRACT_FAILED, "EXIT_CONTRACT_FAILED"),
];

/// Distinguish supervisor-owned exit codes from a child's identical code.
pub fn describe_exit(code: i32) -> String {
    match code {
        EXIT_ROT_EXHAUSTED => "the session kept rotting and the restart budget ran out".to_string(),
        EXIT_TIMEOUT => "the supervised run hit its wall-clock timeout".to_string(),
        EXIT_BUDGET_EXHAUSTED => {
            "the token/tool-call budget was spent and the run was stopped".to_string()
        }
        EXIT_CAPACITY_EXHAUSTED => {
            "the provider kept reporting capacity/overload errors and the restart budget ran out"
                .to_string()
        }
        EXIT_ACCOUNT_EXHAUSTED => {
            "the provider account is out of usable credits/quota; restarting cannot fix a \
             billing problem"
                .to_string()
        }
        EXIT_WRITER_BUSY => {
            "another writing worker already holds this checkout; retry once it finishes, or \
             pass --worktree for an isolated one"
                .to_string()
        }
        EXIT_CONTRACT_FAILED => "the worker report failed its result contract".to_string(),
        EXIT_STALLED => {
            "no progress was observed after a steering nudge and the restart budget ran out"
                .to_string()
        }
        other => format!("exited with code {other}"),
    }
}

/// Pure capacity retry schedule, capped after the third attempt. (#227)
pub(super) fn capacity_backoff_secs(attempt: u32) -> u64 {
    match attempt {
        0 => 0,
        1 => 15,
        2 => 30,
        _ => 60,
    }
}

/// Reset consecutive-nudge budget when this session reports progress.
pub fn nudges_after(used: u32, progressed: bool) -> u32 {
    if progressed { 0 } else { used }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::super::*;
    use super::*;
    use crate::commands::ctx::window::{self, UsageWindows, Window};

    #[test]
    fn every_declared_exit_constant_is_in_exit_codes() {
        let mut declared = Vec::new();
        for line in include_str!("restart.rs").lines() {
            let Some(rest) = line.trim().strip_prefix("pub const EXIT_") else {
                continue;
            };
            let (suffix, value) = rest.split_once(": i32 = ").expect("exit constant shape");
            let code = value
                .trim_end_matches(';')
                .parse::<i32>()
                .expect("exit code");
            let name = format!("EXIT_{suffix}");
            assert!(
                EXIT_CODES.contains(&(code, name.as_str())),
                "{name} missing from EXIT_CODES"
            );
            declared.push(name);
        }
        assert_eq!(declared.len(), EXIT_CODES.len());
    }

    /// Guards `.config/nextest.toml`'s `exec-nudge-restart` group against
    /// silent membership rot: its `filter = 'test(a) or test(b) or ...'`
    /// enumerates 8 test names verbatim, and nextest silently matches
    /// nothing for a clause naming a test that does not exist rather than
    /// erroring -- so a rename here would silently drop a test out of the
    /// serialized group with no signal anywhere. This extracts every
    /// `test(NAME)` clause from that override's filter and asserts each
    /// NAME still resolves to a real `fn` in this file. `include_str!` on
    /// this very file is deliberate, not an accident -- the whole
    /// exec-nudge-restart family lives here -- and the check is on the
    /// exact `fn NAME(` byte pattern (not a loose substring match) so it
    /// cannot be fooled by a name that only ever appears as this test's own
    /// dynamically-parsed data, never as a real function definition. The
    /// reverse direction (every such-shaped `fn` also present in the
    /// filter) is not checked: there is no reliable lexical marker that
    /// distinguishes a member of this family from any other test.
    #[test]
    fn the_nextest_exec_nudge_restart_group_names_still_resolve() {
        const NEXTEST_TOML: &str =
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/.config/nextest.toml"));
        const THIS_FILE: &str = include_str!("restart.rs");

        let block = NEXTEST_TOML
            .split("[[profile.default.overrides]]")
            .find(|block| block.contains("test-group = 'exec-nudge-restart'"))
            .expect("nextest.toml must still have an override naming the exec-nudge-restart group");
        let filter_line = block
            .lines()
            .find(|line| line.trim_start().starts_with("filter = "))
            .expect("the exec-nudge-restart override must still have a filter line");

        let mut names: Vec<&str> = Vec::new();
        let mut rest = filter_line;
        while let Some(start) = rest.find("test(") {
            let after = &rest[start + "test(".len()..];
            let end = after
                .find(')')
                .expect("every test( clause in the filter must close with a )");
            names.push(&after[..end]);
            rest = &after[end + 1..];
        }

        assert!(
            names.len() >= 8,
            "expected at least the 8 known exec-nudge-restart tests, found {}: {:?}",
            names.len(),
            names
        );

        for name in names {
            let needle = format!("fn {name}(");
            assert!(
                THIS_FILE.contains(&needle),
                "nextest.toml's exec-nudge-restart filter names `{name}`, which no longer \
                 resolves to `fn {name}(` in exec.rs -- nextest silently drops a clause like \
                 this rather than erroring, so the test just as silently fell out of the \
                 serialized group"
            );
        }
    }

    /// F2: the nesting guard gates the *interactive* verbs only. Delegating
    /// to a headless worker from inside a session is the entire point of
    /// `zirv ctx agent`, and a worker never takes the shared console over,
    /// so `exec` must run normally with every piece of evidence the guard
    /// keys on present at once.
    ///
    /// A trivial shell command rather than the fake agent: this test is about
    /// what `run_with` refuses, not about supervision, and it should stay
    /// runnable on both platforms.
    /// C3: the cap counts *consecutive* nudge restarts, which is what
    /// `[supervise] max_nudges` has always claimed to bound. It was
    /// implemented cumulatively, so a long-lived session that did real work
    /// between nudges permanently exhausted its budget anyway.
    #[test]
    fn the_nudge_cap_resets_once_the_session_makes_progress() {
        // Without progress the budget is spent and stays spent.
        assert_eq!(nudges_after(0, false), 0);
        assert_eq!(nudges_after(2, false), 2);
        assert_eq!(nudges_after(3, false), 3);

        // A turn boundary from the session ends the consecutive run.
        assert_eq!(
            nudges_after(3, true),
            0,
            "a session that got somewhere may be nudged again"
        );
        assert_eq!(nudges_after(1, true), 0);
    }

    fn store_collector(state_dir: &std::path::Path, percent: f64, resets_in: u64) {
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.to_path_buf());
        let now = crate::commands::ctx::state::now_secs();
        window::store(
            &state,
            &UsageWindows {
                five_hour: Some(Window {
                    used_percentage: percent,
                    resets_at: now + resets_in,
                    observed_at: now,
                    overage_covered: false,
                    limit_reached: false,
                }),
                seven_day: None,
            },
        )
        .expect("store collector state");
    }

    #[test]
    fn unconfirmed_vendor_exhaustion_text_is_ignored() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state_dir);
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            format!("sh {}", fixture("fake-codex-agent.sh").display()),
        );
        store_provider_collector(&state_dir, window::CODEX_USAGE_PROVIDER, 2.0, false);

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "limit\n").expect("write modes");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_ARGV_LOG", argv_log.to_str()),
        ]);
        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: None,
            transcript: None,
            prompt: Some("finish the requested work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(30),
            simple: false,
            reservation_id: None,
            command: Vec::new(),
            ..Default::default()
        };
        let mut out = Vec::new();

        assert_eq!(
            run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned()).expect("runs"),
            1,
            "an unconfirmed text match preserves the child's own exit code"
        );
        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains("\"action\":\"limit-text-unconfirmed\""),
            "{log}"
        );
        assert!(!log.contains("\"action\":\"harness-handover\""), "{log}");
        assert!(!log.contains("\"action\":\"limit-park\""), "{log}");
        assert!(!log.contains("\"action\":\"give-up\""), "{log}");
        assert!(!log.contains("\"action\":\"kill\""), "{log}");
        let argv = std::fs::read_to_string(&argv_log).expect("argv log");
        assert_eq!(
            argv.matches("finish the requested work").count(),
            1,
            "the false positive must not relaunch"
        );
    }

    #[test]
    fn a_limit_hit_hands_over_to_an_enabled_alternate_before_parking() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state_dir);
        // A confirmed vendor limit is stronger evidence than proactive pacing,
        // so fallback remains useful even when the operator disabled the pace
        // gate itself.
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            format!("sh {}", fixture("fake-codex-agent.sh").display()),
        );

        // The first (codex) child reports a hard limit. The same fixture then
        // stands in for the selected claude continuation and exits cleanly.
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "limit\nhealthy\n").expect("write modes");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        store_provider_collector(&state_dir, window::CODEX_USAGE_PROVIDER, 2.0, true);
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_ARGV_LOG", argv_log.to_str()),
        ]);

        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: None,
            transcript: None,
            prompt: Some("finish the requested work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(30),
            simple: false,
            reservation_id: None,
            command: Vec::new(),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        assert_eq!(code.expect("runs"), 0);

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains("\"action\":\"harness-handover\""),
            "the confirmed limit should cross harnesses: {log}"
        );
        assert!(
            log.contains("codex -> claude"),
            "the selected route should be transparent: {log}"
        );
        assert!(
            !log.contains("\"action\":\"limit-park\""),
            "an admissible alternate should be used before the legacy park path: {log}"
        );
        let argv = std::fs::read_to_string(&argv_log).unwrap_or_default();
        assert!(
            argv.contains("finish the requested work"),
            "the logical task must survive the handoff: {argv}"
        );
    }

    /// Finding #7 (issue #358 review): a harness-handover restart re-enters
    /// `run_with_clock_inner` recursively -- a brand new call frame, with its
    /// own fresh `initial_launch` local (T9). Without threading `initial_
    /// launch_allowed` through that recursive call, this second launch would
    /// be (wrongly) treated as the WHOLE delegation's first launch and skip
    /// pacing entirely, even into a provider already inside pacing's soft
    /// throttle band. Same fixture as `a_limit_hit_hands_over_to_an_enabled_
    /// alternate_before_parking` (codex reports a confirmed limit, reroutes
    /// to claude), but this time claude's own usage is ALSO inside the soft
    /// band (85%, between `soft_percent` 80% and the hard `max_percent` 99%
    /// -- high enough to trigger a real `Slow` pace decision, but not so
    /// high it reads as hard-refused and gets excluded as a reroute target
    /// itself), and pacing stays enabled (not disabled like that other test)
    /// so the claude leg's own pre-launch gate is exercised for real.
    #[test]
    fn a_provider_switch_restart_still_paces_into_a_throttled_provider() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE_JITTER_SECS".to_string(), "0".to_string());
        env.insert("ZIRV_CTX_PACE_MAX_WAIT_SECS".to_string(), "2".to_string());
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            format!("sh {}", fixture("fake-codex-agent.sh").display()),
        );

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "limit\nhealthy\n").expect("write modes");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        // codex: a confirmed hard limit, so the reroute to claude fires.
        store_provider_collector(&state_dir, window::CODEX_USAGE_PROVIDER, 2.0, true);
        // claude: inside the soft throttle band -- the provider this
        // delegation is about to switch ONTO -- but not hard-refused, so it
        // still qualifies as an admissible reroute target.
        store_provider_collector(&state_dir, window::LEGACY_USAGE_PROVIDER, 85.0, false);
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_ARGV_LOG", argv_log.to_str()),
        ]);

        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: None,
            transcript: None,
            prompt: Some("finish the requested work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(30),
            simple: false,
            reservation_id: None,
            command: Vec::new(),
            ..Default::default()
        };
        let mut out = Vec::new();
        let slept: std::cell::RefCell<Vec<u64>> = std::cell::RefCell::new(Vec::new());
        let code = run_with_clock(
            &args,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            &crate::commands::ctx::state::now_secs,
            &|d: Duration| slept.borrow_mut().push(d.as_secs()),
        );
        assert_eq!(code.expect("runs"), 0);

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains("codex -> claude"),
            "sanity: the reroute must have happened: {log}"
        );
        assert!(
            !slept.borrow().is_empty(),
            "the provider-switch restart must actually pace into a throttled provider, not skip \
             the gate as if this were the delegation's own first launch: {log}"
        );
        assert!(
            log.contains("\"action\":\"pace-wait\""),
            "the claude leg's own pacing must be a real wait, not `pace-initial-launch-warn`: \
             {log}"
        );
    }

    #[test]
    fn a_low_percentage_reached_flag_waits_before_same_harness_relaunch() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state_dir);
        // Proactive pacing is deliberately off: an actual vendor refusal is
        // stronger than that preference and must still park. Disable fallback
        // to exercise the same-harness relaunch rather than a handover.
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_FALLBACK".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_PACE_JITTER_SECS".to_string(), "0".to_string());
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            format!("sh {}", fixture("fake-codex-agent.sh").display()),
        );
        store_provider_collector(&state_dir, window::CODEX_USAGE_PROVIDER, 2.0, true);

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "limit\nhealthy\n").expect("write modes");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_ARGV_LOG", argv_log.to_str()),
        ]);
        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: None,
            transcript: None,
            prompt: Some("finish the requested work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(30),
            simple: false,
            reservation_id: None,
            command: Vec::new(),
            ..Default::default()
        };
        let clock = std::cell::Cell::new(crate::commands::ctx::state::now_secs());
        let slept = std::cell::RefCell::new(Vec::new());
        let mut out = Vec::new();

        let code = run_with_clock(
            &args,
            &mut out,
            tmp.path(),
            &|key| env.get(key).cloned(),
            &|| clock.get(),
            &|duration| {
                let seconds = duration.as_secs();
                slept.borrow_mut().push(seconds);
                clock.set(clock.get().saturating_add(seconds));
            },
        );

        assert_eq!(code.expect("runs"), 0);
        assert!(
            slept.borrow().iter().sum::<u64>() > 0,
            "the confirmed refusal must delay the relaunch, got {:?}",
            slept.borrow()
        );
        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"limit-park\""), "{log}");
        assert!(
            std::fs::read_to_string(&modes)
                .expect("remaining modes")
                .trim()
                .is_empty(),
            "the healthy second launch must complete after the wait"
        );
    }

    #[test]
    fn a_limit_hit_parks_and_relaunches_without_spending_the_restart_budget() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "99999999-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        // A reset one second out plus no jitter keeps the park short; the point
        // is that it parks and relaunches, not how long it waits.
        env.insert("ZIRV_CTX_PACE_JITTER_SECS".to_string(), "0".to_string());
        env.insert("ZIRV_CTX_PACE_FALLBACK_SECS".to_string(), "1".to_string());
        env.insert("ZIRV_CTX_PACE_MAX_WAIT_SECS".to_string(), "2".to_string());
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        store_collector(&state, 100.0, 60);

        // First child hits the limit, second runs clean.
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "limit\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            // Zero budget: a limit hit must park even with no restarts allowed,
            // because a park is not a restart.
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
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
        }

        assert_eq!(
            code.expect("runs"),
            0,
            "the relaunched child finished cleanly, so exec exits with its code"
        );

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"limit-park\""), "got {log}");
        assert!(
            !log.contains("\"action\":\"give-up\""),
            "a park must not consume the restart budget: {log}"
        );
        assert_eq!(
            transcripts_in(&home).len(),
            2,
            "the relaunch is a new session with its own transcript"
        );
    }

    /// Wording that only loosely resembles a usage-limit notice leaves a
    /// breadcrumb in the decision log and changes nothing else: the run is not
    /// parked, and its exit code is still the child's own.
    #[test]
    fn a_loose_limit_wording_is_noted_without_parking_the_run() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "77777777-2222-4333-8444-555555555555";
        let env = base_env(&state);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "drift");
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
            timeout_secs: Some(60),
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

        assert_eq!(code.expect("runs"), 0, "a breadcrumb is not a park");
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains("\"action\":\"limit-wording-drift\""),
            "the drift must be recorded: {log}"
        );
        assert!(
            !log.contains("\"action\":\"limit-park\""),
            "and it must never park a healthy run: {log}"
        );
    }

    // -- Issue #227: provider capacity errors -------------------------------

    /// A worker that hits a transient provider capacity error (codex's
    /// `Selected model is at capacity`) is restarted within the existing
    /// restart budget, with a short backoff between attempts -- not parked
    /// (there is no usage-window reset to wait for) and not silently
    /// returned as a bare `exit 1`.
    #[test]
    fn a_capacity_error_restarts_within_budget_with_a_backoff() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "11111111-c000-4333-8444-555555555555";
        let env = base_env(&state);

        // First child hits a capacity error, second runs clean.
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "capacity\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
        }
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
        let slept: std::cell::RefCell<Vec<u64>> = std::cell::RefCell::new(Vec::new());
        let code = run_with_clock(
            &args,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            &crate::commands::ctx::state::now_secs,
            &|d: Duration| slept.borrow_mut().push(d.as_secs()),
        );
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
        }

        assert_eq!(
            code.expect("runs"),
            0,
            "the restarted child finished cleanly, so exec exits with its code"
        );
        // T8's blind-mode pacing delay also calls `sleep_fn` (with `0`, since
        // `base_env` zeros `ZIRV_CTX_PACE_BLIND_DELAY_SECS`) ahead of every
        // launch, so the capacity backoff is not necessarily the only entry
        // -- just the one call for its own real, nonzero duration.
        assert_eq!(
            slept.borrow().iter().filter(|&&secs| secs == 15).count(),
            1,
            "the first capacity retry backs off 15s exactly once via the injected sleep_fn, got {:?}",
            slept.borrow()
        );

        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains("\"verdict\":\"capacity\"") && log.contains("\"action\":\"restart\""),
            "a capacity retry is a restart, not a park or a bare failure: {log}"
        );
        assert!(
            !log.contains("\"action\":\"limit-park\""),
            "a capacity error has no usage window to park against: {log}"
        );
        assert_eq!(
            transcripts_in(&home).len(),
            2,
            "the retry is a new session with its own transcript"
        );
    }

    /// Once the restart budget is spent, a capacity error gives up with a
    /// dedicated exit code and a stderr line naming the pattern and attempt
    /// count -- never a bare `exit 1`.
    #[test]
    fn a_capacity_error_exhausts_the_restart_budget_with_a_structured_reason() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "22222222-c000-4333-8444-555555555555";
        let env = base_env(&state);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "capacity");
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
            timeout_secs: Some(60),
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

        assert_eq!(code.expect("runs"), EXIT_CAPACITY_EXHAUSTED);
        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            printed.contains("Selected model is at capacity"),
            "names the matched pattern: {printed}"
        );
        assert!(
            printed.contains("0 restarts"),
            "names the attempt count: {printed}"
        );
        assert!(
            printed.contains("uncommitted"),
            "warns that workspace changes are uncommitted: {printed}"
        );
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains("\"verdict\":\"capacity\"") && log.contains("\"action\":\"give-up\""),
            "got {log}"
        );
    }

    // -- Issue #227 (operator follow-up): account/billing exhaustion --------

    /// An account/billing exhaustion (e.g. `insufficient_quota`) is a hard,
    /// non-retryable condition: the worker gives up immediately, spending
    /// none of the restart budget, even though it is configured.
    #[test]
    fn an_account_exhaustion_gives_up_immediately_without_spending_the_restart_budget() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "33333333-c000-4333-8444-555555555555";
        let env = base_env(&state);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "account");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            // A generous budget: an account exhaustion must never touch it.
            max_restarts: Some(5),
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
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
        }

        assert_eq!(code.expect("runs"), EXIT_ACCOUNT_EXHAUSTED);
        let printed = String::from_utf8(out).expect("utf8");
        assert!(
            printed.contains("not retryable"),
            "must say this cannot be fixed by restarting: {printed}"
        );
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains("\"action\":\"account-exhausted\""),
            "got {log}"
        );
        assert!(
            !log.contains("\"action\":\"restart\""),
            "must never restart on an account exhaustion: {log}"
        );
        assert_eq!(
            transcripts_in(&home).len(),
            1,
            "no retry was attempted -- exactly the one child that failed"
        );
    }

    /// Issue #358 (T9): renamed from `an_exhausted_window_delays_the_first_
    /// spawn` -- usage headroom is a ranking signal now, never a reason to
    /// delay the very first spawn of a fresh session. The `WaitUntil`
    /// verdict this exhausted window used to produce is downgraded to a
    /// one-line warning instead.
    #[test]
    fn an_exhausted_window_no_longer_delays_the_first_spawn() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "aaaaaaaa-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE_JITTER_SECS".to_string(), "0".to_string());
        env.insert("ZIRV_CTX_PACE_MAX_WAIT_SECS".to_string(), "2".to_string());
        store_collector(&state, 100.0, 1);

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
            timeout_secs: Some(60),
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

        assert_eq!(code.expect("runs"), 0, "a warning is never an exit");
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains("\"action\":\"pace-initial-launch-warn\""),
            "got {log}"
        );
        assert!(!log.contains("\"action\":\"pace-wait\""), "got {log}");
    }

    /// Issue #358 (T9): the companion to the test above -- the initial
    /// launch never waits, but a session already running that hits a
    /// confirmed, structured-corroborated vendor refusal still parks before
    /// its restart exactly as before (`initial_launch: false` on that
    /// second, separate `PaceGate`). Fake `sleep_fn`, real clock: the
    /// initial launch must record zero sleeps, and the confirmed-limit park
    /// after the first child exits must record at least one.
    #[test]
    fn the_initial_launch_never_waits_but_a_confirmed_limit_restart_still_does() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "aeaeaeae-2222-4333-8444-555555555555";
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "limit\nhealthy\n").expect("write modes");
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_PACE_JITTER_SECS".to_string(), "0".to_string());
        env.insert("ZIRV_CTX_PACE_MAX_WAIT_SECS".to_string(), "2".to_string());
        store_collector(&state, 100.0, 1);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[(
            "FAKE_AGENT_MODE_FILE",
            modes.to_str(),
        )]);
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
        let slept: std::cell::RefCell<Vec<u64>> = std::cell::RefCell::new(Vec::new());
        let code = run_with_clock(
            &args,
            &mut out,
            tmp.path(),
            &|k| env.get(k).cloned(),
            &crate::commands::ctx::state::now_secs,
            &|d: Duration| slept.borrow_mut().push(d.as_secs()),
        );

        assert_eq!(code.expect("runs"), 0, "the restart completes healthily");
        assert!(
            !slept.borrow().is_empty(),
            "the confirmed-limit restart must still pace before relaunching"
        );
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains("\"action\":\"pace-initial-launch-warn\""),
            "the initial launch must be a warning, not a wait: {log}"
        );
        assert!(
            log.contains("\"action\":\"limit-park\""),
            "the confirmed limit must still park before the restart: {log}"
        );
        assert_eq!(
            transcripts_in(&home).len(),
            2,
            "a limit-hit park mints a fresh session, same as an ordinary restart"
        );
    }

    /// Final wave item 2: a nudge restart of an explicit-command codex run
    /// delivers the
    /// nudge's own guidance -- stored as ordinary session-addressed mail by
    /// `sessions::run_nudge_with` -- because the relaunch it triggers always
    /// rebuilds through `build_headless`, unconditionally zirv's own launch
    /// regardless of what the original `-- <command>` argv looked like. The
    /// task-prompt-text channel `task_prompt_with_mail_fallback` uses exists
    /// on that relaunch even though it never existed at the initial launch.
    #[test]
    fn a_nudge_on_an_explicit_command_codex_run_delivers_the_nudge_mail_on_the_relaunch() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());
        // The nudge restart rebuilds its launch through the adapter's own
        // `headless_cmd` (`build_headless`), not by re-running the original
        // explicit `-- sh <fixture> ...` argv verbatim -- so the fixture has
        // to also be reachable as this adapter's *configured* binary, the
        // same way `a_codex_worker_receives_mail_in_its_task_prompt_since_
        // it_cannot_be_injected` wires it, or the relaunch resolves to
        // whatever `codex` happens to mean on the machine running this test.
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            format!("sh {}", fixture("fake-codex-agent.sh").display()),
        );

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_ARGV_LOG", argv_log.to_str()),
        ]);

        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let modes_for_writer = modes.clone();
        let writer = std::thread::spawn(move || {
            // No session-env log for codex (it never receives `--session-id`
            // at all -- see the fixture's own doc comment), so liveness is
            // polled straight off the real session registry instead of a
            // log-line count, mirroring `nudge_live_session`'s own read.
            // 20s, not the old 5s: honest against this test's own 30s exec
            // timeout, and a give-up now panics instead of silently never
            // nudging -- see `wait_for_live_session_or_panic`'s own doc
            // comment.
            wait_for_live_session_or_panic(&state_for_writer, Duration::from_secs(20));
            // The registry becomes live before the child consumes its mode.
            // Do not interrupt it until the first `hang` has been consumed.
            wait_for_first_line_or_panic(&modes_for_writer, "healthy", Duration::from_secs(20));
            nudge_live_session(
                &state_for_writer,
                &repo_for_writer,
                "heads up: switch focus",
            );
        });

        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: None,
            transcript: None,
            prompt: None,
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(30),
            simple: false,
            reservation_id: None,
            command: vec![
                "sh".to_string(),
                fixture("fake-codex-agent.sh").display().to_string(),
                "exec".to_string(),
                "do the work".to_string(),
            ],
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        writer.join().expect("writer thread");
        assert_eq!(code.expect("runs"), 0);

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains("\"action\":\"nudge-restart\""),
            "the nudge still restarts the process: {log}"
        );

        let argv = std::fs::read_to_string(&argv_log).unwrap_or_default();
        assert!(
            argv.contains("heads up: switch focus"),
            "the relaunch rebuilds through build_headless -- zirv's own launch -- so the \
             nudge's guidance must reach its argv even though the original command was \
             explicit: {argv}"
        );

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        let unread = crate::commands::ctx::mail::list(&state, &slug, None, None).expect("list");
        assert!(
            unread.is_empty(),
            "mail actually delivered into the relaunch's task prompt must be consumed: \
             {unread:?}"
        );
    }

    /// Final wave item 2: an explicit `-- <command>` initial launch
    /// (`adapter_builds_launch == false`) never itself goes through
    /// `build_headless` -- but a relaunch (nudge, park, rot/timeout) always
    /// does, on a Windows npm `.cmd`/`.ps1`-resolved `agent_bin` regardless
    /// of what the original invocation's argv looked like. Before this fix,
    /// `prompt_via_stdin` was ANDed with `adapter_builds_launch` and
    /// therefore pinned `false` for this whole run, so the nudge relaunch --
    /// built through `build_headless`, carrying the nudge's own multi-line
    /// mail block (`\n\n---\n\n...`) -- put that composed task prompt text
    /// on the reparsed `cmd.exe /c <shim>` argv instead of stdin, and
    /// `guard_cmd_shim_reparse` aborted the entire run the moment that
    /// relaunch tried to spawn. A trivial "do the work" prompt with no mail
    /// pending would not reproduce this (no metacharacters to trip the
    /// guard on), which is why the nudge's own guidance -- always multi-line
    /// -- is what this test carries.
    #[cfg(windows)]
    #[test]
    fn a_nudge_relaunch_of_an_explicit_command_codex_run_survives_a_cmd_shim_agent_bin() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        // The nudge's own relaunch is built through `adapter.headless_cmd`
        // (`build_headless`), which resolves `agent_bin` -- a real `.cmd`
        // file on disk, so `resolve_program` genuinely routes it through
        // `cmd.exe /c` the way an npm install would. A bare in-memory path
        // is not enough to reproduce the shim shape. The initial explicit
        // command never touches `agent_bin` at all (`sh` invokes the
        // fixture directly), so only the relaunch exercises it.
        let shim_dir = tempfile::tempdir().expect("tempdir");
        let shim = shim_dir.path().join("codex.cmd");
        std::fs::write(&shim, "@echo off\r\n").expect("write shim");
        env.insert("ZIRV_CTX_AGENT_BIN".to_string(), shim.display().to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\n").expect("write modes");
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[(
            "FAKE_AGENT_MODE_FILE",
            modes.to_str(),
        )]);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);

        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let writer = std::thread::spawn(move || {
            // 20s, not the old 5s: honest against this test's own 30s exec
            // timeout, and a give-up now panics instead of silently never
            // nudging -- see `wait_for_live_session_or_panic`'s own doc
            // comment.
            wait_for_live_session_or_panic(&state_for_writer, Duration::from_secs(20));
            nudge_live_session(
                &state_for_writer,
                &repo_for_writer,
                "heads up: switch focus",
            );
        });

        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: None,
            transcript: None,
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(30),
            simple: false,
            reservation_id: None,
            command: vec![
                "sh".to_string(),
                fixture("fake-codex-agent.sh").display().to_string(),
                "exec".to_string(),
                "do the work".to_string(),
            ],
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        writer.join().expect("writer thread");

        assert_eq!(
            code.expect("the nudge relaunch must spawn, not be aborted by the argv guard"),
            0
        );

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"nudge-restart\""), "got {log}");
    }

    /// Medium 3: the opposite shape from the explicit-command test above --
    /// zirv builds this launch itself (`command: Vec::new()`, so `adapter_
    /// builds_launch` and therefore `mail_deliverable` are both true
    /// regardless of `--simple`), so the nudge's own guidance must reach the
    /// relaunch's task-prompt text. Before this fix the nudge arm's mail
    /// gate lacked the launch path's `|| !system_prompt_supported` escape,
    /// so `--simple` (which always makes `fresh` `None`) silently dropped
    /// the guidance here too, even though codex's real channel -- the task
    /// prompt text -- never depended on `fresh`/`composed` in the first
    /// place.
    #[test]
    fn a_nudge_on_a_simple_codex_run_still_delivers_its_own_guidance() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            format!("sh {}", fixture("fake-codex-agent.sh").display()),
        );

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_ARGV_LOG", argv_log.to_str()),
        ]);

        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let modes_for_writer = modes.clone();
        let writer = std::thread::spawn(move || {
            // 20s, not the old 5s: honest against this test's own 30s exec
            // timeout, and a give-up now panics instead of silently never
            // nudging -- see `wait_for_live_session_or_panic`'s own doc
            // comment.
            wait_for_live_session_or_panic(&state_for_writer, Duration::from_secs(20));
            // The registry becomes live before the child consumes its mode.
            // Do not interrupt it until the first `hang` has been consumed.
            wait_for_first_line_or_panic(&modes_for_writer, "healthy", Duration::from_secs(20));
            nudge_live_session(
                &state_for_writer,
                &repo_for_writer,
                "heads up: switch focus",
            );
        });

        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: None,
            transcript: None,
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(30),
            simple: true,
            reservation_id: None,
            command: Vec::new(),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        writer.join().expect("writer thread");
        assert_eq!(code.expect("runs"), 0);

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(
            log.contains("\"action\":\"nudge-restart\""),
            "the nudge still restarts the process: {log}"
        );

        let argv = std::fs::read_to_string(&argv_log).unwrap_or_default();
        assert!(
            argv.contains("heads up: switch focus"),
            "--simple must not drop the nudge's own guidance for an adapter whose channel does \
             not need composed: {argv}"
        );
    }

    /// Medium 4: `mail_entries` gets reassigned to the nudge's own fresh
    /// listing in the nudge arm, but `mail_messages` (the text-content list
    /// `task_prompt_with_mail_fallback` reuses verbatim in the park and
    /// rot-restart arms, since neither re-lists mail) used to stay pinned to
    /// whatever the *launch* computed. Sequence: launch mail is delivered
    /// and consumed normally on the first spawn; a nudge delivers a second,
    /// different message; the nudged relaunch then itself hits a usage
    /// limit and parks. Before this fix, the park's own relaunch re-
    /// appended the launch mail's text (already consumed, stale) instead of
    /// the nudge's; it must instead carry the nudge's guidance, and the
    /// launch mail's text must never reach argv a second time.
    ///
    /// KNOWN ISSUE (perf/test-suite-speed fix round 2, 2026-08-24): this
    /// test was originally suspected of a nextest-only failure; re-review
    /// corrected that -- the discriminator is process *warmth*, not the
    /// runner (a cold filtered serial `cargo test` run failed it ~60% of
    /// the time; nextest gives every test a cold process and failed it
    /// ~100%; a full serial run reaches it warm, after ~2300 others, and
    /// passed). One real mechanism behind that has been found and fixed:
    /// `OutputTap::try_lines` (`supervise.rs`) was a pure, instantaneous,
    /// non-blocking drain with no synchronization against `forward`'s
    /// reader threads reaching EOF, so a child that prints its limit line
    /// and exits immediately could still have that line in flight when
    /// `child.wait()` observed the exit -- defeating both the poll-loop
    /// `scan_for_limit` and the "final drain" right after `supervise_run`
    /// returns, whose own comment claimed (wrongly) to close this race.
    /// Fixed via `OutputTap::drain_to_eof`, a bounded blocking drain that
    /// waits only as long as it takes for the reader threads to disconnect;
    /// verified independently via a real `spawn_tapped` child reproducing
    /// exactly this shape (`drain_to_eof_catches_a_real_childs_last_line_
    /// even_though_it_already_exited`, `supervise.rs`), 20/20 passes
    /// including under genuine heavy host contention.
    ///
    /// This test also failed intermittently (~30% of filtered, cold,
    /// single-run attempts observed on the dev machine, 100% on CI run
    /// 32723969751) with a DIFFERENT signature than the race above, and the
    /// tap-vs-exit fix did not explain or claim to fix it. Root cause: the
    /// adapter probes capability support by spawning `exec --help`
    /// (`detect_ignore_flags`, `adapters/codex.rs`) before composing the
    /// nudge restart's distiller call. This test's `ZIRV_CTX_AGENT_BIN`
    /// override redirects that probe to `fake-codex-agent.sh` too, and the
    /// probe's argv has no `--sandbox read-only` pair, so the fixture's
    /// `is_distiller` check did not exempt it -- it popped a real line off
    /// `FAKE_AGENT_MODE_FILE`, shifting hang/limit/healthy by one and making
    /// the "limit" stage silently run as "healthy" instead (no
    /// prompt-injection log entry, no limit-park, launch mail re-appended).
    /// The initial suspicion that this was machine-specific (a CI runner
    /// with no `codex` on PATH would never trigger the probe) was wrong:
    /// the probe targets the `ZIRV_CTX_AGENT_BIN` override directly, not a
    /// PATH-resolved `codex`, so it fires on CI just as reliably -- which is
    /// what CI run 32723969751's deterministic failure confirmed. Fixed in
    /// `fake-codex-agent.sh`: a bare `--help` probe is now recognized the
    /// same way `is_distiller` special-cases `--sandbox read-only`, logged
    /// but never popping a mode.
    #[test]
    fn a_post_nudge_park_carries_the_nudges_own_mail_not_the_stale_launch_mail() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        // This regression specifically isolates the same-harness park/relaunch
        // mail path. #186 enables fallback by default, which would correctly
        // continue the already-stopped codex child on claude instead.
        env.insert("ZIRV_CTX_FALLBACK".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            format!("sh {}", fixture("fake-codex-agent.sh").display()),
        );

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        crate::commands::ctx::mail::store(
            &state,
            &slug,
            &crate::commands::ctx::mail::Message {
                from_session: "other-session".to_string(),
                from_agent: "claude".to_string(),
                to: "any".to_string(),
                to_session: None,
                sent: 1,
                body: "heads up: the webhook route moved".to_string(),
            },
            &CtxConfig::default(),
        )
        .expect("store launch mail");
        store_provider_collector(&state_dir, window::CODEX_USAGE_PROVIDER, 2.0, true);

        // hang (nudge target) -> limit (the nudged relaunch parks) -> healthy
        // (the park's own relaunch, the one under test).
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\nlimit\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_ARGV_LOG", argv_log.to_str()),
        ]);

        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let modes_for_writer = modes.clone();
        let writer = std::thread::spawn(move || {
            // 20s, not the old 5s: honest against this test's own 30s exec
            // timeout, and a give-up now panics instead of silently never
            // nudging -- see `wait_for_live_session_or_panic`'s own doc
            // comment.
            wait_for_live_session_or_panic(&state_for_writer, Duration::from_secs(20));
            // Registration precedes the fixture consuming its mode. Wait for
            // that observable transition so the nudge cannot steal `hang`.
            wait_for_first_line_or_panic(&modes_for_writer, "limit", Duration::from_secs(20));
            nudge_live_session(
                &state_for_writer,
                &repo_for_writer,
                "heads up: switch focus",
            );
        });

        let args = ExecArgs {
            agent: Some("codex".to_string()),
            session_id: None,
            transcript: None,
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(30),
            simple: false,
            reservation_id: None,
            command: Vec::new(),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        writer.join().expect("writer thread");
        assert_eq!(code.expect("runs"), 0);

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"nudge-restart\""), "got {log}");
        assert!(log.contains("\"action\":\"limit-park\""), "got {log}");

        let argv = std::fs::read_to_string(&argv_log).unwrap_or_default();
        assert_eq!(
            argv.matches("heads up: the webhook route moved").count(),
            1,
            "the launch mail was delivered and consumed once, on the first spawn -- it must \
             never be re-appended on the post-nudge park's own relaunch: {argv}"
        );
        assert!(
            argv.matches("heads up: switch focus").count() >= 2,
            "the nudge's own guidance reaches both its own relaunch and the park that followed \
             it: {argv}"
        );
    }

    /// Shared with `zirv ctx agent` (agent.rs) and script `agent:` steps
    /// (agent_command.rs), which both delegate to this supervisor and want
    /// the same wording for the same two outcomes: the supervisor's own exit
    /// codes read as outcomes, not agent failures.
    #[test]
    fn describe_exit_names_the_supervisors_own_outcomes() {
        assert!(describe_exit(EXIT_ROT_EXHAUSTED).contains("restart budget"));
        assert!(describe_exit(EXIT_TIMEOUT).contains("wall-clock timeout"));
        assert_eq!(describe_exit(1), "exited with code 1");
    }

    #[test]
    fn a_healthy_window_adds_no_delay() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "bbbbbbbb-2222-4333-8444-555555555555";
        let env = base_env(&state);
        store_collector(&state, 5.0, 3600);

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
            timeout_secs: Some(60),
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
        assert_eq!(code.expect("runs"), 0);
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).unwrap_or_default();
        assert!(!log.contains("pace-wait"), "nothing to wait for: {log}");
    }

    /// The registry-poll sibling of `wait_for_lines_or_panic`, for the
    /// codex-shaped tests that have no session-env log to poll and instead
    /// watch the session registry directly (see the identical comment on
    /// each of their own writer threads before this helper existed).
    fn wait_for_live_session_or_panic(state_dir: &std::path::Path, budget: Duration) {
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.to_path_buf());
        let deadline = std::time::Instant::now() + budget;
        while std::time::Instant::now() < deadline {
            if crate::commands::ctx::sessions::list(&state)
                .iter()
                .any(|(_, liveness)| *liveness == crate::commands::ctx::sessions::Liveness::Live)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "no live session appeared within {budget:?} -- the hang-mode agent likely never \
             started, or this machine is starved badly enough that it could not be observed in \
             time (check for CPU contention before assuming a real regression)"
        );
    }

    fn wait_for_first_line_or_panic(path: &std::path::Path, expected: &str, budget: Duration) {
        let deadline = std::time::Instant::now() + budget;
        while std::time::Instant::now() < deadline {
            if std::fs::read_to_string(path)
                .ok()
                .is_some_and(|text| text.lines().next() == Some(expected))
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "{} never advanced to mode {expected:?} within {budget:?}",
            path.display()
        );
    }

    #[test]
    fn a_headless_worker_stops_at_the_next_poll_and_relaunches_with_the_guidance() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let session_log = tmp.path().join("session.log");
        let session = "10101010-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let _agent =
            crate::commands::ctx::testenv::VarGuard::set(&[("ZIRV_CTX_AGENT", Some("claude"))]);
        // C10: a guard, not a bare set/remove pair. The cleanup below used
        // to sit *after* `writer.join().expect(...)`, so a panicking writer
        // thread (or any failing assertion) skipped it entirely and leaked
        // `FAKE_AGENT_*` into every later test in this process -- which then
        // failed against a tempdir that no longer existed.
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_ARGV_LOG", argv_log.to_str()),
            ("FAKE_AGENT_SESSION_ENV_LOG", session_log.to_str()),
        ]);

        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let session_log_for_writer = session_log.clone();
        let writer = std::thread::spawn(move || {
            // 20s, not the old 5s: honest against this test's own 30s exec
            // timeout, and a give-up now panics instead of silently never
            // nudging -- see `wait_for_lines_or_panic`'s own doc comment.
            wait_for_lines_or_panic(&session_log_for_writer, 1, Duration::from_secs(20));
            nudge_live_session(
                &state_for_writer,
                &repo_for_writer,
                "switch to the new failing test",
            );
        });

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
        writer.join().expect("writer thread");
        assert_eq!(
            code.expect("the second (healthy) launch finishes the run"),
            0
        );

        let sessions = wait_for_lines(&session_log, 2, Duration::from_millis(1));
        assert_eq!(
            sessions.len(),
            2,
            "exactly one relaunch: the nudge, then a clean exit"
        );
        assert_ne!(
            sessions[0], sessions[1],
            "the relaunch mints a fresh session id"
        );

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("switch to the new failing test"),
            "the nudge's guidance must reach the relaunch's composed prompt: {argv}"
        );

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"nudge-restart\""), "got {log}");
    }

    /// N4: a nudge-driven restart must never touch the rot restart budget --
    /// with `max_restarts: 0`, an ordinary rot or timeout restart would
    /// immediately "give up"; a nudge restart must succeed anyway, and the
    /// normal rot-restart machinery (`"action":"restart"`, a `"rot"` or
    /// `"timeout"` verdict) must never fire at all.
    #[test]
    fn a_nudge_restart_does_not_spend_the_rot_restart_budget() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let session_log = tmp.path().join("session.log");
        let session = "20202020-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        // C10: a guard, not a bare set/remove pair. The cleanup below used
        // to sit *after* `writer.join().expect(...)`, so a panicking writer
        // thread (or any failing assertion) skipped it entirely and leaked
        // `FAKE_AGENT_*` into every later test in this process -- which then
        // failed against a tempdir that no longer existed.
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_SESSION_ENV_LOG", session_log.to_str()),
        ]);

        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let session_log_for_writer = session_log.clone();
        let writer = std::thread::spawn(move || {
            // 20s, not the old 5s: honest against this test's own 30s exec
            // timeout, and a give-up now panics instead of silently never
            // nudging -- see `wait_for_lines_or_panic`'s own doc comment.
            wait_for_lines_or_panic(&session_log_for_writer, 1, Duration::from_secs(20));
            nudge_live_session(&state_for_writer, &repo_for_writer, "keep going");
        });

        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            // Zero rot-restart budget: proves the nudge restart below is not
            // drawing from it.
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
        writer.join().expect("writer thread");
        assert_eq!(
            code.expect("a nudge restart with zero rot budget must still succeed"),
            0
        );

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"action\":\"nudge-restart\""), "got {log}");
        assert!(
            !log.contains("\"action\":\"restart\""),
            "the ordinary rot-restart action must never fire: {log}"
        );
        assert!(
            !log.contains("\"action\":\"give-up\""),
            "zero budget only matters to rot/timeout, which never triggered: {log}"
        );
        assert!(
            !log.contains("\"verdict\":\"rot\"") && !log.contains("\"verdict\":\"timeout\""),
            "nothing here rotted or timed out: {log}"
        );
    }

    /// N4: a nudge restart carries a handoff forward exactly like a rot or
    /// timeout restart does -- distilled or structural, stored under the old
    /// session, and named in the decision log detail the same way
    /// `"{source} handoff at {path}"` already reads for the ordinary path.
    #[test]
    fn a_nudge_restart_carries_a_handoff_forward_like_every_other_restart() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let session_log = tmp.path().join("session.log");
        let session = "30303030-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        // C10: a guard, not a bare set/remove pair. The cleanup below used
        // to sit *after* `writer.join().expect(...)`, so a panicking writer
        // thread (or any failing assertion) skipped it entirely and leaked
        // `FAKE_AGENT_*` into every later test in this process -- which then
        // failed against a tempdir that no longer existed.
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_SESSION_ENV_LOG", session_log.to_str()),
        ]);

        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let session_log_for_writer = session_log.clone();
        let writer = std::thread::spawn(move || {
            // 20s, not the old 5s: honest against this test's own 30s exec
            // timeout, and a give-up now panics instead of silently never
            // nudging -- see `wait_for_lines_or_panic`'s own doc comment.
            wait_for_lines_or_panic(&session_log_for_writer, 1, Duration::from_secs(20));
            nudge_live_session(&state_for_writer, &repo_for_writer, "keep going");
        });

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
        writer.join().expect("writer thread");
        assert_eq!(code.expect("runs"), 0);

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        assert!(
            crate::commands::ctx::handoff::latest_for_repo(&state, tmp.path())
                .expect("handoff lookup")
                .is_some(),
            "a nudge restart must distill and store a handoff, like every other restart"
        );

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        let nudge_restart_line = log
            .lines()
            .find(|l| l.contains("\"action\":\"nudge-restart\""))
            .unwrap_or_else(|| panic!("no nudge-restart entry: {log}"));
        assert!(
            nudge_restart_line.contains("handoff at"),
            "names the handoff the same way an ordinary restart does: {nudge_restart_line}"
        );
    }

    /// N4: `cfg.supervise.max_nudges` caps consecutive nudge restarts. Past
    /// the cap the marker is still claimed (so it does not keep re-firing)
    /// but nothing is stopped or relaunched, and the nudge's own mail stays
    /// unread -- still visible via `zirv ctx inbox` -- rather than being
    /// silently dropped.
    #[test]
    fn consecutive_nudge_restarts_are_capped_and_the_message_is_left_unread() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let session_log = tmp.path().join("session.log");
        let session = "40404040-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());
        env.insert("ZIRV_CTX_MAX_NUDGES".to_string(), "1".to_string());

        // Three potential runs scripted; only two are ever expected to
        // start (the first nudge restarts once, the second is ignored, and
        // the second run's own hang has to end some other way -- the
        // `timeout_secs` below, with a zero rot budget, is what ends it).
        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\nhang\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        // C10: a guard, not a bare set/remove pair. The cleanup below used
        // to sit *after* `writer.join().expect(...)`, so a panicking writer
        // thread (or any failing assertion) skipped it entirely and leaked
        // `FAKE_AGENT_*` into every later test in this process -- which then
        // failed against a tempdir that no longer existed.
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_SESSION_ENV_LOG", session_log.to_str()),
        ]);

        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let session_log_for_writer = session_log.clone();
        let writer = std::thread::spawn(move || -> Vec<String> {
            // 2s: the first hang-mode agent only has to spawn and write its
            // one line, no termination involved, so this is comfortably
            // honest against the time actually available. A give-up here
            // panics instead of silently skipping its own nudge -- see
            // `wait_for_lines_or_panic`'s own doc comment.
            let first = wait_for_lines_or_panic(&session_log_for_writer, 1, Duration::from_secs(2));
            debug_assert!(!first.is_empty());
            nudge_live_session(&state_for_writer, &repo_for_writer, "first nudge, honored");

            // 10s, not 2s: unlike the first wait, this one sits behind a
            // real `terminate()` of the first hang-mode child plus the
            // handoff/compile/relaunch work that follows it. On Windows that
            // terminate used to be a synchronous `taskkill /T /F` spawn,
            // whose WMI round trip cost 1.4-1.9s on an ordinarily loaded dev
            // machine and 8-40s on a contended one -- which is what failed
            // this wait. `supervise::kill_tree` now walks the tree natively
            // (one process snapshot per level, well under a second even
            // under load), so 10s is an order-of-magnitude margin again,
            // matching the sibling `a_nudge_restart_carries_a_handoff_
            // forward_like_every_other_restart` test's own 20s wait.
            let second =
                wait_for_lines_or_panic(&session_log_for_writer, 2, Duration::from_secs(10));
            nudge_live_session(
                &state_for_writer,
                &repo_for_writer,
                "second nudge, should be ignored",
            );
            second
        });

        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            // Short enough that the second (ignored-nudge) hang ends the
            // run on its own once the cap has been proven, rather than
            // hanging the test forever.
            timeout_secs: Some(3),
            simple: false,
            reservation_id: None,
            command: fake_agent_command(session),
            ..Default::default()
        };
        let mut out = Vec::new();
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        let sessions = writer.join().expect("writer thread");
        assert_eq!(
            code.expect("runs"),
            EXIT_TIMEOUT,
            "the second hang is never nudged into relaunching again, so it eventually times out \
             with no rot budget left to restart on"
        );

        let all_sessions = wait_for_lines(&session_log, 2, Duration::from_millis(1));
        assert_eq!(
            all_sessions.len(),
            2,
            "exactly one relaunch (the first nudge); the second was ignored: {all_sessions:?}"
        );

        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert_eq!(
            log.lines()
                .filter(|l| l.contains("\"action\":\"nudge-restart\""))
                .count(),
            1,
            "only the first nudge restarts: {log}"
        );
        assert_eq!(
            log.lines()
                .filter(|l| l.contains("\"action\":\"nudge-ignored\""))
                .count(),
            1,
            "the second is claimed but ignored, not silently dropped: {log}"
        );

        // The second nudge's own mail must still be sitting there, unread.
        // It is addressed to this run's *registry* short id -- the address
        // `SessionGuard::refresh_session` deliberately leaves untouched
        // across a restart (C7) and the one `nudge_live_session` itself
        // resolves and sends to -- not to `short_id` of the second
        // session's own rotated id, which is a different value entirely.
        assert!(
            sessions.len() >= 2,
            "the second session started: {sessions:?}"
        );
        let registry_short = crate::commands::ctx::sessions::short_id(session);
        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let slug = crate::commands::ctx::state::repo_slug(tmp.path());
        let unread = crate::commands::ctx::mail::list(&state, &slug, None, Some(&registry_short))
            .expect("list");
        assert_eq!(
            unread.len(),
            1,
            "the ignored nudge's mail is left unread, still visible via `zirv ctx inbox`: {unread:?}"
        );
        assert_eq!(unread[0].1.body, "second nudge, should be ignored");
    }
}
