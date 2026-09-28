//! Execution segments, cumulative budgets, and objective progress.

use super::*;

/// One vendor-backed portion of a logical supervised execution. A cross-harness
/// fallback produces more than one segment; a normal run produces exactly one.
#[derive(Debug, Clone)]
pub struct ExecutionSegment {
    pub session: String,
    pub agent: String,
    pub model: Option<String>,
    pub usage: TranscriptUsage,
    pub wall_ms: u64,
}

/// Accounting returned to callers that need to attribute one logical
/// delegation across cross-harness continuations.
#[derive(Debug, Clone, Default)]
pub struct ExecutionReport {
    pub segments: Vec<ExecutionSegment>,
    /// Issue #358 review finding #4: a harness-handover restart (below)
    /// moves this run's own token reservation to the NEW provider's ledger
    /// mid-recursion, inside `run_with_clock_inner`'s own tail call --
    /// `ExecArgs::reservation_id`/its caller's `provider` local only ever
    /// name the FIRST provider a delegation reserved against. Set every
    /// time such a swap happens (the last one wins across however many
    /// further handovers follow), so a caller that settles once the whole
    /// chain returns reads the ledger the run actually finished on, never
    /// the one it started on.
    pub final_reservation: Option<(String, &'static str)>,
}

/// Reads `transcript` fresh and returns its own usage and tool-call count
/// (via `adapter.parse_events`), or `None` if it cannot be read yet -- a read
/// failure here must never be fatal, since it can just mean the child has not
/// flushed its first line. Shared by [`evaluate_worker_budget`] and
/// [`harvest_spend`] (issue #169.2), so the two can never drift on how one
/// transcript's own spend is computed.
pub(super) fn record_execution_segment(
    report: &mut ExecutionReport,
    adapter: &dyn adapters::AgentAdapter,
    session: &SessionId,
    transcript: &Path,
    prior_usage: &TranscriptUsage,
    model: Option<&str>,
    started: Instant,
) {
    let current = read_transcript_spend(adapter, transcript)
        .map(|(usage, _)| usage)
        .unwrap_or_default();
    report.segments.push(ExecutionSegment {
        session: session.as_str().to_string(),
        agent: adapter.name().to_string(),
        model: model.map(str::to_string),
        usage: add_usage(prior_usage, &current),
        wall_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    });
}

fn read_transcript_spend(
    adapter: &dyn adapters::AgentAdapter,
    transcript: &Path,
) -> Option<(TranscriptUsage, u32)> {
    let body = std::fs::read_to_string(transcript).ok()?;
    let usage = adapter.transcript_usage(&body).unwrap_or_default();
    let tool_calls = adapter
        .parse_events(&body)
        .iter()
        .filter(|event| matches!(event, NormalizedEvent::ToolCall { .. }))
        .count();
    Some((usage, u32::try_from(tool_calls).unwrap_or(u32::MAX)))
}

/// Field-wise saturating sum of two [`TranscriptUsage`]s -- how a restart's
/// outgoing child's spend is folded into the running total, and how that
/// total is folded into the current child's own reading before a budget
/// check.
fn add_usage(a: &TranscriptUsage, b: &TranscriptUsage) -> TranscriptUsage {
    TranscriptUsage {
        input_tokens: a.input_tokens.saturating_add(b.input_tokens),
        cache_creation_input_tokens: a
            .cache_creation_input_tokens
            .saturating_add(b.cache_creation_input_tokens),
        cache_read_input_tokens: a
            .cache_read_input_tokens
            .saturating_add(b.cache_read_input_tokens),
        output_tokens: a.output_tokens.saturating_add(b.output_tokens),
    }
}

/// Issue #169.2: folds `transcript`'s own usage and tool-call count into the
/// running `prior_usage`/`prior_tool_calls` accumulators. Called once, on the
/// OUTGOING transcript, at every restart/nudge/park site in `run_with` --
/// before a fresh session (and therefore a fresh transcript) is minted for
/// the next child. A transcript that cannot be read yet contributes nothing
/// rather than failing the restart it is called from (best-effort, matching
/// `evaluate_worker_budget`'s own tolerance).
pub(super) fn harvest_spend(
    adapter: &dyn adapters::AgentAdapter,
    transcript: &Path,
    prior_usage: &mut TranscriptUsage,
    prior_tool_calls: &mut u32,
) {
    if let Some((usage, tool_calls)) = read_transcript_spend(adapter, transcript) {
        *prior_usage = add_usage(prior_usage, &usage);
        *prior_tool_calls = prior_tool_calls.saturating_add(tool_calls);
    }
}

/// Issue #285: reloads this repository's durable objective (if any),
/// advances its status against `now`/`spent` (`objective::advance`),
/// persists the update, and renders the layer text to append beside the
/// handoff at a restart -- the one channel its own volatile counters (spend
/// changes every restart, not just every recompose) can reach. `None` for no
/// objective set, or one already `Closed`: a closed objective is never
/// reseeded, and reloading it here must not be the thing that reopens it.
///
/// Unlike `composed` (built once at launch and reused across a nudge/rot/
/// timeout/park restart -- see this module's own doc comment), this cannot
/// reuse a launch-time snapshot: the objective's status can flip mid-run.
pub(super) fn objective_layer_for_restart(
    state: &StateDir,
    repo: &Path,
    now: u64,
    spent: u64,
) -> Option<String> {
    let key = super::state::repo_slug(repo);
    let record = objective::load(state, &key).ok().flatten()?;
    if record.status == objective::Status::Closed {
        return None;
    }
    let record = objective::advance(record, now, spent);
    let _ = objective::store(state, &key, &record);
    Some(objective::layer_text(&record))
}

/// Reads `transcript` fresh and evaluates `budget` against it PLUS every
/// prior child's own already-harvested spend (`prior_usage`/`prior_tool_
/// calls`, issue #169.2) -- so the ceiling bounds the whole supervised run
/// across every restart, not just whichever child happens to be running
/// right now. `None` when neither ceiling is configured (the common case,
/// and every delegation before 2.35.0) or the CURRENT transcript cannot be
/// read yet -- a read failure here must never be fatal, since it can just
/// mean the child has not flushed its first line; the next tick that can
/// read it still sees the full cumulative total, prior spend included.
///
/// Shared by `supervise_run`'s own tick (checked on every poll while the
/// child is alive) and its post-exit check just below (issue #155 review
/// finding C1): factored out so the two call sites can never drift on how
/// "spent" is computed.
pub(super) fn evaluate_worker_budget(
    adapter: &dyn adapters::AgentAdapter,
    budget: agent::WorkerBudget,
    transcript: &Path,
    prior_usage: &TranscriptUsage,
    prior_tool_calls: u32,
) -> Option<agent::BudgetState> {
    if budget.tokens.is_none() && budget.tool_calls.is_none() {
        return None;
    }
    let (usage, tool_calls) = read_transcript_spend(adapter, transcript)?;
    let combined_usage = add_usage(prior_usage, &usage);
    let combined_tool_calls = prior_tool_calls.saturating_add(tool_calls);
    Some(agent::budget_state(
        &budget,
        &combined_usage,
        combined_tool_calls,
    ))
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    use crate::commands::ctx::window;

    /// The "restarts >= max_restarts" give-up exit used to return without
    /// ever calling `record_execution_segment`, silently dropping the
    /// harvested spend for the child that just rotted from `ExecutionReport`.
    /// `max_restarts: 0` means give-up fires on the very first rot, so
    /// exactly one child ran and exactly one segment must be recorded for it.
    #[test]
    fn an_exhausted_restart_budget_still_records_its_final_segment() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "33333333-3333-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "rot");
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
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
        let result = run_with_report(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
        }

        let (code, report) = result.expect("runs");
        assert_eq!(code, EXIT_ROT_EXHAUSTED);
        assert_eq!(
            report.segments.len(),
            1,
            "the rotted child's spend must still be recorded before giving up: {:?}",
            report.segments
        );
    }

