//! `zirv agent --runtime native` (issue #479, roadmap N10): the native fork
//! of one delegation.
//!
//! Everything a delegation does BEFORE it picks a backend -- validating
//! `--workdir`, allocating a `--worktree`, resolving the delegation envelope,
//! splicing `--attach-artifact`/`--result-schema`/`--task` onto the prompt,
//! claiming the task card -- is `agent::run_with`'s own work and is done
//! exactly once, for both runtimes, before this module is reached. What is
//! here is only the part that genuinely differs: there is no adapter to
//! select, no argv to build, no harness process to supervise, no transcript
//! to score, and no dashboard pane to hand the work to (a pane hosts a
//! harness TUI; a native session has no terminal UI until roadmap step N11).
//!
//! What is deliberately NOT different:
//!
//! - the SAME task card claim (`task::claim_locked`, taken by the caller) and
//!   the same completion/respawn treatment (`agent::finish_task_card`);
//! - the SAME per-tree writer permit (`permit::acquire_writer`), which is what
//!   makes a native and a legacy worker mutually exclusive on one checkout --
//!   the permit is moved into the native execution broker, so a repository
//!   write that is not backed by a permit for that exact tree is refused at
//!   effect time rather than trusted;
//! - the SAME per-provider token reservation (`reservation::reserve_within`),
//!   settled from the run's real usage;
//! - the SAME delegation envelope narrowing (`envelope::WorkerEnvelope`);
//! - the SAME result persistence, report-back mail and `--json` receipt shape
//!   an unchanged legacy orchestrator already consumes;
//! - the SAME durable delegation record (`ctx::delegation`), so a launch
//!   receipt exists before anything runs and the terminal outcome is
//!   published with a delivery identity a consumer can deduplicate on.

use std::io::Write;
use std::path::{Path, PathBuf};

use super::CtxResult;
use super::agent::{
    AgentArgs, DelegationMode, DelegationReceipt, DelegationState, delegation_outcome,
    finish_task_card, print_receipt, receipt_note, requested_envelope_from_args, resolve_worker_budget,
};
use super::config::{CtxConfig, EnvLookup};
use super::delegation;
use super::envelope;
use super::permit::{self, WorkerMode};
use super::result_schema::Schema;
use super::runtime::RuntimeKind;
use super::runtime::native::{HeadlessRequest, NativeLimits, NativeStatus};
use super::state::StateDir;

/// Everything `agent::run_with` has already resolved by the time it forks.
/// Borrowed rather than owned so nothing is recomputed (and so nothing can
/// drift from what the harness fork would have seen).
pub(crate) struct Request<'a> {
    pub args: &'a AgentArgs,
    /// The fully assembled worker prompt: operator text plus whatever
    /// `--attach-artifact`, `--result-schema` and `--task` spliced on.
    pub prompt: String,
    /// The DELEGATING repository -- where task cards, receipts and mail live.
    pub repo: &'a Path,
    /// The WORKER's own checkout (`--workdir`/`--worktree`, else `repo`).
    pub launch_repo: PathBuf,
    pub state: &'a StateDir,
    pub cfg: &'a CtxConfig,
    pub parent_envelope: &'a envelope::WorkerEnvelope,
    pub result_schema: Option<&'a Schema>,
}

/// Harness-runtime flags a native session has nothing to do with. Refused
/// loudly rather than silently ignored, the same rule `exec::run_native`
/// already applies: a delegation that quietly dropped `--max-restarts` would
/// look supervised when it is not.
fn refuse_harness_only_flags(args: &AgentArgs) -> CtxResult<()> {
    for (name, present) in [
        ("--max-restarts", args.max_restarts.is_some()),
        ("-- <flags>", !args.flags.is_empty()),
    ] {
        if present {
            return Err(format!(
                "{name} is a harness-runtime argument; a native worker supervises no external \
                 process and has no vendor CLI to pass flags to"
            )
            .into());
        }
    }
    Ok(())
}

/// The route this worker spends: `--route` when given, else the positional
/// `<name>` -- which under `--runtime native` names a route rather than a
/// harness -- with the reserved value `native` deferring to the `[roles]`
/// entry for `--role`.
fn requested_route(args: &AgentArgs) -> Option<&str> {
    match args.route.as_deref() {
        Some(route) => Some(route),
        None if args.name.eq_ignore_ascii_case("native") => None,
        None => Some(args.name.as_str()),
    }
}

fn role_of(args: &AgentArgs) -> &str {
    args.role.as_deref().unwrap_or("worker")
}

