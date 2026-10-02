//! The persistent runtime's already-open native conversation
//! (`run_hosted_turns`, issue #489 step N20).

use std::sync::{Arc, mpsc};

use super::super::super::CtxResult;
use super::super::super::config::EnvLookup;
use super::super::super::lifecycle;
use super::super::super::provider::adapter::CancellationFlag;
use super::super::super::state::now_ms;
use super::super::compaction::{DistillBudget, NativeBudget, RETAIN_RECENT_MESSAGES};
use super::super::journal::{Journal, JournalSessionId, RouteIdentity};
use super::super::{BackendConversationRef, RuntimeKind, SessionHandle, UiSurface};
use super::headless::{
    Accounting, HeadlessRequest, brokered_tools, build_transport, compile_standing_context,
};
use super::interactive::InteractiveProgress;
use super::turn::{NativeLoop, TurnDriver};
use super::types::{
    CompactionSettings, NativeFinalStatus, NativeLimits, NativeSessionConfig, RecompileContext,
};

/// Apply the proxy decision on the first turn; an existing transport is never rebuilt, and any route mismatch or workflow skip is announced rather than silently claimed. (#537)
pub(super) fn apply_proxy_first_turn(
    cfg: &super::super::super::config::CtxConfig,
    state: &super::super::super::state::StateDir,
    repo: &std::path::Path,
    request: &str,
    home: &std::path::Path,
    route: &mut RouteIdentity,
    progress_tx: &mpsc::Sender<InteractiveProgress>,
) {
    if super::super::super::proxy::activation(cfg).is_err() {
        return;
    }
    // `apply_proxy_first_turn` only ever runs from `spawn_interactive`'s own
    // worker thread (a `UiSurface::DashboardPane`) -- an interactive native
    // pane, never a headless launch -- so `headless` is always `false` here.
    let decision = super::super::super::proxy::decide(cfg, state.root(), repo, request, false);
    match super::super::super::proxy::launch::start_workflow_for(
        &decision,
        state.root(),
        repo,
        request,
        None,
    ) {
        Ok(super::super::super::proxy::launch::WorkflowStart::Skipped { reason }) => {
            let _ = progress_tx.send(InteractiveProgress::Notice(format!("proxy: {reason}")));
        }
        Ok(super::super::super::proxy::launch::WorkflowStart::Started { .. }) => {}
        Err(error) => {
            let _ = progress_tx.send(InteractiveProgress::Notice(format!(
                "proxy: workflow not started ({error})"
            )));
        }
    }

    let native_config = super::super::super::provider::config::NativeConfig::load(home, repo)
        .ok()
        .flatten();
    match native_config.as_ref().and_then(|native| {
        super::super::super::proxy::native::route_for_decision(&decision, native)
    }) {
        Some(route_id) => {
            let _ = progress_tx.send(InteractiveProgress::Notice(format!(
                "proxy: route {route_id} recorded; transport unchanged this session"
            )));
            route.route = route_id;
        }
        None => {
            let _ = progress_tx.send(InteractiveProgress::Notice(
                "proxy: no matching native route; keeping the configured role route".to_string(),
            ));
        }
    }
}

/// `NativeSessionConfig::task` needs a validated `journal::TaskId`, but by
/// the time it is built the plain `Option<String>` has already been
/// consumed once (`task.clone().map(TaskId::new).transpose()?` above, moved
/// into `SessionIdentity`) -- re-validating from the original string here is
/// cheaper than threading a second clone through every intermediate step
/// above for a value only this one call site still needs.
pub(super) fn task_for_config(
    _handle: &SessionHandle,
    _route: &RouteIdentity,
    task: Option<&str>,
) -> CtxResult<Option<super::super::journal::TaskId>> {
    Ok(task.map(super::super::journal::TaskId::new).transpose()?)
}

/// Everything the persistent runtime needs to run the turns already queued on
/// an EXISTING native conversation (issue #489, step N20).
///
/// The difference from [`HeadlessRequest`] is the whole point: a hosted turn
/// neither creates the journal session nor resumes it nor completes it. The
/// service created it when the client asked for the session, the generation is
/// the one the service is holding, and the conversation outlives this turn --
/// so advancing a generation here (what a resume does) would fence the service
/// out of its own session, and completing it here would end a conversation the
/// operator never asked to end.
#[derive(Debug)]
pub struct HostedTurn<'a> {
    pub repo: &'a std::path::Path,
    /// The journal session whose queued input this runs.
    pub session: &'a JournalSessionId,
    /// The seat short id, so the loop's identity matches the registry record
    /// the service already filed for this session.
    pub seat_short: &'a str,
    pub generation: u64,
    pub role: &'a str,
    pub route: Option<&'a str>,
    pub limits: NativeLimits,
    pub provider: Option<&'a str>,
    pub fixture_tools: Option<&'a std::path::Path>,
    pub task: Option<String>,
    /// The writer permit this session's repository writes are backed by, or
    /// `None` for a session nobody granted a tree to -- whose file writes are
    /// then refused, which is the honest answer rather than an unbacked write.
    pub writer: Option<Box<dyn super::super::enforcement::WriterLease>>,
    /// The hosted protocol controller's exact-action approval channel.
    pub approvals: Option<Arc<super::super::enforcement::InteractiveApprovals>>,
    /// Shared with the host, so `session.interrupt` cancels the turn this
    /// call is running rather than the next one.
    pub cancel: Arc<CancellationFlag>,
}

