//! Issue #487 (N18): the native half of the pure placement model.
//!
//! `allocator.rs` ranks HARNESSES against per-provider usage windows. A
//! native route is a different animal in four ways, and this module states
//! each of them once so `allocator` can keep doing what it already does:
//!
//! 1. **Identity.** A harness is named by one string. A native route is
//!    `(provider, endpoint, credential, model)`, and what it SPENDS is a
//!    fifth thing again -- the billing pool, which two routes on one account
//!    share and two accounts at one provider do not.
//! 2. **Capacity.** A subscription window is one dimension among five;
//!    requests/minute, tokens/minute, concurrent requests and a configured
//!    spend ceiling bind independently, and the tightest of them is what a
//!    placement must respect.
//! 3. **Failure scope.** "The endpoint is down", "this key is wrong", "this
//!    account may not use this model", "the prompt is too long" and "the
//!    model declined" are five different facts. Slice 1 of #455 folded the
//!    first two into one per-harness breaker because a harness has no other
//!    scopes to speak of; a native route has all of them.
//! 4. **Reconciliation.** A harness reports usage through a transcript that
//!    is read once. A native request returns its own usage inline, and the
//!    same request can be seen twice -- a retry that actually succeeded, a
//!    replayed journal, two supervisors reading one run -- so "exactly once"
//!    has to be stated rather than assumed.
//!
//! Pure in exactly the sense `rot.rs` documents for itself: no fs, clock,
//! env or net. Every function takes its inputs and an explicit `now`, so a
//! placement, an exclusion, a breaker verdict and a reconciliation can all
//! be replayed from the evidence that produced them. The I/O that feeds this
//! lives in `fallback.rs`, `health_store.rs`, `reservation.rs` and
//! `runtime/native.rs`.
//!
//! This EXTENDS the existing model rather than standing beside it:
//! `allocator::HarnessCapacity` carries a [`RouteIdentity`] and a
//! [`RouteOffer`], `allocator::place` calls [`eligible`] before it ranks
//! anything, and the health breaker keys on the [`FailureRouting`] scopes
//! below. There is no second `place`.

use std::collections::BTreeSet;

use serde::Serialize;

use super::event::ProviderErrorClass;
use super::provider::adapter::{FailureClass, FailureScopeKind, ProviderFailure};

/// Whether work runs inside a supervised harness process or through zirv's
/// own native runtime. Independent of everything else here on purpose: the
/// same provider, account and model can be reached both ways, and which one
/// a unit uses is a property of the unit, never of the route's capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimeKind {
    #[default]
    Harness,
    Native,
}

impl RuntimeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Harness => "harness",
            Self::Native => "native",
        }
    }
}

/// What a route IS, as opposed to what it currently has left.
///
/// `pool` is deliberately separate from `credential`. Two routes on one
/// account share a pool, which is the whole point: ranking them as two
/// independent capacities invents headroom that the account does not have.
/// Two accounts at one provider do NOT share a pool, which is the other half:
/// coupling them would refuse work the operator has paid for twice.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct RouteIdentity {
    pub runtime: RuntimeKind,
    pub provider: String,
    pub endpoint: String,
    pub credential: String,
    pub model: Option<String>,
    pub pool: String,
}

impl RouteIdentity {
    /// A harness route: one name, one provider, and a pool that is the
    /// provider itself -- which is exactly the coupling `allocator` has
    /// always applied, now stated rather than implied.
    pub fn harness(name: &str, provider: &str) -> Self {
        Self {
            runtime: RuntimeKind::Harness,
            provider: provider.to_string(),
            endpoint: provider.to_string(),
            credential: name.to_string(),
            model: None,
            pool: provider.to_string(),
        }
    }

    /// A native route's identity, read off the resolved route the runtime is
    /// already running on. The single conversion point, so nothing
    /// downstream has to re-derive a pool id from a route id -- and so the
    /// account/pool distinction is made exactly once.
    pub fn from_runtime(route: &super::runtime::journal::RouteIdentity) -> Self {
        Self {
            runtime: RuntimeKind::Native,
            provider: route.provider.as_ref().to_string(),
            endpoint: route.endpoint.as_ref().to_string(),
            credential: route.account.as_ref().to_string(),
            model: Some(route.model.id.clone()),
            pool: route.billing_pool.as_ref().to_string(),
        }
    }

    /// Whether two routes draw on the same account balance.
    pub fn shares_pool(&self, other: &Self) -> bool {
        self.pool.eq_ignore_ascii_case(&other.pool)
    }

    /// Whether two routes depend on the same reachable host. A shared
    /// endpoint outage trips both; nothing else about them is coupled.
    pub fn shares_endpoint(&self, other: &Self) -> bool {
        self.endpoint.eq_ignore_ascii_case(&other.endpoint)
    }

    pub fn label(&self) -> String {
        match &self.model {
            Some(model) => format!("{}/{}@{}", self.provider, model, self.endpoint),
            None => format!("{}@{}", self.provider, self.endpoint),
        }
    }
}

/// The five capacity dimensions a route can run out of, kept apart because
/// they refill differently and bind independently: a per-minute request cap
/// clears in a minute, a subscription window in hours, and a spend ceiling
/// never -- an operator has to raise it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Dimension {
    RequestsPerMinute,
    TokensPerMinute,
    ConcurrentRequests,
    SubscriptionWindow,
    SpendCeiling,
}

impl Dimension {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RequestsPerMinute => "requests-per-minute",
            Self::TokensPerMinute => "tokens-per-minute",
            Self::ConcurrentRequests => "concurrent-requests",
            Self::SubscriptionWindow => "subscription-window",
            Self::SpendCeiling => "spend-ceiling",
        }
    }
}

/// Where a headroom number came from. The distinction is the point of the
/// type: an operator looking at `zirv ctx status` has to be able to tell a
/// number the provider reported from one zirv assumed on its behalf, and a
/// placement that lost to an estimate is a different fact from one that lost
/// to a measurement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "provenance", rename_all = "kebab-case")]
pub enum Provenance {
    /// The provider stated this, recently enough to still bind.
    Measured,
    /// Derived, with the reason. Never treated as free capacity.
    Estimated { reason: String },
}