    /// Same "no prompt to restart with" exit as
    /// `a_run_with_no_discoverable_prompt_refuses_to_restart`, but through
    /// `run_with_report`: exactly one child ever ran, so the accounting
    /// caller (`agent.rs`'s `append_execution_segments`) must see exactly one
    /// segment. Two identical `record_execution_segment` calls on this exit
    /// path used to double it, double-summing cost and writing the
    /// delegation log entry twice.
    #[test]
    fn the_no_prompt_exit_records_exactly_one_execution_segment() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let session = "44444444-3333-4333-8444-555555555555";
        let env = base_env(&tmp.path().join("state"));

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "rot");
            // Keep the child alive past the first scoring tick so rot is seen.
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
        }
        let mut command = fake_agent_command(session);
        command.retain(|a| a != "-p" && a != "do the work");
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: None,
            max_restarts: Some(2),
            budget_tokens: None,
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(60),
            simple: false,
            reservation_id: None,
            command,
            ..Default::default()
        };
        let mut out = Vec::new();
        let result = run_with_report(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
        }

        let (code, report) = result.expect("runs");
        assert_eq!(code, EXIT_ROT_EXHAUSTED);
        assert_eq!(
            report.segments.len(),
            1,
            "exactly one child ran, so exactly one segment must be recorded: {:?}",
            report.segments
        );
    }

    /// Issue #155, Phase 5(d), end to end: `hang` mode writes its whole
    /// transcript (12 turns, well over the tiny budget below) then never
    /// exits, so the very first budget check after spawn -- not the
    /// deadline, set generously long here -- is what actually stops it.
    /// Proves `EXIT_BUDGET_EXHAUSTED` is wired all the way from `ExecArgs`
    /// through `supervise_run`'s tick to the exit code `run_with` returns,
    /// and that it terminates outright rather than restarting.
    #[test]
    fn a_token_budget_stops_a_hanging_child_before_its_wall_clock_deadline() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "77777777-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        // Fast polling, so the first budget check lands well inside the test
        // timeout below rather than waiting out the 2s production default.
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

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
            // Far below what `hang` mode's own fixed 12-turn transcript
            // totals (24 assistant events x 20_000 cache-read tokens each).
            budget_tokens: Some(10_000),
            max_tool_calls: None,
            objective: None,
            timeout_secs: Some(30),
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

        assert_eq!(code.expect("runs"), EXIT_BUDGET_EXHAUSTED);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "the budget check must fire long before the 30s deadline"
        );
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"verdict\":\"budget\""), "got {log}");
    }

    /// C1 (issue #155 review finding): `supervise_child` checks `try_wait`
    /// for a completed child *before* ever running the tick that evaluates
    /// the budget, so a child that writes its whole transcript and exits
    /// promptly -- exactly what `healthy` mode does -- can race past every
    /// tick that would have caught it and report its own clean `0` instead
    /// of the budget stop its transcript actually earned. The `hang`-mode
    /// budget test above cannot exercise this path at all, since a hanging
    /// child never exits on its own; this one proves the post-exit check
    /// added to `supervise_run` (not the tick) is what catches it.
    #[test]
    fn a_clean_exit_with_an_over_budget_final_transcript_reports_budget_exhausted() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "88888888-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
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
            max_restarts: Some(2),
            // Far below `healthy` mode's fixed 12-turn transcript total (24
            // assistant events x 20_000 cache-read tokens each). The child
            // exits on its own well before `max_restarts` above could ever
            // matter -- this is the "clean, over-budget exit" case, not a
            // restart.
            budget_tokens: Some(10_000),
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

        assert_eq!(
            code.expect("runs"),
            EXIT_BUDGET_EXHAUSTED,
            "a clean exit must not hide an over-budget final transcript"
        );
        let log = std::fs::read_to_string(state.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"verdict\":\"budget\""), "got {log}");
    }

    /// The other half of C1: a clean exit whose final transcript is
    /// comfortably under budget must be returned untouched, not overridden
    /// just because a budget was configured at all.
    #[test]
    fn a_clean_under_budget_exit_keeps_its_own_code() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "99999999-2222-4333-8444-555555555555";
        let env = base_env(&state);

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE", "healthy");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(2),
            // Comfortably above `healthy` mode's fixed 480_000-token total.
            budget_tokens: Some(1_000_000),
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

        assert_eq!(code.expect("runs"), 0);
    }

    /// C1's asymmetry: a child that exited with its OWN failure code keeps
    /// that code even when its final transcript is over budget -- only a
    /// clean (`0`) exit is eligible to be overridden with
    /// `EXIT_BUDGET_EXHAUSTED`, so a budget verdict never erases a real
    /// failure it may have nothing to do with.
    #[test]
    fn a_failed_exit_with_an_over_budget_transcript_keeps_its_failure_code() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state = tmp.path().join("state");
        let session = "10101010-2222-4333-8444-555555555555";
        let mut env = base_env(&state);
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            // `fail` mode writes the same over-budget transcript `healthy`
            // does, then exits 3.
            std::env::set_var("FAKE_AGENT_MODE", "fail");
        }
        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(transcript_for(&home, tmp.path(), session)),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            budget_tokens: Some(10_000),
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

        assert_eq!(
            code.expect("runs"),
            3,
            "a real failure code must survive an over-budget final transcript"
        );
    }

    /// Finding #4 (issue #358 review): a mid-run harness-handover restart
    /// moves this delegation's token reservation to the NEW provider's
    /// ledger deep inside the recursive `run_with_clock_inner` call --
    /// `ExecArgs::reservation_id`'s own ORIGINAL provider is stale the
    /// moment that happens. `ExecutionReport::final_reservation` must
    /// surface the actual, final `(id, provider)` pair so a caller (like
    /// `agent::run_with`) that only settles once this whole chain returns
    /// hits the ledger the run actually finished on.
    #[test]
    fn a_harness_handover_moves_the_reservation_and_reports_the_final_ledger() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let argv_log = tmp.path().join("argv.log");
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert(
            "ZIRV_CTX_AGENT_BIN".to_string(),
            format!("sh {}", fixture("fake-codex-agent.sh").display()),
        );

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "limit\nhealthy\n").expect("write modes");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        store_provider_collector(&state_dir, window::CODEX_USAGE_PROVIDER, 2.0, true);
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_ARGV_LOG", argv_log.to_str()),
        ]);

        let state = crate::commands::ctx::state::StateDir::from_root(state_dir.clone());
        let seeded = crate::commands::ctx::reservation::reserve(
            &state,
            "openai",
            "seed-session",
            1_000,
            1_700_000_000,
        )
        .expect("seed reservation");

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
            reservation_id: Some(seeded.id.clone()),
            command: Vec::new(),
            ..Default::default()
        };
        let mut out = Vec::new();
        let (code, report) =
            run_with_report(&args, &mut out, tmp.path(), &|k| env.get(k).cloned()).expect("runs");
        assert_eq!(code, 0);

        let (final_id, final_provider) = report
            .final_reservation
            .expect("a harness handover must surface the moved reservation");
        assert_eq!(final_provider, "anthropic");
        assert_ne!(
            final_id, seeded.id,
            "the moved reservation must get a fresh id"
        );
        assert_eq!(
            crate::commands::ctx::reservation::outstanding(&state, "openai", 1_700_000_100),
            0,
            "the old provider's reservation must be released"
        );
        assert_eq!(
            crate::commands::ctx::reservation::entries(&state, "anthropic").len(),
            1,
            "the new provider's ledger must carry exactly the moved reservation"
        );
    }

    /// Issue #285, the core acceptance criterion: `exec` reloads the durable
    /// objective across a rot restart -- it is not part of the launch-time
    /// `composed` prompt this restart path reuses untouched (see this
    /// module's own doc comment), so it has to be carried by hand, beside the
    /// handoff. `rot` mode reports 170k cache-read tokens on its very first
    /// turn, well past the tiny budget set below, so by the time the restart
    /// fires the objective has already crossed its soft budget and the
    /// injected text has switched to the fixed wrap-up instruction.
    #[test]
    fn exec_carries_the_objective_across_a_rot_restart_and_swaps_in_the_wrap_up_text() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let state = StateDir::from_root(state_dir.clone());
        let argv_log = tmp.path().join("argv.log");
        let session = "dddddddd-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());

        objective::store(
            &state,
            &super::super::state::repo_slug(tmp.path()),
            &objective::Objective {
                schema_version: objective::SCHEMA_VERSION,
                objective: "carry me across the restart".to_string(),
                budget_tokens: Some(50_000),
                deadline_secs: None,
                spent_tokens: 0,
                started_at: now_secs(),
                status: objective::Status::Active,
                pending_note: None,
                evidence: Vec::new(),
            },
        )
        .expect("store objective");

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "rot\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        unsafe {
            std::env::set_var("FAKE_AGENT_MODE_FILE", &modes);
            std::env::set_var("FAKE_AGENT_SLEEP", "30");
            std::env::set_var("FAKE_AGENT_ARGV_LOG", &argv_log);
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
        let code = run_with(&args, &mut out, tmp.path(), &|k| env.get(k).cloned());
        unsafe {
            std::env::remove_var("FAKE_AGENT_MODE_FILE");
            std::env::remove_var("FAKE_AGENT_SLEEP");
            std::env::remove_var("FAKE_AGENT_ARGV_LOG");
        }
        assert_eq!(code.expect("runs"), 0);

        let argv = std::fs::read_to_string(&argv_log).expect("argv recorded");
        assert!(
            argv.contains("carry me across the restart"),
            "the restarted child must carry the same objective: {argv}"
        );
        assert!(
            argv.contains("Do not start new substantive work"),
            "crossing the budget must swap in the wrap-up instruction: {argv}"
        );

        let record = objective::load(&state, &super::super::state::repo_slug(tmp.path()))
            .expect("load")
            .expect("still present");
        assert_eq!(
            record.status,
            objective::Status::BudgetLimited,
            "the persisted record itself must carry the flip, not just the injected text"
        );
        assert!(record.spent_tokens >= 50_000, "got {}", record.spent_tokens);
    }

    /// The restart hook advances the durable objective with THIS process's
    /// own `prior_usage` total, which restarts from zero every time `exec`
    /// relaunches -- while `roll_up_spend` has been accumulating the whole
    /// run's spend into the same record. Persisting the smaller figure would
    /// erase the rolled-up total and hand the next child a budget line that
    /// says it has room it does not have.
    #[test]
    fn an_objective_restart_layer_never_regresses_a_rolled_up_spend() {
        let tmp = crate::commands::ctx::testenv::repo();
        let state = StateDir::from_root(tmp.path().join("state"));
        let key = super::super::state::repo_slug(tmp.path());
        objective::store(
            &state,
            &key,
            &objective::Objective {
                schema_version: objective::SCHEMA_VERSION,
                objective: "finish the batch".to_string(),
                budget_tokens: Some(200_000),
                deadline_secs: None,
                spent_tokens: 300_000,
                started_at: 1_700_000_000,
                status: objective::Status::BudgetLimited,
                pending_note: None,
                evidence: Vec::new(),
            },
        )
        .expect("store objective");

        let text = objective_layer_for_restart(&state, tmp.path(), 1_700_000_100, 50_000)
            .expect("a live objective renders a layer");
        assert!(
            text.contains("300000 / 200000 tokens spent"),
            "the layer must show the durable total, not this process's own: {text}"
        );

        let record = objective::load(&state, &key)
            .expect("load")
            .expect("still present");
        assert_eq!(record.spent_tokens, 300_000);
        assert_eq!(record.status, objective::Status::BudgetLimited);
    }

    /// Issue #169.2: a restart must never reset the token budget meter. Two
    /// children, each well under `--budget-tokens` on its own transcript
    /// alone, whose COMBINED spend exceeds it -- before this fix, a fresh
    /// transcript per restart meant the second child's own (still-under-
    /// budget) reading was all `evaluate_worker_budget` ever saw, so the run
    /// finished with exit `0` instead of `EXIT_BUDGET_EXHAUSTED`.
    ///
    /// `FAKE_AGENT_TURNS=1` shrinks one child's own transcript to a known,
    /// small total (2 assistant events x 20_000 cache-read tokens = 40_004
    /// with the fixture's own `input_tokens: 2` per event) so both runs can
    /// sit comfortably under a budget individually while landing over it
    /// together. The restart itself is a real nudge (`nudge_live_session`),
    /// the same deterministic trigger `a_nudge_restart_does_not_spend_the_
    /// rot_restart_budget` already uses -- not rot or a timeout -- because a
    /// nudge relaunch mints a fresh transcript exactly the same way, and
    /// does not depend on the rot scorer's own heuristics to fire on cue.
    #[test]
    fn a_restart_accumulates_spend_instead_of_resetting_the_budget_meter() {
        let tmp = crate::commands::ctx::testenv::repo();
        let home = tmp.path().join("home");
        let state_dir = tmp.path().join("state");
        let session_log = tmp.path().join("session.log");
        let session = "40404040-2222-4333-8444-555555555555";
        let mut env = base_env(&state_dir);
        env.insert("ZIRV_CTX_PACE".to_string(), "false".to_string());
        env.insert("ZIRV_CTX_POLL_MS".to_string(), "50".to_string());

        let modes = tmp.path().join("modes.txt");
        std::fs::write(&modes, "hang\nhealthy\n").expect("write modes");

        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        // C10: a guard, not a bare set/remove pair -- see the identical
        // comment on the nudge tests above this one.
        let _fake_agent = crate::commands::ctx::testenv::VarGuard::set(&[
            ("FAKE_AGENT_MODE_FILE", modes.to_str()),
            ("FAKE_AGENT_SESSION_ENV_LOG", session_log.to_str()),
            ("FAKE_AGENT_TURNS", Some("1")),
        ]);

        let first_transcript = transcript_for(&home, tmp.path(), session);
        let state_for_writer = state_dir.clone();
        let repo_for_writer = tmp.path().to_path_buf();
        let session_log_for_writer = session_log.clone();
        let transcript_for_writer = first_transcript.clone();
        let writer = std::thread::spawn(move || {
            wait_for_lines_or_panic(&session_log_for_writer, 1, Duration::from_secs(20));
            // The session-env line only says the child STARTED: the fixture
            // appends it before it has even created its transcript, let alone
            // written a turn into it. Nudging on that line alone raced the
            // restart ahead of the outgoing child's own spend, so
            // `harvest_spend` folded in a transcript that did not exist yet
            // (0 tokens) and the incoming child's own under-budget reading was
            // all the meter ever saw -- exit `0` instead of the accumulated
            // `EXIT_BUDGET_EXHAUSTED` this test is about. Wait for the whole
            // `FAKE_AGENT_TURNS=1` turn (4 lines) that harvest has to see.
            wait_for_lines_or_panic(&transcript_for_writer, 4, Duration::from_secs(20));
            nudge_live_session(&state_for_writer, &repo_for_writer, "keep going");
        });

        let args = ExecArgs {
            agent: Some("claude".to_string()),
            session_id: Some(session.to_string()),
            transcript: Some(first_transcript),
            prompt: Some("do the work".to_string()),
            max_restarts: Some(0),
            // Above one child's own ~40_004-token transcript, below two
            // combined (~80_008): neither child trips the budget on its own
            // reading, only the accumulated total does.
            budget_tokens: Some(60_000),
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
            code.expect("runs"),
            EXIT_BUDGET_EXHAUSTED,
            "the second child's own transcript alone is under budget -- only the accumulated \
             total exceeds it"
        );
        let log = std::fs::read_to_string(state_dir.join("logs/decisions.jsonl")).expect("log");
        assert!(log.contains("\"verdict\":\"budget\""), "got {log}");
        assert!(
            log.contains("\"action\":\"nudge-restart\""),
            "the restart that must not reset the meter actually happened: {log}"
        );
    }
}
