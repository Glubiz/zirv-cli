//! Issue #358 (task 2): pure cross-harness capacity planning.
//!
//! `fallback.rs` owns the only I/O (`capacity_snapshot`) and hands the result
//! here as a plain [`CapacitySnapshot`]. Everything in this module is a
//! function of that snapshot and `CtxConfig` alone -- no fs/clock/env/net,
//! the same purity precedent `rot.rs` documents for itself: identical inputs
//! give identical placements, every time, so a plan can be replayed,
//! diffed, or serialized for `zirv ctx status` without re-reading any state.

use serde::Serialize;

use super::config::CtxConfig;
pub use super::fallback::TaskBounds;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum HarnessState {
    Ready,
    Draining,
    HardBlocked,
    Unknown,
    Disabled,
}

impl HarnessState {
    /// Not yet called from production code: the `zirv ctx status` surface
    /// this feeds (issue #358, a later task) lands after this one. Kept
    /// `pub` and exercised by this module's own tests now, the same
    /// task-ordering shape `FallbackConfig::rollover_headroom_pct` already
    /// documents for itself.
    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Draining => "draining",
            Self::HardBlocked => "hard-blocked",
            Self::Unknown => "unknown",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WindowReading {
    pub window: String,
    pub used_pct: f64,
    pub headroom_pct: f64,
    pub resets_at: u64,
    pub observed_at: u64,
    pub age_secs: u64,
    pub source: String,
    pub stale: bool,
    pub limit_reached: bool,
    pub overage_covered: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderCapacity {
    pub provider: String,
    pub windows: Vec<WindowReading>,
    /// Index into `windows` of the most restrictive (binding) reading, per
    /// `pace`'s own binding rule -- `None` when no window is currently
    /// usable.
    pub binding: Option<usize>,
    pub hard_refused: bool,
    pub reserved_tokens: u64,
    pub degraded: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HarnessCapacity {
    pub name: String,
    pub provider: String,
    pub enabled: bool,
    pub ready: bool,
    pub unready_reason: Option<String>,
    pub capacity_small: bool,
    pub counts_tool_calls: bool,
    pub active: u32,
    pub max_active: Option<u32>,
    pub reserve_headroom_pct: f64,
    pub state: HarnessState,
    pub state_reason: String,
    /// Issue #455: what the route-health breaker says about this harness
    /// (`health_store::harness_admission`, resolved once by the snapshot).
    /// `Allow` whenever the policy is off, so this module's behaviour is
    /// unchanged by default.
    pub health: super::health::Admission,
    /// Issue #487 (N18): what this route IS -- runtime, provider, endpoint,
    /// credential, model and billing pool. A harness row states the identity
    /// this module always implied (`RouteIdentity::harness`), so nothing
    /// about harness placement changes; a native row states a real one, and
    /// its `pool` is what `CapacitySnapshot::pool` looks capacity up by.
    pub identity: super::route::RouteIdentity,
    /// What this route can do and how it is paid for, for the eligibility
    /// gate that runs BEFORE ranking. `None` for a route that declares
    /// nothing, which is every harness row today: an undeclared offer is not
    /// gated, exactly as before.
    pub offer: Option<super::route::RouteOffer>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CapacitySnapshot {
    pub taken_at: u64,
    pub providers: Vec<ProviderCapacity>,
    pub harnesses: Vec<HarnessCapacity>,
    pub degraded: bool,
}

impl CapacitySnapshot {
    pub fn provider(&self, name: &str) -> Option<&ProviderCapacity> {
        self.providers
            .iter()
            .find(|p| p.provider.eq_ignore_ascii_case(name))
    }

    pub fn harness(&self, name: &str) -> Option<&HarnessCapacity> {
        self.harnesses
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
    }

    /// The capacity one route actually draws on (issue #487, item 1).
    ///
    /// `providers` is keyed by BILLING POOL, not by vendor: a harness row's
    /// pool is its provider, so every existing lookup resolves exactly as
    /// before, while two native routes on one account resolve to the one
    /// `ProviderCapacity` -- and therefore to one `reserved_tokens` and one
    /// set of windows, which is what stops two routes on one balance being
    /// ranked as twice the capacity. Two accounts at one vendor carry
    /// different pool ids and stay independent.
    pub fn pool(&self, harness: &HarnessCapacity) -> Option<&ProviderCapacity> {
        self.provider(&harness.identity.pool)
            .or_else(|| self.provider(&harness.provider))
    }

    /// Every route drawing on one billing pool, `route` included (issue
    /// #487, criterion 1). More than one name here means those routes share
    /// a balance and must never be ranked as separate capacities; a route
    /// alone in its pool is independent of every other route at the same
    /// vendor.
    pub fn pool_siblings(&self, route: &HarnessCapacity) -> Vec<&HarnessCapacity> {
        self.harnesses
            .iter()
            .filter(|other| other.identity.shares_pool(&route.identity))
            .collect()
    }

    /// Every route an outage on `route`'s endpoint would take with it,
    /// `route` included (criterion 2). Sharing an endpoint is the ONLY
    /// coupling a transport or server failure creates: a sibling account on
    /// the same host is denied with it, a route on another host is not, and
    /// a credential failure couples nothing at all.
    pub fn endpoint_siblings(&self, route: &HarnessCapacity) -> Vec<&HarnessCapacity> {
        self.harnesses
            .iter()
            .filter(|other| other.identity.shares_endpoint(&route.identity))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkUnit {
    pub id: String,
    pub requested: String,
    pub bounds: TaskBounds,
    pub expected_tokens: u64,
    pub needs_tool_call_counting: bool,
    pub source_model: Option<String>,
    pub source_model_explicit: bool,
    pub delegation: bool,
    /// Issue #487 (item 5): what this unit needs from whatever route takes
    /// it -- capabilities, context room and authorized billing. Checked
    /// BEFORE ranking, so a route that could never have run the work is
    /// excluded by its own reason rather than reported as outranked.
    /// `Demand::default()` constrains nothing, which is every existing
    /// caller.
    pub demand: super::route::Demand,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Exclusion {
    Disabled,
    Unready(String),
    CapacitySmall,
    NoToolCallCounting,
    HardBlocked,
    Draining(String),
    AtMaxActive {
        active: u32,
        max: u32,
    },
    InsufficientHeadroom {
        have: f64,
        need: f64,
    },
    UnknownHeadroomOptedOut,
    NoEquivalentModel,
    Visited,
    Excluded,
    /// Issue #455: the route-health breaker is open (or the route is
    /// `Unavailable`) for this harness. Carries the breaker's own reason so
    /// `PoolView.exclusions` and a park message name the failures, the
    /// window and the estimated retry, rather than reporting a bare
    /// `HardBlocked` that reads as a usage refusal.
    Unhealthy(String),
    /// Issue #487 (item 5): the route cannot run this work at all -- policy
    /// refuses it, it lacks a required capability, its context window cannot
    /// hold the prompt, or its billing posture is not one this work is
    /// authorized for. Judged before capacity and before ranking, because
    /// ranking a route that could never take the task is how "outranked"
    /// ends up naming a route the work was never eligible for.
    Ineligible(super::route::Ineligible),
    /// This candidate cleared every eligibility check but lost to `by`,
    /// whose own projected headroom (`projected_headroom_pct`) was greater
    /// (or tied and earlier in `cfg.fallback.order`). Issue #358 follow-up:
    /// `Placement.exclusions` must name a reason for every candidate it
    /// considered, eligible losers included, not just the disqualified
    /// ones.
    Outranked {
        by: String,
        projected_headroom_pct: f64,
    },
}

impl Exclusion {
    /// Same task-ordering note as `HarnessState::as_str`: the human-facing
    /// surface this labels for lands in a later issue #358 task.
    #[allow(dead_code)]
    pub fn label(&self) -> String {
        match self {
            Self::Disabled => "disabled".to_string(),
            Self::Unready(reason) => format!("not ready: {reason}"),
            Self::CapacitySmall => "capacity-limited harness cannot take this task".to_string(),
            Self::NoToolCallCounting => "does not count tool calls".to_string(),
            Self::HardBlocked => "hard blocked by usage".to_string(),
            Self::Draining(reason) => format!("draining: {reason}"),
            Self::AtMaxActive { active, max } => format!("at max_active ({active}/{max})"),
            Self::InsufficientHeadroom { have, need } => {
                format!("insufficient headroom ({have:.1}% < {need:.1}%)")
            }
            Self::UnknownHeadroomOptedOut => {
                "unknown headroom opted out (unknown_headroom_pct=0)".to_string()
            }
            Self::NoEquivalentModel => "no equivalent model available".to_string(),
            Self::Visited => "already visited in this order".to_string(),
            Self::Excluded => "explicitly excluded".to_string(),
            Self::Unhealthy(reason) => reason.clone(),
            Self::Ineligible(why) => why.label(),
            Self::Outranked {
                by,
                projected_headroom_pct,
            } => format!("outranked by {by} ({projected_headroom_pct:.1}% projected headroom)"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Candidate {
    pub name: String,
    pub model: Option<String>,
    pub headroom_pct: f64,
    pub projected_headroom_pct: f64,
    pub assumed: bool,
    /// Audit finding G2: the reading this candidate was ranked on is real
    /// but older than `pace.collector_max_age_secs`, so nothing binds it.
    /// Distinct from `assumed` (no reading at all, `unknown_headroom_pct`
    /// stood in for one) and reported as `stale` rather than `unknown`.
    pub stale: bool,
    pub binding_window: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Placement {
    pub unit: String,
    pub selected: Option<Candidate>,
    pub keep_requested: bool,
    pub exclusions: Vec<(String, Exclusion)>,
}

/// The state ladder: disabled/unready first (the harness cannot run at all),
/// then a confirmed hard refusal (it can run, but usage says not now), then
/// draining (it could accept work, but not enough is left for a new unit or
/// it is already at its concurrency cap), then unknown (no usable reading to
/// judge by), and only then ready.
pub fn classify(
    harness: &HarnessCapacity,
    provider: &ProviderCapacity,
    cfg: &CtxConfig,
) -> (HarnessState, String) {
    if !harness.enabled {
        return (HarnessState::Disabled, "harness disabled".to_string());
    }
    if !harness.ready {
        let reason = harness
            .unready_reason
            .clone()
            .unwrap_or_else(|| "not ready".to_string());
        return (HarnessState::Disabled, reason);
    }
    // Issue #455: read before the usage ladder below. A route that cannot be
    // connected to is unusable at any headroom, so a denied breaker is a
    // hard block regardless of what the usage windows say -- and the reason
    // travels with it, which is what `zirv ctx status` and a park message
    // then show instead of a bare "hard blocked".
    if let Some(reason) = harness.health.denied() {
        return (HarnessState::HardBlocked, reason.to_string());
    }
    if provider.hard_refused {
        return (
            HarnessState::HardBlocked,
            "binding window at or above the hard spawn ceiling".to_string(),
        );
    }
    if let Some(max) = harness.max_active
        && harness.active >= max
    {
        return (
            HarnessState::Draining,
            format!("at max_active ({}/{max})", harness.active),
        );
    }
    let projected = projected_headroom(provider, cfg, 0);
    if let Some(p) = projected
        && p <= harness.reserve_headroom_pct
    {
        return (
            HarnessState::Draining,
            format!(
                "projected headroom {p:.1}% at or below reserve {:.1}%",
                harness.reserve_headroom_pct
            ),
        );
    }
    if provider.binding.is_none() {
        return (
            HarnessState::Unknown,
            "no usable binding usage reading".to_string(),
        );
    }
    // A half-open breaker is `Ready`, not blocked: exactly one trial is the
    // whole point. The suffix is the only place a human sees that this
    // launch is a probe rather than an ordinary placement.
    if harness.health.is_trial() {
        return (
            HarnessState::Ready,
            "ready (route health trial: one attempt admitted after a cooldown)".to_string(),
        );
    }
    // Slice A: a degraded route is `Ready` -- it answers, just worse than it
    // should. The reason rides on the state so `zirv ctx status` explains why
    // this harness keeps losing ties it used to win.
    if let Some(reason) = harness.health.degraded() {
        return (HarnessState::Ready, format!("ready (degraded: {reason})"));
    }
    (HarnessState::Ready, "ready".to_string())
}

fn window_budget(window_name: &str, cfg: &CtxConfig) -> u64 {
    match window_name {
        "five_hour" => cfg.pace.five_hour_budget_tokens,
        "seven_day" => cfg.pace.seven_day_budget_tokens,
        _ => 0,
    }
}

pub(super) fn window_projected_headroom(
    window: &WindowReading,
    cfg: &CtxConfig,
    reserved: u64,
    extra: u64,
) -> f64 {
    let budget = window_budget(&window.window, cfg);
    let headroom = if budget > 0 {
        window.headroom_pct - ((reserved + extra) as f64 / budget as f64) * 100.0
    } else {
        window.headroom_pct
    };
    headroom.clamp(0.0, 100.0)
}

/// The binding window's headroom, minus this provider's already-reserved
/// tokens plus `extra_tokens`, expressed as a percentage of that window's
/// configured token budget. `None` when the provider has no binding window
/// at all. When the binding window has no configured budget, the raw
/// headroom is returned unchanged (there is nothing to convert tokens into).
pub fn projected_headroom(
    provider: &ProviderCapacity,
    cfg: &CtxConfig,
    extra_tokens: u64,
) -> Option<f64> {
    let idx = provider.binding?;
    let window = provider.windows.get(idx)?;
    Some(window_projected_headroom(
        window,
        cfg,
        provider.reserved_tokens,
        extra_tokens,
    ))
}

/// Issue #487 (items 2 and 7): every capacity dimension this route can run
/// out of, as one list, each number labelled with where it came from.
///
/// The usage windows a provider reports are all `SubscriptionWindow`
/// readings -- that is the one dimension a harness has ever had. A native
/// route's offer adds the other four (requests/minute, tokens/minute,
/// concurrent requests, a configured spend ceiling), which bind
/// independently and are therefore NOT folded into the window figure.
/// Dimensions the provider has never reported on are included as unknowns,
/// because an unmeasured dimension is the one most likely to be binding and
/// omitting it would report exactly the free-capacity illusion this list
/// exists to prevent.
///
/// Diagnostic only, for now: `place`'s ranking still reads the windows
/// through `projected_headroom`, so this adds a readout without moving a
/// placement. It is the shape the ranking grows into once native routes
/// report per-minute limits (see this issue's design note).
pub fn route_dimensions(
    harness: &HarnessCapacity,
    provider: Option<&ProviderCapacity>,
    now: u64,
    cfg: &CtxConfig,
) -> Vec<super::route::Headroom> {
    let policy = estimate_policy(cfg);
    let mut out: Vec<super::route::Headroom> = dimension_readings(harness, provider)
        .iter()
        .map(|reading| super::route::headroom(reading, now, &policy))
        .collect();
    out.sort_by(|a, b| {
        a.pct
            .partial_cmp(&b.pct)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.dimension.cmp(&b.dimension))
    });
    out
}

/// The dimension that actually binds this route, with its provenance (issue
/// #487, item 7). `None` only for a route with no dimensions at all, which
/// [`dimension_readings`] never produces.
pub fn route_binding(
    harness: &HarnessCapacity,
    provider: Option<&ProviderCapacity>,
    now: u64,
    cfg: &CtxConfig,
) -> Option<super::route::Headroom> {
    super::route::binding(
        &dimension_readings(harness, provider),
        now,
        &estimate_policy(cfg),
    )
}

/// The two operator knobs that already govern "what do we do about a reading
/// we do not have", handed to the pure model.
fn estimate_policy(cfg: &CtxConfig) -> super::route::EstimatePolicy {
    super::route::EstimatePolicy {
        unknown_headroom_pct: cfg.fallback.unknown_headroom_pct,
        max_age_secs: cfg.pace.collector_max_age_secs,
        ..super::route::EstimatePolicy::default()
    }
}

/// Every dimension this route can run out of, as raw readings.
///
/// The usage windows a provider reports are all `SubscriptionWindow`
/// readings -- that is the one dimension a harness has ever had. A native
/// route's offer adds the other four, which bind independently and are
/// therefore NOT folded into the window figure. A dimension nothing has
/// reported on is still listed, as an unknown: omitting it would report
/// exactly the free-capacity illusion this list exists to prevent, since an
/// unmeasured dimension is the one most likely to be binding.
fn dimension_readings(
    harness: &HarnessCapacity,
    provider: Option<&ProviderCapacity>,
) -> Vec<super::route::Reading> {
    let mut readings: Vec<super::route::Reading> = Vec::new();
    if let Some(provider) = provider {
        for window in &provider.windows {
            // `used_pct` is already a percentage, so the reading is stated
            // against a notional 100 rather than a token budget: what this
            // list adds is the dimension's identity and its provenance, not
            // a second arithmetic for a number `pace` already computed.
            let used = window.used_pct.clamp(0.0, 100.0).round() as u64;
            readings.push(super::route::Reading::new(
                super::route::Dimension::SubscriptionWindow,
                Some(100),
                used,
                window.observed_at,
            ));
        }
    }
    if let Some(offer) = &harness.offer {
        readings.extend(offer.readings.iter().cloned());
    }
    for dimension in [
        super::route::Dimension::RequestsPerMinute,
        super::route::Dimension::TokensPerMinute,
        super::route::Dimension::ConcurrentRequests,
        super::route::Dimension::SubscriptionWindow,
        super::route::Dimension::SpendCeiling,
    ] {
        if !readings.iter().any(|r| r.dimension == dimension) {
            readings.push(super::route::Reading::unknown(dimension));
        }
    }
    readings
}

fn binding_headroom(provider: &ProviderCapacity) -> (f64, Option<String>) {
    match provider.binding.and_then(|i| provider.windows.get(i)) {
        Some(w) => (w.headroom_pct, Some(w.window.clone())),
        None => (0.0, None),
    }
}

/// The reading a provider is RANKED on. The binding window when one binds;
/// otherwise (audit finding G2) the tightest reading still stored for this
/// provider -- real, still-live numbers that `pace::binding` dropped only
/// because nothing has refreshed them inside `collector_max_age_secs`.
/// Codex is the motivating case: its rollout files are written only during a
/// turn, so a provider sitting on 47% used goes "unknown" 15 minutes after
/// the last one and ranked at the blanket `unknown_headroom_pct` instead.
///
/// Ranking only. `classify` still keys `HarnessState::Unknown` off
/// `provider.binding` alone, so a reading nothing binds can never authorise
/// a hard gate -- it can only order candidates that were already admissible.
pub fn ranking_window(provider: &ProviderCapacity) -> Option<&WindowReading> {
    provider
        .binding
        .and_then(|i| provider.windows.get(i))
        .or_else(|| {
            provider.windows.iter().min_by(|a, b| {
                a.headroom_pct
                    .partial_cmp(&b.headroom_pct)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        })
}

/// Every window that has a configured budget must have enough projected
/// headroom for `bounds`, not merely the single binding window -- a task
/// that fits the five-hour window can still be rejected by a tighter
/// seven-day ceiling.
fn fits_all_windows(
    provider: &ProviderCapacity,
    bounds: &TaskBounds,
    cfg: &CtxConfig,
    extra_tokens: u64,
) -> bool {
    provider
        .windows
        .iter()
        .all(|w| match bounds.required_headroom_pct(cfg, &w.window) {
            Some(required) => {
                window_projected_headroom(w, cfg, provider.reserved_tokens, extra_tokens)
                    >= required
            }
            None => true,
        })
}

/// The single worst-shortfall window, for a human-readable exclusion reason:
/// the window with the largest gap between what `bounds` requires and what
/// is actually projected to be left.
fn worst_window_shortfall(
    provider: &ProviderCapacity,
    bounds: &TaskBounds,
    cfg: &CtxConfig,
    extra_tokens: u64,
) -> (f64, f64) {
    provider
        .windows
        .iter()
        .filter_map(|w| {
            let required = bounds.required_headroom_pct(cfg, &w.window)?;
            let have = window_projected_headroom(w, cfg, provider.reserved_tokens, extra_tokens);
            Some((have, required, required - have))
        })
        .max_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(have, need, _)| (have, need))
        .unwrap_or((0.0, 0.0))
}

/// Why the requested harness itself did not qualify under rule (a) of
/// [`place`], recorded so a `Placement` with no selection still explains
/// every candidate it considered -- including the one that was asked for.
fn requested_unfit_reason(
    harness: &HarnessCapacity,
    provider: &ProviderCapacity,
    cfg: &CtxConfig,
    unit: &WorkUnit,
) -> Exclusion {
    // Issue #455: named before the state ladder, so a health denial reports
    // its own reason rather than the generic `HardBlocked` label `classify`
    // folded it into.
    if let Some(reason) = harness.health.denied() {
        return Exclusion::Unhealthy(reason.to_string());
    }
    match harness.state {
        HarnessState::Disabled => Exclusion::Disabled,
        HarnessState::HardBlocked => Exclusion::HardBlocked,
        HarnessState::Draining => Exclusion::Draining(harness.state_reason.clone()),
        HarnessState::Unknown => {
            if cfg.fallback.unknown_headroom_pct <= 0.0 {
                Exclusion::UnknownHeadroomOptedOut
            } else {
                Exclusion::InsufficientHeadroom {
                    have: cfg.fallback.unknown_headroom_pct,
                    need: cfg
                        .fallback
                        .min_candidate_headroom_pct
                        .max(unit.bounds.required_unknown_headroom_pct(cfg)),
                }
            }
        }
        HarnessState::Ready => {
            let (have, need) =
                worst_window_shortfall(provider, &unit.bounds, cfg, unit.expected_tokens);
            Exclusion::InsufficientHeadroom { have, need }
        }
    }
}

/// Issue #487 (item 5): whether this route is disqualified from the task
/// itself, as opposed to from its current capacity or health. `None` for a
/// route that declares no offer, which leaves every existing harness row
/// exactly as it was.
fn ineligible(harness: &HarnessCapacity, unit: &WorkUnit) -> Option<super::route::Ineligible> {
    let offer = harness.offer.as_ref()?;
    super::route::eligible(offer, &unit.demand).err()
}

/// Places one [`WorkUnit`], deterministically: identical `snapshot`/`cfg`/
/// `unit`/`exclude` inputs always produce the identical `Placement`.
///
/// Rule (a): the requested harness is kept when it is `Ready` and fits every
/// budgeted window with `unit.expected_tokens` added to whatever this
/// provider already has reserved -- this reproduces today's "no reroute"
/// outcome exactly.
///
/// Rule (b): otherwise `cfg.fallback.order` is walked in order, skipping the
/// requested harness (already tried) and anything in `exclude`; the
/// candidate with the greatest projected headroom wins, ties broken by
/// `order` position. An `Unknown` harness may still win, using the
/// configured `unknown_headroom_pct` as an assumed headroom (never reduced
/// by reservations, since there is no window to compute a projection
/// against); `unknown_headroom_pct <= 0` opts every `Unknown` harness out.
pub fn place(
    snapshot: &CapacitySnapshot,
    cfg: &CtxConfig,
    unit: &WorkUnit,
    exclude: &[&str],
    models: &dyn Fn(&str) -> Option<String>,
) -> Placement {
    let mut exclusions: Vec<(String, Exclusion)> = Vec::new();
    // Slice A: the requested harness's own fit, held back rather than
    // returned when it is DEGRADED. Rule (a) may keep a degraded route only
    // once the order walk below has proved there is no healthy one to take
    // the work instead -- a degraded route is reduced, never excluded.
    let mut degraded_requested: Option<(String, String, Candidate)> = None;
    // Rule (a) is subject to `exclude` like every other candidate: a caller
    // that named the requested harness there (a `VISITED_ENV` entry, an
    // orchestrator seat's own harness, or a trigger that has already decided
    // this harness must not keep the work) must never have it handed back.
    let requested_excluded = exclude
        .iter()
        .any(|excl| excl.eq_ignore_ascii_case(&unit.requested));

    if let Some(requested) = snapshot.harness(&unit.requested) {
        if let Some(provider) = snapshot.pool(requested) {
            if requested_excluded {
                exclusions.push((requested.name.clone(), Exclusion::Excluded));
            } else if let Some(why) = ineligible(requested, unit) {
                // Item 5: before every capacity and health question. A route
                // the work may not run on does not keep it merely because it
                // is the one that asked.
                exclusions.push((requested.name.clone(), Exclusion::Ineligible(why)));
            } else if requested.state == HarnessState::Ready
                && fits_all_windows(provider, &unit.bounds, cfg, unit.expected_tokens)
            {
                let (headroom_pct, binding_window) = binding_headroom(provider);
                let projected =
                    projected_headroom(provider, cfg, unit.expected_tokens).unwrap_or(headroom_pct);
                let candidate = Candidate {
                    name: requested.name.clone(),
                    model: models(&requested.name),
                    headroom_pct,
                    projected_headroom_pct: projected,
                    assumed: false,
                    stale: ranking_window(provider).is_some_and(|w| w.stale),
                    binding_window,
                };
                match requested.health.degraded() {
                    None => {
                        return Placement {
                            unit: unit.id.clone(),
                            selected: Some(candidate),
                            keep_requested: true,
                            exclusions,
                        };
                    }
                    Some(reason) => {
                        degraded_requested =
                            Some((requested.name.clone(), reason.to_string(), candidate));
                    }
                }
            } else {
                exclusions.push((
                    requested.name.clone(),
                    requested_unfit_reason(requested, provider, cfg, unit),
                ));
            }
        } else {
            exclusions.push((
                requested.name.clone(),
                Exclusion::Unready("no provider capacity for this harness".to_string()),
            ));
        }
    } else {
        exclusions.push((
            unit.requested.clone(),
            Exclusion::Unready("harness not in this capacity snapshot".to_string()),
        ));
    }

    let mut seen: hashbrown::HashSet<String> = hashbrown::HashSet::new();
    seen.insert(unit.requested.to_lowercase());
    // Every candidate that clears every check, in `cfg.fallback.order`
    // position order -- kept whole (not just the running best) so every
    // eligible loser can be recorded as `Exclusion::Outranked` once the
    // winner is known, not only the disqualified ones.
    let mut eligible: Vec<(usize, bool, Candidate)> = Vec::new();

    for (order_index, name) in cfg.fallback.order.iter().enumerate() {
        if name.eq_ignore_ascii_case(&unit.requested) {
            continue;
        }
        if exclude.iter().any(|excl| excl.eq_ignore_ascii_case(name)) {
            exclusions.push((name.clone(), Exclusion::Excluded));
            continue;
        }
        if !seen.insert(name.to_lowercase()) {
            exclusions.push((name.clone(), Exclusion::Visited));
            continue;
        }

        let Some(harness) = snapshot.harness(name) else {
            exclusions.push((
                name.clone(),
                Exclusion::Unready("harness not in this capacity snapshot".to_string()),
            ));
            continue;
        };
        if !harness.enabled {
            exclusions.push((name.clone(), Exclusion::Disabled));
            continue;
        }
        if !harness.ready {
            exclusions.push((
                name.clone(),
                Exclusion::Unready(
                    harness
                        .unready_reason
                        .clone()
                        .unwrap_or_else(|| "not ready".to_string()),
                ),
            ));
            continue;
        }
        if harness.capacity_small && !unit.bounds.is_small(cfg) {
            exclusions.push((name.clone(), Exclusion::CapacitySmall));
            continue;
        }
        if unit.needs_tool_call_counting && !harness.counts_tool_calls {
            exclusions.push((name.clone(), Exclusion::NoToolCallCounting));
            continue;
        }
        // Item 5: capability, policy, context room and authorized billing,
        // all before any capacity is read or any ranking happens.
        if let Some(why) = ineligible(harness, unit) {
            exclusions.push((name.clone(), Exclusion::Ineligible(why)));
            continue;
        }
        // Issue #455: before the state match below, which would otherwise
        // report a health denial as a bare `HardBlocked` and lose the
        // breaker's reason.
        if let Some(reason) = harness.health.denied() {
            exclusions.push((name.clone(), Exclusion::Unhealthy(reason.to_string())));
            continue;
        }
        match harness.state {
            HarnessState::HardBlocked => {
                exclusions.push((name.clone(), Exclusion::HardBlocked));
                continue;
            }
            HarnessState::Draining => {
                exclusions.push((
                    name.clone(),
                    Exclusion::Draining(harness.state_reason.clone()),
                ));
                continue;
            }
            _ => {}
        }
        if let Some(max) = harness.max_active
            && harness.active >= max
        {
            exclusions.push((
                name.clone(),
                Exclusion::AtMaxActive {
                    active: harness.active,
                    max,
                },
            ));
            continue;
        }

        let Some(provider) = snapshot.pool(harness) else {
            exclusions.push((
                name.clone(),
                Exclusion::Unready("no provider capacity for this harness".to_string()),
            ));
            continue;
        };

        let (headroom_pct, assumed, stale, binding_window) =
            match (harness.state, ranking_window(provider)) {
                // G2: a real reading nothing binds still ranks on its own
                // numbers -- stale, never assumed. The `unknown_headroom_pct`
                // opt-out below governs the genuinely blind case only, so it
                // does not apply here.
                (HarnessState::Unknown, Some(reading)) => (
                    reading.headroom_pct,
                    false,
                    true,
                    Some(reading.window.clone()),
                ),
                (HarnessState::Unknown, None) => {
                    let pct = cfg.fallback.unknown_headroom_pct;
                    if pct <= 0.0 {
                        exclusions.push((name.clone(), Exclusion::UnknownHeadroomOptedOut));
                        continue;
                    }
                    (pct, true, false, None)
                }
                _ => {
                    let (raw, window_name) = binding_headroom(provider);
                    (
                        raw,
                        false,
                        ranking_window(provider).is_some_and(|w| w.stale),
                        window_name,
                    )
                }
            };

        if !assumed && !fits_all_windows(provider, &unit.bounds, cfg, unit.expected_tokens) {
            let (have, need) =
                worst_window_shortfall(provider, &unit.bounds, cfg, unit.expected_tokens);
            exclusions.push((name.clone(), Exclusion::InsufficientHeadroom { have, need }));
            continue;
        }

        let projected = if assumed {
            headroom_pct
        } else {
            projected_headroom(provider, cfg, unit.expected_tokens).unwrap_or(headroom_pct)
        };
        let required = if assumed {
            cfg.fallback
                .min_candidate_headroom_pct
                .max(unit.bounds.required_unknown_headroom_pct(cfg))
        } else {
            cfg.fallback.min_candidate_headroom_pct.max(
                binding_window
                    .as_deref()
                    .and_then(|w| unit.bounds.required_headroom_pct(cfg, w))
                    .unwrap_or(0.0),
            )
        };
        if projected < required {
            exclusions.push((
                name.clone(),
                Exclusion::InsufficientHeadroom {
                    have: projected,
                    need: required,
                },
            ));
            continue;
        }

        let Some(model) = models(name) else {
            exclusions.push((name.clone(), Exclusion::NoEquivalentModel));
            continue;
        };

        eligible.push((
            order_index,
            harness.health.degraded().is_some(),
            Candidate {
                name: name.clone(),
                model: Some(model),
                headroom_pct,
                projected_headroom_pct: projected,
                assumed,
                stale,
                binding_window,
            },
        ));
    }

    // The same "greatest projected headroom, ties by order position" rule
    // as before, just applied over the whole `eligible` set at once instead
    // of tracked incrementally, so the loser(s) can still be identified.
    // Slice A: a healthy eligible candidate beats a degraded one outright;
    // among equals the rule is unchanged (greatest projected headroom, ties
    // by `cfg.fallback.order` position).
    let winner_index = eligible.iter().enumerate().min_by(
        |(_, (a_order, a_degraded, a)), (_, (b_order, b_degraded, b))| {
            a_degraded
                .cmp(b_degraded)
                .then_with(|| {
                    b.projected_headroom_pct
                        .partial_cmp(&a.projected_headroom_pct)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a_order.cmp(b_order))
        },
    );

    let selected = winner_index.map(|(i, _)| eligible[i].2.clone());
    if let Some((winner_i, _)) = winner_index {
        let winner_name = eligible[winner_i].2.name.clone();
        let winner_projected = eligible[winner_i].2.projected_headroom_pct;
        for (i, (_, _, candidate)) in eligible.iter().enumerate() {
            if i == winner_i {
                continue;
            }
            exclusions.push((
                candidate.name.clone(),
                Exclusion::Outranked {
                    by: winner_name.clone(),
                    projected_headroom_pct: winner_projected,
                },
            ));
        }
    }

    // Rule (a), resolved: the degraded requested harness keeps the work
    // unless the walk above found a HEALTHY candidate. A degraded winner is
    // no improvement on a degraded incumbent, and moving the work anyway
    // would just trade one degraded route for another.
    if let Some((name, reason, candidate)) = degraded_requested {
        let healthy_alternative = winner_index.is_some_and(|(i, _)| !eligible[i].1);
        if healthy_alternative {
            exclusions.push((name, Exclusion::Unhealthy(reason)));
            return Placement {
                unit: unit.id.clone(),
                selected,
                keep_requested: false,
                exclusions,
            };
        }
        if let Some(loser) = &selected {
            exclusions.push((
                loser.name.clone(),
                Exclusion::Outranked {
                    by: name,
                    projected_headroom_pct: candidate.projected_headroom_pct,
                },
            ));
        }
        return Placement {
            unit: unit.id.clone(),
            selected: Some(candidate),
            keep_requested: true,
            exclusions,
        };
    }

    Placement {
        unit: unit.id.clone(),
        selected,
        keep_requested: false,
        exclusions,
    }
}

/// Plans every unit in order against one scratch copy of `snapshot`: each
/// admitted unit's `expected_tokens` is added to its provider's `reserved_
/// tokens` and its harness's `active` count is incremented before the next
/// unit is placed, so later units see the capacity the earlier ones already
/// claimed. `O(units * harnesses)`: each unit does one `place` call plus a
/// bounded scratch update, no unit ever re-scans earlier units.
///
/// Not yet called from production code: the multi-unit scheduling call site
/// (issue #358, a later task) lands after this one. Kept `pub` and exercised
/// by this module's own tests now, the same task-ordering shape
/// `FallbackConfig::rollover_headroom_pct` already documents for itself.
#[allow(dead_code)]
pub fn plan(
    snapshot: &CapacitySnapshot,
    cfg: &CtxConfig,
    units: &[WorkUnit],
    models: &dyn Fn(&WorkUnit, &str) -> Option<String>,
) -> Vec<Placement> {
    let mut scratch = snapshot.clone();
    let mut placements = Vec::with_capacity(units.len());

    for unit in units {
        let placement = place(&scratch, cfg, unit, &[], &|name: &str| models(unit, name));

        if let Some(candidate) = &placement.selected {
            // Item 1: the reservation lands on the POOL the route actually
            // spends from, so a second unit placed on a sibling route of the
            // same account sees the tokens the first one already claimed.
            let provider_name = scratch.harness(&candidate.name).map(|h| {
                match scratch.provider(&h.identity.pool) {
                    Some(pool) => pool.provider.clone(),
                    None => h.provider.clone(),
                }
            });
            if let Some(provider_name) = provider_name {
                if let Some(provider) = scratch
                    .providers
                    .iter_mut()
                    .find(|p| p.provider.eq_ignore_ascii_case(&provider_name))
                {
                    provider.reserved_tokens = provider
                        .reserved_tokens
                        .saturating_add(unit.expected_tokens);
                }
                if let Some(harness) = scratch
                    .harnesses
                    .iter_mut()
                    .find(|h| h.name.eq_ignore_ascii_case(&candidate.name))
                {
                    harness.active = harness.active.saturating_add(1);
                }
                // Finding #8 (issue #358 review): `reserved_tokens` just
                // moved on the WHOLE provider, not just the harness that was
                // placed -- every sibling harness sharing this provider has
                // stale `state`/`state_reason` the moment that happens (a
                // second harness on the same provider can flip Ready ->
                // Draining purely from a sibling's admission, with no
                // capacity change of its own). Reclassify every harness on
                // this provider, not only `candidate.name`, so the NEXT
                // unit's own `place` call sees an accurate snapshot.
                if let Some(provider) = scratch.provider(&provider_name).cloned() {
                    let siblings: Vec<String> = scratch
                        .harnesses
                        .iter()
                        .filter(|h| h.provider.eq_ignore_ascii_case(&provider_name))
                        .map(|h| h.name.clone())
                        .collect();
                    for sibling_name in siblings {
                        let Some(harness) = scratch.harness(&sibling_name).cloned() else {
                            continue;
                        };
                        let (state, reason) = classify(&harness, &provider, cfg);
                        if let Some(harness_mut) = scratch
                            .harnesses
                            .iter_mut()
                            .find(|h| h.name.eq_ignore_ascii_case(&sibling_name))
                        {
                            harness_mut.state = state;
                            harness_mut.state_reason = reason;
                        }
                    }
                }
            }
        }

        placements.push(placement);
    }

    placements
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(name: &str, headroom: f64) -> WindowReading {
        WindowReading {
            window: name.to_string(),
            used_pct: 100.0 - headroom,
            headroom_pct: headroom,
            resets_at: 1_700_003_600,
            observed_at: 1_700_000_000,
            age_secs: 0,
            source: "collector".to_string(),
            stale: false,
            limit_reached: false,
            overage_covered: false,
        }
    }

    fn provider(
        name: &str,
        windows: Vec<WindowReading>,
        binding: Option<usize>,
    ) -> ProviderCapacity {
        ProviderCapacity {
            provider: name.to_string(),
            windows,
            binding,
            hard_refused: false,
            reserved_tokens: 0,
            degraded: false,
        }
    }

    fn harness(
        name: &str,
        provider: &str,
        active: u32,
        max_active: Option<u32>,
    ) -> HarnessCapacity {
        HarnessCapacity {
            name: name.to_string(),
            provider: provider.to_string(),
            enabled: true,
            ready: true,
            unready_reason: None,
            capacity_small: false,
            counts_tool_calls: true,
            active,
            max_active,
            reserve_headroom_pct: 10.0,
            state: HarnessState::Unknown,
            state_reason: String::new(),
            health: super::super::health::Admission::Allow,
            identity: super::super::route::RouteIdentity::harness(name, provider),
            offer: None,
        }
    }

    /// A native route row: its own endpoint, credential and model, and a
    /// billing pool that may or may not be shared with another row.
    fn native(name: &str, endpoint: &str, credential: &str, pool: &str) -> HarnessCapacity {
        use super::super::route::{RouteIdentity, RouteOffer, RuntimeKind};
        let identity = RouteIdentity {
            runtime: RuntimeKind::Native,
            provider: "anthropic".to_string(),
            endpoint: endpoint.to_string(),
            credential: credential.to_string(),
            model: Some(name.to_string()),
            pool: pool.to_string(),
        };
        HarnessCapacity {
            // `provider` stays the vendor; `identity.pool` is what capacity
            // is looked up by, which is the whole point of the distinction.
            provider: pool.to_string(),
            offer: Some(RouteOffer {
                route: name.to_string(),
                identity: identity.clone(),
                capabilities: Default::default(),
                billing: super::super::route::BillingPosture::Api,
                context_window_tokens: Some(200_000),
                readings: Vec::new(),
                policy: super::super::route::PolicyVerdict::Allowed,
            }),
            identity,
            ..harness(name, pool, 0, None)
        }
    }

    fn classify_all(
        cfg: &CtxConfig,
        providers: Vec<ProviderCapacity>,
        mut harnesses: Vec<HarnessCapacity>,
    ) -> CapacitySnapshot {
        for h in &mut harnesses {
            let p = providers
                .iter()
                .find(|p| p.provider == h.provider)
                .expect("provider present");
            let (state, reason) = classify(h, p, cfg);
            h.state = state;
            h.state_reason = reason;
        }
        CapacitySnapshot {
            taken_at: 1_700_000_000,
            providers,
            harnesses,
            degraded: false,
        }
    }

    fn base_cfg() -> CtxConfig {
        let mut cfg = CtxConfig::default();
        cfg.fallback.order = vec!["claude".to_string(), "codex".to_string()];
        cfg
    }

    fn unit(id: &str, requested: &str, expected_tokens: u64) -> WorkUnit {
        WorkUnit {
            id: id.to_string(),
            requested: requested.to_string(),
            bounds: TaskBounds {
                tokens: None,
                tool_calls: None,
            },
            expected_tokens,
            needs_tool_call_counting: false,
            source_model: None,
            source_model_explicit: false,
            delegation: true,
            demand: super::super::route::Demand::default(),
        }
    }

    fn always_model(_: &str) -> Option<String> {
        Some("model".to_string())
    }

    fn degraded(reason: &str) -> super::super::health::Admission {
        super::super::health::Admission::Degraded {
            reason: reason.to_string(),
        }
    }

    /// Issue #487, criterion 1: two routes on ONE account are one capacity.
    ///
    /// Before N18 the lookup was by vendor name, so two models on one
    /// Anthropic account resolved to one `ProviderCapacity` only by accident
    /// of both being called "anthropic" -- and two SEPARATE accounts at
    /// Anthropic would have collided into that same row, refusing work the
    /// operator had paid for twice. The lookup is by billing POOL now, so
    /// both facts hold at once.
    #[test]
    fn two_routes_on_one_account_are_one_capacity_and_two_accounts_are_not() {
        let cfg = base_cfg();
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("work-account", vec![window("five_hour", 50.0)], Some(0)),
                provider("personal-account", vec![window("five_hour", 90.0)], Some(0)),
            ],
            vec![
                native("opus", "api.anthropic.com", "work", "work-account"),
                native("sonnet", "api.anthropic.com", "work", "work-account"),
                native("haiku", "api.anthropic.com", "personal", "personal-account"),
            ],
        );

        let opus = snapshot.harness("opus").expect("opus row");
        let sonnet = snapshot.harness("sonnet").expect("sonnet row");
        let personal = snapshot.harness("haiku").expect("personal row");

        assert_eq!(
            snapshot.pool(opus).map(|p| p.provider.as_str()),
            snapshot.pool(sonnet).map(|p| p.provider.as_str()),
            "one account is one capacity, whichever model it is asked for"
        );
        assert_ne!(
            snapshot.pool(opus).map(|p| p.provider.as_str()),
            snapshot.pool(personal).map(|p| p.provider.as_str()),
            "a second account at the same vendor is a second balance"
        );

        let pool_names: Vec<&str> = snapshot
            .pool_siblings(opus)
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(pool_names, vec!["opus", "sonnet"], "and it says which");
        assert_eq!(
            snapshot.pool_siblings(personal).len(),
            1,
            "the other account is coupled to nothing"
        );
    }

    /// The other half of criterion 1: a unit placed on one route claims the
    /// POOL's tokens, so a later unit on a sibling route of the same account
    /// sees a balance that has already been drawn down. Ranking them as two
    /// capacities is exactly the artificial headroom N18 removes.
    #[test]
    fn a_placement_on_one_route_reserves_against_the_whole_pool() {
        let mut cfg = base_cfg();
        cfg.fallback.order = vec!["opus".to_string(), "sonnet".to_string()];
        let snapshot = classify_all(
            &cfg,
            vec![provider(
                "work-account",
                vec![window("five_hour", 90.0)],
                Some(0),
            )],
            vec![
                native("opus", "api.anthropic.com", "work", "work-account"),
                native("sonnet", "api.anthropic.com", "work", "work-account"),
            ],
        );

        let planned = plan(
            &snapshot,
            &cfg,
            &[unit("u1", "opus", 5_000), unit("u2", "sonnet", 5_000)],
            &|_unit, name| Some(name.to_string()),
        );
        assert_eq!(planned.len(), 2);
        assert!(
            planned.iter().all(|p| p.selected.is_some()),
            "both fit at 90% headroom: {planned:?}"
        );
        // The second placement was judged against the first one's claim,
        // which only happens because both routes resolve to one pool row.
        let first = planned[0].selected.as_ref().expect("first");
        let second = planned[1].selected.as_ref().expect("second");
        assert!(
            second.projected_headroom_pct <= first.projected_headroom_pct,
            "the sibling saw the tokens the first unit already claimed: {planned:?}"
        );
    }

    /// Criterion 2: an endpoint outage takes exactly the routes that share
    /// the endpoint, and nothing else -- not a route on another host, and
    /// never one whose only relationship is the vendor's name.
    #[test]
    fn an_endpoint_outage_couples_only_the_routes_on_that_endpoint() {
        let cfg = base_cfg();
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("work-account", vec![window("five_hour", 50.0)], Some(0)),
                provider("personal-account", vec![window("five_hour", 50.0)], Some(0)),
                provider("aws-account", vec![window("five_hour", 50.0)], Some(0)),
            ],
            vec![
                native("opus", "api.anthropic.com", "work", "work-account"),
                native("haiku", "api.anthropic.com", "personal", "personal-account"),
                native(
                    "nova",
                    "bedrock.us-east-1.amazonaws.com",
                    "aws",
                    "aws-account",
                ),
            ],
        );

        let direct = snapshot.harness("opus").expect("row");
        let shared: Vec<&str> = snapshot
            .endpoint_siblings(direct)
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(
            shared,
            vec!["opus", "haiku"],
            "the routes an outage on this host takes with it"
        );
        assert!(
            !shared.contains(&"nova"),
            "a route on another host is untouched: {shared:?}"
        );
        let bedrock = snapshot.harness("nova").expect("row");
        assert!(!direct.identity.shares_endpoint(&bedrock.identity));
        assert!(
            !direct.identity.shares_pool(&bedrock.identity),
            "and the two hosts are separate balances as well as separate dependencies"
        );
        // Sharing an endpoint is NOT sharing a balance.
        let sibling = snapshot.harness("haiku").expect("row");
        assert!(direct.identity.shares_endpoint(&sibling.identity));
        assert!(!direct.identity.shares_pool(&sibling.identity));
    }

    /// Criterion 6 / item 6: a route the work is not authorized to be billed
    /// to is excluded BEFORE ranking, by its own typed reason -- so nothing
    /// reads as "outranked", and no subscription work slides onto metered
    /// credit because the subscription happened to be busy.
    #[test]
    fn an_unauthorized_billing_route_is_excluded_before_ranking_with_its_own_reason() {
        use super::super::route::{BillingPosture, Ineligible};
        let cfg = base_cfg();
        let mut metered = native("opus", "api.anthropic.com", "work", "work-account");
        if let Some(offer) = metered.offer.as_mut() {
            offer.billing = BillingPosture::Api;
        }
        let snapshot = classify_all(
            &cfg,
            vec![provider(
                "work-account",
                vec![window("five_hour", 90.0)],
                Some(0),
            )],
            vec![metered],
        );

        let mut refused = unit("u1", "opus", 100);
        refused.demand.authorized_billing = [BillingPosture::Subscription].into_iter().collect();
        let placement = place(&snapshot, &cfg, &refused, &[], &always_model);

        assert!(placement.selected.is_none(), "{placement:?}");
        let (name, reason) = placement
            .exclusions
            .iter()
            .find(|(_, reason)| matches!(reason, Exclusion::Ineligible(_)))
            .expect("a typed ineligibility, not an outranking");
        assert_eq!(name, "opus");
        assert!(
            matches!(
                reason,
                Exclusion::Ineligible(Ineligible::UnauthorizedBilling { .. })
            ),
            "{reason:?}"
        );
        assert!(reason.label().contains("not authorized"), "{reason:?}");

        // Authorize it and the very same route takes the work -- the refusal
        // is about permission, never about capacity or health.
        let mut allowed = refused.clone();
        allowed.demand.authorized_billing = [BillingPosture::Api].into_iter().collect();
        let placement = place(&snapshot, &cfg, &allowed, &[], &always_model);
        assert_eq!(
            placement.selected.map(|c| c.name),
            Some("opus".to_string()),
            "an authorized route is placed normally"
        );
    }

    /// Item 2 / criterion 7: a route that has reported one dimension still
    /// lists the other four, as labelled estimates -- never as free capacity
    /// -- and the binding dimension is always one of the listed ones.
    #[test]
    fn every_dimension_is_reported_and_the_unmeasured_ones_are_estimates() {
        let cfg = base_cfg();
        let route = native("opus", "api.anthropic.com", "work", "work-account");
        let capacity = provider("work-account", vec![window("five_hour", 80.0)], Some(0));
        let now = 1_700_000_010;

        let dimensions = route_dimensions(&route, Some(&capacity), now, &cfg);
        assert_eq!(
            dimensions.len(),
            5,
            "one reported window plus the four nothing has reported: {dimensions:?}"
        );
        assert_eq!(
            dimensions.iter().filter(|d| d.is_measured()).count(),
            1,
            "exactly the one the provider stated: {dimensions:?}"
        );
        assert!(
            dimensions
                .iter()
                .filter(|d| !d.is_measured())
                .all(|d| d.pct < 100.0 && d.provenance.reason().is_some()),
            "an unmeasured dimension is a labelled conservative bound, never 100%: {dimensions:?}"
        );

        let binding = route_binding(&route, Some(&capacity), now, &cfg).expect("a binding");
        assert!(
            dimensions.contains(&binding),
            "the binding dimension must be one this row also lists"
        );
    }

    /// Slice A: a degraded route is reduced, not excluded -- rule (a) hands
    /// the work to a healthy alternative instead of keeping the requested
    /// harness, and says why.
    #[test]
    fn a_degraded_requested_harness_loses_to_a_healthy_alternative() {
        let cfg = base_cfg();
        let mut claude = harness("claude", "anthropic", 0, None);
        claude.health = degraded("claude: error rate 30% over 10 turns");
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 90.0)], Some(0)),
                provider("openai", vec![window("five_hour", 40.0)], Some(0)),
            ],
            vec![claude, harness("codex", "openai", 0, None)],
        );

        assert_eq!(
            snapshot.harness("claude").map(|h| h.state),
            Some(HarnessState::Ready),
            "a degraded route still answers"
        );
        let placement = place(&snapshot, &cfg, &unit("u", "claude", 0), &[], &always_model);
        assert!(!placement.keep_requested);
        assert_eq!(
            placement.selected.map(|c| c.name),
            Some("codex".to_string()),
            "even though claude has more than twice the headroom"
        );
        assert!(
            placement
                .exclusions
                .iter()
                .any(|(name, exclusion)| name == "claude"
                    && matches!(exclusion, Exclusion::Unhealthy(reason)
                    if reason.contains("error rate 30%"))),
            "{:?}",
            placement.exclusions
        );
    }

    #[test]
    fn a_degraded_harness_is_still_chosen_when_it_is_the_only_candidate() {
        let cfg = base_cfg();
        let mut claude = harness("claude", "anthropic", 0, None);
        claude.health = degraded("claude: error rate 30% over 10 turns");
        let mut codex = harness("codex", "openai", 0, None);
        codex.health = degraded("codex: error rate 40% over 10 turns");
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 90.0)], Some(0)),
                provider("openai", vec![window("five_hour", 40.0)], Some(0)),
            ],
            vec![claude, codex],
        );