impl Provenance {
    pub fn is_measured(&self) -> bool {
        matches!(self, Self::Measured)
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Measured => None,
            Self::Estimated { reason } => Some(reason),
        }
    }
}

/// One dimension's raw reading. `limit: None` means the provider never
/// stated a ceiling for this dimension -- which is NOT the same as an
/// unlimited one, and [`headroom`] treats it accordingly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reading {
    pub dimension: Dimension,
    pub limit: Option<u64>,
    pub used: u64,
    /// When the provider stated this. `None` for a dimension nothing has
    /// ever reported.
    pub observed_at: Option<u64>,
}

impl Reading {
    pub fn new(dimension: Dimension, limit: Option<u64>, used: u64, observed_at: u64) -> Self {
        Self {
            dimension,
            limit,
            used,
            observed_at: Some(observed_at),
        }
    }

    /// A dimension the provider has never reported on.
    pub fn unknown(dimension: Dimension) -> Self {
        Self {
            dimension,
            limit: None,
            used: 0,
            observed_at: None,
        }
    }
}

/// How an unknown or stale reading is turned into a number, since it must
/// never become an unlimited one.
///
/// Both knobs already exist in `CtxConfig` (`fallback.unknown_headroom_pct`
/// and `pace.collector_max_age_secs`); this struct is how they reach a pure
/// function, and is the ONLY policy input to [`headroom`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EstimatePolicy {
    /// What a dimension with no reading at all is assumed to have left.
    /// `0.0` opts such a dimension out of placement entirely, exactly as
    /// `fallback.unknown_headroom_pct` already does for a blind harness.
    pub unknown_headroom_pct: f64,
    /// How old a reading may be and still bind.
    pub max_age_secs: u64,
    /// The fraction of a stale reading's headroom that is still credited.
    /// A reading that bound at 60% an hour ago is evidence, but weaker
    /// evidence than one from a minute ago, and crediting it in full is what
    /// let an exhausted account keep winning placements.
    pub stale_credit: f64,
}

impl Default for EstimatePolicy {
    fn default() -> Self {
        Self {
            unknown_headroom_pct: 25.0,
            max_age_secs: 900,
            stale_credit: 0.5,
        }
    }
}

/// One dimension's verdict: how much is left, and whether that number was
/// measured or assumed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Headroom {
    pub dimension: Dimension,
    pub pct: f64,
    pub provenance: Provenance,
}

impl Headroom {
    pub fn is_measured(&self) -> bool {
        self.provenance.is_measured()
    }
}

/// Turns one raw [`Reading`] into a [`Headroom`].
///
/// The three degradations, in order of how much they are trusted:
///
/// - A fresh reading with a stated limit is `Measured`, at its real value.
/// - A reading whose limit was never stated, or that has aged past
///   `max_age_secs`, is `Estimated`. A stale one keeps `stale_credit` of what
///   it last showed (evidence, discounted); an unstated one falls back to
///   `unknown_headroom_pct` (no evidence at all).
/// - `unknown_headroom_pct <= 0` makes the unstated case `0.0`, which
///   excludes the route rather than guessing on the operator's behalf.
///
/// What is deliberately NOT here: treating a missing reading as 100%. That
/// is the failure this whole type exists to prevent -- an unmeasured
/// dimension is the one most likely to be the binding one.
pub fn headroom(reading: &Reading, now: u64, policy: &EstimatePolicy) -> Headroom {
    let unknown = |reason: &str| Headroom {
        dimension: reading.dimension,
        pct: policy.unknown_headroom_pct.clamp(0.0, 100.0),
        provenance: Provenance::Estimated {
            reason: reason.to_string(),
        },
    };
    let Some(limit) = reading.limit.filter(|limit| *limit > 0) else {
        return unknown("no limit reported for this dimension");
    };
    let Some(observed_at) = reading.observed_at else {
        return unknown("no reading for this dimension");
    };
    let remaining = limit.saturating_sub(reading.used);
    let raw = (remaining as f64 / limit as f64) * 100.0;
    let age = now.saturating_sub(observed_at);
    if age > policy.max_age_secs {
        return Headroom {
            dimension: reading.dimension,
            pct: (raw * policy.stale_credit.clamp(0.0, 1.0)).clamp(0.0, 100.0),
            provenance: Provenance::Estimated {
                reason: format!(
                    "reading is {} old, past the {} freshness limit",
                    crate::style::format_age(age),
                    crate::style::format_age(policy.max_age_secs)
                ),
            },
        };
    }
    Headroom {
        dimension: reading.dimension,
        pct: raw.clamp(0.0, 100.0),
        provenance: Provenance::Measured,
    }
}

/// The dimension that binds: the tightest headroom across every reading,
/// ties broken by [`Dimension`]'s own declaration order so the result is
/// deterministic. `None` only for a route with no dimensions at all.
pub fn binding(readings: &[Reading], now: u64, policy: &EstimatePolicy) -> Option<Headroom> {
    readings
        .iter()
        .map(|reading| headroom(reading, now, policy))
        .min_by(|a, b| {
            a.pct
                .partial_cmp(&b.pct)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.dimension.cmp(&b.dimension))
        })
}

/// A task capability a route must actually support. Deliberately a small
/// closed set: these are the four things a placement can get wrong in a way
/// no retry fixes, and an open string set would invite a typo to read as a
/// missing capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    ToolCalls,
    Streaming,
    Vision,
    ReplayableReasoning,
}

impl Capability {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ToolCalls => "tool calls",
            Self::Streaming => "streaming",
            Self::Vision => "vision",
            Self::ReplayableReasoning => "replayable reasoning",
        }
    }
}

/// How a route is paid for. Mirrors `provider::BillingClass`, with the
/// "nobody is billed" case local runtimes need -- an Ollama route has no
/// ceiling to authorize and no invoice to move work onto.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BillingPosture {
    /// Metered API credit: every token is billable.
    #[default]
    Api,
    /// A subscription seat: tokens are inside a window the operator already
    /// paid for, and are reported as unpriced rather than as free.
    Subscription,
    /// A local runtime. No credential, no invoice, no ceiling.
    Local,
}

