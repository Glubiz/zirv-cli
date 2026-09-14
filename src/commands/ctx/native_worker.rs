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

/// What the shared allocator said about this native route (issue #554).
#[derive(Debug)]
pub(crate) struct NativePlacement {
    /// The route the allocator would run this unit on, when it has an
    /// opinion. Reported, never acted on as a silent reroute: an operator's
    /// configured route is not swapped for another vendor's billing behind
    /// their back (#487 item 6).
    pub selected: Option<String>,
    /// Why the requested route may not take the work at all: an open breaker
    /// on its endpoint, credential or model. `None` admits.
    pub refusal: Option<String>,
}

/// Places one native request through the SHARED allocator (issue #554).
///
/// `None` when there is no native configuration to build a row from, which
/// is exactly the pre-#554 behaviour: nothing placed, nothing refused. A
/// configured route the allocator excludes on HEALTH carries a typed
/// refusal naming the breaker that stopped the work; see the comment on the
/// refusal below for why a capacity exclusion is reported but not enforced.
fn native_placement(
    state: &StateDir,
    cfg: &CtxConfig,
    repo: &Path,
    route_id: &super::provider::RouteId,
    now: u64,
) -> Option<NativePlacement> {
    use super::allocator;

    let home = crate::utils::home_dir().ok()?;
    let native = super::provider::config::NativeConfig::load(&home, repo).ok()??;
    let offers = super::route::offers_from_config(&native);
    let requested = route_id.as_ref().to_string();
    let offer = offers.iter().find(|offer| offer.route == requested)?;
    let demand = super::route::Demand {
        // What the seat already spends, plus local runtimes: a placement may
        // not move subscription work onto metered API credit on its own
        // (#487 item 6), and this route's own posture is what it is
        // authorized for.
        authorized_billing: [offer.billing, super::route::BillingPosture::Local]
            .into_iter()
            .collect(),
        preferred: Some(requested.clone()),
        ..super::route::Demand::default()
    };
    let snapshot = super::fallback::capacity_snapshot_with_native(state, cfg, now, None, &offers);
    let unit = allocator::WorkUnit {
        id: requested.clone(),
        requested: requested.clone(),
        bounds: super::fallback::TaskBounds {
            tokens: None,
            tool_calls: None,
        },
        expected_tokens: 0,
        needs_tool_call_counting: false,
        source_model: offer.identity.model.clone(),
        source_model_explicit: true,
        delegation: true,
        demand,
    };
    let placement = allocator::place(&snapshot, cfg, &unit, &[], &|name| {
        snapshot
            .harness(name)
            .and_then(|row| row.identity.model.clone())
    });
    // Only an UNHEALTHY verdict refuses admission. Every other exclusion the
    // allocator reports for a native row today is a capacity reading it does
    // not have yet: a native pool has no per-minute window of its own, so it
    // ranks `Unknown`, which for a harness means "prefer someone else" and
    // for a native route would mean "never run at all". The breaker is a
    // different fact -- an endpoint the provider just refused, a credential
    // it rejected, a model this account may not use -- and all three are
    // durable evidence that sending the request now buys a second failure.
    let refusal = placement
        .exclusions
        .iter()
        .find(|(name, why)| {
            name.eq_ignore_ascii_case(&requested)
                && matches!(why, allocator::Exclusion::Unhealthy(_))
        })
        .map(|(name, why)| format!("native route `{name}` is not healthy right now: {why:?}"));
    // The requested route is the placement when nothing excluded it on
    // health: an operator's configured route is never silently swapped for
    // another vendor's billing (#487 item 6), so `selected` reports what the
    // allocator ranked rather than authorizing a reroute.
    let selected = placement
        .selected
        .map(|candidate| candidate.name)
        .or_else(|| refusal.is_none().then(|| requested.clone()));
    Some(NativePlacement { selected, refusal })
}

/// Everything one finished native run needs to close out its accounting
/// that is not already on the run's own [`NativeFinalStatus`].
pub(crate) struct Settlement<'a> {
    /// `(billing pool, reservation id)` -- the pool's ledger this run drew
    /// from, keyed by POOL and not by vendor (issue #554).
    pub reservation: Option<&'a (String, String)>,
    pub group: Option<&'a str>,
    pub reserved_ceiling: Option<u64>,
    pub parent_session: Option<&'a str>,
    pub principal: &'a str,
    pub envelope_sha256: Option<&'a str>,
    pub mode: WorkerMode,
    pub exit_code: i32,
}

