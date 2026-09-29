//! Issue #800 (autoresearch): campaign/candidate/trial/task attribution
//! ridden on the EXISTING usage seams (`delegations.jsonl`, `jev-
//! decisions.jsonl`, `jev-effects.jsonl`, `proxy-decisions.jsonl`), plus the
//! pure reconciler (`reconcile`) that folds them into one [`SpendReport`] --
//! no new billing ledger. See `docs/superpowers/specs/2026-09-26-
//! autoresearch-design.md`'s `#800` section for the full contract.
//!
//! Every id here is opaque and validated (`^[A-Za-z0-9._:-]{1,64}$`); an
//! invalid or empty value is dropped rather than stored, so a caller can
//! never smuggle raw task text or secrets into an attribution field by
//! mistake -- the same "never trust ambient env for anything sensitive"
//! posture the rest of this crate's env readers hold.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use clap::Args;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::config::CtxConfig;
use super::price::{self, PriceTable};
use super::state::StateDir;
use super::{CtxResult, log};

// -- Attribution ------------------------------------------------------------

pub const CAMPAIGN_ENV: &str = "ZIRV_ATTR_CAMPAIGN";
pub const CANDIDATE_ENV: &str = "ZIRV_ATTR_CANDIDATE";
pub const TRIAL_ENV: &str = "ZIRV_ATTR_TRIAL";
pub const TASK_ENV: &str = "ZIRV_ATTR_TASK";

/// The opaque id shape every attribution field is validated against: 1-64
/// bytes of `[A-Za-z0-9._:-]`. Never raw task text -- an id that fails this
/// is dropped outright, never truncated or sanitized into something that
/// still parses.
fn is_valid_id(value: &str) -> bool {
    let len = value.len();
    (1..=64).contains(&len)
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

/// Campaign/candidate/trial/logical-task ids for the autoresearch runner
/// (#802), read from `ZIRV_ATTR_CAMPAIGN|CANDIDATE|TRIAL|TASK`. Serializes
/// only the fields that are set (`is_empty` -> nothing at all), so an
/// existing row's JSON is byte-identical when no campaign is running.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attribution {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub campaign: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trial: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
}

impl Attribution {
    pub fn is_empty(&self) -> bool {
        self.campaign.is_none()
            && self.candidate.is_none()
            && self.trial.is_none()
            && self.task.is_none()
    }

    /// Reads this process's own `ZIRV_ATTR_*` env, dropping any value that
    /// does not match [`is_valid_id`]. Best-effort by construction: there is
    /// no failure mode other than "this field is unset".
    pub fn from_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    pub(crate) fn from_lookup(env: impl Fn(&str) -> Option<String>) -> Self {
        let valid = |key: &str| env(key).filter(|value| is_valid_id(value));
        Self {
            campaign: valid(CAMPAIGN_ENV),
            candidate: valid(CANDIDATE_ENV),
            trial: valid(TRIAL_ENV),
            task: valid(TASK_ENV),
        }
    }

    /// Whether `other` matches this attribution as a FILTER: every field
    /// `self` has set must equal the same field on `other`; an unset filter
    /// field matches anything. An empty filter (the default -- no campaign
    /// running) therefore matches every receipt, and two campaigns sharing
    /// one state dir can never cross-attribute, because each campaign's own
    /// non-empty `campaign` (at minimum) excludes the other's rows.
    pub(crate) fn matches(&self, other: &Attribution) -> bool {
        fn field_matches(filter: &Option<String>, value: &Option<String>) -> bool {
            filter
                .as_deref()
                .is_none_or(|want| value.as_deref() == Some(want))
        }
        field_matches(&self.campaign, &other.campaign)
            && field_matches(&self.candidate, &other.candidate)
            && field_matches(&self.trial, &other.trial)
            && field_matches(&self.task, &other.task)
    }
}

// -- Route env (issue #800: the headless launch seam's own env export) ------

pub const ROUTE_HARNESS_ENV: &str = "ZIRV_ROUTE_HARNESS";
pub const ROUTE_MODEL_ENV: &str = "ZIRV_ROUTE_MODEL";
pub const ROUTE_TIER_ENV: &str = "ZIRV_ROUTE_TIER";
pub const ROUTE_EFFORT_ENV: &str = "ZIRV_ROUTE_EFFORT";

/// The `ZIRV_ROUTE_*` pairs `zirv ctx exec`'s own child-env builder
/// (`exec.rs`'s `turn_env_for`) exports for a launched agent process --
/// `ZIRV_CTX_MODEL` was already taken by `[handover].model`, hence the
/// distinct `ZIRV_ROUTE_*` family. Every argument is `None` when unknown at
/// the point of launch; an unknown value is simply left unset rather than
/// guessed, matching this module's own "never invent a fact" rule.
pub fn route_env(
    harness: Option<&str>,
    model: Option<&str>,
    tier: Option<&str>,
    effort: Option<&str>,
) -> Vec<(String, String)> {
    let mut env = Vec::new();
    let mut push = |key: &str, value: Option<&str>| {
        if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
            env.push((key.to_string(), value.to_string()));
        }
    };
    push(ROUTE_HARNESS_ENV, harness);
    push(ROUTE_MODEL_ENV, model);
    push(ROUTE_TIER_ENV, tier);
    push(ROUTE_EFFORT_ENV, effort);
    env
}

// -- Policy fingerprint -------------------------------------------------