        let placement = place(&snapshot, &cfg, &unit("u", "claude", 0), &[], &always_model);
        assert!(
            placement.keep_requested,
            "trading one degraded route for another is churn, not a reroute"
        );
        assert_eq!(
            placement.selected.map(|c| c.name),
            Some("claude".to_string())
        );
    }

    /// Rule (b): among alternatives, healthy beats degraded outright -- the
    /// projected-headroom comparison only decides ties within one band.
    #[test]
    fn a_healthy_alternative_outranks_a_degraded_one_with_more_headroom() {
        let mut cfg = base_cfg();
        cfg.fallback.order = vec![
            "claude".to_string(),
            "codex".to_string(),
            "gemini".to_string(),
        ];
        let mut codex = harness("codex", "openai", 0, None);
        codex.health = degraded("codex: error rate 30% over 10 turns");
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 5.0)], Some(0)),
                provider("openai", vec![window("five_hour", 95.0)], Some(0)),
                provider("google", vec![window("five_hour", 40.0)], Some(0)),
            ],
            vec![
                harness("claude", "anthropic", 0, None),
                codex,
                harness("gemini", "google", 0, None),
            ],
        );

        let placement = place(&snapshot, &cfg, &unit("u", "claude", 0), &[], &always_model);
        assert_eq!(
            placement.selected.map(|c| c.name),
            Some("gemini".to_string()),
            "40% healthy beats 95% degraded"
        );
    }

    #[test]
    fn classify_names_the_degradation_on_an_otherwise_ready_harness() {
        let cfg = base_cfg();
        let mut claude = harness("claude", "anthropic", 0, None);
        claude.health = degraded("claude: first-token p50 30.0s over 8 turns");
        let provider = provider("anthropic", vec![window("five_hour", 90.0)], Some(0));
        let (state, reason) = classify(&claude, &provider, &cfg);
        assert_eq!(state, HarnessState::Ready);
        assert_eq!(
            reason,
            "ready (degraded: claude: first-token p50 30.0s over 8 turns)"
        );
    }

    /// Audit finding G2: codex's only usage source is its own rollout files,
    /// written only during a turn, so `pace::binding` drops the reading
    /// roughly `collector_max_age_secs` after the last one and `classify`
    /// reports `Unknown` -- even though the reading is real, its window has
    /// not reset, and `window::available` still shows it. Ranking then used
    /// the configured `unknown_headroom_pct` (25) in place of the 53% the
    /// provider actually has. For RANKING that reading now contributes its
    /// own headroom, marked stale rather than assumed; `classify` still
    /// refuses it as a hard-gate authority.
    #[test]
    fn an_available_but_unbinding_reading_ranks_on_its_real_headroom() {
        let cfg = base_cfg();
        let mut aged = window("five_hour", 53.0);
        aged.age_secs = 7_200;
        aged.stale = true;

        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 5.0)], Some(0)),
                provider("openai", vec![aged], None),
            ],
            vec![
                harness("claude", "anthropic", 0, None),
                harness("codex", "openai", 0, None),
            ],
        );
        assert_eq!(
            snapshot.harness("codex").expect("codex row").state,
            HarnessState::Unknown,
            "a reading nothing binds is still no hard-gate authority"
        );

        let placement = place(
            &snapshot,
            &cfg,
            &unit("u1", "claude", 0),
            &[],
            &always_model,
        );
        let selected = placement.selected.expect("codex is the only alternative");
        assert_eq!(selected.name, "codex");
        assert_eq!(
            cfg.fallback.unknown_headroom_pct, 25.0,
            "the blanket assumption this replaces"
        );
        assert_eq!(
            selected.headroom_pct, 53.0,
            "ranks on the headroom the provider actually reported"
        );
        assert_eq!(selected.projected_headroom_pct, 53.0);
        assert!(!selected.assumed, "a real reading is not an assumption");
        assert!(selected.stale, "but it is stale, and says so");
        assert_eq!(selected.binding_window.as_deref(), Some("five_hour"));
    }

    #[test]
    fn placing_the_same_snapshot_twice_is_identical() {
        let cfg = base_cfg();
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 5.0)], Some(0)),
                provider("openai", vec![window("five_hour", 90.0)], Some(0)),
            ],
            vec![
                harness("claude", "anthropic", 0, None),
                harness("codex", "openai", 0, None),
            ],
        );
        let unit = unit("u1", "claude", 0);
        let first = place(&snapshot, &cfg, &unit, &[], &always_model);
        let second = place(&snapshot, &cfg, &unit, &[], &always_model);
        assert_eq!(first, second);
    }

    #[test]
    fn requested_ready_harness_is_kept() {
        let cfg = base_cfg();
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 50.0)], Some(0)),
                provider("openai", vec![window("five_hour", 90.0)], Some(0)),
            ],
            vec![
                harness("claude", "anthropic", 0, None),
                harness("codex", "openai", 0, None),
            ],
        );
        let unit = unit("u1", "claude", 0);
        let placement = place(&snapshot, &cfg, &unit, &[], &always_model);
        assert!(placement.keep_requested);
        assert_eq!(placement.selected.expect("selected").name, "claude");
    }

    #[test]
    fn draining_requested_moves_to_the_best_projected_headroom() {
        let cfg = base_cfg();
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 5.0)], Some(0)),
                provider("openai", vec![window("five_hour", 60.0)], Some(0)),
            ],
            vec![
                harness("claude", "anthropic", 0, None),
                harness("codex", "openai", 0, None),
            ],
        );
        let unit = unit("u1", "claude", 0);
        let placement = place(&snapshot, &cfg, &unit, &[], &always_model);
        assert!(!placement.keep_requested);
        let selected = placement.selected.expect("an alternate was selected");
        assert_eq!(selected.name, "codex");
    }

    #[test]
    fn a_tie_in_projected_headroom_is_broken_by_order() {
        let cfg = base_cfg();
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 5.0)], Some(0)),
                provider("openai", vec![window("five_hour", 40.0)], Some(0)),
                provider("third", vec![window("five_hour", 40.0)], Some(0)),
            ],
            vec![
                harness("claude", "anthropic", 0, None),
                harness("codex", "openai", 0, None),
                harness("gemini", "third", 0, None),
            ],
        );
        let mut cfg = cfg;
        cfg.fallback.order = vec![
            "claude".to_string(),
            "codex".to_string(),
            "gemini".to_string(),
        ];
        let unit = unit("u1", "claude", 0);
        let placement = place(&snapshot, &cfg, &unit, &[], &always_model);
        let selected = placement.selected.expect("a tie still selects one");
        assert_eq!(
            selected.name, "codex",
            "codex precedes gemini in fallback.order"
        );
    }

    /// Follow-up to issue #358 task 2: every eligible-but-not-chosen
    /// candidate must carry a reason too, not just the disqualified ones --
    /// `zirv ctx status` needs to explain every candidate it considered.
    #[test]
    fn an_eligible_loser_carries_outranked() {
        let cfg = base_cfg();
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 5.0)], Some(0)),
                provider("openai", vec![window("five_hour", 60.0)], Some(0)),
                provider("third", vec![window("five_hour", 40.0)], Some(0)),
            ],
            vec![
                harness("claude", "anthropic", 0, None),
                harness("codex", "openai", 0, None),
                harness("gemini", "third", 0, None),
            ],
        );
        let mut cfg = cfg;
        cfg.fallback.order = vec![
            "claude".to_string(),
            "codex".to_string(),
            "gemini".to_string(),
        ];
        let unit = unit("u1", "claude", 0);
        let placement = place(&snapshot, &cfg, &unit, &[], &always_model);
        let selected = placement
            .selected
            .expect("codex has the greater projected headroom");
        assert_eq!(selected.name, "codex");

        let gemini_reason = placement
            .exclusions
            .iter()
            .find(|(name, _)| name == "gemini")
            .map(|(_, reason)| reason.clone());
        assert_eq!(
            gemini_reason,
            Some(Exclusion::Outranked {
                by: "codex".to_string(),
                projected_headroom_pct: 60.0,
            })
        );
    }

    #[test]
    fn max_active_is_respected_across_a_plan_of_fifteen_units() {
        let mut cfg = base_cfg();
        cfg.fallback.min_candidate_headroom_pct = 0.0;
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 90.0)], Some(0)),
                provider("openai", vec![window("five_hour", 90.0)], Some(0)),
            ],
            vec![
                harness("claude", "anthropic", 0, Some(5)),
                harness("codex", "openai", 0, Some(5)),
            ],
        );
        let units: Vec<WorkUnit> = (0..15)
            .map(|i| unit(&format!("u{i}"), "claude", 0))
            .collect();
        let placements = plan(&snapshot, &cfg, &units, &|_, _| Some("model".to_string()));
        assert_eq!(placements.len(), 15);

        let mut counts = std::collections::HashMap::new();
        for placement in &placements {
            if let Some(candidate) = &placement.selected {
                *counts.entry(candidate.name.clone()).or_insert(0u32) += 1;
            } else {
                assert!(
                    !placement.exclusions.is_empty(),
                    "an unplaced unit must explain every candidate it considered"
                );
            }
        }
        for (_, count) in counts {
            assert!(count <= 5, "no harness may exceed its max_active cap");
        }
    }

    #[test]
    fn reservations_reduce_projected_headroom_and_can_flip_a_candidate_to_draining() {
        let mut cfg = base_cfg();
        cfg.pace.five_hour_budget_tokens = 100_000;
        cfg.fallback.min_candidate_headroom_pct = 5.0;
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 5.0)], Some(0)),
                provider("openai", vec![window("five_hour", 20.0)], Some(0)),
            ],
            vec![
                harness("claude", "anthropic", 0, None),
                harness("codex", "openai", 0, None),
            ],
        );
        // 15% of the 100k budget: enough to clear codex's raw 20% headroom
        // for the first unit, but the second unit's reservation should push
        // the projected headroom under the 5% floor.
        let units = vec![unit("u1", "claude", 15_000), unit("u2", "claude", 15_000)];
        let placements = plan(&snapshot, &cfg, &units, &|_, _| Some("model".to_string()));
        let first = placements[0].selected.as_ref().expect("first unit placed");
        assert_eq!(first.name, "codex");
        assert!(
            placements[1].selected.is_none(),
            "the second unit should now be excluded"
        );
        let codex_reason = placements[1]
            .exclusions
            .iter()
            .find(|(name, _)| name == "codex")
            .map(|(_, reason)| reason.clone());
        assert!(matches!(
            codex_reason,
            Some(Exclusion::InsufficientHeadroom { .. }) | Some(Exclusion::Draining(_))
        ));
    }

    #[test]
    fn two_harnesses_on_one_provider_share_reserved_tokens() {
        let mut cfg = base_cfg();
        cfg.fallback.order = vec![
            "claude".to_string(),
            "claude-worker".to_string(),
            "codex".to_string(),
        ];
        cfg.pace.five_hour_budget_tokens = 100_000;
        cfg.fallback.min_candidate_headroom_pct = 5.0;
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 30.0)], Some(0)),
                provider("openai", vec![window("five_hour", 90.0)], Some(0)),
            ],
            vec![
                harness("claude", "anthropic", 0, None),
                harness("claude-worker", "anthropic", 0, None),
                harness("codex", "openai", 0, None),
            ],
        );
        let units = vec![
            unit("u1", "claude-worker", 15_000),
            unit("u2", "claude", 15_000),
        ];
        let placements = plan(&snapshot, &cfg, &units, &|_, _| Some("model".to_string()));
        // Both units target harnesses on the shared "anthropic" provider;
        // the second unit's own requested-harness projection must already
        // reflect the first unit's reservation.
        let second_provider = placements[1]
            .selected
            .as_ref()
            .map(|c| c.projected_headroom_pct);
        assert!(second_provider.is_some());
        assert!(second_provider.unwrap() < 30.0);
    }

    /// Finding #8 (issue #358 review): `plan()` used to reclassify only the
    /// harness it had just placed after moving `reserved_tokens` onto the
    /// shared provider -- a SIBLING harness on that same provider kept its
    /// stale `state`, so `place()`'s own requested-harness fast path (`state
    /// == Ready`) would keep handing it out long after the provider's real
    /// projected headroom had crossed below its own `reserve_headroom_pct`.
    /// Two harnesses share one provider here; the first unit's own
    /// reservation alone (600 of the provider's 1000-token five-hour budget,
    /// against 15% raw headroom) is enough to push projected headroom to 0%,
    /// under BOTH harnesses' 10% reserve floor -- so the second unit's
    /// requested harness must already read Draining, not a stale Ready.
    #[test]
    fn a_sibling_harness_is_reclassified_after_a_plan_admission_on_its_shared_provider() {
        let mut cfg = base_cfg();
        cfg.fallback.order = vec!["claude-a".to_string(), "claude-b".to_string()];
        cfg.pace.five_hour_budget_tokens = 1_000;
        let snapshot = classify_all(
            &cfg,
            vec![provider(
                "anthropic",
                vec![window("five_hour", 15.0)],
                Some(0),
            )],
            vec![
                harness("claude-a", "anthropic", 0, None),
                harness("claude-b", "anthropic", 0, None),
            ],
        );
        let units = vec![unit("u1", "claude-a", 600), unit("u2", "claude-b", 0)];
        let placements = plan(&snapshot, &cfg, &units, &|_, name| always_model(name));

        assert_eq!(
            placements[0].selected.as_ref().map(|c| c.name.as_str()),
            Some("claude-a"),
            "sanity: the first unit lands on the harness it requested"
        );

        assert!(
            !placements[1].keep_requested,
            "claude-b must no longer be kept as a fresh Ready candidate once the shared \
             provider's headroom has drained below its own reserve: {:?}",
            placements[1]
        );
        assert!(
            placements[1].selected.is_none(),
            "no other harness is eligible either (claude-a is also draining): {:?}",
            placements[1]
        );
        let claude_b_reason = placements[1]
            .exclusions
            .iter()
            .find(|(name, _)| name == "claude-b")
            .map(|(_, reason)| reason.clone());
        assert!(
            matches!(claude_b_reason, Some(Exclusion::Draining(_))),
            "got {claude_b_reason:?} in {:?}",
            placements[1]
        );
    }

    #[test]
    fn unknown_headroom_opted_out_excludes_the_candidate() {
        let mut cfg = base_cfg();
        cfg.fallback.unknown_headroom_pct = 0.0;
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 5.0)], Some(0)),
                provider("openai", vec![], None),
            ],
            vec![
                harness("claude", "anthropic", 0, None),
                harness("codex", "openai", 0, None),
            ],
        );
        assert_eq!(
            snapshot.harness("codex").expect("codex present").state,
            HarnessState::Unknown
        );
        let unit = unit("u1", "claude", 0);
        let placement = place(&snapshot, &cfg, &unit, &[], &always_model);
        let codex_reason = placement
            .exclusions
            .iter()
            .find(|(name, _)| name == "codex")
            .map(|(_, reason)| reason.clone());
        assert_eq!(codex_reason, Some(Exclusion::UnknownHeadroomOptedOut));
    }

    #[test]
    fn a_seven_day_shortfall_rejects_a_unit_the_five_hour_window_would_accept() {
        let mut cfg = base_cfg();
        cfg.pace.five_hour_budget_tokens = 100_000;
        cfg.pace.seven_day_budget_tokens = 1_000_000;
        cfg.fallback.min_candidate_headroom_pct = 0.0;
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 5.0)], Some(0)),
                provider(
                    "openai",
                    vec![window("five_hour", 50.0), window("seven_day", 1.0)],
                    Some(1),
                ),
            ],
            vec![
                harness("claude", "anthropic", 0, None),
                harness("codex", "openai", 0, None),
            ],
        );
        let bounds = TaskBounds {
            tokens: Some(25_000),
            tool_calls: None,
        };
        let mut unit = unit("u1", "claude", 25_000);
        unit.bounds = bounds;
        let placement = place(&snapshot, &cfg, &unit, &[], &always_model);
        assert!(
            placement.selected.is_none(),
            "codex's seven_day window cannot fit the bounded task even though five_hour could: {placement:?}"
        );
    }

    #[test]
    fn a_limit_reached_window_hard_blocks_the_provider() {
        let cfg = base_cfg();
        let mut blocked = window("five_hour", 0.0);
        blocked.limit_reached = true;
        let anthropic = ProviderCapacity {
            hard_refused: true,
            ..provider("anthropic", vec![blocked], Some(0))
        };
        let claude = harness("claude", "anthropic", 0, None);
        let (state, _) = classify(&claude, &anthropic, &cfg);
        assert_eq!(state, HarnessState::HardBlocked);
    }

    #[test]
    fn overage_covered_never_hard_blocks() {
        let cfg = base_cfg();
        let mut covered = window("five_hour", 0.0);
        covered.overage_covered = true;
        covered.used_pct = 100.0;
        let anthropic = provider("anthropic", vec![covered], Some(0));
        let claude = harness("claude", "anthropic", 0, None);
        let (state, _) = classify(&claude, &anthropic, &cfg);
        assert_ne!(state, HarnessState::HardBlocked);
    }

    #[test]
    fn classify_state_ladder_order() {
        let mut cfg = base_cfg();
        cfg.fallback.min_candidate_headroom_pct = 50.0;

        // Disabled beats everything else, even a hard refusal.
        let mut h = harness("claude", "anthropic", 0, None);
        h.enabled = false;
        let p = ProviderCapacity {
            hard_refused: true,
            ..provider("anthropic", vec![window("five_hour", 0.0)], Some(0))
        };
        assert_eq!(classify(&h, &p, &cfg).0, HarnessState::Disabled);

        // HardBlocked beats draining/unknown.
        let h = harness("claude", "anthropic", 10, Some(1));
        assert_eq!(classify(&h, &p, &cfg).0, HarnessState::HardBlocked);

        // Draining (at max_active) beats unknown.
        let h = harness("claude", "anthropic", 1, Some(1));
        let p = provider("anthropic", vec![], None);
        assert_eq!(classify(&h, &p, &cfg).0, HarnessState::Draining);

        // No binding reading at all -> Unknown.
        let h = harness("claude", "anthropic", 0, None);
        let p = provider("anthropic", vec![], None);
        assert_eq!(classify(&h, &p, &cfg).0, HarnessState::Unknown);

        // Everything clears -> Ready.
        cfg.fallback.min_candidate_headroom_pct = 0.0;
        let h = harness("claude", "anthropic", 0, None);
        let p = provider("anthropic", vec![window("five_hour", 90.0)], Some(0));
        assert_eq!(classify(&h, &p, &cfg).0, HarnessState::Ready);
    }

    /// D-5: rule (a) never consulted `exclude`, so `route_blocked_session`
    /// could keep the very harness its caller had just excluded (a
    /// `VISITED_ENV` entry, or the orchestrator seat's own harness).
    #[test]
    fn rule_a_never_keeps_an_excluded_requested_harness() {
        let cfg = base_cfg();
        let snapshot = classify_all(
            &cfg,
            vec![
                provider("anthropic", vec![window("five_hour", 90.0)], Some(0)),
                provider("openai", vec![window("five_hour", 50.0)], Some(0)),
            ],
            vec![
                harness("claude", "anthropic", 0, None),
                harness("codex", "openai", 0, None),
            ],
        );
        let unit = unit("u1", "claude", 0);
        let placement = place(&snapshot, &cfg, &unit, &["claude"], &always_model);
        assert!(
            !placement.keep_requested,
            "an excluded harness may never be kept by rule (a)"
        );
        assert_eq!(
            placement.selected.map(|c| c.name),
            Some("codex".to_string())
        );
    }
}