impl BillingPosture {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Subscription => "subscription",
            Self::Local => "local",
        }
    }

    /// Whether usage on this route turns into money.
    pub fn is_billable(&self) -> bool {
        matches!(self, Self::Api)
    }
}

/// Everything a route offers, as one value, so eligibility is a function
/// rather than a scattering of `if`s across the ranking loop.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RouteOffer {
    pub identity: RouteIdentity,
    pub capabilities: BTreeSet<Capability>,
    pub billing: BillingPosture,
    /// The route's own context window. `None` means the model's window is
    /// not stated, which is treated as "cannot prove it fits" rather than as
    /// "fits" -- see [`Ineligible::NoContextRoom`].
    pub context_window_tokens: Option<u64>,
    pub readings: Vec<Reading>,
    /// `false` when operator policy does not permit this route at all
    /// (`[native.policy] allowed_routes`, a `LegacyOnly` vendor, a repo
    /// layer that narrowed the set). Carries its own reason.
    pub policy: PolicyVerdict,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "policy", rename_all = "kebab-case")]
pub enum PolicyVerdict {
    #[default]
    Allowed,
    Refused {
        reason: String,
    },
}

/// What a unit of work needs from whatever route takes it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Demand {
    pub capabilities: BTreeSet<Capability>,
    /// Tokens the prompt itself will occupy, which the route's context
    /// window has to hold.
    pub context_tokens: u64,
    /// What the operator has authorized this work to be billed to. A route
    /// whose posture is not in this set is excluded BEFORE ranking, which is
    /// what stops subscription work sliding onto paid API credit because the
    /// subscription happened to be busy.
    pub authorized_billing: BTreeSet<BillingPosture>,
    /// The route the operator configured as preferred, if any. Never a
    /// requirement -- it breaks ties and it is what recovery reclaims work
    /// to; it is not permission to ignore capacity or health.
    pub preferred: Option<String>,
}

impl Demand {
    /// The default authorization: whatever the work is already on. An empty
    /// set would exclude everything, so an unset field means "no billing
    /// constraint was stated", which is the pre-N18 behaviour.
    pub fn authorizes(&self, billing: BillingPosture) -> bool {
        self.authorized_billing.is_empty() || self.authorized_billing.contains(&billing)
    }
}

/// Why a route was excluded before it was ever ranked.
///
/// Each of these is a fact about fit, not about capacity or health: a route
/// that cannot run the task at all must not compete for it, and must not be
/// reported as "outranked" when the truth is "could never have taken it".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "ineligible", rename_all = "kebab-case")]
pub enum Ineligible {
    PolicyRefused {
        reason: String,
    },
    MissingCapability {
        capability: Capability,
    },
    NoContextRoom {
        need: u64,
        have: Option<u64>,
    },
    /// The route is paid for in a way this work is not authorized to use.
    /// Item 6's hard rule: moving subscription work onto metered API credit
    /// is a billing decision, so it is refused with a reason rather than
    /// taken silently.
    UnauthorizedBilling {
        posture: BillingPosture,
        authorized: Vec<BillingPosture>,
    },
}

impl Ineligible {
    pub fn label(&self) -> String {
        match self {
            Self::PolicyRefused { reason } => format!("refused by policy: {reason}"),
            Self::MissingCapability { capability } => {
                format!("does not support {}", capability.as_str())
            }
            Self::NoContextRoom {
                need,
                have: Some(have),
            } => format!("context window too small ({need} tokens needed, {have} available)"),
            Self::NoContextRoom { need, have: None } => {
                format!("context window unknown, so {need} tokens cannot be shown to fit")
            }
            Self::UnauthorizedBilling {
                posture,
                authorized,
            } => {
                let allowed: Vec<&str> = authorized.iter().map(BillingPosture::as_str).collect();
                format!(
                    "billed as {} which this work is not authorized for (authorized: {})",
                    posture.as_str(),
                    if allowed.is_empty() {
                        "none".to_string()
                    } else {
                        allowed.join(", ")
                    }
                )
            }
        }
    }
}

/// Whether `offer` can take `demand` AT ALL, judged before any ranking.
///
/// The order is the order the reasons matter in: policy first (an operator's
/// refusal outranks every technical fact), then capability, then context
/// room, then billing authorization. A route clearing all four is eligible;
/// whether it WINS is `allocator::place`'s question, and whether it is
/// reachable is `health`'s.
pub fn eligible(offer: &RouteOffer, demand: &Demand) -> Result<(), Ineligible> {
    if let PolicyVerdict::Refused { reason } = &offer.policy {
        return Err(Ineligible::PolicyRefused {
            reason: reason.clone(),
        });
    }
    if let Some(missing) = demand
        .capabilities
        .iter()
        .find(|capability| !offer.capabilities.contains(capability))
    {
        return Err(Ineligible::MissingCapability {
            capability: *missing,
        });
    }
    if demand.context_tokens > 0
        && !offer
            .context_window_tokens
            .is_some_and(|window| window >= demand.context_tokens)
    {
        return Err(Ineligible::NoContextRoom {
            need: demand.context_tokens,
            have: offer.context_window_tokens,
        });
    }
    if !demand.authorizes(offer.billing) {
        let mut authorized: Vec<BillingPosture> =
            demand.authorized_billing.iter().copied().collect();
        authorized.sort_by_key(|posture| posture.as_str());
        return Err(Ineligible::UnauthorizedBilling {
            posture: offer.billing,
            authorized,
        });
    }
    Ok(())
}

