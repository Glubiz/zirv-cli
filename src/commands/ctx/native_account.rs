//! Issue #554: what one native request owes the shared scheduling machinery,
//! in one place for every path that makes one.
//!
//! There are three ways a native request reaches a provider -- a delegated
//! worker (`native_worker`), the operator's own dashboard-hosted pane
//! (`dash::native_pane`) and a headless `zirv ctx exec` run
//! (`runtime::native::run_session`) -- and all three owe the same four things:
//!
//! 1. **admission** through the SHARED allocator ([`native_placement`]), so a
//!    native route is placed against the same `CapacitySnapshot` a harness
//!    delegation is, with capacity keyed by billing pool;
//! 2. **health** folded into the persistent breaker in the scope the loop
//!    already decided ([`record_route_health`]) -- a rate limit, a context
//!    overflow, a refusal and a cancellation reach no breaker at all;
//! 3. **reservation and settlement** against the route's BILLING POOL, on
//!    every token the provider metered rather than the completion alone;
//! 4. **spend**: one `log::Delegation` row, which is the ledger
//!    `zirv ctx spend` reads.
//!
//! Lifted out of `native_worker` rather than reimplemented per path: three
//! copies of an accounting rule is how two of them end up disagreeing about
//! what an account actually spent.

use std::path::Path;

use super::agent::delegation_outcome;
use super::config::CtxConfig;
use super::permit::WorkerMode;
use super::runtime::RuntimeKind;
use super::runtime::native::NativeFinalStatus;
use super::state::StateDir;

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
pub(crate) fn native_placement(
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
    /// The delegation mode, for a delegated worker. `None` for a seat: a
    /// seat is not a delegation and has no mode to report.
    pub mode: Option<WorkerMode>,
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
            mode: settlement.mode,
            task_class: None,
            principal: settlement.principal,
            envelope_sha256: settlement.envelope_sha256,
        },
    );
    settled_total
}

/// Every token the provider metered for one run.
pub(crate) fn total_tokens(usage: &super::provider::adapter::ProviderUsage) -> u64 {
    usage
        .input_tokens
        .saturating_add(usage.cache_creation_input_tokens)
        .saturating_add(usage.cache_read_input_tokens)
        .saturating_add(usage.output_tokens)
}

/// Folds one finished native run's outcome into the persistent route-health
/// breaker (issue #554). The SCOPE was decided purely by the loop
/// (`NativeFinalStatus::failure_routing`); this is the durable write.
pub(crate) fn record_route_health(
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

/// Admission, reservation and settlement for one native SEAT turn -- a
/// dashboard-hosted pane or a headless `zirv ctx exec` run (issue #554).
///
/// A seat is not a delegation: there is no worker handle, no work group and
/// no delegation mode, so those fields are absent rather than invented. What
/// it does owe is identical to a delegated worker's: the pool's ledger sees
/// every token the provider metered, the breaker sees the outcome in the
/// scope the loop decided, and `zirv ctx spend` sees the row.
pub(crate) fn settle_seat_turn(
    state: &StateDir,
    cfg: &CtxConfig,
    status: &NativeFinalStatus,
    reservation: Option<&(String, String)>,
    parent_session: Option<&str>,
) -> u64 {
    settle_native_run(
        state,
        cfg,
        status,
        &Settlement {
            reservation,
            group: None,
            reserved_ceiling: None,
            parent_session,
            principal: "seat",
            envelope_sha256: None,
            mode: None,
            exit_code: status.exit_code,
        },
    )
}

/// Holds an estimate against the route's billing pool for one turn about to
/// run, returning `(pool, reservation id)` for [`settle_seat_turn`].
///
/// Best-effort by construction: a ledger that cannot be written must never be
/// why a session refuses to run, which is the same posture every other
/// state-dir write in this codebase takes. `None` simply means this turn
/// settles nothing -- it never means the turn is refused.
pub(crate) fn reserve_seat_turn(
    state: &StateDir,
    billing_pool: &str,
    session: &str,
    estimate: u64,
    now: u64,
) -> Option<(String, String)> {
    super::reservation::reserve_within(state, billing_pool, session, estimate, None, now)
        .ok()
        .and_then(Result::ok)
        .map(|reservation| (billing_pool.to_string(), reservation.id))
}

#[cfg(test)]
mod tests {
    use super::*;

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
                mode: Some(WorkerMode::Writing),
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
}