/// Closes out one finished native run's accounting, and returns the total it
/// settled (issue #554).
///
/// Four things happen here, in one place so they can never disagree:
///
/// 1. the billing-pool reservation is settled with the TOTAL usage -- prompt,
///    cache write, cache read and completion. Settling on the completion
///    alone under-reported what the account actually spent by however much
///    context the run carried, which on a long session is most of it;
/// 2. the work-group ledger settles the same total;
/// 3. the persistent route breaker folds this run's outcome in, in the scope
///    the loop already decided it belongs to -- so a rate limit, a context
///    overflow, a refusal and a cancellation reach no breaker at all;
/// 4. one `log::Delegation` row is appended, which is the ledger `zirv ctx
///    spend` reads. Without it a native worker's usage existed only inside
///    its own JSON status: real tokens, on a real account, invisible to
///    every spend surface zirv has.
pub(crate) fn settle_native_run(
    state: &StateDir,
    cfg: &CtxConfig,
    status: &super::runtime::native::NativeFinalStatus,
    settlement: &Settlement<'_>,
) -> u64 {
    let settled_total = total_tokens(&status.usage);
    if let Some((pool, id)) = settlement.reservation {
        let _ = super::reservation::settle(state, pool, id, settled_total);
    }
    if let Some(id) = settlement.group {
        let _ = super::group::settle_reservation(
            state,
            id,
            settlement.reserved_ceiling.unwrap_or(0),
            settled_total,
        );
    }
    record_route_health(state, cfg, status, super::state::now_secs());
    let _ = super::log::append_delegation(
        state,
        &super::log::Delegation {
            ts: super::state::now_secs(),
            session: &status.session,
            parent_session: settlement.parent_session.unwrap_or_default(),
            work_group_id: settlement.group,
            agent: RuntimeKind::Native.as_str(),
            model: Some(&status.configured_model),
            input_tokens: status.usage.input_tokens,
            cache_creation_input_tokens: status.usage.cache_creation_input_tokens,
            cache_read_input_tokens: status.usage.cache_read_input_tokens,
            output_tokens: status.usage.output_tokens,
            wall_ms: 0,
            exit_code: settlement.exit_code,
            outcome: delegation_outcome(settlement.exit_code),
            mode: Some(settlement.mode),
            task_class: None,
            principal: settlement.principal,
            envelope_sha256: settlement.envelope_sha256,
        },
    );
    settled_total
}

/// Every token the provider metered for one run.
fn total_tokens(usage: &super::provider::adapter::ProviderUsage) -> u64 {
    usage
        .input_tokens
        .saturating_add(usage.cache_creation_input_tokens)
        .saturating_add(usage.cache_read_input_tokens)
        .saturating_add(usage.output_tokens)
}