/// Every configured native route as a [`RouteOffer`], in route-id order.
///
/// The single conversion point from `provider::config`'s vocabulary into the
/// placement model's, so the account/pool distinction, the billing posture
/// and the policy refusal are each derived exactly once. Capabilities and
/// the context window come from the route's bound profile
/// (`provider::capability::declared`), which is the only place that vendor
/// fact is stated.
///
/// Not yet called from production code: the snapshot producer that turns
/// these into `allocator::HarnessCapacity` rows lands with the step that
/// gives native routes their per-minute usage readings, and until it does
/// there is nothing for a native row's capacity dimensions to report. Kept
/// `pub` and exercised by this module's own tests now -- the same
/// task-ordering shape `allocator::plan` already documents for itself.
#[allow(dead_code)]
pub fn offers_from_config(config: &super::provider::config::NativeConfig) -> Vec<RouteOffer> {
    use super::provider::{BillingClass, capability};

    let allowed = config.allowed_routes();
    config
        .routes
        .iter()
        .map(|(route_id, route)| {
            let account = config.accounts.get(&route.account);
            let provider = account.map(|a| a.provider.as_ref().to_string());
            let endpoint = route
                .endpoint
                .as_ref()
                .map(|id| id.as_ref().to_string())
                .or_else(|| provider.clone())
                .unwrap_or_default();
            // A route with no credential at all is a local runtime
            // (`CredentialClass::LocalNone`/`LocalOptional`): real usage, no
            // invoice and no ceiling to authorize. Reported as unpriced
            // rather than as free, so a spend readout is neither inflated
            // nor silently incomplete.
            let billing = match account.map(|a| (a.billing, a.credential.is_some())) {
                Some((_, false)) => BillingPosture::Local,
                Some((BillingClass::Subscription, _)) => BillingPosture::Subscription,
                _ => BillingPosture::Api,
            };
            let spec = account.and_then(|a| super::provider::provider(a.provider.as_ref()));
            let model_id = super::provider::ModelId {
                vendor: provider.clone().unwrap_or_default(),
                id: route.model.clone(),
            };
            let declared = spec.map(|spec| capability::declared(spec.protocol, &model_id, None));
            let mut capabilities: BTreeSet<Capability> = BTreeSet::new();
            if let Some(declared) = &declared {
                // Only a capability the vendor actually declares (or a probe
                // verified) is offered. `Unknown` is not a yes: a route that
                // has never said it supports tool calls must not be handed a
                // task that needs them.
                let mut add = |flag: capability::Capability, what: Capability| {
                    let supported = matches!(
                        flag,
                        capability::Capability::Declared { declared: true }
                            | capability::Capability::Verified { verified: true }
                    );
                    if supported {
                        capabilities.insert(what);
                    }
                };
                add(declared.tools, Capability::ToolCalls);
                add(declared.streaming, Capability::Streaming);
                add(declared.vision, Capability::Vision);
                add(declared.continuation, Capability::ReplayableReasoning);
            }
            RouteOffer {
                identity: RouteIdentity {
                    runtime: RuntimeKind::Native,
                    provider: provider.clone().unwrap_or_default(),
                    endpoint,
                    credential: route.account.as_ref().to_string(),
                    model: Some(route.model.clone()),
                    pool: config.account_pool(&route.account).as_ref().to_string(),
                },
                capabilities,
                billing,
                context_window_tokens: declared.and_then(|d| d.context_window),
                // A configured route reports nothing about its own
                // per-minute limits until it has run; `headroom` degrades
                // each missing dimension to a labelled estimate rather than
                // to free capacity.
                readings: Vec::new(),
                policy: if allowed.contains(route_id) {
                    PolicyVerdict::Allowed
                } else {
                    PolicyVerdict::Refused {
                        reason: "not in the operator's allowed_routes".to_string(),
                    }
                },
            }
        })
        .collect()
}

/// Which scope a provider failure is evidence about, and whether it is
/// evidence for the health breaker at all.
///
/// This is item 4 of #487, and the whole reason it is a type: folding all of
/// these into one per-route breaker meant one wrong API key disabled an
/// account's other models, one over-long prompt looked like an outage, and a
/// model that simply declined to answer counted as a failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "scope", rename_all = "kebab-case")]
pub enum FailureRouting {
    /// A shared transport or server failure: every route on this endpoint is
    /// affected, because the failing hop is the endpoint itself.
    Endpoint {
        endpoint: String,
        class: ProviderErrorClass,
    },
    /// The provider rejected the caller. This credential only -- a sibling
    /// account at the same endpoint is untouched.
    Credential { credential: String },
    /// This account may not use this model, or the request was not valid for
    /// it. That model only: the account's other models still work.
    Model {
        model: Option<String>,
        credential: String,
    },
    /// A rate limit, against the billing POOL rather than the route, with
    /// whatever `Retry-After` the provider sent. Capacity, not health:
    /// `pace`/`fallback` own it and no breaker opens.
    RateLimit {
        pool: String,
        retry_after_secs: Option<u64>,
    },
    /// The prompt did not fit. Not a health failure at all -- it is a
    /// compaction signal, and routing the work elsewhere would send the same
    /// oversized prompt to a second provider.
    Compaction,
    /// Refusals, cancellations and everything unattributed. Never a breaker
    /// input: a model declining to answer says nothing about whether the
    /// route works, and a cancellation is zirv's own doing.
    Ignored { reason: String },
}

impl FailureRouting {
    /// The observation class the health breaker should fold, and the key it
    /// belongs to. `None` for everything that is not breaker evidence.
    pub fn breaker_input(&self) -> Option<(String, ProviderErrorClass)> {
        match self {
            Self::Endpoint { endpoint, class } => Some((endpoint.clone(), *class)),
            Self::Credential { credential } => Some((credential.clone(), ProviderErrorClass::Auth)),
            Self::Model { model, credential } => Some((
                match model {
                    Some(model) => format!("{credential}/{model}"),
                    None => credential.clone(),
                },
                ProviderErrorClass::Auth,
            )),
            Self::RateLimit { .. } | Self::Compaction | Self::Ignored { .. } => None,
        }
    }

    /// The breaker record this failure belongs in, with the class to fold.
    /// `None` for everything that is not breaker evidence, which is the
    /// whole point of the type: a rate limit, an overflow, a refusal and a
    /// cancellation reach no breaker at all.
    pub fn breaker_key(&self) -> Option<(super::health::RouteKey, ProviderErrorClass)> {
        use super::health::{RouteKey, RouteScope};
        let (id, class) = self.breaker_input()?;
        let scope = match self {
            Self::Endpoint { .. } => RouteScope::Endpoint,
            Self::Credential { .. } => RouteScope::Credential,
            Self::Model { .. } => RouteScope::Model,
            _ => return None,
        };
        Some((RouteKey::scoped(scope, &id), class))
    }