/// Sha256, first 16 hex chars, over the policy-relevant config subset: `[jev]`
/// gates/ttl/floors, `[proxy] min_confidence`/`min_margin`, the `[handover]` ladder,
/// `[headless.effort]`, and `[score]`'s token ratios. Built from a hand-picked
/// `serde_json::Value` (rather than deriving `Serialize` on the config structs themselves,
/// which stay load-only) so two configs that agree on every policy-relevant key -- whatever
/// else differs -- fingerprint identically; two that disagree on any of them never collide
/// by construction (the input is exact field values, not a lossy summary).
pub fn policy_fingerprint(cfg: &CtxConfig) -> String {
    let jev = &cfg.jev;
    let floor = |f: &super::config::JevSiteFloor| serde_json::json!({"min_confidence": f.min_confidence, "min_margin": f.min_margin});
    let value = serde_json::json!({
        "jev": {
            "memory": jev.memory,
            "supervisor": jev.supervisor,
            "dispatch": jev.dispatch,
            "review": jev.review,
            "gates": jev.gates,
            "context": jev.context,
            "intake_savings": jev.intake_savings,
            "review_reuse": jev.review_reuse,
            "harvest_screen": jev.harvest_screen,
            "admin_dispatch": jev.admin_dispatch,
            "approve": jev.approve,
            "approve_allow": jev.approve_allow,
            "classify": jev.classify,
            "handoff_select": jev.handoff_select,
            "compaction_select": jev.compaction_select,
            "inject_screen": jev.inject_screen,
            "inject": jev.inject,
            "stop_verify": jev.stop_verify,
            "missing_tests": jev.missing_tests,
            "launch_effort": jev.launch_effort,
            "cache_ttl_secs": jev.cache_ttl_secs,
            "floors": {
                "memory": floor(&jev.floors.memory),
                "context": floor(&jev.floors.context),
                "harvest_screen": floor(&jev.floors.harvest_screen),
                "handoff_select": floor(&jev.floors.handoff_select),
                "compaction_select": floor(&jev.floors.compaction_select),
                "dispatch": floor(&jev.floors.dispatch),
                "launch_effort": floor(&jev.floors.launch_effort),
                "classify": floor(&jev.floors.classify),
                "inject": floor(&jev.floors.inject),
            },
        },
        "proxy": {
            "min_confidence": cfg.proxy.min_confidence,
            "min_margin": cfg.proxy.min_margin,
        },
        "handover": {
            "claude": [&cfg.handover.claude.cheap, &cfg.handover.claude.standard, &cfg.handover.claude.deep],
            "codex": [&cfg.handover.codex.cheap, &cfg.handover.codex.standard, &cfg.handover.codex.deep],
        },
        "headless_effort": [
            &cfg.headless.effort.trivial,
            &cfg.headless.effort.bounded,
            &cfg.headless.effort.substantial,
        ],
        "score": {
            "token_floor_ratio": cfg.score.token_floor_ratio,
            "token_ceiling_ratio": cfg.score.token_ceiling_ratio,
        },
    });
    let bytes = serde_json::to_vec(&value).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

// -- Receipts and the reconciler ---------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Agent,
    Intake,
    Jev,
    Helper,
    Worker,
    Judge,
    Proposer,
}

impl Source {
    pub const ALL: [Source; 7] = [
        Source::Agent,
        Source::Intake,
        Source::Jev,
        Source::Helper,
        Source::Worker,
        Source::Judge,
        Source::Proposer,
    ];