/// Folds one finished native run's outcome into the persistent route-health
/// breaker (issue #554). The SCOPE was decided purely by the loop
/// (`NativeFinalStatus::failure_routing`); this is the durable write.
fn record_route_health(
    state: &StateDir,
    cfg: &CtxConfig,
    status: &super::runtime::native::NativeFinalStatus,
    now: u64,
) {
    let identity = super::route::RouteIdentity {
        runtime: super::route::RuntimeKind::Native,
        provider: status.provider.clone(),
        endpoint: status.endpoint.clone(),
        credential: status.account.clone(),
        model: Some(status.configured_model.clone()),
        pool: status.billing_pool.clone(),
    };
    super::health_store::record_native_outcome(
        state,
        &identity,
        status.failure_routing.as_ref(),
        now,
        &cfg.fallback.effective_health(),
    );
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
    let placement = native_placement(state, cfg, &request.launch_repo, &route_id, now);
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
        match permit::acquire_writer(
            state,
            cfg.supervise.max_writers,
            &format!("session {child_short}: native/{route_id}"),
            &tree,
            // Issue #488 (review finding 1 follow-up, PR #535): a delegated
            // worker holds no seat of its own at this point -- the eventual
            // native session's own seat is created and stored later, inside
            // `runtime::native::run_session`, under a session identity
            // `NativeBackend::start` mints fresh (a random uuid/short,
            // generation 1) and never derived from `child_short`/
            // `worker_session` above. `seat::register`, the only reusable
            // registration primitive, also hardcodes `runtime:
            // RuntimeKind::Harness` for a brand-new record (issue #470), so
            // it cannot even correctly stand in for one. Pre-registering a
            // seat for `child_short` here would therefore be a seat this
            // delegated worker's own real native session never uses, orphaned
            // in state forever rather than swept the way a real seat is --
            // worse than the honest answer this env fence already gives (see
            // `seat::guard_from_env`'s own doc comment for the full list of
            // callers this reasoning applies to).
            None,
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
            // silently swaps its worker's provider for a fixture.
            provider: None,
            fixture_tools: None,
            task: args.task.clone(),
            writer: writer_permit.map(|permit| {
                Box::new(permit) as Box<dyn super::runtime::enforcement::WriterLease>
            }),
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
    let settled_total = settle_native_run(
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
            mode: args.mode,
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

    /// Issue #554 (spec #487 items 1-5 and 7): a native request goes through
    /// the SHARED allocator, the persistent health store, the billing pool's
    /// reservation ledger and the delegation ledger `zirv ctx spend` reads.
    ///
    /// Drives the production seams themselves -- `native_placement` and
    /// `settle_native_run`, the two functions `run` calls -- rather than a
    /// re-implementation, because the thing under test IS the wiring.
    #[test]
    fn native_requests_allocate_record_health_and_reconcile_pool_spend() {
        use crate::commands::ctx::health::{Phase, RouteKey, RouteScope};
        use crate::commands::ctx::provider::adapter::ProviderUsage;
        use crate::commands::ctx::runtime::native::NativeStatus;

        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let _home = crate::commands::ctx::testenv::HomeGuard::set(&home);
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        let state = StateDir::from_root(tmp.path().join("state"));
        let now = 1_700_000_000u64;

        // Two routes on ONE account (one billing pool, one balance) plus a
        // third on a second account at the same vendor (a separate pool).
        let native_toml = home.join(crate::utils::SCRIPT_DIR_NAME).join("native.toml");
        std::fs::create_dir_all(native_toml.parent().expect("parent")).expect("mkdir .zirv");
        std::fs::write(
            &native_toml,
            "schema=1\n\
             [account.work]\nprovider='anthropic'\ncredential='env:KEY'\n\
             [account.other]\nprovider='anthropic'\ncredential='env:OTHER'\n\
             [route.opus]\naccount='work'\nmodel='claude-opus-5'\n\
             [route.sonnet]\naccount='work'\nmodel='claude-sonnet-5'\n\
             [route.spare]\naccount='other'\nmodel='claude-opus-5'\n",
        )
        .expect("write native.toml");

        let mut cfg = CtxConfig::default();
        cfg.fallback.health.enabled = true;
        let policy = cfg.fallback.effective_health();
        let opus = crate::commands::ctx::provider::RouteId::new("opus").expect("route id");

        // 1. SUCCESS: the shared allocator places the requested native route,
        //    and admits it.
        let placement = native_placement(&state, &cfg, &repo, &opus, now)
            .expect("a configured native route is placed by the shared allocator");
        assert_eq!(
            placement.refusal, None,
            "a healthy configured route is admitted"
        );
        assert_eq!(
            placement.selected.as_deref(),
            Some("opus"),
            "the allocator's own choice is the operator's configured route"
        );

        // 2. Settlement: TOTAL usage against the POOL's ledger, and one
        //    delegation row for `zirv ctx spend`.
        let usage = ProviderUsage {
            input_tokens: 1_000,
            cache_creation_input_tokens: 200,
            cache_read_input_tokens: 30,
            output_tokens: 4,
            reasoning_tokens: None,
        };
        let group = crate::commands::ctx::group::run_create(
            &state,
            &mut Vec::new(),
            &crate::commands::ctx::group::CreateArgs {
                scope: "native spend".to_string(),
                child_limit: 4,
                token_budget: None,
                deadline_secs: None,
                completion_contract: "n/a".to_string(),
                parent_session: None,
            },
            now,
        )
        .expect("create group");
        let reservation = crate::commands::ctx::reservation::reserve_within(
            &state,
            "work",
            "session-1",
            5_000,
            None,
            now,
        )
        .expect("reserve")
        .expect("within limit");
        let held = ("work".to_string(), reservation.id.clone());
        let ok = status_fixture(NativeStatus::Completed, usage.clone(), None);
        let settled = settle_native_run(
            &state,
            &cfg,
            &ok,
            &Settlement {
                reservation: Some(&held),
                group: Some(&group),
                reserved_ceiling: Some(5_000),
                parent_session: Some("parent01"),
                principal: "root/child",
                envelope_sha256: None,
                mode: WorkerMode::Writing,
                exit_code: 0,
            },
        );
        assert_eq!(
            settled, 1_234,
            "every token the provider metered is settled, not the completion alone"
        );
        assert_eq!(
            crate::commands::ctx::group::load(&state, &group)
                .expect("load")
                .expect("group")
                .spent_tokens,
            1_234,
            "the work group rolls up the same total"
        );
        assert!(
            state
                .reservations()
                .join(format!(
                    "{}.json",
                    crate::commands::ctx::state::provider_slug("work")
                ))
                .exists(),
            "the reservation is keyed by the BILLING POOL, never by the vendor"
        );
        let rows = crate::commands::ctx::log::read_delegations(&state, 10);
        let row = rows
            .iter()
            .find(|row| row.session == ok.session)
            .expect("a native run appears in the delegation ledger zirv ctx spend reads");
        assert_eq!(row.agent, "native");
        assert_eq!(row.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(row.input_tokens, 1_000);
        assert_eq!(row.cache_creation_input_tokens, 200);
        assert_eq!(row.cache_read_input_tokens, 30);
        assert_eq!(row.output_tokens, 4);
        let spend = crate::commands::ctx::spend::aggregate(
            &rows,
            crate::commands::ctx::spend::SpendDimension::Harness,
            &crate::commands::ctx::price::built_in_table(),
        );
        let native_row = spend
            .iter()
            .find(|row| row.key == "native")
            .expect("`zirv ctx spend --by harness` reports the native run");
        assert_eq!(native_row.input_tokens, 1_000);
        assert_eq!(native_row.output_tokens, 4);

        // 3. RATE EXHAUSTION: capacity, never a breaker. No health record is
        //    written at all, so a 429 cannot disable the route.
        let rate_limited = status_fixture(
            NativeStatus::Failed,
            usage.clone(),
            Some(crate::commands::ctx::route::FailureRouting::RateLimit {
                pool: "work".to_string(),
                retry_after_secs: Some(30),
            }),
        );
        record_route_health(&state, &cfg, &rate_limited, now);
        assert_eq!(
            crate::commands::ctx::health_store::load(
                &state,
                &RouteKey::scoped(RouteScope::Endpoint, "anthropic"),
                now,
            )
            .phase,
            Phase::Healthy,
            "a rate limit is capacity, not health evidence"
        );

        // 4. FAILURE/FALLBACK: an endpoint outage DOES trip the durable
        //    breaker, and the shared allocator then refuses the route.
        let failed = status_fixture(
            NativeStatus::Failed,
            usage,
            Some(crate::commands::ctx::route::FailureRouting::Endpoint {
                endpoint: "anthropic".to_string(),
                class: crate::commands::ctx::event::ProviderErrorClass::Server,
            }),
        );
        for _ in 0..policy.open_after_failures.max(1) {
            record_route_health(&state, &cfg, &failed, now);
        }
        let endpoint_key = RouteKey::scoped(RouteScope::Endpoint, "anthropic");
        assert!(
            matches!(
                crate::commands::ctx::health_store::load(&state, &endpoint_key, now).phase,
                Phase::Open { .. }
            ),
            "an endpoint outage is folded into a DURABLE breaker record, keyed per route scope"
        );
        assert!(
            crate::commands::ctx::health_store::native_admission(
                &state,
                &crate::commands::ctx::route::RouteIdentity {
                    runtime: crate::commands::ctx::route::RuntimeKind::Native,
                    provider: "anthropic".to_string(),
                    endpoint: "anthropic".to_string(),
                    credential: "work".to_string(),
                    model: Some("claude-opus-5".to_string()),
                    pool: "work".to_string(),
                },
                now,
                &policy,
            )
            .denied()
            .is_some(),
            "and the route's own admission is denied while the breaker is open"
        );
        let after = native_placement(&state, &cfg, &repo, &opus, now)
            .expect("the route is still configured");
        assert!(
            after.refusal.is_some(),
            "the shared allocator now refuses the request rather than sending it into an outage: \
             {after:?}",
        );
    }

    fn status_fixture(
        status: crate::commands::ctx::runtime::native::NativeStatus,
        usage: crate::commands::ctx::provider::adapter::ProviderUsage,
        failure_routing: Option<crate::commands::ctx::route::FailureRouting>,
    ) -> crate::commands::ctx::runtime::native::NativeFinalStatus {
        crate::commands::ctx::runtime::native::NativeFinalStatus {
            schema_version: 1,
            runtime: RuntimeKind::Native.as_str(),
            status,
            session: "11111111-2222-4333-8444-555555555555".to_string(),
            route: "opus".to_string(),
            provider: "anthropic".to_string(),
            endpoint: "anthropic".to_string(),
            account: "work".to_string(),
            billing_pool: "work".to_string(),
            configured_model: "claude-opus-5".to_string(),
            served_model: None,
            turns: 1,
            requests: 1,
            tool_calls: 0,
            usage,
            reconciliation: Default::default(),
            finish_reason: None,
            final_text: Some("done".to_string()),
            incomplete_tools: Vec::new(),
            outcome_unknown_tools: Vec::new(),
            queued_input: Vec::new(),
            limit: None,
            failure: None,
            failure_routing,
            blocked_reason: None,
            compactions: Vec::new(),
            compaction_decision: None,
            evidence: Vec::new(),
            exit_code: 0,
        }
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