    pub fn label(&self) -> String {
        match self {
            Self::Endpoint { endpoint, .. } => format!("endpoint {endpoint}"),
            Self::Credential { credential } => format!("credential {credential}"),
            Self::Model {
                model: Some(model), ..
            } => format!("model {model}"),
            Self::Model { model: None, .. } => "model".to_string(),
            Self::RateLimit { pool, .. } => format!("rate limit on pool {pool}"),
            Self::Compaction => "context overflow (compaction signal)".to_string(),
            Self::Ignored { reason } => reason.clone(),
        }
    }
}

/// Maps one typed [`ProviderFailure`] onto the scope it is evidence about.
///
/// The failure's OWN `scope` is honoured where it is more specific than the
/// class would imply (an adapter that knows a 500 came from one model says
/// so), and the class decides otherwise. Nothing here consults a clock or
/// any state: the same failure against the same identity always routes the
/// same way.
pub fn route_failure(failure: &ProviderFailure, identity: &RouteIdentity) -> FailureRouting {
    match failure.class {
        // Not health evidence, in either direction.
        FailureClass::Cancelled => FailureRouting::Ignored {
            reason: "cancelled by zirv".to_string(),
        },
        FailureClass::ContextOverflow => FailureRouting::Compaction,
        FailureClass::InvalidToolArguments => FailureRouting::Ignored {
            reason: "the model produced unusable tool arguments".to_string(),
        },

        // Capacity, against the pool that is actually being metered.
        FailureClass::RateLimited => FailureRouting::RateLimit {
            pool: identity.pool.clone(),
            retry_after_secs: failure.retry.after_ms.map(|ms| ms.div_ceil(1_000)),
        },

        // This credential. An entitlement failure is the account's plan, not
        // the endpoint's health, so it must not deny a sibling account.
        FailureClass::Authentication | FailureClass::Permission | FailureClass::Entitlement => {
            FailureRouting::Credential {
                credential: identity.credential.clone(),
            }
        }

        // This model, on this credential.
        FailureClass::ModelAccess | FailureClass::Configuration => FailureRouting::Model {
            model: identity.model.clone(),
            credential: identity.credential.clone(),
        },

        // The endpoint. `FirstEventTimeout`/`IdleTimeout` are transport --
        // the connection produced nothing -- while `Overloaded` and
        // `Provider` reached the endpoint and it failed, which is `Server`.
        FailureClass::Transport | FailureClass::FirstEventTimeout | FailureClass::IdleTimeout => {
            FailureRouting::Endpoint {
                endpoint: identity.endpoint.clone(),
                class: ProviderErrorClass::Transport,
            }
        }
        FailureClass::Overloaded | FailureClass::Provider => FailureRouting::Endpoint {
            endpoint: identity.endpoint.clone(),
            class: ProviderErrorClass::Server,
        },
        // A stream zirv could not parse names no hop at all. Recording it
        // against the endpoint would open a breaker on a bug of zirv's own.
        FailureClass::InvalidStream => FailureRouting::Ignored {
            reason: "the provider stream could not be parsed".to_string(),
        },
    }
    // A failure that named a MORE specific scope than the class implies is
    // believed: an adapter that knows which hop failed knows better than a
    // status code does. It may only ever narrow -- widening a per-model
    // error into an endpoint outage is how one bad model takes an account
    // down.
    .narrowed_by(&failure.scope.kind, identity)
}

impl FailureRouting {
    fn narrowed_by(self, kind: &FailureScopeKind, identity: &RouteIdentity) -> Self {
        match (self, kind) {
            (Self::Endpoint { .. }, FailureScopeKind::Model) => Self::Model {
                model: identity.model.clone(),
                credential: identity.credential.clone(),
            },
            (Self::Endpoint { .. }, FailureScopeKind::Account) => Self::Credential {
                credential: identity.credential.clone(),
            },
            (Self::Credential { .. }, FailureScopeKind::Model) => Self::Model {
                model: identity.model.clone(),
                credential: identity.credential.clone(),
            },
            (routing, _) => routing,
        }
    }
}

/// One request's settled usage, as the provider reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct Settled {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Settled {
    pub fn total(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// The running reconciliation for one pool: what has been counted, and which
/// requests it was counted from.
///
/// `seen` is what makes [`reconcile`] idempotent. A provider request id is
/// the only identifier that survives a retry, a resumed journal and two
/// supervisors reading one run, so it -- not a call count -- is the key. The
/// ring is bounded for the same reason `health::RouteHealth`'s is: a long
/// session must not grow this record without limit.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Reconciliation {
    /// Tokens on a route that turns usage into money.
    pub billable_tokens: u64,
    /// Tokens inside a subscription window or on a local runtime: real
    /// usage, no invoice. Reported separately rather than folded into
    /// `billable_tokens` or dropped, so a spend readout is neither inflated
    /// nor silently incomplete.
    pub unpriced_tokens: u64,
    /// What was RESERVED for the requests in `seen`, so the drift between
    /// the estimate and the settlement is visible instead of implied.
    pub reserved_tokens: u64,
    pub requests: u64,
    pub seen: Vec<String>,
}

/// How many request ids one [`Reconciliation`] remembers. Sized like
/// `health::MAX_SAMPLES`: large enough that an ordinary turn's retries can
/// never push a still-live request out, small enough to stay a record.
pub const MAX_SEEN_REQUESTS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "reconciled", rename_all = "kebab-case")]
pub enum Reconciled {
    /// Folded in for the first time.
    Applied,
    /// This provider request was already counted. The ledger is returned
    /// unchanged, which is the whole contract: a retry that turns out to
    /// have succeeded, or a replayed journal, must not double the bill.
    AlreadyCounted,
}