/// Maps the native loop's own final status onto the delegation vocabulary an
/// orchestrator already reads. `Completed` is the only phase a finish token
/// alone can produce, and only because `NativeStatus::Completed` already
/// outranks a bare finish token with every incomplete execution, unknown
/// outcome and undelivered input (issue #478).
fn phase_of(status: NativeStatus) -> delegation::Phase {
    match status {
        NativeStatus::Completed => delegation::Phase::Completed,
        NativeStatus::Interrupted => delegation::Phase::Cancelled,
        _ => delegation::Phase::Failed,
    }
}

/// Runs one delegated native worker end to end.
pub(crate) fn run<W: Write>(request: Request<'_>, w: &mut W, env: EnvLookup<'_>) -> CtxResult<i32> {
    let args = request.args;
    refuse_harness_only_flags(args)?;
    let state = request.state;
    let repo = request.repo;
    let cfg = request.cfg;
    let now = super::state::now_secs();

    // Ownership step 1 of 3: the per-provider token ledger. Resolved from
    // operator configuration alone (no credential store, no network), so an
    // unconfigured route fails here -- before a task card is marked running
    // by a worker that was never going to start.
    let (route_id, provider) =
        super::runtime::native::route_provider(&request.launch_repo, requested_route(args), role_of(args), env)?;

    let (worker_budget, reserved_ceiling) = resolve_worker_budget(&env, args)?;
    let worker_session = super::event::SessionId::new_v4().to_string();
    let child_short = super::sessions::short_id(&worker_session);

    // Ownership step 2 of 3: the delegation envelope. A request that would
    // WIDEN what the parent granted is refused before anything runs, never
    // silently clamped -- identical to the harness fork.
    let principal = format!("{}/{}", request.parent_envelope.principal, child_short);
    let requested = requested_envelope_from_args(
        args,
        request.parent_envelope,
        principal.clone(),
        worker_budget.tokens,
    );
    let child_envelope = match envelope::WorkerEnvelope::narrow(request.parent_envelope, &requested)
    {
        Ok(envelope) => envelope,
        Err(err) => {
            let reason = format!("delegation envelope refused: {err}");
            return refuse(args, w, &request.launch_repo, None, 2, reason);
        }
    };

    let reservation = super::reservation::reserve_within(
        state,
        &provider,
        &worker_session,
        worker_budget.tokens.unwrap_or(0),
        None,
        now,
    )
    .ok()
    .and_then(Result::ok)
    .map(|reservation| (provider.clone(), reservation.id));

    // Ownership step 3 of 3: the writer permit. The SAME per-tree claim a
    // legacy `--mode writing` worker takes, so the two can never both hold
    // one checkout -- and the same guard is then moved into the native
    // execution broker, which refuses any repository write it does not
    // cover.
    let tree = std::fs::canonicalize(&request.launch_repo).unwrap_or_else(|_| request.launch_repo.clone());
    let writer_permit = if args.mode == WorkerMode::Writing {
        match permit::acquire_writer(
            state,
            cfg.supervise.max_writers,
            &format!("session {child_short}: native/{route_id}"),
            &tree,
        ) {
            Ok(permit) => Some(permit),
            Err(refusal) => {
                if let Some((provider, id)) = &reservation {
                    let _ = super::reservation::release(state, provider, id);
                }
                let reason =
                    permit::describe_writer_refusal(&refusal, state, cfg.supervise.max_writers, &tree);
                return refuse(
                    args,
                    w,
                    &request.launch_repo,
                    Some(&worker_session),
                    super::exec::EXIT_WRITER_BUSY,
                    reason,
                );
            }
        }
    } else {
        None
    };

    // The immediate, durable launch receipt -- written BEFORE the session
    // starts, so an orchestrator that dies a millisecond later still finds
    // this delegation and its ownership.
    let delegation_id = uuid::Uuid::new_v4().simple().to_string();
    let handle = delegation::WorkerHandle {
        delegation: delegation_id.clone(),
        attempt: 1,
        runtime: RuntimeKind::Native,
        worker_session: worker_session.clone(),
        short: child_short.clone(),
        role: role_of(args).to_string(),
        task: args.task.clone(),
        group: args.group.clone(),
        objective: None,
        workdir: request.launch_repo.clone(),
    };
    let parent_session = super::mail::session_identity(&env);
    delegation::record_launch(state, repo, handle, parent_session, now)?;
    delegation::record_ownership(
        state,
        repo,
        &delegation_id,
        reservation.clone(),
        writer_permit.is_some().then(|| tree.clone()),
        now,
    )?;

    let mut limits = NativeLimits::default();
    if let Some(max_tool_calls) = worker_budget.tool_calls {
        limits.max_tool_calls = max_tool_calls;
    }
    if let Some(timeout_secs) = args.timeout_secs {
        limits.max_wall_ms = timeout_secs.saturating_mul(1000);
    }
    if let Some(tokens) = worker_budget.tokens {
        limits.max_output_tokens = tokens;
    }

    // The one human line a run can owe before its status exists is a resume's
    // outcome-unknown reconcile notice. It is captured rather than written to
    // `w`, because a `--json` delegation prints exactly one object on stdout;
    // anything captured here is re-emitted on stderr below.
    let mut notices: Vec<u8> = Vec::new();
    let status = super::runtime::native::run_session(
        &mut HeadlessRequest {
            repo: &request.launch_repo,
            prompt: &request.prompt,
            route: requested_route(args),
            role: role_of(args),
            limits,
            // A delegated worker always starts a fresh session; a follow-up
            // against a finished one is `delegation::follow_up`'s Resume, not
            // a second launch (see this module's own doc comment).
            resume: None,
            // Operator-only transport overrides belong to `zirv ctx exec`,
            // which is where an operator types them. A delegation never
            // silently swaps its worker's provider for a fixture.
            provider: None,
            fixture_tools: None,
            task: args.task.clone(),
            writer: writer_permit
                .map(|permit| Box::new(permit) as Box<dyn super::runtime::enforcement::WriterLease>),
        },
        &mut notices,
        env,
    );
    if !notices.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&notices));
    }

    let status = match status {
        Ok(status) => status,
        Err(error) => {
            // The worker never ran. Release everything this delegation took,
            // publish the failure (so a parent waiting on it is not left
            // waiting forever), and respawn-guard the task card exactly as
            // the harness fork does for a launch failure.
            let _ = delegation::publish_terminal(
                state,
                repo,
                cfg,
                &delegation_id,
                delegation::Phase::Failed,
                None,
                Some(error.to_string()),
                None,
                super::state::now_secs(),
            );
            let _ = delegation::close(state, repo, &delegation_id, super::state::now_secs());
            finish_task_card(
                state,
                repo,
                args,
                super::task::ExitKind::Crash,
                "launch failed",
                super::state::now_secs(),
            );
            return Err(error);
        }
    };

    let mut code = status.exit_code;
    let mut delegation_state = match status.final_text.as_deref() {
        Some(_) => DelegationState::Reported,
        None => DelegationState::ExitedNoReport,
    };
    let mut contract_errors: Vec<String> = Vec::new();
    let mut undeclared = Vec::new();
    let mut validated = None;

    // `--result-schema`/`--result-kind`: the identical structural contract a
    // harness worker is held to, evaluated against the native loop's own
    // final assistant text. There is no bounded resume retry here: a native
    // continuation is a follow-up against the durable delegation handle
    // (`delegation::follow_up`), not a second headless launch.
    if let Some(schema) = request.result_schema {
        let mut attempts: Vec<Vec<String>> = Vec::new();
        match status.final_text.as_deref() {
            Some(text) => {
                match super::agent::evaluate_report(schema, text, &request.launch_repo, &mut undeclared)
                {
                    Ok(value) => validated = Some(value),
                    Err(errors) => attempts.push(errors),
                }
            }
            None => attempts.push(vec![
                "no JSON object found in the worker's final message".to_string(),
            ]),
        }
        if validated.is_none() {
            code = super::exec::EXIT_CONTRACT_FAILED;
        }
        delegation_state = if validated.is_some() {
            DelegationState::ReportedValidated
        } else {
            DelegationState::ReportedContractFailed
        };
        contract_errors = attempts.last().cloned().unwrap_or_default();
    }

    let (stored_report, report_truncated) =
        super::agent::cap_report(status.final_text.as_deref());
    let result_path = if request.result_schema.is_some() {
        Some(super::agent::store_result(
            state,
            &worker_session,
            &args.name,
            &validated,
            &[contract_errors.clone()],
            &undeclared,
            stored_report.as_deref(),
            report_truncated,
        ))
    } else {
        stored_report.as_deref().map(|text| {
            super::agent::store_report_only(state, &worker_session, &args.name, text, report_truncated)
        })
    };

    // Persist first, notify second: `publish_terminal`'s own contract. The
    // delivery identity it returns is what a consumer deduplicates on.
    let summary = status
        .final_text
        .as_deref()
        .map(|text| super::agent::cap_report(Some(text)).0.unwrap_or_default());
    let publication = delegation::publish_terminal(
        state,
        repo,
        cfg,
        &delegation_id,
        phase_of(status.status),
        Some(code),
        summary,
        result_path.clone(),
        super::state::now_secs(),
    )?;

    // Issue #317: the card is only ever `Done` from a real report -- an exit
    // 0 with nothing to show is `SilentZero`, which respawn-guards instead.
    let exit_kind = match (request.result_schema.is_some(), validated.is_some(), code) {
        (true, true, _) => super::task::ExitKind::Reported,
        (true, false, _) => super::task::ExitKind::SilentZero,
        (false, _, 0) if status.final_text.is_some() => super::task::ExitKind::Reported,
        (false, _, 0) => super::task::ExitKind::SilentZero,
        _ => super::task::ExitKind::Crash,
    };
    finish_task_card(
        state,
        repo,
        args,
        exit_kind,
        delegation_outcome(code),
        super::state::now_secs(),
    );

    if let Some((provider, id)) = &reservation {
        let _ = super::reservation::settle(state, provider, id, status.usage.output_tokens);
    }
    if let Some(id) = args.group.as_deref() {
        let _ = super::group::settle_reservation(
            state,
            id,
            reserved_ceiling.unwrap_or(0),
            status.usage.output_tokens,
        );
    }
    let envelope_sha256 = envelope::digest(&child_envelope).ok();
    let _ = super::log::append(
        state,
        &super::log::Decision {
            ts: super::state::now_secs(),
            session: &worker_session,
            verb: "agent",
            verdict: "n/a",
            score: 0,
            action: super::log::DELEGATION_ACTION,
            detail: &format!(
                "native/{route_id} ({}): {} in / {} out -- {} [envelope {}]",
                status.configured_model,
                status.usage.input_tokens,
                status.usage.output_tokens,
                delegation_outcome(code),
                envelope_sha256.as_deref().unwrap_or("n/a"),
            ),
            observed_at: None,
        },
    );
    // Ownership released the moment the work is done -- the writer permit
    // went into the broker and drops with the session above, and this
    // releases the reservation record and retires the delegation while
    // preserving every receipt it published.
    let _ = delegation::close(state, repo, &delegation_id, super::state::now_secs());

    if args.json {
        let receipt = DelegationReceipt {
            schema_version: 1,
            harness: args.name.clone(),
            runtime: RuntimeKind::Native.as_str(),
            delegation: Some(delegation_id),
            model: Some(status.configured_model.clone()),
            mode: DelegationMode::Inline,
            state: delegation_state,
            exit_code: Some(code),
            session: Some(child_short),
            task: args.task.clone(),
            workdir: Some(request.launch_repo.clone()),
            result_path,
            report_truncated,
            mail_delivered: publication.mailed,
            errors: contract_errors,
            capability_warnings: Vec::new(),
            reason: None,
            note: receipt_note(delegation_state),
        };
        print_receipt(w, &receipt)?;
    } else {
        writeln!(
            w,
            "{}",
            super::agent::no_contract_result_line(result_path.as_deref(), code)
        )?;
    }
    Ok(code)
}