    fn label(self) -> &'static str {
        match self {
            Source::Agent => "agent",
            Source::Intake => "intake",
            Source::Jev => "jev",
            Source::Helper => "helper",
            Source::Worker => "worker",
            Source::Judge => "judge",
            Source::Proposer => "proposer",
        }
    }

    /// `execution` = every source that is part of getting the task itself
    /// done; `overhead` = the autoresearch loop's own proposer/judge spend,
    /// never charged against the task it is evaluating.
    fn is_execution(self) -> bool {
        !matches!(self, Source::Judge | Source::Proposer)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Billing {
    Metered,
    Subscription,
    #[default]
    Unknown,
}

/// One priced or priceable event, folded from an existing log by
/// [`load_receipts`] or supplied externally via `zirv workflow spend
/// --receipts`. Never a new ledger of its own -- see this module's own doc
/// comment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub source: Source,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// An external id used to dedupe an exact repeat of the same event.
    /// `None` never matches another `None` for dedup purposes -- only a
    /// shared, non-empty id counts as the same receipt logged twice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_id: Option<String>,
    /// Whether this row's numeric fields are a running TOTAL for its
    /// `(session, source)` rather than this one event's own increment (a
    /// resumed session's harness-reported usage, typically).
    #[serde(default)]
    pub cumulative: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    /// Accept Anthropic cache usage field names as aliases so receipts retain their
    /// measured values.
    #[serde(default, alias = "cache_creation_input_tokens")]
    pub cache_creation_tokens: u64,
    #[serde(default, alias = "cache_read_input_tokens")]
    pub cache_read_tokens: u64,
    /// A cached Jev hit: a real call was skipped, so this receipt counts as
    /// a call with zero provider spend, never as unpriced/unknown usage.
    #[serde(default)]
    pub cached: bool,
    #[serde(default)]
    pub billing: Billing,
    #[serde(default, skip_serializing_if = "Attribution::is_empty")]
    pub attribution: Attribution,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Money {
    /// `null` (never omitted -- other lanes depend on this key always being
    /// present) when no receipt in this bucket carried a harness-reported
    /// cost.
    #[serde(default)]
    pub reported_usd: Option<f64>,
    /// `null` (never omitted) when nothing in this bucket was priced.
    #[serde(default)]
    pub estimated_usd: Option<f64>,
    #[serde(default)]
    pub unknown_count: u64,
    /// `null` (never omitted) when nothing in this bucket was priced.
    #[serde(default)]
    pub price_as_of: Option<String>,
    #[serde(default)]
    pub calls: u64,
}

impl Money {
    fn add_reported(&mut self, usd: f64) {
        self.reported_usd = Some(self.reported_usd.unwrap_or(0.0) + usd);
    }

    fn add_estimated(&mut self, usd: f64, as_of: &str) {
        self.estimated_usd = Some(self.estimated_usd.unwrap_or(0.0) + usd);
        self.price_as_of = Some(as_of.to_string());
    }

    fn merge(&mut self, other: &Money) {
        if let Some(usd) = other.reported_usd {
            self.add_reported(usd);
        }
        if let Some(usd) = other.estimated_usd {
            self.estimated_usd = Some(self.estimated_usd.unwrap_or(0.0) + usd);
        }
        if other.price_as_of.is_some() {
            self.price_as_of = other.price_as_of.clone();
        }
        self.unknown_count = self.unknown_count.saturating_add(other.unknown_count);
        self.calls = self.calls.saturating_add(other.calls);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Completeness {
    Complete,
    Partial,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpendReport {
    pub schema: u32,
    pub filter: Attribution,
    pub execution: Money,
    pub overhead: Money,
    pub by_source: BTreeMap<String, Money>,
    pub calls: u64,
    pub cached_calls: u64,
    pub duplicates_dropped: u64,
    pub completeness: Completeness,
    pub billing: String,
    pub tokens: TokenTotals,
    #[serde(default)]
    pub receipts: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenTotals {
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
}

pub const SPEND_REPORT_SCHEMA: u32 = 1;

/// Drops every receipt `filter` does not [`Attribution::matches`] -- the one
/// place cross-campaign attribution is enforced.
fn filter_receipts<'a>(receipts: &'a [Receipt], filter: &Attribution) -> Vec<&'a Receipt> {
    receipts
        .iter()
        .filter(|r| filter.matches(&r.attribution))
        .collect()
}

/// Drops an exact repeat of the same `receipt_id` (a receipt logged twice by
/// a retried write, a re-read log tail, or a caller that supplied the same
/// external receipt more than once). A `None` id is never deduped against
/// anything -- only a shared, non-empty id counts as the same event.
fn dedupe_by_receipt_id(receipts: Vec<&Receipt>) -> (Vec<&Receipt>, u64) {
    let mut seen = std::collections::HashSet::new();
    let mut kept = Vec::with_capacity(receipts.len());
    let mut dropped = 0u64;
    for receipt in receipts {
        match receipt.receipt_id.as_deref() {
            Some(id) if !seen.insert(id) => dropped += 1,
            _ => kept.push(receipt),
        }
    }
    (kept, dropped)
}

/// One receipt's usable numeric picture after cumulative-resume normalization
/// -- an owned copy so a cumulative row can be rewritten into its own
/// increment without mutating the input.
#[derive(Clone)]
struct Normalized {
    source: Source,
    session: Option<String>,
    reported_usd: Option<f64>,
    model: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    cached: bool,
    billing: Billing,
}

impl From<&Receipt> for Normalized {
    fn from(r: &Receipt) -> Self {
        Self {
            source: r.source,
            session: r.session.clone(),
            reported_usd: r.reported_usd,
            model: r.model.clone(),
            input_tokens: r.input_tokens,
            output_tokens: r.output_tokens,
            cache_creation_tokens: r.cache_creation_tokens,
            cache_read_tokens: r.cache_read_tokens,
            cached: r.cached,
            billing: r.billing,
        }
    }
}

/// Converts every CUMULATIVE receipt in `receipts` into its own per-session
/// increment, in the order they were given (a resumed session's harness
/// reports a running total across turns, never a per-turn delta): grouped by
/// `(session, source)`, the first row in a group is its own baseline
/// increment, and each later row's increment is `current - previous`
/// (saturating -- a total that appears to shrink, e.g. a mid-run reset,
/// contributes zero rather than a negative spend). A cumulative receipt with
/// no session is left as a single, un-incremented row: there is no group to
/// diff it against.
fn normalize_cumulative(receipts: &[&Receipt]) -> Vec<Normalized> {
    let mut order: Vec<(Option<String>, Source)> = Vec::new();
    let mut groups: BTreeMap<(Option<String>, Source), Vec<&Receipt>> = BTreeMap::new();
    let mut passthrough = Vec::new();
    for receipt in receipts {
        if receipt.cumulative && receipt.session.is_some() {
            let key = (receipt.session.clone(), receipt.source);
            if !groups.contains_key(&key) {
                order.push(key.clone());
            }
            groups.entry(key).or_default().push(receipt);
        } else {
            passthrough.push(Normalized::from(*receipt));
        }
    }
    let mut out = passthrough;
    for key in order {
        let Some(rows) = groups.get(&key) else {
            continue;
        };
        let mut prev_tokens = (0u64, 0u64, 0u64, 0u64);
        let mut prev_usd: Option<f64> = None;
        for row in rows {
            let cur_tokens = (
                row.input_tokens,
                row.output_tokens,
                row.cache_creation_tokens,
                row.cache_read_tokens,
            );
            let delta = Normalized {
                source: row.source,
                session: row.session.clone(),
                reported_usd: match (row.reported_usd, prev_usd) {
                    (Some(cur), Some(prev)) => Some((cur - prev).max(0.0)),
                    (Some(cur), None) => Some(cur),
                    (None, _) => None,
                },
                model: row.model.clone(),
                input_tokens: cur_tokens.0.saturating_sub(prev_tokens.0),
                output_tokens: cur_tokens.1.saturating_sub(prev_tokens.1),
                cache_creation_tokens: cur_tokens.2.saturating_sub(prev_tokens.2),
                cache_read_tokens: cur_tokens.3.saturating_sub(prev_tokens.3),
                cached: row.cached,
                billing: row.billing,
            };
            prev_tokens = cur_tokens;
            if row.reported_usd.is_some() {
                prev_usd = row.reported_usd;
            }
            out.push(delta);
        }
    }
    out
}

/// For every `(session, source)` group that has at least one harness-reported
/// row (`reported_usd.is_some()`), drops every token-estimate-only row
/// (`reported_usd.is_none()`) in that SAME group -- "a harness-reported agent
/// cost wins over a token estimate of the same session, never both". Rows
/// with no session are never grouped this way (each stands alone).
fn prefer_reported_over_estimate(rows: Vec<Normalized>) -> Vec<Normalized> {
    let mut has_reported: std::collections::HashSet<(String, Source)> =
        std::collections::HashSet::new();
    for row in &rows {
        if let (Some(session), true) = (&row.session, row.reported_usd.is_some()) {
            has_reported.insert((session.clone(), row.source));
        }
    }
    rows.into_iter()
        .filter(|row| match &row.session {
            Some(session) if row.reported_usd.is_none() => {
                !has_reported.contains(&(session.clone(), row.source))
            }
            _ => true,
        })
        .collect()
}

/// Folds `receipts` into one [`SpendReport`]: filters by `filter`, dedupes
/// exact repeats, converts cumulative resume rows into increments, prefers a
/// harness-reported cost over a token estimate for the same session, then
/// prices whatever is left. Pure -- no I/O, no clock, no env; see
/// [`load_receipts`] for the impure half.
pub fn reconcile(receipts: &[Receipt], prices: &PriceTable, filter: &Attribution) -> SpendReport {
    let filtered = filter_receipts(receipts, filter);
    let (deduped, duplicates_dropped) = dedupe_by_receipt_id(filtered);
    let normalized = prefer_reported_over_estimate(normalize_cumulative(&deduped));

    let mut by_source: BTreeMap<String, Money> = Source::ALL
        .iter()
        .map(|s| (s.label().to_string(), Money::default()))
        .collect();
    let mut tokens = TokenTotals::default();
    let mut cached_calls = 0u64;
    let mut billings: std::collections::BTreeSet<Billing> = std::collections::BTreeSet::new();

    for row in &normalized {
        tokens.input = tokens.input.saturating_add(row.input_tokens);
        tokens.output = tokens.output.saturating_add(row.output_tokens);
        tokens.cache_creation = tokens
            .cache_creation
            .saturating_add(row.cache_creation_tokens);
        tokens.cache_read = tokens.cache_read.saturating_add(row.cache_read_tokens);
        if row.cached {
            cached_calls += 1;
        }
        if !matches!(row.billing, Billing::Unknown) || row.reported_usd.is_some() {
            billings.insert(row.billing);
        }

        let money = by_source.entry(row.source.label().to_string()).or_default();
        money.calls += 1;
        if let Some(usd) = row.reported_usd {
            money.add_reported(usd);
            continue;
        }
        if row.cached {
            // A cached hit is a known zero, never an unpriced unknown.
            money.add_reported(0.0);
            continue;
        }
        let usage = super::event::TranscriptUsage {
            input_tokens: row.input_tokens,
            cache_creation_input_tokens: row.cache_creation_tokens,
            cache_read_input_tokens: row.cache_read_tokens,
            output_tokens: row.output_tokens,
        };
        match row
            .model
            .as_deref()
            .and_then(|model| price::price(model, &usage, prices))
        {
            Some(micros) => money.add_estimated(micros as f64 / 1_000_000.0, &prices.as_of),
            None => money.unknown_count += 1,
        }
    }

    let mut execution = Money::default();
    let mut overhead = Money::default();
    for source in Source::ALL {
        let money = by_source.get(source.label()).cloned().unwrap_or_default();
        if source.is_execution() {
            execution.merge(&money);
        } else {
            overhead.merge(&money);
        }
    }

    let total_unknown = execution.unknown_count + overhead.unknown_count;
    let total_priced_calls = execution.calls + overhead.calls - total_unknown;
    let completeness = if total_priced_calls == 0 && total_unknown > 0 {
        Completeness::Unknown
    } else if total_unknown == 0 {
        Completeness::Complete
    } else {
        Completeness::Partial
    };

    let billing = match billings.len() {
        0 => "unknown".to_string(),
        1 => match billings.into_iter().next() {
            Some(Billing::Metered) => "metered".to_string(),
            Some(Billing::Subscription) => "subscription".to_string(),
            _ => "unknown".to_string(),
        },
        _ => "mixed".to_string(),
    };

    SpendReport {
        schema: SPEND_REPORT_SCHEMA,
        filter: filter.clone(),
        execution,
        overhead,
        by_source,
        calls: normalized.len() as u64,
        cached_calls,
        duplicates_dropped,
        completeness,
        billing,
        tokens,
        receipts: BTreeMap::new(),
    }
}

// -- I/O: folding the existing logs into receipts ------------------------

/// Maps one `delegations.jsonl` row to a [`Source`], from fields the row
/// already carries -- never a new column:
///
/// - `agent == "typesafe"` is either the harness proxy's own intake decision
///   (`proxy::persist`, model pinned to the `"jev-latest"` catalogue alias)
///   or another `[jev]`-gated site's advisory call (`jev::record`, model is
///   whatever `[proxy.typesafe] model` names, e.g. `"jev-1.13.0"` by
///   default) -- the two are told apart by that model string. This is a
///   genuine limitation, not a design choice: `proxy::persist` does not carry
///   which decider (`typesafe` vs `helper`) produced a decision onto the spend
///   row, so a `"helper"`-decider intake decision is folded into `Intake`
///   here, same as a `"typesafe"`-decider one, rather than `Helper`. `Source::
///   Helper`/`Source::Agent`/`Source::Judge`/`Source::Proposer` are therefore
///   never produced by THIS function -- they only ever appear via an
///   externally supplied receipt (`--receipts`), e.g. a benchmark harness's
///   own harness-reported cost for the orchestrating session itself.
/// - anything else is a delegated worker (`zirv ctx agent`, a dash pane, or a
///   native run settlement) -- `Source::Worker`.
fn source_for_delegation(agent: &str, model: Option<&str>) -> Source {
    if agent == "typesafe" {
        if model == Some("jev-latest") {
            Source::Intake
        } else {
            Source::Jev
        }
    } else {
        Source::Worker
    }
}

fn delegation_to_receipt(row: &log::DelegationRow) -> Receipt {
    Receipt {
        source: source_for_delegation(&row.agent, row.model.as_deref()),
        session: Some(row.session.clone()),
        receipt_id: None,
        cumulative: false,
        reported_usd: None,
        model: row.model.clone(),
        input_tokens: row.input_tokens,
        output_tokens: row.output_tokens,
        cache_creation_tokens: row.cache_creation_input_tokens,
        cache_read_tokens: row.cache_read_input_tokens,
        cached: row.cached,
        billing: Billing::Unknown,
        attribution: row.attribution.clone(),
    }
}

/// Reads every existing spend-adjacent log under `state_dir` -- today, only
/// `delegations.jsonl` (via [`log::read_delegations`]) -- into [`Receipt`]s.
/// `jev-decisions.jsonl`/`jev-effects.jsonl`/`proxy-decisions.jsonl` are
/// advisory call/effect COUNTS, not spend rows (a `jev::record` call already
/// writes its own priced `Delegation` row for the same event) -- see
/// [`load_receipt_counts`] for those. Best-effort like every other state-dir
/// reader in this crate: a missing or corrupt file yields fewer receipts,
/// never an error.
pub fn load_receipts(state_dir: &Path) -> Vec<Receipt> {
    let state = StateDir::from_path(state_dir.to_path_buf());
    log::read_delegations(&state, usize::MAX)
        .iter()
        .map(delegation_to_receipt)
        .collect()
}

#[derive(Debug, Deserialize)]
struct DecisionCountRow {
    site: String,
    #[serde(default)]
    fallbacks: Vec<String>,
    #[serde(default)]
    attribution: Attribution,
}

#[derive(Debug, Deserialize)]
struct EffectCountRow {
    site: String,
    #[serde(default)]
    attribution: Attribution,
}

#[derive(Debug, Deserialize)]
struct ProxyDecisionCountRow {
    #[serde(default)]
    attribution: Attribution,
    /// Missing decider fields default to the plain proxy total for older rows (#803).
    #[serde(default)]
    decider: Option<String>,
}

fn read_jsonl_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Advisory call/effect counts for `SpendReport.receipts`: `jev:<site>`
/// (every `jev-decisions.jsonl` row for that site), `jev:<site>:fallback`
/// (the subset whose `fallbacks` was non-empty), `effect:<site>` (every
/// `jev-effects.jsonl` row), `proxy:decision` (every `proxy-decisions.jsonl`
/// row, written even when Jev never ran -- the deterministic decider writes
/// one too, so this alone cannot prove the production Jev path ran), and
/// `proxy:decider:<decider>` (`typesafe`/`helper`/`deterministic`, from
/// that same row's own `decider` field -- `proxy:decider:typesafe` or
/// `proxy:decider:helper` is the receipt that actually proves the
/// production Jev/helper intake path ran, issue #803). Filtered by `filter`
/// exactly like [`reconcile`] filters receipts, so two campaigns sharing
/// one state dir never see each other's counts either.
pub fn load_receipt_counts(state_dir: &Path, filter: &Attribution) -> BTreeMap<String, u64> {
    let mut counts = BTreeMap::new();
    for line in read_jsonl_lines(&state_dir.join("jev-decisions.jsonl")) {
        let Ok(row) = serde_json::from_str::<DecisionCountRow>(&line) else {
            continue;
        };
        if !filter.matches(&row.attribution) {
            continue;
        }
        *counts.entry(format!("jev:{}", row.site)).or_insert(0u64) += 1;
        if !row.fallbacks.is_empty() {
            *counts
                .entry(format!("jev:{}:fallback", row.site))
                .or_insert(0u64) += 1;
        }
    }
    for line in read_jsonl_lines(&state_dir.join("jev-effects.jsonl")) {
        let Ok(row) = serde_json::from_str::<EffectCountRow>(&line) else {
            continue;
        };
        if !filter.matches(&row.attribution) {
            continue;
        }
        *counts.entry(format!("effect:{}", row.site)).or_insert(0u64) += 1;
    }
    for line in read_jsonl_lines(&state_dir.join("proxy-decisions.jsonl")) {
        let Ok(row) = serde_json::from_str::<ProxyDecisionCountRow>(&line) else {
            continue;
        };
        if !filter.matches(&row.attribution) {
            continue;
        }
        *counts.entry("proxy:decision".to_string()).or_insert(0u64) += 1;
        if let Some(decider) = &row.decider {
            *counts
                .entry(format!("proxy:decider:{decider}"))
                .or_insert(0u64) += 1;
        }
    }
    counts
}

// -- CLI: `zirv workflow spend` ------------------------------------------

#[derive(Debug, Args)]
pub struct SpendArgs {
    #[arg(long)]
    pub state_dir: std::path::PathBuf,
    /// A JSONL file of extra [`Receipt`] rows (e.g. a harness-reported agent
    /// cost) to fold in alongside the ones read from `--state-dir`.
    #[arg(long)]
    pub receipts: Option<std::path::PathBuf>,
    #[arg(long)]
    pub campaign: Option<String>,
    #[arg(long)]
    pub candidate: Option<String>,
    #[arg(long)]
    pub trial: Option<String>,
    #[arg(long)]
    pub task: Option<String>,
    #[arg(long)]
    pub json: bool,
}

fn read_extra_receipts(path: &Path) -> Vec<Receipt> {
    read_jsonl_lines(path)
        .iter()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn render_text(report: &SpendReport, writer: &mut impl Write) -> CtxResult<()> {
    let money = |m: &Money| {
        format!(
            "reported=${:.4} estimated=${:.4} unknown={} calls={}",
            m.reported_usd.unwrap_or(0.0),
            m.estimated_usd.unwrap_or(0.0),
            m.unknown_count,
            m.calls
        )
    };
    writeln!(
        writer,
        "zirv workflow spend: {} call(s), {} cached, {} duplicate(s) dropped, completeness={:?}, billing={}",
        report.calls,
        report.cached_calls,
        report.duplicates_dropped,
        report.completeness,
        report.billing
    )?;
    writeln!(writer, "execution: {}", money(&report.execution))?;
    writeln!(writer, "overhead:  {}", money(&report.overhead))?;
    for (source, m) in &report.by_source {
        if m.calls > 0 {
            writeln!(writer, "  {source}: {}", money(m))?;
        }
    }
    Ok(())
}

pub fn run_spend(args: &SpendArgs, writer: &mut impl Write) -> CtxResult<i32> {
    let cfg = CtxConfig::load_for_launch(&std::env::current_dir()?, &|key| std::env::var(key).ok())
        .unwrap_or_default();
    let prices = price::resolve_table(&cfg);
    let mut receipts = load_receipts(&args.state_dir);
    if let Some(path) = &args.receipts {
        receipts.extend(read_extra_receipts(path));
    }
    let filter = Attribution {
        campaign: args.campaign.clone().filter(|v| is_valid_id(v)),
        candidate: args.candidate.clone().filter(|v| is_valid_id(v)),
        trial: args.trial.clone().filter(|v| is_valid_id(v)),
        task: args.task.clone().filter(|v| is_valid_id(v)),
    };
    let mut report = reconcile(&receipts, &prices, &filter);
    report.receipts = load_receipt_counts(&args.state_dir, &filter);
    if args.json {
        writeln!(writer, "{}", serde_json::to_string_pretty(&report)?)?;
    } else {
        render_text(&report, writer)?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> PriceTable {
        let mut models = BTreeMap::new();
        models.insert(
            "jev-1.13.0".to_string(),
            super::super::price::ModelPrice {
                input_micros: 1_000_000,
                cache_write_micros: 500_000,
                cache_read_micros: 100_000,
                output_micros: 2_000_000,
            },
        );
        models.insert(
            "claude-x".to_string(),
            super::super::price::ModelPrice {
                input_micros: 3_000_000,
                cache_write_micros: 1_000_000,
                cache_read_micros: 300_000,
                output_micros: 15_000_000,
            },
        );
        PriceTable {
            as_of: "2026-09-26".to_string(),
            models,
        }
    }

    fn receipt(source: Source, session: &str, input: u64, output: u64) -> Receipt {
        Receipt {
            source,
            session: Some(session.to_string()),
            receipt_id: None,
            cumulative: false,
            reported_usd: None,
            model: Some(match source {
                Source::Intake | Source::Jev => "jev-1.13.0".to_string(),
                _ => "claude-x".to_string(),
            }),
            input_tokens: input,
            output_tokens: output,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            cached: false,
            billing: Billing::Metered,
            attribution: Attribution::default(),
        }
    }

    /// Regression for issue-review finding R6: `run.py`'s own `make_receipt`
    /// (docs/benchmarks/wrapped-vs-vanilla/run.py) emits Anthropic's own
    /// field spelling, `cache_creation_input_tokens`/
    /// `cache_read_input_tokens`, not `Receipt`'s `cache_creation_tokens`/
    /// `cache_read_tokens` -- so a receipt line it writes must still
    /// populate those fields via `#[serde(alias = ...)]`, not silently
    /// parse them as 0. This is `make_receipt`'s exact key set and value
    /// shape, byte-for-byte (a JSON object with precisely its keys, in its
    /// order, none added or dropped) -- not a synthetic shorthand.
    #[test]
    fn a_receipt_line_shaped_exactly_like_run_pys_make_receipt_parses_its_cache_tokens() {
        let line = serde_json::json!({
            "source": "judge",
            "session": "sess-1",
            "receipt_id": "r-1",
            "cumulative": false,
            "reported_usd": 0.0123,
            "model": "claude-x",
            "input_tokens": 100,
            "output_tokens": 20,
            "cache_creation_input_tokens": 7,
            "cache_read_input_tokens": 13,
            "cached": false,
            "billing": "metered",
        })
        .to_string();
        let receipt: Receipt = serde_json::from_str(&line).unwrap();
        assert_eq!(receipt.source, Source::Judge);
        assert_eq!(receipt.session.as_deref(), Some("sess-1"));
        assert_eq!(receipt.receipt_id.as_deref(), Some("r-1"));
        assert!(!receipt.cumulative);
        assert_eq!(receipt.reported_usd, Some(0.0123));
        assert_eq!(receipt.model.as_deref(), Some("claude-x"));
        assert_eq!(receipt.input_tokens, 100);
        assert_eq!(receipt.output_tokens, 20);
        assert_eq!(
            receipt.cache_creation_tokens, 7,
            "cache_creation_input_tokens must alias into cache_creation_tokens"
        );
        assert_eq!(
            receipt.cache_read_tokens, 13,
            "cache_read_input_tokens must alias into cache_read_tokens"
        );
        assert!(!receipt.cached);
        assert_eq!(receipt.billing, Billing::Metered);
    }

    // -- Attribution -----------------------------------------------------

    #[test]
    fn invalid_ids_are_dropped() {
        let attr = Attribution::from_lookup(|key| match key {
            CAMPAIGN_ENV => Some("ok-campaign.1".to_string()),
            CANDIDATE_ENV => Some("has a space".to_string()),
            TRIAL_ENV => Some(String::new()),
            TASK_ENV => Some("x".repeat(65)),
            _ => None,
        });
        assert_eq!(attr.campaign.as_deref(), Some("ok-campaign.1"));
        assert_eq!(attr.candidate, None);
        assert_eq!(attr.trial, None);
        assert_eq!(attr.task, None);
    }

    #[test]
    fn empty_attribution_serializes_to_nothing() {
        let attr = Attribution::default();
        assert!(attr.is_empty());
        assert_eq!(serde_json::to_string(&attr).unwrap(), "{}");

        #[derive(Serialize)]
        struct Row {
            session: &'static str,
            #[serde(default, skip_serializing_if = "Attribution::is_empty")]
            attribution: Attribution,
        }
        let row = Row {
            session: "s1",
            attribution: Attribution::default(),
        };
        assert_eq!(
            serde_json::to_value(&row).unwrap(),
            serde_json::json!({"session": "s1"})
        );
    }

    #[test]
    fn attribution_filter_matches_only_set_fields() {
        let filter = Attribution {
            campaign: Some("c1".to_string()),
            ..Attribution::default()
        };
        let matching = Attribution {
            campaign: Some("c1".to_string()),
            candidate: Some("anything".to_string()),
            ..Attribution::default()
        };
        let other_campaign = Attribution {
            campaign: Some("c2".to_string()),
            ..Attribution::default()
        };
        let no_campaign = Attribution::default();
        assert!(filter.matches(&matching));
        assert!(!filter.matches(&other_campaign));
        assert!(!filter.matches(&no_campaign));
        assert!(Attribution::default().matches(&matching));
    }

    // -- reconciler --------------------------------------------------------

    #[test]
    fn reconciles_intake_jev_helper_worker_retry_duplicate_and_resume_exactly() {
        let mut receipts = vec![
            receipt(Source::Intake, "s-intake", 100, 50),
            receipt(Source::Jev, "s-jev-1", 10, 5),
            receipt(Source::Jev, "s-jev-2", 10, 5),
            Receipt {
                cached: true,
                input_tokens: 0,
                output_tokens: 0,
                ..receipt(Source::Jev, "s-jev-3", 0, 0)
            },
            receipt(Source::Helper, "s-helper", 20, 10),
            receipt(Source::Worker, "s-worker", 200, 100),
        ];
        // A retry: a second worker call for the same session, genuinely a
        // second event (its own tokens), must be counted, not dropped.
        receipts.push(receipt(Source::Worker, "s-worker", 50, 25));
        // A duplicate receipt: identical id repeated must be dropped once.
        let dup = Receipt {
            receipt_id: Some("dup-1".to_string()),
            ..receipt(Source::Worker, "s-dup", 30, 15)
        };
        receipts.push(dup.clone());
        receipts.push(dup);
        // A resumed session with cumulative receipts: two turns' running
        // totals, must reconcile to the increments (30,15) then (20,10).
        receipts.push(Receipt {
            cumulative: true,
            ..receipt(Source::Worker, "s-resume", 30, 15)
        });
        receipts.push(Receipt {
            cumulative: true,
            ..receipt(Source::Worker, "s-resume", 50, 25)
        });

        let report = reconcile(&receipts, &table(), &Attribution::default());

        assert_eq!(report.duplicates_dropped, 1);
        assert_eq!(report.cached_calls, 1);
        // worker calls: s-worker x2 + s-dup x1 + s-resume x2 = 5
        assert_eq!(report.by_source["worker"].calls, 5);
        assert_eq!(report.by_source["intake"].calls, 1);
        assert_eq!(report.by_source["jev"].calls, 3);
        assert_eq!(report.by_source["helper"].calls, 1);
        // Resume increments: (30,15) then (20,10) -> 50 in, 25 out combined
        // with the other worker rows: 200+50+30+30+20=330 input.
        assert_eq!(
            report.tokens.input,
            100 + 10 + 10 + 20 + 200 + 50 + 30 + 30 + 20
        );
        assert_eq!(
            report.tokens.output,
            50 + 5 + 5 + 10 + 100 + 25 + 15 + 15 + 10
        );
        assert_eq!(report.completeness, Completeness::Complete);
        assert!(report.execution.calls > 0);
        assert_eq!(report.overhead.calls, 0);
    }

    #[test]
    fn harness_reported_cost_beats_token_estimate_for_the_same_session() {
        let estimate = receipt(Source::Worker, "s1", 1_000_000, 1_000_000);
        let reported = Receipt {
            reported_usd: Some(1.23),
            input_tokens: 0,
            output_tokens: 0,
            model: None,
            ..receipt(Source::Worker, "s1", 0, 0)
        };
        let report = reconcile(&[estimate, reported], &table(), &Attribution::default());
        assert_eq!(report.by_source["worker"].calls, 1);
        assert_eq!(report.by_source["worker"].reported_usd, Some(1.23));
        assert_eq!(report.by_source["worker"].estimated_usd, None);
    }

    #[test]
    fn unpriced_model_increments_unknown_never_reports_zero() {
        let receipt = Receipt {
            model: Some("no-such-model".to_string()),
            ..receipt(Source::Worker, "s1", 10, 10)
        };
        let report = reconcile(&[receipt], &table(), &Attribution::default());
        assert_eq!(report.by_source["worker"].unknown_count, 1);
        assert_eq!(report.by_source["worker"].estimated_usd, None);
        assert_eq!(report.completeness, Completeness::Unknown);
    }

    #[test]
    fn subscription_billing_is_carried_and_tokens_still_counted() {
        let receipt = Receipt {
            billing: Billing::Subscription,
            ..receipt(Source::Worker, "s1", 10, 10)
        };
        let report = reconcile(&[receipt], &table(), &Attribution::default());
        assert_eq!(report.billing, "subscription");
        assert_eq!(report.tokens.input, 10);
        assert_eq!(report.tokens.output, 10);
    }

    #[test]
    fn two_interleaved_campaigns_never_cross_attribute() {
        let mut a = receipt(Source::Worker, "s1", 10, 10);
        a.attribution = Attribution {
            campaign: Some("camp-a".to_string()),
            ..Attribution::default()
        };
        let mut b = receipt(Source::Worker, "s2", 20, 20);
        b.attribution = Attribution {
            campaign: Some("camp-b".to_string()),
            ..Attribution::default()
        };
        let receipts = vec![a, b];
        let filter_a = Attribution {
            campaign: Some("camp-a".to_string()),
            ..Attribution::default()
        };
        let report_a = reconcile(&receipts, &table(), &filter_a);
        assert_eq!(report_a.by_source["worker"].calls, 1);
        assert_eq!(report_a.tokens.input, 10);
    }

    // -- load_receipts / load_receipt_counts --------------------------------

    #[test]
    fn load_receipts_over_a_temp_state_dir_maps_delegation_rows() {
        let root = tempfile::tempdir().unwrap();
        let state = StateDir::from_root(root.path().to_path_buf());
        super::super::state::create_private_dir_all(&state.logs()).unwrap();
        let intake = log::Delegation {
            ts: 1,
            session: "s1",
            parent_session: "",
            work_group_id: None,
            agent: "typesafe",
            model: Some("jev-latest"),
            input_tokens: 5,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            output_tokens: 5,
            wall_ms: 1,
            exit_code: 0,
            outcome: "ok",
            mode: None,
            task_class: None,
            principal: "root",
            envelope_sha256: None,
        };
        log::append_delegation(&state, &intake).unwrap();
        let jev = log::Delegation {
            model: Some("jev-1.13.0"),
            ..intake
        };
        log::append_delegation(&state, &jev).unwrap();

        let receipts = load_receipts(state.root());
        assert_eq!(receipts.len(), 2);
        assert_eq!(receipts[0].source, Source::Intake);
        assert_eq!(receipts[1].source, Source::Jev);
    }

    #[test]
    fn load_receipt_counts_reads_jev_site_and_effect_names() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("jev-decisions.jsonl"),
            "{\"site\":\"memory\",\"fallbacks\":[]}\n{\"site\":\"memory\",\"fallbacks\":[\"x\"]}\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("jev-effects.jsonl"),
            "{\"site\":\"handoff_select\"}\n",
        )
        .unwrap();
        let counts = load_receipt_counts(root.path(), &Attribution::default());
        assert_eq!(counts["jev:memory"], 2);
        assert_eq!(counts["jev:memory:fallback"], 1);
        assert_eq!(counts["effect:handoff_select"], 1);
    }

    /// Issue #803: `proxy:decision` alone is written even when Jev never
    /// ran (the deterministic decider writes one too), so it cannot prove
    /// the production Jev intake path actually ran. `proxy:decider:<decider>`
    /// (from the row's own `decider` field) can -- a row missing that field
    /// entirely (persisted before it existed) still counts toward the plain
    /// bucket, just not a per-decider one.
    #[test]
    fn load_receipt_counts_splits_proxy_decisions_by_decider() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("proxy-decisions.jsonl"),
            "{\"decider\":\"typesafe\"}\n{\"decider\":\"deterministic\"}\n{\"decider\":\"deterministic\"}\n{}\n",
        )
        .unwrap();
        let counts = load_receipt_counts(root.path(), &Attribution::default());
        assert_eq!(
            counts["proxy:decision"], 4,
            "every row counts toward the plain bucket"
        );
        assert_eq!(counts["proxy:decider:typesafe"], 1);
        assert_eq!(counts["proxy:decider:deterministic"], 2);
        assert!(
            !counts.contains_key("proxy:decider:"),
            "a row with no decider field must not fabricate an empty-string bucket"
        );
    }
}