/// Folds one completed request into `ledger`, exactly once.
///
/// `request_key` is the provider's own request id where there is one; a
/// caller with no id must synthesize a key that is stable for the REQUEST
/// (never a fresh uuid per attempt), because an unstable key is
/// indistinguishable from a new request and defeats the guard.
///
/// `reserved` is what was held for this request before it ran, and is added
/// once alongside the settlement so the reservation ledger can be closed out
/// against the truth rather than against another estimate.
pub fn reconcile(
    ledger: &Reconciliation,
    request_key: &str,
    settled: Settled,
    reserved: u64,
    billing: BillingPosture,
) -> (Reconciliation, Reconciled) {
    if ledger.seen.iter().any(|seen| seen == request_key) {
        return (ledger.clone(), Reconciled::AlreadyCounted);
    }
    let mut next = ledger.clone();
    let total = settled.total();
    if billing.is_billable() {
        next.billable_tokens = next.billable_tokens.saturating_add(total);
    } else {
        next.unpriced_tokens = next.unpriced_tokens.saturating_add(total);
    }
    next.reserved_tokens = next.reserved_tokens.saturating_add(reserved);
    next.requests = next.requests.saturating_add(1);
    next.seen.push(request_key.to_string());
    if next.seen.len() > MAX_SEEN_REQUESTS {
        let drop = next.seen.len() - MAX_SEEN_REQUESTS;
        next.seen.drain(..drop);
    }
    (next, Reconciled::Applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ctx::provider::adapter::{FailureScope, RetryHint};

    fn identity() -> RouteIdentity {
        RouteIdentity {
            runtime: RuntimeKind::Native,
            provider: "anthropic".to_string(),
            endpoint: "api.anthropic.com".to_string(),
            credential: "work".to_string(),
            model: Some("claude-opus".to_string()),
            pool: "work".to_string(),
        }
    }

    fn failure(class: FailureClass, kind: FailureScopeKind) -> ProviderFailure {
        ProviderFailure {
            class,
            scope: FailureScope { kind, id: None },
            message: "boom".to_string(),
            http_status: None,
            provider_request_id: None,
            retry: RetryHint::default(),
        }
    }

    /// Criterion 1, both halves: two routes on ONE account are one pool (so
    /// they cannot be ranked as two capacities), and two accounts at one
    /// provider are two pools (so one running dry cannot refuse the other).
    #[test]
    fn one_account_is_one_pool_and_two_accounts_at_one_provider_are_not() {
        let opus = identity();
        let sonnet = RouteIdentity {
            model: Some("claude-sonnet".to_string()),
            ..identity()
        };
        assert!(
            opus.shares_pool(&sonnet),
            "two models on one account draw on one balance"
        );

        let personal = RouteIdentity {
            credential: "personal".to_string(),
            pool: "personal".to_string(),
            ..identity()
        };
        assert!(
            !opus.shares_pool(&personal),
            "two accounts at one provider are two balances"
        );
        assert!(
            opus.shares_endpoint(&personal),
            "but they do share the host, which is what an outage couples"
        );
    }

    /// Item 2: an unmeasured dimension is the one most likely to be binding,
    /// so it degrades to a conservative bound and says so -- never to free
    /// capacity.
    #[test]
    fn an_unknown_dimension_is_a_conservative_estimate_not_free_capacity() {
        let policy = EstimatePolicy::default();
        let unknown = headroom(
            &Reading::unknown(Dimension::TokensPerMinute),
            1_000,
            &policy,
        );
        assert_eq!(unknown.pct, policy.unknown_headroom_pct);
        assert!(!unknown.is_measured(), "{unknown:?}");
        assert!(unknown.provenance.reason().is_some());

        let measured = headroom(
            &Reading::new(Dimension::TokensPerMinute, Some(1_000), 200, 1_000),
            1_010,
            &policy,
        );
        assert_eq!(measured.pct, 80.0);
        assert!(measured.is_measured(), "{measured:?}");
    }

    /// A stale reading is discounted evidence, labelled as an estimate --
    /// not full-value evidence, and not nothing.
    #[test]
    fn a_stale_reading_is_discounted_and_labelled_an_estimate() {
        let policy = EstimatePolicy::default();
        let reading = Reading::new(Dimension::SubscriptionWindow, Some(100), 40, 1_000);
        let fresh = headroom(&reading, 1_100, &policy);
        assert_eq!(fresh.pct, 60.0);
        assert!(fresh.is_measured());

        let stale = headroom(&reading, 1_000 + policy.max_age_secs + 1, &policy);
        assert_eq!(
            stale.pct, 30.0,
            "half credit for a reading nothing refreshed"
        );
        assert!(stale.provenance.reason().is_some_and(|r| r.contains("old")));
    }

    /// The dimensions bind independently: a full subscription window does
    /// not license a request that the per-minute cap has no room for.
    #[test]
    fn the_tightest_dimension_binds_whichever_one_it_is() {
        let policy = EstimatePolicy::default();
        let readings = vec![
            Reading::new(Dimension::SubscriptionWindow, Some(1_000), 100, 1_000),
            Reading::new(Dimension::RequestsPerMinute, Some(10), 9, 1_000),
            Reading::new(Dimension::SpendCeiling, Some(100), 20, 1_000),
        ];
        let binding = binding(&readings, 1_010, &policy).expect("a binding dimension");
        assert_eq!(binding.dimension, Dimension::RequestsPerMinute);
        assert_eq!(binding.pct, 10.0);
    }

    /// Item 5: routes that cannot take the task are excluded BEFORE ranking,
    /// each with its own reason, so nothing reads as "outranked" when the
    /// truth is "could never have run it".
    #[test]
    fn ineligible_routes_are_named_by_their_own_reason() {
        let base = RouteOffer {
            identity: identity(),
            capabilities: [Capability::ToolCalls, Capability::Streaming]
                .into_iter()
                .collect(),
            billing: BillingPosture::Api,
            context_window_tokens: Some(200_000),
            readings: Vec::new(),
            policy: PolicyVerdict::Allowed,
        };
        let demand = Demand {
            capabilities: [Capability::ToolCalls].into_iter().collect(),
            context_tokens: 100_000,
            ..Demand::default()
        };
        assert_eq!(eligible(&base, &demand), Ok(()));

        let refused = RouteOffer {
            policy: PolicyVerdict::Refused {
                reason: "not in allowed_routes".to_string(),
            },
            ..base.clone()
        };
        assert!(matches!(
            eligible(&refused, &demand),
            Err(Ineligible::PolicyRefused { .. })
        ));

        let no_vision = Demand {
            capabilities: [Capability::Vision].into_iter().collect(),
            ..demand.clone()
        };
        assert_eq!(
            eligible(&base, &no_vision),
            Err(Ineligible::MissingCapability {
                capability: Capability::Vision
            })
        );

        let cramped = RouteOffer {
            context_window_tokens: Some(8_000),
            ..base.clone()
        };
        assert_eq!(
            eligible(&cramped, &demand),
            Err(Ineligible::NoContextRoom {
                need: 100_000,
                have: Some(8_000)
            })
        );

        let unstated = RouteOffer {
            context_window_tokens: None,
            ..base.clone()
        };
        assert_eq!(
            eligible(&unstated, &demand),
            Err(Ineligible::NoContextRoom {
                need: 100_000,
                have: None
            }),
            "an unstated window cannot be shown to fit, so it does not"
        );
    }

    /// Item 6: subscription work does not slide onto metered credit because
    /// the subscription is busy. The refusal is typed and names what WAS
    /// authorized.
    #[test]
    fn a_route_billed_outside_the_authorization_is_refused_not_silently_taken() {
        let paid = RouteOffer {
            identity: identity(),
            capabilities: BTreeSet::new(),
            billing: BillingPosture::Api,
            context_window_tokens: Some(200_000),
            readings: Vec::new(),
            policy: PolicyVerdict::Allowed,
        };
        let subscription_only = Demand {
            authorized_billing: [BillingPosture::Subscription].into_iter().collect(),
            ..Demand::default()
        };
        let refused = eligible(&paid, &subscription_only).expect_err("must refuse");
        assert!(
            matches!(
                refused,
                Ineligible::UnauthorizedBilling {
                    posture: BillingPosture::Api,
                    ..
                }
            ),
            "{refused:?}"
        );
        assert!(refused.label().contains("subscription"));

        // The same route is fine once the operator authorizes it, and an
        // unstated authorization constrains nothing (the pre-N18 behaviour).
        let both = Demand {
            authorized_billing: [BillingPosture::Api, BillingPosture::Subscription]
                .into_iter()
                .collect(),
            ..Demand::default()
        };
        assert_eq!(eligible(&paid, &both), Ok(()));
        assert_eq!(eligible(&paid, &Demand::default()), Ok(()));
    }

    /// Criterion 3 and item 4: five classes, five distinct decisions. The
    /// assertion that matters is that only two of them are breaker input at
    /// all, and that the two that are name DIFFERENT scopes.
    #[test]
    fn each_failure_class_reaches_its_own_scope_and_only_some_are_breaker_input() {
        let id = identity();

        let proxy = route_failure(
            &failure(FailureClass::Provider, FailureScopeKind::Request),
            &id,
        );
        assert_eq!(
            proxy,
            FailureRouting::Endpoint {
                endpoint: "api.anthropic.com".to_string(),
                class: ProviderErrorClass::Server,
            }
        );
        assert_eq!(
            proxy.breaker_input(),
            Some(("api.anthropic.com".to_string(), ProviderErrorClass::Server))
        );

        let auth = route_failure(
            &failure(FailureClass::Authentication, FailureScopeKind::Request),
            &id,
        );
        assert_eq!(
            auth,
            FailureRouting::Credential {
                credential: "work".to_string()
            }
        );
        assert_eq!(
            auth.breaker_input(),
            Some(("work".to_string(), ProviderErrorClass::Auth)),
            "a credential failure is its own key, never the endpoint's"
        );

        let model = route_failure(
            &failure(FailureClass::ModelAccess, FailureScopeKind::Request),
            &id,
        );
        assert_eq!(
            model.breaker_input(),
            Some(("work/claude-opus".to_string(), ProviderErrorClass::Auth)),
            "a model mismatch is scoped to that model on that credential"
        );

        let mut limited = failure(FailureClass::RateLimited, FailureScopeKind::BillingPool);
        limited.retry.after_ms = Some(30_000);
        let limited = route_failure(&limited, &id);
        assert_eq!(
            limited,
            FailureRouting::RateLimit {
                pool: "work".to_string(),
                retry_after_secs: Some(30),
            }
        );
        assert_eq!(
            limited.breaker_input(),
            None,
            "a rate limit is capacity; no breaker opens"
        );

        let overflow = route_failure(
            &failure(FailureClass::ContextOverflow, FailureScopeKind::Request),
            &id,
        );
        assert_eq!(overflow, FailureRouting::Compaction);
        assert_eq!(overflow.breaker_input(), None);

        for ignored in [FailureClass::Cancelled, FailureClass::InvalidToolArguments] {
            let routed = route_failure(&failure(ignored, FailureScopeKind::Request), &id);
            assert!(
                matches!(routed, FailureRouting::Ignored { .. }),
                "{routed:?}"
            );
            assert_eq!(routed.breaker_input(), None);
        }
    }

    /// Criterion 2: an endpoint outage trips the routes that share the
    /// endpoint and NOTHING else -- a sibling account on the same host is
    /// denied, a route on a different host is not, and a credential failure
    /// never widens to either.
    #[test]
    fn an_endpoint_outage_is_scoped_to_that_endpoint_alone() {
        let shared = identity();
        let sibling_account = RouteIdentity {
            credential: "personal".to_string(),
            pool: "personal".to_string(),
            ..identity()
        };
        let elsewhere = RouteIdentity {
            endpoint: "bedrock.us-east-1.amazonaws.com".to_string(),
            ..identity()
        };

        let outage = route_failure(
            &failure(FailureClass::Transport, FailureScopeKind::Endpoint),
            &shared,
        );
        let (key, _) = outage.breaker_input().expect("breaker input");
        assert_eq!(key, shared.endpoint);
        assert!(shared.shares_endpoint(&sibling_account), "denied with it");
        assert!(!shared.shares_endpoint(&elsewhere), "untouched");

        // An auth failure on the shared endpoint denies only its own key.
        let auth = route_failure(
            &failure(FailureClass::Authentication, FailureScopeKind::Account),
            &shared,
        );
        let (auth_key, _) = auth.breaker_input().expect("breaker input");
        assert_eq!(auth_key, shared.credential);
        assert_ne!(auth_key, sibling_account.credential);
    }

    /// A failure that names a narrower scope than its class implies is
    /// believed; one that names a wider scope is not, because one bad model
    /// must never be allowed to take an endpoint down.
    #[test]
    fn a_failures_own_scope_may_narrow_the_routing_but_never_widen_it() {
        let id = identity();
        let narrowed = route_failure(
            &failure(FailureClass::Provider, FailureScopeKind::Model),
            &id,
        );
        assert!(
            matches!(narrowed, FailureRouting::Model { .. }),
            "a 5xx an adapter attributes to one model stays there: {narrowed:?}"
        );

        let not_widened = route_failure(
            &failure(FailureClass::ModelAccess, FailureScopeKind::Endpoint),
            &id,
        );
        assert!(
            matches!(not_widened, FailureRouting::Model { .. }),
            "a model error claiming endpoint scope is still a model error: {not_widened:?}"
        );
    }

    /// Criterion 4: every request reconciles exactly once, into the pool's
    /// own ledger, whatever replays it.
    #[test]
    fn a_request_reconciles_exactly_once_however_often_it_is_replayed() {
        let settled = Settled {
            input_tokens: 900,
            output_tokens: 100,
        };
        let (after, first) = reconcile(
            &Reconciliation::default(),
            "req-1",
            settled,
            1_200,
            BillingPosture::Api,
        );
        assert_eq!(first, Reconciled::Applied);
        assert_eq!(after.billable_tokens, 1_000);
        assert_eq!(after.reserved_tokens, 1_200);
        assert_eq!(after.requests, 1);

        let (again, second) = reconcile(&after, "req-1", settled, 1_200, BillingPosture::Api);
        assert_eq!(second, Reconciled::AlreadyCounted);
        assert_eq!(again, after, "a replay changes nothing at all");

        // A different request does count, and a subscription route's tokens
        // are real usage with no invoice rather than billable or dropped.
        let (third, verdict) =
            reconcile(&after, "req-2", settled, 800, BillingPosture::Subscription);
        assert_eq!(verdict, Reconciled::Applied);
        assert_eq!(third.billable_tokens, 1_000, "unchanged");
        assert_eq!(third.unpriced_tokens, 1_000);
        assert_eq!(third.requests, 2);
        assert_eq!(
            third.reserved_tokens, 2_000,
            "the estimate is kept beside the settlement, so the drift is visible"
        );
    }

    /// The conversion from configured routes to offers, which is where the
    /// account/pool distinction, the billing posture and the operator's own
    /// refusal are each derived exactly once.
    #[test]
    fn every_configured_route_becomes_an_offer_with_its_own_pool_and_posture() {
        let config: super::super::provider::config::NativeConfig = toml::from_str(
            "schema=1\n\
             [account.work]\nprovider='anthropic'\ncredential='env:KEY'\n\
             [account.seat]\nprovider='anthropic'\ncredential='env:SEAT'\nbilling='subscription'\n\
             [account.local]\nprovider='ollama'\n\
             [route.opus]\naccount='work'\nmodel='claude-opus'\n\
             [route.sonnet]\naccount='work'\nmodel='claude-sonnet'\n\
             [route.seated]\naccount='seat'\nmodel='claude-opus'\n\
             [route.llama]\naccount='local'\nmodel='llama3'\n\
             [policy]\nallowed_routes=['opus','sonnet','seated']\n",
        )
        .expect("a parseable native config");

        let offers = offers_from_config(&config);
        let by_model = |model: &str| {
            offers
                .iter()
                .find(|offer| offer.identity.model.as_deref() == Some(model))
                .cloned()
                .expect("offer")
        };
        let opus = offers
            .iter()
            .find(|o| {
                o.identity.credential == "work"
                    && o.identity.model.as_deref() == Some("claude-opus")
            })
            .expect("opus offer");
        let sonnet = by_model("claude-sonnet");
        let seated = offers
            .iter()
            .find(|o| o.identity.credential == "seat")
            .expect("seat offer");
        let local = by_model("llama3");

        assert!(
            opus.identity.shares_pool(&sonnet.identity),
            "two routes on one account are one balance"
        );
        assert!(
            !opus.identity.shares_pool(&seated.identity),
            "a second account at the same vendor is not"
        );
        assert_eq!(opus.billing, BillingPosture::Api);
        assert_eq!(seated.billing, BillingPosture::Subscription);
        assert_eq!(
            local.billing,
            BillingPosture::Local,
            "a route with no credential is a local runtime, not a billable one"
        );
        assert!(
            matches!(local.policy, PolicyVerdict::Refused { .. }),
            "a route the operator left out of allowed_routes is refused by policy, \
             with its own reason: {:?}",
            local.policy
        );
        assert_eq!(opus.policy, PolicyVerdict::Allowed);
        // Every declared dimension is absent, which `headroom` degrades to a
        // labelled estimate rather than to unlimited capacity.
        assert!(opus.readings.is_empty());
    }

    #[test]
    fn the_seen_request_ring_is_bounded() {
        let mut ledger = Reconciliation::default();
        for i in 0..(MAX_SEEN_REQUESTS * 2) {
            let (next, verdict) = reconcile(
                &ledger,
                &format!("req-{i}"),
                Settled::default(),
                0,
                BillingPosture::Local,
            );
            assert_eq!(verdict, Reconciled::Applied);
            ledger = next;
        }
        assert_eq!(ledger.seen.len(), MAX_SEEN_REQUESTS);
        assert_eq!(ledger.requests, MAX_SEEN_REQUESTS as u64 * 2);
    }

    /// Criterion 5: the decisions are pure functions of their inputs plus an
    /// explicit clock, so the same evidence replays to the same verdict --
    /// and a different clock, with identical evidence, is the ONLY thing
    /// that can change one.
    #[test]
    fn placement_evidence_replays_to_the_identical_verdict() {
        let policy = EstimatePolicy::default();
        let readings = vec![
            Reading::new(Dimension::SubscriptionWindow, Some(1_000), 500, 1_000),
            Reading::unknown(Dimension::SpendCeiling),
        ];
        let first = binding(&readings, 1_100, &policy).expect("binding");
        let replayed = binding(&readings, 1_100, &policy).expect("binding");
        assert_eq!(first, replayed);

        // Only the clock moved, and the verdict changed accordingly -- and
        // says why, which is what "evidence provenance" buys.
        let later = binding(&readings, 1_000 + policy.max_age_secs + 1, &policy).expect("binding");
        assert_ne!(later, first);
        assert!(!later.is_measured(), "{later:?}");
    }
}