/// Drives every turn already queued on a hosted native session to completion.
///
/// Returns when the conversation has no unconsumed input left, the turn was
/// interrupted, or a limit was hit -- i.e. when the session is idle again. The
/// session itself stays open: the caller (`session::native`) keeps its
/// journal, its registry record and its identity, and calls this again the
/// next time input arrives.
pub fn run_hosted_turns<W: std::io::Write>(
    turn: &mut HostedTurn<'_>,
    w: &mut W,
    env: EnvLookup<'_>,
) -> CtxResult<NativeFinalStatus> {
    super::super::require_native_available()?;
    use super::super::super::state::StateDir;

    let _ = w;
    let state = StateDir::resolve(env)?;
    let home = crate::utils::home_dir()?;
    let cfg = super::super::super::config::CtxConfig::load(turn.repo, env)?;
    let task = turn
        .task
        .clone()
        .map(super::super::journal::TaskId::new)
        .transpose()?;

    // The SAME transport, route resolution and broker assembly a headless run
    // uses. A second way to build either would be a second place for a native
    // launch to drift, which is exactly what issue #489 says not to do.
    let mut request = HeadlessRequest {
        repo: turn.repo,
        prompt: "",
        route: turn.route,
        role: turn.role,
        limits: turn.limits,
        session_id: None,
        cancellation: None,
        resume: None,
        provider: turn.provider,
        fixture_tools: turn.fixture_tools,
        task: turn.task.clone(),
        writer: turn.writer.take(),
        accounting: Accounting::Seat,
    };
    let (provider, mut tools, route, brokered) =
        build_transport(&request, &state, &home, &cfg, env)?;

    let execution_pool =
        matches!(&provider, TurnDriver::Execution(_)).then(|| route.billing_pool.to_string());
    if execution_pool.is_some()
        && let Some(refusal) = super::super::super::native_account::native_placement(
            &state,
            &cfg,
            turn.repo,
            &route.route,
            super::super::super::state::now_secs(),
        )
        .and_then(|placement| placement.refusal)
    {
        return Err(refusal.into());
    }

    let handle = SessionHandle {
        runtime: RuntimeKind::Native,
        logical_id: turn.session.to_string(),
        short: turn.seat_short.to_string(),
        generation: turn.generation,
        role: turn.role.to_string(),
        surface: UiSurface::Headless,
        conversation: Some(BackendConversationRef {
            agent: RuntimeKind::Native.as_str().to_string(),
            conversation: turn.session.to_string(),
        }),
    };
    if brokered {
        tools = brokered_tools(
            &mut request,
            &state,
            &home,
            &cfg,
            &handle,
            turn.approvals.clone(),
            env,
        )?;
    }

    let compaction = CompactionSettings {
        enabled: true,
        policy: super::super::super::provider::config::NativeConfig::load(&home, turn.repo)?
            .map(|native| native.compaction_policy())
            .unwrap_or_default(),
        budget: NativeBudget {
            context_window_tokens: super::super::super::provider::capability::declared(
                route.protocol,
                &route.model,
                None,
            )
            .context_window,
            output_reserve_tokens: turn.limits.max_output_tokens,
        },
        score: cfg.score.clone(),
        distill: DistillBudget::default(),
        retain_recent_messages: RETAIN_RECENT_MESSAGES,
        constraints: Vec::new(),
        state: Some(state.clone()),
    };

    // Compile the same standing context as a headless native run. (#484)
    let (system, preamble) = compile_standing_context(
        &state,
        &home,
        &cfg,
        &request,
        &route,
        turn.session,
        super::super::super::state::now_secs(),
        &[],
    )?;
    let mut journal = Journal::open(&state)?;
    let mut driver = NativeLoop::new_driver(
        NativeSessionConfig {
            session: turn.session.clone(),
            generation: turn.generation,
            route,
            role: turn.role.to_string(),
            seat_model: env(super::super::super::adapters::SEAT_MODEL_ENV),
            write_posture: lifecycle::orchestrator_write_posture(&cfg),
            limits: turn.limits,
            task,
            workflow_gate: None,
            compaction,
            // Gate real effects against this session's repository workflow. (#484)
            workflow_repo: brokered.then(|| turn.repo.to_path_buf()),
            system,
            preamble,
        },
        &provider,
        tools.as_mut(),
        &mut journal,
        Arc::clone(&turn.cancel),
        &now_ms,
        env,
    );
    // Recheck standing instructions at each turn boundary. (#538)
    driver.set_recompile_context(RecompileContext {
        state: state.clone(),
        home: home.clone(),
        cfg: cfg.clone(),
        repo: turn.repo.to_path_buf(),
    });
    let reservation = execution_pool.as_ref().and_then(|pool| {
        super::super::super::native_account::reserve_seat_turn(
            &state,
            pool,
            turn.session.as_str(),
            turn.limits.max_output_tokens,
            super::super::super::state::now_secs(),
        )
    });
    let result = driver.run_to_completion();
    if execution_pool.is_some() {
        let status = match &result {
            Ok(status) => status,
            Err(aborted) => aborted.status.as_ref(),
        };
        super::super::super::native_account::settle_seat_turn(
            &state,
            &cfg,
            status,
            reservation.as_ref(),
            Some(turn.seat_short),
        );
    }
    result.map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::super::interactive::{spawn_fixture_interactive_session, wait_for_idle};
    use super::super::tests::interactive_shutdown_fixture;

    /// Issue #537 (T2b): the harness proxy's decision applies on a native
    /// session's FIRST submitted turn only. Enabled against a typesafe
    /// endpoint that refuses the connection immediately (a local TCP
    /// listener bound then dropped before use, so nothing is ever
    /// listening) -- `proxy::decide` never fails even so, it falls through
    /// to the deterministic baseline and still persists a decision.
    /// `ZIRV_CTX_AGENT` names an adapter that does not exist, so the
    /// in-process helper fallback fails on a plain lookup rather than
    /// touching any real adapter or subprocess. Two turns are submitted;
    /// only ONE decision is ever appended to `proxy-decisions.jsonl`.
    #[test]
    fn proxy_decision_applies_once_on_the_first_submitted_turn_only() {
        let (repo, state, _tree, mut env) = interactive_shutdown_fixture();

        let closed_port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.local_addr().expect("addr").port()
            // The listener is dropped here: nothing answers on this port
            // from this point on, so a connection attempt refuses fast.
        };
        let credential_env = "NATIVE_TEST_PROXY_KEY_537";
        env.insert("ZIRV_CTX_PROXY_ENABLED".to_string(), "true".to_string());
        env.insert(
            "ZIRV_CTX_PROXY_TYPESAFE_BASE_URL".to_string(),
            format!("http://127.0.0.1:{closed_port}"),
        );
        env.insert(
            "ZIRV_CTX_PROXY_TYPESAFE_CREDENTIAL_ENV".to_string(),
            credential_env.to_string(),
        );
        env.insert(
            "ZIRV_CTX_PROXY_TYPESAFE_TIMEOUT_SECS".to_string(),
            "1".to_string(),
        );
        env.insert(
            "ZIRV_CTX_AGENT".to_string(),
            "zirv-test-no-such-adapter-537".to_string(),
        );
        // SAFETY (test-only): a unique env var name this test owns.
        unsafe {
            std::env::set_var(credential_env, "secret");
        }

        let session = spawn_fixture_interactive_session(repo.path(), &env);
        session.submit("first request".to_string()).expect("submit");
        wait_for_idle(&session);
        session
            .submit("second request".to_string())
            .expect("submit");
        wait_for_idle(&session);
        session.shutdown();

        // SAFETY (test-only): cleans up the var this test set above.
        unsafe {
            std::env::remove_var(credential_env);
        }

        let decisions_path = state.root().join("proxy-decisions.jsonl");
        let text = std::fs::read_to_string(&decisions_path).unwrap_or_default();
        let count = text.lines().filter(|line| !line.trim().is_empty()).count();
        assert_eq!(count, 1, "decide must run on the first turn only: {text}");
    }

    /// Issue #537 (T2b): `[proxy] enabled = false` (the default) leaves a
    /// native session's first turn exactly as before -- no decision is ever
    /// computed or persisted, so `proxy-decisions.jsonl` never appears.
    ///
    /// Issue #713: `interactive_shutdown_fixture` does not isolate the
    /// operator's real `~/.zirv/ctx.toml`, so a machine whose operator
    /// config enables the proxy would otherwise leak that setting in here.
    /// Pin the posture this test actually needs instead.
    #[test]
    fn proxy_disabled_leaves_the_native_session_unaffected() {
        let (repo, state, _tree, mut env) = interactive_shutdown_fixture();
        env.insert("ZIRV_CTX_PROXY_ENABLED".to_string(), "false".to_string());

        let session = spawn_fixture_interactive_session(repo.path(), &env);
        session.submit("do the thing".to_string()).expect("submit");
        wait_for_idle(&session);
        session.shutdown();

        let decisions_path = state.root().join("proxy-decisions.jsonl");
        assert!(
            !decisions_path.exists(),
            "a disabled proxy must never persist a decision"
        );
    }
}
