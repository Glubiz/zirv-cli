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
    finish_task_card, print_receipt, receipt_note, requested_envelope_from_args,
    resolve_worker_budget,
};
use super::config::{CtxConfig, EnvLookup};
use super::delegation;
use super::envelope;
use super::native_account::{self, Settlement};
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
    /// `runtime::native::HeadlessRequest::provider`'s own escape hatch, one
    /// level up: `Some("fixture:<path>")` opens this worker against the
    /// deterministic fixture provider instead of the operator's real native
    /// configuration, so a test can drive this whole function end to end.
    /// `None` on every production call site (`agent::run_with`), which is
    /// why a delegation can never silently swap its worker's provider.
    pub provider_override: Option<String>,
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
    args.role.as_deref().unwrap_or(super::team::DEFAULT_ROLE)
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
    super::runtime::require_native_available()?;
    let args = request.args;
    let state = request.state;
    let repo = request.repo;
    let cfg = request.cfg;
    let now = super::state::now_secs();
    if let Err(error) = refuse_harness_only_flags(args) {
        release_task_claim(state, repo, args);
        return Err(error);
    }

    // Ownership step 1 of 3: the BILLING POOL's token ledger. Resolved from
    // operator configuration alone (no credential store, no network), so an
    // unconfigured route fails here -- before a task card is marked running
    // by a worker that was never going to start.
    //
    // Issue #554: keyed by the POOL, not by the vendor. Two routes on one
    // account draw on one balance and must reserve against one ledger; two
    // accounts at one vendor must not be coupled into one.
    let (route_id, _provider, billing_pool) = match super::runtime::native::route_pool(
        &request.launch_repo,
        requested_route(args),
        role_of(args),
        env,
    ) {
        Ok(route) => route,
        Err(error) => {
            release_task_claim(state, repo, args);
            return Err(error);
        }
    };

    // Issue #554: the SHARED allocator, not a parallel decision. The native
    // route becomes a real row in the same `CapacitySnapshot` a harness
    // delegation is placed against -- pool-keyed capacity, endpoint coupling,
    // and the eligibility gate that runs ahead of ranking -- and its health
    // comes from the persistent breaker records this run folds its own
    // outcome back into below.
    let placement =
        native_account::native_placement(state, cfg, &request.launch_repo, &route_id, now);
    if let Some(refusal) = placement.as_ref().and_then(|p| p.refusal.clone()) {
        return refuse(
            (state, repo),
            args,
            w,
            &request.launch_repo,
            None,
            1,
            refusal,
        );
    }

    let (worker_budget, reserved_ceiling) = match resolve_worker_budget(&env, args) {
        Ok(budget) => budget,
        Err(error) => {
            release_task_claim(state, repo, args);
            return Err(error);
        }
    };
    let worker_session = args
        .session_id
        .clone()
        .unwrap_or_else(|| super::event::SessionId::new_v4().to_string());
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
            return refuse(
                (state, repo),
                args,
                w,
                &request.launch_repo,
                None,
                2,
                reason,
            );
        }
    };
    let child_envelope_json = envelope::canonical_json(&child_envelope).ok();
    let child_env = super::agent::envelope_env(
        env,
        child_envelope_json,
        Some(child_envelope.principal.clone()),
    );

    // Issue #554: a reservation of ZERO reserved nothing -- a worker with no
    // explicit `--budget-tokens` took its capacity without ever holding any.
    // The estimate falls back to the loop's own output ceiling, which is the
    // only number this seam has before the run, and the settlement below
    // replaces it with the truth.
    let reserved_estimate = worker_budget
        .tokens
        .unwrap_or_else(|| NativeLimits::default().max_output_tokens);
    let reservation = super::reservation::reserve_within(
        state,
        &billing_pool,
        &worker_session,
        reserved_estimate,
        None,
        now,
    )
    .ok()
    .and_then(Result::ok)
    .map(|reservation| (billing_pool.clone(), reservation.id));

    // Ownership step 3 of 3: the writer permit. The SAME per-tree claim a
    // legacy `--mode writing` worker takes, so the two can never both hold
    // one checkout -- and the same guard is then moved into the native
    // execution broker, which refuses any repository write it does not
    // cover.
    let tree =
        std::fs::canonicalize(&request.launch_repo).unwrap_or_else(|_| request.launch_repo.clone());
    let writer_permit = if args.mode == WorkerMode::Writing {
        // Issue #543: this process's own seat identity (if any), read the
        // same way `seat::guard_from_env` does, but fed into the STRICT
        // `seat::guard` verdict via an explicit `SeatFence` -- an uncommitted
        // successor delegating a writing worker must not hand that worker a
        // lease before its own rollover commits, which `guard_from_env`'s
        // supersession-only check let through. This is NOT the delegated
        // worker's own future seat: `child_short`'s eventual native session
        // seat is still created later inside `runtime::native::run_session`
        // under a fresh identity, and pre-registering one here remains wrong
        // for the reason PR #535 already gave (an orphaned record `seat::
        // register`'s hardcoded `RuntimeKind::Harness` couldn't even stand in
        // for correctly) -- see issue #543's own tracking comment for closing
        // that separate gap.
        let identity = super::seat::env_seat_identity();
        let fence = identity
            .as_ref()
            .map(|(short, generation)| permit::SeatFence {
                short,
                generation: *generation,
            });
        match permit::acquire_writer(
            state,
            cfg.supervise.max_writers,
            &format!("session {child_short}: native/{route_id}"),
            &tree,
            fence,
        ) {
            Ok(permit) => Some(permit),
            Err(refusal) => {
                if let Some((provider, id)) = &reservation {
                    let _ = super::reservation::release(state, provider, id);
                }
                let reason = permit::describe_writer_refusal(
                    &refusal,
                    state,
                    cfg.supervise.max_writers,
                    &tree,
                );
                return refuse(
                    (state, repo),
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
        manifest: None,
        plan_override: false,
    };
    let parent_session = super::mail::session_identity(&env);
    delegation::record_launch(state, repo, handle, parent_session.clone(), now)?;
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
        // Carry the delegation ceiling to the loop as well. Official
        // execution must refuse a token ceiling it cannot enforce.
        limits.max_budget_tokens = Some(tokens);
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
            session_id: Some(&worker_session),
            cancellation: args.cancellation.clone(),
            // A delegated worker always starts a fresh session; a follow-up
            // against a finished one is `delegation::follow_up`'s Resume, not
            // a second launch (see this module's own doc comment).
            resume: None,
            // Operator-only transport overrides belong to `zirv ctx exec`,
            // which is where an operator types them. A delegation never
            // silently swaps its worker's provider for a fixture -- see
            // `Request::provider_override`, which is `None` for every
            // production caller.
            provider: request.provider_override.as_deref(),
            fixture_tools: None,
            task: args.task.clone(),
            writer: writer_permit.map(|permit| {
                Box::new(permit) as Box<dyn super::runtime::enforcement::WriterLease>
            }),
            accounting: super::runtime::native::Accounting::CallerOwned,
        },
        &mut notices,
        &child_env,
    );
    if !notices.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&notices));
    }

    let status = match status {
        Ok(status) => status,
        Err(error) => {
            // Issue #554 (integration review): "the worker never ran" is only
            // true for a LAUNCH failure. A loop that aborted after billing
            // real turns carries what it spent (`AbortedRun`), and those
            // tokens are spent whatever happens next -- so they settle here,
            // with this delegation's own identity, before anything is torn
            // down. `delegation::close` below would otherwise merely release
            // the estimate and the real spend would vanish from
            // `zirv ctx spend`.
            if let Some(aborted) = error.downcast_ref::<super::runtime::native::AbortedRun>() {
                native_account::settle_native_run(
                    state,
                    cfg,
                    &aborted.status,
                    &Settlement {
                        reservation: reservation.as_ref(),
                        group: args.group.as_deref(),
                        reserved_ceiling,
                        parent_session: parent_session.as_deref(),
                        principal: &child_envelope.principal,
                        envelope_sha256: envelope::digest(&child_envelope).ok().as_deref(),
                        mode: Some(args.mode),
                        exit_code: aborted.status.exit_code,
                    },
                );
            }
            // Release everything else this delegation took, publish the
            // failure (so a parent waiting on it is not left waiting
            // forever), and respawn-guard the task card exactly as the
            // harness fork does for a launch failure.
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
                match super::agent::evaluate_report(
                    schema,
                    text,
                    &request.launch_repo,
                    &mut undeclared,
                ) {
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

    let (stored_report, report_truncated) = super::agent::cap_report(status.final_text.as_deref());
    let result_path = if request.result_schema.is_some() {
        Some(super::agent::store_result(
            state,
            repo,
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
            super::agent::store_report_only(
                state,
                repo,
                &worker_session,
                &args.name,
                text,
                report_truncated,
            )
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

    let envelope_sha256 = envelope::digest(&child_envelope).ok();
    let settled_total = native_account::settle_native_run(
        state,
        cfg,
        &status,
        &Settlement {
            reservation: reservation.as_ref(),
            group: args.group.as_deref(),
            reserved_ceiling,
            parent_session: parent_session.as_deref(),
            principal: &child_envelope.principal,
            envelope_sha256: envelope_sha256.as_deref(),
            mode: Some(args.mode),
            exit_code: code,
        },
    );
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
                "native/{route_id} ({}): {} in / {} out, {settled_total} settled on pool \
                 {billing_pool} -- {} [placed {}] [envelope {}]",
                status.configured_model,
                status.usage.input_tokens,
                status.usage.output_tokens,
                delegation_outcome(code),
                placement
                    .as_ref()
                    .and_then(|p| p.selected.as_deref())
                    .unwrap_or("n/a"),
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
            session: Some(status.session.clone()),
            task: args.task.clone(),
            workdir: Some(request.launch_repo.clone()),
            result_path,
            report_truncated,
            mail_delivered: publication.mailed,
            errors: contract_errors,
            capability_warnings: Vec::new(),
            blocked_families: Vec::new(),
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
    claim: (&StateDir, &Path),
    args: &AgentArgs,
    w: &mut W,
    workdir: &Path,
    worker_session: Option<&str>,
    code: i32,
    reason: String,
) -> CtxResult<i32> {
    release_task_claim(claim.0, claim.1, args);
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
            blocked_families: Vec::new(),
            reason: Some(reason),
            note: receipt_note(DelegationState::LaunchFailed),
        };
        print_receipt(w, &receipt)?;
    } else {
        writeln!(w, "{reason}")?;
    }
    Ok(code)
}

fn release_task_claim(state: &StateDir, repo: &Path, args: &AgentArgs) {
    finish_task_card(
        state,
        repo,
        args,
        super::task::ExitKind::Crash,
        "launch refused",
        super::state::now_secs(),
    );
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

    /// Issue #554 (review round 2): a delegated native worker accounts
    /// EXACTLY ONCE.
    ///
    /// Round 1 put reserve/settle/place into `run_session` for the seat
    /// paths, but `native_worker` already did all three itself -- so every
    /// delegated worker double-reserved the same billing pool and wrote two
    /// `log::Delegation` rows, doubling what `zirv ctx spend` reported an
    /// account had spent. Drives the whole of `run` end to end against the
    /// fixture provider and counts.
    #[test]
    fn delegated_native_worker_accounts_exactly_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let state = StateDir::from_root(tmp.path().join("state"));

        let native_toml = home.join(crate::utils::SCRIPT_DIR_NAME).join("native.toml");
        std::fs::create_dir_all(native_toml.parent().expect("parent")).expect("mkdir .zirv");
        std::fs::write(
            &native_toml,
            "schema=1
             [account.work]
provider='anthropic'
credential='env:KEY'
             [route.opus]
account='work'
model='claude-opus-5'
             [roles]
worker='opus'
",
        )
        .expect("write native.toml");

        let mut args = args_for("opus");
        args.mode = WorkerMode::ReadOnly;
        let cfg = CtxConfig::default();
        let parent = envelope::WorkerEnvelope::locked();
        let provider = format!(
            "fixture:{}",
            crate::commands::ctx::runtime::fixture::fixture_root()
                .join("helper-answer.json")
                .display()
        );
        let env: std::collections::HashMap<String, String> = [(
            crate::commands::ctx::state::STATE_ENV.to_string(),
            state.root().to_str().expect("utf8").to_string(),
        )]
        .into();
        let lookup = |key: &str| env.get(key).cloned();

        let code = run(
            Request {
                args: &args,
                prompt: "do the thing".to_string(),
                repo: &repo,
                launch_repo: repo.clone(),
                state: &state,
                cfg: &cfg,
                parent_envelope: &parent,
                result_schema: None,
                provider_override: Some(provider),
            },
            &mut Vec::new(),
            &lookup,
        )
        .expect("a delegated native worker runs end to end");
        assert_eq!(code, 0, "the fixture worker completes");

        let rows = super::super::log::read_delegations(&state, 20);
        assert_eq!(
            rows.len(),
            1,
            "exactly ONE delegation row per worker -- two owners means `zirv ctx spend`              reports double what the account spent: {rows:?}"
        );
        assert_ne!(
            rows[0].principal, "seat",
            "and it is the DELEGATION's row, carrying this worker's own narrowed principal              ({}), never the seat row `run_session` would have written in parallel",
            rows[0].principal
        );
        assert!(
            rows[0].mode.is_some(),
            "with the worker's own delegation mode, which a seat row has no value for"
        );
        assert_eq!(
            crate::commands::ctx::reservation::outstanding(&state, "work", 0),
            0,
            "and the pool has nothing left outstanding: one reserve, one settle"
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
    fn native_worker_installs_child_envelope_before_nested_delegation() {
        use super::super::{agent, coordinator, team};

        let parent = envelope::WorkerEnvelope {
            principal: "root".to_string(),
            paths: vec![envelope::PathScope::new("repo")],
            tools: envelope::ToolSet::all(),
            network: true,
            destructive: true,
            delegation_depth: 2,
            expires_at: u64::MAX,
            token_budget: Some(1_000),
        };
        let requested = envelope::WorkerEnvelope::requested(
            &parent,
            "root/child".to_string(),
            &["repo/src".to_string()],
            true,
            false,
            None,
            Some(500),
        );
        let child = envelope::WorkerEnvelope::narrow(&parent, &requested).expect("narrow child");
        let inherited = envelope::canonical_json(&parent).expect("parent json");
        let base = |key: &str| (key == agent::ENVELOPE_ENV).then(|| inherited.clone());
        let installed = agent::envelope_env(
            &base,
            Some(envelope::canonical_json(&child).expect("child json")),
            Some(child.principal.clone()),
        );
        let observed = agent::resolve_parent_envelope(&Default::default(), &installed)
            .expect("nested worker reads envelope");
        assert_eq!(observed, child);
        assert_eq!(observed.delegation_depth, 1);
        assert!(!observed.network);
        assert_eq!(observed.paths, [envelope::PathScope::new("repo/src")]);

        let first = coordinator::check(&coordinator::Bounds {
            parent_role: team::COORDINATOR,
            child_role: team::IMPLEMENTER,
            depth: observed.delegation_depth,
            cancelled: false,
            requested_write: true,
            manifest: None,
            plan: None,
        })
        .expect("one nested worker may launch");
        assert_eq!(first.depth, 0);
        assert_eq!(
            coordinator::check(&coordinator::Bounds {
                parent_role: team::COORDINATOR,
                child_role: team::IMPLEMENTER,
                depth: first.depth,
                cancelled: false,
                requested_write: true,
                manifest: None,
                plan: None,
            }),
            Err(coordinator::Refusal::DepthExhausted)
        );
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

        let legacy = permit::acquire_writer(&state, 4, "session legacy: claude", &tree, None)
            .expect("the legacy worker takes the tree");
        let native =
            permit::acquire_writer(&state, 4, "session nativeone: native/fast", &tree, None);
        assert!(
            native.is_err(),
            "a native worker must not hold a checkout a legacy worker already writes"
        );
        drop(legacy);
        let native =
            permit::acquire_writer(&state, 4, "session nativeone: native/fast", &tree, None)
                .expect("released");
        assert!(
            permit::acquire_writer(&state, 4, "session legacy2: claude", &tree, None).is_err(),
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

    #[test]
    fn native_launch_refusal_releases_live_coordinator_claim() {
        use super::super::task;

        for failure in ["route", "budget", "writer"] {
            let dir = tempfile::tempdir().expect("tempdir");
            let state = StateDir::from_root(dir.path().join("state"));
            let repo = dir.path().join("repo");
            std::fs::create_dir_all(&repo).expect("repo");
            let slug = super::super::state::repo_slug(&repo);
            let task_id = format!("issue575-{failure}");
            task::append_event(
                &state,
                &slug,
                &task::Event::Created {
                    id: task_id.clone(),
                    repo_slug: slug.clone(),
                    title: failure.to_string(),
                    brief: "must not strand claim".to_string(),
                    parents: Vec::new(),
                    group_id: None,
                    workdir: None,
                    at: 1,
                },
            )
            .expect("create card");
            let pid = std::process::id();
            task::claim_locked(
                &state,
                &slug,
                &task_id,
                "live-coordinator",
                pid,
                super::super::sessions::process_start_secs(pid),
                &task::local_host(),
                2,
                task::DEFAULT_CLAIM_TTL_SECS,
            )
            .expect("claim")
            .expect("card")
            .expect("first claimant");

            let mut args = args_for("native");
            args.task = Some(task_id.clone());
            refuse(
                (&state, &repo),
                &args,
                &mut Vec::new(),
                &repo,
                None,
                2,
                failure.to_string(),
            )
            .expect("refusal receipt");

            let reclaimed = task::claim_locked(
                &state,
                &slug,
                &task_id,
                "replacement",
                pid,
                super::super::sessions::process_start_secs(pid),
                &task::local_host(),
                3,
                task::DEFAULT_CLAIM_TTL_SECS,
            )
            .expect("claim")
            .expect("card");
            assert!(
                reclaimed.is_ok(),
                "{failure} refusal stranded the live claim"
            );
        }
    }
}