/// The pre-launch refusal shape, in whichever of the two output forms this
/// delegation asked for. Nothing has run, so there is no delegation record to
/// name yet -- exactly what `launch_failure_receipt`'s own `Option`
/// parameters exist for.
fn refuse<W: Write>(
    args: &AgentArgs,
    w: &mut W,
    workdir: &Path,
    worker_session: Option<&str>,
    code: i32,
    reason: String,
) -> CtxResult<i32> {
    if args.json {
        let receipt = DelegationReceipt {
            schema_version: 1,
            harness: args.name.clone(),
            runtime: RuntimeKind::Native.as_str(),
            delegation: None,
            model: None,
            mode: DelegationMode::Inline,
            state: DelegationState::LaunchFailed,
            exit_code: Some(code),
            session: worker_session.map(super::sessions::short_id),
            task: args.task.clone(),
            workdir: Some(workdir.to_path_buf()),
            result_path: None,
            report_truncated: false,
            mail_delivered: false,
            errors: Vec::new(),
            capability_warnings: Vec::new(),
            reason: Some(reason),
            note: receipt_note(DelegationState::LaunchFailed),
        };
        print_receipt(w, &receipt)?;
    } else {
        writeln!(w, "{reason}")?;
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_for(name: &str) -> AgentArgs {
        AgentArgs {
            name: name.to_string(),
            prompt: "do the thing".to_string(),
            runtime: "native".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn the_positional_name_selects_the_route_and_native_defers_to_the_role() {
        assert_eq!(requested_route(&args_for("fast")), Some("fast"));
        assert_eq!(requested_route(&args_for("native")), None);
        let mut explicit = args_for("fast");
        explicit.route = Some("slow".to_string());
        assert_eq!(
            requested_route(&explicit),
            Some("slow"),
            "--route overrides the positional"
        );
    }

    #[test]
    fn harness_only_arguments_are_refused_rather_than_silently_dropped() {
        let mut restarts = args_for("native");
        restarts.max_restarts = Some(3);
        assert!(refuse_harness_only_flags(&restarts).is_err());

        let mut passthrough = args_for("native");
        passthrough.flags = vec!["--dangerously-skip-permissions".to_string()];
        assert!(refuse_harness_only_flags(&passthrough).is_err());

        assert!(refuse_harness_only_flags(&args_for("native")).is_ok());
    }

    #[test]
    fn only_a_native_completed_status_becomes_a_completed_delegation() {
        assert_eq!(
            phase_of(NativeStatus::Completed),
            delegation::Phase::Completed
        );
        assert_eq!(
            phase_of(NativeStatus::Interrupted),
            delegation::Phase::Cancelled
        );
        for status in [
            NativeStatus::Incomplete,
            NativeStatus::LimitReached,
            NativeStatus::Failed,
        ] {
            assert_eq!(
                phase_of(status),
                delegation::Phase::Failed,
                "{status:?} must never read as a completed delegation"
            );
        }
    }

    #[test]
    fn a_native_worker_cannot_take_a_checkout_a_legacy_worker_already_holds() {
        // Acceptance criterion (b): the exclusion is the SHARED per-tree
        // writer claim, so it does not matter which runtime got there first.
        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        let tree = dir.path().join("repo");
        std::fs::create_dir_all(&tree).expect("tree");
        let tree = std::fs::canonicalize(&tree).expect("canonical");

        let legacy = permit::acquire_writer(&state, 4, "session legacy: claude", &tree)
            .expect("the legacy worker takes the tree");
        let native = permit::acquire_writer(&state, 4, "session nativeone: native/fast", &tree);
        assert!(
            native.is_err(),
            "a native worker must not hold a checkout a legacy worker already writes"
        );
        drop(legacy);
        let native = permit::acquire_writer(&state, 4, "session nativeone: native/fast", &tree)
            .expect("released");
        assert!(
            permit::acquire_writer(&state, 4, "session legacy2: claude", &tree).is_err(),
            "and the exclusion holds in the other direction too"
        );
        drop(native);
    }

    #[test]
    fn a_native_and_a_legacy_worker_cannot_both_claim_one_task_card() {
        // Acceptance criterion (b), the task half: a native delegation claims
        // its card through the SAME `task::claim_locked` a legacy delegation
        // uses, so the second claimant -- whichever runtime it is -- is
        // refused rather than paid to redo the first one's work.
        use super::super::task;

        let dir = tempfile::tempdir().expect("tempdir");
        let state = StateDir::from_root(dir.path().join("state"));
        let slug = "repo-under-test";
        task::append_event(
            &state,
            slug,
            &task::Event::Created {
                id: "task-1".to_string(),
                repo_slug: slug.to_string(),
                title: "investigate".to_string(),
                brief: "read and report".to_string(),
                parents: Vec::new(),
                group_id: None,
                workdir: None,
                at: 1,
            },
        )
        .expect("created");

        let live_pid = std::process::id();
        let start = super::super::sessions::process_start_secs(live_pid);
        let first = task::claim_locked(
            &state,
            slug,
            "task-1",
            "legacy-session",
            live_pid,
            start,
            "host",
            2,
            task::DEFAULT_CLAIM_TTL_SECS,
        )
        .expect("claim")
        .expect("card exists");
        assert!(first.is_ok(), "the first claimant takes the card");

        let second = task::claim_locked(
            &state,
            slug,
            "task-1",
            "native-session",
            live_pid,
            start,
            "host",
            3,
            task::DEFAULT_CLAIM_TTL_SECS,
        )
        .expect("claim")
        .expect("card exists");
        assert!(
            second.is_err(),
            "a native worker must not claim a card a live legacy claimant already holds"
        );
    }
}
